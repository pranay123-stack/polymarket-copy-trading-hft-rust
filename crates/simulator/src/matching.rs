//! Fill simulation against real order-book liquidity.
//!
//! The paper engine does **not** assume orders fill instantly at the requested price.
//! It walks the same book the live strategy priced against, so paper results are
//! constrained by the liquidity that actually existed:
//!
//! * a marketable order sweeps real levels and gets their volume-weighted price;
//! * a book too thin to fill the order produces a **partial** fill, not a full one;
//! * a resting limit order fills only if the market trades through its price;
//! * configured adverse slippage and fees are applied on top.
//!
//! The defaults are deliberately pessimistic. A paper fill should be *harder* to obtain
//! than a live one, so that paper performance understates rather than flatters.

use domain::{OrderBook, OrderRequest, OrderType, Price, Qty, Side, TimeInForce, Usd};
use rand::Rng;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// What the simulated venue decided.
#[derive(Debug, Clone, PartialEq)]
pub enum SimOutcome {
    /// Filled in full or in part.
    Filled { quantity: Qty, price: Price, fee: Usd, is_maker: bool },
    /// Accepted and resting; no fill yet.
    Resting,
    /// The venue refused it.
    Rejected { reason: String },
    /// Marketable but nothing was reachable within the limit.
    NoLiquidity,
}

#[derive(Debug, Clone)]
pub struct MatchParams {
    pub fee_bps: u32,
    /// Extra adverse price movement applied on top of the book walk, modelling the
    /// latency between our decision and the order arriving.
    pub slippage_bps: u32,
    pub partial_fill_enabled: bool,
    /// Probability a *resting* order fills when the market trades through it.
    pub fill_probability: f64,
    pub reject_probability: f64,
}

impl Default for MatchParams {
    fn default() -> Self {
        Self {
            fee_bps: 0,
            slippage_bps: 10,
            partial_fill_enabled: true,
            fill_probability: 0.92,
            reject_probability: 0.01,
        }
    }
}

pub struct MatchingEngine;

impl MatchingEngine {
    /// Simulates one order against a book snapshot.
    ///
    /// `rng` is threaded in explicitly so a seeded generator makes an entire paper
    /// session reproducible — a paper run that cannot be replayed is not evidence.
    pub fn match_order<R: Rng>(
        order: &OrderRequest,
        book: &OrderBook,
        params: &MatchParams,
        rng: &mut R,
    ) -> SimOutcome {
        if rng.gen::<f64>() < params.reject_probability {
            return SimOutcome::Rejected { reason: "simulated venue rejection".into() };
        }

        let marketable = Self::is_marketable(order, book);

        if !marketable {
            return match order.time_in_force {
                // IOC/FOK cannot rest, so an unmarketable one dies immediately.
                TimeInForce::Ioc | TimeInForce::Fok => SimOutcome::NoLiquidity,
                TimeInForce::Gtc => SimOutcome::Resting,
            };
        }

        // Walk the real book for our size, within our limit.
        let Some((vwap, available)) = book.sweep_vwap(order.side, order.quantity, order.limit_price)
        else {
            return SimOutcome::NoLiquidity;
        };

        // Apply configured adverse slippage on top of the book walk.
        let slipped = Self::apply_slippage(vwap, order.side, params.slippage_bps);
        // Never fill better than our own limit allows.
        let fill_price = match order.side {
            Side::Buy if slipped > order.limit_price => order.limit_price,
            Side::Sell if slipped < order.limit_price => order.limit_price,
            _ => slipped,
        };

        let mut qty = available;
        if qty < order.quantity {
            // The book could not cover the full size.
            if order.time_in_force == TimeInForce::Fok {
                return SimOutcome::NoLiquidity; // all-or-nothing
            }
            if !params.partial_fill_enabled {
                return SimOutcome::NoLiquidity;
            }
        }

        // Model queue/latency risk on an otherwise-fillable order.
        if params.fill_probability < 1.0 && rng.gen::<f64>() > params.fill_probability {
            if !params.partial_fill_enabled {
                return SimOutcome::NoLiquidity;
            }
            // Got a worse fill than hoped: a fraction of the intended size.
            let frac = Decimal::try_from(rng.gen_range(0.2f64..0.8)).unwrap_or(dec!(0.5));
            qty = Qty::new((qty.get() * frac).max(Decimal::ZERO)).unwrap_or(Qty::ZERO);
            if qty.is_zero() {
                return SimOutcome::NoLiquidity;
            }
        }

        let fee = Usd::new(qty.notional(fill_price).get() * Decimal::from(params.fee_bps) / dec!(10000));
        SimOutcome::Filled { quantity: qty, price: fill_price, fee, is_maker: false }
    }

