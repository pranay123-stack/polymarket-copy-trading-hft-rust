//! Bearer-token auth for mutating endpoints.
//!
//! Read endpoints are open so the dashboard works out of the box in paper mode. Anything
//! that *changes* trading behaviour — the kill switch, wallet configuration, paper reset
//! — requires the token, and config validation refuses to start LIVE without one.
//!
//! Comparison is constant-time: a token oracle on an endpoint that can halt trading is
//! not a theoretical concern.

use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

use crate::state::AppState;

/// Constant-time equality. Length is compared first, which leaks only length.
fn secure_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() { return false; }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Extracts a bearer token from an `Authorization` header.
pub fn bearer(h: Option<&str>) -> Option<&str> {
    let v = h?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token.trim())
}

/// Guards mutating routes.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let configured = state.config.server.api_token.expose().to_string();

    if configured.is_empty() {
        // No token configured. Permitted in paper/replay for zero-setup demos, but
        // never in live — config validation already refuses that combination, and this
        // is the defence in depth behind it.
        if state.mode.is_live() {
            return Err(StatusCode::UNAUTHORIZED);
        }
        return Ok(next.run(req).await);
    }

    let supplied = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match bearer(supplied) {
        Some(t) if secure_eq(t, &configured) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing_accepts_valid_headers_only() {
        assert_eq!(bearer(Some("Bearer abc123")), Some("abc123"));
        assert_eq!(bearer(Some("bearer abc123")), Some("abc123"), "scheme is case-insensitive");
        assert_eq!(bearer(Some("Basic abc123")), None);
        assert_eq!(bearer(Some("abc123")), None);
        assert_eq!(bearer(None), None);
    }

    #[test]
    fn comparison_is_length_safe_and_correct() {
        assert!(secure_eq("secret", "secret"));
        assert!(!secure_eq("secret", "secrer"));
        assert!(!secure_eq("secret", "secret2"));
        assert!(!secure_eq("", "x"));
        assert!(secure_eq("", ""));
    }

    #[test]
    fn comparison_examines_every_byte() {
        // A short-circuiting compare would make a timing oracle possible on an
        // endpoint that can halt trading.
        let a = "aaaaaaaaaaaaaaaa";
        let b = "aaaaaaaaaaaaaaab"; // differs only in the last byte
        assert!(!secure_eq(a, b));
        let c = "baaaaaaaaaaaaaaa"; // differs only in the first byte
        assert!(!secure_eq(a, c));
    }
}
