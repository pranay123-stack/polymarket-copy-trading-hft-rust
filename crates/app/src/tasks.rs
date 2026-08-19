//! Background tasks.
//!
//! Each runs until the shutdown watch fires. Nothing here holds a lock across an await
//! on the trading path — the pipeline must never be blocked by housekeeping.

use std::sync::Arc;
use std::time::Duration;

use api::state::AppState;
use chrono::Utc;
use domain::{AppMode, HealthState, SystemEvent, TradeSource};
use execution::{BookCache, ExecutionAdapter, PaperExecution, Reconciler};
use market_data::{FeedMessage, PolymarketRest, RtdsClient};
use rust_decimal_macros::dec;
use tokio::sync::watch::Receiver;
use tracing::{debug, error, info, warn};
use wallet_tracker::{BatchOrdinals, Detection};

use crate::demo::DemoGenerator;
use crate::pipeline::Pipeline;
use crate::replay::ReplaySession;

type Shutdown = Receiver<bool>;

/// Restores state after a restart so nothing is duplicated and nothing is lost.
pub async fn recover(state: &Arc<AppState>) {
    if state.repos.is_ephemeral() {
        info!("ephemeral storage: starting with a clean book");
        return;
    }
    let since = Utc::now() - chrono::Duration::hours(wallet_tracker::DEFAULT_RETENTION_HOURS);
    match state.repos.recover(since).await {
        Err(e) => error!(error = %e, "recovery failed; continuing with a clean book"),
        Ok(r) => {
            // Dedup first: it is what stops a replayed feed re-copying old trades.
            let entries: Vec<_> = r.dedup_entries.iter()
                .filter_map(|d| persistence::repositories::dedup_row_to_key(d)
                    .map(|k| (k, d.occurrences, d.last_seen)))
                .collect();
            let n = entries.len();
            state.tracker.restore_dedup(entries);

            let cash = r.cash.unwrap_or(state.config.simulation.starting_cash_usd);
            state.portfolio.restore(cash, r.positions.clone(), r.realized_pnl, r.fees_paid);
            for w in r.wallets { state.tracker.upsert_wallet(w); }

            info!(
                dedup_contents = n,
                positions = r.positions.len(),
                cash = %cash,
                "state recovered"
            );

            // Orders that might have executed must be reconciled before we trust the book.
            match state.repos.load_open_orders().await {
                Ok(open) => {
                    let unresolved = open.iter().filter(|o| o.needs_reconciliation()).count();
                    if unresolved > 0 {
                        warn!(unresolved, "recovered orders may have executed — reconciliation required");
                    }
                }
                Err(e) => error!(error = %e, "could not load open orders"),
            }
        }
    }
}

/// Warms the book stream with the markets our targets actually trade.
///
/// Seeding from `sampling-markets` alone warms the most *liquid* markets, which is not the
/// same thing: a target's next trade is far more likely to be in a market they traded
/// yesterday. Each wallet's recent history is therefore pulled once at startup and those
/// tokens subscribed, so the first copy of the session is priced from the stream instead
/// of paying a REST round trip.
pub async fn warm_target_markets(
    state: Arc<AppState>,
    subs: Arc<market_data::TokenSubscriptions>,
    rest: Arc<PolymarketRest>,
) {
    let since = Utc::now() - chrono::Duration::days(2);
    let mut added = 0usize;
    for w in state.tracker.list_wallets() {
        if !w.enabled { continue; }
        match rest.backfill_wallet_trades(&w.address, since, 200).await {
            Err(e) => debug!(wallet = %w.address, error = %e, "could not warm markets for wallet"),
            Ok(rows) => {
                for t in rows {
                    if subs.add(t.token_id.clone()) { added += 1; }
                }
            }
        }
    }
    if added > 0 {
        info!(added, followed = subs.len(), "warmed book stream from target trade history");
    }
}

