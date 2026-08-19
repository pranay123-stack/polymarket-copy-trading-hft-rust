//! Copy-trade sizing.
//!
//! Sizing is the step that decides how much of somebody else's conviction we take on.
//! Every mode funnels through [`SizingEngine::size`], which applies the mode's raw
//! notional and then a fixed sequence of **caps that can only reduce it**:
//!
//! ```text
//! raw notional  ->  per-wallet max trade  ->  global max trade
//!               ->  headroom to the position cap
//!               ->  available liquidity (risk-adjusted mode only)
//!               ->  minimum-size floor (or refuse entirely)
//! ```
//!
//! Caps never *increase* size. That is the property the whole risk story rests on, and
//! it is asserted directly in the tests: no configuration of inputs can produce an order
//! larger than every applicable limit.

use domain::{Price, Qty, Side, SizingMode, TargetWallet, Usd};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Everything sizing needs to know about the world.
#[derive(Debug, Clone)]
pub struct SizingContext {
    /// Total portfolio equity, for percentage-of-portfolio sizing.
    pub equity: Usd,
    /// Absolute notional already held in this market.
    pub current_market_exposure: Usd,
    /// Global cap on any single position.
    pub max_position_usd: Usd,
    /// Global cap on any single trade.
    pub max_trade_usd: Usd,
    /// Global floor; below this we do not trade at all.
    pub min_trade_usd: Usd,
    /// Resting notional available at or better than our limit price.
    pub available_liquidity: Usd,
    /// Remaining daily loss budget. Zero or less means stop.
    pub remaining_daily_risk: Usd,
    /// The market's size increment.
    pub min_order_size: Decimal,
}

/// Why sizing produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SizingRefusal {
    /// Result fell below the minimum tradable notional.
    BelowMinimum { sized: String, minimum: String },
    /// The position cap is already reached in this market.
    NoPositionHeadroom,
    /// The daily loss budget is exhausted.
    DailyRiskExhausted,
    /// Not enough resting liquidity to trade meaningfully.
    NoLiquidity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SizedOrder {
    pub quantity: Qty,
    pub notional: Usd,
    /// Which rule produced the number, for the audit trail.
    pub mode: &'static str,
    /// Which cap, if any, bound the result — surfaced on the dashboard so a
    /// systematically undersized copy is visible rather than mysterious.
    pub binding_constraint: Option<&'static str>,
}

pub struct SizingEngine;

impl SizingEngine {
    /// Computes the copy size for one source trade.
    ///
    /// `limit_price` is the worst price we would accept, and is what the notional is
    /// converted to shares at — sizing in shares against an optimistic price would
    /// systematically overshoot the notional cap.
    pub fn size(
        wallet: &TargetWallet,
        source_notional: Usd,
        limit_price: Price,
        ctx: &SizingContext,
    ) -> Result<SizedOrder, SizingRefusal> {
        if ctx.remaining_daily_risk <= Usd::ZERO {
            return Err(SizingRefusal::DailyRiskExhausted);
        }

        let mode_name = wallet.sizing.name();
        let raw = Self::raw_notional(&wallet.sizing, source_notional, ctx);
        let mut notional = raw;
        let mut binding: Option<&'static str> = None;

        let cap = |limit: Usd, name: &'static str, n: &mut Usd, b: &mut Option<&'static str>| {
            if *n > limit {
                *n = limit;
                *b = Some(name);
            }
        };

        cap(wallet.max_trade_usd, "wallet_max_trade", &mut notional, &mut binding);
        cap(ctx.max_trade_usd, "global_max_trade", &mut notional, &mut binding);

        // Headroom to the per-market position cap.
        let headroom = (ctx.max_position_usd - ctx.current_market_exposure).max(Usd::ZERO);
        if headroom <= Usd::ZERO {
            return Err(SizingRefusal::NoPositionHeadroom);
        }
        cap(headroom, "position_headroom", &mut notional, &mut binding);

        // Wallet exposure cap.
        cap(wallet.max_exposure_usd, "wallet_max_exposure", &mut notional, &mut binding);

        // Never risk more than the remaining daily budget on one trade.
        cap(ctx.remaining_daily_risk, "daily_risk_budget", &mut notional, &mut binding);

        // Liquidity only constrains the risk-adjusted mode; the other modes leave the
        // liquidity decision to the risk engine so their sizing stays predictable.
        if let SizingMode::RiskAdjusted { liquidity_cap_pct, .. } = wallet.sizing {
            if ctx.available_liquidity <= Usd::ZERO {
                return Err(SizingRefusal::NoLiquidity);
            }
            let liq_cap = ctx.available_liquidity * liquidity_cap_pct;
            cap(liq_cap, "liquidity", &mut notional, &mut binding);
        }

        // Floors: the stricter of the wallet's and the global minimum.
        let floor = wallet.min_trade_usd.max(ctx.min_trade_usd);
        if notional < floor {
            return Err(SizingRefusal::BelowMinimum {
                sized: notional.to_string(),
                minimum: floor.to_string(),
            });
        }

        // Convert to shares at the *worst* acceptable price, then round down to the
        // venue's size increment. Rounding down keeps us inside every notional cap.
        let shares = notional.shares_at(limit_price).floor_to(ctx.min_order_size);
        if shares.is_zero() {
            return Err(SizingRefusal::BelowMinimum {
                sized: "0".into(),
                minimum: ctx.min_order_size.to_string(),
            });
        }
        let final_notional = shares.notional(limit_price);

        Ok(SizedOrder { quantity: shares, notional: final_notional, mode: mode_name, binding_constraint: binding })
    }

