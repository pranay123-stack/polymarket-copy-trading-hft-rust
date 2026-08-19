//! Durable state: Postgres persistence and crash recovery.
//!
//! Two design commitments:
//!
//! 1. **Duplicate protection is enforced by the database, not only in memory.**
//!    `source_events.event_id` is the primary key and the content tuple carries its own
//!    UNIQUE constraint, so a restart, a race between the live feed and backfill, or a
//!    bug in the in-memory index all surface as a constraint violation rather than a
//!    duplicate order. See `migrations/0001_init.sql`.
//! 2. **The system runs without a database.** In ephemeral mode every repository call is
//!    a no-op and the app keeps trading — losing durable audit is a degraded state, not a
//!    reason to halt a live book. Crash recovery is naturally unavailable there, and the
//!    health report says so rather than implying otherwise.
//!
//! Queries are runtime (`sqlx::query`) rather than compile-time-checked macros, so the
//! workspace builds and tests without a live database.

pub mod repositories;
pub mod store;

pub use repositories::{RecoveredState, Repositories};
pub use store::{Store, StoreError};
