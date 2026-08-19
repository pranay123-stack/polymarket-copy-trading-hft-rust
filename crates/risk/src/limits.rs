//! Risk limit definitions.

use config::RiskConfig;
use domain::Usd;
use serde::{Deserialize, Serialize};

/// The full set of enforced limits.
///
/// Kept separate from [`RiskConfig`] so limits can be adjusted at runtime through the
/// API without mutating loaded configuration, and so the risk crate does not depend on
/// how configuration happened to be loaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_trade_usd: Usd,
    pub min_trade_usd: Usd,
    pub max_position_usd: Usd,
    pub max_market_exposure_usd: Usd,
    pub max_portfolio_exposure_usd: Usd,
    pub max_daily_loss_usd: Usd,
    pub max_open_orders: u32,
    pub max_slippage_bps: u32,
    pub min_liquidity_usd: Usd,
    pub max_market_data_age_ms: i64,
    /// Additional, tighter cap that applies only in LIVE mode.
    pub max_live_order_usd: Usd,
    /// Refuse to trade at all without a fresh book. Off in paper/demo so the system is
    /// demonstrable without market data; recommended on in live.
    pub require_market_data: bool,
}

impl RiskLimits {
    pub fn from_config(r: &RiskConfig, max_live_order_usd: Usd, require_market_data: bool) -> Self {
        Self {
            max_trade_usd: r.max_trade_usd,
            min_trade_usd: r.min_trade_usd,
            max_position_usd: r.max_position_usd,
            max_market_exposure_usd: r.max_market_exposure_usd,
            max_portfolio_exposure_usd: r.max_portfolio_exposure_usd,
            max_daily_loss_usd: r.max_daily_loss_usd,
            max_open_orders: r.max_open_orders,
            max_slippage_bps: r.max_slippage_bps,
            min_liquidity_usd: r.min_liquidity_usd,
            max_market_data_age_ms: r.max_market_data_age_ms,
            max_live_order_usd,
            require_market_data,
        }
    }
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self::from_config(&RiskConfig::default(), Usd::new(rust_decimal::Decimal::new(50, 0)), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn defaults_mirror_the_conservative_config() {
        let l = RiskLimits::default();
        assert_eq!(l.max_trade_usd.get(), dec!(100));
        assert_eq!(l.max_daily_loss_usd.get(), dec!(100));
        assert_eq!(l.max_live_order_usd.get(), dec!(50));
        assert!(l.max_live_order_usd <= l.max_trade_usd,
            "the live cap must never be looser than the global cap");
    }
}
