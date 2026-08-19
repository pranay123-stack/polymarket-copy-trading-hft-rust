//! Portfolio-level PnL and equity accounting.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::money::Usd;

/// A point-in-time view of the whole book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PnlSnapshot {
    pub at: DateTime<Utc>,
    /// Free cash.
    pub cash: Usd,
    /// Mark-to-market value of open positions.
    pub position_value: Usd,
    /// `cash + position_value`.
    pub equity: Usd,
    pub realized_pnl: Usd,
    /// `None` when any held position lacks a mark — an unmarked book must not
    /// masquerade as a flat one.
    pub unrealized_pnl: Option<Usd>,
    pub fees_paid: Usd,
    /// Sum of absolute position notionals.
    pub gross_exposure: Usd,
    /// Realised + unrealised since the start of the current UTC day.
    pub daily_pnl: Usd,
    /// Peak equity seen so far, for drawdown.
    pub peak_equity: Usd,
    pub open_orders: u32,
    pub active_positions: u32,
}

impl PnlSnapshot {
    pub fn total_pnl(&self) -> Usd {
        self.realized_pnl + self.unrealized_pnl.unwrap_or(Usd::ZERO)
    }

    /// Drawdown from peak equity, as a positive fraction (0.10 = 10% off the peak).
    pub fn drawdown_pct(&self) -> Decimal {
        let peak = self.peak_equity.get();
        if peak <= Decimal::ZERO { return Decimal::ZERO; }
        ((peak - self.equity.get()) / peak).max(Decimal::ZERO)
    }

    /// Capital still deployable: cash minus what is already at risk.
    pub fn available_capital(&self) -> Usd {
        (self.cash - self.gross_exposure).max(Usd::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn snap(equity: Decimal, peak: Decimal) -> PnlSnapshot {
        PnlSnapshot {
            at: Utc::now(),
            cash: Usd::new(equity),
            position_value: Usd::ZERO,
            equity: Usd::new(equity),
            realized_pnl: Usd::ZERO,
            unrealized_pnl: Some(Usd::ZERO),
            fees_paid: Usd::ZERO,
            gross_exposure: Usd::ZERO,
            daily_pnl: Usd::ZERO,
            peak_equity: Usd::new(peak),
            open_orders: 0,
            active_positions: 0,
        }
    }

    #[test]
    fn drawdown_is_zero_at_the_peak() {
        assert_eq!(snap(dec!(1000), dec!(1000)).drawdown_pct(), Decimal::ZERO);
    }

    #[test]
    fn drawdown_measures_distance_below_peak() {
        assert_eq!(snap(dec!(900), dec!(1000)).drawdown_pct(), dec!(0.1));
    }

    #[test]
    fn equity_above_peak_is_not_negative_drawdown() {
        assert_eq!(snap(dec!(1100), dec!(1000)).drawdown_pct(), Decimal::ZERO);
    }

    #[test]
    fn unmarked_book_reports_unknown_unrealised() {
        let mut s = snap(dec!(1000), dec!(1000));
        s.unrealized_pnl = None;
        assert_eq!(s.total_pnl(), Usd::ZERO);
        assert!(s.unrealized_pnl.is_none(), "must stay distinguishable from a real zero");
    }
}
