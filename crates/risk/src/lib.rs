//! Pre-trade risk control.
//!
//! Nothing reaches an execution adapter without passing [`engine::RiskEngine::check`].
//! The order state machine makes that structural rather than conventional: `Submitted`
//! is only reachable from `Validated`, and only this crate issues that verdict.

pub mod engine;
pub mod kill_switch;
pub mod limits;
pub mod validation;

pub use engine::{DailyRiskBudget, RiskEngine, RiskSnapshot, RiskVerdict, SystemStatus};
pub use kill_switch::{KillSwitch, KillSwitchState};
pub use limits::RiskLimits;
pub use validation::{OrderValidator, ValidationError};
