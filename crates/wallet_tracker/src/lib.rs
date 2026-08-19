//! Target-wallet tracking and source-event idempotency.
//!
//! This crate answers two questions for every frame on the firehose:
//! *is this one of our traders?* and *have we already acted on this exact fill?*
//!
//! The second question is harder than it sounds — Polymarket publishes no unique
//! identifier for a fill, and genuinely emits byte-identical rows. See [`dedup`] for the
//! measurement and the resolution.

pub mod dedup;
pub mod tracker;

pub use dedup::{
    content_key, BatchOrdinals, ContentKey, DedupIndex, DedupVerdict, DEFAULT_RETENTION_HOURS,
};
pub use tracker::{Detection, TrackerStats, WalletTracker};
