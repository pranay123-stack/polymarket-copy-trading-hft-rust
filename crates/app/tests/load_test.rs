//! Load test.
//!
//! Drives a high volume of source events through the real detection, dedup, sizing and
//! risk path and measures sustained throughput and latency. Marked `#[ignore]` so it
//! does not slow the normal suite; run with:
//!
//! ```text
//! cargo test -p app --release --test load_test -- --ignored --nocapture
//! ```
//!
//! Results are printed, not asserted against absolute numbers — those depend on the
//! machine. The assertions cover *correctness under load*: no duplicates, no limit
//! breach, no lost events.

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use domain::{
    Address, AppMode, Level, MarketId, OrderBook, Price, Qty, Side, SizingMode, TargetWallet,
    TokenId, TradeSource, TxHash, Usd,
};
use market_data::ParsedTrade;
use risk::{KillSwitch, RiskEngine, RiskLimits, RiskSnapshot, SystemStatus};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{CopyTrader, SizingContext, StrategyConfig};
use wallet_tracker::{Detection, WalletTracker};

const WALLETS: usize = 20;
const EVENTS: usize = 200_000;

fn token(i: usize) -> TokenId { TokenId::new(format!("{}", 700_000_000_000u64 + i as u64)).unwrap() }
fn market(i: usize) -> MarketId { MarketId::new(format!("0x{:064x}", 0xA000 + i)).unwrap() }
fn wallet_addr(i: usize) -> Address { Address::new(format!("0x{:040x}", 0xB00 + i)).unwrap() }

fn book(i: usize) -> OrderBook {
    OrderBook {
        market_id: market(i % 10),
        token_id: token(i % 10),
        bids: vec![Level { price: Price::new(dec!(0.49)).unwrap(), size: Qty::new(dec!(1000000)).unwrap() }],
        asks: vec![Level { price: Price::new(dec!(0.51)).unwrap(), size: Qty::new(dec!(1000000)).unwrap() }],
        tick_size: dec!(0.01),
        min_order_size: dec!(5),
        timestamp: Utc::now(),
        seq: i as u64,
    }
}

fn percentiles(mut v: Vec<u64>) -> (u64, u64, u64, u64) {
    v.sort_unstable();
    let n = v.len();
    if n == 0 { return (0, 0, 0, 0); }
    let at = |p: f64| v[((p * n as f64) as usize).min(n - 1)];
    (at(0.50), at(0.95), at(0.99), v[n - 1])
}

