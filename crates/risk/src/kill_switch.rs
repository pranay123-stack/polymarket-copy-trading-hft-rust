//! Emergency kill switch.
//!
//! Enforced in the **backend**, inside the risk engine, on the path every order must
//! traverse. It is deliberately not a dashboard concern: the UI can trigger it and
//! display it, but disabling or bypassing the UI must not re-enable trading.
//!
//! Engaging is always permitted and never fails. Disengaging is a separate, explicit
//! action that records who did it — the asymmetry is intentional.

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillSwitchState {
    pub engaged: bool,
    pub reason: Option<String>,
    pub engaged_by: Option<String>,
    pub engaged_at: Option<DateTime<Utc>>,
    /// Whether engaging should also try to cancel resting orders.
    pub cancel_open_orders: bool,
    /// How many times it has been engaged this process lifetime.
    pub activations: u64,
}

impl Default for KillSwitchState {
    fn default() -> Self {
        Self {
            engaged: false,
            reason: None,
            engaged_by: None,
            engaged_at: None,
            cancel_open_orders: true,
            activations: 0,
        }
    }
}

/// Thread-safe kill switch shared by every component.
#[derive(Default)]
pub struct KillSwitch {
    state: RwLock<KillSwitchState>,
}

impl KillSwitch {
    pub fn new() -> Self { Self::default() }

    /// Starts already engaged — used when startup reconciliation finds a problem.
    pub fn engaged(reason: impl Into<String>) -> Self {
        let k = Self::default();
        k.engage(reason, "startup");
        k
    }

    #[inline]
    pub fn is_engaged(&self) -> bool { self.state.read().engaged }

    pub fn state(&self) -> KillSwitchState { self.state.read().clone() }

    pub fn reason(&self) -> Option<String> { self.state.read().reason.clone() }

    /// Halts new orders. Idempotent: re-engaging keeps the original reason, since the
    /// first cause is the diagnostically useful one.
    pub fn engage(&self, reason: impl Into<String>, by: impl Into<String>) -> KillSwitchState {
        let mut s = self.state.write();
        if !s.engaged {
            s.engaged = true;
            s.reason = Some(reason.into());
            s.engaged_by = Some(by.into());
            s.engaged_at = Some(Utc::now());
            s.activations += 1;
        }
        s.clone()
    }

    /// Resumes trading. Always explicit and attributed.
    pub fn reset(&self, by: impl Into<String>) -> KillSwitchState {
        let mut s = self.state.write();
        s.engaged = false;
        s.reason = None;
        s.engaged_by = Some(by.into());
        s.engaged_at = None;
        s.clone()
    }

    pub fn set_cancel_open_orders(&self, v: bool) { self.state.write().cancel_open_orders = v; }
    pub fn should_cancel_open_orders(&self) -> bool { self.state.read().cancel_open_orders }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_disengaged() {
        let k = KillSwitch::new();
        assert!(!k.is_engaged());
        assert_eq!(k.state().activations, 0);
    }

    #[test]
    fn engaging_records_who_and_why() {
        let k = KillSwitch::new();
        k.engage("daily loss breached", "risk-engine");
        let s = k.state();
        assert!(s.engaged);
        assert_eq!(s.reason.as_deref(), Some("daily loss breached"));
        assert_eq!(s.engaged_by.as_deref(), Some("risk-engine"));
        assert!(s.engaged_at.is_some());
        assert_eq!(s.activations, 1);
    }

    #[test]
    fn re_engaging_preserves_the_original_cause() {
        let k = KillSwitch::new();
        k.engage("first cause", "a");
        k.engage("second cause", "b");
        // The first reason is the diagnostically useful one.
        assert_eq!(k.reason().as_deref(), Some("first cause"));
        assert_eq!(k.state().activations, 1, "re-engaging is not a new activation");
    }

    #[test]
    fn reset_requires_attribution_and_clears_state() {
        let k = KillSwitch::new();
        k.engage("x", "risk");
        let s = k.reset("operator@api");
        assert!(!s.engaged);
        assert!(s.reason.is_none());
        assert_eq!(s.engaged_by.as_deref(), Some("operator@api"));
    }

    #[test]
    fn engage_reset_engage_counts_both_activations() {
        let k = KillSwitch::new();
        k.engage("a", "x");
        k.reset("x");
        k.engage("b", "x");
        assert_eq!(k.state().activations, 2);
        assert_eq!(k.reason().as_deref(), Some("b"));
    }

    #[test]
    fn is_shareable_across_threads() {
        use std::sync::Arc;
        let k = Arc::new(KillSwitch::new());
        let mut hs = vec![];
        for i in 0..8 {
            let k = k.clone();
            hs.push(std::thread::spawn(move || {
                if i == 0 { k.engage("halt", "t0"); }
                k.is_engaged()
            }));
        }
        for h in hs { let _ = h.join().unwrap(); }
        assert!(k.is_engaged());
        assert_eq!(k.state().activations, 1, "concurrent engages must not double-count");
    }
}
