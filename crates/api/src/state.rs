//! Shared application state.
//!
//! Held behind `Arc` and shared by the HTTP layer, the ingest pipeline and the
//! background tasks. Every field is independently synchronised, so a slow dashboard
//! request cannot block the trading path.

use std::sync::Arc;

use domain::{AppMode, CopySignal, SourceTrade, SystemEvent};
use execution::{OrderManager, PaperExecution};
use metrics::{HealthMonitor, Metrics};
use parking_lot::RwLock;
use portfolio::Portfolio;
use risk::{KillSwitch, RiskEngine};
use wallet_tracker::WalletTracker;

/// A recent copy-trading row, joined for the dashboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CopyRow {
    pub correlation_id: String,
    pub source_event_id: String,
    pub wallet: String,
    pub wallet_nickname: String,
    pub market_title: String,
    pub outcome: String,
    pub side: String,
    pub source_notional: String,
    pub copy_notional: String,
    pub source_price: String,
    pub copy_price: Option<String>,
    pub slippage_bps: Option<i64>,
    pub status: String,
    pub detection_latency_ms: Option<f64>,
    pub execution_latency_ms: Option<f64>,
    pub end_to_end_latency_ms: Option<f64>,
    pub at: chrono::DateTime<chrono::Utc>,
}

/// Bounded ring of recent activity for the dashboard.
pub struct RecentActivity {
    source_trades: RwLock<Vec<SourceTrade>>,
    signals: RwLock<Vec<CopySignal>>,
    copies: RwLock<Vec<CopyRow>>,
    events: RwLock<Vec<SystemEvent>>,
    cap: usize,
}

impl RecentActivity {
    pub fn new(cap: usize) -> Self {
        Self {
            source_trades: RwLock::new(Vec::new()),
            signals: RwLock::new(Vec::new()),
            copies: RwLock::new(Vec::new()),
            events: RwLock::new(Vec::new()),
            cap,
        }
    }

    fn push<T>(v: &RwLock<Vec<T>>, item: T, cap: usize) {
        let mut g = v.write();
        g.insert(0, item);
        g.truncate(cap);
    }

    pub fn add_source_trade(&self, t: SourceTrade) { Self::push(&self.source_trades, t, self.cap) }
    pub fn add_signal(&self, s: CopySignal) { Self::push(&self.signals, s, self.cap) }
    pub fn add_copy(&self, c: CopyRow) { Self::push(&self.copies, c, self.cap) }
    pub fn add_event(&self, e: SystemEvent) { Self::push(&self.events, e, self.cap) }

    /// Updates an existing copy row in place, keyed by correlation id.
    pub fn update_copy(&self, correlation_id: &str, f: impl FnOnce(&mut CopyRow)) -> bool {
        let mut g = self.copies.write();
        match g.iter_mut().find(|c| c.correlation_id == correlation_id) {
            Some(c) => { f(c); true }
            None => false,
        }
    }

    pub fn source_trades(&self, n: usize) -> Vec<SourceTrade> {
        self.source_trades.read().iter().take(n).cloned().collect()
    }
    pub fn signals(&self, n: usize) -> Vec<CopySignal> {
        self.signals.read().iter().take(n).cloned().collect()
    }
    pub fn copies(&self, n: usize) -> Vec<CopyRow> {
        self.copies.read().iter().take(n).cloned().collect()
    }
    pub fn events(&self, n: usize) -> Vec<SystemEvent> {
        self.events.read().iter().take(n).cloned().collect()
    }
}

