//! Component health tracking.

use chrono::{DateTime, Duration, Utc};
use domain::HealthState;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentStatus {
    pub name: String,
    pub state: HealthState,
    pub detail: String,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_change: DateTime<Utc>,
}

/// Overall system health, assembled from components.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub state: HealthState,
    pub components: Vec<ComponentStatus>,
    pub mode: String,
    pub at: DateTime<Utc>,
}

impl HealthReport {
    pub fn is_healthy(&self) -> bool { self.state == HealthState::Healthy }
}

pub struct HealthMonitor {
    components: RwLock<Vec<ComponentStatus>>,
    mode: String,
}

impl HealthMonitor {
    pub fn new(mode: impl Into<String>) -> Self {
        Self { components: RwLock::new(Vec::new()), mode: mode.into() }
    }

    pub fn set(&self, name: &str, state: HealthState, detail: impl Into<String>) {
        let now = Utc::now();
        let mut g = self.components.write();
        match g.iter_mut().find(|c| c.name == name) {
            Some(c) => {
                if c.state != state {
                    c.last_change = now;
                }
                c.state = state;
                c.detail = detail.into();
                if state == HealthState::Healthy { c.last_ok = Some(now); }
            }
            None => g.push(ComponentStatus {
                name: name.to_string(),
                state,
                detail: detail.into(),
                last_ok: (state == HealthState::Healthy).then_some(now),
                last_change: now,
            }),
        }
    }

    pub fn get(&self, name: &str) -> Option<ComponentStatus> {
        self.components.read().iter().find(|c| c.name == name).cloned()
    }

    /// Marks a component degraded if it has not been healthy within `stale_after`.
    /// A feed that stopped delivering without disconnecting is otherwise invisible.
    pub fn expire_stale(&self, stale_after: Duration, now: DateTime<Utc>) {
        let mut g = self.components.write();
        for c in g.iter_mut() {
            if c.state == HealthState::Healthy {
                if let Some(ok) = c.last_ok {
                    if now - ok > stale_after {
                        c.state = HealthState::Degraded;
                        c.detail = format!("no update for {}s", (now - ok).num_seconds());
                        c.last_change = now;
                    }
                }
            }
        }
    }

    pub fn report(&self) -> HealthReport {
        let components = self.components.read().clone();
        // The worst component determines the whole.
        let state = if components.iter().any(|c| c.state == HealthState::Down) {
            HealthState::Down
        } else if components.iter().any(|c| c.state == HealthState::Degraded) {
            HealthState::Degraded
        } else {
            HealthState::Healthy
        };
        HealthReport { state, components, mode: self.mode.clone(), at: Utc::now() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_monitor_is_healthy() {
        assert!(HealthMonitor::new("PAPER").report().is_healthy());
    }

    #[test]
    fn worst_component_determines_overall_state() {
        let h = HealthMonitor::new("PAPER");
        h.set("db", HealthState::Healthy, "ok");
        h.set("feed", HealthState::Degraded, "reconnecting");
        assert_eq!(h.report().state, HealthState::Degraded);
        h.set("execution", HealthState::Down, "adapter not ready");
        assert_eq!(h.report().state, HealthState::Down, "Down must dominate Degraded");
    }

    #[test]
    fn recovery_returns_to_healthy() {
        let h = HealthMonitor::new("PAPER");
        h.set("feed", HealthState::Down, "disconnected");
        assert_eq!(h.report().state, HealthState::Down);
        h.set("feed", HealthState::Healthy, "connected");
        assert!(h.report().is_healthy());
        assert!(h.get("feed").unwrap().last_ok.is_some());
    }

    #[test]
    fn silent_feeds_are_caught_by_staleness() {
        // A feed that stops delivering without disconnecting is the dangerous case.
        let h = HealthMonitor::new("PAPER");
        h.set("feed", HealthState::Healthy, "connected");
        h.expire_stale(Duration::seconds(30), Utc::now() + Duration::seconds(120));
        let c = h.get("feed").unwrap();
        assert_eq!(c.state, HealthState::Degraded);
        assert!(c.detail.contains("no update"));
    }

    #[test]
    fn fresh_components_survive_expiry() {
        let h = HealthMonitor::new("PAPER");
        h.set("feed", HealthState::Healthy, "connected");
        h.expire_stale(Duration::seconds(30), Utc::now());
        assert!(h.report().is_healthy());
    }

    #[test]
    fn state_change_timestamps_only_move_on_change() {
        let h = HealthMonitor::new("PAPER");
        h.set("db", HealthState::Healthy, "ok");
        let t1 = h.get("db").unwrap().last_change;
        std::thread::sleep(std::time::Duration::from_millis(5));
        h.set("db", HealthState::Healthy, "still ok");
        assert_eq!(h.get("db").unwrap().last_change, t1, "an unchanged state must not re-stamp");
        h.set("db", HealthState::Down, "gone");
        assert!(h.get("db").unwrap().last_change > t1);
    }
}
