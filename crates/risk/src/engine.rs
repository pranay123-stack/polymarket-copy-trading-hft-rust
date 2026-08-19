//! Pre-trade risk engine.
//!
//! **Every order passes through [`RiskEngine::check`] before it can be submitted**, and
//! the order state machine enforces that structurally: an order cannot reach `Submitted`
//! without first reaching `Validated`, and only this engine produces that transition.
//!
//! Checks run in a fixed order, cheapest and most categorical first, so an order halted
//! by the kill switch never consumes work on liquidity maths. The order also matters for
//! diagnosis: the returned rejection is the *first* limit breached, which is the one an
//! operator should act on.

use chrono::{DateTime, Datelike, Utc};
use domain::{
    AppMode, Bps, MarketId, OrderBook, OrderRequest, RiskRejection, SourceEventId, Usd,
};
use rust_decimal::Decimal;

use crate::kill_switch::KillSwitch;
use crate::limits::RiskLimits;

/// The portfolio facts the engine needs. Supplied by the caller so the engine stays
/// synchronous, pure and trivially testable.
#[derive(Debug, Clone)]
pub struct RiskSnapshot {
    pub daily_pnl: Usd,
    pub gross_exposure: Usd,
    pub market_exposure: Usd,
    pub wallet_exposure: Usd,
    /// Signed position notional in the specific token, for headroom maths.
    pub token_exposure: Usd,
    pub open_orders: u32,
    pub equity: Usd,
}

/// Health inputs that gate trading.
#[derive(Debug, Clone, Copy)]
pub struct SystemStatus {
    pub market_data_healthy: bool,
    pub source_feed_healthy: bool,
    pub execution_ready: bool,
    pub database_healthy: bool,
}

impl Default for SystemStatus {
    fn default() -> Self {
        Self {
            market_data_healthy: true,
            source_feed_healthy: true,
            execution_ready: true,
            database_healthy: true,
        }
    }
}

impl SystemStatus {
    fn first_problem(&self) -> Option<&'static str> {
        if !self.execution_ready { return Some("execution adapter not ready"); }
        if !self.source_feed_healthy { return Some("source trade feed disconnected"); }
        if !self.market_data_healthy { return Some("market data feed disconnected"); }
        // A database outage does not stop trading — it stops *durable auditing*, which
        // is a degraded but survivable state. It is reported, not blocking.
        None
    }
}

/// A verdict plus the evidence behind it.
#[derive(Debug, Clone, PartialEq)]
pub enum RiskVerdict {
    Approved,
    Rejected(Box<RiskRejection>),
}

impl RiskVerdict {
    pub fn is_approved(&self) -> bool { matches!(self, Self::Approved) }
    pub fn rejection(&self) -> Option<&RiskRejection> {
        match self { Self::Rejected(r) => Some(r), Self::Approved => None }
    }
}

pub struct RiskEngine {
    limits: RiskLimits,
    mode: AppMode,
    live_armed: bool,
}

impl RiskEngine {
    pub fn new(limits: RiskLimits, mode: AppMode, live_armed: bool) -> Self {
        Self { limits, mode, live_armed }
    }

    pub fn limits(&self) -> &RiskLimits { &self.limits }
    pub fn set_limits(&mut self, l: RiskLimits) { self.limits = l; }

