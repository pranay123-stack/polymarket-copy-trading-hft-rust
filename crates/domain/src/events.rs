//! The internal event bus vocabulary.
//!
//! Modules communicate exclusively through these events, which is what keeps the
//! strategy ignorant of whether execution is paper or live, and lets the API/dashboard
//! layer observe everything without any component knowing it exists.
//!
//! Every variant carries a `CorrelationId` so a single source trade can be traced
//! end to end from one dashboard row.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{CorrelationId, MarketId, OrderId, SignalId, SourceEventId, TokenId};
use crate::money::{Price, Qty, Usd};
use crate::order::OrderState;
use crate::pnl::PnlSnapshot;
use crate::position::Position;
use crate::trade::{CopySignal, Fill, SignalSkipReason, SourceTrade};

/// Operating mode. The strategy, risk, portfolio and OMS layers are identical across
/// all three; only the execution adapter and the data source differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AppMode {
    Paper,
    Live,
    Replay,
}

impl AppMode {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Paper => "PAPER", Self::Live => "LIVE", Self::Replay => "REPLAY" }
    }
    pub fn is_live(&self) -> bool { matches!(self, Self::Live) }
}

impl std::fmt::Display for AppMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
}

/// Why the risk engine refused an order. Every rejection names its own cause; there is
/// no generic "rejected" path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RiskRejection {
    MaxTradeSizeExceeded { requested: Usd, limit: Usd },
    MaxPositionExceeded { market: MarketId, projected: Usd, limit: Usd },
    MaxPortfolioExposureExceeded { projected: Usd, limit: Usd },
    MaxMarketExposureExceeded { market: MarketId, projected: Usd, limit: Usd },
    MaxWalletExposureExceeded { wallet: String, projected: Usd, limit: Usd },
    DailyLossLimitReached { daily_pnl: Usd, limit: Usd },
    MaxOpenOrdersReached { open: u32, limit: u32 },
    SlippageTooWide { estimated_bps: i64, limit_bps: u32 },
    InsufficientLiquidity { available: Usd, required: Usd },
    MarketNotTradable { market: MarketId, reason: String },
    WalletDisabled { wallet: String },
    DuplicateOrder { source_event: SourceEventId },
    KillSwitchActive { reason: String },
    SystemUnhealthy { detail: String },
    LiveExecutionNotArmed,
    BelowMinimumOrderSize { requested: Usd, minimum: Usd },
    StaleMarketData { age_ms: i64, max_age_ms: i64 },
}

impl RiskRejection {
    /// Stable machine name, used for metric labels and DB rows.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MaxTradeSizeExceeded { .. } => "max_trade_size",
            Self::MaxPositionExceeded { .. } => "max_position",
            Self::MaxPortfolioExposureExceeded { .. } => "max_portfolio_exposure",
            Self::MaxMarketExposureExceeded { .. } => "max_market_exposure",
            Self::MaxWalletExposureExceeded { .. } => "max_wallet_exposure",
            Self::DailyLossLimitReached { .. } => "daily_loss_limit",
            Self::MaxOpenOrdersReached { .. } => "max_open_orders",
            Self::SlippageTooWide { .. } => "slippage_too_wide",
            Self::InsufficientLiquidity { .. } => "insufficient_liquidity",
            Self::MarketNotTradable { .. } => "market_not_tradable",
            Self::WalletDisabled { .. } => "wallet_disabled",
            Self::DuplicateOrder { .. } => "duplicate_order",
            Self::KillSwitchActive { .. } => "kill_switch",
            Self::SystemUnhealthy { .. } => "system_unhealthy",
            Self::LiveExecutionNotArmed => "live_not_armed",
            Self::BelowMinimumOrderSize { .. } => "below_min_order_size",
            Self::StaleMarketData { .. } => "stale_market_data",
        }
    }
}

/// Connection/health state of a subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HealthState {
    Healthy,
    Degraded,
    Down,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "component")]
pub enum ComponentHealth {
    MarketData { state: HealthState, detail: String },
    SourceFeed { state: HealthState, detail: String },
    Database { state: HealthState, detail: String },
    Redis { state: HealthState, detail: String },
    Execution { state: HealthState, detail: String },
}

