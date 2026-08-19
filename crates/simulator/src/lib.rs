//! Realistic paper-execution simulation.
//!
//! Fills are simulated against the **same order books the live strategy prices
//! against**, not against an idealised instant-fill assumption. See [`matching`] for the
//! model and its deliberately pessimistic defaults.

pub mod matching;

pub use matching::{MatchParams, MatchingEngine, SimOutcome};
