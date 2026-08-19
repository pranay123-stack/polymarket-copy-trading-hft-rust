//! The copy-trading strategy and its sizing rules.
//!
//! This crate is pure and synchronous: given a source trade, a wallet configuration and
//! a market snapshot, it returns a signal or an explicit refusal. It performs no I/O and
//! has no idea whether the resulting order will be simulated or sent to the real venue.

pub mod copy_trader;
pub mod sizing;

pub use copy_trader::{achievable_price, CopyTrader, SignalRefusal, StrategyConfig};
pub use sizing::{SizedOrder, SizingContext, SizingEngine, SizingRefusal};
