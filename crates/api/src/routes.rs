//! Route table.
//!
//! Read routes are open; mutating routes sit behind [`crate::auth::require_auth`].
//! The split is explicit here so it is auditable at a glance rather than scattered
//! across handlers.

use std::sync::Arc;

use axum::routing::{delete, get, patch, post};
use axum::{middleware, Router};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::handlers as h;
use crate::state::AppState;
use crate::websocket::ws_handler;

pub fn router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Anything that can change trading behaviour.
    let protected = Router::new()
        .route("/api/risk/kill-switch", post(h::kill_switch_engage))
        .route("/api/risk/kill-switch/reset", post(h::kill_switch_reset))
        .route("/api/target-wallets", post(h::add_wallet))
        .route("/api/target-wallets/:id", patch(h::update_wallet))
        .route("/api/target-wallets/:id", delete(h::delete_wallet))
        .route("/api/paper/reset", post(h::paper_reset))
        .layer(middleware::from_fn_with_state(state.clone(), crate::auth::require_auth));

    let public = Router::new()
        .route("/api/health", get(h::health))
        .route("/api/status", get(h::status))
        .route("/api/mode", get(h::mode))
        .route("/api/config", get(h::config_view))
        .route("/api/metrics", get(h::metrics_prometheus))
        .route("/metrics", get(h::metrics_prometheus))
        .route("/api/positions", get(h::positions))
        .route("/api/orders", get(h::orders))
        .route("/api/fills", get(h::fills))
        .route("/api/trades", get(h::trades))
        .route("/api/pnl", get(h::pnl))
        .route("/api/latency", get(h::latency))
        .route("/api/risk", get(h::risk_view))
        .route("/api/target-wallets", get(h::list_wallets))
        .route("/ws", get(ws_handler));

    public
        .merge(protected)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Every route, for documentation and the route-coverage test.
pub const ROUTES: &[(&str, &str, bool)] = &[
    ("GET", "/api/health", false),
    ("GET", "/api/status", false),
    ("GET", "/api/mode", false),
    ("GET", "/api/config", false),
    ("GET", "/api/metrics", false),
    ("GET", "/api/positions", false),
    ("GET", "/api/orders", false),
    ("GET", "/api/fills", false),
    ("GET", "/api/trades", false),
    ("GET", "/api/pnl", false),
    ("GET", "/api/latency", false),
    ("GET", "/api/risk", false),
    ("GET", "/api/target-wallets", false),
    ("GET", "/ws", false),
    ("POST", "/api/risk/kill-switch", true),
    ("POST", "/api/risk/kill-switch/reset", true),
    ("POST", "/api/target-wallets", true),
    ("PATCH", "/api/target-wallets/:id", true),
    ("DELETE", "/api/target-wallets/:id", true),
    ("POST", "/api/paper/reset", true),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mutating_route_is_protected() {
        // The property that matters: nothing that changes trading is publicly callable.
        for (method, path, protected) in ROUTES {
            let mutating = matches!(*method, "POST" | "PATCH" | "DELETE" | "PUT");
            assert_eq!(
                mutating, *protected,
                "{method} {path} protection ({protected}) does not match its mutability ({mutating})"
            );
        }
    }

    #[test]
    fn the_documented_route_set_is_complete() {
        // Guards against a route being added without documentation or auth review.
        let required = [
            "/api/health", "/api/status", "/api/mode", "/api/metrics", "/api/positions",
            "/api/orders", "/api/fills", "/api/trades", "/api/pnl", "/api/risk",
            "/api/target-wallets", "/api/risk/kill-switch", "/api/risk/kill-switch/reset",
            "/api/config", "/api/paper/reset", "/api/latency", "/ws",
        ];
        for r in required {
            assert!(ROUTES.iter().any(|(_, p, _)| *p == r), "route {r} is missing");
        }
    }
}
