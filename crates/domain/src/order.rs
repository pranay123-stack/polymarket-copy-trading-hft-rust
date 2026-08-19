//! Orders and the order state machine.
//!
//! Order state is an enum with *validated transitions*, not a string. Illegal
//! transitions (a filled order going back to submitted, a cancelled order filling)
//! are rejected by [`OrderState::can_transition_to`] rather than silently corrupting
//! the position, which is the usual way a trading system loses track of its exposure.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ids::{CorrelationId, MarketId, OrderId, SignalId, TokenId};
use crate::latency::LatencyStamps;
use crate::money::{Price, Qty, Side, Usd};

/// Lifecycle state of one of *our* orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderState {
    /// Constructed in memory, not yet risk-checked.
    Created,
    /// Passed every pre-trade risk check.
    Validated,
    /// Handed to the execution adapter; no venue response yet.
    Submitted,
    /// Venue accepted it and it is working.
    Acknowledged,
    PartiallyFilled,
    Filled,
    CancelRequested,
    Cancelled,
    /// Venue rejected it (risk, price, size, auth).
    Rejected,
    /// We failed to submit it (network, serialisation, timeout with no ack).
    Failed,
    /// Terminal-ambiguous: we submitted but cannot determine the outcome.
    /// Reconciliation must resolve this; it is never treated as "no position".
    Unknown,
}

impl OrderState {
    /// No further transitions are possible.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Filled | Self::Cancelled | Self::Rejected | Self::Failed)
    }

    /// Still consuming risk budget / open-order slots.
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            Self::Created | Self::Validated | Self::Submitted | Self::Acknowledged
                | Self::PartiallyFilled | Self::CancelRequested | Self::Unknown
        )
    }

    /// Could this order have executed at the venue? Drives reconciliation: anything
    /// that might have traded must be reconciled against the exchange.
    pub fn may_have_executed(&self) -> bool {
        matches!(
            self,
            Self::Submitted | Self::Acknowledged | Self::PartiallyFilled
                | Self::Filled | Self::CancelRequested | Self::Unknown
        )
    }

    pub fn can_transition_to(&self, next: OrderState) -> bool {
        use OrderState::*;
        match (self, next) {
            // Nothing leaves a terminal state.
            (s, _) if s.is_terminal() => false,
            // Any non-terminal state can degrade into Unknown (timeout, lost connection).
            (_, Unknown) => true,
            // Unknown is resolved by reconciliation into any concrete outcome.
            (Unknown, n) => matches!(
                n, Acknowledged | PartiallyFilled | Filled | Cancelled | Rejected | Failed
            ),
            (Created, Validated | Rejected | Failed) => true,
            (Validated, Submitted | Rejected | Failed) => true,
            (Submitted, Acknowledged | PartiallyFilled | Filled | Rejected | Failed | Cancelled) => true,
            (Acknowledged, PartiallyFilled | Filled | CancelRequested | Cancelled | Rejected) => true,
            (PartiallyFilled, PartiallyFilled | Filled | CancelRequested | Cancelled) => true,
            (CancelRequested, Cancelled | Filled | PartiallyFilled) => true,
            _ => false,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Validated => "VALIDATED",
            Self::Submitted => "SUBMITTED",
            Self::Acknowledged => "ACKNOWLEDGED",
            Self::PartiallyFilled => "PARTIALLY_FILLED",
            Self::Filled => "FILLED",
            Self::CancelRequested => "CANCEL_REQUESTED",
            Self::Cancelled => "CANCELLED",
            Self::Rejected => "REJECTED",
            Self::Failed => "FAILED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

impl std::fmt::Display for OrderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OrderError {
    #[error("illegal order transition {from} -> {to}")]
    IllegalTransition { from: OrderState, to: OrderState },
    #[error("fill of {fill} would exceed remaining {remaining} on order {order}")]
    Overfill { order: OrderId, fill: Qty, remaining: Qty },
    #[error("order quantity must be greater than zero")]
    ZeroQuantity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    /// Executes against resting liquidity immediately. Still carries a protective
    /// limit derived from the slippage budget — we never send a truly unbounded order.
    Market,
    /// Rests at `price`.
    Limit,
}

/// How long an order stays working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimeInForce {
    /// Good til cancelled.
    Gtc,
    /// Immediate-or-cancel: take what is available now, cancel the rest.
    Ioc,
    /// Fill-or-kill: all now, or nothing.
    Fok,
}

/// What the strategy asks for. Contains no venue-specific fields — the adapter
/// translates it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub order_id: OrderId,
    pub correlation_id: CorrelationId,
    /// Present when the order originates from a copy signal; absent for manual orders.
    pub signal_id: Option<SignalId>,
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub side: Side,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub quantity: Qty,
    /// For `Limit`, the resting price. For `Market`, the protective worst-acceptable
    /// price computed from the slippage budget.
    pub limit_price: Price,
    /// The price the *source* trader got — the benchmark slippage is measured against.
    pub reference_price: Price,
    pub tick_size: Decimal,
    pub created_at: DateTime<Utc>,
}