/// Consumes the RTDS firehose and drives the pipeline.
pub async fn run_source_feed(
    pipe: Arc<Pipeline>,
    state: Arc<AppState>,
    cfg: config::AppConfig,
    mut shutdown: Shutdown,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8192);
    let client = RtdsClient::new(cfg.endpoints.rtds_ws_url.clone(), cfg.endpoints.ws_connect_timeout_ms);
    tokio::spawn(client.run(tx, shutdown.clone()));

    let rest = PolymarketRest::new(
        cfg.endpoints.clob_url.clone(),
        cfg.endpoints.gamma_url.clone(),
        cfg.endpoints.data_api_url.clone(),
        cfg.endpoints.http_timeout_ms,
    )
    .ok();

    loop {
        tokio::select! {
            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
            msg = rx.recv() => match msg {
                None => return,
                Some(FeedMessage::Connected { at }) => {
                    state.health.set("source_feed", HealthState::Healthy, "connected");
                    state.metrics.feed_connected.set(1);
                    let _ = state.events.send(SystemEvent::FeedReconnected {
                        source: "rtds".into(), backfilled: 0, at });
                }
                Some(FeedMessage::Disconnected { at, last_trade_at, reason }) => {
                    warn!(%reason, "source feed down");
                    state.health.set("source_feed", HealthState::Down, reason.clone());
                    state.metrics.feed_connected.set(0);
                    state.metrics.feed_reconnects_total.inc();
                    let gap_from = last_trade_at.unwrap_or(at);
                    let _ = state.events.send(SystemEvent::FeedDisconnected {
                        source: "rtds".into(), gap_from, at });

                    // Backfill the gap for tracked wallets only.
                    if let Some(r) = &rest {
                        let n = backfill_gap(&pipe, &state, r, gap_from).await;
                        if n > 0 {
                            info!(backfilled = n, "gap backfilled from data-api");
                            let _ = state.events.send(SystemEvent::FeedReconnected {
                                source: "rtds-backfill".into(), backfilled: n, at: Utc::now() });
                        }
                    }
                }
                Some(FeedMessage::Trade(t)) => {
                    state.metrics.market_events_total.inc();
                    // Liveness is data, not connection state.
                    state.mark_source_data(Utc::now());
                    // One hash lookup for the ~99% that are not ours.
                    match state.tracker.observe_live(*t) {
                        Detection::NotTracked => {}
                        Detection::Skipped { reason, .. } => {
                            state.metrics.source_trades_skipped_total.inc();
                            if matches!(reason, domain::SignalSkipReason::DuplicateEvent) {
                                state.metrics.duplicates_suppressed_total.inc();
                            }
                        }
                        Detection::Actionable(trade) => {
                            pipe.on_source_trade(*trade).await;
                        }
                    }
                }
            }
        }
    }
}

/// Backfills tracked wallets over a feed gap.
///
/// `takerOnly=false` is used inside the REST client, otherwise maker fills would be
/// invisible here while visible on the live feed.
async fn backfill_gap(
    pipe: &Arc<Pipeline>,
    state: &Arc<AppState>,
    rest: &PolymarketRest,
    since: chrono::DateTime<Utc>,
) -> u32 {
    let mut count = 0;
    for w in state.tracker.list_wallets() {
        if !w.enabled { continue; }
        match rest.backfill_wallet_trades(&w.address, since, 500).await {
            Err(e) => warn!(wallet = %w.address, error = %e, "backfill failed"),
            Ok(rows) => {
                // Ordinals per identical row within this batch, so overlap with the live
                // feed reconciles exactly rather than double-copying.
                let mut batch = BatchOrdinals::new();
                for t in rows {
                    if let Detection::Actionable(trade) = state.tracker.observe_backfill(t, &mut batch) {
                        count += 1;
                        pipe.on_source_trade(*trade).await;
                    } else {
                        state.metrics.duplicates_suppressed_total.inc();
                    }
                }
            }
        }
    }
    count
}

