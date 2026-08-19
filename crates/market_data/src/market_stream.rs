//! The market-channel streaming task.
//!
//! Keeps a live book for a dynamically-growing set of tokens, so the trading path does not
//! pay a REST round trip to price a copy.
//!
//! ## Why the token set has to be dynamic
//!
//! Target wallets trade across the whole platform. A fixed subscription list chosen at
//! startup will not contain the token a target trades next — measured, that produced a
//! 100% rejection rate before on-demand fetching was added. So a token is subscribed the
//! first time we see it, and every subsequent trade in that market is priced from the live
//! book with no network call.
//!
//! The venue takes the subscription set at connect time, so growing it means reconnecting.
//! That is batched (`RESUBSCRIBE_DEBOUNCE`) rather than done per new token, otherwise a
//! busy period would reconnect continuously.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use domain::{OrderBook, TokenId};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::market_ws::{parse_market_frame, subscribe_frame, BookBuilder, MarketEvent};
use crate::reconnect::Backoff;

/// Wait this long after the token set changes before reconnecting to pick it up.
const RESUBSCRIBE_DEBOUNCE: Duration = Duration::from_secs(5);
/// Hard cap so a long-running process cannot subscribe to unbounded tokens.
pub const MAX_SUBSCRIBED_TOKENS: usize = 400;

/// The set of tokens the stream should follow. Shared with the trading path, which adds
/// to it whenever a target trades something new.
#[derive(Default)]
pub struct TokenSubscriptions {
    tokens: Mutex<BTreeSet<TokenId>>,
    dirty: Mutex<bool>,
}

impl TokenSubscriptions {
    pub fn new() -> Self { Self::default() }

    /// Adds a token. Returns true if it was not already followed.
    pub fn add(&self, t: TokenId) -> bool {
        let mut g = self.tokens.lock();
        if g.len() >= MAX_SUBSCRIBED_TOKENS || g.contains(&t) {
            return false;
        }
        g.insert(t);
        *self.dirty.lock() = true;
        true
    }

    pub fn extend(&self, ts: impl IntoIterator<Item = TokenId>) {
        for t in ts { self.add(t); }
    }

    pub fn snapshot(&self) -> Vec<TokenId> { self.tokens.lock().iter().cloned().collect() }
    pub fn len(&self) -> usize { self.tokens.lock().len() }
    pub fn is_empty(&self) -> bool { self.tokens.lock().is_empty() }
    pub fn take_dirty(&self) -> bool {
        let mut d = self.dirty.lock();
        std::mem::replace(&mut *d, false)
    }
}

/// Runtime counters for the health endpoint.
#[derive(Debug, Default, Clone, Copy)]
pub struct StreamStats {
    pub snapshots: u64,
    pub level_changes: u64,
    pub tick_changes: u64,
    pub reconnects: u64,
    pub integrity_resyncs: u64,
}

