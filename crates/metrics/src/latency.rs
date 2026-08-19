//! Latency accumulation and percentiles.
//!
//! Percentiles come from a bounded ring of **actual observations**. Nothing is modelled,
//! smoothed or assumed: if a stage was never measured, it reports no statistics rather
//! than a plausible-looking zero.
//!
//! The ring is deliberately small enough to stay cache-friendly and large enough that
//! p99 means something (4096 samples). Older samples fall out, so the numbers describe
//! recent behaviour rather than a lifetime average that hides a recent regression.

use domain::LatencyStage;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

const CAPACITY: usize = 4096;

/// Percentiles over recent observations, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LatencyStats {
    pub count: u64,
    pub min_us: i64,
    pub mean_us: i64,
    pub p50_us: i64,
    pub p95_us: i64,
    pub p99_us: i64,
    pub max_us: i64,
}

impl LatencyStats {
    pub fn p50_ms(&self) -> f64 { self.p50_us as f64 / 1000.0 }
    pub fn p99_ms(&self) -> f64 { self.p99_us as f64 / 1000.0 }
}

/// A fixed-capacity ring of samples for one stage.
struct Ring {
    buf: Vec<i64>,
    next: usize,
    total: u64,
}

impl Ring {
    fn new() -> Self { Self { buf: Vec::with_capacity(CAPACITY), next: 0, total: 0 } }

    fn push(&mut self, v: i64) {
        self.total += 1;
        if self.buf.len() < CAPACITY {
            self.buf.push(v);
        } else {
            self.buf[self.next] = v;
            self.next = (self.next + 1) % CAPACITY;
        }
    }

    fn stats(&self) -> Option<LatencyStats> {
        if self.buf.is_empty() { return None; }
        let mut s = self.buf.clone();
        s.sort_unstable();
        let n = s.len();
        // Nearest-rank percentile: the value at ceil(p*n), which is always a real
        // observation rather than an interpolation between two.
        let at = |p: f64| -> i64 {
            let idx = ((p * n as f64).ceil() as usize).saturating_sub(1).min(n - 1);
            s[idx]
        };
        Some(LatencyStats {
            count: self.total,
            min_us: s[0],
            mean_us: (s.iter().sum::<i64>()) / n as i64,
            p50_us: at(0.50),
            p95_us: at(0.95),
            p99_us: at(0.99),
            max_us: s[n - 1],
        })
    }
}

/// Per-stage latency recorder.
pub struct LatencyRecorder {
    stages: RwLock<Vec<(LatencyStage, Ring)>>,
}

impl Default for LatencyRecorder {
    fn default() -> Self { Self::new() }
}

impl LatencyRecorder {
    pub fn new() -> Self {
        Self {
            stages: RwLock::new(LatencyStage::ALL.iter().map(|s| (*s, Ring::new())).collect()),
        }
    }

    pub fn record(&self, stage: LatencyStage, micros: i64) {
        // Negative deltas mean a clock went backwards; recording them would corrupt the
        // percentiles, so they are dropped rather than clamped to zero.
        if micros < 0 { return; }
        if let Some(e) = self.stages.write().iter_mut().find(|(s, _)| *s == stage) {
            e.1.push(micros);
        }
    }

    /// Records every stage that was genuinely measured on a chain.
    pub fn record_all(&self, stamps: &domain::LatencyStamps) {
        for s in stamps.samples() {
            self.record(s.stage, s.micros);
        }
    }

    /// `None` for stages with no observations.
    pub fn stats(&self, stage: LatencyStage) -> Option<LatencyStats> {
        self.stages.read().iter().find(|(s, _)| *s == stage).and_then(|(_, r)| r.stats())
    }

    pub fn all_stats(&self) -> Vec<(LatencyStage, LatencyStats)> {
        self.stages
            .read()
            .iter()
            .filter_map(|(s, r)| r.stats().map(|st| (*s, st)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmeasured_stages_report_nothing() {
        let r = LatencyRecorder::new();
        assert_eq!(r.stats(LatencyStage::Detection), None, "must not invent a zero");
        assert!(r.all_stats().is_empty());
    }

    #[test]
    fn percentiles_are_real_observations() {
        let r = LatencyRecorder::new();
        for i in 1..=100 { r.record(LatencyStage::Internal, i * 1000); }
        let s = r.stats(LatencyStage::Internal).unwrap();
        assert_eq!(s.count, 100);
        assert_eq!(s.min_us, 1000);
        assert_eq!(s.max_us, 100_000);
        assert_eq!(s.p50_us, 50_000);
        assert_eq!(s.p95_us, 95_000);
        assert_eq!(s.p99_us, 99_000);
        assert_eq!(s.mean_us, 50_500);
    }

    #[test]
    fn percentile_ordering_always_holds() {
        let r = LatencyRecorder::new();
        for i in [500, 12, 9000, 3, 77, 100_000, 42] { r.record(LatencyStage::Risk, i); }
        let s = r.stats(LatencyStage::Risk).unwrap();
        assert!(s.min_us <= s.p50_us);
        assert!(s.p50_us <= s.p95_us);
        assert!(s.p95_us <= s.p99_us);
        assert!(s.p99_us <= s.max_us);
    }

    #[test]
    fn backwards_clocks_are_dropped_not_recorded() {
        let r = LatencyRecorder::new();
        r.record(LatencyStage::Ack, -5000);
        assert_eq!(r.stats(LatencyStage::Ack), None, "a negative delta is not a measurement");
        r.record(LatencyStage::Ack, 100);
        assert_eq!(r.stats(LatencyStage::Ack).unwrap().count, 1);
    }

    #[test]
    fn ring_bounds_memory_and_tracks_recent_behaviour() {
        let r = LatencyRecorder::new();
        // Fill with fast samples, then flood with slow ones.
        for _ in 0..CAPACITY { r.record(LatencyStage::Execution, 1_000); }
        assert_eq!(r.stats(LatencyStage::Execution).unwrap().p50_us, 1_000);
        for _ in 0..CAPACITY { r.record(LatencyStage::Execution, 900_000); }
        let s = r.stats(LatencyStage::Execution).unwrap();
        assert_eq!(s.p50_us, 900_000, "recent regression must show, not be diluted");
        assert_eq!(s.count, (CAPACITY * 2) as u64, "the lifetime count is still exact");
    }

    #[test]
    fn single_sample_is_every_percentile() {
        let r = LatencyRecorder::new();
        r.record(LatencyStage::Strategy, 1234);
        let s = r.stats(LatencyStage::Strategy).unwrap();
        assert_eq!((s.min_us, s.p50_us, s.p99_us, s.max_us), (1234, 1234, 1234, 1234));
    }

    #[test]
    fn record_all_only_captures_measured_stages() {
        use chrono::{TimeDelta, Utc};
        let b = Utc::now();
        let mut st = domain::LatencyStamps::from_source(b, false, b + TimeDelta::milliseconds(400));
        st.signal = Some(b + TimeDelta::milliseconds(402));
        // No risk/submission/fill stamps.
        let r = LatencyRecorder::new();
        r.record_all(&st);
        assert!(r.stats(domain::LatencyStage::Detection).is_some());
        assert!(r.stats(domain::LatencyStage::Strategy).is_some());
        assert!(r.stats(domain::LatencyStage::Risk).is_none(), "unmeasured stage stays absent");
        assert!(r.stats(domain::LatencyStage::EndToEnd).is_none());
    }
}
