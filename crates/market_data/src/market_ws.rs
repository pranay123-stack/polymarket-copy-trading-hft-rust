//! CLOB market-channel client — streaming order books.
//!
//! Replaces per-signal REST book fetches on the hot path. Verified against production on
//! 2026-08-19:
//!
//! * subscribe with `{"assets_ids": [...], "type": "market"}`;
//! * **the first frame is a JSON array** of `book` snapshots, one per subscribed token;
//!   every later frame is a single object. Parsing only the object shape breaks on connect;
//! * `book` frames carry `bids`/`asks` **worst-first**, exactly like `GET /book`, so the
//!   same normalisation applies;
//! * `price_change` frames batch several changes under `price_changes`, each with its own
//!   `asset_id` — so they must be routed per asset, not per frame — and each carries
//!   `best_bid`/`best_ask`, which gives a free integrity check;
//! * `tick_size_change` exists and matters: quoting against a stale tick is rejected.
//!
//! A `price_change` size is the **new aggregate size at that level**, not a delta, and a
//! size of `0` deletes the level. Treating it as a delta silently corrupts the book.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use domain::{Level, MarketId, OrderBook, Price, Qty, TokenId};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::parser::{epoch_to_utc, ParseError};

/// Builds the subscription frame for a set of tokens.
pub fn subscribe_frame(tokens: &[TokenId]) -> String {
    serde_json::json!({
        "assets_ids": tokens.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
        "type": "market",
    })
    .to_string()
}