#[test]
#[ignore = "load test: run explicitly with --ignored --release"]
fn sustained_source_event_throughput() {
    let wallets: Vec<TargetWallet> = (0..WALLETS)
        .map(|i| {
            let mut w = TargetWallet::new(wallet_addr(i), format!("W{i}"));
            w.sizing = SizingMode::FixedRatio { ratio: dec!(0.25) };
            w.max_trade_usd = Usd::new(dec!(100));
            w.min_source_notional_usd = Usd::new(dec!(10));
            w
        })
        .collect();
    let tracker = Arc::new(WalletTracker::new(wallets));
    let trader = CopyTrader::new(StrategyConfig::default());
    let limits = RiskLimits {
        max_trade_usd: Usd::new(dec!(100)),
        min_trade_usd: Usd::new(dec!(5)),
        max_position_usd: Usd::new(dec!(1_000_000)),
        max_market_exposure_usd: Usd::new(dec!(1_000_000)),
        max_portfolio_exposure_usd: Usd::new(dec!(100_000_000)),
        max_daily_loss_usd: Usd::new(dec!(1_000_000)),
        max_open_orders: 1_000_000,
        max_slippage_bps: 500,
        min_liquidity_usd: Usd::new(dec!(1)),
        max_market_data_age_ms: 600_000,
        max_live_order_usd: Usd::new(dec!(50)),
        require_market_data: false,
    };
    let engine = RiskEngine::new(limits.clone(), AppMode::Paper, false);
    let kill = KillSwitch::new();
    let books: Vec<OrderBook> = (0..10).map(book).collect();

    let ctx = SizingContext {
        equity: Usd::new(dec!(1_000_000)),
        current_market_exposure: Usd::ZERO,
        max_position_usd: limits.max_position_usd,
        max_trade_usd: limits.max_trade_usd,
        min_trade_usd: limits.min_trade_usd,
        available_liquidity: Usd::ZERO,
        remaining_daily_risk: limits.max_daily_loss_usd,
        min_order_size: dec!(5),
    };

    // Mixed traffic: ~25% from tracked wallets, matching the real firehose where the
    // overwhelming majority of frames are not ours.
    let mut detect_ns = Vec::with_capacity(EVENTS / 4);
    let mut signal_ns = Vec::with_capacity(EVENTS / 4);
    let mut risk_ns = Vec::with_capacity(EVENTS / 4);
    let (mut actionable, mut signals, mut approved, mut skipped) = (0u64, 0u64, 0u64, 0u64);

    let start = Instant::now();
    for i in 0..EVENTS {
        let tracked = i % 4 == 0;
        let trader_addr = if tracked {
            wallet_addr(i % WALLETS)
        } else {
            Address::new(format!("0x{:040x}", 0xF000_0000u64 + i as u64)).unwrap()
        };
        let now = Utc::now();
        let parsed = ParsedTrade {
            trader: trader_addr,
            market_id: market(i % 10),
            token_id: token(i % 10),
            outcome: "Yes".into(),
            side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
            price: Price::new(dec!(0.50)).unwrap(),
            quantity: Qty::new(Decimal::from(100 + (i % 900))).unwrap(),
            // Unique per event, so dedup does real work rather than short-circuiting.
            tx_hash: TxHash::new(format!("0x{:064x}", i)).unwrap(),
            source_ts: now,
            source_is_coarse: false,
            detected_ts: now,
            market_title: "Load".into(),
            market_slug: "load".into(),
            source: TradeSource::RtdsWebsocket,
        };

        let t0 = Instant::now();
        let d = tracker.observe_live(parsed);
        detect_ns.push(t0.elapsed().as_nanos() as u64);

        let trade = match d {
            Detection::Actionable(t) => { actionable += 1; *t }
            Detection::Skipped { .. } => { skipped += 1; continue }
            Detection::NotTracked => continue,
        };

        let w = tracker.get_wallet(&trade.trader).unwrap();
        let bk = &books[i % 10];

        let t1 = Instant::now();
        let sig = trader.on_source_trade(&trade, &w, Some(bk), ctx.clone(), dec!(0.01));
        signal_ns.push(t1.elapsed().as_nanos() as u64);

        let Ok(sig) = sig else { continue };
        signals += 1;

        let order = domain::OrderRequest {
            order_id: domain::OrderId::new(),
            correlation_id: sig.correlation_id,
            signal_id: Some(sig.signal_id),
            market_id: sig.market_id.clone(),
            token_id: sig.token_id.clone(),
            side: sig.side,
            order_type: domain::OrderType::Market,
            time_in_force: domain::TimeInForce::Ioc,
            quantity: sig.copy_quantity,
            limit_price: sig.limit_price,
            reference_price: sig.target_price,
            tick_size: dec!(0.01),
            created_at: Utc::now(),
        };
        let snap = RiskSnapshot {
            daily_pnl: Usd::ZERO, gross_exposure: Usd::ZERO, market_exposure: Usd::ZERO,
            wallet_exposure: Usd::ZERO, token_exposure: Usd::ZERO, open_orders: 0,
            equity: Usd::new(dec!(1_000_000)),
        };
        let t2 = Instant::now();
        let v = engine.check(&order, &snap, &SystemStatus::default(), &kill, Some(bk),
            true, &w.nickname, None, None, Utc::now());
        risk_ns.push(t2.elapsed().as_nanos() as u64);
        if v.is_approved() { approved += 1; }

        // Every approved order must respect the trade cap, under any load.
        assert!(order.notional() <= limits.max_trade_usd,
            "order notional {} breached the cap at event {i}", order.notional());
    }
    let elapsed = start.elapsed();

    let (d50, d95, d99, dmax) = percentiles(detect_ns);
    let (s50, s95, s99, smax) = percentiles(signal_ns);
    let (r50, r95, r99, rmax) = percentiles(risk_ns);
    let eps = EVENTS as f64 / elapsed.as_secs_f64();

    println!("\n╔════════════════════════════════════════════════════════════════╗");
    println!("║  LOAD TEST — {EVENTS} source events, {WALLETS} tracked wallets");
    println!("╠════════════════════════════════════════════════════════════════╣");
    println!("║  wall clock          : {:.2}s", elapsed.as_secs_f64());
    println!("║  events/sec          : {eps:.0}");
    println!("║  actionable          : {actionable}  ({:.1}%)", actionable as f64 / EVENTS as f64 * 100.0);
    println!("║  signals/sec         : {:.0}", signals as f64 / elapsed.as_secs_f64());
    println!("║  risk-approved       : {approved}");
    println!("║  skipped (dedup/dust): {skipped}");
    println!("╠════════════════════════════════════════════════════════════════╣");
    println!("║  stage           p50        p95        p99        max");
    println!("║  detect+dedup  {d50:>7}ns {d95:>7}ns {d99:>7}ns {dmax:>7}ns");
    println!("║  signal gen    {s50:>7}ns {s95:>7}ns {s99:>7}ns {smax:>7}ns");
    println!("║  risk check    {r50:>7}ns {r95:>7}ns {r99:>7}ns {rmax:>7}ns");
    println!("╠════════════════════════════════════════════════════════════════╣");
    println!("║  dedup index size    : {}", tracker.dedup_size());
    println!("║  headroom vs live    : {:.0}x (live feed measured ~33 msg/s)", eps / 33.0);
    println!("╚════════════════════════════════════════════════════════════════╝\n");

    // Correctness under load — the part that is actually asserted.
    assert_eq!(actionable, (EVENTS / 4) as u64, "every tracked event must be detected exactly once");
    assert_eq!(skipped, 0, "unique tx hashes must not be deduplicated");
    assert_eq!(tracker.dedup_size(), EVENTS / 4, "dedup index must hold one entry per fill");
    assert!(eps > 1_000.0, "throughput {eps:.0}/s is implausibly low");
}

