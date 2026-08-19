//! Application entry point.
//!
//! Wires the pipeline together and runs the background tasks. The mode selects the data
//! source and the execution adapter; **everything between them is identical** across
//! paper, replay and live.

mod demo;
mod pipeline;
mod replay;
mod tasks;

use std::sync::Arc;

use anyhow::{Context, Result};
use api::state::{AppState, RecentActivity};
use clap::Parser;
use domain::AppMode;
use execution::{BookCache, ExecutionAdapter, LiveExecution, OrderManager, PaperExecution};
use metrics::{HealthMonitor, Metrics};
use parking_lot::RwLock;
use portfolio::Portfolio;
use risk::{KillSwitch, RiskEngine, RiskLimits};
use strategy::{CopyTrader, StrategyConfig};
use tracing::{error, info, warn};
use wallet_tracker::WalletTracker;

use crate::pipeline::Pipeline;

#[derive(Parser, Debug)]
#[command(name = "copytrader", about = "Polymarket copy-trading system", version)]
struct Cli {
    /// Operating mode. Overrides APP_MODE.
    #[arg(long, value_parser = ["paper", "live", "replay"])]
    mode: Option<String>,

    /// Recorded session to replay (required for --mode replay).
    #[arg(long)]
    file: Option<String>,

    /// Emit synthetic DEMO activity. Refused in live mode.
    #[arg(long)]
    demo: bool,

    /// HTTP port. Overrides SERVER_PORT.
    #[arg(long)]
    port: Option<u16>,

    /// Validate configuration and exit without trading.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // CLI overrides land in the environment before config is loaded, so there is
    // exactly one place that interprets configuration.
    if let Some(m) = &cli.mode { std::env::set_var("APP_MODE", m); }
    if let Some(f) = &cli.file { std::env::set_var("REPLAY_FILE", f); }
    if let Some(p) = cli.port { std::env::set_var("SERVER_PORT", p.to_string()); }
    if cli.demo { std::env::set_var("DEMO_DATA", "true"); }

    let cfg = config::AppConfig::from_env().context("configuration is invalid")?;
    init_logging(&cfg);

    banner(&cfg);

    if cli.check {
        info!("configuration is valid");
        return Ok(());
    }

    run(cfg).await
}

fn init_logging(cfg: &config::AppConfig) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,hyper=warn,tower_http=warn"));
    let reg = tracing_subscriber::registry().with(filter);
    if cfg.log_format_json {
        reg.with(fmt::layer().json().with_current_span(true)).init();
    } else {
        reg.with(fmt::layer().with_target(false)).init();
    }
}

fn banner(cfg: &config::AppConfig) {
    let mode = cfg.mode.as_str();
    info!("╔══════════════════════════════════════════════════════════╗");
    info!("║  Polymarket Copy-Trading System                          ║");
    info!("╠══════════════════════════════════════════════════════════╣");
    info!("║  MODE: {:<50}║", mode);
    if cfg.live_execution_armed() {
        // Impossible to miss in a log tail.
        warn!("║  *** LIVE EXECUTION ARMED — REAL MONEY AT RISK ***       ║");
    } else if cfg.mode.is_live() {
        error!("║  LIVE mode requested but NOT armed — refusing to trade   ║");
    } else {
        info!("║  Simulated execution — no real funds at risk             ║");
    }
    if cfg.demo_data {
        info!("║  DEMO DATA enabled — synthetic activity, clearly labelled ║");
    }
    info!("║  Target wallets configured: {:<29}║", cfg.wallets.len());
    info!("╚══════════════════════════════════════════════════════════╝");
}