/// Keeps order books fresh for the tokens we care about.
#[allow(clippy::too_many_arguments)]
pub async fn run_market_data(
    state: Arc<AppState>,
    books: Arc<BookCache>,
    paper: Option<Arc<PaperExecution>>,
    cfg: config::AppConfig,
    subs: Option<Arc<market_data::TokenSubscriptions>>,
    mut shutdown: Shutdown,
) {
    let Ok(rest) = PolymarketRest::new(
        cfg.endpoints.clob_url.clone(),
        cfg.endpoints.gamma_url.clone(),
        cfg.endpoints.data_api_url.clone(),
        cfg.endpoints.http_timeout_ms,
    ) else {
        error!("could not build the REST client; market data is unavailable");
        state.health.set("market_data", HealthState::Down, "client construction failed");
        return;
    };

    // Seed with markets that actually have books, so paper fills are realistic.
    let mut tokens: Vec<domain::TokenId> = Vec::new();
    match rest.sampling_markets().await {
        Ok(ms) => {
            for m in ms.iter().filter(|m| m.is_tradable()).take(40) {
                for o in &m.outcomes { tokens.push(o.token_id.clone()); }
                // Keep a durable record of the market, so an order placed today can still
                // be interpreted after the market closes and drops out of the API.
                let _ = state.repos.upsert_market(m).await;
            }
            // Seed the stream too, so the most liquid books are warm before any target
            // trades and the very first copy is not paying a REST round trip.
            if let Some(s) = &subs {
                s.extend(tokens.iter().take(market_data::MAX_SUBSCRIBED_TOKENS / 2).cloned());
                info!(followed = s.len(), "seeded streaming book subscriptions");
            }
            info!(markets = ms.len(), tokens = tokens.len(), "seeded market data");
            state.health.set("market_data", HealthState::Healthy, "seeded");
        }
        Err(e) => {
            warn!(error = %e, "could not seed markets");
            state.health.set("market_data", HealthState::Degraded, e.to_string());
        }
    }

    let mut seq = 0u64;
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tokio::select! {
            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
            _ = tick.tick() => {
                let mut ok = 0;
                // Refresh a rotating slice so one pass never stalls on a large set.
                for t in tokens.iter().take(20) {
                    seq += 1;
                    match rest.book(t, seq).await {
                        Ok(b) => {
                            ok += 1;
                            state.metrics.market_events_total.inc();
                            // Mark positions and let resting paper orders fill.
                            if let Some(mid) = b.mid() {
                                state.portfolio.mark(&b.token_id, mid, Utc::now());
                            }
                            if let Some(p) = &paper {
                                for f in p.on_book_update(&b) {
                                    if state.orders.apply_external_fill(&f).is_ok() {
                                        state.portfolio.apply_fill(&f, "");
                                        state.metrics.orders_filled_total.inc();
                                        // Same FK ordering rule as the submit path: the
                                        // order row must be current before its fill.
                                        if let Some(o) = state.orders.get(f.order_id) {
                                            let _ = state.repos
                                                .upsert_order(&o, state.mode.as_str()).await;
                                        }
                                        if let Err(e) = state.repos.insert_fill(&f).await {
                                            warn!(error = %e, "failed to persist resting-order fill");
                                        }
                                    }
                                }
                            }
                            books.put(b);
                        }
                        Err(e) => debug!(error = %e, "book fetch failed"),
                    }
                }
                if ok > 0 {
                    state.health.set("market_data", HealthState::Healthy, format!("{ok} books refreshed"));
                } else if !tokens.is_empty() {
                    state.health.set("market_data", HealthState::Degraded, "no books refreshed");
                }
                let step = 20.min(tokens.len());
                tokens.rotate_left(step);
            }
        }
    }
}

/// Generates labelled synthetic activity so the dashboard is populated without credentials.
pub async fn run_demo(
    pipe: Arc<Pipeline>,
    books: Arc<BookCache>,
    paper: Option<Arc<PaperExecution>>,
    mut gen: DemoGenerator,
    mut shutdown: Shutdown,
) {
    info!("DEMO generator running — all activity is synthetic and labelled DEMO");
    // Seed books first so the very first demo order has liquidity to trade against.
    for i in 0..gen.markets.len() {
        for leg in [true, false] { books.put(gen.book(i, leg)); }
    }
    let mut trade_tick = tokio::time::interval(Duration::from_millis(2500));
    let mut book_tick = tokio::time::interval(Duration::from_millis(1200));
    loop {
        tokio::select! {
            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
            _ = book_tick.tick() => {
                gen.step_prices();
                for i in 0..gen.markets.len() {
                    for leg in [true, false] {
                        let b = gen.book(i, leg);
                        if let Some(mid) = b.mid() {
                            pipe.state.portfolio.mark(&b.token_id, mid, Utc::now());
                        }
                        if let Some(p) = &paper {
                            for f in p.on_book_update(&b) {
                                if pipe.state.orders.apply_external_fill(&f).is_ok() {
                                    pipe.state.portfolio.apply_fill(&f, "");
                                }
                            }
                        }
                        books.put(b);
                    }
                }
            }
            _ = trade_tick.tick() => {
                let t = gen.source_trade();
                debug_assert_eq!(t.source, TradeSource::Demo);
                if let Detection::Actionable(trade) = pipe.state.tracker.observe_live(
                    market_data::ParsedTrade {
                        trader: t.trader.clone(), market_id: t.market_id.clone(),
                        token_id: t.token_id.clone(), outcome: t.outcome.clone(),
                        side: t.side, price: t.price, quantity: t.quantity,
                        tx_hash: t.tx_hash.clone(), source_ts: t.source_ts,
                        source_is_coarse: false, detected_ts: t.detected_ts,
                        market_title: t.market_title.clone(), market_slug: t.market_slug.clone(),
                        source: TradeSource::Demo,
                    })
                {
                    pipe.on_source_trade(*trade).await;
                }
            }
        }
    }
}

