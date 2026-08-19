//! Polymarket data ingestion.
//!
//! Every Polymarket wire format and URL is confined to this crate. The rest of the
//! system consumes only `domain` types, so the venue can be swapped, mocked or replayed
//! without touching strategy, risk, execution or persistence.
//!
//! * [`rtds`] — the event-driven, wallet-attributed trade feed (primary detection path)
//! * [`rest`] — CLOB books, market metadata, and gap backfill
//! * [`parser`] — all wire→domain normalisation, including the two silent traps
//! * [`reconnect`] — jittered backoff and a circuit breaker

pub mod market_stream;
pub mod market_ws;
pub mod parser;
pub mod reconnect;
pub mod rest;
pub mod rtds;

pub use parser::{
    parse_book, parse_data_api_trade, parse_gamma_market, parse_rtds_frame, ParseError, ParsedTrade,
    RtdsFrame,
};
pub use market_stream::{run_market_stream, StreamStats, TokenSubscriptions, MAX_SUBSCRIBED_TOKENS};
pub use market_ws::{parse_market_frame, subscribe_frame, BookBuilder, MarketEvent};
pub use reconnect::{Backoff, BreakerState, CircuitBreaker};
pub use rest::{PolymarketRest, RestError};
pub use rtds::{FeedMessage, FeedStats, RtdsClient, SUBSCRIBE_TRADES};
