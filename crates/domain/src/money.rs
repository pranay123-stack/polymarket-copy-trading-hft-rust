//! Money, price and quantity types.
//!
//! Every monetary value in this system is `rust_decimal::Decimal`. No `f64` arithmetic
//! is performed on prices, sizes, notionals or PnL anywhere in the codebase.
//!
//! Polymarket's JSON emits sizes and prices as floats (`"price": 0.5599999776`), so the
//! ingest boundary is the one place a float is touched: it is rendered to its shortest
//! round-trip decimal form and parsed into `Decimal`. `Price::from_feed_f64` is the
//! single sanctioned entry point and it is deliberately loud about what it does.

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};

use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MoneyError {
    #[error("price {0} outside the valid Polymarket range (0, 1)")]
    PriceOutOfRange(Decimal),
    #[error("quantity {0} is negative")]
    NegativeQuantity(Decimal),
    #[error("value {0:?} is not a finite decimal")]
    NotFinite(String),
}

/// A share price, strictly within `(0, 1)`.
///
/// A prediction-market share pays out \$1 or \$0, so price *is* implied probability.
/// The exclusive bounds are deliberate: a resting order at exactly 0 or 1 is
/// meaningless, and the CLOB rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Price(Decimal);

impl Price {
    pub const MIN: Decimal = dec!(0.0001);
    pub const MAX: Decimal = dec!(0.9999);

    pub fn new(d: Decimal) -> Result<Self, MoneyError> {
        if d <= Decimal::ZERO || d >= Decimal::ONE {
            return Err(MoneyError::PriceOutOfRange(d));
        }
        Ok(Self(d))
    }

    /// Ingest boundary for Polymarket's float-encoded prices.
    ///
    /// `0.5599999776` from the wire becomes `0.5599999776`, not
    /// `0.55999997760000001...`: we go through the shortest round-trip string form so
    /// the decimal we store is the number the venue meant.
    pub fn from_feed_f64(v: f64) -> Result<Self, MoneyError> {
        if !v.is_finite() {
            return Err(MoneyError::NotFinite(v.to_string()));
        }
        let d = Decimal::from_str(&v.to_string())
            .map_err(|_| MoneyError::NotFinite(v.to_string()))?;
        Self::new(d)
    }

    /// Clamps into the tradable range instead of failing. Used when deriving a limit
    /// price from a slippage budget, where arithmetic can legitimately push past a bound.
    pub fn saturating(d: Decimal) -> Self {
        Self(d.clamp(Self::MIN, Self::MAX))
    }

    pub fn get(&self) -> Decimal { self.0 }

    /// Rounds to the market's tick. Quoting off-tick gets orders rejected by the CLOB,
    /// and tick size is per-market (0.01 and 0.001 are both common).
    ///
    /// Rounds toward the side that is *less* aggressive, so rounding can never
    /// silently increase the price we are willing to pay.
    pub fn round_to_tick(&self, tick: Decimal, side: Side) -> Self {
        if tick <= Decimal::ZERO {
            return *self;
        }
        let steps = self.0 / tick;
        let rounded = match side {
            Side::Buy => steps.floor(),  // pay no more than asked
            Side::Sell => steps.ceil(),  // receive no less than asked
        };
        Self::saturating(rounded * tick)
    }

    /// The complementary leg's price. YES at 0.61 implies NO at 0.39.
    pub fn complement(&self) -> Self { Self(Decimal::ONE - self.0) }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
}

/// A share quantity. Always non-negative; direction lives in [`Side`], never in a sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Qty(Decimal);

impl Qty {
    pub const ZERO: Qty = Qty(Decimal::ZERO);

    pub fn new(d: Decimal) -> Result<Self, MoneyError> {
        if d < Decimal::ZERO {
            return Err(MoneyError::NegativeQuantity(d));
        }
        Ok(Self(d))
    }

    pub fn from_feed_f64(v: f64) -> Result<Self, MoneyError> {
        if !v.is_finite() {
            return Err(MoneyError::NotFinite(v.to_string()));
        }
        let d = Decimal::from_str(&v.to_string())
            .map_err(|_| MoneyError::NotFinite(v.to_string()))?;
        Self::new(d)
    }