#[derive(Debug, Deserialize)]
struct WireLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct WireBook {
    #[serde(default)]
    market: String,
    #[serde(default)]
    asset_id: String,
    #[serde(default)]
    bids: Vec<WireLevel>,
    #[serde(default)]
    asks: Vec<WireLevel>,
    #[serde(default)]
    tick_size: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WirePriceChange {
    asset_id: String,
    price: String,
    size: String,
    side: String,
    #[serde(default)]
    best_bid: Option<String>,
    #[serde(default)]
    best_ask: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireFrame {
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    market: String,
    #[serde(default)]
    asset_id: String,
    #[serde(default)]
    timestamp: Option<String>,
    // book
    #[serde(default)]
    bids: Vec<WireLevel>,
    #[serde(default)]
    asks: Vec<WireLevel>,
    #[serde(default)]
    tick_size: Option<String>,
    // price_change
    #[serde(default)]
    price_changes: Vec<WirePriceChange>,
    // tick_size_change
    #[serde(default)]
    new_tick_size: Option<String>,
}

/// What one frame told us.
#[derive(Debug, Clone, PartialEq)]
pub enum MarketEvent {
    /// A full snapshot; replaces any existing book for the token.
    Snapshot(Box<OrderBook>),
    /// One level replaced (or deleted when size is zero).
    LevelChange {
        token_id: TokenId,
        side: domain::Side,
        price: Price,
        /// The **new aggregate** size at this level. Zero deletes it.
        size: Decimal,
        best_bid: Option<Price>,
        best_ask: Option<Price>,
    },
    TickSizeChanged { token_id: TokenId, tick_size: Decimal },
    /// Keepalive, unknown type, or a frame we do not consume.
    Ignored,
}

fn levels(raw: &[WireLevel]) -> Vec<Level> {
    raw.iter()
        .filter_map(|l| {
            let p = l.price.parse::<Decimal>().ok().and_then(|d| Price::new(d).ok())?;
            let s = l.size.parse::<Decimal>().ok().and_then(|d| Qty::new(d).ok())?;
            (!s.is_zero()).then_some(Level { price: p, size: s })
        })
        .collect()
}

/// Parses one text frame into zero or more events.
///
/// Returns a vector because both the array-shaped first frame and `price_change` batches
/// carry multiple independent updates.
pub fn parse_market_frame(
    raw: &str,
    seq: u64,
    received_at: DateTime<Utc>,
) -> Result<Vec<MarketEvent>, ParseError> {
    let t = raw.trim();
    if t.is_empty() || !(t.starts_with('{') || t.starts_with('[')) {
        return Ok(vec![MarketEvent::Ignored]);
    }
    // Handle both the array first-frame and the single-object steady state.
    let frames: Vec<WireFrame> = if t.starts_with('[') {
        serde_json::from_str(t)?
    } else {
        vec![serde_json::from_str(t)?]
    };

    let mut out = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        match f.event_type.as_str() {
            "book" => {
                let b = WireBook {
                    market: f.market.clone(),
                    asset_id: f.asset_id.clone(),
                    bids: f.bids.iter().map(|l| WireLevel { price: l.price.clone(), size: l.size.clone() }).collect(),
                    asks: f.asks.iter().map(|l| WireLevel { price: l.price.clone(), size: l.size.clone() }).collect(),
                    tick_size: f.tick_size.clone(),
                    timestamp: f.timestamp.clone(),
                };
                if let Some(ob) = to_book(&b, seq + i as u64, received_at) {
                    out.push(MarketEvent::Snapshot(Box::new(ob)));
                }
            }
            "price_change" => {
                for c in &f.price_changes {
                    let (Ok(token), Ok(price)) = (
                        TokenId::new(&c.asset_id),
                        c.price.parse::<Decimal>().map_err(|_| ()).and_then(|d| Price::new(d).map_err(|_| ())),
                    ) else { continue };
                    let Ok(size) = c.size.parse::<Decimal>() else { continue };
                    let side = match c.side.to_ascii_uppercase().as_str() {
                        "BUY" => domain::Side::Buy,
                        "SELL" => domain::Side::Sell,
                        _ => continue,
                    };
                    out.push(MarketEvent::LevelChange {
                        token_id: token,
                        side,
                        price,
                        size,
                        best_bid: c.best_bid.as_deref()
                            .and_then(|s| s.parse::<Decimal>().ok())
                            .and_then(|d| Price::new(d).ok()),
                        best_ask: c.best_ask.as_deref()
                            .and_then(|s| s.parse::<Decimal>().ok())
                            .and_then(|d| Price::new(d).ok()),
                    });
                }
            }
            "tick_size_change" => {
                if let (Ok(token), Some(ts)) = (
                    TokenId::new(&f.asset_id),
                    f.new_tick_size.as_deref().and_then(|s| s.parse::<Decimal>().ok()),
                ) {
                    out.push(MarketEvent::TickSizeChanged { token_id: token, tick_size: ts });
                }
            }
            _ => out.push(MarketEvent::Ignored),
        }
    }
    if out.is_empty() {
        out.push(MarketEvent::Ignored);
    }
    Ok(out)
}

fn to_book(b: &WireBook, seq: u64, fallback: DateTime<Utc>) -> Option<OrderBook> {
    let mut bids = levels(&b.bids);
    let mut asks = levels(&b.asks);
    // Same worst-first trap as the REST endpoint; normalise explicitly.
    bids.sort_by(|x, y| y.price.cmp(&x.price));
    asks.sort_by(|x, y| x.price.cmp(&y.price));
    Some(OrderBook {
        market_id: MarketId::new(&b.market).ok()?,
        token_id: TokenId::new(&b.asset_id).ok()?,
        bids,
        asks,
        tick_size: b.tick_size.as_deref().and_then(|s| s.parse().ok()).unwrap_or(Decimal::new(1, 2)),
        // The WS book frame omits min_order_size; the REST/metadata value is authoritative
        // and is re-applied by the cache.
        min_order_size: Decimal::new(5, 0),
        timestamp: b.timestamp.as_deref()
            .and_then(|t| t.parse::<i64>().ok())
            .and_then(epoch_to_utc)
            .unwrap_or(fallback),
        seq,
    })
}

/// Maintains books from a stream of [`MarketEvent`]s.
#[derive(Default)]
pub struct BookBuilder {
    books: HashMap<TokenId, OrderBook>,
    /// Preserved across snapshots, since WS book frames omit it.
    min_order_size: HashMap<TokenId, Decimal>,
    pub integrity_failures: u64,
}

impl BookBuilder {
    pub fn new() -> Self { Self::default() }

    /// Records the venue's minimum order size, learned from REST metadata.
    pub fn set_min_order_size(&mut self, t: TokenId, v: Decimal) {
        self.min_order_size.insert(t, v);
    }

    pub fn get(&self, t: &TokenId) -> Option<&OrderBook> { self.books.get(t) }
    pub fn len(&self) -> usize { self.books.len() }
    pub fn is_empty(&self) -> bool { self.books.is_empty() }

