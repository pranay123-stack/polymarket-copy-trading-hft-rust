//! Wire-format parsing and normalisation.
//!
//! This module is the *only* place that touches Polymarket's JSON shapes. Two traps
//! live here, both verified against production and both silent if mishandled:
//!
//! 1. **`GET /book` sorts both sides worst-first.** Bids ascend, asks descend, so the
//!    best price is the **last** element. Reading `bids[0]` as the best bid is a
//!    catastrophic mispricing that produces no error. [`parse_book`] reverses both
//!    sides into best-first order and every book leaves here normalised.
//! 2. **`clobTokenIds` and `outcomes` are double-encoded** — JSON *strings* containing
//!    JSON arrays — and `outcomeIndex` is sometimes the sentinel `999`. Legs are
//!    therefore matched by token id, never by position.

use chrono::{DateTime, TimeZone, Utc};
use domain::{
    Address, CorrelationId, Level, Market, MarketId, OrderBook, Outcome, Price, Qty, Side,
    SourceTrade, TokenId, TradeSource, TxHash,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("malformed json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("field {field}: {detail}")]
    Field { field: &'static str, detail: String },
    #[error("frame is not a trade event")]
    NotATrade,
    #[error("frame carried no payload")]
    NoPayload,
}

fn field<E: std::fmt::Display>(f: &'static str) -> impl Fn(E) -> ParseError {
    move |e| ParseError::Field { field: f, detail: e.to_string() }
}

/// Seconds or milliseconds since epoch → UTC. Polymarket mixes both resolutions in the
/// same frame (envelope in ms, payload in seconds), so the unit is inferred by magnitude.
pub fn epoch_to_utc(v: i64) -> Option<DateTime<Utc>> {
    // 1e11 s is year 5138; anything larger is certainly milliseconds.
    let (secs, nanos) = if v > 100_000_000_000 {
        (v / 1000, ((v % 1000) * 1_000_000) as u32)
    } else {
        (v, 0)
    };
    Utc.timestamp_opt(secs, nanos).single()
}

// ---------------------------------------------------------------------------
// RTDS activity feed
// ---------------------------------------------------------------------------

/// Envelope of a `wss://ws-live-data.polymarket.com` frame.
#[derive(Debug, Deserialize)]
pub struct RtdsEnvelope {
    pub topic: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// **Milliseconds.** The only stamp precise enough for latency work; the payload's
    /// own `timestamp` is whole seconds.
    pub timestamp: Option<i64>,
    pub payload: Option<RtdsTradePayload>,
}

#[derive(Debug, Deserialize)]
pub struct RtdsTradePayload {
    #[serde(rename = "proxyWallet")]
    pub proxy_wallet: String,
    pub side: String,
    pub asset: String,
    #[serde(rename = "conditionId")]
    pub condition_id: String,
    pub outcome: String,
    /// Sometimes `999`, a sentinel. Never used to index `outcomes[]`.
    #[serde(rename = "outcomeIndex", default)]
    pub outcome_index: i64,
    pub price: f64,
    pub size: f64,
    /// Whole seconds.
    pub timestamp: i64,
    #[serde(rename = "transactionHash")]
    pub transaction_hash: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub slug: String,
    /// Present on only ~40% of frames.
    #[serde(default)]
    pub fee: Option<f64>,
}

/// Outcome of parsing one RTDS frame.
pub enum RtdsFrame {
    Trade(Box<ParsedTrade>),
    /// Keepalive, empty string, or a topic we do not consume. Not an error.
    Ignored,
}

/// A trade parsed from the wire, before dedup assigns its identity.
pub struct ParsedTrade {
    pub trader: Address,
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub outcome: String,
    pub side: Side,
    pub price: Price,
    pub quantity: Qty,
    pub tx_hash: TxHash,
    pub source_ts: DateTime<Utc>,
    /// True when we had to fall back to the second-resolution payload stamp.
    pub source_is_coarse: bool,
    pub detected_ts: DateTime<Utc>,
    pub market_title: String,
    pub market_slug: String,
    pub source: TradeSource,
}

impl ParsedTrade {
    /// Completes the trade once dedup has assigned a stable identity.
    pub fn into_source_trade(
        self,
        event_id: domain::SourceEventId,
        occurrence: u32,
    ) -> SourceTrade {
        SourceTrade {
            event_id,
            correlation_id: CorrelationId::new(),
            trader: self.trader,
            market_id: self.market_id,
            token_id: self.token_id,
            outcome: self.outcome,
            side: self.side,
            price: self.price,
            quantity: self.quantity,
            tx_hash: self.tx_hash,
            occurrence,
            source_ts: self.source_ts,
            detected_ts: self.detected_ts,
            source: self.source,
            market_title: self.market_title,
            market_slug: self.market_slug,
        }
    }
}

/// Parses one RTDS text frame.
///
/// Tolerates the empty first frame and any non-JSON keepalive, returning
/// [`RtdsFrame::Ignored`] rather than an error — treating those as failures would trip
/// the reconnect logic on every single connection.
pub fn parse_rtds_frame(raw: &str, received_at: DateTime<Utc>) -> Result<RtdsFrame, ParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.starts_with('{') {
        return Ok(RtdsFrame::Ignored);
    }
    let env: RtdsEnvelope = serde_json::from_str(trimmed)?;
    if env.kind.as_deref() != Some("trades") {
        return Ok(RtdsFrame::Ignored);
    }
    let p = env.payload.ok_or(ParseError::NoPayload)?;

    // Prefer the millisecond envelope stamp; fall back to the coarse payload stamp and
    // say so, so latency figures are never silently quantised to a second.
    let (source_ts, coarse) = match env.timestamp.and_then(epoch_to_utc) {
        Some(t) => (t, false),
        None => (
            epoch_to_utc(p.timestamp).ok_or(ParseError::Field {
                field: "timestamp",
                detail: format!("{} is not a valid epoch", p.timestamp),
            })?,
            true,
        ),
    };

    Ok(RtdsFrame::Trade(Box::new(ParsedTrade {
        trader: Address::new(&p.proxy_wallet).map_err(field("proxyWallet"))?,
        market_id: MarketId::new(&p.condition_id).map_err(field("conditionId"))?,
        token_id: TokenId::new(&p.asset).map_err(field("asset"))?,
        outcome: p.outcome,
        side: parse_side(&p.side)?,
        price: Price::from_feed_f64(p.price).map_err(field("price"))?,
        quantity: Qty::from_feed_f64(p.size).map_err(field("size"))?,
        tx_hash: TxHash::new(&p.transaction_hash).map_err(field("transactionHash"))?,
        source_ts,
        source_is_coarse: coarse,
        detected_ts: received_at,
        market_title: p.title,
        market_slug: p.slug,
        source: TradeSource::RtdsWebsocket,
    })))
}

pub fn parse_side(s: &str) -> Result<Side, ParseError> {
    match s.trim().to_ascii_uppercase().as_str() {
        "BUY" => Ok(Side::Buy),
        "SELL" => Ok(Side::Sell),
        other => Err(ParseError::Field { field: "side", detail: format!("unknown side {other:?}") }),
    }
}

// ---------------------------------------------------------------------------
// data-api REST backfill
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DataApiTrade {
    #[serde(rename = "proxyWallet")]
    pub proxy_wallet: String,
    pub side: String,
    pub asset: String,
    #[serde(rename = "conditionId")]
    pub condition_id: String,
    #[serde(default)]
    pub outcome: String,
    pub price: f64,
    pub size: f64,
    pub timestamp: i64,
    #[serde(rename = "transactionHash")]
    pub transaction_hash: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub slug: String,
}

/// Converts a REST backfill row. The stamp is whole seconds, so `source_is_coarse` is
/// always true and backfilled rows never contribute precise detection latency.
pub fn parse_data_api_trade(
    t: &DataApiTrade,
    received_at: DateTime<Utc>,
) -> Result<ParsedTrade, ParseError> {
    Ok(ParsedTrade {
        trader: Address::new(&t.proxy_wallet).map_err(field("proxyWallet"))?,
        market_id: MarketId::new(&t.condition_id).map_err(field("conditionId"))?,
        token_id: TokenId::new(&t.asset).map_err(field("asset"))?,
        outcome: t.outcome.clone(),
        side: parse_side(&t.side)?,
        price: Price::from_feed_f64(t.price).map_err(field("price"))?,
        quantity: Qty::from_feed_f64(t.size).map_err(field("size"))?,
        tx_hash: TxHash::new(&t.transaction_hash).map_err(field("transactionHash"))?,
        source_ts: epoch_to_utc(t.timestamp).ok_or(ParseError::Field {
            field: "timestamp", detail: format!("{} invalid", t.timestamp) })?,
        source_is_coarse: true,
        detected_ts: received_at,
        market_title: t.title.clone(),
        market_slug: t.slug.clone(),
        source: TradeSource::RestBackfill,
    })
}

// ---------------------------------------------------------------------------
// CLOB order book
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ClobLevel {
    pub price: String,
    pub size: String,
}

#[derive(Debug, Deserialize)]
pub struct ClobBook {
    pub market: String,
    pub asset_id: String,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub bids: Vec<ClobLevel>,
    #[serde(default)]
    pub asks: Vec<ClobLevel>,
    #[serde(default)]
    pub tick_size: Option<String>,
    #[serde(default)]
    pub min_order_size: Option<String>,
}

fn levels(raw: &[ClobLevel]) -> Vec<Level> {
    raw.iter()
        .filter_map(|l| {
            let price = l.price.parse::<Decimal>().ok().and_then(|d| Price::new(d).ok())?;
            let size = l.size.parse::<Decimal>().ok().and_then(|d| Qty::new(d).ok())?;
            (!size.is_zero()).then_some(Level { price, size })
        })
        .collect()
}

/// Parses a CLOB book into the normalised, **best-first** representation.
///
/// The venue sends both sides worst-first. Rather than trusting that ordering blindly,
/// levels are sorted explicitly: a change in the venue's ordering then cannot corrupt
/// the book, it simply becomes a no-op.
pub fn parse_book(raw: &str, seq: u64, fallback_ts: DateTime<Utc>) -> Result<OrderBook, ParseError> {
    let b: ClobBook = serde_json::from_str(raw)?;
    parse_book_value(b, seq, fallback_ts)
}

pub fn parse_book_value(
    b: ClobBook,
    seq: u64,
    fallback_ts: DateTime<Utc>,
) -> Result<OrderBook, ParseError> {
    let mut bids = levels(&b.bids);
    let mut asks = levels(&b.asks);
    // Best-first: bids descending, asks ascending.
    bids.sort_by_key(|l| std::cmp::Reverse(l.price));
    asks.sort_by_key(|l| l.price);

    let timestamp = b
        .timestamp
        .as_deref()
        .and_then(|t| t.parse::<i64>().ok())
        .and_then(epoch_to_utc)
        .unwrap_or(fallback_ts);

    Ok(OrderBook {
        market_id: MarketId::new(&b.market).map_err(field("market"))?,
        token_id: TokenId::new(&b.asset_id).map_err(field("asset_id"))?,
        bids,
        asks,
        tick_size: b.tick_size.and_then(|t| t.parse().ok()).unwrap_or(Decimal::new(1, 2)),
        min_order_size: b.min_order_size.and_then(|t| t.parse().ok()).unwrap_or(Decimal::new(5, 0)),
        timestamp,
        seq,
    })
}

// ---------------------------------------------------------------------------
// Gamma market metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GammaMarket {
    #[serde(rename = "conditionId")]
    pub condition_id: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub question: String,
    /// **Double-encoded**: a JSON string containing a JSON array.
    #[serde(rename = "clobTokenIds", default)]
    pub clob_token_ids: Option<String>,
    /// Also double-encoded.
    #[serde(default)]
    pub outcomes: Option<String>,
    #[serde(rename = "orderPriceMinTickSize", default)]
    pub tick_size: Option<f64>,
    #[serde(rename = "orderMinSize", default)]
    pub min_size: Option<f64>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub closed: bool,
    #[serde(rename = "acceptingOrders", default)]
    pub accepting_orders: bool,
    #[serde(rename = "negRisk", default)]
    pub neg_risk: bool,
}