    pub fn get(&self) -> Decimal { self.0 }
    pub fn is_zero(&self) -> bool { self.0.is_zero() }

    /// `qty * price` — the USD notional of this many shares.
    pub fn notional(&self, p: Price) -> Usd { Usd(self.0 * p.get()) }

    pub fn min(self, other: Self) -> Self { Self(self.0.min(other.0)) }
    pub fn max(self, other: Self) -> Self { Self(self.0.max(other.0)) }

    /// Saturating subtraction — a remaining-quantity can never go negative.
    pub fn saturating_sub(self, other: Self) -> Self {
        Self((self.0 - other.0).max(Decimal::ZERO))
    }

    /// Rounds down to a whole number of the venue's size increment.
    pub fn floor_to(&self, step: Decimal) -> Self {
        if step <= Decimal::ZERO { return *self; }
        Self((self.0 / step).floor() * step)
    }
}

impl Add for Qty { type Output = Qty; fn add(self, o: Self) -> Qty { Qty(self.0 + o.0) } }
impl Sub for Qty { type Output = Qty; fn sub(self, o: Self) -> Qty { Qty(self.0 - o.0) } }
impl AddAssign for Qty { fn add_assign(&mut self, o: Self) { self.0 += o.0 } }
impl SubAssign for Qty { fn sub_assign(&mut self, o: Self) { self.0 -= o.0 } }
impl Sum for Qty { fn sum<I: Iterator<Item = Qty>>(i: I) -> Qty { Qty(i.map(|q| q.0).sum()) } }

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
}

/// A USD amount. **Signed** — PnL, cash deltas and realised losses are all `Usd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Usd(Decimal);

impl Usd {
    pub const ZERO: Usd = Usd(Decimal::ZERO);

    pub fn new(d: Decimal) -> Self { Self(d) }

    pub fn from_feed_f64(v: f64) -> Result<Self, MoneyError> {
        if !v.is_finite() {
            return Err(MoneyError::NotFinite(v.to_string()));
        }
        Decimal::from_str(&v.to_string())
            .map(Self)
            .map_err(|_| MoneyError::NotFinite(v.to_string()))
    }

    pub fn get(&self) -> Decimal { self.0 }
    pub fn is_negative(&self) -> bool { self.0 < Decimal::ZERO }
    pub fn abs(&self) -> Usd { Usd(self.0.abs()) }

    /// How many shares this much cash buys at `p`.
    pub fn shares_at(&self, p: Price) -> Qty {
        Qty((self.0 / p.get()).max(Decimal::ZERO))
    }

    /// Rounds to cents for display and storage. Internal maths keeps full precision.
    pub fn round_cents(&self) -> Usd {
        Usd(self.0.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero))
    }

    pub fn min(self, other: Self) -> Self { Self(self.0.min(other.0)) }
    pub fn max(self, other: Self) -> Self { Self(self.0.max(other.0)) }
}

impl Add for Usd { type Output = Usd; fn add(self, o: Self) -> Usd { Usd(self.0 + o.0) } }
impl Sub for Usd { type Output = Usd; fn sub(self, o: Self) -> Usd { Usd(self.0 - o.0) } }
impl Neg for Usd { type Output = Usd; fn neg(self) -> Usd { Usd(-self.0) } }
impl AddAssign for Usd { fn add_assign(&mut self, o: Self) { self.0 += o.0 } }
impl SubAssign for Usd { fn sub_assign(&mut self, o: Self) { self.0 -= o.0 } }
impl Mul<Decimal> for Usd { type Output = Usd; fn mul(self, r: Decimal) -> Usd { Usd(self.0 * r) } }
impl Div<Decimal> for Usd { type Output = Usd; fn div(self, r: Decimal) -> Usd { Usd(self.0 / r) } }
impl Sum for Usd { fn sum<I: Iterator<Item = Usd>>(i: I) -> Usd { Usd(i.map(|u| u.0).sum()) } }

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{:.2}", self.0) }
}

/// Basis points, used for fees and slippage budgets. 1 bps = 0.01%.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Bps(pub u32);

impl Bps {
    pub fn as_fraction(&self) -> Decimal { Decimal::from(self.0) / dec!(10000) }
    pub fn apply_to(&self, v: Usd) -> Usd { v * self.as_fraction() }