    /// Applies an event, returning the token whose book changed.
    pub fn apply(&mut self, ev: MarketEvent, at: DateTime<Utc>) -> Option<TokenId> {
        match ev {
            MarketEvent::Ignored => None,
            MarketEvent::Snapshot(mut b) => {
                if let Some(m) = self.min_order_size.get(&b.token_id) {
                    b.min_order_size = *m;
                }
                let t = b.token_id.clone();
                self.books.insert(t.clone(), *b);
                Some(t)
            }
            MarketEvent::TickSizeChanged { token_id, tick_size } => {
                // Quoting against a stale tick gets orders rejected, so this is applied
                // even though it does not change any level.
                if let Some(b) = self.books.get_mut(&token_id) {
                    b.tick_size = tick_size;
                    b.timestamp = at;
                }
                Some(token_id)
            }
            MarketEvent::LevelChange { token_id, side, price, size, best_bid, best_ask } => {
                let b = self.books.get_mut(&token_id)?;
                let levels = match side {
                    domain::Side::Buy => &mut b.bids,
                    domain::Side::Sell => &mut b.asks,
                };
                // Size is the new aggregate at this level; zero removes it.
                levels.retain(|l| l.price != price);
                if size > Decimal::ZERO {
                    if let Ok(q) = Qty::new(size) {
                        levels.push(Level { price, size: q });
                    }
                }
                match side {
                    domain::Side::Buy => levels.sort_by(|x, y| y.price.cmp(&x.price)),
                    domain::Side::Sell => levels.sort_by(|x, y| x.price.cmp(&y.price)),
                }
                b.timestamp = at;
                b.seq += 1;

                // The venue tells us its own best bid/ask on every change. If ours
                // disagrees we have drifted, and the book must be resynced rather than
                // quietly mispriced. Snapshots republish every few seconds, so
                // invalidating is safe and self-healing.
                let drifted = match (best_bid, best_ask) {
                    (Some(bb), _) if b.best_bid().map(|l| l.price) != Some(bb) => true,
                    (_, Some(ba)) if b.best_ask().map(|l| l.price) != Some(ba) => true,
                    _ => false,
                };
                if drifted {
                    self.integrity_failures += 1;
                    self.books.remove(&token_id);
                }
                Some(token_id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const MARKET: &str = "0xa467b14d51f01b957109d9cbb1d6c124fab2a089d52ed8f471d23c2812e743b7";
    const TOKEN: &str = "32338220190071351435772801779725302244575775216413325951443816017994629993401";

    /// Shaped exactly like the production first frame: an ARRAY of book snapshots,
    /// with both sides worst-first.
    fn array_first_frame() -> String {
        format!(
            r#"[{{"event_type":"book","market":"{MARKET}","asset_id":"{TOKEN}",
            "bids":[{{"price":"0.001","size":"10611926.49"}},{{"price":"0.043","size":"100"}},{{"price":"0.044","size":"21577.95"}}],
            "asks":[{{"price":"0.999","size":"1598544.78"}},{{"price":"0.046","size":"200"}},{{"price":"0.045","size":"1953.32"}}],
            "tick_size":"0.001","timestamp":"1787102287053","hash":"abc"}}]"#
        )
    }

    #[test]
    fn array_first_frame_parses() {
        // Parsing only the object shape breaks on every single connection.
        let evs = parse_market_frame(&array_first_frame(), 1, Utc::now()).unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            MarketEvent::Snapshot(b) => {
                assert_eq!(b.best_bid().unwrap().price.get(), dec!(0.044));
                assert_eq!(b.best_ask().unwrap().price.get(), dec!(0.045));
                assert!(b.is_well_formed());
                assert_eq!(b.tick_size, dec!(0.001));
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[test]
    fn empty_and_keepalive_frames_are_ignored() {
        for f in ["", "   ", "PING"] {
            assert_eq!(parse_market_frame(f, 1, Utc::now()).unwrap(), vec![MarketEvent::Ignored]);
        }
    }

    #[test]
    fn price_change_batches_route_per_asset() {
        // One frame really does carry changes for several tokens.
        let f = format!(
            r#"{{"event_type":"price_change","market":"{MARKET}","timestamp":"1787102287053",
            "price_changes":[
              {{"asset_id":"{TOKEN}","price":"0.034","size":"6759","side":"BUY","best_bid":"0.044","best_ask":"0.045"}},
              {{"asset_id":"999","price":"0.5","size":"10","side":"SELL","best_bid":"0.4","best_ask":"0.5"}}]}}"#
        );
        let evs = parse_market_frame(&f, 1, Utc::now()).unwrap();
        assert_eq!(evs.len(), 2, "each change must become its own event");
        match &evs[0] {
            MarketEvent::LevelChange { token_id, side, price, size, best_bid, .. } => {
                assert_eq!(token_id.as_str(), TOKEN);
                assert_eq!(*side, domain::Side::Buy);
                assert_eq!(price.get(), dec!(0.034));
                assert_eq!(*size, dec!(6759));
                assert_eq!(best_bid.unwrap().get(), dec!(0.044));
            }
            other => panic!("{other:?}"),
        }
    }

    fn builder_with_book() -> BookBuilder {
        let mut b = BookBuilder::new();
        let evs = parse_market_frame(&array_first_frame(), 1, Utc::now()).unwrap();
        for e in evs { b.apply(e, Utc::now()); }
        b
    }

    #[test]
    fn level_change_replaces_aggregate_size_not_a_delta() {
        let mut b = builder_with_book();
        let tok = TokenId::new(TOKEN).unwrap();
        let before = b.get(&tok).unwrap().bids.iter()
            .find(|l| l.price.get() == dec!(0.043)).unwrap().size.get();
        assert_eq!(before, dec!(100));

        b.apply(MarketEvent::LevelChange {
            token_id: tok.clone(), side: domain::Side::Buy,
            price: Price::new(dec!(0.043)).unwrap(), size: dec!(250),
            best_bid: Some(Price::new(dec!(0.044)).unwrap()),
            best_ask: Some(Price::new(dec!(0.045)).unwrap()),
        }, Utc::now());

        let after = b.get(&tok).unwrap().bids.iter()
            .find(|l| l.price.get() == dec!(0.043)).unwrap().size.get();
        // Replace, not 100 + 250.
        assert_eq!(after, dec!(250));
    }

    #[test]
    fn zero_size_deletes_the_level() {
        let mut b = builder_with_book();
        let tok = TokenId::new(TOKEN).unwrap();
        b.apply(MarketEvent::LevelChange {
            token_id: tok.clone(), side: domain::Side::Buy,
            price: Price::new(dec!(0.043)).unwrap(), size: Decimal::ZERO,
            best_bid: Some(Price::new(dec!(0.044)).unwrap()),
            best_ask: Some(Price::new(dec!(0.045)).unwrap()),
        }, Utc::now());
        assert!(b.get(&tok).unwrap().bids.iter().all(|l| l.price.get() != dec!(0.043)));
    }

    #[test]
    fn book_stays_sorted_and_well_formed_after_updates() {
        let mut b = builder_with_book();
        let tok = TokenId::new(TOKEN).unwrap();
        for (px, sz) in [(dec!(0.0435), dec!(10)), (dec!(0.042), dec!(20)), (dec!(0.0415), dec!(5))] {
            b.apply(MarketEvent::LevelChange {
                token_id: tok.clone(), side: domain::Side::Buy,
                price: Price::new(px).unwrap(), size: sz,
                best_bid: Some(Price::new(dec!(0.044)).unwrap()),
                best_ask: Some(Price::new(dec!(0.045)).unwrap()),
            }, Utc::now());
        }
        assert!(b.get(&tok).unwrap().is_well_formed());
    }

    #[test]
    fn venue_reported_best_price_catches_drift_and_resyncs() {
        // The venue tells us its own best bid on every change. If we disagree, our book
        // has drifted and must be dropped rather than used to price an order.
        let mut b = builder_with_book();
        let tok = TokenId::new(TOKEN).unwrap();
        assert!(b.get(&tok).is_some());
        b.apply(MarketEvent::LevelChange {
            token_id: tok.clone(), side: domain::Side::Buy,
            price: Price::new(dec!(0.043)).unwrap(), size: dec!(1),
            // Venue says the best bid is 0.09, we think it is 0.044 -> drift.
            best_bid: Some(Price::new(dec!(0.09)).unwrap()),
            best_ask: Some(Price::new(dec!(0.095)).unwrap()),
        }, Utc::now());
        assert!(b.get(&tok).is_none(), "a drifted book must be invalidated, not used");
        assert_eq!(b.integrity_failures, 1);
    }

    #[test]
    fn tick_size_change_is_applied() {
        let mut b = builder_with_book();
        let tok = TokenId::new(TOKEN).unwrap();
        assert_eq!(b.get(&tok).unwrap().tick_size, dec!(0.001));
        b.apply(MarketEvent::TickSizeChanged {
            token_id: tok.clone(), tick_size: dec!(0.01) }, Utc::now());
        assert_eq!(b.get(&tok).unwrap().tick_size, dec!(0.01));
    }

    #[test]
    fn min_order_size_survives_snapshots() {
        // WS book frames omit it; REST metadata is authoritative.
        let mut b = BookBuilder::new();
        let tok = TokenId::new(TOKEN).unwrap();
        b.set_min_order_size(tok.clone(), dec!(15));
        for e in parse_market_frame(&array_first_frame(), 1, Utc::now()).unwrap() {
            b.apply(e, Utc::now());
        }
        assert_eq!(b.get(&tok).unwrap().min_order_size, dec!(15));
    }

    #[test]
    fn changes_for_unknown_tokens_are_harmless() {
        let mut b = BookBuilder::new();
        let r = b.apply(MarketEvent::LevelChange {
            token_id: TokenId::new("42").unwrap(), side: domain::Side::Buy,
            price: Price::new(dec!(0.5)).unwrap(), size: dec!(1),
            best_bid: None, best_ask: None,
        }, Utc::now());
        assert!(r.is_none());
        assert!(b.is_empty());
    }

    #[test]
    fn subscription_frame_matches_the_verified_shape() {
        let f = subscribe_frame(&[TokenId::new(TOKEN).unwrap()]);
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["type"], "market");
        assert_eq!(v["assets_ids"][0], TOKEN);
    }
}
