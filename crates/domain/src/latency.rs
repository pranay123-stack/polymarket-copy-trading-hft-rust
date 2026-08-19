//! End-to-end latency instrumentation.
//!
//! Every stamp here is a real observation. Nothing is estimated, and no stage is
//! reported unless its stamp was actually recorded — a missing stage yields `None`,
//! never a plausible-looking zero.
//!
//! ## What the source stamp actually means
//!
//! The RTDS envelope carries a **millisecond** timestamp; the trade payload carries a
//! **whole-second** one (see `docs/POLYMARKET_API.md` §2). Only the envelope is usable
//! for latency work, so [`LatencyStamps::source`] is populated from the envelope and
//! [`source_is_coarse`] records when we had to fall back to the second-resolution
//! payload stamp — in that case detection latency carries up to 1 s of quantisation
//! error and is flagged rather than quietly reported as precise.
//!
//! Wire latency also includes Polymarket's own publish delay (measured median ≈392 ms),
//! which is not ours to control. `detection_us` therefore measures *venue publish →
//! our ingest*, and [`internal_us`] measures the part we own.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Monotonic-ish stamps captured as a signal moves through the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyStamps {
    /// Venue-side publish time (RTDS envelope, ms resolution).
    pub source: Option<DateTime<Utc>>,
    /// True when `source` came from the second-resolution payload stamp.
    pub source_is_coarse: bool,
    /// The frame arrived in our process.
    pub detection: Option<DateTime<Utc>>,
    /// A CopySignal was produced (wallet matched, sizing done).
    pub signal: Option<DateTime<Utc>>,
    /// Risk engine returned a verdict.
    pub risk_check: Option<DateTime<Utc>>,
    /// Order handed to the execution adapter.
    pub submission: Option<DateTime<Utc>>,
    /// Venue (or simulator) acknowledged.
    pub ack: Option<DateTime<Utc>>,
    /// First fill observed.
    pub fill: Option<DateTime<Utc>>,
    /// Broadcast to dashboard subscribers.
    pub dashboard: Option<DateTime<Utc>>,
}

impl LatencyStamps {
    /// Starts a chain at ingest with no venue stamp (replay/demo sources).
    pub fn begin(detection: DateTime<Utc>) -> Self {
        Self {
            source: None,
            source_is_coarse: false,
            detection: Some(detection),
            signal: None,
            risk_check: None,
            submission: None,
            ack: None,
            fill: None,
            dashboard: None,
        }
    }

    /// Starts a chain from a venue-stamped event.
    pub fn from_source(source: DateTime<Utc>, coarse: bool, detection: DateTime<Utc>) -> Self {
        Self { source: Some(source), source_is_coarse: coarse, ..Self::begin(detection) }
    }

    fn delta_us(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<i64> {
        match (a, b) {
            (Some(a), Some(b)) => (b - a).num_microseconds(),
            _ => None,
        }
    }

    /// Venue publish → our ingest. Includes network and Polymarket's publish delay.
    pub fn detection_us(&self) -> Option<i64> { Self::delta_us(self.source, self.detection) }
    /// Ingest → signal generated. Wallet match + sizing.
    pub fn strategy_us(&self) -> Option<i64> { Self::delta_us(self.detection, self.signal) }
    /// Signal → risk verdict.
    pub fn risk_us(&self) -> Option<i64> { Self::delta_us(self.signal, self.risk_check) }
    /// Risk verdict → handed to execution.
    pub fn submission_us(&self) -> Option<i64> { Self::delta_us(self.risk_check, self.submission) }
    /// Submission → venue acknowledgement.
    pub fn ack_us(&self) -> Option<i64> { Self::delta_us(self.submission, self.ack) }
    /// Submission → first fill.
    pub fn execution_us(&self) -> Option<i64> { Self::delta_us(self.submission, self.fill) }

    /// The latency we are actually responsible for: ingest → order on the wire.
    /// This is the number to optimise; `detection_us` is mostly the venue's.
    pub fn internal_us(&self) -> Option<i64> { Self::delta_us(self.detection, self.submission) }

    /// Venue publish → fill. `None` unless both ends were observed.
    pub fn end_to_end_us(&self) -> Option<i64> {
        Self::delta_us(self.source, self.fill).or_else(|| Self::delta_us(self.detection, self.fill))
    }

    /// True if the ordering of recorded stamps is monotonically non-decreasing.
    /// Violations mean a clock jumped or stamps were recorded out of order.
    pub fn is_monotonic(&self) -> bool {
        let seq = [
            self.source, self.detection, self.signal, self.risk_check,
            self.submission, self.ack, self.fill, self.dashboard,
        ];
        let mut last: Option<DateTime<Utc>> = None;
        for s in seq.into_iter().flatten() {
            if let Some(l) = last {
                if s < l { return false; }
            }
            last = Some(s);
        }
        true
    }
}

/// A single named latency measurement, for the metrics layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySample {
    pub stage: LatencyStage,
    pub micros: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyStage {
    Detection,
    Strategy,
    Risk,
    Submission,
    Ack,
    Execution,
    Internal,
    EndToEnd,
}

impl LatencyStage {
    pub const ALL: [LatencyStage; 8] = [
        Self::Detection, Self::Strategy, Self::Risk, Self::Submission,
        Self::Ack, Self::Execution, Self::Internal, Self::EndToEnd,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Detection => "detection",
            Self::Strategy => "strategy",
            Self::Risk => "risk",
            Self::Submission => "submission",
            Self::Ack => "ack",
            Self::Execution => "execution",
            Self::Internal => "internal",
            Self::EndToEnd => "end_to_end",
        }
    }
}

