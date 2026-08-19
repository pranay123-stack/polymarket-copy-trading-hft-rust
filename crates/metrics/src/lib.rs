//! Metrics, latency statistics and health.
//!
//! Every number here derives from a real observation. Stages that were never measured
//! report nothing rather than a zero, and that distinction is preserved all the way to
//! the Prometheus output and the dashboard.

pub mod health;
pub mod latency;
pub mod registry;

pub use health::{ComponentStatus, HealthMonitor, HealthReport};
pub use latency::{LatencyRecorder, LatencyStats};
pub use registry::{Counter, Gauge, Metrics};
