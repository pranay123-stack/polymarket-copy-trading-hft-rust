//! Core domain model for the Polymarket copy-trading system.
//!
//! This crate is deliberately dependency-light and contains **no I/O**. Everything here
//! is pure data plus the invariants that protect it, so the rules that matter — order
//! state transitions, position accounting, price validity — can be tested without a
//! network, a database or a clock.
//!
//! Design rules enforced here:
//!
//! * **No `f64` arithmetic on money.** Floats appear only at the ingest boundary
//!   (`Price::from_feed_f64`) because Polymarket sends JSON numbers, and are converted
//!   via their shortest round-trip string form.
//! * **Direction is a [`Side`], never the sign of a quantity.** [`Qty`] cannot be negative.
//! * **Order state is a validated enum**, not a string; illegal transitions are errors.
//! * **Identifiers are distinct types.** `conditionId`, `asset` and `transactionHash`
//!   are all opaque strings on the wire and trivially confusable as `String`.
//! * **Absent measurements are `None`**, never a plausible zero.

pub mod events;
pub mod ids;
pub mod latency;
pub mod market;
pub mod money;
pub mod order;
pub mod pnl;
pub mod position;
pub mod trade;
pub mod wallet;

pub use events::{AppMode, ComponentHealth, HealthState, RiskRejection, SystemEvent};
pub use ids::{
    Address, CorrelationId, FillId, IdError, MarketId, OrderId, SignalId, SourceEventId, TokenId,
    TxHash,
};
pub use latency::{LatencySample, LatencyStage, LatencyStamps};
pub use market::{Level, Market, OrderBook, Outcome};
pub use money::{Bps, MoneyError, Price, Qty, Side, Usd};
pub use order::{Order, OrderError, OrderRequest, OrderState, OrderType, TimeInForce};
pub use pnl::PnlSnapshot;
pub use position::Position;
pub use trade::{CopySignal, Fill, SignalSkipReason, SourceTrade, TradeSource};
pub use wallet::{SizingMode, TargetWallet};

/// Re-exported so downstream crates share one `Decimal` type.
pub use rust_decimal::Decimal;
pub use rust_decimal_macros::dec;