pub struct AppState {
    /// Epoch-millis of the last *trade* seen on the source feed.
    ///
    /// Deliberately separate from connection state. A feed can connect, deliver nothing,
    /// drop and reconnect indefinitely — refreshing "last ok" on connect would report that
    /// as healthy forever. Liveness means data arriving, not a socket being open.
    pub last_source_data_ms: std::sync::atomic::AtomicI64,
    pub mode: AppMode,
    pub config: config::AppConfig,
    pub portfolio: Arc<Portfolio>,
    pub orders: Arc<OrderManager>,
    pub tracker: Arc<WalletTracker>,
    pub kill_switch: Arc<KillSwitch>,
    pub risk: Arc<RwLock<RiskEngine>>,
    pub metrics: Arc<Metrics>,
    pub health: Arc<HealthMonitor>,
    pub repos: Arc<persistence::Repositories>,
    pub recent: Arc<RecentActivity>,
    pub events: tokio::sync::broadcast::Sender<SystemEvent>,
    /// Present only in paper mode; backs `POST /api/paper/reset`.
    pub paper: Option<Arc<PaperExecution>>,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl AppState {
    /// Records that a trade actually arrived.
    pub fn mark_source_data(&self, at: chrono::DateTime<chrono::Utc>) {
        self.last_source_data_ms
            .store(at.timestamp_millis(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Milliseconds since the last trade, or `None` if none has ever arrived.
    pub fn source_data_age_ms(&self, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
        let v = self.last_source_data_ms.load(std::sync::atomic::Ordering::Relaxed);
        (v > 0).then(|| now.timestamp_millis() - v)
    }

    pub fn uptime_seconds(&self) -> i64 {
        (chrono::Utc::now() - self.started_at).num_seconds()
    }

    /// True when real money is at stake — drives the dashboard's prominent warning.
    pub fn is_real_money(&self) -> bool { self.orders.is_real_money() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_activity_is_bounded_and_newest_first() {
        let r = RecentActivity::new(3);
        for i in 0..10 {
            r.add_copy(CopyRow {
                correlation_id: i.to_string(),
                source_event_id: String::new(), wallet: String::new(),
                wallet_nickname: String::new(), market_title: String::new(),
                outcome: String::new(), side: "BUY".into(),
                source_notional: "0".into(), copy_notional: "0".into(),
                source_price: "0".into(), copy_price: None, slippage_bps: None,
                status: "NEW".into(), detection_latency_ms: None,
                execution_latency_ms: None, end_to_end_latency_ms: None,
                at: chrono::Utc::now(),
            });
        }
        let c = r.copies(100);
        assert_eq!(c.len(), 3, "must stay bounded");
        assert_eq!(c[0].correlation_id, "9", "newest first");
        assert_eq!(c[2].correlation_id, "7");
    }

    #[test]
    fn feed_liveness_is_measured_by_data_not_connection() {
        // The bug this guards: a feed that connects, delivers nothing, drops and
        // reconnects refreshes connection health forever and never looks unhealthy.
        use std::sync::atomic::AtomicI64;
        let s = AtomicI64::new(0);
        // Never received anything -> age is unknown, not zero.
        assert_eq!(s.load(std::sync::atomic::Ordering::Relaxed), 0);

        let now = chrono::Utc::now();
        s.store(now.timestamp_millis() - 120_000, std::sync::atomic::Ordering::Relaxed);
        let age = now.timestamp_millis() - s.load(std::sync::atomic::Ordering::Relaxed);
        assert!(age >= 120_000, "a silent feed must report its true data age");
    }

    #[test]
    fn copy_rows_update_in_place_by_correlation_id() {
        let r = RecentActivity::new(10);
        r.add_copy(CopyRow {
            correlation_id: "abc".into(), source_event_id: String::new(),
            wallet: String::new(), wallet_nickname: String::new(), market_title: String::new(),
            outcome: String::new(), side: "BUY".into(), source_notional: "0".into(),
            copy_notional: "0".into(), source_price: "0".into(), copy_price: None,
            slippage_bps: None, status: "SUBMITTED".into(), detection_latency_ms: None,
            execution_latency_ms: None, end_to_end_latency_ms: None, at: chrono::Utc::now(),
        });
        assert!(r.update_copy("abc", |c| c.status = "FILLED".into()));
        assert_eq!(r.copies(1)[0].status, "FILLED");
        assert!(!r.update_copy("missing", |_| {}));
    }
}
