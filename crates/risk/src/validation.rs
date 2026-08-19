//! Structural order validation, ahead of the limit checks.
//!
//! These are correctness properties of the order itself — an order failing any of them
//! is malformed rather than merely too large, and would be rejected by the venue.

use domain::{OrderRequest, OrderType, Price, Side};
use rust_decimal::Decimal;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("quantity must be greater than zero")]
    ZeroQuantity,
    #[error("quantity {qty} is below the market minimum {min}")]
    BelowMinSize { qty: String, min: String },
    #[error("limit price {price} is not a multiple of tick {tick}")]
    OffTick { price: String, tick: String },
    #[error("buy limit {limit} is below the reference {reference}: the order could never fill")]
    BuyLimitBelowReference { limit: String, reference: String },
    #[error("sell limit {limit} is above the reference {reference}: the order could never fill")]
    SellLimitAboveReference { limit: String, reference: String },
}

pub struct OrderValidator;

impl OrderValidator {
    pub fn validate(o: &OrderRequest, min_order_size: Decimal) -> Result<(), ValidationError> {
        if o.quantity.is_zero() {
            return Err(ValidationError::ZeroQuantity);
        }
        if o.quantity.get() < min_order_size {
            return Err(ValidationError::BelowMinSize {
                qty: o.quantity.to_string(), min: min_order_size.to_string() });
        }
        if o.tick_size > Decimal::ZERO {
            let steps = o.limit_price.get() / o.tick_size;
            if steps.fract() != Decimal::ZERO {
                return Err(ValidationError::OffTick {
                    price: o.limit_price.to_string(), tick: o.tick_size.to_string() });
            }
        }
        // A limit on the wrong side of the reference indicates a sign error upstream:
        // it would rest forever rather than copy the trade.
        if o.order_type == OrderType::Market {
            match o.side {
                Side::Buy if o.limit_price < o.reference_price => {
                    return Err(ValidationError::BuyLimitBelowReference {
                        limit: o.limit_price.to_string(), reference: o.reference_price.to_string() });
                }
                Side::Sell if o.limit_price > o.reference_price => {
                    return Err(ValidationError::SellLimitAboveReference {
                        limit: o.limit_price.to_string(), reference: o.reference_price.to_string() });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Is `p` a whole multiple of `tick`?
    pub fn is_on_tick(p: Price, tick: Decimal) -> bool {
        tick <= Decimal::ZERO || (p.get() / tick).fract() == Decimal::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use domain::{CorrelationId, MarketId, OrderId, Qty, TimeInForce, TokenId};
    use rust_decimal_macros::dec;

    fn order(qty: Decimal, price: Decimal, tick: Decimal, ty: OrderType, side: Side, reference: Decimal) -> OrderRequest {
        OrderRequest {
            order_id: OrderId::new(),
            correlation_id: CorrelationId::new(),
            signal_id: None,
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: TokenId::new("83208474815813611206796889197671166802498709571847428026387018914516").unwrap(),
            side,
            order_type: ty,
            time_in_force: TimeInForce::Gtc,
            quantity: Qty::new(qty).unwrap(),
            limit_price: Price::new(price).unwrap(),
            reference_price: Price::new(reference).unwrap(),
            tick_size: tick,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn valid_order_passes() {
        let o = order(dec!(100), dec!(0.61), dec!(0.01), OrderType::Limit, Side::Buy, dec!(0.61));
        assert!(OrderValidator::validate(&o, dec!(5)).is_ok());
    }

    #[test]
    fn off_tick_prices_are_rejected() {
        // The venue rejects these, and tick size varies per market (0.01 vs 0.001).
        let o = order(dec!(100), dec!(0.615), dec!(0.01), OrderType::Limit, Side::Buy, dec!(0.61));
        assert!(matches!(OrderValidator::validate(&o, dec!(5)), Err(ValidationError::OffTick { .. })));
        // Same price is fine on a finer tick.
        let o = order(dec!(100), dec!(0.615), dec!(0.001), OrderType::Limit, Side::Buy, dec!(0.61));
        assert!(OrderValidator::validate(&o, dec!(5)).is_ok());
    }

    #[test]
    fn below_market_minimum_is_rejected() {
        let o = order(dec!(2), dec!(0.61), dec!(0.01), OrderType::Limit, Side::Buy, dec!(0.61));
        assert!(matches!(OrderValidator::validate(&o, dec!(5)), Err(ValidationError::BelowMinSize { .. })));
    }

    #[test]
    fn marketable_order_on_the_wrong_side_is_caught() {
        // A "market" buy whose protective limit sits below the reference could never
        // fill — that is a sign error upstream, not a valid order.
        let o = order(dec!(100), dec!(0.50), dec!(0.01), OrderType::Market, Side::Buy, dec!(0.61));
        assert!(matches!(OrderValidator::validate(&o, dec!(5)),
            Err(ValidationError::BuyLimitBelowReference { .. })));

        let o = order(dec!(100), dec!(0.70), dec!(0.01), OrderType::Market, Side::Sell, dec!(0.61));
        assert!(matches!(OrderValidator::validate(&o, dec!(5)),
            Err(ValidationError::SellLimitAboveReference { .. })));
    }

    #[test]
    fn resting_limit_orders_may_sit_away_from_the_reference() {
        // A passive limit buy below the market is perfectly legitimate.
        let o = order(dec!(100), dec!(0.50), dec!(0.01), OrderType::Limit, Side::Buy, dec!(0.61));
        assert!(OrderValidator::validate(&o, dec!(5)).is_ok());
    }

    #[test]
    fn tick_helper_matches_both_common_ticks() {
        assert!(OrderValidator::is_on_tick(Price::new(dec!(0.61)).unwrap(), dec!(0.01)));
        assert!(!OrderValidator::is_on_tick(Price::new(dec!(0.615)).unwrap(), dec!(0.01)));
        assert!(OrderValidator::is_on_tick(Price::new(dec!(0.615)).unwrap(), dec!(0.001)));
    }
}
