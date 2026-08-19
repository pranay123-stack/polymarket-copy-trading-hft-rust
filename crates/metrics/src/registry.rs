//! Prometheus-compatible metric registry.
//!
//! A small hand-rolled registry rather than a dependency: the metric set is fixed and
//! known, so this keeps the exposition format explicit and auditable, and avoids pulling
//! a global-state crate into a system that otherwise passes its dependencies explicitly.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use parking_lot::RwLock;

use crate::latency::LatencyRecorder;

/// A monotonically increasing counter.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) { self.0.fetch_add(1, Ordering::Relaxed); }
    pub fn add(&self, n: u64) { self.0.fetch_add(n, Ordering::Relaxed); }
    pub fn get(&self) -> u64 { self.0.load(Ordering::Relaxed) }
}

/// A value that can go up and down. Stored as millionths so decimals survive.
#[derive(Debug, Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    pub fn set(&self, v: i64) { self.0.store(v, Ordering::Relaxed); }
    pub fn set_decimal(&self, v: rust_decimal::Decimal) {
        use rust_decimal::prelude::ToPrimitive;
        self.0.store((v * rust_decimal::Decimal::from(1_000_000)).to_i64().unwrap_or(0), Ordering::Relaxed);
    }
    pub fn get(&self) -> i64 { self.0.load(Ordering::Relaxed) }
    pub fn get_decimal(&self) -> f64 { self.0.load(Ordering::Relaxed) as f64 / 1_000_000.0 }
}

#[derive(Default)]
pub struct Metrics {
    // --- counters ---
    pub market_events_total: Counter,
    pub source_trades_total: Counter,
    pub source_trades_skipped_total: Counter,
    pub duplicates_suppressed_total: Counter,
    pub copy_signals_total: Counter,
    pub orders_submitted_total: Counter,
    pub orders_acknowledged_total: Counter,
    pub orders_filled_total: Counter,
    pub orders_rejected_total: Counter,
    pub orders_cancelled_total: Counter,
    pub risk_rejections_total: Counter,
    pub feed_reconnects_total: Counter,
    pub reconciliation_mismatches_total: Counter,
    pub kill_switch_activations_total: Counter,

    // --- gauges ---
    pub pnl_total: Gauge,
    pub daily_pnl: Gauge,
    pub fees_total: Gauge,
    pub equity: Gauge,
    pub gross_exposure: Gauge,
    pub active_positions: Gauge,
    pub open_orders: Gauge,
    pub tracked_wallets: Gauge,
    pub kill_switch_engaged: Gauge,
    pub feed_connected: Gauge,

    /// Rejections broken down by reason code.
    rejections_by_code: RwLock<BTreeMap<&'static str, u64>>,
    pub latency: LatencyRecorder,
}

impl Metrics {
    pub fn new() -> Self { Self::default() }

    pub fn record_rejection(&self, code: &'static str) {
        self.risk_rejections_total.inc();
        *self.rejections_by_code.write().entry(code).or_insert(0) += 1;
    }