/// Replays a recorded session through the identical pipeline.
pub async fn run_replay(
    pipe: Arc<Pipeline>,
    books: Arc<BookCache>,
    file: String,
    mut shutdown: Shutdown,
) {
    let session = match ReplaySession::load(&file) {
        Ok(s) => s,
        Err(e) => { error!(error = %e, "replay failed to load"); return; }
    };
    let trades = session.to_source_trades(Utc::now());
    info!(file = %file, events = trades.len(), "replaying session");

    // A recorded session names its own cast of traders, and the pipeline only acts on
    // *tracked* wallets. Without this, replaying a capture of real market activity
    // silently produces nothing at all — the session's wallets are by definition not the
    // ones in your config. Any wallet appearing in the file is therefore auto-registered
    // for the duration of the replay, unless it is already configured (in which case the
    // operator's own limits win).
    let mut registered = 0;
    for t in &trades {
        if pipe.state.tracker.get_wallet(&t.trader).is_none() {
            let mut w = domain::TargetWallet::new(
                t.trader.clone(),
                format!("replay-{}", &t.trader.as_str()[..8]),
            );
            // Replay is for exercising the pipeline, so do not filter on source size;
            // the configured risk limits still apply in full.
            w.min_source_notional_usd = domain::Usd::ZERO;
            pipe.state.tracker.upsert_wallet(w);
            registered += 1;
        }
    }
    if registered > 0 {
        info!(registered, "auto-registered replay wallets (replay scope only)");
    }

    // Synthesise a book around each replayed price so the simulator has liquidity.
    //
    // The spread is **one tick**, not an arbitrary percentage. Polymarket books are
    // routinely one tick wide, and a wider synthetic spread would push every replayed
    // order past the slippage budget and get it rejected — making replay look broken
    // when it is the fixture that is unrealistic.
    let tick = dec!(0.001);
    for t in &trades {
        let (Ok(bid), Ok(ask)) = (
            domain::Price::new((t.price.get() - tick).max(dec!(0.001))),
            domain::Price::new((t.price.get() + tick).min(dec!(0.999))),
        ) else { continue };
        // `Qty::new` on a positive constant cannot fail.
        books.put(domain::OrderBook {
            market_id: t.market_id.clone(),
            token_id: t.token_id.clone(),
            bids: vec![domain::Level { price: bid, size: domain::Qty::new(dec!(100000)).unwrap() }],
            asks: vec![domain::Level { price: ask, size: domain::Qty::new(dec!(100000)).unwrap() }],
            tick_size: tick,
            min_order_size: dec!(5),
            timestamp: Utc::now(),
            seq: 0,
        });
    }

    // Modelled venue publish delay, matching the ~392ms median measured on the live feed.
    const REPLAY_PUBLISH_DELAY_MS: i64 = 400;

    for mut t in trades {
        if *shutdown.borrow() { return; }

        // Re-stamp at the moment of processing.
        //
        // `to_source_trades` rebases the recording onto a synthetic timeline to preserve
        // the original event spacing, which is what makes replay deterministic. But those
        // stamps describe the *recording*, not this run. Feeding them into the latency
        // chain would compare a synthetic clock against real wall-clock stamps taken
        // downstream and report latency that never happened — one earlier run showed a
        // fabricated 606 ms "strategy" stage this way.
        //
        // The ordering and spacing above are what replay is for; the stamps below are what
        // makes its latency numbers real.
        let now = Utc::now();
        t.source_ts = now - chrono::Duration::milliseconds(REPLAY_PUBLISH_DELAY_MS);
        t.detected_ts = now;

        pipe.on_source_trade(t).await;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
        }
    }
    info!("replay complete");
    pipe.state.health.set("source_feed", HealthState::Healthy, "replay finished");
}

/// Persists the durable event log.
///
/// Subscribes to the internal bus rather than threading a repository handle through every
/// component, so adding a new event type does not require touching persistence. Critical
/// events (kill switch, reconciliation mismatch, feed loss, risk breach) are always
/// recorded; routine per-trade events are already captured in their own tables and would
/// otherwise duplicate a high-volume write path for no benefit.
pub async fn run_event_log(state: Arc<AppState>, mut shutdown: Shutdown) {
    let mut rx = state.events.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
            ev = rx.recv() => match ev {
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                // Dropping routine events under load is acceptable; the critical ones we
                // care about are low-volume and will be caught on the next iteration.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "event log lagged");
                }
                Ok(e) => {
                    if e.is_critical() || matches!(e.kind(),
                        "kill_switch_reset" | "health_changed" | "feed_reconnected")
                    {
                        if let Err(err) = state.repos.insert_system_event(&e).await {
                            debug!(error = %err, "failed to persist system event");
                        }
                    }
                }
            }
        }
    }
}

