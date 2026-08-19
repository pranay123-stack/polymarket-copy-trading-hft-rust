//! Deterministic replay.
//!
//! Reads a recorded session and drives the identical pipeline, so a strategy or risk
//! change can be evaluated against a known sequence of events with no network.
//!
//! Replay is deterministic in *event order and content*. Wall-clock timestamps are
//! rewritten to now-relative values by default so latency stages remain meaningful,
//! with the original venue-relative spacing preserved.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use domain::{
    Address, CorrelationId, MarketId, Price, Qty, Side, SourceEventId, SourceTrade, TokenId,
    TradeSource, TxHash,
};
use serde::{Deserialize, Serialize};

/// One recorded event. Deliberately close to the RTDS payload so real captures can be
/// replayed with minimal transformation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayEvent {
    pub trader: String,
    pub market_id: String,
    pub token_id: String,
    #[serde(default)]
    pub outcome: String,
    pub side: String,
    pub price: f64,
    pub size: f64,
    pub tx_hash: String,
    /// Milliseconds since epoch.
    pub source_ts_ms: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaySession {
    #[serde(default)]
    pub name: String,
    pub events: Vec<ReplayEvent>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("cannot read {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("malformed replay file: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("replay file contains no events")]
    Empty,
}

impl ReplaySession {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ReplayError> {
        let p = path.as_ref();
        let raw = std::fs::read_to_string(p)
            .map_err(|e| ReplayError::Io { path: p.display().to_string(), source: e })?;
        let s: ReplaySession = serde_json::from_str(&raw)?;
        if s.events.is_empty() {
            return Err(ReplayError::Empty);
        }
        Ok(s)
    }

    /// Converts to domain trades, rebasing timestamps onto `now` while preserving the
    /// original spacing between events.
    pub fn to_source_trades(&self, now: DateTime<Utc>) -> Vec<SourceTrade> {
        let base = self.events.iter().map(|e| e.source_ts_ms).min().unwrap_or(0);
        self.events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let offset = Duration::milliseconds(e.source_ts_ms - base);
                let source_ts = now + offset;
                Some(SourceTrade {
                    // Deterministic per position, so a replay is reproducible and its
                    // dedup behaviour is identical run to run.
                    event_id: SourceEventId::from_digest(format!("replay-{i:08}-{}", e.tx_hash)),
                    correlation_id: CorrelationId::new(),
                    trader: Address::new(&e.trader).ok()?,
                    market_id: MarketId::new(&e.market_id).ok()?,
                    token_id: TokenId::new(&e.token_id).ok()?,
                    outcome: e.outcome.clone(),
                    side: match e.side.to_ascii_uppercase().as_str() {
                        "BUY" => Side::Buy,
                        "SELL" => Side::Sell,
                        _ => return None,
                    },
                    price: Price::from_feed_f64(e.price).ok()?,
                    quantity: Qty::from_feed_f64(e.size).ok()?,
                    tx_hash: TxHash::new(&e.tx_hash).ok()?,
                    occurrence: 0,
                    source_ts,
                    // Replay assumes a 400ms publish delay, matching the measured feed.
                    detected_ts: source_ts + Duration::milliseconds(400),
                    source: TradeSource::Replay,
                    market_title: e.title.clone(),
                    market_slug: e.slug.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> ReplaySession {
        ReplaySession {
            name: "test".into(),
            events: vec![
                ReplayEvent {
                    trader: format!("0x{:040x}", 1),
                    market_id: format!("0x{:064x}", 1),
                    token_id: "12345".into(),
                    outcome: "Yes".into(),
                    side: "BUY".into(),
                    price: 0.61,
                    size: 100.0,
                    tx_hash: format!("0x{:064x}", 2),
                    source_ts_ms: 1_787_102_287_000,
                    title: "T".into(),
                    slug: "t".into(),
                },
                ReplayEvent {
                    trader: format!("0x{:040x}", 1),
                    market_id: format!("0x{:064x}", 1),
                    token_id: "12345".into(),
                    outcome: "Yes".into(),
                    side: "SELL".into(),
                    price: 0.63,
                    size: 50.0,
                    tx_hash: format!("0x{:064x}", 3),
                    source_ts_ms: 1_787_102_292_000, // +5s
                    title: "T".into(),
                    slug: "t".into(),
                },
            ],
        }
    }

    #[test]
    fn replay_preserves_event_spacing() {
        let now = Utc::now();
        let ts = session().to_source_trades(now);
        assert_eq!(ts.len(), 2);
        let gap = (ts[1].source_ts - ts[0].source_ts).num_seconds();
        assert_eq!(gap, 5, "original 5s spacing must be preserved");
    }

    #[test]
    fn replay_is_deterministic_across_runs() {
        let s = session();
        let a = s.to_source_trades(Utc::now());
        let b = s.to_source_trades(Utc::now());
        // Ids depend only on position and content, never on wall clock.
        assert_eq!(a[0].event_id, b[0].event_id);
        assert_eq!(a[1].event_id, b[1].event_id);
        assert_ne!(a[0].event_id, a[1].event_id);
    }

    #[test]
    fn replay_trades_are_marked_as_replay() {
        let ts = session().to_source_trades(Utc::now());
        assert!(ts.iter().all(|t| t.source == TradeSource::Replay));
        assert!(!ts[0].source.is_real());
    }

    #[test]
    fn malformed_rows_are_skipped_not_fatal() {
        let mut s = session();
        s.events.push(ReplayEvent {
            trader: "garbage".into(), market_id: "bad".into(), token_id: "x".into(),
            outcome: String::new(), side: "SIDEWAYS".into(), price: 5.0, size: -1.0,
            tx_hash: "nope".into(), source_ts_ms: 0, title: String::new(), slug: String::new(),
        });
        // One bad row must not discard the whole session.
        assert_eq!(s.to_source_trades(Utc::now()).len(), 2);
    }

    #[test]
    fn empty_session_is_rejected_at_load() {
        let dir = std::env::temp_dir().join("copytrader-replay-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("empty.json");
        std::fs::write(&p, r#"{"name":"e","events":[]}"#).unwrap();
        assert!(matches!(ReplaySession::load(&p), Err(ReplayError::Empty)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_file_reports_its_path() {
        let e = ReplaySession::load("/nonexistent/replay.json").unwrap_err();
        assert!(e.to_string().contains("/nonexistent/replay.json"));
    }

    #[test]
    fn session_round_trips_through_json() {
        let s = session();
        let j = serde_json::to_string(&s).unwrap();
        let back: ReplaySession = serde_json::from_str(&j).unwrap();
        assert_eq!(back.events.len(), 2);
        assert_eq!(back.events[0].price, 0.61);
    }
}