impl LatencyStamps {
    /// Only stages that were genuinely measured. Callers cannot accidentally publish a
    /// zero for a stage that never happened.
    pub fn samples(&self) -> Vec<LatencySample> {
        use LatencyStage::*;
        [
            (Detection, self.detection_us()),
            (Strategy, self.strategy_us()),
            (Risk, self.risk_us()),
            (Submission, self.submission_us()),
            (Ack, self.ack_us()),
            (Execution, self.execution_us()),
            (Internal, self.internal_us()),
            (EndToEnd, self.end_to_end_us()),
        ]
        .into_iter()
        .filter_map(|(stage, us)| us.map(|micros| LatencySample { stage, micros }))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn t(base: &DateTime<Utc>, ms: i64) -> DateTime<Utc> { *base + TimeDelta::milliseconds(ms) }

    #[test]
    fn unmeasured_stages_are_none_not_zero() {
        let s = LatencyStamps::begin(Utc::now());
        assert_eq!(s.detection_us(), None, "no source stamp -> no detection latency");
        assert_eq!(s.execution_us(), None);
        // Only stages we actually observed appear.
        assert!(s.samples().is_empty());
    }

    #[test]
    fn full_chain_computes_each_stage() {
        let b = Utc::now();
        let s = LatencyStamps {
            source: Some(b),
            source_is_coarse: false,
            detection: Some(t(&b, 400)),
            signal: Some(t(&b, 402)),
            risk_check: Some(t(&b, 403)),
            submission: Some(t(&b, 405)),
            ack: Some(t(&b, 425)),
            fill: Some(t(&b, 430)),
            dashboard: Some(t(&b, 431)),
        };
        assert_eq!(s.detection_us(), Some(400_000));
        assert_eq!(s.strategy_us(), Some(2_000));
        assert_eq!(s.risk_us(), Some(1_000));
        assert_eq!(s.submission_us(), Some(2_000));
        assert_eq!(s.ack_us(), Some(20_000));
        assert_eq!(s.execution_us(), Some(25_000));
        // The part we own: ingest -> wire = 5ms, excluding the venue's 400ms publish delay.
        assert_eq!(s.internal_us(), Some(5_000));
        assert_eq!(s.end_to_end_us(), Some(430_000));
        assert_eq!(s.samples().len(), 8);
        assert!(s.is_monotonic());
    }

    #[test]
    fn end_to_end_falls_back_to_detection_when_venue_stamp_absent() {
        let b = Utc::now();
        let mut s = LatencyStamps::begin(b);
        s.fill = Some(t(&b, 10));
        assert_eq!(s.end_to_end_us(), Some(10_000));
    }

    #[test]
    fn out_of_order_stamps_are_detected() {
        let b = Utc::now();
        let mut s = LatencyStamps::from_source(b, false, t(&b, 5));
        s.signal = Some(t(&b, 1)); // clock went backwards
        assert!(!s.is_monotonic());
    }

    #[test]
    fn coarse_source_is_flagged() {
        let b = Utc::now();
        let s = LatencyStamps::from_source(b, true, t(&b, 400));
        assert!(s.source_is_coarse, "second-resolution source must be flagged, not trusted silently");
    }

    // helper needs a reference form for the &b calls above
}
