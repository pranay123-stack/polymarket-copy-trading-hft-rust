//! The execution abstraction.
//!
//! **This trait is the seam between the strategy and reality.** Everything above it —
//! signal generation, sizing, risk, the order manager, portfolio accounting — is
//! identical in paper, replay and live. Only the implementation behind this trait
//! differs.
//!
//! That is not merely tidy: it is what makes paper results meaningful. A paper fill
//! travelled the same code path, through the same risk checks, in the same state
//! machine, as a live one would have.
//!
//! Implementations must:
//!
//! * be **async and cancellation-safe** — a dropped future must not leave a phantom order;
//! * report **acknowledgement separately from fills**, since the two have different
//!   latencies and different failure modes;
//! * never silently swallow an error: an ambiguous outcome is reported as
//!   [`ExecutionError::Ambiguous`] so the order can enter `UNKNOWN` and be reconciled,
//!   rather than being assumed dead.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain::{Fill, OrderId, OrderRequest, Qty, TokenId};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("venue rejected the order: {0}")]
    Rejected(String),
    #[error("transport failure: {0}")]
    Transport(String),
    #[error("not authenticated: {0}")]
    Auth(String),
    #[error("execution adapter is not ready: {0}")]
    NotReady(String),
    /// We may or may not have an order at the venue. **Never treat this as a no-op.**
    #[error("ambiguous outcome, reconciliation required: {0}")]
    Ambiguous(String),
    #[error("order {0} is unknown to this adapter")]
    UnknownOrder(OrderId),
    #[error("operation unsupported by this adapter: {0}")]
    Unsupported(String),
}

impl ExecutionError {
    /// Does this outcome leave a possible order at the venue?
    pub fn requires_reconciliation(&self) -> bool {
        matches!(self, Self::Ambiguous(_) | Self::Transport(_))
    }
}

/// The venue accepted the order.
#[derive(Debug, Clone, PartialEq)]
pub struct Acknowledgement {
    pub order_id: OrderId,
    /// The venue's identifier, when it supplies one.
    pub venue_order_id: Option<String>,
    pub accepted_at: DateTime<Utc>,
    /// Fills that came back on the same round trip (an immediately marketable order).
    pub immediate_fills: Vec<Fill>,
    /// True when the venue considers the order finished already.
    pub terminal: bool,
}

/// The venue's view of one of our positions, for reconciliation.
#[derive(Debug, Clone, PartialEq)]
pub struct VenuePosition {
    pub token_id: TokenId,
    pub quantity: Qty,
}

/// What an adapter can do, and how it identifies itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterCapabilities {
    pub supports_cancel: bool,
    pub supports_position_query: bool,
    /// True for adapters that touch real money. Used for prominent UI warnings and to
    /// make "am I actually live?" answerable from one place.
    pub is_real_money: bool,
}

/// The seam. Implemented by `PaperExecution` and `LiveExecution`.
#[async_trait]
pub trait ExecutionAdapter: Send + Sync {
    /// Stable name for logs, metrics and the dashboard ("paper", "live").
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> AdapterCapabilities;

    /// Is the adapter able to accept orders right now?
    async fn is_ready(&self) -> bool;

    /// Submits an order. Returns once the venue has acknowledged it.
    async fn submit(&self, order: &OrderRequest) -> Result<Acknowledgement, ExecutionError>;

    /// Requests cancellation. Success means the request was accepted, not that the
    /// order is definitely cancelled — a fill may still race it.
    async fn cancel(&self, order_id: OrderId, venue_order_id: Option<&str>) -> Result<(), ExecutionError>;

    /// The venue's own position view, for reconciliation against ours.
    async fn positions(&self) -> Result<Vec<VenuePosition>, ExecutionError>;

    /// Fills that have occurred since the last poll. Adapters with a push feed may
    /// return an empty vector and deliver fills through their event channel instead.
    async fn poll_fills(&self) -> Result<Vec<Fill>, ExecutionError> { Ok(Vec::new()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguity_and_transport_failures_demand_reconciliation() {
        // The dangerous case: assuming "no response" means "no order".
        assert!(ExecutionError::Ambiguous("timeout after send".into()).requires_reconciliation());
        assert!(ExecutionError::Transport("connection reset".into()).requires_reconciliation());
        // A clean rejection is unambiguous — nothing exists at the venue.
        assert!(!ExecutionError::Rejected("price off tick".into()).requires_reconciliation());
        assert!(!ExecutionError::Auth("bad key".into()).requires_reconciliation());
    }
}
