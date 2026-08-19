//! Positions and average-entry accounting.
//!
//! Uses **weighted-average cost**: realised PnL is booked when a position is reduced,
//! and the average entry price is only changed by trades that *increase* exposure.
//! A reduction never moves the average entry — that is what keeps unrealised PnL on
//! the remaining shares honest.
//!
//! A position that crosses through zero (long 50, sell 80) is handled explicitly:
//! the closing leg realises against the old average, and the opening leg establishes a
//! fresh average from the crossing price. Getting this wrong is a classic source of
//! PnL that slowly drifts from reality.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::ids::{MarketId, TokenId};
use crate::money::{Price, Qty, Side, Usd};

/// Net exposure in one outcome token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub outcome: String,
    /// Signed: positive = long, negative = short.
    pub net_quantity: Decimal,
    /// Average entry of the *current* exposure. Zero when flat.
    pub avg_entry: Decimal,
    pub realized_pnl: Usd,
    pub fees_paid: Usd,
    /// Last observed mark, for unrealised PnL. `None` until a mark arrives.
    pub mark_price: Option<Price>,
    pub updated_at: DateTime<Utc>,
}

impl Position {
    pub fn new(market_id: MarketId, token_id: TokenId, outcome: String) -> Self {
        Self {
            market_id,
            token_id,
            outcome,
            net_quantity: Decimal::ZERO,
            avg_entry: Decimal::ZERO,
            realized_pnl: Usd::ZERO,
            fees_paid: Usd::ZERO,
            mark_price: None,
            updated_at: Utc::now(),
        }
    }

    pub fn is_flat(&self) -> bool { self.net_quantity.is_zero() }
    pub fn is_long(&self) -> bool { self.net_quantity > Decimal::ZERO }

    /// Absolute notional at the current mark (or at cost if unmarked).
    pub fn exposure(&self) -> Usd {
        let px = self.mark_price.map(|p| p.get()).unwrap_or(self.avg_entry);
        Usd::new((self.net_quantity * px).abs())
    }

    /// Unrealised PnL. `None` while unmarked — we do not invent a mark.
    pub fn unrealized_pnl(&self) -> Option<Usd> {
        let mark = self.mark_price?.get();
        Some(Usd::new(self.net_quantity * (mark - self.avg_entry)))
    }

    pub fn total_pnl(&self) -> Usd {
        self.realized_pnl + self.unrealized_pnl().unwrap_or(Usd::ZERO)
    }

    /// Applies a fill and returns the realised PnL booked by *this* fill.
    pub fn apply(&mut self, side: Side, qty: Qty, price: Price, fee: Usd, at: DateTime<Utc>) -> Usd {
        let signed = side.sign() * qty.get();
        let px = price.get();
        let old_qty = self.net_quantity;
        let mut realized = Decimal::ZERO;

        if old_qty.is_zero() || (old_qty > Decimal::ZERO) == (signed > Decimal::ZERO) {
            // Opening or increasing: roll the weighted average, realise nothing.
            let new_qty = old_qty + signed;
            if !new_qty.is_zero() {
                self.avg_entry = ((old_qty * self.avg_entry) + (signed * px)) / new_qty;
            }
            self.net_quantity = new_qty;
        } else {
            // Reducing, closing or crossing.
            let closing = signed.abs().min(old_qty.abs());
            // Long closed above entry, or short closed below entry, is a gain.
            realized = if old_qty > Decimal::ZERO {
                closing * (px - self.avg_entry)
            } else {
                closing * (self.avg_entry - px)
            };
            let new_qty = old_qty + signed;
            if new_qty.is_zero() {
                self.avg_entry = Decimal::ZERO;
            } else if (new_qty > Decimal::ZERO) != (old_qty > Decimal::ZERO) {
                // Crossed through zero: the residual is a brand-new position at `px`.
                self.avg_entry = px;
            }
            // Pure reduction leaves avg_entry untouched, by design.
            self.net_quantity = new_qty;
        }

        self.realized_pnl += Usd::new(realized);
        self.fees_paid += fee;
        // Fees are a realised cost the moment they are incurred.
        self.realized_pnl -= fee;
        self.updated_at = at;
        Usd::new(realized) - fee
    }

