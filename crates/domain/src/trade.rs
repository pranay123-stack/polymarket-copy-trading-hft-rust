//! Source trades, copy signals and our own fills.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{
    Address, CorrelationId, FillId, MarketId, OrderId, SignalId, SourceEventId, TokenId, TxHash,
};
use crate::latency::LatencyStamps;
use crate::money::{Price, Qty, Side, Usd};

/// Where we observed a source trade. Kept on the record because the two paths have
/// genuinely different coverage: the live feed includes maker fills, the REST backfill
/// defaults to taker-only (`docs/POLYMARKET_API.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeSource {
    /// RTDS `activity/trades` websocket — the primary, event-driven path.
    RtdsWebsocket,
    /// `data-api /trades` REST backfill, used after a disconnect.
    RestBackfill,
    /// Deterministic replay of a recorded session.
    Replay,
    /// Synthetic demo generator. Never mixed with real data.
    Demo,
}

impl TradeSource {
    pub fn is_real(&self) -> bool {
        matches!(self, Self::RtdsWebsocket | Self::RestBackfill)
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RtdsWebsocket => "rtds_ws",
            Self::RestBackfill => "rest_backfill",
            Self::Replay => "replay",
            Self::Demo => "demo",
        }
    }
}

/// A trade executed by somebody else that we observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceTrade {
    /// Deterministic dedup identity — see `wallet_tracker::dedup`.
    pub event_id: SourceEventId,
    pub correlation_id: CorrelationId,
    pub trader: Address,
    pub market_id: MarketId,
    pub token_id: TokenId,
    /// Human-readable leg name ("Yes", "Down", "Colorado Rockies").
    pub outcome: String,
    pub side: Side,
    pub price: Price,
    pub quantity: Qty,
    pub tx_hash: TxHash,
    /// Ordinal distinguishing genuinely identical fills inside one transaction.
    /// Polymarket really does emit byte-identical rows; see `docs/POLYMARKET_API.md` §3.
    pub occurrence: u32,
    /// Venue publish time (ms resolution, from the RTDS envelope).
    pub source_ts: DateTime<Utc>,
    /// When our process saw it.
    pub detected_ts: DateTime<Utc>,
    pub source: TradeSource,
    pub market_title: String,
    pub market_slug: String,
}

impl SourceTrade {
    pub fn notional(&self) -> Usd { self.quantity.notional(self.price) }
}

/// Why a signal was not acted upon. Every drop is explicit and observable — the system
/// never silently ignores a target's trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason", content = "detail")]
pub enum SignalSkipReason {
    DuplicateEvent,
    WalletNotTracked,
    WalletDisabled,
    MarketBlocked(String),
    MarketNotAllowed(String),
    BelowMinNotional { got: String, min: String },
    SizedToZero,
}

/// The normalised, venue-agnostic instruction derived from a source trade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopySignal {
    pub signal_id: SignalId,
    pub correlation_id: CorrelationId,
    pub source_event_id: SourceEventId,
    pub target_wallet: Address,
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub outcome: String,
    pub side: Side,
    /// The price the source trader achieved.
    pub target_price: Price,
    pub target_quantity: Qty,
    pub target_notional: Usd,
    /// What we intend to trade after sizing rules.
    pub copy_quantity: Qty,
    pub copy_notional: Usd,
    /// Worst price we will accept, derived from the slippage budget.
    pub limit_price: Price,
    /// Which sizing rule produced `copy_quantity`, for auditability.
    pub sizing_mode: String,
    /// 0..=1. Currently a function of feed provenance and staleness.
    pub confidence: f64,
    pub source_ts: DateTime<Utc>,
    pub detection_ts: DateTime<Utc>,
    pub signal_ts: DateTime<Utc>,
    pub latency: LatencyStamps,
    pub metadata: serde_json::Value,
}

/// One execution against one of our orders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub fill_id: FillId,
    pub order_id: OrderId,
    pub correlation_id: CorrelationId,
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub side: Side,
    pub quantity: Qty,
    pub price: Price,
    pub fee: Usd,
    /// Venue's fill/trade id where available; the simulator mints its own.
    pub venue_fill_id: Option<String>,
    pub is_maker: bool,
    pub filled_at: DateTime<Utc>,
}

impl Fill {
    pub fn notional(&self) -> Usd { self.quantity.notional(self.price) }

    /// Signed cash impact including fees: buying costs cash, selling returns it,
    /// fees always cost.
    pub fn cash_delta(&self) -> Usd {
        match self.side {
            Side::Buy => -(self.notional() + self.fee),
            Side::Sell => self.notional() - self.fee,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn fill(side: Side, qty: Decimal, px: Decimal, fee: Decimal) -> Fill {
        Fill {
            fill_id: FillId::new(),
            order_id: OrderId::new(),
            correlation_id: CorrelationId::new(),
            market_id: MarketId::new("0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52").unwrap(),
            token_id: TokenId::new("725510240982585425945346839425236061430146906202430232984977299578468701").unwrap(),
            side,
            quantity: Qty::new(qty).unwrap(),
            price: Price::new(px).unwrap(),
            fee: Usd::new(fee),
            venue_fill_id: None,
            is_maker: false,
            filled_at: Utc::now(),
        }
    }
    use rust_decimal::Decimal;

    #[test]
    fn buy_consumes_cash_including_fee() {
        let f = fill(Side::Buy, dec!(100), dec!(0.60), dec!(0.30));
        assert_eq!(f.notional().get(), dec!(60));
        assert_eq!(f.cash_delta().get(), dec!(-60.30));
    }

    #[test]
    fn sell_returns_cash_net_of_fee() {
        let f = fill(Side::Sell, dec!(100), dec!(0.60), dec!(0.30));
        assert_eq!(f.cash_delta().get(), dec!(59.70));
    }

    #[test]
    fn demo_source_is_never_treated_as_real() {
        assert!(!TradeSource::Demo.is_real());
        assert!(!TradeSource::Replay.is_real());
        assert!(TradeSource::RtdsWebsocket.is_real());
        assert!(TradeSource::RestBackfill.is_real());
    }
}