#[test]
#[ignore = "load test: run explicitly with --ignored --release"]
fn duplicate_storm_is_fully_suppressed() {
    // A pathological re-delivery storm: the same fill offered 50,000 times via backfill.
    // Exactly one may be actionable.
    let mut w = TargetWallet::new(wallet_addr(0), "W");
    w.min_source_notional_usd = Usd::ZERO;
    let tracker = WalletTracker::new(vec![w]);

    let make = || ParsedTrade {
        trader: wallet_addr(0),
        market_id: market(0),
        token_id: token(0),
        outcome: "Yes".into(),
        side: Side::Buy,
        price: Price::new(dec!(0.5)).unwrap(),
        quantity: Qty::new(dec!(100)).unwrap(),
        tx_hash: TxHash::new(format!("0x{:064x}", 1)).unwrap(),
        source_ts: Utc::now(),
        source_is_coarse: false,
        detected_ts: Utc::now(),
        market_title: "T".into(),
        market_slug: "t".into(),
        source: TradeSource::RestBackfill,
    };

    let start = Instant::now();
    let mut actionable = 0;
    // The live feed saw it once.
    assert!(matches!(tracker.observe_live(make()), Detection::Actionable(_)));
    // Backfill re-reports it 50k times within one batch.
    let mut batch = wallet_tracker::BatchOrdinals::new();
    for _ in 0..50_000 {
        if matches!(tracker.observe_backfill(make(), &mut batch), Detection::Actionable(_)) {
            actionable += 1;
        }
    }
    let elapsed = start.elapsed();

    println!("\nduplicate storm: 50,000 re-deliveries in {:.3}s ({:.0}/s)",
        elapsed.as_secs_f64(), 50_000.0 / elapsed.as_secs_f64());
    println!("  newly actionable beyond the first: {actionable}");
    println!("  suppressed as duplicate       : {}", tracker.stats().duplicates_suppressed);
    println!("  suppressed as malformed input : {}", tracker.suspicious_suppressions());

    // The exact re-delivery of the live fill reconciles away, and the pathological tail
    // is capped by MAX_OCCURRENCES_PER_CONTENT rather than minting an order per repeat.
    // Without that ceiling this storm would have produced ~50,000 orders.
    assert!(actionable <= wallet_tracker::dedup::MAX_OCCURRENCES_PER_CONTENT as usize,
        "a repeat storm must be capped, got {actionable} actionable");
    assert!(tracker.suspicious_suppressions() > 45_000,
        "the bulk of the storm must be reported as malformed input");
}