    pub fn mark(&mut self, price: Price, at: DateTime<Utc>) {
        self.mark_price = Some(price);
        self.updated_at = at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn pos() -> Position {
        Position::new(
            MarketId::new("0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52").unwrap(),
            TokenId::new("725510240982585425945346839425236061430146906202430232984977").unwrap(),
            "Yes".into(),
        )
    }
    fn q(d: Decimal) -> Qty { Qty::new(d).unwrap() }
    fn p(d: Decimal) -> Price { Price::new(d).unwrap() }

    #[test]
    fn opening_sets_average_entry() {
        let mut x = pos();
        x.apply(Side::Buy, q(dec!(100)), p(dec!(0.60)), Usd::ZERO, Utc::now());
        assert_eq!(x.net_quantity, dec!(100));
        assert_eq!(x.avg_entry, dec!(0.60));
        assert_eq!(x.realized_pnl, Usd::ZERO);
    }

    #[test]
    fn adding_rolls_the_weighted_average() {
        let mut x = pos();
        x.apply(Side::Buy, q(dec!(100)), p(dec!(0.60)), Usd::ZERO, Utc::now());
        x.apply(Side::Buy, q(dec!(100)), p(dec!(0.80)), Usd::ZERO, Utc::now());
        assert_eq!(x.avg_entry, dec!(0.70));
        assert_eq!(x.realized_pnl, Usd::ZERO, "adding must not realise anything");
    }

    #[test]
    fn reducing_realises_but_leaves_average_alone() {
        let mut x = pos();
        x.apply(Side::Buy, q(dec!(100)), p(dec!(0.60)), Usd::ZERO, Utc::now());
        let r = x.apply(Side::Sell, q(dec!(40)), p(dec!(0.70)), Usd::ZERO, Utc::now());
        assert_eq!(r.get(), dec!(4));                 // 40 * 0.10
        assert_eq!(x.net_quantity, dec!(60));
        assert_eq!(x.avg_entry, dec!(0.60), "a reduction must not move the average entry");
        assert_eq!(x.realized_pnl.get(), dec!(4));
    }

    #[test]
    fn closing_flat_resets_average() {
        let mut x = pos();
        x.apply(Side::Buy, q(dec!(100)), p(dec!(0.60)), Usd::ZERO, Utc::now());
        x.apply(Side::Sell, q(dec!(100)), p(dec!(0.65)), Usd::ZERO, Utc::now());
        assert!(x.is_flat());
        assert_eq!(x.avg_entry, Decimal::ZERO);
        assert_eq!(x.realized_pnl.get(), dec!(5));
        assert_eq!(x.unrealized_pnl(), None, "flat and unmarked -> no unrealised");
    }

    #[test]
    fn crossing_through_zero_reprices_the_residual() {
        let mut x = pos();
        x.apply(Side::Buy, q(dec!(50)), p(dec!(0.60)), Usd::ZERO, Utc::now());
        // Sell 80: closes 50 long (realising 50*0.10) and opens 30 short at 0.70.
        let r = x.apply(Side::Sell, q(dec!(80)), p(dec!(0.70)), Usd::ZERO, Utc::now());
        assert_eq!(r.get(), dec!(5));
        assert_eq!(x.net_quantity, dec!(-30));
        assert_eq!(x.avg_entry, dec!(0.70), "residual short must be priced at the crossing price");
    }

    #[test]
    fn short_profits_when_price_falls() {
        let mut x = pos();
        x.apply(Side::Sell, q(dec!(100)), p(dec!(0.60)), Usd::ZERO, Utc::now());
        assert_eq!(x.net_quantity, dec!(-100));
        assert_eq!(x.avg_entry, dec!(0.60));
        let r = x.apply(Side::Buy, q(dec!(100)), p(dec!(0.50)), Usd::ZERO, Utc::now());
        assert_eq!(r.get(), dec!(10));
    }

    #[test]
    fn fees_are_realised_immediately() {
        let mut x = pos();
        x.apply(Side::Buy, q(dec!(100)), p(dec!(0.60)), Usd::new(dec!(0.5)), Utc::now());
        assert_eq!(x.realized_pnl.get(), dec!(-0.5));
        assert_eq!(x.fees_paid.get(), dec!(0.5));
    }

    #[test]
    fn unrealised_requires_a_mark() {
        let mut x = pos();
        x.apply(Side::Buy, q(dec!(100)), p(dec!(0.60)), Usd::ZERO, Utc::now());
        assert_eq!(x.unrealized_pnl(), None);
        x.mark(p(dec!(0.75)), Utc::now());
        assert_eq!(x.unrealized_pnl().unwrap().get(), dec!(15));
        assert_eq!(x.exposure().get(), dec!(75));
    }
}