impl OrderRequest {
    pub fn notional(&self) -> Usd { self.quantity.notional(self.limit_price) }
}

/// A live order plus everything needed to audit it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub request: OrderRequest,
    pub state: OrderState,
    /// The venue's own id, once known. `None` in paper mode until the sim assigns one.
    pub venue_order_id: Option<String>,
    pub filled_qty: Qty,
    /// Notional actually transacted, used to derive the true average fill price.
    pub filled_notional: Usd,
    pub fees_paid: Usd,
    pub reject_reason: Option<String>,
    pub latency: LatencyStamps,
    pub updated_at: DateTime<Utc>,
}

impl Order {
    pub fn new(request: OrderRequest, latency: LatencyStamps) -> Result<Self, OrderError> {
        if request.quantity.is_zero() {
            return Err(OrderError::ZeroQuantity);
        }
        let now = request.created_at;
        Ok(Self {
            request,
            state: OrderState::Created,
            venue_order_id: None,
            filled_qty: Qty::ZERO,
            filled_notional: Usd::ZERO,
            fees_paid: Usd::ZERO,
            reject_reason: None,
            latency,
            updated_at: now,
        })
    }

    pub fn id(&self) -> OrderId { self.request.order_id }
    pub fn remaining(&self) -> Qty { self.request.quantity.saturating_sub(self.filled_qty) }

    /// True average fill price: total notional / total filled quantity.
    /// `None` before the first fill (rather than a misleading zero).
    pub fn avg_fill_price(&self) -> Option<Price> {
        if self.filled_qty.is_zero() { return None; }
        Price::new(self.filled_notional.get() / self.filled_qty.get()).ok()
    }

    pub fn transition(&mut self, next: OrderState, at: DateTime<Utc>) -> Result<(), OrderError> {
        if !self.state.can_transition_to(next) {
            return Err(OrderError::IllegalTransition { from: self.state, to: next });
        }
        self.state = next;
        self.updated_at = at;
        Ok(())
    }