    fn raw_notional(mode: &SizingMode, source_notional: Usd, ctx: &SizingContext) -> Usd {
        match mode {
            SizingMode::FixedRatio { ratio } => source_notional * *ratio,
            SizingMode::FixedUsd { amount } => Usd::new(*amount),
            SizingMode::PortfolioPercent { pct } => ctx.equity * *pct,
            SizingMode::RiskAdjusted { base_ratio, .. } => {
                let base = source_notional * *base_ratio;
                // Scale down as the market position fills up: at half the cap, take
                // half the size. Convex de-risking rather than a cliff at the limit.
                let used = if ctx.max_position_usd.get() > Decimal::ZERO {
                    (ctx.current_market_exposure.get() / ctx.max_position_usd.get())
                        .clamp(Decimal::ZERO, Decimal::ONE)
                } else {
                    Decimal::ONE
                };
                base * (Decimal::ONE - used)
            }
        }
    }

    /// The worst price we will accept, given a slippage budget in bps.
    ///
    /// Buying: the reference price *plus* the budget. Selling: *minus*. The result is
    /// rounded to the market tick in the conservative direction and clamped into the
    /// tradable range.
    pub fn limit_price(reference: Price, side: Side, max_slippage_bps: u32, tick: Decimal) -> Price {
        let budget = Decimal::from(max_slippage_bps) / dec!(10000);
        let adjusted = match side {
            Side::Buy => reference.get() * (Decimal::ONE + budget),
            Side::Sell => reference.get() * (Decimal::ONE - budget),
        };
        Price::saturating(adjusted).round_to_tick(tick, side)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::Address;

    fn ctx() -> SizingContext {
        SizingContext {
            equity: Usd::new(dec!(10_000)),
            current_market_exposure: Usd::ZERO,
            max_position_usd: Usd::new(dec!(1000)),
            max_trade_usd: Usd::new(dec!(500)),
            min_trade_usd: Usd::new(dec!(5)),
            available_liquidity: Usd::new(dec!(10_000)),
            remaining_daily_risk: Usd::new(dec!(1000)),
            min_order_size: dec!(1),
        }
    }

    fn wallet(mode: SizingMode) -> TargetWallet {
        let mut w = TargetWallet::new(
            Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap(), "W");
        w.sizing = mode;
        w.max_trade_usd = Usd::new(dec!(500));
        w.max_exposure_usd = Usd::new(dec!(5000));
        w.min_trade_usd = Usd::new(dec!(5));
        w
    }

    fn px(d: Decimal) -> Price { Price::new(d).unwrap() }

    #[test]
    fn fixed_ratio_takes_the_configured_share() {
        // The brief's example: target buys $1000, ratio 0.25 -> we want $250.
        let w = wallet(SizingMode::FixedRatio { ratio: dec!(0.25) });
        let s = SizingEngine::size(&w, Usd::new(dec!(1000)), px(dec!(0.5)), &ctx()).unwrap();
        assert_eq!(s.notional.get(), dec!(250));
        assert_eq!(s.quantity.get(), dec!(500)); // 250 / 0.50
        assert_eq!(s.mode, "fixed_ratio");
        assert_eq!(s.binding_constraint, None);
    }

    #[test]
    fn fixed_usd_ignores_the_source_size() {
        let w = wallet(SizingMode::FixedUsd { amount: dec!(50) });
        let small = SizingEngine::size(&w, Usd::new(dec!(100)), px(dec!(0.5)), &ctx()).unwrap();
        let huge = SizingEngine::size(&w, Usd::new(dec!(1_000_000)), px(dec!(0.5)), &ctx()).unwrap();
        assert_eq!(small.notional.get(), dec!(50));
        assert_eq!(huge.notional.get(), dec!(50));
    }

    #[test]
    fn portfolio_percent_scales_with_equity() {
        let w = wallet(SizingMode::PortfolioPercent { pct: dec!(0.02) });
        let mut c = ctx();
        let a = SizingEngine::size(&w, Usd::new(dec!(1000)), px(dec!(0.5)), &c).unwrap();
        assert_eq!(a.notional.get(), dec!(200)); // 2% of 10k
        c.equity = Usd::new(dec!(50_000));
        let b = SizingEngine::size(&w, Usd::new(dec!(1000)), px(dec!(0.5)), &c).unwrap();
        assert_eq!(b.notional.get(), dec!(500), "capped by max_trade, not by equity");
        assert_eq!(b.binding_constraint, Some("wallet_max_trade"));
    }

    #[test]
    fn risk_adjusted_shrinks_as_the_position_fills() {
        let w = wallet(SizingMode::RiskAdjusted { base_ratio: dec!(0.5), liquidity_cap_pct: dec!(0.1) });
        let mut c = ctx();
        let empty = SizingEngine::size(&w, Usd::new(dec!(400)), px(dec!(0.5)), &c).unwrap();
        c.current_market_exposure = Usd::new(dec!(500)); // half the 1000 cap
        let half = SizingEngine::size(&w, Usd::new(dec!(400)), px(dec!(0.5)), &c).unwrap();
        assert_eq!(empty.notional.get(), dec!(200));
        assert_eq!(half.notional.get(), dec!(100), "half-full position -> half size");
        assert!(half.notional < empty.notional);
    }

    #[test]
    fn risk_adjusted_respects_available_liquidity() {
        let w = wallet(SizingMode::RiskAdjusted { base_ratio: dec!(1), liquidity_cap_pct: dec!(0.1) });
        let mut c = ctx();
        c.available_liquidity = Usd::new(dec!(300)); // 10% of it = 30
        let s = SizingEngine::size(&w, Usd::new(dec!(400)), px(dec!(0.5)), &c).unwrap();
        assert_eq!(s.notional.get(), dec!(30));
        assert_eq!(s.binding_constraint, Some("liquidity"));
    }

    #[test]
    fn risk_adjusted_refuses_an_empty_book() {
        let w = wallet(SizingMode::RiskAdjusted { base_ratio: dec!(1), liquidity_cap_pct: dec!(0.1) });
        let mut c = ctx();
        c.available_liquidity = Usd::ZERO;
        assert_eq!(SizingEngine::size(&w, Usd::new(dec!(400)), px(dec!(0.5)), &c), Err(SizingRefusal::NoLiquidity));
    }

    #[test]
    fn caps_can_only_reduce_never_increase() {
        // The core safety property, exercised across a wide input sweep.
        let c = ctx();
        for src in [10, 100, 1_000, 10_000, 1_000_000] {
            for ratio in [dec!(0.01), dec!(0.25), dec!(1), dec!(5)] {
                let w = wallet(SizingMode::FixedRatio { ratio });
                if let Ok(s) = SizingEngine::size(&w, Usd::new(Decimal::from(src)), px(dec!(0.5)), &c) {
                    assert!(s.notional <= w.max_trade_usd, "breached wallet max_trade");
                    assert!(s.notional <= c.max_trade_usd, "breached global max_trade");
                    assert!(s.notional <= c.max_position_usd, "breached position cap");
                    assert!(s.notional <= c.remaining_daily_risk, "breached daily risk budget");
                }
            }
        }
    }

    #[test]
    fn position_headroom_binds_and_then_refuses() {
        let w = wallet(SizingMode::FixedRatio { ratio: dec!(1) });
        let mut c = ctx();
        c.current_market_exposure = Usd::new(dec!(900)); // 100 headroom of 1000
        let s = SizingEngine::size(&w, Usd::new(dec!(400)), px(dec!(0.5)), &c).unwrap();
        assert_eq!(s.notional.get(), dec!(100));
        assert_eq!(s.binding_constraint, Some("position_headroom"));

        c.current_market_exposure = Usd::new(dec!(1000)); // full
        assert_eq!(SizingEngine::size(&w, Usd::new(dec!(400)), px(dec!(0.5)), &c),
            Err(SizingRefusal::NoPositionHeadroom));
    }

    #[test]
    fn exhausted_daily_risk_stops_trading_entirely() {
        let w = wallet(SizingMode::FixedRatio { ratio: dec!(0.25) });
        let mut c = ctx();
        c.remaining_daily_risk = Usd::ZERO;
        assert_eq!(SizingEngine::size(&w, Usd::new(dec!(1000)), px(dec!(0.5)), &c),
            Err(SizingRefusal::DailyRiskExhausted));
    }

    #[test]
    fn dust_results_are_refused_rather_than_rounded_up() {
        let w = wallet(SizingMode::FixedRatio { ratio: dec!(0.001) });
        // 0.1% of $100 = $0.10, below the $5 floor.
        assert!(matches!(
            SizingEngine::size(&w, Usd::new(dec!(100)), px(dec!(0.5)), &ctx()),
            Err(SizingRefusal::BelowMinimum { .. })
        ));
    }

    #[test]
    fn shares_round_down_to_the_size_increment() {
        let w = wallet(SizingMode::FixedUsd { amount: dec!(100) });
        let mut c = ctx();
        c.min_order_size = dec!(5);
        let s = SizingEngine::size(&w, Usd::new(dec!(1000)), px(dec!(0.33)), &c).unwrap();
        // 100 / 0.33 = 303.03 -> floor to a multiple of 5 = 300
        assert_eq!(s.quantity.get(), dec!(300));
        assert!(s.notional <= Usd::new(dec!(100)), "rounding must never breach the cap");
    }

    #[test]
    fn limit_price_widens_in_the_costly_direction() {
        let tick = dec!(0.001);
        let buy = SizingEngine::limit_price(px(dec!(0.61)), Side::Buy, 50, tick);
        let sell = SizingEngine::limit_price(px(dec!(0.61)), Side::Sell, 50, tick);
        // 50bps = 0.5%: buy up to 0.61305 -> tick-floored to 0.613
        assert_eq!(buy.get(), dec!(0.613));
        assert!(buy.get() > dec!(0.61), "a buy limit must sit above the reference");
        assert_eq!(sell.get(), dec!(0.607));
        assert!(sell.get() < dec!(0.61), "a sell limit must sit below the reference");
    }

    #[test]
    fn zero_slippage_budget_pins_the_reference_price() {
        let p = SizingEngine::limit_price(px(dec!(0.61)), Side::Buy, 0, dec!(0.01));
        assert_eq!(p.get(), dec!(0.61));
    }

    #[test]
    fn limit_price_stays_inside_the_tradable_range() {
        // A wide budget near the boundary must not produce an invalid price.
        let p = SizingEngine::limit_price(px(dec!(0.99)), Side::Buy, 5000, dec!(0.01));
        assert!(p.get() < Decimal::ONE && p.get() > Decimal::ZERO);
        let q = SizingEngine::limit_price(px(dec!(0.01)), Side::Sell, 5000, dec!(0.01));
        assert!(q.get() > Decimal::ZERO && q.get() < Decimal::ONE);
    }
}