/// Periodic snapshots, dedup eviction and staleness detection.
pub async fn run_housekeeping(state: Arc<AppState>, mut shutdown: Shutdown) {
    let mut snap_tick = tokio::time::interval(Duration::from_secs(15));
    let mut evict_tick = tokio::time::interval(Duration::from_secs(300));
    loop {
        tokio::select! {
            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
            _ = snap_tick.tick() => {
                let s = state.portfolio.snapshot(state.orders.open_count());
                let _ = state.repos.insert_pnl_snapshot(&s).await;
                for p in state.portfolio.positions() {
                    let _ = state.repos.upsert_position(&p).await;
                }
                let _ = state.events.send(SystemEvent::PnlUpdated { snapshot: Box::new(s) });
                state.health.set("database",
                    if state.repos.is_ephemeral() { HealthState::Degraded } else { HealthState::Healthy },
                    if state.repos.is_ephemeral() { "ephemeral: no durable audit" } else { "ok" });
                state.health.set("execution", HealthState::Healthy, state.orders.adapter_name());
                // A feed that stopped delivering without disconnecting is caught here.
                state.health.expire_stale(chrono::Duration::seconds(120), Utc::now());

                // And a feed that keeps *reconnecting* while delivering nothing: each
                // reconnect refreshes connection health, so only the data clock reveals it.
                const SILENT_FEED_MS: i64 = 90_000;
                match state.source_data_age_ms(Utc::now()) {
                    Some(age) if age > SILENT_FEED_MS => {
                        state.health.set("source_feed", HealthState::Degraded,
                            format!("connected but silent for {}s", age / 1000));
                        state.metrics.feed_connected.set(0);
                    }
                    None if state.uptime_seconds() > SILENT_FEED_MS / 1000 => {
                        state.health.set("source_feed", HealthState::Degraded,
                            "connected but no trade has ever arrived");
                        state.metrics.feed_connected.set(0);
                    }
                    _ => {}
                }
            }
            _ = evict_tick.tick() => {
                state.tracker.evict_stale(Utc::now());
                debug!(contents = state.tracker.dedup_size(), "dedup index evicted");
            }
        }
    }
}

/// Compares our position book against the venue's.
pub async fn run_reconciliation(
    state: Arc<AppState>,
    adapter: Arc<dyn ExecutionAdapter>,
    mut shutdown: Shutdown,
) {
    if !adapter.capabilities().supports_position_query {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
            _ = tick.tick() => {
                let venue = match adapter.positions().await {
                    Ok(v) => v,
                    Err(e) => { debug!(error = %e, "position query failed"); continue; }
                };
                let internal = state.portfolio.exposure_map();
                let report = Reconciler::reconcile(&internal, &venue, dec!(0.000001), Utc::now());
                if report.is_clean() { continue; }

                warn!(summary = %report.summary(), "RECONCILIATION MISMATCH");
                state.metrics.reconciliation_mismatches_total.add(report.mismatches.len() as u64);
                for m in &report.mismatches {
                    let _ = state.events.send(SystemEvent::ReconciliationMismatch {
                        token_id: m.token_id.clone(),
                        internal: domain::Qty::new(m.internal.abs()).unwrap_or(domain::Qty::ZERO),
                        venue: domain::Qty::new(m.venue.abs()).unwrap_or(domain::Qty::ZERO),
                        at: Utc::now(),
                    });
                }
                // Never silently continue on a serious disagreement in live mode.
                if state.mode == AppMode::Live && report.warrants_halt(dec!(1)) {
                    let reason = format!("reconciliation mismatch: {}", report.summary());
                    state.kill_switch.engage(&reason, "reconciler");
                    state.metrics.kill_switch_activations_total.inc();
                    let _ = state.events.send(SystemEvent::KillSwitchActivated {
                        reason, by: "reconciler".into(), at: Utc::now() });
                    let (ok, fail) = state.orders.cancel_all().await;
                    error!(cancelled = ok, failed = fail, "trading halted pending investigation");
                }
            }
        }
    }
}