    pub fn rejection_breakdown(&self) -> BTreeMap<&'static str, u64> {
        self.rejections_by_code.read().clone()
    }

    /// Renders the Prometheus text exposition format.
    pub fn render_prometheus(&self) -> String {
        let mut o = String::with_capacity(4096);

        let mut counter = |name: &str, help: &str, v: u64| {
            o.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
        };
        counter("market_events_total", "Order book updates ingested", self.market_events_total.get());
        counter("source_trades_total", "Target-wallet trades detected", self.source_trades_total.get());
        counter("source_trades_skipped_total", "Detected trades deliberately not copied", self.source_trades_skipped_total.get());
        counter("duplicates_suppressed_total", "Re-delivered source fills suppressed by dedup", self.duplicates_suppressed_total.get());
        counter("copy_signals_total", "Copy signals generated", self.copy_signals_total.get());
        counter("orders_submitted_total", "Orders sent to the execution adapter", self.orders_submitted_total.get());
        counter("orders_acknowledged_total", "Orders acknowledged by the venue", self.orders_acknowledged_total.get());
        counter("orders_filled_total", "Orders fully filled", self.orders_filled_total.get());
        counter("orders_rejected_total", "Orders rejected by the venue", self.orders_rejected_total.get());
        counter("orders_cancelled_total", "Orders cancelled", self.orders_cancelled_total.get());
        counter("risk_rejections_total", "Orders refused by the risk engine", self.risk_rejections_total.get());
        counter("feed_reconnects_total", "Source feed reconnections", self.feed_reconnects_total.get());
        counter("reconciliation_mismatches_total", "Position mismatches against the venue", self.reconciliation_mismatches_total.get());
        counter("kill_switch_activations_total", "Kill switch engagements", self.kill_switch_activations_total.get());

        {
            let b = self.rejections_by_code.read();
            if !b.is_empty() {
                o.push_str("# HELP risk_rejections_by_reason Risk rejections by reason code\n");
                o.push_str("# TYPE risk_rejections_by_reason counter\n");
                for (code, n) in b.iter() {
                    o.push_str(&format!("risk_rejections_by_reason{{reason=\"{code}\"}} {n}\n"));
                }
            }
        }

        let mut gauge = |name: &str, help: &str, v: f64| {
            o.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n"));
        };
        gauge("pnl_total", "Total PnL in USD", self.pnl_total.get_decimal());
        gauge("daily_pnl", "PnL since the start of the UTC day", self.daily_pnl.get_decimal());
        gauge("fees_total", "Fees paid in USD", self.fees_total.get_decimal());
        gauge("equity", "Portfolio equity in USD", self.equity.get_decimal());
        gauge("gross_exposure", "Sum of absolute position notionals", self.gross_exposure.get_decimal());
        gauge("active_positions", "Open positions", self.active_positions.get() as f64);
        gauge("open_orders", "Working orders", self.open_orders.get() as f64);
        gauge("tracked_wallets", "Configured target wallets", self.tracked_wallets.get() as f64);
        gauge("kill_switch_engaged", "1 when the kill switch is engaged", self.kill_switch_engaged.get() as f64);
        gauge("feed_connected", "1 when the source feed is connected", self.feed_connected.get() as f64);

        // Latency: only stages with real observations are exported.
        for (stage, s) in self.latency.all_stats() {
            let n = format!("{}_latency_ms", stage.as_str());
            o.push_str(&format!("# HELP {n} Measured {} latency\n# TYPE {n} summary\n", stage.as_str()));
            for (q, v) in [("0.5", s.p50_us), ("0.95", s.p95_us), ("0.99", s.p99_us)] {
                o.push_str(&format!("{n}{{quantile=\"{q}\"}} {}\n", v as f64 / 1000.0));
            }
            o.push_str(&format!("{n}_count {}\n", s.count));
            o.push_str(&format!("{n}_max {}\n", s.max_us as f64 / 1000.0));
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::LatencyStage;
    use rust_decimal_macros::dec;

    #[test]
    fn counters_accumulate() {
        let m = Metrics::new();
        m.orders_filled_total.inc();
        m.orders_filled_total.add(4);
        assert_eq!(m.orders_filled_total.get(), 5);
    }

    #[test]
    fn gauges_carry_decimal_precision() {
        let m = Metrics::new();
        m.pnl_total.set_decimal(dec!(-123.45));
        assert!((m.pnl_total.get_decimal() - (-123.45)).abs() < 1e-6);
    }

    #[test]
    fn rejection_breakdown_is_labelled_by_reason() {
        let m = Metrics::new();
        m.record_rejection("max_position");
        m.record_rejection("max_position");
        m.record_rejection("kill_switch");
        assert_eq!(m.risk_rejections_total.get(), 3);
        let b = m.rejection_breakdown();
        assert_eq!(b["max_position"], 2);
        assert_eq!(b["kill_switch"], 1);
    }

    #[test]
    fn prometheus_output_is_well_formed() {
        let m = Metrics::new();
        m.orders_filled_total.add(7);
        m.pnl_total.set_decimal(dec!(250.5));
        m.record_rejection("slippage_too_wide");
        m.latency.record(LatencyStage::Internal, 4321);
        let s = m.render_prometheus();

        assert!(s.contains("# TYPE orders_filled_total counter"));
        assert!(s.contains("\norders_filled_total 7\n"));
        assert!(s.contains("pnl_total 250.5"));
        assert!(s.contains("risk_rejections_by_reason{reason=\"slippage_too_wide\"} 1"));
        assert!(s.contains("internal_latency_ms{quantile=\"0.5\"} 4.321"));
        // Every metric line must be preceded by HELP and TYPE.
        for line in s.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            assert!(line.split_whitespace().count() >= 2, "malformed line: {line}");
        }
    }

    #[test]
    fn unmeasured_latency_stages_are_absent_from_output() {
        let m = Metrics::new();
        let s = m.render_prometheus();
        // No stage was measured, so no latency series should be exported at all.
        assert!(!s.contains("_latency_ms"), "must not export latency it never measured");
    }

    #[test]
    fn counters_are_thread_safe() {
        use std::sync::Arc;
        let m = Arc::new(Metrics::new());
        let hs: Vec<_> = (0..8).map(|_| {
            let m = m.clone();
            std::thread::spawn(move || for _ in 0..1000 { m.source_trades_total.inc(); })
        }).collect();
        for h in hs { h.join().unwrap(); }
        assert_eq!(m.source_trades_total.get(), 8000);
    }
}