/// Streams books until shutdown, publishing each updated book through `on_book`.
pub async fn run_market_stream<F>(
    url: String,
    connect_timeout_ms: u64,
    subs: Arc<TokenSubscriptions>,
    stats: Arc<Mutex<StreamStats>>,
    on_book: F,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) where
    F: Fn(&OrderBook) + Send + Sync + 'static,
{
    let mut backoff = Backoff::new(500, 20_000);
    let mut seq: u64 = 0;

    loop {
        if *shutdown.borrow() { return; }

        let tokens = subs.snapshot();
        if tokens.is_empty() {
            // Nothing to follow yet; wait for the trading path to discover something.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                _ = shutdown.changed() => { if *shutdown.borrow() { return; } continue }
            }
        }

        let connect = tokio::time::timeout(
            Duration::from_millis(connect_timeout_ms),
            tokio_tungstenite::connect_async(&url),
        );

        match connect.await {
            Ok(Ok((mut ws, _))) => {
                if ws.send(Message::Text(subscribe_frame(&tokens))).await.is_err() {
                    stats.lock().reconnects += 1;
                    Self_wait(&mut backoff, &mut shutdown).await;
                    continue;
                }
                backoff.reset();
                info!(tokens = tokens.len(), "market stream connected");
                subs.take_dirty();

                let mut builder = BookBuilder::new();
                let mut since_change = tokio::time::Instant::now();

                let reason = loop {
                    // Pick up newly-discovered tokens by reconnecting, debounced.
                    if subs.take_dirty() { since_change = tokio::time::Instant::now(); }
                    if since_change.elapsed() > RESUBSCRIBE_DEBOUNCE
                        && subs.len() != tokens.len()
                    {
                        break "resubscribing for new tokens".to_string();
                    }

                    tokio::select! {
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() { return; }
                        }
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        msg = ws.next() => match msg {
                            None => break "stream ended".into(),
                            Some(Err(e)) => break format!("ws error: {e}"),
                            Some(Ok(Message::Close(_))) => break "server closed".into(),
                            Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; }
                            Some(Ok(Message::Text(t))) => {
                                let at = Utc::now();
                                match parse_market_frame(&t, seq, at) {
                                    Err(e) => debug!(error = %e, "unparseable market frame"),
                                    Ok(events) => {
                                        for ev in events {
                                            seq += 1;
                                            {
                                                let mut s = stats.lock();
                                                match &ev {
                                                    MarketEvent::Snapshot(_) => s.snapshots += 1,
                                                    MarketEvent::LevelChange { .. } => s.level_changes += 1,
                                                    MarketEvent::TickSizeChanged { .. } => s.tick_changes += 1,
                                                    MarketEvent::Ignored => {}
                                                }
                                            }
                                            let before = builder.integrity_failures;
                                            if let Some(tok) = builder.apply(ev, at) {
                                                if builder.integrity_failures > before {
                                                    stats.lock().integrity_resyncs += 1;
                                                }
                                                if let Some(b) = builder.get(&tok) {
                                                    on_book(b);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Some(Ok(_)) => {}
                        }
                    }
                };
                if *shutdown.borrow() { return; }
                warn!(%reason, "market stream disconnected");
                stats.lock().reconnects += 1;
            }
            Ok(Err(e)) => {
                warn!(error = %e, "market stream connect failed");
                stats.lock().reconnects += 1;
            }
            Err(_) => {
                warn!("market stream connect timed out");
                stats.lock().reconnects += 1;
            }
        }
        Self_wait(&mut backoff, &mut shutdown).await;
    }
}

#[allow(non_snake_case)]
async fn Self_wait(b: &mut Backoff, shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    let d = b.next_delay();
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = shutdown.changed() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(n: u32) -> TokenId { TokenId::new(format!("{}", 1000 + n)).unwrap() }

    #[test]
    fn subscriptions_deduplicate_and_track_changes() {
        let s = TokenSubscriptions::new();
        assert!(s.add(tok(1)));
        assert!(!s.add(tok(1)), "adding twice must not re-dirty the set");
        assert!(s.add(tok(2)));
        assert_eq!(s.len(), 2);
        assert!(s.take_dirty());
        assert!(!s.take_dirty(), "dirty flag must clear once consumed");
    }

    #[test]
    fn subscription_set_is_bounded() {
        // A long-running process must not subscribe to unbounded tokens.
        let s = TokenSubscriptions::new();
        for i in 0..(MAX_SUBSCRIBED_TOKENS as u32 + 50) { s.add(tok(i)); }
        assert_eq!(s.len(), MAX_SUBSCRIBED_TOKENS);
    }

    #[test]
    fn snapshot_is_stable_and_sorted() {
        let s = TokenSubscriptions::new();
        s.extend([tok(3), tok(1), tok(2)]);
        let a = s.snapshot();
        assert_eq!(a.len(), 3);
        assert_eq!(a, s.snapshot(), "snapshot must be deterministic");
    }

    #[tokio::test]
    async fn empty_subscription_set_does_not_spin_or_connect() {
        let subs = Arc::new(TokenSubscriptions::new());
        let stats = Arc::new(Mutex::new(StreamStats::default()));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = tokio::spawn(run_market_stream(
            "ws://192.0.2.1:9/".into(), 200, subs, stats.clone(), |_| {}, rx));
        tokio::time::sleep(Duration::from_millis(600)).await;
        let _ = tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), h).await;
        // With nothing to follow it must not have attempted a connection at all.
        assert_eq!(stats.lock().reconnects, 0);
    }
}
