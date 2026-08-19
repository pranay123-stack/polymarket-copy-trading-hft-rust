//! Dashboard WebSocket at `/ws`.
//!
//! Broadcasts every [`SystemEvent`] to connected dashboards. Uses a broadcast channel, so
//! a slow or stalled dashboard client **lags and skips** rather than applying
//! backpressure to the trading pipeline. That trade-off is deliberate: a browser tab must
//! never be able to slow down order handling.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

use crate::state::AppState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle(socket, state))
}

/// Envelope sent to the dashboard.
#[derive(serde::Serialize)]
struct Envelope<'a> {
    kind: &'a str,
    critical: bool,
    at: chrono::DateTime<chrono::Utc>,
    payload: &'a domain::SystemEvent,
}

async fn handle(socket: WebSocket, state: Arc<AppState>) {
    let (mut tx, mut rx) = socket.split();
    let mut events = state.events.subscribe();

    // Send an immediate snapshot so a freshly-opened dashboard is not blank until the
    // next event happens to fire.
    let snapshot = serde_json::json!({
        "kind": "snapshot",
        "critical": false,
        "at": chrono::Utc::now(),
        "payload": {
            "mode": state.mode.as_str(),
            "real_money": state.is_real_money(),
            "kill_switch": state.kill_switch.state(),
            "pnl": state.portfolio.snapshot(state.orders.open_count()),
            "health": state.health.report(),
        }
    });
    if tx.send(Message::Text(snapshot.to_string())).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            // Detect client disconnects promptly.
            incoming = rx.next() => match incoming {
                None | Some(Err(_)) => break,
                Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            },
            ev = events.recv() => match ev {
                Ok(e) => {
                    let env = Envelope {
                        kind: e.kind(),
                        critical: e.is_critical(),
                        at: chrono::Utc::now(),
                        payload: &e,
                    };
                    let Ok(txt) = serde_json::to_string(&env) else { continue };
                    if tx.send(Message::Text(txt)).await.is_err() { break; }
                }
                Err(RecvError::Lagged(n)) => {
                    // The dashboard fell behind. Tell it so it can resync, and keep going
                    // — the alternative is slowing the trading path for a browser tab.
                    warn!(skipped = n, "dashboard websocket lagged");
                    let notice = serde_json::json!({
                        "kind": "lagged", "critical": false,
                        "at": chrono::Utc::now(),
                        "payload": { "skipped": n, "action": "resync via REST" }
                    });
                    if tx.send(Message::Text(notice.to_string())).await.is_err() { break; }
                }
                Err(RecvError::Closed) => break,
            }
        }
    }
    debug!("dashboard websocket closed");
}

#[cfg(test)]
mod tests {
    use domain::{AppMode, SystemEvent};

    #[test]
    fn events_serialise_with_a_stable_discriminator() {
        let e = SystemEvent::KillSwitchActivated {
            reason: "manual".into(), by: "op".into(), at: chrono::Utc::now() };
        let v = serde_json::to_value(&e).unwrap();
        // The dashboard switches on this tag; it must be present and stable.
        assert_eq!(v["event"], "kill_switch_activated");
        assert_eq!(e.kind(), "kill_switch_activated");
        assert!(e.is_critical());
    }

    #[test]
    fn mode_serialises_uppercase_for_the_ui_banner() {
        assert_eq!(serde_json::to_value(AppMode::Live).unwrap(), "LIVE");
        assert_eq!(serde_json::to_value(AppMode::Paper).unwrap(), "PAPER");
    }
}
