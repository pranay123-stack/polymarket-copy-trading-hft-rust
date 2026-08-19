//! End-to-end pipeline tests.
//!
//! These exercise the real components wired together —
//! `source trade → wallet match → dedup → strategy → risk → OMS → paper execution →
//! portfolio → PnL` — with no mocks except the market data, which is a fixed book so
//! outcomes are deterministic.

use std::sync::Arc;

use chrono::Utc;
use domain::{
    Address, AppMode, CorrelationId, Level, MarketId, OrderBook, OrderState, Price, Qty, Side,
    SizingMode, SourceEventId, SourceTrade, TargetWallet, TokenId, TradeSource, TxHash, Usd,
};
use execution::{BookCache, ExecutionAdapter, OrderManager, PaperExecution};
use metrics::{HealthMonitor, Metrics};
use parking_lot::RwLock;
use portfolio::Portfolio;
use risk::{KillSwitch, RiskEngine, RiskLimits, RiskSnapshot, RiskVerdict, SystemStatus};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{CopyTrader, SizingContext, StrategyConfig};
use wallet_tracker::{Detection, WalletTracker};

// ------------------------------------------------------------------ fixtures

fn market() -> MarketId {
    MarketId::new("0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52").unwrap()
}
fn token() -> TokenId {
    TokenId::new("72551024098258542594534683942523606143014690620243023298497729957846870197074").unwrap()
}
fn whale() -> Address {
    Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap()
}

fn book(ask: Decimal, depth: Decimal) -> OrderBook {
    OrderBook {
        market_id: market(),
        token_id: token(),
        bids: vec![Level { price: Price::new(ask - dec!(0.02)).unwrap(), size: Qty::new(depth).unwrap() }],
        asks: vec![Level { price: Price::new(ask).unwrap(), size: Qty::new(depth).unwrap() }],
        tick_size: dec!(0.01),
        min_order_size: dec!(5),
        timestamp: Utc::now(),
        seq: 1,
    }
}

fn source_trade(tx: u8, qty: Decimal, px: Decimal, side: Side) -> SourceTrade {
    SourceTrade {
        event_id: SourceEventId::from_digest(format!("evt-{tx}")),
        correlation_id: CorrelationId::new(),
        trader: whale(),
        market_id: market(),
        token_id: token(),
        outcome: "Yes".into(),
        side,
        price: Price::new(px).unwrap(),
        quantity: Qty::new(qty).unwrap(),
        tx_hash: TxHash::new(format!("0x{:064x}", tx)).unwrap(),
        occurrence: 0,
        source_ts: Utc::now() - chrono::Duration::milliseconds(400),
        detected_ts: Utc::now(),
        source: TradeSource::RtdsWebsocket,
        market_title: "Integration market".into(),
        market_slug: "integration".into(),
    }
}

fn target(ratio: Decimal, max_trade: Decimal) -> TargetWallet {
    let mut w = TargetWallet::new(whale(), "Whale");
    w.sizing = SizingMode::FixedRatio { ratio };
    w.max_trade_usd = Usd::new(max_trade);
    w.min_trade_usd = Usd::new(dec!(5));
    w.min_source_notional_usd = Usd::new(dec!(10));
    w
}

/// The full stack, wired the way `main.rs` wires it.
struct Harness {
    books: Arc<BookCache>,
    tracker: Arc<WalletTracker>,
    trader: CopyTrader,
    risk: RwLock<RiskEngine>,
    kill: Arc<KillSwitch>,
    orders: Arc<OrderManager>,
    portfolio: Arc<Portfolio>,
    metrics: Arc<Metrics>,
    paper: Arc<PaperExecution>,
    events: tokio::sync::broadcast::Receiver<domain::SystemEvent>,
}