    /// Measures realised slippage of `actual` against `reference`, in bps.
    /// Sign convention: positive = worse for us.
    pub fn slippage(reference: Price, actual: Price, side: Side) -> Decimal {
        let r = reference.get();
        if r.is_zero() { return Decimal::ZERO; }
        let diff = match side {
            Side::Buy => actual.get() - r,   // paying above reference is bad
            Side::Sell => r - actual.get(),  // selling below reference is bad
        };
        (diff / r) * dec!(10000)
    }
}

/// Trade direction. Direction is never encoded as the sign of a quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(&self) -> Side {
        match self { Side::Buy => Side::Sell, Side::Sell => Side::Buy }
    }
    /// +1 for buy, -1 for sell — for signed position arithmetic.
    pub fn sign(&self) -> Decimal {
        match self { Side::Buy => Decimal::ONE, Side::Sell => Decimal::NEGATIVE_ONE }
    }
    pub fn as_str(&self) -> &'static str {
        match self { Side::Buy => "BUY", Side::Sell => "SELL" }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_rejects_degenerate_bounds() {
        assert!(Price::new(dec!(0)).is_err());
        assert!(Price::new(dec!(1)).is_err());
        assert!(Price::new(dec!(-0.5)).is_err());
        assert!(Price::new(dec!(1.5)).is_err());
        assert!(Price::new(dec!(0.5)).is_ok());
    }

    #[test]
    fn feed_float_does_not_leak_binary_noise() {
        // The literal float Polymarket sent us in a real frame.
        let p = Price::from_feed_f64(0.5599999776).unwrap();
        assert_eq!(p.get(), dec!(0.5599999776));
        assert_eq!(p.to_string(), "0.5599999776");
    }

    #[test]
    fn feed_float_rejects_nan_and_inf() {
        assert!(Price::from_feed_f64(f64::NAN).is_err());
        assert!(Qty::from_feed_f64(f64::INFINITY).is_err());
        assert!(Usd::from_feed_f64(f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn tick_rounding_never_worsens_our_price() {
        let p = Price::new(dec!(0.6178)).unwrap();
        // Buying: round down, never pay more than we intended.
        assert_eq!(p.round_to_tick(dec!(0.01), Side::Buy).get(), dec!(0.61));
        // Selling: round up, never accept less.
        assert_eq!(p.round_to_tick(dec!(0.01), Side::Sell).get(), dec!(0.62));
        // Finer tick, same rule.
        assert_eq!(p.round_to_tick(dec!(0.001), Side::Buy).get(), dec!(0.617));
    }

    #[test]
    fn complement_is_involutive() {
        let p = Price::new(dec!(0.61)).unwrap();
        assert_eq!(p.complement().get(), dec!(0.39));
        assert_eq!(p.complement().complement(), p);
    }

    #[test]
    fn qty_cannot_be_negative_and_saturates() {
        assert!(Qty::new(dec!(-1)).is_err());
        let a = Qty::new(dec!(5)).unwrap();
        let b = Qty::new(dec!(9)).unwrap();
        assert_eq!(a.saturating_sub(b), Qty::ZERO);
    }

    #[test]
    fn notional_and_shares_round_trip() {
        let q = Qty::new(dec!(250)).unwrap();
        let p = Price::new(dec!(0.4)).unwrap();
        assert_eq!(q.notional(p).get(), dec!(100));
        assert_eq!(Usd::new(dec!(100)).shares_at(p).get(), dec!(250));
    }

    #[test]
    fn slippage_sign_means_worse_for_us() {
        let want = Price::new(dec!(0.61)).unwrap();
        let worse_buy = Price::new(dec!(0.6161)).unwrap();
        // Paid 1% more than intended -> +100 bps.
        assert_eq!(Bps::slippage(want, worse_buy, Side::Buy).round_dp(4), dec!(100.0000));
        // Same fill on a sell is favourable -> negative.
        assert!(Bps::slippage(want, worse_buy, Side::Sell) < Decimal::ZERO);
    }

    #[test]
    fn bps_applies_to_notional() {
        assert_eq!(Bps(50).apply_to(Usd::new(dec!(1000))).get(), dec!(5));
    }
}
