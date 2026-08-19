//! Benchmarks for the hot path.
//!
//! These measure the components that run on **every** frame of a ~33 msg/s firehose,
//! plus the per-signal work. Run with `cargo bench -p app`.
//!
//! All numbers are produced by criterion from real executions; none are hard-coded.

use std::time::Duration;

use chrono::Utc;
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use domain::{
    Address, CorrelationId, Level, MarketId, OrderBook, OrderRequest, OrderType, Price, Qty, Side,
    SizingMode, SourceEventId, SourceTrade, TargetWallet, TimeInForce, TokenId, TradeSource, TxHash,
    Usd,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const REAL_FRAME: &str = r#"{"connection_id":"gXseIO-NQWeIKEhJwA==","payload":{"asset":"72551024098258542594534683942523606143014690620243023298497729957846870197074","bio":"","conditionId":"0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52","eventSlug":"e","icon":"","name":"PPMT","outcome":"No","outcomeIndex":1,"price":0.26,"profileImage":"","proxyWallet":"0x510F4963b66B1B18505faaB74b0bB943D1dDa43C","pseudonym":"R","side":"BUY","size":2.7027,"slug":"s","timestamp":1787102287,"title":"T","transactionHash":"0xb6acf6859bc84216f4b3e2567fb392a2eae19d275340ad96ea17218ccfec27b7"},"timestamp":1787102287053,"topic":"activity","type":"trades"}"#;

fn token() -> TokenId {
    TokenId::new("72551024098258542594534683942523606143014690620243023298497729957846870197074").unwrap()
}
fn market() -> MarketId {
    MarketId::new("0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52").unwrap()
}

fn book(levels: usize) -> OrderBook {
    let mut bids = Vec::with_capacity(levels);
    let mut asks = Vec::with_capacity(levels);
    // Step scaled so even a 500-level book stays strictly inside (0, 1): a price of
    // exactly 0 is correctly rejected by `Price::new`, and this is a fixture, not a
    // reason to loosen that invariant.
    let step = Decimal::new(4, 1) / Decimal::from(levels.max(1) as i64 + 1);
    for i in 1..=levels as i64 {
        let off = step * Decimal::from(i);
        bids.push(Level {
            price: Price::new(dec!(0.50) - off).unwrap(),
            size: Qty::new(Decimal::from(100 * i)).unwrap(),
        });
        asks.push(Level {
            price: Price::new(dec!(0.51) + off).unwrap(),
            size: Qty::new(Decimal::from(100 * i)).unwrap(),
        });
    }
    OrderBook {
        market_id: market(), token_id: token(), bids, asks,
        tick_size: dec!(0.001), min_order_size: dec!(5),
        timestamp: Utc::now(), seq: 1,
    }
}

fn source_trade(wallet: Address) -> SourceTrade {
    SourceTrade {
        event_id: SourceEventId::from_digest("bench"),
        correlation_id: CorrelationId::new(),
        trader: wallet,
        market_id: market(),
        token_id: token(),
        outcome: "Yes".into(),
        side: Side::Buy,
        price: Price::new(dec!(0.50)).unwrap(),
        quantity: Qty::new(dec!(2000)).unwrap(),
        tx_hash: TxHash::new(format!("0x{:064x}", 1)).unwrap(),
        occurrence: 0,
        source_ts: Utc::now(),
        detected_ts: Utc::now(),
        source: TradeSource::RtdsWebsocket,
        market_title: "T".into(),
        market_slug: "t".into(),
    }
}

fn wallet(n: u8) -> TargetWallet {
    let mut w = TargetWallet::new(Address::new(format!("0x{:040x}", n)).unwrap(), "W");
    w.sizing = SizingMode::FixedRatio { ratio: dec!(0.25) };
    w.max_trade_usd = Usd::new(dec!(500));
    w.min_source_notional_usd = Usd::ZERO;
    w
}

/// Parsing every frame on the firehose.
fn bench_parse(c: &mut Criterion) {
    let now = Utc::now();
    c.bench_function("rtds_frame_parse", |b| {
        b.iter(|| market_data::parse_rtds_frame(black_box(REAL_FRAME), black_box(now)).unwrap())
    });
}

/// The hot-path question: is this one of our wallets?
fn bench_wallet_match(c: &mut Criterion) {
    let mut g = c.benchmark_group("wallet_match");
    for n in [1usize, 10, 100] {
        let tracker = wallet_tracker::WalletTracker::new(
            (0..n).map(|i| wallet(i as u8)).collect());
        // The common case: a frame from somebody we do not track.
        let miss = Address::new(format!("0x{:040x}", 200)).unwrap();
        g.bench_function(format!("miss_{n}_wallets"), |b| {
            b.iter(|| black_box(tracker.is_tracked(black_box(&miss))))
        });
    }
    g.finish();
}

/// Dedup identity derivation, run for every matched fill.
fn bench_dedup(c: &mut Criterion) {
    use wallet_tracker::{ContentKey, DedupIndex};
    let key = ContentKey::new(
        &TxHash::new(format!("0x{:064x}", 1)).unwrap(),
        &Address::new(format!("0x{:040x}", 1)).unwrap(),
        &token(), Side::Buy,
        Price::new(dec!(0.98)).unwrap(), Qty::new(dec!(5)).unwrap());

    c.bench_function("dedup_event_id_hash", |b| {
        b.iter(|| black_box(key.event_id(black_box(0))))
    });

    c.bench_function("dedup_observe_live", |b| {
        b.iter_batched(
            DedupIndex::default,
            |mut idx| black_box(idx.observe_live(key.clone(), Utc::now())),
            BatchSize::SmallInput,
        )
    });
}

/// Sizing and limit-price derivation.
fn bench_sizing(c: &mut Criterion) {
    use strategy::{SizingContext, SizingEngine};
    let w = wallet(1);
    let ctx = SizingContext {
        equity: Usd::new(dec!(10_000)),
        current_market_exposure: Usd::ZERO,
        max_position_usd: Usd::new(dec!(1000)),
        max_trade_usd: Usd::new(dec!(500)),
        min_trade_usd: Usd::new(dec!(5)),
        available_liquidity: Usd::new(dec!(10_000)),
        remaining_daily_risk: Usd::new(dec!(1000)),
        min_order_size: dec!(5),
    };
    let px = Price::new(dec!(0.51)).unwrap();
    c.bench_function("copy_sizing", |b| {
        b.iter(|| SizingEngine::size(black_box(&w), black_box(Usd::new(dec!(1000))), black_box(px), black_box(&ctx)))
    });
    c.bench_function("limit_price_from_slippage", |b| {
        b.iter(|| SizingEngine::limit_price(black_box(px), Side::Buy, 50, dec!(0.001)))
    });
}

/// Full signal generation, including the book walk.
fn bench_signal(c: &mut Criterion) {
    use strategy::{CopyTrader, SizingContext, StrategyConfig};
    let trader = CopyTrader::new(StrategyConfig::default());
    let w = wallet(1);
    let t = source_trade(w.address.clone());
    let ctx = SizingContext {
        equity: Usd::new(dec!(10_000)),
        current_market_exposure: Usd::ZERO,
        max_position_usd: Usd::new(dec!(1000)),
        max_trade_usd: Usd::new(dec!(500)),
        min_trade_usd: Usd::new(dec!(5)),
        available_liquidity: Usd::ZERO,
        remaining_daily_risk: Usd::new(dec!(1000)),
        min_order_size: dec!(5),
    };
    let mut g = c.benchmark_group("signal_generation");
    for depth in [5usize, 50] {
        let bk = book(depth);
        g.bench_function(format!("book_depth_{depth}"), |b| {
            b.iter(|| trader.on_source_trade(black_box(&t), black_box(&w), Some(black_box(&bk)),
                ctx.clone(), dec!(0.001)))
        });
    }
    g.finish();
}

/// The full pre-trade risk check.
fn bench_risk(c: &mut Criterion) {
    use risk::{KillSwitch, RiskEngine, RiskLimits, RiskSnapshot, SystemStatus};
    let engine = RiskEngine::new(RiskLimits::default(), domain::AppMode::Paper, false);
    let ks = KillSwitch::new();
    let bk = book(20);
    let order = OrderRequest {
        order_id: domain::OrderId::new(),
        correlation_id: CorrelationId::new(),
        signal_id: None,
        market_id: market(),
        token_id: token(),
        side: Side::Buy,
        order_type: OrderType::Market,
        time_in_force: TimeInForce::Ioc,
        quantity: Qty::new(dec!(100)).unwrap(),
        limit_price: Price::new(dec!(0.52)).unwrap(),
        reference_price: Price::new(dec!(0.51)).unwrap(),
        tick_size: dec!(0.001),
        created_at: Utc::now(),
    };
    let snap = RiskSnapshot {
        daily_pnl: Usd::ZERO, gross_exposure: Usd::ZERO, market_exposure: Usd::ZERO,
        wallet_exposure: Usd::ZERO, token_exposure: Usd::ZERO, open_orders: 0,
        equity: Usd::new(dec!(10_000)),
    };
    let status = SystemStatus::default();
    let now = Utc::now();
    c.bench_function("risk_check_full", |b| {
        b.iter(|| engine.check(black_box(&order), &snap, &status, &ks, Some(&bk),
            true, "W", None, None, now))
    });
}

/// Book normalisation and walking.
fn bench_book(c: &mut Criterion) {
    let mut g = c.benchmark_group("orderbook");
    for depth in [5usize, 50, 500] {
        let bk = book(depth);
        g.bench_function(format!("sweep_vwap_depth_{depth}"), |b| {
            b.iter(|| bk.sweep_vwap(Side::Buy, black_box(Qty::new(dec!(5000)).unwrap()),
                Price::new(dec!(0.99)).unwrap()))
        });
    }
    let raw = serde_json::to_string(&serde_json::json!({
        "market": market().as_str(), "asset_id": token().as_str(),
        "timestamp": "1787102287053",
        "bids": (1..=50).map(|i| serde_json::json!({"price": format!("0.{:03}", i), "size": "100"})).collect::<Vec<_>>(),
        "asks": (1..=50).map(|i| serde_json::json!({"price": format!("0.{:03}", 500+i), "size": "100"})).collect::<Vec<_>>(),
        "tick_size": "0.001", "min_order_size": "5",
    })).unwrap();
    let now = Utc::now();
    g.bench_function("parse_and_normalise_book_100_levels", |b| {
        b.iter(|| market_data::parse_book(black_box(&raw), 1, now).unwrap())
    });
    g.finish();
}

/// Position and PnL accounting.
fn bench_portfolio(c: &mut Criterion) {
    use portfolio::{make_fill, Portfolio};
    let f = make_fill(
        domain::OrderId::new(), CorrelationId::new(), market(), token(), Side::Buy,
        Qty::new(dec!(100)).unwrap(), Price::new(dec!(0.5)).unwrap(), Usd::ZERO, Utc::now());
    c.bench_function("portfolio_apply_fill", |b| {
        b.iter_batched(
            || Portfolio::new(Usd::new(dec!(1_000_000))),
            |p| { p.apply_fill(black_box(&f), "Yes"); },
            BatchSize::SmallInput,
        )
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().measurement_time(Duration::from_secs(4)).warm_up_time(Duration::from_secs(1));
    targets = bench_parse, bench_wallet_match, bench_dedup, bench_sizing,
              bench_signal, bench_risk, bench_book, bench_portfolio
}
criterion_main!(benches);
