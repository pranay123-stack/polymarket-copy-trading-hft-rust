//! Markets and order books.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use crate::ids::{MarketId, TokenId};
use crate::money::{Price, Qty, Side, Usd};

/// One outcome leg of a market.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub token_id: TokenId,
    /// Match legs by this name, never by array index — Polymarket's `outcomeIndex`
    /// is sometimes the sentinel `999` (`docs/POLYMARKET_API.md` §2).
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Market {
    pub market_id: MarketId,
    pub slug: String,
    pub title: String,
    pub outcomes: Vec<Outcome>,
    /// Per-market price increment. Quoting off-tick is rejected by the venue.
    pub tick_size: Decimal,
    pub min_order_size: Decimal,
    pub neg_risk: bool,
    pub active: bool,
    pub closed: bool,
    pub accepting_orders: bool,
}

impl Market {
    pub fn outcome_by_token(&self, t: &TokenId) -> Option<&Outcome> {
        self.outcomes.iter().find(|o| &o.token_id == t)
    }
    pub fn is_tradable(&self) -> bool {
        self.active && !self.closed && self.accepting_orders
    }
}

/// One price level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Level {
    pub price: Price,
    pub size: Qty,
}

/// A normalised order book.
///
/// **Both sides are stored best-first**, which is *not* how the venue sends them:
/// `GET /book` returns bids ascending and asks descending, so the best price on each
/// side is the **last** element on the wire. Normalisation happens once, in
/// `market_data::parser`, and this invariant is asserted here so a regression in the
/// parser fails loudly instead of silently mispricing every order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderBook {
    pub market_id: MarketId,
    pub token_id: TokenId,
    /// Descending by price — `bids[0]` is the highest bid.
    pub bids: Vec<Level>,
    /// Ascending by price — `asks[0]` is the lowest ask.
    pub asks: Vec<Level>,
    pub tick_size: Decimal,
    pub min_order_size: Decimal,
    /// Venue publish time.
    pub timestamp: DateTime<Utc>,
    /// Sequence assigned by us at ingest — the market channel carries no sequence numbers.
    pub seq: u64,
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<Level> { self.bids.first().copied() }
    pub fn best_ask(&self) -> Option<Level> { self.asks.first().copied() }

    pub fn mid(&self) -> Option<Price> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => Price::new((b.price.get() + a.price.get()) / dec!(2)).ok(),
            _ => None,
        }
    }

    pub fn spread(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => Some(a.price.get() - b.price.get()),
            _ => None,
        }
    }

    /// Debug assertion helper: are both sides really best-first and non-crossed?
    pub fn is_well_formed(&self) -> bool {
        let bids_ok = self.bids.windows(2).all(|w| w[0].price >= w[1].price);
        let asks_ok = self.asks.windows(2).all(|w| w[0].price <= w[1].price);
        let uncrossed = match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => b.price < a.price,
            _ => true,
        };
        bids_ok && asks_ok && uncrossed
    }

    /// The side a taker consumes: a BUY lifts asks, a SELL hits bids.
    pub fn taking_side(&self, side: Side) -> &[Level] {
        match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        }
    }

    /// Total size available at or better than `limit`.
    pub fn liquidity_within(&self, side: Side, limit: Price) -> Qty {
        self.taking_side(side)
            .iter()
            .take_while(|l| match side {
                Side::Buy => l.price <= limit,
                Side::Sell => l.price >= limit,
            })
            .map(|l| l.size)
            .sum()
    }

    /// USD notional resting at or better than `limit`. Used by the liquidity risk check
    /// and by risk-adjusted sizing.
    pub fn notional_within(&self, side: Side, limit: Price) -> Usd {
        self.taking_side(side)
            .iter()
            .take_while(|l| match side {
                Side::Buy => l.price <= limit,
                Side::Sell => l.price >= limit,
            })
            .map(|l| l.size.notional(l.price))
            .sum()
    }

    /// Volume-weighted price to take `qty`, walking the book.
    /// Returns `None` if the book cannot fill it within `limit` — the caller must then
    /// size down rather than assume a fill.
    pub fn sweep_vwap(&self, side: Side, qty: Qty, limit: Price) -> Option<(Price, Qty)> {
        let mut remaining = qty.get();
        let mut notional = Decimal::ZERO;
        let mut taken = Decimal::ZERO;
        for l in self.taking_side(side) {
            let within = match side {
                Side::Buy => l.price <= limit,
                Side::Sell => l.price >= limit,
            };
            if !within || remaining <= Decimal::ZERO { break; }
            let take = remaining.min(l.size.get());
            notional += take * l.price.get();
            taken += take;
            remaining -= take;
        }
        if taken.is_zero() { return None; }
        Price::new(notional / taken).ok().map(|p| (p, Qty::new(taken).ok().unwrap_or(Qty::ZERO)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lvl(p: Decimal, s: Decimal) -> Level {
        Level { price: Price::new(p).unwrap(), size: Qty::new(s).unwrap() }
    }

    fn book() -> OrderBook {
        OrderBook {
            market_id: MarketId::new("0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52").unwrap(),
            token_id: TokenId::new("7255102409825854259453468394252360614301469062024302329849").unwrap(),
            // best-first, as normalised
            bids: vec![lvl(dec!(0.59), dec!(100)), lvl(dec!(0.58), dec!(200)), lvl(dec!(0.57), dec!(500))],
            asks: vec![lvl(dec!(0.61), dec!(150)), lvl(dec!(0.62), dec!(300)), lvl(dec!(0.63), dec!(400))],
            tick_size: dec!(0.01),
            min_order_size: dec!(5),
            timestamp: Utc::now(),
            seq: 1,
        }
    }

    #[test]
    fn best_prices_come_from_the_front() {
        let b = book();
        assert_eq!(b.best_bid().unwrap().price.get(), dec!(0.59));
        assert_eq!(b.best_ask().unwrap().price.get(), dec!(0.61));
        assert_eq!(b.mid().unwrap().get(), dec!(0.60));
        assert_eq!(b.spread().unwrap(), dec!(0.02));
        assert!(b.is_well_formed());
    }

    #[test]
    fn malformed_books_are_detected() {
        // A book left in the venue's wire order (worst-first) must not pass.
        let mut b = book();
        b.bids.reverse();
        assert!(!b.is_well_formed(), "worst-first bids must be caught");

        let mut c = book();
        c.asks.reverse();
        assert!(!c.is_well_formed());

        // Crossed book.
        let mut d = book();
        d.bids[0] = lvl(dec!(0.70), dec!(1));
        assert!(!d.is_well_formed());
    }

    #[test]
    fn buy_takes_asks_and_sell_takes_bids() {
        let b = book();
        assert_eq!(b.taking_side(Side::Buy)[0].price.get(), dec!(0.61));
        assert_eq!(b.taking_side(Side::Sell)[0].price.get(), dec!(0.59));
    }

    #[test]
    fn liquidity_respects_the_limit_price() {
        let b = book();
        // Willing to pay up to 0.62: first two ask levels qualify.
        assert_eq!(b.liquidity_within(Side::Buy, Price::new(dec!(0.62)).unwrap()).get(), dec!(450));
        // Only the touch.
        assert_eq!(b.liquidity_within(Side::Buy, Price::new(dec!(0.61)).unwrap()).get(), dec!(150));
        // Below the touch: nothing is available.
        assert_eq!(b.liquidity_within(Side::Buy, Price::new(dec!(0.60)).unwrap()).get(), dec!(0));
    }

    #[test]
    fn sweep_vwap_walks_multiple_levels() {
        let b = book();
        // Take 300 with a 0.63 limit: 150@0.61 + 150@0.62.
        let (px, got) = b.sweep_vwap(Side::Buy, Qty::new(dec!(300)).unwrap(), Price::new(dec!(0.63)).unwrap()).unwrap();
        assert_eq!(got.get(), dec!(300));
        assert_eq!(px.get(), dec!(0.615));
    }

    #[test]
    fn sweep_stops_at_the_limit_and_reports_short_fill() {
        let b = book();
        // Want 500 but only 150 is within 0.61.
        let (px, got) = b.sweep_vwap(Side::Buy, Qty::new(dec!(500)).unwrap(), Price::new(dec!(0.61)).unwrap()).unwrap();
        assert_eq!(got.get(), dec!(150), "must report what is actually available");
        assert_eq!(px.get(), dec!(0.61));
    }

    #[test]
    fn sweep_returns_none_when_nothing_is_reachable() {
        let b = book();
        assert!(b.sweep_vwap(Side::Buy, Qty::new(dec!(100)).unwrap(), Price::new(dec!(0.50)).unwrap()).is_none());
    }

    #[test]
    fn notional_within_prices_each_level_correctly() {
        let b = book();
        // 150*0.61 + 300*0.62 = 91.5 + 186 = 277.5
        assert_eq!(b.notional_within(Side::Buy, Price::new(dec!(0.62)).unwrap()).get(), dec!(277.50));
    }
}