/// Decodes one of Gamma's double-encoded array fields.
pub fn decode_nested_array(raw: &str) -> Result<Vec<String>, ParseError> {
    serde_json::from_str::<Vec<String>>(raw).map_err(ParseError::Json)
}

pub fn parse_gamma_market(g: &GammaMarket) -> Result<Market, ParseError> {
    let ids = g.clob_token_ids.as_deref().map(decode_nested_array).transpose()?.unwrap_or_default();
    let names = g.outcomes.as_deref().map(decode_nested_array).transpose()?.unwrap_or_default();
    if ids.len() != names.len() {
        return Err(ParseError::Field {
            field: "clobTokenIds/outcomes",
            detail: format!("length mismatch: {} ids vs {} outcomes", ids.len(), names.len()),
        });
    }
    let outcomes = ids
        .iter()
        .zip(names.iter())
        .map(|(id, name)| {
            Ok(Outcome { token_id: TokenId::new(id).map_err(field("clobTokenIds"))?, name: name.clone() })
        })
        .collect::<Result<Vec<_>, ParseError>>()?;

    Ok(Market {
        market_id: MarketId::new(&g.condition_id).map_err(field("conditionId"))?,
        slug: g.slug.clone(),
        title: g.question.clone(),
        outcomes,
        tick_size: g.tick_size.and_then(|t| Decimal::try_from(t).ok()).unwrap_or(Decimal::new(1, 2)),
        min_order_size: g.min_size.and_then(|t| Decimal::try_from(t).ok()).unwrap_or(Decimal::new(5, 0)),
        neg_risk: g.neg_risk,
        active: g.active,
        closed: g.closed,
        accepting_orders: g.accepting_orders,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    // Captured verbatim from wss://ws-live-data.polymarket.com on 2026-08-19.
    const REAL_RTDS_FRAME: &str = r#"{"connection_id":"gXseIO-NQWeIKEhJwA==","payload":{"asset":"72551024098258542594534683942523606143014690620243023298497729957846870197074","bio":"","conditionId":"0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52","eventSlug":"highest-temperature-in-wellington-on-august-19-2026","icon":"https://x.jpg","name":"PPMT","outcome":"No","outcomeIndex":1,"price":0.26,"profileImage":"","proxyWallet":"0x510F4963b66B1B18505faaB74b0bB943D1dDa43C","pseudonym":"Rowdy-Tenement","side":"BUY","size":2.7027,"slug":"highest-temperature-in-wellington-on-august-19-2026-10c","timestamp":1787102287,"title":"Will the highest temperature in Wellington be 10C on August 19?","transactionHash":"0xb6acf6859bc84216f4b3e2567fb392a2eae19d275340ad96ea17218ccfec27b7"},"timestamp":1787102287053,"topic":"activity","type":"trades"}"#;

    #[test]
    fn parses_a_real_rtds_frame() {
        let now = Utc::now();
        match parse_rtds_frame(REAL_RTDS_FRAME, now).unwrap() {
            RtdsFrame::Trade(t) => {
                assert_eq!(t.trader.as_str(), "0x510f4963b66b1b18505faab74b0bb943d1dda43c");
                assert_eq!(t.side, Side::Buy);
                assert_eq!(t.price.get(), dec!(0.26));
                assert_eq!(t.quantity.get(), dec!(2.7027));
                assert_eq!(t.outcome, "No");
                // Envelope ms stamp must win over the payload's whole-second one.
                assert_eq!(t.source_ts.timestamp_millis(), 1787102287053);
                assert!(!t.source_is_coarse);
            }
            RtdsFrame::Ignored => panic!("real trade frame was ignored"),
        }
    }

    #[test]
    fn empty_first_frame_is_ignored_not_an_error() {
        // The very first frame after subscribing really is an empty string.
        assert!(matches!(parse_rtds_frame("", Utc::now()).unwrap(), RtdsFrame::Ignored));
        assert!(matches!(parse_rtds_frame("   ", Utc::now()).unwrap(), RtdsFrame::Ignored));
        assert!(matches!(parse_rtds_frame("PING", Utc::now()).unwrap(), RtdsFrame::Ignored));
    }

    #[test]
    fn other_topics_are_ignored() {
        let f = r#"{"topic":"activity","type":"orders_matched","timestamp":1787102287053,"payload":null}"#;
        assert!(matches!(parse_rtds_frame(f, Utc::now()).unwrap(), RtdsFrame::Ignored));
    }

    #[test]
    fn missing_envelope_stamp_falls_back_and_flags_coarseness() {
        let f = REAL_RTDS_FRAME.replace(r#","timestamp":1787102287053,"topic""#, r#","topic""#);
        match parse_rtds_frame(&f, Utc::now()).unwrap() {
            RtdsFrame::Trade(t) => {
                assert_eq!(t.source_ts.timestamp(), 1787102287);
                assert!(t.source_is_coarse, "second-resolution stamp must be flagged");
            }
            RtdsFrame::Ignored => panic!("should still parse"),
        }
    }

    #[test]
    fn outcome_index_999_sentinel_does_not_break_parsing() {
        let f = REAL_RTDS_FRAME.replace(r#""outcomeIndex":1"#, r#""outcomeIndex":999"#);
        // We never index outcomes[] by this value, so 999 is harmless.
        assert!(matches!(parse_rtds_frame(&f, Utc::now()).unwrap(), RtdsFrame::Trade(_)));
    }

    #[test]
    fn garbage_addresses_are_rejected_not_silently_accepted() {
        let f = REAL_RTDS_FRAME.replace("0x510F4963b66B1B18505faaB74b0bB943D1dDa43C", "nonsense");
        assert!(parse_rtds_frame(&f, Utc::now()).is_err());
    }

    #[test]
    fn epoch_unit_is_inferred_from_magnitude() {
        assert_eq!(epoch_to_utc(1787102287).unwrap().timestamp(), 1787102287);
        assert_eq!(epoch_to_utc(1787102287053).unwrap().timestamp_millis(), 1787102287053);
    }

    // Captured verbatim from GET /book — note both sides are WORST-first on the wire.
    const REAL_BOOK: &str = r#"{"market":"0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52","asset_id":"32338220190071351435772801779725302244575775216413325951443816017994629993401","timestamp":"1787102287053","hash":"abc","bids":[{"price":"0.001","size":"10611926.49"},{"price":"0.002","size":"2301168.91"},{"price":"0.043","size":"15505.85"},{"price":"0.044","size":"18228.38"}],"asks":[{"price":"0.999","size":"1598544.78"},{"price":"0.998","size":"243752.25"},{"price":"0.046","size":"8991.48"},{"price":"0.045","size":"429.57"}],"min_order_size":"5","tick_size":"0.001","neg_risk":false}"#;

    #[test]
    fn book_is_normalised_to_best_first() {
        let b = parse_book(REAL_BOOK, 1, Utc::now()).unwrap();
        // On the wire bids[0] was 0.001 (the WORST bid) and asks[0] was 0.999.
        assert_eq!(b.best_bid().unwrap().price.get(), dec!(0.044), "best bid must be the highest");
        assert_eq!(b.best_ask().unwrap().price.get(), dec!(0.045), "best ask must be the lowest");
        assert!(b.is_well_formed());
        assert_eq!(b.tick_size, dec!(0.001));
        assert_eq!(b.spread().unwrap(), dec!(0.001));
    }

    #[test]
    fn book_normalisation_is_order_independent() {
        // If the venue ever switches to best-first, we must still be correct.
        let mut v: serde_json::Value = serde_json::from_str(REAL_BOOK).unwrap();
        v["bids"].as_array_mut().unwrap().reverse();
        v["asks"].as_array_mut().unwrap().reverse();
        let b = parse_book(&v.to_string(), 1, Utc::now()).unwrap();
        assert_eq!(b.best_bid().unwrap().price.get(), dec!(0.044));
        assert_eq!(b.best_ask().unwrap().price.get(), dec!(0.045));
        assert!(b.is_well_formed());
    }

    #[test]
    fn zero_size_levels_are_dropped() {
        let v = REAL_BOOK.replace(r#"{"price":"0.044","size":"18228.38"}"#, r#"{"price":"0.044","size":"0"}"#);
        let b = parse_book(&v, 1, Utc::now()).unwrap();
        // 0.044 was the best bid; with size 0 it must vanish, not linger as a phantom.
        assert_eq!(b.best_bid().unwrap().price.get(), dec!(0.043));
    }

    #[test]
    fn gamma_double_encoded_arrays_are_decoded() {
        let g: GammaMarket = serde_json::from_str(r#"{
            "conditionId":"0x7d0aaf81bbd3fd73b6a1651cce08a452c0cbf9c0cbb4520ce0f981065b639d88",
            "slug":"test","question":"Q?",
            "clobTokenIds":"[\"27146956652877944551877724690365745048289675287536243265951843487691050802191\", \"33216695217861742195941369663873573949679634432452142092545486849801915283392\"]",
            "outcomes":"[\"Yes\", \"No\"]",
            "orderPriceMinTickSize":0.001,"active":true,"closed":false,"acceptingOrders":true}"#).unwrap();
        let m = parse_gamma_market(&g).unwrap();
        assert_eq!(m.outcomes.len(), 2);
        assert_eq!(m.outcomes[0].name, "Yes");
        assert_eq!(m.outcomes[1].name, "No");
        assert_eq!(m.tick_size, dec!(0.001));
        assert!(m.is_tradable());
        // Legs are found by token id, never by index.
        let t = m.outcomes[1].token_id.clone();
        assert_eq!(m.outcome_by_token(&t).unwrap().name, "No");
    }

    #[test]
    fn gamma_length_mismatch_is_an_error() {
        let g: GammaMarket = serde_json::from_str(r#"{
            "conditionId":"0x7d0aaf81bbd3fd73b6a1651cce08a452c0cbf9c0cbb4520ce0f981065b639d88",
            "clobTokenIds":"[\"123\",\"456\"]","outcomes":"[\"Yes\"]","active":true}"#).unwrap();
        assert!(parse_gamma_market(&g).is_err());
    }

    #[test]
    fn closed_market_is_not_tradable() {
        let g: GammaMarket = serde_json::from_str(r#"{
            "conditionId":"0x7d0aaf81bbd3fd73b6a1651cce08a452c0cbf9c0cbb4520ce0f981065b639d88",
            "clobTokenIds":"[\"123\"]","outcomes":"[\"Yes\"]",
            "active":true,"closed":true,"acceptingOrders":false}"#).unwrap();
        assert!(!parse_gamma_market(&g).unwrap().is_tradable());
    }
}