    /// Runs every pre-trade check.
    ///
    /// `book` is optional: without it the liquidity and slippage checks cannot run, and
    /// rather than skipping them the engine refuses on staleness grounds if market data
    /// is required. Silently passing an order we could not price would defeat the point.
    #[allow(clippy::too_many_arguments)]
    pub fn check(
        &self,
        order: &OrderRequest,
        snap: &RiskSnapshot,
        status: &SystemStatus,
        kill: &KillSwitch,
        book: Option<&OrderBook>,
        wallet_enabled: bool,
        wallet_label: &str,
        market_tradable: Option<(&MarketId, bool, &str)>,
        already_processed: Option<&SourceEventId>,
        now: DateTime<Utc>,
    ) -> RiskVerdict {
        use RiskRejection as R;

        // 1. Kill switch — categorical, checked before anything else.
        if kill.is_engaged() {
            return Self::reject(R::KillSwitchActive {
                reason: kill.reason().unwrap_or_else(|| "engaged".into()),
            });
        }

        // 2. Execution mode. In LIVE, both switches must be set; this is the last line
        //    of defence behind the config-level interlock.
        if self.mode.is_live() && !self.live_armed {
            return Self::reject(R::LiveExecutionNotArmed);
        }

        // 3. System health.
        if let Some(detail) = status.first_problem() {
            return Self::reject(R::SystemUnhealthy { detail: detail.into() });
        }

        // 4. Idempotency. A duplicated source event must never reach the venue, even if
        //    the tracker's dedup were somehow bypassed.
        if let Some(ev) = already_processed {
            return Self::reject(R::DuplicateOrder { source_event: ev.clone() });
        }

        // 5. Wallet enabled.
        if !wallet_enabled {
            return Self::reject(R::WalletDisabled { wallet: wallet_label.to_string() });
        }

        // 6. Market tradable.
        if let Some((mid, tradable, why)) = market_tradable {
            if !tradable {
                return Self::reject(R::MarketNotTradable { market: mid.clone(), reason: why.into() });
            }
        }

        // 7. Daily loss. Checked before sizing limits so a system already over its loss
        //    budget stops immediately rather than reporting a size complaint.
        if snap.daily_pnl < -self.limits.max_daily_loss_usd {
            return Self::reject(R::DailyLossLimitReached {
                daily_pnl: snap.daily_pnl,
                limit: self.limits.max_daily_loss_usd,
            });
        }

        // 8. Open order slots.
        if snap.open_orders >= self.limits.max_open_orders {
            return Self::reject(R::MaxOpenOrdersReached {
                open: snap.open_orders,
                limit: self.limits.max_open_orders,
            });
        }

        let notional = order.notional();

        // 9. Trade size bounds.
        if notional < self.limits.min_trade_usd {
            return Self::reject(R::BelowMinimumOrderSize {
                requested: notional, minimum: self.limits.min_trade_usd });
        }
        if notional > self.limits.max_trade_usd {
            return Self::reject(R::MaxTradeSizeExceeded {
                requested: notional, limit: self.limits.max_trade_usd });
        }
        // Live mode carries an extra, tighter cap.
        if self.mode.is_live() && notional > self.limits.max_live_order_usd {
            return Self::reject(R::MaxTradeSizeExceeded {
                requested: notional, limit: self.limits.max_live_order_usd });
        }

        // 10. Projected exposures. Projections assume a full fill — the conservative
        //     assumption, since a partial fill can only land under the limit.
        let projected_token = snap.token_exposure + notional;
        if projected_token > self.limits.max_position_usd {
            return Self::reject(R::MaxPositionExceeded {
                market: order.market_id.clone(),
                projected: projected_token,
                limit: self.limits.max_position_usd,
            });
        }
        let projected_market = snap.market_exposure + notional;
        if projected_market > self.limits.max_market_exposure_usd {
            return Self::reject(R::MaxMarketExposureExceeded {
                market: order.market_id.clone(),
                projected: projected_market,
                limit: self.limits.max_market_exposure_usd,
            });
        }
        let projected_gross = snap.gross_exposure + notional;
        if projected_gross > self.limits.max_portfolio_exposure_usd {
            return Self::reject(R::MaxPortfolioExposureExceeded {
                projected: projected_gross,
                limit: self.limits.max_portfolio_exposure_usd,
            });
        }

        // 11. Market-data dependent checks.
        if let Some(b) = book {
            let age = (now - b.timestamp).num_milliseconds();
            if age > self.limits.max_market_data_age_ms {
                return Self::reject(R::StaleMarketData {
                    age_ms: age, max_age_ms: self.limits.max_market_data_age_ms });
            }

            // Liquidity within our limit price.
            let available = b.notional_within(order.side, order.limit_price);
            if available < self.limits.min_liquidity_usd {
                return Self::reject(R::InsufficientLiquidity {
                    available, required: self.limits.min_liquidity_usd });
            }

            // Expected slippage from actually sweeping the book for our size.
            if let Some((vwap, _)) = b.sweep_vwap(order.side, order.quantity, order.limit_price) {
                let bps = Bps::slippage(order.reference_price, vwap, order.side);
                if bps > Decimal::from(self.limits.max_slippage_bps) {
                    return Self::reject(R::SlippageTooWide {
                        estimated_bps: bps.round().try_into().unwrap_or(i64::MAX),
                        limit_bps: self.limits.max_slippage_bps,
                    });
                }
            } else {
                // The book cannot fill any of it within our limit.
                return Self::reject(R::InsufficientLiquidity {
                    available: Usd::ZERO, required: self.limits.min_liquidity_usd });
            }
        } else if self.limits.require_market_data {
            return Self::reject(R::StaleMarketData {
                age_ms: i64::MAX, max_age_ms: self.limits.max_market_data_age_ms });
        }

        RiskVerdict::Approved
    }

    fn reject(r: RiskRejection) -> RiskVerdict { RiskVerdict::Rejected(Box::new(r)) }

