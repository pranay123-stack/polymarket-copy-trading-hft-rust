//! Live execution adapter for the Polymarket CLOB.
//!
//! # Scope and honesty about what is verified
//!
//! Everything in this file that touches the network was probed against production on
//! 2026-08-19 (see `docs/POLYMARKET_API.md` §7). What was confirmed:
//!
//! | Endpoint | Unauthenticated response | Meaning |
//! |---|---|---|
//! | `POST /order` | 401 `{"error":"missing address header"}` | real endpoint, **L1** signature gate |
//! | `POST /auth/api-key` | 401 `{"error":"Invalid L1 Request headers"}` | L1 derives L2 |
//! | `GET /data/orders` | 401 `{"error":"Unauthorized/Invalid api key"}` | **L2** HMAC gate |
//! | `GET /data/trades` | 401 `{"error":"Unauthorized/Invalid api key"}` | L2 HMAC gate |
//!
//! What **could not** be verified without funded credentials, and is therefore *not*
//! implemented speculatively:
//!
//! * the exact EIP-712 typed-data struct and signature encoding for an order;
//! * the success-path response body of `POST /order`;
//! * cancellation semantics and its response shape;
//! * the `/ws/user` fill-event payload.
//!
//! Rather than invent those, order signing is abstracted behind [`OrderSigner`], which
//! has **no default implementation**. Without an injected signer this adapter refuses to
//! submit and says exactly what is missing. That is a deliberate design choice: a live
//! adapter that appears to work but silently does nothing is far more dangerous than one
//! that refuses loudly.
//!
//! Every remaining unknown is marked `UNVERIFIED:` inline.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use domain::{Fill, OrderId, OrderRequest, Qty, TokenId};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tracing::{error, info, warn};

use crate::adapter::{
    Acknowledgement, AdapterCapabilities, ExecutionAdapter, ExecutionError, VenuePosition,
};

/// A signed order payload, ready to POST.
#[derive(Debug, Clone)]
pub struct SignedOrder {
    /// The complete JSON body for `POST /order`.
    pub body: serde_json::Value,
    /// Value for the `POLY_ADDRESS` header (L1).
    pub address: String,
}

/// Produces an EIP-712 signature over an order.
///
/// **Intentionally not implemented in this crate.** Implementing it requires a
/// secp256k1 signer plus the exact typed-data struct Polymarket expects, which could not
/// be confirmed against production without funded credentials. Supplying a wrong
/// signature would produce silent rejections that look like connectivity problems.
///
/// To go live: implement this trait against the official client's signing scheme, verify
/// against a funded account on a minimum-size order, and inject it with
/// [`LiveExecution::with_signer`].
pub trait OrderSigner: Send + Sync {
    fn sign(&self, order: &OrderRequest, tick_size: rust_decimal::Decimal) -> Result<SignedOrder, String>;
    /// The signing wallet address, used for reconciliation.
    fn address(&self) -> &str;
}

/// L2 HMAC credentials, derived from an L1 signature via `POST /auth/api-key`.
#[derive(Clone)]
pub struct L2Credentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl std::fmt::Debug for L2Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render credential material.
        f.write_str("L2Credentials(<redacted>)")
    }
}

impl L2Credentials {
    /// Builds the L2 auth headers.
    ///
    /// UNVERIFIED: the exact HMAC message construction (field order and separators) is
    /// taken from the documented scheme and could not be confirmed against a live 200
    /// response without credentials. If live requests return
    /// `Unauthorized/Invalid api key` **with** credentials present, this is the first
    /// place to look.
    pub fn headers(&self, method: &str, path: &str, body: &str, ts: i64) -> HeaderMap {
        let mut h = HeaderMap::new();
        let msg = format!("{ts}{method}{path}{body}");
        let sig = hmac_sha256_b64(&self.secret, &msg);
        let insert = |h: &mut HeaderMap, k: &'static str, v: &str| {
            if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                h.insert(name, val);
            }
        };
        insert(&mut h, "POLY_API_KEY", &self.api_key);
        insert(&mut h, "POLY_PASSPHRASE", &self.passphrase);
        insert(&mut h, "POLY_TIMESTAMP", &ts.to_string());
        insert(&mut h, "POLY_SIGNATURE", &sig);
        h
    }
}