/// Everything that happens in the system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum SystemEvent {
    MarketUpdated {
        market_id: MarketId,
        token_id: TokenId,
        best_bid: Option<Price>,
        best_ask: Option<Price>,
        seq: u64,
        at: DateTime<Utc>,
    },
    SourceTradeDetected(Box<SourceTrade>),
    /// A source trade was observed but deliberately not acted on.
    SourceTradeSkipped {
        event_id: SourceEventId,
        correlation_id: CorrelationId,
        reason: SignalSkipReason,
        at: DateTime<Utc>,
    },
    CopySignalGenerated(Box<CopySignal>),
    OrderRiskApproved {
        order_id: OrderId,
        correlation_id: CorrelationId,
        signal_id: SignalId,
        at: DateTime<Utc>,
    },
    OrderRiskRejected {
        correlation_id: CorrelationId,
        signal_id: Option<SignalId>,
        rejection: Box<RiskRejection>,
        at: DateTime<Utc>,
    },
    OrderSubmitted { order_id: OrderId, correlation_id: CorrelationId, at: DateTime<Utc> },
    OrderAcknowledged {
        order_id: OrderId,
        correlation_id: CorrelationId,
        venue_order_id: Option<String>,
        at: DateTime<Utc>,
    },
    OrderPartiallyFilled { order_id: OrderId, correlation_id: CorrelationId, fill: Box<Fill> },
    OrderFilled { order_id: OrderId, correlation_id: CorrelationId, fill: Box<Fill> },
    OrderCancelled { order_id: OrderId, correlation_id: CorrelationId, at: DateTime<Utc> },
    OrderRejected {
        order_id: OrderId,
        correlation_id: CorrelationId,
        reason: String,
        at: DateTime<Utc>,
    },
    OrderStateChanged {
        order_id: OrderId,
        correlation_id: CorrelationId,
        from: OrderState,
        to: OrderState,
        at: DateTime<Utc>,
    },
    PositionUpdated { position: Box<Position>, at: DateTime<Utc> },
    PnlUpdated { snapshot: Box<PnlSnapshot> },
    RiskLimitBreached { rejection: Box<RiskRejection>, at: DateTime<Utc> },
    KillSwitchActivated { reason: String, by: String, at: DateTime<Utc> },
    KillSwitchReset { by: String, at: DateTime<Utc> },
    /// Internal position disagrees with the venue. Never auto-resolved.
    ReconciliationMismatch {
        token_id: TokenId,
        internal: Qty,
        venue: Qty,
        at: DateTime<Utc>,
    },
    HealthChanged { health: ComponentHealth, at: DateTime<Utc> },
    /// The source feed dropped; `gap_from` bounds what backfill must cover.
    FeedDisconnected { source: String, gap_from: DateTime<Utc>, at: DateTime<Utc> },
    FeedReconnected { source: String, backfilled: u32, at: DateTime<Utc> },
}

impl SystemEvent {
    /// The correlating id, where the event belongs to a trade lifecycle.
    pub fn correlation_id(&self) -> Option<CorrelationId> {
        use SystemEvent::*;
        match self {
            SourceTradeDetected(t) => Some(t.correlation_id),
            CopySignalGenerated(s) => Some(s.correlation_id),
            SourceTradeSkipped { correlation_id, .. }
            | OrderRiskApproved { correlation_id, .. }
            | OrderRiskRejected { correlation_id, .. }
            | OrderSubmitted { correlation_id, .. }
            | OrderAcknowledged { correlation_id, .. }
            | OrderPartiallyFilled { correlation_id, .. }
            | OrderFilled { correlation_id, .. }
            | OrderCancelled { correlation_id, .. }
            | OrderRejected { correlation_id, .. }
            | OrderStateChanged { correlation_id, .. } => Some(*correlation_id),
            _ => None,
        }
    }

    /// Stable topic name for logging, metrics and dashboard routing.
    pub fn kind(&self) -> &'static str {
        use SystemEvent::*;
        match self {
            MarketUpdated { .. } => "market_updated",
            SourceTradeDetected(_) => "source_trade_detected",
            SourceTradeSkipped { .. } => "source_trade_skipped",
            CopySignalGenerated(_) => "copy_signal_generated",
            OrderRiskApproved { .. } => "order_risk_approved",
            OrderRiskRejected { .. } => "order_risk_rejected",
            OrderSubmitted { .. } => "order_submitted",
            OrderAcknowledged { .. } => "order_acknowledged",
            OrderPartiallyFilled { .. } => "order_partially_filled",
            OrderFilled { .. } => "order_filled",
            OrderCancelled { .. } => "order_cancelled",
            OrderRejected { .. } => "order_rejected",
            OrderStateChanged { .. } => "order_state_changed",
            PositionUpdated { .. } => "position_updated",
            PnlUpdated { .. } => "pnl_updated",
            RiskLimitBreached { .. } => "risk_limit_breached",
            KillSwitchActivated { .. } => "kill_switch_activated",
            KillSwitchReset { .. } => "kill_switch_reset",
            ReconciliationMismatch { .. } => "reconciliation_mismatch",
            HealthChanged { .. } => "health_changed",
            FeedDisconnected { .. } => "feed_disconnected",
            FeedReconnected { .. } => "feed_reconnected",
        }
    }

    /// Events an operator must not miss, regardless of dashboard filters.
    pub fn is_critical(&self) -> bool {
        matches!(
            self,
            SystemEvent::RiskLimitBreached { .. }
                | SystemEvent::KillSwitchActivated { .. }
                | SystemEvent::ReconciliationMismatch { .. }
                | SystemEvent::FeedDisconnected { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rejection_has_a_distinct_code() {
        let all = [
            RiskRejection::MaxTradeSizeExceeded { requested: Usd::ZERO, limit: Usd::ZERO },
            RiskRejection::MaxOpenOrdersReached { open: 1, limit: 1 },
            RiskRejection::KillSwitchActive { reason: "x".into() },
            RiskRejection::LiveExecutionNotArmed,
            RiskRejection::StaleMarketData { age_ms: 1, max_age_ms: 1 },
        ];
        let codes: Vec<_> = all.iter().map(|r| r.code()).collect();
        let uniq: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(codes.len(), uniq.len());
    }

    #[test]
    fn critical_events_are_flagged() {
        let e = SystemEvent::KillSwitchActivated {
            reason: "manual".into(), by: "api".into(), at: Utc::now(),
        };
        assert!(e.is_critical());
        assert_eq!(e.kind(), "kill_switch_activated");
    }

    #[test]
    fn mode_is_explicit_about_live() {
        assert!(AppMode::Live.is_live());
        assert!(!AppMode::Paper.is_live());
        assert!(!AppMode::Replay.is_live());
        assert_eq!(AppMode::Paper.to_string(), "PAPER");
    }
}