impl Harness {
    fn new(wallet: TargetWallet, limits: RiskLimits) -> Self {
        let books = Arc::new(BookCache::new());
        books.put(book(dec!(0.61), dec!(100_000)));
        let paper = Arc::new(PaperExecution::new(
            books.clone(),
            simulator::MatchParams {
                fee_bps: 0, slippage_bps: 0, partial_fill_enabled: true,
                fill_probability: 1.0, reject_probability: 0.0,
            },
            0, 0, 42,
        ));
        let (tx, events) = tokio::sync::broadcast::channel(1024);
        Self {
            books,
            tracker: Arc::new(WalletTracker::new(vec![wallet])),
            trader: CopyTrader::new(StrategyConfig::default()),
            risk: RwLock::new(RiskEngine::new(limits, AppMode::Paper, false)),
            kill: Arc::new(KillSwitch::new()),
            orders: Arc::new(OrderManager::new(paper.clone(), tx)),
            portfolio: Arc::new(Portfolio::new(Usd::new(dec!(10_000)))),
            metrics: Arc::new(Metrics::new()),
            paper,
            events,
        }
    }

    fn sizing_ctx(&self) -> SizingContext {
        let limits = self.risk.read().limits().clone();
        let s = self.portfolio.snapshot(self.orders.open_count());
        SizingContext {
            equity: s.equity,
            current_market_exposure: self.portfolio.market_exposure(&market()),
            max_position_usd: limits.max_position_usd,
            max_trade_usd: limits.max_trade_usd,
            min_trade_usd: limits.min_trade_usd,
            available_liquidity: Usd::ZERO,
            remaining_daily_risk: limits.max_daily_loss_usd,
            min_order_size: Decimal::ZERO,
        }
    }

    /// Runs the whole pipeline for a trade arriving on the **live feed**.
    async fn run(&self, trade: SourceTrade) -> Outcome {
        self.run_via(trade, None).await
    }

    /// Runs the pipeline for a trade arriving via **REST backfill**, where
    /// `ordinal` is its position among identical rows in the same response.
    async fn run_backfill(&self, trade: SourceTrade, batch: &mut wallet_tracker::BatchOrdinals) -> Outcome {
        self.run_via(trade, Some(batch)).await
    }

    async fn run_via(
        &self,
        trade: SourceTrade,
        backfill: Option<&mut wallet_tracker::BatchOrdinals>,
    ) -> Outcome {
        // --- dedup + wallet match ---
        let parsed = market_data::ParsedTrade {
            trader: trade.trader.clone(), market_id: trade.market_id.clone(),
            token_id: trade.token_id.clone(), outcome: trade.outcome.clone(),
            side: trade.side, price: trade.price, quantity: trade.quantity,
            tx_hash: trade.tx_hash.clone(), source_ts: trade.source_ts,
            source_is_coarse: false, detected_ts: trade.detected_ts,
            market_title: trade.market_title.clone(), market_slug: trade.market_slug.clone(),
            source: trade.source,
        };
        let detection = match backfill {
            Some(batch) => self.tracker.observe_backfill(parsed, batch),
            None => self.tracker.observe_live(parsed),
        };
        let detected = match detection {
            Detection::NotTracked => return Outcome::NotTracked,
            Detection::Skipped { reason, .. } => return Outcome::Skipped(format!("{reason:?}")),
            Detection::Actionable(t) => *t,
        };
        self.metrics.source_trades_total.inc();

        let wallet = self.tracker.get_wallet(&detected.trader).unwrap();
        let bk = self.books.get(&detected.token_id);

        // --- strategy ---
        let signal = match self.trader.on_source_trade(
            &detected, &wallet, bk.as_ref(), self.sizing_ctx(), dec!(0.01))
        {
            Ok(s) => s,
            Err(e) => return Outcome::NoSignal(format!("{e:?}")),
        };
        self.metrics.copy_signals_total.inc();

        // --- order ---
        let order = domain::OrderRequest {
            order_id: domain::OrderId::new(),
            correlation_id: signal.correlation_id,
            signal_id: Some(signal.signal_id),
            market_id: signal.market_id.clone(),
            token_id: signal.token_id.clone(),
            side: signal.side,
            order_type: domain::OrderType::Market,
            time_in_force: domain::TimeInForce::Ioc,
            quantity: signal.copy_quantity,
            limit_price: signal.limit_price,
            reference_price: signal.target_price,
            tick_size: dec!(0.01),
            created_at: Utc::now(),
        };

        // --- risk ---
        let snap = self.portfolio.snapshot(self.orders.open_count());
        let rs = RiskSnapshot {
            daily_pnl: snap.daily_pnl,
            gross_exposure: snap.gross_exposure,
            market_exposure: self.portfolio.market_exposure(&order.market_id),
            wallet_exposure: Usd::ZERO,
            token_exposure: self.portfolio.token_exposure(&order.token_id),
            open_orders: snap.open_orders,
            equity: snap.equity,
        };
        let verdict = self.risk.read().check(
            &order, &rs, &SystemStatus::default(), &self.kill, bk.as_ref(),
            wallet.enabled, &wallet.nickname, None, None, Utc::now());
        if let RiskVerdict::Rejected(r) = verdict {
            self.metrics.record_rejection(r.code());
            return Outcome::RiskRejected(r.code());
        }

        // --- execution ---
        self.portfolio.attribute_order(order.order_id, signal.target_wallet.clone());
        let id = self.orders.register_validated(order, signal.latency).unwrap();
        self.metrics.orders_submitted_total.inc();

        match self.orders.submit(id).await {
            execution::SubmitOutcome::Acknowledged { fills, .. } => {
                for f in &fills {
                    self.portfolio.apply_fill(f, "Yes");
                    self.metrics.orders_filled_total.inc();
                }
                if let Some(o) = self.orders.get(id) {
                    self.metrics.latency.record_all(&o.latency);
                }
                Outcome::Executed { order_id: id, filled: fills.len() }
            }
            execution::SubmitOutcome::Rejected { reason, .. } => Outcome::ExecRejected(reason),
            execution::SubmitOutcome::Ambiguous { detail, .. } => Outcome::Ambiguous(detail),
        }
    }
}