    /// Applies a fill, moving to `PartiallyFilled` or `Filled` as appropriate.
    ///
    /// Rejects, without mutating anything:
    ///
    /// * fills on an order that was never submitted (`Created`/`Validated`) or that has
    ///   already reached a terminal state — a fill there means our view of the order and
    ///   the venue's have diverged, and quietly booking the quantity would corrupt the
    ///   position while leaving the state machine inconsistent;
    /// * overfills, where a venue reports more filled than we asked for.
    ///
    /// Both are integrity failures that must surface, not be absorbed.
    pub fn apply_fill(
        &mut self,
        qty: Qty,
        price: Price,
        fee: Usd,
        at: DateTime<Utc>,
    ) -> Result<(), OrderError> {
        if !self.state.may_have_executed() {
            return Err(OrderError::IllegalTransition {
                from: self.state,
                to: OrderState::PartiallyFilled,
            });
        }
        if qty > self.remaining() {
            return Err(OrderError::Overfill {
                order: self.id(),
                fill: qty,
                remaining: self.remaining(),
            });
        }
        self.filled_qty += qty;
        self.filled_notional += qty.notional(price);
        self.fees_paid += fee;
        let next = if self.remaining().is_zero() {
            OrderState::Filled
        } else {
            OrderState::PartiallyFilled
        };
        // Legal from every state that reaches here, including CancelRequested (a fill
        // racing our cancel) and Unknown (reconciliation discovering a fill).
        self.state = next;
        self.updated_at = at;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::OrderState::*;
    use super::*;
    use crate::latency::LatencyStamps;
    use rust_decimal_macros::dec;

    fn req(qty: Decimal) -> OrderRequest {
        OrderRequest {
            order_id: OrderId::new(),
            correlation_id: CorrelationId::new(),
            signal_id: None,
            market_id: MarketId::new(
                "0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52").unwrap(),
            token_id: TokenId::new("72551024098258542594534683942523606143014690620243023298497729957846870197074").unwrap(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            quantity: Qty::new(qty).unwrap(),
            limit_price: Price::new(dec!(0.61)).unwrap(),
            reference_price: Price::new(dec!(0.61)).unwrap(),
            tick_size: dec!(0.01),
            created_at: Utc::now(),
        }
    }

    fn order(qty: Decimal) -> Order {
        Order::new(req(qty), LatencyStamps::begin(Utc::now())).unwrap()
    }

    #[test]
    fn terminal_states_are_absorbing() {
        for t in [Filled, Cancelled, Rejected, Failed] {
            assert!(t.is_terminal());
            for n in [Created, Validated, Submitted, Acknowledged, PartiallyFilled, Unknown] {
                assert!(!t.can_transition_to(n), "{t} must not reach {n}");
            }
        }
    }

    #[test]
    fn happy_path_transitions_are_allowed() {
        assert!(Created.can_transition_to(Validated));
        assert!(Validated.can_transition_to(Submitted));
        assert!(Submitted.can_transition_to(Acknowledged));
        assert!(Acknowledged.can_transition_to(PartiallyFilled));
        assert!(PartiallyFilled.can_transition_to(Filled));
    }

    #[test]
    fn skipping_risk_validation_is_impossible() {
        // The whole point of the state machine: an order cannot reach the venue
        // without passing through Validated.
        assert!(!Created.can_transition_to(Submitted));
        assert!(!Created.can_transition_to(Acknowledged));
        assert!(!Created.can_transition_to(Filled));
    }

    #[test]
    fn unknown_is_reachable_and_resolvable() {
        for s in [Submitted, Acknowledged, PartiallyFilled, CancelRequested] {
            assert!(s.can_transition_to(Unknown), "{s} must degrade to Unknown on timeout");
        }
        assert!(Unknown.can_transition_to(Filled));
        assert!(Unknown.can_transition_to(Cancelled));
        // Unknown must never be assumed to be "nothing happened".
        assert!(Unknown.may_have_executed());
    }

    #[test]
    fn fill_race_after_cancel_request_is_accepted() {
        let mut o = working(dec!(100));
        o.transition(CancelRequested, Utc::now()).unwrap();
        // Venue filled it just before our cancel landed.
        o.apply_fill(Qty::new(dec!(100)).unwrap(), Price::new(dec!(0.61)).unwrap(), Usd::ZERO, Utc::now()).unwrap();
        assert_eq!(o.state, Filled);
    }

    #[test]
    fn overfill_is_rejected_not_absorbed() {
        let mut o = working(dec!(100));
        let p = Price::new(dec!(0.6)).unwrap();
        o.apply_fill(Qty::new(dec!(60)).unwrap(), p, Usd::ZERO, Utc::now()).unwrap();
        let err = o.apply_fill(Qty::new(dec!(41)).unwrap(), p, Usd::ZERO, Utc::now()).unwrap_err();
        assert!(matches!(err, OrderError::Overfill { .. }));
        assert_eq!(o.filled_qty.get(), dec!(60), "rejected fill must not mutate state");
    }

    /// Drives an order to a working state the way the OMS does.
    fn working(qty: Decimal) -> Order {
        let mut o = order(qty);
        o.transition(Validated, Utc::now()).unwrap();
        o.transition(Submitted, Utc::now()).unwrap();
        o.transition(Acknowledged, Utc::now()).unwrap();
        o
    }

    #[test]
    fn fill_on_unsubmitted_order_is_refused_without_mutating() {
        let mut o = order(dec!(100));
        let err = o.apply_fill(Qty::new(dec!(10)).unwrap(), Price::new(dec!(0.6)).unwrap(), Usd::ZERO, Utc::now())
            .unwrap_err();
        assert!(matches!(err, OrderError::IllegalTransition { from: Created, .. }));
        assert_eq!(o.filled_qty, Qty::ZERO, "refused fill must leave quantities untouched");
        assert_eq!(o.state, Created);
    }

    #[test]
    fn fill_on_terminal_order_is_refused() {
        let mut o = working(dec!(100));
        o.transition(Cancelled, Utc::now()).unwrap();
        assert!(o.apply_fill(Qty::new(dec!(1)).unwrap(), Price::new(dec!(0.6)).unwrap(), Usd::ZERO, Utc::now()).is_err());
    }

    #[test]
    fn avg_fill_price_is_notional_weighted() {
        let mut o = working(dec!(100));
        o.apply_fill(Qty::new(dec!(50)).unwrap(), Price::new(dec!(0.60)).unwrap(), Usd::ZERO, Utc::now()).unwrap();
        assert_eq!(o.state, PartiallyFilled);
        o.apply_fill(Qty::new(dec!(50)).unwrap(), Price::new(dec!(0.70)).unwrap(), Usd::ZERO, Utc::now()).unwrap();
        assert_eq!(o.state, Filled);
        assert_eq!(o.avg_fill_price().unwrap().get(), dec!(0.65));
        assert_eq!(o.remaining(), Qty::ZERO);
    }

    #[test]
    fn avg_price_is_none_before_any_fill() {
        assert!(order(dec!(100)).avg_fill_price().is_none());
    }

    #[test]
    fn zero_quantity_order_is_rejected_at_construction() {
        let mut r = req(dec!(1));
        r.quantity = Qty::ZERO;
        assert_eq!(Order::new(r, LatencyStamps::begin(Utc::now())).unwrap_err(), OrderError::ZeroQuantity);
    }
}