    /// Should the kill switch trip automatically on this rejection?
    ///
    /// Only breaches that indicate the *system* is in a bad state, not ordinary
    /// per-order limit hits — otherwise one oversized signal would halt everything.
    pub fn should_auto_engage_kill_switch(r: &RiskRejection) -> bool {
        matches!(r, RiskRejection::DailyLossLimitReached { .. })
    }
}

/// Tracks daily PnL against a UTC day boundary.
#[derive(Debug, Clone)]
pub struct DailyRiskBudget {
    day: u32,
    realized_today: Usd,
    limit: Usd,
}

impl DailyRiskBudget {
    pub fn new(limit: Usd, now: DateTime<Utc>) -> Self {
        Self { day: now.ordinal(), realized_today: Usd::ZERO, limit }
    }

    /// Books PnL, rolling over automatically at the UTC day boundary.
    pub fn record(&mut self, pnl: Usd, now: DateTime<Utc>) {
        if now.ordinal() != self.day {
            self.day = now.ordinal();
            self.realized_today = Usd::ZERO;
        }
        self.realized_today += pnl;
    }

    pub fn realized_today(&self) -> Usd { self.realized_today }

    /// Loss budget still available. Zero once the limit is reached.
    pub fn remaining(&self) -> Usd {
        (self.limit + self.realized_today.min(Usd::ZERO)).max(Usd::ZERO)
    }

    pub fn is_breached(&self) -> bool { self.realized_today < -self.limit }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{
        CorrelationId, Level, OrderId, OrderType, Price, Qty, Side, TimeInForce, TokenId,
    };
    use rust_decimal_macros::dec;

    fn limits() -> RiskLimits {
        RiskLimits {
            max_trade_usd: Usd::new(dec!(100)),
            min_trade_usd: Usd::new(dec!(5)),
            max_position_usd: Usd::new(dec!(1000)),
            max_market_exposure_usd: Usd::new(dec!(1000)),
            max_portfolio_exposure_usd: Usd::new(dec!(5000)),
            max_daily_loss_usd: Usd::new(dec!(100)),
            max_open_orders: 20,
            max_slippage_bps: 50,
            min_liquidity_usd: Usd::new(dec!(50)),
            max_market_data_age_ms: 30_000,
            max_live_order_usd: Usd::new(dec!(50)),
            require_market_data: false,
        }
    }

    fn snap() -> RiskSnapshot {
        RiskSnapshot {
            daily_pnl: Usd::ZERO,
            gross_exposure: Usd::ZERO,
            market_exposure: Usd::ZERO,
            wallet_exposure: Usd::ZERO,
            token_exposure: Usd::ZERO,
            open_orders: 0,
            equity: Usd::new(dec!(10_000)),
        }
    }