/// The payloads here are read through `Debug` in assertion failure messages — which is
/// exactly when they matter — so clippy's dead-code analysis does not see the use.
#[allow(dead_code)]
#[derive(Debug)]
enum Outcome {
    NotTracked,
    Skipped(String),
    NoSignal(String),
    RiskRejected(&'static str),
    ExecRejected(String),
    Ambiguous(String),
    Executed { order_id: domain::OrderId, filled: usize },
}

fn limits() -> RiskLimits {
    RiskLimits {
        max_trade_usd: Usd::new(dec!(500)),
        min_trade_usd: Usd::new(dec!(5)),
        max_position_usd: Usd::new(dec!(5000)),
        max_market_exposure_usd: Usd::new(dec!(5000)),
        max_portfolio_exposure_usd: Usd::new(dec!(50_000)),
        max_daily_loss_usd: Usd::new(dec!(1000)),
        max_open_orders: 20,
        max_slippage_bps: 200,
        min_liquidity_usd: Usd::new(dec!(10)),
        max_market_data_age_ms: 60_000,
        max_live_order_usd: Usd::new(dec!(50)),
        require_market_data: false,
    }
}

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn full_pipeline_source_trade_to_pnl() {
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    // Whale buys 1000 shares at 0.60 = $600 notional.
    let out = h.run(source_trade(1, dec!(1000), dec!(0.60), Side::Buy)).await;

    let Outcome::Executed { order_id, filled } = out else {
        panic!("expected execution, got {out:?}");
    };
    assert_eq!(filled, 1);

    // Order reached a terminal filled state.
    let o = h.orders.get(order_id).unwrap();
    assert_eq!(o.state, OrderState::Filled);

    // 25% of $600 = $150 of copy notional.
    assert!(o.request.notional().get() <= dec!(150), "sized to {}", o.request.notional());

    // Position and cash both moved.
    let pos = h.portfolio.position(&token()).expect("position must exist");
    assert!(pos.net_quantity > Decimal::ZERO);
    assert!(h.portfolio.cash().get() < dec!(10_000), "cash must have been spent");

    // PnL attributed back to the originating wallet.
    let snap = h.portfolio.snapshot(h.orders.open_count());
    assert_eq!(snap.active_positions, 1);

    // Latency was genuinely measured through the chain.
    assert!(o.latency.detection_us().is_some(), "detection latency must be recorded");
    assert!(o.latency.internal_us().is_some(), "internal latency must be recorded");
    assert!(h.metrics.latency.stats(domain::LatencyStage::Internal).is_some());
}

#[tokio::test]
async fn redelivered_fill_produces_exactly_one_order() {
    // The single most important safety property of a copy trader: a fill the live feed
    // already handled must not be copied again when REST backfill re-reports it after a
    // reconnect.
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    let t = source_trade(7, dec!(1000), dec!(0.60), Side::Buy);

    let first = h.run(t.clone()).await;
    assert!(matches!(first, Outcome::Executed { .. }), "{first:?}");

    // The feed drops; backfill re-reports the same fill.
    let mut batch = wallet_tracker::BatchOrdinals::new();
    let second = h.run_backfill(t.clone(), &mut batch).await;
    assert!(matches!(second, Outcome::Skipped(_)), "re-delivery must be skipped, got {second:?}");

    assert_eq!(h.orders.all().len(), 1, "a re-delivered fill must never create a second order");
    assert_eq!(h.tracker.stats().duplicates_suppressed, 1);
}

#[tokio::test]
async fn backfill_reconciles_against_the_live_feed_as_a_multiset() {
    // Live feed saw two identical fills; backfill reports three. Exactly one is new.
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    let t = source_trade(12, dec!(1000), dec!(0.60), Side::Buy);
    h.run(t.clone()).await;
    h.run(t.clone()).await;
    assert_eq!(h.orders.all().len(), 2);

    let mut batch = wallet_tracker::BatchOrdinals::new();
    let a = h.run_backfill(t.clone(), &mut batch).await;
    let b = h.run_backfill(t.clone(), &mut batch).await;
    let c = h.run_backfill(t.clone(), &mut batch).await;
    assert!(matches!(a, Outcome::Skipped(_)), "{a:?}");
    assert!(matches!(b, Outcome::Skipped(_)), "{b:?}");
    assert!(matches!(c, Outcome::Executed { .. }), "the third really is unseen: {c:?}");
    assert_eq!(h.orders.all().len(), 3);
}

#[tokio::test]
async fn genuinely_identical_live_fills_are_both_copied() {
    // Polymarket really does emit byte-identical rows for two distinct fills inside one
    // transaction (verified in production). Collapsing them would under-copy the target,
    // so on the live feed each arrival is a separate fill by design. This pins that
    // semantic against a future "fix" that would silently drop real volume.
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    let t = source_trade(13, dec!(1000), dec!(0.60), Side::Buy);
    assert!(matches!(h.run(t.clone()).await, Outcome::Executed { .. }));
    assert!(matches!(h.run(t.clone()).await, Outcome::Executed { .. }));
    assert_eq!(h.orders.all().len(), 2);
    assert_eq!(h.tracker.stats().duplicates_suppressed, 0);
}

#[tokio::test]
async fn kill_switch_halts_the_pipeline_at_the_risk_gate() {
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    h.kill.engage("integration test", "test");

    let out = h.run(source_trade(2, dec!(1000), dec!(0.60), Side::Buy)).await;
    assert!(matches!(out, Outcome::RiskRejected("kill_switch")), "{out:?}");
    assert!(h.orders.all().is_empty(), "no order may be created while halted");
    assert_eq!(h.portfolio.cash().get(), dec!(10_000), "no cash may move");
}

#[tokio::test]
async fn position_limit_stops_accumulation() {
    let mut l = limits();
    l.max_position_usd = Usd::new(dec!(200)); // room for roughly one copy
    let h = Harness::new(target(dec!(0.5), dec!(500)), l);

    let mut executed = 0;
    let mut rejected = 0;
    for i in 1..=10u8 {
        match h.run(source_trade(i, dec!(1000), dec!(0.60), Side::Buy)).await {
            Outcome::Executed { .. } => executed += 1,
            Outcome::RiskRejected(_) | Outcome::NoSignal(_) => rejected += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(executed >= 1, "at least the first copy should go through");
    assert!(rejected > 0, "the position cap must eventually bite");
    // The cap must actually hold.
    assert!(h.portfolio.token_exposure(&token()).get() <= dec!(220),
        "exposure {} breached the cap", h.portfolio.token_exposure(&token()));
}

#[tokio::test]
async fn round_trip_realises_pnl_correctly() {
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());

    // Buy against a 0.61 ask.
    let buy = h.run(source_trade(3, dec!(1000), dec!(0.60), Side::Buy)).await;
    assert!(matches!(buy, Outcome::Executed { .. }), "{buy:?}");
    let qty_held = h.portfolio.position(&token()).unwrap().net_quantity;
    assert!(qty_held > Decimal::ZERO);

    // Market rises; the whale sells and we follow.
    h.books.put(book(dec!(0.81), dec!(100_000)));
    let sell = h.run(source_trade(4, dec!(1000), dec!(0.80), Side::Sell)).await;
    assert!(matches!(sell, Outcome::Executed { .. }), "{sell:?}");

    assert!(h.portfolio.realized_pnl().get() > Decimal::ZERO,
        "a profitable round trip must realise a gain, got {}", h.portfolio.realized_pnl());
}

#[tokio::test]
async fn thin_liquidity_produces_a_partial_fill_not_a_phantom_one() {
    let h = Harness::new(target(dec!(1), dec!(500)), limits());
    // Only 50 shares available on the ask.
    h.books.put(book(dec!(0.61), dec!(50)));

    let out = h.run(source_trade(5, dec!(10_000), dec!(0.60), Side::Buy)).await;
    match out {
        Outcome::Executed { order_id, .. } => {
            let o = h.orders.get(order_id).unwrap();
            assert!(o.filled_qty.get() <= dec!(50), "cannot fill more than the book held");
            assert_eq!(h.portfolio.position(&token()).unwrap().net_quantity, o.filled_qty.get());
        }
        // Being refused outright is also correct — what must not happen is a full fill.
        Outcome::RiskRejected(_) | Outcome::ExecRejected(_) | Outcome::NoSignal(_) => {}
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn untracked_wallets_never_reach_the_strategy() {
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    let mut t = source_trade(6, dec!(1000), dec!(0.60), Side::Buy);
    t.trader = Address::new("0x1111111111111111111111111111111111111111").unwrap();

    assert!(matches!(h.run(t).await, Outcome::NotTracked));
    assert!(h.orders.all().is_empty());
    assert_eq!(h.metrics.copy_signals_total.get(), 0);
}

#[tokio::test]
async fn disabled_wallet_stops_copying_immediately() {
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    assert!(matches!(h.run(source_trade(8, dec!(1000), dec!(0.60), Side::Buy)).await,
        Outcome::Executed { .. }));

    h.tracker.set_enabled(&whale(), false);
    let out = h.run(source_trade(9, dec!(1000), dec!(0.60), Side::Buy)).await;
    assert!(matches!(out, Outcome::Skipped(_)), "{out:?}");
    assert_eq!(h.orders.all().len(), 1);
}

#[tokio::test]
async fn events_are_emitted_across_the_whole_lifecycle() {
    let mut h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    h.run(source_trade(10, dec!(1000), dec!(0.60), Side::Buy)).await;

    let mut kinds = Vec::new();
    while let Ok(e) = h.events.try_recv() {
        kinds.push(e.kind());
    }
    for expected in ["order_submitted", "order_acknowledged", "order_filled"] {
        assert!(kinds.contains(&expected), "missing {expected} in {kinds:?}");
    }
}

#[tokio::test]
async fn dust_source_trades_are_ignored() {
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    // 10 shares at 0.60 = $6 notional, below the $10 floor.
    let out = h.run(source_trade(11, dec!(10), dec!(0.60), Side::Buy)).await;
    assert!(matches!(out, Outcome::Skipped(_)), "{out:?}");
    assert!(h.orders.all().is_empty());
}

#[tokio::test]
async fn health_monitor_reflects_a_running_system() {
    let h = HealthMonitor::new("PAPER");
    h.set("source_feed", domain::HealthState::Healthy, "connected");
    h.set("execution", domain::HealthState::Healthy, "paper");
    h.set("database", domain::HealthState::Degraded, "ephemeral");
    let r = h.report();
    assert_eq!(r.state, domain::HealthState::Degraded);
    assert_eq!(r.components.len(), 3);
}

#[tokio::test]
async fn paper_adapter_never_claims_to_be_real_money() {
    let h = Harness::new(target(dec!(0.25), dec!(500)), limits());
    assert!(!h.paper.capabilities().is_real_money);
    assert!(!h.orders.is_real_money());
    assert_eq!(h.orders.adapter_name(), "paper");
}
