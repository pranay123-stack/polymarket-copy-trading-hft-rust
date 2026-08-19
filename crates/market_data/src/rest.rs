//! REST clients for CLOB and data-api.
//!
//! All Polymarket HTTP lives here. Nothing else in the codebase constructs a
//! Polymarket URL, so swapping or mocking the venue is a single-file change.

use std::time::Duration;

use chrono::{DateTime, Utc};
use domain::{Address, Market, MarketId, OrderBook, TokenId};
use reqwest::Client;
use tracing::{debug, warn};

use crate::parser::{
    parse_book_value, parse_data_api_trade, parse_gamma_market, ClobBook, DataApiTrade, GammaMarket,
    ParsedTrade,
};

#[derive(Debug, thiserror::Error)]
pub enum RestError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("parse error: {0}")]
    Parse(#[from] crate::parser::ParseError),
    #[error("{endpoint} returned {status}: {body}")]
    Status { endpoint: String, status: u16, body: String },
}

/// Thin, typed wrapper over the public Polymarket REST surface.
#[derive(Clone)]
pub struct PolymarketRest {
    client: Client,
    clob_url: String,
    gamma_url: String,
    data_api_url: String,
}

impl PolymarketRest {
    pub fn new(clob_url: String, gamma_url: String, data_api_url: String, timeout_ms: u64) -> Result<Self, RestError> {
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .connect_timeout(Duration::from_millis(timeout_ms.min(5_000)))
            // Gamma 403s some default agents and reqwest sends none.
            .user_agent(concat!("polymarket-copytrader/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client, clob_url, gamma_url, data_api_url })
    }

    async fn get_text(&self, url: &str) -> Result<String, RestError> {
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(RestError::Status {
                endpoint: url.to_string(),
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            });
        }
        Ok(body)
    }

    /// Order book for one outcome token, normalised best-first.
    pub async fn book(&self, token: &TokenId, seq: u64) -> Result<OrderBook, RestError> {
        let url = format!("{}/book?token_id={}", self.clob_url, token);
        let body = self.get_text(&url).await?;
        let raw: ClobBook = serde_json::from_str(&body).map_err(crate::parser::ParseError::Json)?;
        Ok(parse_book_value(raw, seq, Utc::now())?)
    }

    /// Markets that currently have an active book — used to seed the paper simulator.
    pub async fn sampling_markets(&self) -> Result<Vec<Market>, RestError> {
        #[derive(serde::Deserialize)]
        struct Wrap { data: Vec<serde_json::Value> }
        let body = self.get_text(&format!("{}/sampling-markets", self.clob_url)).await?;
        let w: Wrap = serde_json::from_str(&body).map_err(crate::parser::ParseError::Json)?;
        Ok(w.data.iter().filter_map(|v| clob_market_to_domain(v).ok()).collect())
    }

    /// Gamma metadata for the highest-volume open markets.
    pub async fn top_markets(&self, limit: u32) -> Result<Vec<Market>, RestError> {
        let url = format!(
            "{}/markets?closed=false&order=volumeNum&ascending=false&limit={}",
            self.gamma_url, limit
        );
        let body = self.get_text(&url).await?;
        let raw: Vec<GammaMarket> = serde_json::from_str(&body).map_err(crate::parser::ParseError::Json)?;
        Ok(raw
            .iter()
            .filter_map(|g| match parse_gamma_market(g) {
                Ok(m) => Some(m),
                Err(e) => {
                    debug!(error = %e, condition_id = %g.condition_id, "skipping unparseable market");
                    None
                }
            })
            .collect())
    }

    pub async fn market_by_condition(&self, id: &MarketId) -> Result<Option<Market>, RestError> {
        let url = format!("{}/markets?condition_ids={}", self.gamma_url, id);
        let body = self.get_text(&url).await?;
        let raw: Vec<GammaMarket> = serde_json::from_str(&body).map_err(crate::parser::ParseError::Json)?;
        Ok(raw.first().and_then(|g| parse_gamma_market(g).ok()))
    }

    /// Backfills a wallet's recent trades after a feed gap.
    ///
    /// **`takerOnly=false` is mandatory here.** The endpoint defaults to taker-only,
    /// which omits maker fills entirely — a target trader providing liquidity would be
    /// invisible to backfill while visible on the live feed, so reconciliation would
    /// silently disagree with itself. See `docs/POLYMARKET_API.md` §4.
    ///
    /// Bounded by `since` rather than offset paging: paging a live feed both duplicates
    /// and skips rows (measured: 160 dupes over 3000 rows).
    pub async fn backfill_wallet_trades(
        &self,
        wallet: &Address,
        since: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ParsedTrade>, RestError> {
        let url = format!(
            "{}/trades?user={}&takerOnly=false&limit={}",
            self.data_api_url, wallet, limit.min(1000)
        );
        let body = self.get_text(&url).await?;
        let raw: Vec<DataApiTrade> = serde_json::from_str(&body).map_err(crate::parser::ParseError::Json)?;
        let cutoff = since.timestamp();
        let now = Utc::now();
        let mut out = Vec::new();
        for t in raw.iter().filter(|t| t.timestamp >= cutoff) {
            match parse_data_api_trade(t, now) {
                Ok(p) => out.push(p),
                Err(e) => warn!(error = %e, tx = %t.transaction_hash, "unparseable backfill row"),
            }
        }
        Ok(out)
    }

    /// Server time, for clock-skew detection.
    pub async fn server_time(&self) -> Result<i64, RestError> {
        let body = self.get_text(&format!("{}/time", self.clob_url)).await?;
        Ok(body.trim().parse::<i64>().unwrap_or_default())
    }
}

/// CLOB `/markets` rows use a different shape from Gamma's — flat, already-decoded.
fn clob_market_to_domain(v: &serde_json::Value) -> Result<Market, crate::parser::ParseError> {
    use crate::parser::ParseError;
    let cid = v.get("condition_id").and_then(|x| x.as_str()).unwrap_or_default();
    let market_id = MarketId::new(cid)
        .map_err(|e| ParseError::Field { field: "condition_id", detail: e.to_string() })?;
    let outcomes = v
        .get("tokens")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    Some(domain::Outcome {
                        token_id: TokenId::new(t.get("token_id")?.as_str()?).ok()?,
                        name: t.get("outcome")?.as_str()?.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Market {
        market_id,
        slug: v.get("market_slug").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
        title: v.get("question").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
        outcomes,
        tick_size: v.get("minimum_tick_size").and_then(|x| x.as_f64())
            .and_then(|f| rust_decimal::Decimal::try_from(f).ok())
            .unwrap_or(rust_decimal::Decimal::new(1, 2)),
        min_order_size: v.get("minimum_order_size").and_then(|x| x.as_f64())
            .and_then(|f| rust_decimal::Decimal::try_from(f).ok())
            .unwrap_or(rust_decimal::Decimal::new(5, 0)),
        neg_risk: v.get("neg_risk").and_then(|x| x.as_bool()).unwrap_or(false),
        active: v.get("active").and_then(|x| x.as_bool()).unwrap_or(false),
        closed: v.get("closed").and_then(|x| x.as_bool()).unwrap_or(true),
        accepting_orders: v.get("accepting_orders").and_then(|x| x.as_bool()).unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clob_market_shape_is_parsed() {
        let v: serde_json::Value = serde_json::from_str(r#"{
            "condition_id":"0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52",
            "question":"Q?","market_slug":"q",
            "minimum_order_size":5,"minimum_tick_size":0.01,
            "active":true,"closed":false,"accepting_orders":true,"neg_risk":false,
            "tokens":[{"token_id":"123","outcome":"Yes"},{"token_id":"456","outcome":"No"}]}"#).unwrap();
        let m = clob_market_to_domain(&v).unwrap();
        assert_eq!(m.outcomes.len(), 2);
        assert_eq!(m.outcomes[0].name, "Yes");
        assert!(m.is_tradable());
    }

    #[test]
    fn market_missing_fields_defaults_to_not_tradable() {
        // Fail closed: an unparseable/incomplete market must never look tradable.
        let v: serde_json::Value = serde_json::from_str(r#"{
            "condition_id":"0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52"}"#).unwrap();
        let m = clob_market_to_domain(&v).unwrap();
        assert!(!m.is_tradable());
        assert!(m.closed);
    }

    #[test]
    fn bad_condition_id_is_an_error() {
        let v: serde_json::Value = serde_json::from_str(r#"{"condition_id":"nope"}"#).unwrap();
        assert!(clob_market_to_domain(&v).is_err());
    }
}