/// Base64 HMAC-SHA256. The venue's secret is base64url-encoded.
fn hmac_sha256_b64(secret_b64: &str, msg: &str) -> String {
    use sha2::{Digest, Sha256};
    // Minimal HMAC-SHA256 (RFC 2104) over the decoded secret, avoiding an extra crate.
    let key = b64url_decode(secret_b64).unwrap_or_else(|| secret_b64.as_bytes().to_vec());
    const BLOCK: usize = 64;
    let mut k = if key.len() > BLOCK {
        Sha256::digest(&key).to_vec()
    } else {
        key
    };
    k.resize(BLOCK, 0);
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = {
        let mut h = Sha256::new();
        h.update(ipad);
        h.update(msg.as_bytes());
        h.finalize()
    };
    let outer = {
        let mut h = Sha256::new();
        h.update(opad);
        h.update(inner);
        h.finalize()
    };
    b64url_encode(&outer)
}

fn b64url_encode(b: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut o = String::new();
    for c in b.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((*c.get(1).unwrap_or(&0) as u32) << 8)
            | (*c.get(2).unwrap_or(&0) as u32);
        o.push(T[(n >> 18 & 63) as usize] as char);
        o.push(T[(n >> 12 & 63) as usize] as char);
        o.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        o.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    o
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    };
    let clean: Vec<u8> = s.bytes().filter(|c| *c != b'=' && !c.is_ascii_whitespace()).collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    Some(out)
}

pub struct LiveExecution {
    client: reqwest::Client,
    clob_url: String,
    creds: Option<L2Credentials>,
    signer: Option<Arc<dyn OrderSigner>>,
    /// Both `APP_MODE=live` and `LIVE_TRADING_ENABLED=true` were satisfied.
    armed: bool,
    funder_address: Option<String>,
}

impl LiveExecution {
    pub fn new(clob_url: String, timeout_ms: u64, armed: bool) -> Result<Self, ExecutionError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .user_agent(concat!("polymarket-copytrader/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ExecutionError::NotReady(e.to_string()))?;
        Ok(Self { client, clob_url, creds: None, signer: None, armed, funder_address: None })
    }

    pub fn with_credentials(mut self, c: L2Credentials) -> Self { self.creds = Some(c); self }

    /// Injects the EIP-712 signer. Without it, [`submit`] refuses.
    pub fn with_signer(mut self, s: Arc<dyn OrderSigner>) -> Self { self.signer = Some(s); self }

    pub fn with_funder(mut self, a: String) -> Self { self.funder_address = Some(a); self }

    /// Everything that must be true before a real order can be sent.
    /// Returns the list of what is missing, so the operator gets a complete answer at once.
    pub fn readiness_gaps(&self) -> Vec<&'static str> {
        let mut g = Vec::new();
        if !self.armed { g.push("APP_MODE=live and LIVE_TRADING_ENABLED=true"); }
        if self.creds.is_none() { g.push("L2 API credentials (key/secret/passphrase)"); }
        if self.signer.is_none() { g.push("an OrderSigner implementation (EIP-712 L1 signing)"); }
        if self.funder_address.is_none() { g.push("POLYMARKET_FUNDER_ADDRESS"); }
        g
    }
}

