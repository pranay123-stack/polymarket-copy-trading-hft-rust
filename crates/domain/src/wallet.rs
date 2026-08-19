//! Target wallet configuration — the traders we copy.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use crate::ids::{Address, MarketId};
use crate::money::Usd;
use crate::trade::SignalSkipReason;

/// How a target's trade size is translated into ours.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum SizingMode {
    /// `copy = target_notional * ratio`
    FixedRatio { ratio: Decimal },
    /// Every copy is the same notional, regardless of the target's size.
    FixedUsd { amount: Decimal },
    /// `copy = portfolio_equity * pct`
    PortfolioPercent { pct: Decimal },
    /// Ratio-based, then scaled down by remaining risk budget, available liquidity
    /// and headroom to the position cap. See `strategy::sizing`.
    RiskAdjusted { base_ratio: Decimal, liquidity_cap_pct: Decimal },
}

impl SizingMode {
    pub fn name(&self) -> &'static str {
        match self {
            Self::FixedRatio { .. } => "fixed_ratio",
            Self::FixedUsd { .. } => "fixed_usd",
            Self::PortfolioPercent { .. } => "portfolio_percent",
            Self::RiskAdjusted { .. } => "risk_adjusted",
        }
    }
}

impl Default for SizingMode {
    fn default() -> Self { Self::FixedRatio { ratio: dec!(0.25) } }
}

/// One trader we mirror, with per-wallet limits that apply *before* global risk limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetWallet {
    pub address: Address,
    pub nickname: String,
    pub enabled: bool,
    pub sizing: SizingMode,
    pub max_trade_usd: Usd,
    pub max_exposure_usd: Usd,
    pub min_trade_usd: Usd,
    /// Empty = all markets permitted. Non-empty = allowlist.
    pub allowed_markets: Vec<MarketId>,
    /// Always denied, and checked before the allowlist.
    pub blocked_markets: Vec<MarketId>,
    /// Ignore source trades smaller than this — avoids copying dust.
    pub min_source_notional_usd: Usd,
}

impl TargetWallet {
    pub fn new(address: Address, nickname: impl Into<String>) -> Self {
        Self {
            address,
            nickname: nickname.into(),
            enabled: true,
            sizing: SizingMode::default(),
            max_trade_usd: Usd::new(dec!(100)),
            max_exposure_usd: Usd::new(dec!(1000)),
            min_trade_usd: Usd::new(dec!(5)),
            allowed_markets: Vec::new(),
            blocked_markets: Vec::new(),
            min_source_notional_usd: Usd::new(dec!(50)),
        }
    }

    /// Decides whether this wallet's trade in this market should produce a signal.
    /// Returns the explicit reason on refusal so it can be surfaced, never swallowed.
    pub fn admits(&self, market: &MarketId, source_notional: Usd) -> Result<(), SignalSkipReason> {
        if !self.enabled {
            return Err(SignalSkipReason::WalletDisabled);
        }
        // Block list wins over the allow list.
        if self.blocked_markets.contains(market) {
            return Err(SignalSkipReason::MarketBlocked(market.to_string()));
        }
        if !self.allowed_markets.is_empty() && !self.allowed_markets.contains(market) {
            return Err(SignalSkipReason::MarketNotAllowed(market.to_string()));
        }
        if source_notional < self.min_source_notional_usd {
            return Err(SignalSkipReason::BelowMinNotional {
                got: source_notional.to_string(),
                min: self.min_source_notional_usd.to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> Address { Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap() }
    fn m(n: u8) -> MarketId {
        MarketId::new(format!("0x{:064x}", n)).unwrap()
    }

    #[test]
    fn disabled_wallet_admits_nothing() {
        let mut w = TargetWallet::new(addr(), "A");
        w.enabled = false;
        assert_eq!(w.admits(&m(1), Usd::new(dec!(1000))), Err(SignalSkipReason::WalletDisabled));
    }

    #[test]
    fn block_list_beats_allow_list() {
        let mut w = TargetWallet::new(addr(), "A");
        w.allowed_markets = vec![m(1)];
        w.blocked_markets = vec![m(1)];
        // Same market on both lists must be denied — fail safe, not fail open.
        assert!(matches!(w.admits(&m(1), Usd::new(dec!(1000))), Err(SignalSkipReason::MarketBlocked(_))));
    }

    #[test]
    fn empty_allow_list_means_all_markets() {
        let w = TargetWallet::new(addr(), "A");
        assert!(w.admits(&m(7), Usd::new(dec!(1000))).is_ok());
    }

    #[test]
    fn non_empty_allow_list_excludes_others() {
        let mut w = TargetWallet::new(addr(), "A");
        w.allowed_markets = vec![m(1)];
        assert!(w.admits(&m(1), Usd::new(dec!(1000))).is_ok());
        assert!(matches!(w.admits(&m(2), Usd::new(dec!(1000))), Err(SignalSkipReason::MarketNotAllowed(_))));
    }

    #[test]
    fn dust_source_trades_are_skipped() {
        let mut w = TargetWallet::new(addr(), "A");
        w.min_source_notional_usd = Usd::new(dec!(50));
        assert!(matches!(w.admits(&m(1), Usd::new(dec!(49.99))), Err(SignalSkipReason::BelowMinNotional { .. })));
        assert!(w.admits(&m(1), Usd::new(dec!(50))).is_ok());
    }
}
