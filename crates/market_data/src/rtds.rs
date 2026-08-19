//! RTDS activity-feed client — the event-driven source of third-party trades.
//!
//! ## Why this is a firehose and not a filtered subscription
//!
//! `wss://ws-live-data.polymarket.com` accepts a subscription to `activity/trades` and
//! then pushes **every trade on the platform** (~33/sec) with `proxyWallet` attribution.
//! Three plausible server-side wallet-filter spellings were tested against production
//! and every one was accepted without error and then delivered **zero frames** — the
//! filter silently breaks the subscription instead of narrowing it
//! (`docs/POLYMARKET_API.md` §2).
//!
//! Consequently target-wallet matching is *our* job, on the hot path, for every frame.
//! That is why the matcher in `wallet_tracker` is an O(1) hash lookup performed before
//! any allocation or parsing work that a non-target frame would waste.

use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::parser::{parse_rtds_frame, ParsedTrade, RtdsFrame};
use crate::reconnect::Backoff;

/// The subscription frame. Verified working against production.
pub const SUBSCRIBE_TRADES: &str =
    r#"{"action":"subscribe","subscriptions":[{"topic":"activity","type":"trades"}]}"#;

/// What the feed task emits.
#[derive(Debug)]
pub enum FeedMessage {
    Trade(Box<ParsedTrade>),
    Connected { at: DateTime<Utc> },
    /// Carries the last time we saw a trade, which bounds what backfill must cover.
    Disconnected { at: DateTime<Utc>, last_trade_at: Option<DateTime<Utc>>, reason: String },
}

impl std::fmt::Debug for ParsedTrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedTrade")
            .field("trader", &self.trader)
            .field("side", &self.side)
            .field("price", &self.price)
            .field("qty", &self.quantity)
            .field("tx", &self.tx_hash)
            .finish()
    }
}

/// Runtime statistics, surfaced on the health endpoint.
#[derive(Debug, Default, Clone, Copy)]
pub struct FeedStats {
    pub frames_received: u64,
    pub trades_parsed: u64,
    pub parse_errors: u64,
    pub ignored_frames: u64,
    pub reconnects: u64,
}

pub struct RtdsClient {
    url: String,
    connect_timeout: Duration,
}

impl RtdsClient {
    pub fn new(url: String, connect_timeout_ms: u64) -> Self {
        Self { url, connect_timeout: Duration::from_millis(connect_timeout_ms) }
    }

    /// Runs the feed until `shutdown` fires, reconnecting with jittered backoff.
    ///
    /// A dropped connection is reported with the timestamp of the last trade seen, so
    /// the caller can backfill exactly the gap rather than guessing.
    pub async fn run(
        self,
        tx: mpsc::Sender<FeedMessage>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut backoff = Backoff::new(500, 30_000);
        let mut last_trade_at: Option<DateTime<Utc>> = None;

        loop {
            if *shutdown.borrow() {
                info!("rtds feed shutting down");
                return;
            }

            let connect = tokio::time::timeout(
                self.connect_timeout,
                tokio_tungstenite::connect_async(&self.url),
            );

            match connect.await {
                Ok(Ok((mut ws, _resp))) => {
                    if let Err(e) = ws.send(Message::Text(SUBSCRIBE_TRADES.into())).await {
                        warn!(error = %e, "rtds subscribe failed");
                        let _ = tx.send(FeedMessage::Disconnected {
                            at: Utc::now(), last_trade_at, reason: format!("subscribe: {e}") }).await;
                        Self::wait(&mut backoff, &mut shutdown).await;
                        continue;
                    }
                    backoff.reset();
                    info!(url = %self.url, "rtds feed connected");
                    let _ = tx.send(FeedMessage::Connected { at: Utc::now() }).await;

                    let reason = loop {
                        tokio::select! {
                            _ = shutdown.changed() => {
                                if *shutdown.borrow() { break "shutdown".to_string(); }
                            }
                            msg = ws.next() => match msg {
                                None => break "stream ended".to_string(),
                                Some(Err(e)) => break format!("ws error: {e}"),
                                Some(Ok(Message::Close(_))) => break "server closed".to_string(),
                                Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; }
                                Some(Ok(Message::Text(t))) => {
                                    // Stamp arrival before any parsing work so detection
                                    // latency measures the wire, not our CPU time.
                                    let at = Utc::now();
                                    match parse_rtds_frame(&t, at) {
                                        Ok(RtdsFrame::Trade(tr)) => {
                                            last_trade_at = Some(tr.source_ts);
                                            if tx.send(FeedMessage::Trade(tr)).await.is_err() {
                                                break "consumer dropped".to_string();
                                            }
                                        }
                                        Ok(RtdsFrame::Ignored) => {}
                                        Err(e) => debug!(error = %e, "unparseable rtds frame"),
                                    }
                                }
                                Some(Ok(_)) => {}
                            }
                        }
                    };

                    if *shutdown.borrow() { return; }
                    warn!(%reason, "rtds feed disconnected");
                    let _ = tx.send(FeedMessage::Disconnected {
                        at: Utc::now(), last_trade_at, reason }).await;
                }
                Ok(Err(e)) => {
                    error!(error = %e, "rtds connect failed");
                    let _ = tx.send(FeedMessage::Disconnected {
                        at: Utc::now(), last_trade_at, reason: format!("connect: {e}") }).await;
                }
                Err(_) => {
                    error!(timeout_ms = ?self.connect_timeout, "rtds connect timed out");
                    let _ = tx.send(FeedMessage::Disconnected {
                        at: Utc::now(), last_trade_at, reason: "connect timeout".into() }).await;
                }
            }

            Self::wait(&mut backoff, &mut shutdown).await;
        }
    }

    async fn wait(backoff: &mut Backoff, shutdown: &mut tokio::sync::watch::Receiver<bool>) {
        let d = backoff.next_delay();
        debug!(delay_ms = d.as_millis() as u64, attempt = backoff.attempt(), "rtds backoff");
        tokio::select! {
            _ = tokio::time::sleep(d) => {}
            _ = shutdown.changed() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_frame_matches_the_verified_shape() {
        let v: serde_json::Value = serde_json::from_str(SUBSCRIBE_TRADES).unwrap();
        assert_eq!(v["action"], "subscribe");
        assert_eq!(v["subscriptions"][0]["topic"], "activity");
        assert_eq!(v["subscriptions"][0]["type"], "trades");
        // A wallet filter here silently yields zero frames — it must never be added.
        assert!(v["subscriptions"][0].get("filters").is_none());
        assert!(v["subscriptions"][0].get("user").is_none());
    }

    #[tokio::test]
    async fn connect_failure_reports_disconnection_rather_than_panicking() {
        let (tx, mut rx) = mpsc::channel(8);
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
        // Reserved-for-documentation address; guaranteed not to connect.
        let c = RtdsClient::new("ws://192.0.2.1:9/".into(), 150);
        let h = tokio::spawn(c.run(tx, sd_rx));
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(msg, FeedMessage::Disconnected { .. }), "got {msg:?}");
        let _ = sd_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
    }
}