    fn order(qty: Decimal, price: Decimal) -> OrderRequest {
        OrderRequest {
            order_id: OrderId::new(),
            correlation_id: CorrelationId::new(),
            signal_id: None,
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: TokenId::new("8320847481581361120679688919767116680249870957184742802638701891451667752578").unwrap(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            quantity: Qty::new(qty).unwrap(),
            limit_price: Price::new(price).unwrap(),
            reference_price: Price::new(price).unwrap(),
            tick_size: dec!(0.01),
            created_at: Utc::now(),
        }
    }

    fn book(ask: Decimal, size: Decimal, age_ms: i64) -> OrderBook {
        OrderBook {
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: TokenId::new("8320847481581361120679688919767116680249870957184742802638701891451667752578").unwrap(),
            bids: vec![Level { price: Price::new(dec!(0.5)).unwrap(), size: Qty::new(size).unwrap() }],
            asks: vec![Level { price: Price::new(ask).unwrap(), size: Qty::new(size).unwrap() }],
            tick_size: dec!(0.01),
            min_order_size: dec!(1),
            timestamp: Utc::now() - chrono::Duration::milliseconds(age_ms),
            seq: 1,
        }
    }

    fn engine() -> RiskEngine { RiskEngine::new(limits(), AppMode::Paper, false) }

    fn check(e: &RiskEngine, o: &OrderRequest, s: &RiskSnapshot, k: &KillSwitch, b: Option<&OrderBook>) -> RiskVerdict {
        e.check(o, s, &SystemStatus::default(), k, b, true, "W", None, None, Utc::now())
    }

    #[test]
    fn a_clean_order_is_approved() {
        let v = check(&engine(), &order(dec!(100), dec!(0.5)), &snap(), &KillSwitch::new(), Some(&book(dec!(0.5), dec!(10_000), 0)));
        assert!(v.is_approved(), "got {v:?}");
    }

    #[test]
    fn kill_switch_blocks_everything_first() {
        let k = KillSwitch::new();
        k.engage("manual halt", "operator");
        // Even an otherwise-perfect order is refused, and the reason is the kill switch.
        let v = check(&engine(), &order(dec!(100), dec!(0.5)), &snap(), &k, Some(&book(dec!(0.5), dec!(10_000), 0)));
        assert_eq!(v.rejection().unwrap().code(), "kill_switch");
    }

    #[test]
    fn kill_switch_outranks_other_breaches() {
        // An order that also breaches size must still report the kill switch, because
        // that is the condition an operator needs to see.
        let k = KillSwitch::new();
        k.engage("halt", "op");
        let mut s = snap();
        s.open_orders = 999;
        let v = check(&engine(), &order(dec!(100_000), dec!(0.5)), &s, &k, None);
        assert_eq!(v.rejection().unwrap().code(), "kill_switch");
    }

    #[test]
    fn live_mode_without_arming_is_refused() {
        let e = RiskEngine::new(limits(), AppMode::Live, false);
        let v = check(&e, &order(dec!(50), dec!(0.5)), &snap(), &KillSwitch::new(), Some(&book(dec!(0.5), dec!(10_000), 0)));
        assert_eq!(v.rejection().unwrap().code(), "live_not_armed");
    }

    #[test]
    fn live_mode_applies_a_tighter_order_cap() {
        let e = RiskEngine::new(limits(), AppMode::Live, true);
        // $80 passes the $100 global cap but breaches the $50 live cap.
        let v = check(&e, &order(dec!(160), dec!(0.5)), &snap(), &KillSwitch::new(), Some(&book(dec!(0.5), dec!(10_000), 0)));
        assert_eq!(v.rejection().unwrap().code(), "max_trade_size");
    }

    #[test]
    fn each_limit_produces_its_own_rejection_code() {
        let e = engine();
        let k = KillSwitch::new();
        let b = book(dec!(0.5), dec!(10_000), 0);

        // oversized trade
        assert_eq!(check(&e, &order(dec!(1000), dec!(0.5)), &snap(), &k, Some(&b))
            .rejection().unwrap().code(), "max_trade_size");
        // undersized trade
        assert_eq!(check(&e, &order(dec!(2), dec!(0.5)), &snap(), &k, Some(&b))
            .rejection().unwrap().code(), "below_min_order_size");
        // open order slots
        let mut s = snap(); s.open_orders = 20;
        assert_eq!(check(&e, &order(dec!(100), dec!(0.5)), &s, &k, Some(&b))
            .rejection().unwrap().code(), "max_open_orders");
        // daily loss
        let mut s = snap(); s.daily_pnl = Usd::new(dec!(-150));
        assert_eq!(check(&e, &order(dec!(100), dec!(0.5)), &s, &k, Some(&b))
            .rejection().unwrap().code(), "daily_loss_limit");
        // position cap
        let mut s = snap(); s.token_exposure = Usd::new(dec!(990));
        assert_eq!(check(&e, &order(dec!(100), dec!(0.5)), &s, &k, Some(&b))
            .rejection().unwrap().code(), "max_position");
        // portfolio exposure
        let mut s = snap(); s.gross_exposure = Usd::new(dec!(4990));
        assert_eq!(check(&e, &order(dec!(100), dec!(0.5)), &s, &k, Some(&b))
            .rejection().unwrap().code(), "max_portfolio_exposure");
        // stale data
        assert_eq!(check(&e, &order(dec!(100), dec!(0.5)), &snap(), &k, Some(&book(dec!(0.5), dec!(10_000), 60_000)))
            .rejection().unwrap().code(), "stale_market_data");
        // thin book
        assert_eq!(check(&e, &order(dec!(100), dec!(0.5)), &snap(), &k, Some(&book(dec!(0.5), dec!(10), 0)))
            .rejection().unwrap().code(), "insufficient_liquidity");
    }

    #[test]
    fn duplicate_source_events_are_refused_at_the_risk_layer_too() {
        let e = engine();
        let ev = SourceEventId::from_digest("deadbeef");
        let v = e.check(&order(dec!(100), dec!(0.5)), &snap(), &SystemStatus::default(),
            &KillSwitch::new(), Some(&book(dec!(0.5), dec!(10_000), 0)), true, "W", None, Some(&ev), Utc::now());
        assert_eq!(v.rejection().unwrap().code(), "duplicate_order");
    }

    #[test]
    fn disabled_wallet_and_untradable_market_are_refused() {
        let e = engine();
        let b = book(dec!(0.5), dec!(10_000), 0);
        let v = e.check(&order(dec!(100), dec!(0.5)), &snap(), &SystemStatus::default(),
            &KillSwitch::new(), Some(&b), false, "W", None, None, Utc::now());
        assert_eq!(v.rejection().unwrap().code(), "wallet_disabled");

        let mid = MarketId::new(format!("0x{:064x}", 1)).unwrap();
        let v = e.check(&order(dec!(100), dec!(0.5)), &snap(), &SystemStatus::default(),
            &KillSwitch::new(), Some(&b), true, "W", Some((&mid, false, "closed")), None, Utc::now());
        assert_eq!(v.rejection().unwrap().code(), "market_not_tradable");
    }

    #[test]
    fn unhealthy_subsystems_block_trading() {
        let e = engine();
        let b = book(dec!(0.5), dec!(10_000), 0);
        for (st, _label) in [
            (SystemStatus { execution_ready: false, ..Default::default() }, "exec"),
            (SystemStatus { source_feed_healthy: false, ..Default::default() }, "feed"),
            (SystemStatus { market_data_healthy: false, ..Default::default() }, "md"),
        ] {
            let v = e.check(&order(dec!(100), dec!(0.5)), &snap(), &st, &KillSwitch::new(),
                Some(&b), true, "W", None, None, Utc::now());
            assert_eq!(v.rejection().unwrap().code(), "system_unhealthy");
        }
    }

    #[test]
    fn database_outage_degrades_but_does_not_block() {
        // Losing durable audit is bad, but halting a live book over it is worse.
        let e = engine();
        let st = SystemStatus { database_healthy: false, ..Default::default() };
        let v = e.check(&order(dec!(100), dec!(0.5)), &snap(), &st, &KillSwitch::new(),
            Some(&book(dec!(0.5), dec!(10_000), 0)), true, "W", None, None, Utc::now());
        assert!(v.is_approved());
    }

    #[test]
    fn wide_slippage_is_refused() {
        let e = engine();
        // Reference 0.50 but the book only offers 0.60 -> 2000bps, way past the 50 limit.
        let mut o = order(dec!(100), dec!(0.65));
        o.reference_price = Price::new(dec!(0.50)).unwrap();
        let v = check(&e, &o, &snap(), &KillSwitch::new(), Some(&book(dec!(0.60), dec!(10_000), 0)));
        assert_eq!(v.rejection().unwrap().code(), "slippage_too_wide");
    }

    #[test]
    fn missing_book_is_refused_when_market_data_is_required() {
        let mut l = limits();
        l.require_market_data = true;
        let e = RiskEngine::new(l, AppMode::Paper, false);
        let v = check(&e, &order(dec!(100), dec!(0.5)), &snap(), &KillSwitch::new(), None);
        assert_eq!(v.rejection().unwrap().code(), "stale_market_data");
    }

    #[test]
    fn only_systemic_breaches_auto_engage_the_kill_switch() {
        assert!(RiskEngine::should_auto_engage_kill_switch(
            &RiskRejection::DailyLossLimitReached { daily_pnl: Usd::ZERO, limit: Usd::ZERO }));
        // An ordinary oversized order must not halt the whole system.
        assert!(!RiskEngine::should_auto_engage_kill_switch(
            &RiskRejection::MaxTradeSizeExceeded { requested: Usd::ZERO, limit: Usd::ZERO }));
        assert!(!RiskEngine::should_auto_engage_kill_switch(
            &RiskRejection::InsufficientLiquidity { available: Usd::ZERO, required: Usd::ZERO }));
    }

    #[test]
    fn daily_budget_rolls_over_at_the_utc_day_boundary() {
        let day1 = Utc::now();
        let mut b = DailyRiskBudget::new(Usd::new(dec!(100)), day1);
        b.record(Usd::new(dec!(-80)), day1);
        assert_eq!(b.remaining().get(), dec!(20));
        assert!(!b.is_breached());
        b.record(Usd::new(dec!(-30)), day1);
        assert!(b.is_breached());
        assert_eq!(b.remaining().get(), Decimal::ZERO);

        let day2 = day1 + chrono::Duration::days(1);
        b.record(Usd::ZERO, day2);
        assert!(!b.is_breached(), "a new UTC day must reset the loss budget");
        assert_eq!(b.remaining().get(), dec!(100));
    }

    #[test]
    fn profits_do_not_expand_the_loss_budget() {
        let now = Utc::now();
        let mut b = DailyRiskBudget::new(Usd::new(dec!(100)), now);
        b.record(Usd::new(dec!(500)), now);
        // Gains must not licence larger losses later in the day.
        assert_eq!(b.remaining().get(), dec!(100));
    }
}
