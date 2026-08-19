//! HTTP API and dashboard WebSocket.
//!
//! Read endpoints are unauthenticated so the dashboard works with zero setup in paper
//! mode; every mutating endpoint is behind a bearer token, and live mode refuses to
//! start without one configured.

pub mod auth;
pub mod handlers;
pub mod routes;
pub mod state;
pub mod websocket;

pub use routes::{router, ROUTES};
pub use state::{AppState, CopyRow, RecentActivity};