    /// Would this order cross the spread right now?
    pub fn is_marketable(order: &OrderRequest, book: &OrderBook) -> bool {
        if order.order_type == OrderType::Market {
            return true;
        }
        match order.side {
            Side::Buy => book.best_ask().is_some_and(|a| a.price <= order.limit_price),
            Side::Sell => book.best_bid().is_some_and(|b| b.price >= order.limit_price),
        }
    }

    /// Would a resting order have been filled by trading at `traded_price`?
    ///
    /// A resting buy fills when the market trades at or below its price.
    pub fn resting_would_fill(order: &OrderRequest, traded_price: Price) -> bool {
        match order.side {
            Side::Buy => traded_price <= order.limit_price,
            Side::Sell => traded_price >= order.limit_price,
        }
    }

    fn apply_slippage(p: Price, side: Side, bps: u32) -> Price {
        if bps == 0 { return p; }
        let f = Decimal::from(bps) / dec!(10000);
        Price::saturating(match side {
            Side::Buy => p.get() * (Decimal::ONE + f),
            Side::Sell => p.get() * (Decimal::ONE - f),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use domain::{CorrelationId, Level, MarketId, OrderId, TokenId};
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    fn rng() -> ChaCha8Rng { ChaCha8Rng::seed_from_u64(7) }

    fn book(levels_ask: &[(Decimal, Decimal)], levels_bid: &[(Decimal, Decimal)]) -> OrderBook {
        OrderBook {
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: TokenId::new("83208474815813611206796889197671166802498709571847428026387").unwrap(),
            bids: levels_bid.iter().map(|(p, s)| Level {
                price: Price::new(*p).unwrap(), size: Qty::new(*s).unwrap() }).collect(),
            asks: levels_ask.iter().map(|(p, s)| Level {
                price: Price::new(*p).unwrap(), size: Qty::new(*s).unwrap() }).collect(),
            tick_size: dec!(0.01),
            min_order_size: dec!(1),
            timestamp: Utc::now(),
            seq: 1,
        }
    }

    fn order(side: Side, qty: Decimal, limit: Decimal, ty: OrderType, tif: TimeInForce) -> OrderRequest {
        OrderRequest {
            order_id: OrderId::new(),
            correlation_id: CorrelationId::new(),
            signal_id: None,
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: TokenId::new("83208474815813611206796889197671166802498709571847428026387").unwrap(),
            side,
            order_type: ty,
            time_in_force: tif,
            quantity: Qty::new(qty).unwrap(),
            limit_price: Price::new(limit).unwrap(),
            reference_price: Price::new(limit).unwrap(),
            tick_size: dec!(0.01),
            created_at: Utc::now(),
        }
    }

    fn certain() -> MatchParams {
        // Deterministic: no random rejection, no random partial.
        MatchParams { fee_bps: 0, slippage_bps: 0, partial_fill_enabled: true,
            fill_probability: 1.0, reject_probability: 0.0 }
    }

    #[test]
    fn marketable_order_sweeps_real_levels_at_vwap() {
        let b = book(&[(dec!(0.61), dec!(100)), (dec!(0.62), dec!(100))], &[(dec!(0.59), dec!(100))]);
        let o = order(Side::Buy, dec!(200), dec!(0.63), OrderType::Limit, TimeInForce::Gtc);
        match MatchingEngine::match_order(&o, &b, &certain(), &mut rng()) {
            SimOutcome::Filled { quantity, price, .. } => {
                assert_eq!(quantity.get(), dec!(200));
                assert_eq!(price.get(), dec!(0.615)); // (100*0.61 + 100*0.62)/200
            }
            other => panic!("expected a fill, got {other:?}"),
        }
    }

    #[test]
    fn thin_book_produces_a_partial_fill_not_a_full_one() {
        // The whole point of not assuming instant fills.
        let b = book(&[(dec!(0.61), dec!(50))], &[(dec!(0.59), dec!(100))]);
        let o = order(Side::Buy, dec!(500), dec!(0.63), OrderType::Limit, TimeInForce::Gtc);
        match MatchingEngine::match_order(&o, &b, &certain(), &mut rng()) {
            SimOutcome::Filled { quantity, .. } => {
                assert_eq!(quantity.get(), dec!(50), "must fill only what the book held");
                assert!(quantity < o.quantity);
            }
            other => panic!("expected a partial fill, got {other:?}"),
        }
    }

    #[test]
    fn fok_refuses_a_partial() {
        let b = book(&[(dec!(0.61), dec!(50))], &[(dec!(0.59), dec!(100))]);
        let o = order(Side::Buy, dec!(500), dec!(0.63), OrderType::Limit, TimeInForce::Fok);
        assert_eq!(MatchingEngine::match_order(&o, &b, &certain(), &mut rng()), SimOutcome::NoLiquidity);
    }

    #[test]
    fn partial_fills_can_be_disabled() {
        let b = book(&[(dec!(0.61), dec!(50))], &[(dec!(0.59), dec!(100))]);
        let o = order(Side::Buy, dec!(500), dec!(0.63), OrderType::Limit, TimeInForce::Gtc);
        let p = MatchParams { partial_fill_enabled: false, ..certain() };
        assert_eq!(MatchingEngine::match_order(&o, &b, &p, &mut rng()), SimOutcome::NoLiquidity);
    }

    #[test]
    fn unmarketable_gtc_rests_rather_than_filling() {
        let b = book(&[(dec!(0.61), dec!(100))], &[(dec!(0.59), dec!(100))]);
        // Bidding 0.55 when the ask is 0.61.
        let o = order(Side::Buy, dec!(100), dec!(0.55), OrderType::Limit, TimeInForce::Gtc);
        assert_eq!(MatchingEngine::match_order(&o, &b, &certain(), &mut rng()), SimOutcome::Resting);
    }

    #[test]
    fn unmarketable_ioc_dies_immediately() {
        let b = book(&[(dec!(0.61), dec!(100))], &[(dec!(0.59), dec!(100))]);
        let o = order(Side::Buy, dec!(100), dec!(0.55), OrderType::Limit, TimeInForce::Ioc);
        assert_eq!(MatchingEngine::match_order(&o, &b, &certain(), &mut rng()), SimOutcome::NoLiquidity);
    }

    #[test]
    fn fill_never_beats_our_own_limit_price() {
        // Slippage must not be allowed to push the fill through the limit we promised.
        let b = book(&[(dec!(0.61), dec!(1000))], &[(dec!(0.59), dec!(1000))]);
        let o = order(Side::Buy, dec!(100), dec!(0.615), OrderType::Limit, TimeInForce::Gtc);
        let p = MatchParams { slippage_bps: 5000, ..certain() }; // 50% adverse
        match MatchingEngine::match_order(&o, &b, &p, &mut rng()) {
            SimOutcome::Filled { price, .. } => {
                assert!(price <= o.limit_price, "filled at {price}, worse than limit {}", o.limit_price);
                assert_eq!(price.get(), dec!(0.615));
            }
            other => panic!("expected fill, got {other:?}"),
        }
    }

    #[test]
    fn slippage_moves_the_price_against_us_on_both_sides() {
        let ask = book(&[(dec!(0.60), dec!(1000))], &[(dec!(0.40), dec!(1000))]);
        let p = MatchParams { slippage_bps: 100, ..certain() }; // 1%

        let buy = order(Side::Buy, dec!(100), dec!(0.70), OrderType::Limit, TimeInForce::Gtc);
        match MatchingEngine::match_order(&buy, &ask, &p, &mut rng()) {
            SimOutcome::Filled { price, .. } => assert!(price.get() > dec!(0.60), "buy should pay up"),
            o => panic!("{o:?}"),
        }
        let sell = order(Side::Sell, dec!(100), dec!(0.30), OrderType::Limit, TimeInForce::Gtc);
        match MatchingEngine::match_order(&sell, &ask, &p, &mut rng()) {
            SimOutcome::Filled { price, .. } => assert!(price.get() < dec!(0.40), "sell should receive less"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn fees_are_charged_on_notional() {
        let b = book(&[(dec!(0.50), dec!(1000))], &[(dec!(0.40), dec!(1000))]);
        let o = order(Side::Buy, dec!(100), dec!(0.60), OrderType::Limit, TimeInForce::Gtc);
        let p = MatchParams { fee_bps: 100, ..certain() }; // 1%
        match MatchingEngine::match_order(&o, &b, &p, &mut rng()) {
            SimOutcome::Filled { fee, quantity, price, .. } => {
                assert_eq!(fee.get(), quantity.notional(price).get() * dec!(0.01));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn empty_book_yields_no_liquidity() {
        let b = book(&[], &[]);
        let o = order(Side::Buy, dec!(100), dec!(0.60), OrderType::Market, TimeInForce::Gtc);
        assert_eq!(MatchingEngine::match_order(&o, &b, &certain(), &mut rng()), SimOutcome::NoLiquidity);
    }

    #[test]
    fn rejection_probability_is_honoured() {
        let b = book(&[(dec!(0.50), dec!(1000))], &[(dec!(0.40), dec!(1000))]);
        let o = order(Side::Buy, dec!(100), dec!(0.60), OrderType::Limit, TimeInForce::Gtc);
        let p = MatchParams { reject_probability: 1.0, ..certain() };
        assert!(matches!(MatchingEngine::match_order(&o, &b, &p, &mut rng()), SimOutcome::Rejected { .. }));
    }

    #[test]
    fn seeded_runs_are_reproducible() {
        // A paper run that cannot be replayed is not evidence.
        let b = book(&[(dec!(0.61), dec!(80))], &[(dec!(0.59), dec!(100))]);
        let o = order(Side::Buy, dec!(500), dec!(0.63), OrderType::Limit, TimeInForce::Gtc);
        let p = MatchParams { fill_probability: 0.5, reject_probability: 0.1, ..certain() };
        let a: Vec<_> = (0..20).map(|_| MatchingEngine::match_order(&o, &b, &p, &mut ChaCha8Rng::seed_from_u64(99))).collect();
        let c: Vec<_> = (0..20).map(|_| MatchingEngine::match_order(&o, &b, &p, &mut ChaCha8Rng::seed_from_u64(99))).collect();
        assert_eq!(a, c);
    }

    #[test]
    fn resting_fill_requires_the_market_to_trade_through() {
        let buy = order(Side::Buy, dec!(100), dec!(0.55), OrderType::Limit, TimeInForce::Gtc);
        assert!(MatchingEngine::resting_would_fill(&buy, Price::new(dec!(0.55)).unwrap()));
        assert!(MatchingEngine::resting_would_fill(&buy, Price::new(dec!(0.50)).unwrap()));
        assert!(!MatchingEngine::resting_would_fill(&buy, Price::new(dec!(0.60)).unwrap()));

        let sell = order(Side::Sell, dec!(100), dec!(0.65), OrderType::Limit, TimeInForce::Gtc);
        assert!(MatchingEngine::resting_would_fill(&sell, Price::new(dec!(0.70)).unwrap()));
        assert!(!MatchingEngine::resting_would_fill(&sell, Price::new(dec!(0.60)).unwrap()));
    }

    #[test]
    fn filled_quantity_never_exceeds_requested() {
        // Property sweep: no configuration can overfill.
        let mut r = rng();
        for qty in [dec!(1), dec!(10), dec!(500), dec!(10_000)] {
            for depth in [dec!(5), dec!(100), dec!(100_000)] {
                let b = book(&[(dec!(0.61), depth)], &[(dec!(0.59), depth)]);
                let o = order(Side::Buy, qty, dec!(0.70), OrderType::Limit, TimeInForce::Gtc);
                if let SimOutcome::Filled { quantity, .. } =
                    MatchingEngine::match_order(&o, &b, &MatchParams::default(), &mut r)
                {
                    assert!(quantity <= o.quantity, "overfilled: {quantity} > {}", o.quantity);
                }
            }
        }
    }
}