async fn run(cfg: config::AppConfig) -> Result<()> {
    let started_at = chrono::Utc::now();
    let (events_tx, _) = tokio::sync::broadcast::channel(4096);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // ---- storage ----
    let store = if cfg.storage.ephemeral {
        info!("EPHEMERAL_STORAGE set — running without persistence");
        persistence::Store::ephemeral()
    } else {
        persistence::Store::connect_or_ephemeral(
            cfg.storage.database_url.expose(), cfg.storage.max_connections).await
    };
    let repos = Arc::new(persistence::Repositories::new(store));

    // ---- core components ----
    let metrics = Arc::new(Metrics::new());
    let health = Arc::new(HealthMonitor::new(cfg.mode.as_str()));
    let kill_switch = Arc::new(KillSwitch::new());
    let books = Arc::new(BookCache::new());
    let portfolio = Arc::new(Portfolio::new(cfg.simulation.starting_cash_usd));

    // Wallets from config, then anything persisted, then demo wallets.
    let mut wallets = cfg.wallets.clone();
    let mut demo_gen = cfg.demo_data.then(|| demo::DemoGenerator::new(cfg.simulation.rng_seed));
    if let Some(g) = &demo_gen {
        wallets.extend(g.wallets.clone());
    }
    let tracker = Arc::new(WalletTracker::new(wallets));
    info!(
        tracked = tracker.wallet_count(),
        from_config = cfg.wallets.len(),
        "target wallets registered"
    );

    // ---- execution adapter: the only thing that differs between modes ----
    let (adapter, paper): (Arc<dyn ExecutionAdapter>, Option<Arc<PaperExecution>>) =
        if cfg.mode.is_live() {
            let live = LiveExecution::new(
                cfg.endpoints.clob_url.clone(),
                cfg.endpoints.http_timeout_ms,
                cfg.live_execution_armed(),
            )?
            .with_credentials(execution::L2Credentials {
                api_key: cfg.live.api_key.expose().to_string(),
                secret: cfg.live.api_secret.expose().to_string(),
                passphrase: cfg.live.api_passphrase.expose().to_string(),
            });
            let live = match &cfg.live.funder_address {
                Some(a) => live.with_funder(a.to_string()),
                None => live,
            };
            // Attach the verified EIP-712 signer. The scheme was recovered from and
            // proved against Polygon mainnet (see execution::signing); what remains
            // unproven is only whether POST /order accepts the enveloping JSON.
            let live = match cfg.live.private_key.is_empty() {
                true => live,
                false => {
                    // A configured funder address means the maker is a Polymarket proxy
                    // wallet and the key signs on its behalf; otherwise we trade as an EOA.
                    let (maker, sig_type) = match &cfg.live.funder_address {
                        Some(a) => (Some(a.to_string()), execution::SignatureType::PolyProxy),
                        None => (None, execution::SignatureType::Eoa),
                    };
                    match execution::EoaSigner::new(
                        cfg.live.private_key.expose(), maker.as_deref(), sig_type)
                    {
                        Ok(s) => {
                            info!(
                                signer = %s.address(),
                                maker = %s.maker(),
                                signature_type = ?sig_type,
                                "EIP-712 order signer ready"
                            );
                            live.with_signer(Arc::new(s))
                        }
                        Err(e) => {
                            error!(error = %e, "private key is unusable; live execution stays disarmed");
                            live
                        }
                    }
                }
            };

            let gaps = live.readiness_gaps();
            if !gaps.is_empty() {
                // Loud, specific, and non-fatal: the API and dashboard still come up so
                // the operator can see the state, but no order can be sent.
                error!("LIVE execution is NOT ready. Missing: {}", gaps.join(", "));
                error!("No orders will be submitted. See docs/LIVE_MODE.md.");
                kill_switch.engage(
                    format!("live adapter not ready: {}", gaps.join(", ")), "startup");
            }
            (Arc::new(live), None)
        } else {
            let p = Arc::new(PaperExecution::from_config(books.clone(), &cfg.simulation));
            (p.clone(), Some(p))
        };
    info!(adapter = adapter.name(), real_money = adapter.capabilities().is_real_money,
        "execution adapter selected");

    let orders = Arc::new(OrderManager::new(adapter.clone(), events_tx.clone()));

    let limits = RiskLimits::from_config(
        &cfg.risk,
        cfg.live.max_live_order_usd,
        // Live trading without a fresh book is refused; paper/demo may proceed.
        cfg.mode.is_live(),
    );
    let risk_engine = Arc::new(RwLock::new(RiskEngine::new(
        limits, cfg.mode, cfg.live_execution_armed())));

    let state = Arc::new(AppState {
        last_source_data_ms: std::sync::atomic::AtomicI64::new(0),
        mode: cfg.mode,
        config: cfg.clone(),
        portfolio: portfolio.clone(),
        orders: orders.clone(),
        tracker: tracker.clone(),
        kill_switch: kill_switch.clone(),
        risk: risk_engine.clone(),
        metrics: metrics.clone(),
        health: health.clone(),
        repos: repos.clone(),
        recent: Arc::new(RecentActivity::new(500)),
        events: events_tx.clone(),
        paper: paper.clone(),
        started_at,
    });

    // ---- crash recovery ----
    tasks::recover(&state).await;

    // Persist the effective wallet set, so the database reflects what is actually being
    // copied rather than only wallets that happened to be added through the API.
    for w in tracker.list_wallets() {
        let _ = repos.upsert_wallet(&w).await;
    }

    let trader = Arc::new(CopyTrader::new(StrategyConfig {
        max_slippage_bps: cfg.risk.max_slippage_bps,
        max_book_age_ms: cfg.risk.max_market_data_age_ms,
        allow_pricing_without_book: !cfg.mode.is_live(),
    }));
    // The REST client backs on-demand book fetching: target wallets trade across the
    // whole platform, so the token we need is usually not in the warm working set.
    let rest_for_books = market_data::PolymarketRest::new(
        cfg.endpoints.clob_url.clone(),
        cfg.endpoints.gamma_url.clone(),
        cfg.endpoints.data_api_url.clone(),
        cfg.endpoints.http_timeout_ms,
    )
    .ok()
    .map(Arc::new);

    // Streaming books: the trading path adds tokens as it discovers them.
    let subs = Arc::new(market_data::TokenSubscriptions::new());
    let stream_stats = Arc::new(parking_lot::Mutex::new(market_data::StreamStats::default()));

    let mut pipe = Pipeline::new(state.clone(), trader, books.clone());
    if let Some(r) = rest_for_books {
        pipe = pipe.with_rest(r);
    }
    pipe = pipe.with_subscriptions(subs.clone());
    let pipe = Arc::new(pipe);

    // ---- background tasks ----
    let mut handles = Vec::new();
    let _ = &stream_stats;

    match cfg.mode {
        AppMode::Replay => {
            let file = cfg.replay_file.clone().context("REPLAY_FILE is required in replay mode")?;
            handles.push(tokio::spawn(tasks::run_replay(pipe.clone(), books.clone(), file, shutdown_rx.clone())));
        }
        AppMode::Paper | AppMode::Live => {
            // Real market data + the real RTDS feed.
            handles.push(tokio::spawn(tasks::run_source_feed(
                pipe.clone(), state.clone(), cfg.clone(), shutdown_rx.clone())));
            handles.push(tokio::spawn(tasks::run_market_data(
                state.clone(), books.clone(), paper.clone(), cfg.clone(),
                Some(subs.clone()), shutdown_rx.clone())));
        }
    }

    if let Some(g) = demo_gen.take() {
        handles.push(tokio::spawn(tasks::run_demo(
            pipe.clone(), books.clone(), paper.clone(), g, shutdown_rx.clone())));
    }

    if !matches!(cfg.mode, AppMode::Replay) {
        // Live book stream. Books it publishes also fill resting paper orders.
        let books_for_stream = books.clone();
        let paper_for_stream = paper.clone();
        let state_for_stream = state.clone();
        handles.push(tokio::spawn(market_data::run_market_stream(
            cfg.endpoints.market_ws_url.clone(),
            cfg.endpoints.ws_connect_timeout_ms,
            subs.clone(),
            stream_stats.clone(),
            move |b| {
                if let Some(mid) = b.mid() {
                    state_for_stream.portfolio.mark(&b.token_id, mid, chrono::Utc::now());
                }
                if let Some(p) = &paper_for_stream {
                    // Resting paper orders fill when the streamed market reaches them.
                    for f in p.on_book_update(b) {
                        if state_for_stream.orders.apply_external_fill(&f).is_ok() {
                            state_for_stream.portfolio.apply_fill(&f, "");
                            state_for_stream.metrics.orders_filled_total.inc();
                        }
                    }
                }
                books_for_stream.put(b.clone());
                state_for_stream.metrics.market_events_total.inc();
            },
            shutdown_rx.clone(),
        )));
    }

    // Warm the stream with markets our targets actually trade, before the feed starts.
    if !matches!(cfg.mode, AppMode::Replay) {
        if let Ok(r) = market_data::PolymarketRest::new(
            cfg.endpoints.clob_url.clone(), cfg.endpoints.gamma_url.clone(),
            cfg.endpoints.data_api_url.clone(), cfg.endpoints.http_timeout_ms)
        {
            tokio::spawn(tasks::warm_target_markets(state.clone(), subs.clone(), Arc::new(r)));
        }
    }

    handles.push(tokio::spawn(tasks::run_housekeeping(state.clone(), shutdown_rx.clone())));
    handles.push(tokio::spawn(tasks::run_event_log(state.clone(), shutdown_rx.clone())));
    handles.push(tokio::spawn(tasks::run_reconciliation(
        state.clone(), adapter.clone(), shutdown_rx.clone())));

    // ---- http server ----
    let addr = format!("{}:{}", cfg.server.bind_addr, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await
        .with_context(|| format!("cannot bind {addr}"))?;
    info!("API listening on http://{addr}  (dashboard websocket at /ws)");
    if cfg.server.api_token.is_empty() && !cfg.mode.is_live() {
        warn!("API_AUTH_TOKEN is not set: mutating endpoints are unauthenticated (permitted outside LIVE)");
    }

    let app = api::router(state.clone());
    let mut sd = shutdown_rx.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = sd.changed().await;
    });

    tokio::select! {
        r = server => { if let Err(e) = r { error!(error = %e, "http server failed"); } }
        _ = shutdown_signal() => {
            info!("shutdown signal received");
        }
    }

    // ---- graceful shutdown ----
    let _ = shutdown_tx.send(true);
    info!("stopping background tasks…");
    for h in handles {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h).await;
    }
    // Persist a final snapshot so a restart resumes from an accurate position.
    let snap = portfolio.snapshot(orders.open_count());
    let _ = repos.insert_pnl_snapshot(&snap).await;
    for p in portfolio.positions() {
        let _ = repos.upsert_position(&p).await;
    }
    info!(equity = %snap.equity, realized = %snap.realized_pnl, "shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async { let _ = tokio::signal::ctrl_c().await; };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