#[async_trait]
impl ExecutionAdapter for LiveExecution {
    fn name(&self) -> &'static str { "live" }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            supports_cancel: true,
            supports_position_query: true,
            is_real_money: true,
        }
    }

    async fn is_ready(&self) -> bool { self.readiness_gaps().is_empty() }

    async fn submit(&self, order: &OrderRequest) -> Result<Acknowledgement, ExecutionError> {
        // Fail closed, and say precisely what is missing.
        let gaps = self.readiness_gaps();
        if !gaps.is_empty() {
            let msg = format!(
                "live execution is not configured; missing: {}. \
                 See docs/LIVE_MODE.md — no order was sent.",
                gaps.join(", ")
            );
            error!(order = %order.order_id, "{msg}");
            return Err(ExecutionError::NotReady(msg));
        }

        let signer = self.signer.as_ref().expect("checked by readiness_gaps");
        let creds = self.creds.as_ref().expect("checked by readiness_gaps");

        let signed = signer
            .sign(order, order.tick_size)
            .map_err(|e| ExecutionError::Auth(format!("order signing failed: {e}")))?;
        let body = signed.body.to_string();
        let ts = Utc::now().timestamp();
        let mut headers = creds.headers("POST", "/order", &body, ts);
        if let Ok(v) = HeaderValue::from_str(&signed.address) {
            headers.insert(HeaderName::from_static("poly_address"), v);
        }

        let sent_at = Utc::now();
        let resp = self
            .client
            .post(format!("{}/order", self.clob_url))
            .headers(headers)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await;

        match resp {
            Err(e) if e.is_timeout() => {
                // Critical: a timeout after sending may still have created an order.
                // Reporting this as a plain failure would lose a real position.
                warn!(order = %order.order_id, "live submit timed out; outcome unknown");
                Err(ExecutionError::Ambiguous(format!(
                    "timeout after sending order {}: it may exist at the venue", order.order_id)))
            }
            Err(e) => Err(ExecutionError::Transport(e.to_string())),
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(ExecutionError::Rejected(format!("HTTP {status}: {text}")));
                }
                // UNVERIFIED: the success-path body shape. Parsed defensively — an
                // unrecognised body yields an acknowledgement with no venue id rather
                // than a panic or a fabricated fill.
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                let venue_order_id = v.get("orderID")
                    .or_else(|| v.get("orderId"))
                    .or_else(|| v.get("id"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                info!(order = %order.order_id, ?venue_order_id, "live order acknowledged");
                Ok(Acknowledgement {
                    order_id: order.order_id,
                    venue_order_id,
                    accepted_at: sent_at,
                    // UNVERIFIED: whether immediate fills are returned inline. We assume
                    // not, and let reconciliation/the user channel discover fills. That
                    // is the safe direction: we never invent a fill.
                    immediate_fills: Vec::new(),
                    terminal: false,
                })
            }
        }
    }

    async fn cancel(&self, order_id: OrderId, venue_order_id: Option<&str>) -> Result<(), ExecutionError> {
        let Some(creds) = &self.creds else {
            return Err(ExecutionError::NotReady("no L2 credentials".into()));
        };
        let Some(vid) = venue_order_id else {
            return Err(ExecutionError::UnknownOrder(order_id));
        };
        // UNVERIFIED: cancellation request shape and response body.
        let body = serde_json::json!({ "orderID": vid }).to_string();
        let ts = Utc::now().timestamp();
        let headers = creds.headers("DELETE", "/order", &body, ts);
        let r = self
            .client
            .delete(format!("{}/order", self.clob_url))
            .headers(headers)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| ExecutionError::Transport(e.to_string()))?;
        if r.status().is_success() { Ok(()) }
        else {
            Err(ExecutionError::Rejected(format!("cancel HTTP {}", r.status())))
        }
    }

    async fn positions(&self) -> Result<Vec<VenuePosition>, ExecutionError> {
        let Some(addr) = &self.funder_address else {
            return Err(ExecutionError::NotReady("no funder address configured".into()));
        };
        // data-api /positions is public and verified reachable (400 without `user`).
        let url = format!("https://data-api.polymarket.com/positions?user={addr}");
        let r = self.client.get(&url).send().await
            .map_err(|e| ExecutionError::Transport(e.to_string()))?;
        if !r.status().is_success() {
            return Err(ExecutionError::Transport(format!("positions HTTP {}", r.status())));
        }
        let text = r.text().await.unwrap_or_default();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
        Ok(rows
            .iter()
            .filter_map(|v| {
                let token = TokenId::new(v.get("asset")?.as_str()?).ok()?;
                let size = v.get("size")?.as_f64()?;
                Some(VenuePosition { token_id: token, quantity: Qty::from_feed_f64(size.abs()).ok()? })
            })
            .collect())
    }

    async fn poll_fills(&self) -> Result<Vec<Fill>, ExecutionError> {
        // UNVERIFIED: `GET /data/trades` response shape (L2-gated, could not be sampled).
        // Returning empty rather than guessing; fills arrive via reconciliation until
        // this is confirmed against a funded account.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unarmed_adapter_lists_every_missing_prerequisite() {
        let l = LiveExecution::new("https://clob.polymarket.com".into(), 5000, false).unwrap();
        let gaps = l.readiness_gaps();
        assert_eq!(gaps.len(), 4, "should report all gaps at once, got {gaps:?}");
        assert!(gaps.iter().any(|g| g.contains("LIVE_TRADING_ENABLED")));
        assert!(gaps.iter().any(|g| g.contains("OrderSigner")));
    }

    #[tokio::test]
    async fn live_adapter_refuses_to_submit_without_a_signer() {
        // The central safety property: no silent no-op, and no fabricated acknowledgement.
        let l = LiveExecution::new("https://clob.polymarket.com".into(), 5000, true)
            .unwrap()
            .with_credentials(L2Credentials {
                api_key: "k".into(), secret: "c2VjcmV0".into(), passphrase: "p".into() })
            .with_funder("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f".into());
        assert!(!l.is_ready().await);
        let gaps = l.readiness_gaps();
        assert_eq!(gaps, vec!["an OrderSigner implementation (EIP-712 L1 signing)"]);
    }

    #[test]
    fn live_adapter_declares_itself_real_money() {
        let l = LiveExecution::new("https://clob.polymarket.com".into(), 5000, true).unwrap();
        assert!(l.capabilities().is_real_money);
        assert_eq!(l.name(), "live");
    }

    #[test]
    fn credentials_never_render_their_material() {
        let c = L2Credentials { api_key: "key123".into(), secret: "s3cr3t".into(), passphrase: "pass".into() };
        let s = format!("{c:?}");
        assert_eq!(s, "L2Credentials(<redacted>)");
        assert!(!s.contains("key123") && !s.contains("s3cr3t"));
    }

    #[test]
    fn hmac_matches_rfc4231_vector() {
        // RFC 4231 test case 1, so the hand-rolled HMAC is verified rather than assumed.
        // key = 0x0b * 20, data = "Hi There"
        let key = b64url_encode(&[0x0b; 20]);
        let got = hmac_sha256_b64(&key, "Hi There");
        let expect_hex = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
        let raw = b64url_decode(&got).unwrap();
        assert_eq!(hex_of(&raw), expect_hex);
    }

    fn hex_of(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn base64url_round_trips() {
        for v in [vec![], vec![1], vec![1, 2], vec![1, 2, 3], (0u8..=255).collect::<Vec<_>>()] {
            assert_eq!(b64url_decode(&b64url_encode(&v)).unwrap(), v);
        }
    }

    #[test]
    fn auth_headers_carry_every_required_field() {
        let c = L2Credentials { api_key: "k".into(), secret: "c2VjcmV0".into(), passphrase: "p".into() };
        let h = c.headers("POST", "/order", "{}", 1787102287);
        for k in ["poly_api_key", "poly_passphrase", "poly_timestamp", "poly_signature"] {
            assert!(h.contains_key(k), "missing header {k}");
        }
    }

    #[test]
    fn signature_changes_with_every_signed_component() {
        let c = L2Credentials { api_key: "k".into(), secret: "c2VjcmV0".into(), passphrase: "p".into() };
        let sig = |m: &str, p: &str, b: &str, t: i64| {
            c.headers(m, p, b, t).get("poly_signature").unwrap().to_str().unwrap().to_string()
        };
        let base = sig("POST", "/order", "{}", 1);
        assert_ne!(base, sig("DELETE", "/order", "{}", 1), "method must be signed");
        assert_ne!(base, sig("POST", "/orders", "{}", 1), "path must be signed");
        assert_ne!(base, sig("POST", "/order", "{\"a\":1}", 1), "body must be signed");
        assert_ne!(base, sig("POST", "/order", "{}", 2), "timestamp must be signed");
    }
}
