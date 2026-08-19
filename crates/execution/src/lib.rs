//! Execution: the boundary between strategy and the outside world.
//!
//! [`ExecutionAdapter`] is the mandated seam. Strategy, risk, the order manager and the
//! portfolio are identical across paper, replay and live; only the implementation behind
//! this trait changes. See [`adapter`] for why that matters and what implementations owe
//! their callers.

pub mod adapter;
pub mod live;
pub mod order_manager;
pub mod paper;
pub mod reconciliation;
pub mod signing;

pub use adapter::{
    Acknowledgement, AdapterCapabilities, ExecutionAdapter, ExecutionError, VenuePosition,
};
pub use live::{L2Credentials, LiveExecution, OrderSigner, SignedOrder};
pub use order_manager::{OrderManager, SubmitOutcome};
pub use paper::{BookCache, PaperExecution};
pub use reconciliation::{Mismatch, ReconciliationReport, Reconciler};
pub use signing::{
    domain_separator, EoaSigner, PolymarketOrder, SignatureType, SigningError, CHAIN_ID,
    EXCHANGE_ADDRESS, ORDER_TYPE,
};
