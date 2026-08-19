//! Typed repositories over the schema.
//!
//! Every write is idempotent: re-inserting the same source event, signal, order or fill
//! is a no-op rather than an error. That is what makes crash recovery safe — replaying
//! the tail of a session cannot duplicate anything.

use chrono::{DateTime, Utc};
use domain::{
    Address, CopySignal, Fill, MarketId, Order, OrderState, PnlSnapshot, Position,
    RiskRejection, SourceTrade, SystemEvent, TargetWallet, TokenId, Usd,
};
use rust_decimal::Decimal;
use sqlx::Row;
use tracing::debug;
use uuid::Uuid;

use crate::store::{Store, StoreError};

/// What was loaded back on startup.
#[derive(Debug, Default)]
pub struct RecoveredState {
    pub orders: Vec<Order>,
    pub positions: Vec<Position>,
    pub cash: Option<Usd>,
    pub realized_pnl: Usd,
    pub fees_paid: Usd,
    /// (content components, occurrence count, last seen) for rebuilding the dedup index.
    pub dedup_entries: Vec<DedupRow>,
    pub wallets: Vec<TargetWallet>,
}

#[derive(Debug, Clone)]
pub struct DedupRow {
    pub tx_hash: String,
    pub trader: String,
    pub token_id: String,
    pub side: String,
    pub price: Decimal,
    pub size: Decimal,
    pub occurrences: u32,
    pub last_seen: DateTime<Utc>,
}

#[derive(Clone)]
pub struct Repositories {
    store: Store,
}

impl Repositories {
    pub fn new(store: Store) -> Self { Self { store } }
    pub fn store(&self) -> &Store { &self.store }
    pub fn is_ephemeral(&self) -> bool { self.store.is_ephemeral() }

    // ---------------------------------------------------------------- source events

    /// Records a processed source event.
    ///
    /// Returns `true` if this was genuinely new. `false` means the database already had
    /// it — the durable half of duplicate protection, and the reason a restart cannot
    /// re-copy a trade the in-memory index has forgotten.
    pub async fn insert_source_event(&self, t: &SourceTrade) -> Result<bool, StoreError> {
        let Some(p) = self.store.pool() else { return Ok(true) };
        let r = sqlx::query(
            "INSERT INTO source_events
             (event_id, correlation_id, trader, market_id, token_id, outcome, side, price, size,
              notional_usd, tx_hash, occurrence, source, source_ts, detected_ts)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
             ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(t.event_id.as_str())
        .bind(t.correlation_id.as_uuid())
        .bind(t.trader.as_str())
        .bind(t.market_id.as_str())
        .bind(t.token_id.as_str())
        .bind(&t.outcome)
        .bind(t.side.as_str())
        .bind(t.price.get())
        .bind(t.quantity.get())
        .bind(t.notional().get())
        .bind(t.tx_hash.as_str())
        .bind(t.occurrence as i32)
        .bind(t.source.as_str())
        .bind(t.source_ts)
        .bind(t.detected_ts)
        .execute(p)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Has this event already been processed?
    pub async fn source_event_exists(&self, event_id: &str) -> Result<bool, StoreError> {
        let Some(p) = self.store.pool() else { return Ok(false) };
        let r = sqlx::query("SELECT 1 FROM source_events WHERE event_id = $1")
            .bind(event_id)
            .fetch_optional(p)
            .await?;
        Ok(r.is_some())
    }

    /// Rebuilds dedup state for the retention window.
    pub async fn load_dedup_window(&self, since: DateTime<Utc>) -> Result<Vec<DedupRow>, StoreError> {
        let Some(p) = self.store.pool() else { return Ok(Vec::new()) };
        let rows = sqlx::query(
            "SELECT tx_hash, trader, token_id, side, price, size,
                    COUNT(*)::int AS n, MAX(detected_ts) AS last_seen
             FROM source_events WHERE source_ts >= $1
             GROUP BY tx_hash, trader, token_id, side, price, size",
        )
        .bind(since)
        .fetch_all(p)
        .await?;
        Ok(rows
            .iter()
            .map(|r| DedupRow {
                tx_hash: r.get("tx_hash"),
                trader: r.get("trader"),
                token_id: r.get("token_id"),
                side: r.get("side"),
                price: r.get("price"),
                size: r.get("size"),
                occurrences: r.get::<i32, _>("n").max(0) as u32,
                last_seen: r.get("last_seen"),
            })
            .collect())
    }

    // ---------------------------------------------------------------- signals

    pub async fn insert_signal(&self, s: &CopySignal) -> Result<(), StoreError> {
        let q = sqlx::query(
            "INSERT INTO copy_signals
             (signal_id, correlation_id, source_event_id, target_wallet, market_id, token_id,
              outcome, side, target_price, target_quantity, target_notional, copy_quantity,
              copy_notional, limit_price, sizing_mode, confidence, metadata,
              source_ts, detection_ts, signal_ts)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)
             ON CONFLICT (source_event_id) DO NOTHING",
        )
        .bind(s.signal_id.as_uuid())
        .bind(s.correlation_id.as_uuid())
        .bind(s.source_event_id.as_str())
        .bind(s.target_wallet.as_str())
        .bind(s.market_id.as_str())
        .bind(s.token_id.as_str())
        .bind(&s.outcome)
        .bind(s.side.as_str())
        .bind(s.target_price.get())
        .bind(s.target_quantity.get())
        .bind(s.target_notional.get())
        .bind(s.copy_quantity.get())
        .bind(s.copy_notional.get())
        .bind(s.limit_price.get())
        .bind(&s.sizing_mode)
        .bind(s.confidence)
        .bind(s.metadata.clone())
        .bind(s.source_ts)
        .bind(s.detection_ts)
        .bind(s.signal_ts);
        self.store.exec(q).await?;
        Ok(())
    }

    // ---------------------------------------------------------------- orders & fills

    pub async fn upsert_order(&self, o: &Order, mode: &str) -> Result<(), StoreError> {
        let q = sqlx::query(
            "INSERT INTO orders
             (order_id, correlation_id, signal_id, venue_order_id, market_id, token_id, side,
              order_type, time_in_force, quantity, limit_price, reference_price, state,
              filled_qty, filled_notional, fees_paid, reject_reason, mode, latency,
              created_at, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)
             ON CONFLICT (order_id) DO UPDATE SET
               venue_order_id = EXCLUDED.venue_order_id,
               state = EXCLUDED.state,
               filled_qty = EXCLUDED.filled_qty,
               filled_notional = EXCLUDED.filled_notional,
               fees_paid = EXCLUDED.fees_paid,
               reject_reason = EXCLUDED.reject_reason,
               latency = EXCLUDED.latency,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(o.request.order_id.as_uuid())
        .bind(o.request.correlation_id.as_uuid())
        .bind(o.request.signal_id.map(|s| s.as_uuid()))
        .bind(o.venue_order_id.clone())
        .bind(o.request.market_id.as_str())
        .bind(o.request.token_id.as_str())
        .bind(o.request.side.as_str())
        .bind(format!("{:?}", o.request.order_type).to_uppercase())
        .bind(format!("{:?}", o.request.time_in_force).to_uppercase())
        .bind(o.request.quantity.get())
        .bind(o.request.limit_price.get())
        .bind(o.request.reference_price.get())
        .bind(o.state.as_str())
        .bind(o.filled_qty.get())
        .bind(o.filled_notional.get())
        .bind(o.fees_paid.get())
        .bind(o.reject_reason.clone())
        .bind(mode)
        .bind(serde_json::to_value(o.latency).unwrap_or_default())
        .bind(o.request.created_at)
        .bind(o.updated_at);
        self.store.exec(q).await?;
        Ok(())
    }

    pub async fn insert_fill(&self, f: &Fill) -> Result<(), StoreError> {
        let q = sqlx::query(
            "INSERT INTO fills
             (fill_id, order_id, correlation_id, market_id, token_id, side, quantity, price,
              fee, venue_fill_id, is_maker, filled_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
             ON CONFLICT DO NOTHING",
        )
        .bind(f.fill_id.as_uuid())
        .bind(f.order_id.as_uuid())
        .bind(f.correlation_id.as_uuid())
        .bind(f.market_id.as_str())
        .bind(f.token_id.as_str())
        .bind(f.side.as_str())
        .bind(f.quantity.get())
        .bind(f.price.get())
        .bind(f.fee.get())
        .bind(f.venue_fill_id.clone())
        .bind(f.is_maker)
        .bind(f.filled_at);
        self.store.exec(q).await?;
        Ok(())
    }

    /// Loads orders that were still working when the process stopped.
    pub async fn load_open_orders(&self) -> Result<Vec<OrderRow>, StoreError> {
        let Some(p) = self.store.pool() else { return Ok(Vec::new()) };
        let rows = sqlx::query(
            "SELECT order_id, state, venue_order_id, filled_qty, filled_notional, fees_paid
             FROM orders
             WHERE state IN ('CREATED','VALIDATED','SUBMITTED','ACKNOWLEDGED',
                             'PARTIALLY_FILLED','CANCEL_REQUESTED','UNKNOWN')",
        )
        .fetch_all(p)
        .await?;
        Ok(rows
            .iter()
            .map(|r| OrderRow {
                order_id: r.get::<Uuid, _>("order_id"),
                state: r.get::<String, _>("state"),
                venue_order_id: r.get("venue_order_id"),
                filled_qty: r.get("filled_qty"),
                filled_notional: r.get("filled_notional"),
                fees_paid: r.get("fees_paid"),
            })
            .collect())
    }

    // ---------------------------------------------------------------- positions & pnl

    pub async fn upsert_position(&self, p: &Position) -> Result<(), StoreError> {
        let q = sqlx::query(
            "INSERT INTO positions
             (token_id, market_id, outcome, net_quantity, avg_entry, realized_pnl, fees_paid,
              mark_price, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
             ON CONFLICT (token_id) DO UPDATE SET
               net_quantity = EXCLUDED.net_quantity,
               avg_entry = EXCLUDED.avg_entry,
               realized_pnl = EXCLUDED.realized_pnl,
               fees_paid = EXCLUDED.fees_paid,
               mark_price = EXCLUDED.mark_price,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(p.token_id.as_str())
        .bind(p.market_id.as_str())
        .bind(&p.outcome)
        .bind(p.net_quantity)
        .bind(p.avg_entry)
        .bind(p.realized_pnl.get())
        .bind(p.fees_paid.get())
        .bind(p.mark_price.map(|m| m.get()))
        .bind(p.updated_at);
        self.store.exec(q).await?;
        Ok(())
    }

    pub async fn load_positions(&self) -> Result<Vec<Position>, StoreError> {
        let Some(p) = self.store.pool() else { return Ok(Vec::new()) };
        let rows = sqlx::query("SELECT * FROM positions WHERE net_quantity <> 0").fetch_all(p).await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(Position {
                    market_id: MarketId::new(r.get::<String, _>("market_id")).ok()?,
                    token_id: TokenId::new(r.get::<String, _>("token_id")).ok()?,
                    outcome: r.get("outcome"),
                    net_quantity: r.get("net_quantity"),
                    avg_entry: r.get("avg_entry"),
                    realized_pnl: Usd::new(r.get("realized_pnl")),
                    fees_paid: Usd::new(r.get("fees_paid")),
                    mark_price: r.get::<Option<Decimal>, _>("mark_price")
                        .and_then(|d| domain::Price::new(d).ok()),
                    updated_at: r.get("updated_at"),
                })
            })
            .collect())
    }

    pub async fn insert_pnl_snapshot(&self, s: &PnlSnapshot) -> Result<(), StoreError> {
        let q = sqlx::query(
            "INSERT INTO pnl_snapshots
             (at, cash, position_value, equity, realized_pnl, unrealized_pnl, fees_paid,
              gross_exposure, daily_pnl, peak_equity, open_orders, active_positions)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
        )
        .bind(s.at)
        .bind(s.cash.get())
        .bind(s.position_value.get())
        .bind(s.equity.get())
        .bind(s.realized_pnl.get())
        // NULL, not zero: an unmarked book must stay distinguishable in the record.
        .bind(s.unrealized_pnl.map(|u| u.get()))
        .bind(s.fees_paid.get())
        .bind(s.gross_exposure.get())
        .bind(s.daily_pnl.get())
        .bind(s.peak_equity.get())
        .bind(s.open_orders as i32)
        .bind(s.active_positions as i32);
        self.store.exec(q).await?;
        Ok(())
    }

    /// Latest snapshot, for restoring cash on startup.
    pub async fn latest_pnl_snapshot(&self) -> Result<Option<(Usd, Usd, Usd)>, StoreError> {
        let Some(p) = self.store.pool() else { return Ok(None) };
        let r = sqlx::query("SELECT cash, realized_pnl, fees_paid FROM pnl_snapshots ORDER BY at DESC LIMIT 1")
            .fetch_optional(p)
            .await?;
        Ok(r.map(|r| (
            Usd::new(r.get("cash")),
            Usd::new(r.get("realized_pnl")),
            Usd::new(r.get("fees_paid")),
        )))
    }

    // ---------------------------------------------------------------- markets

    /// Records market metadata, so a historical order can be interpreted later without
    /// re-querying a market that may since have closed.
    pub async fn upsert_market(&self, m: &domain::Market) -> Result<(), StoreError> {
        let q = sqlx::query(
            "INSERT INTO markets
             (market_id, slug, title, outcomes, tick_size, min_order_size, neg_risk,
              active, closed, accepting_orders, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,NOW())
             ON CONFLICT (market_id) DO UPDATE SET
               slug = EXCLUDED.slug,
               title = EXCLUDED.title,
               outcomes = EXCLUDED.outcomes,
               tick_size = EXCLUDED.tick_size,
               min_order_size = EXCLUDED.min_order_size,
               active = EXCLUDED.active,
               closed = EXCLUDED.closed,
               accepting_orders = EXCLUDED.accepting_orders,
               updated_at = NOW()",
        )
        .bind(m.market_id.as_str())
        .bind(&m.slug)
        .bind(&m.title)
        .bind(serde_json::to_value(&m.outcomes).unwrap_or_default())
        .bind(m.tick_size)
        .bind(m.min_order_size)
        .bind(m.neg_risk)
        .bind(m.active)
        .bind(m.closed)
        .bind(m.accepting_orders);
        self.store.exec(q).await?;
        Ok(())
    }

    // ---------------------------------------------------------------- events & audit

    pub async fn insert_risk_event(
        &self,
        correlation_id: Option<Uuid>,
        signal_id: Option<Uuid>,
        r: &RiskRejection,
    ) -> Result<(), StoreError> {
        let q = sqlx::query(
            "INSERT INTO risk_events (correlation_id, signal_id, reason_code, detail)
             VALUES ($1,$2,$3,$4)",
        )
        .bind(correlation_id)
        .bind(signal_id)
        .bind(r.code())
        .bind(serde_json::to_value(r).unwrap_or_default());
        self.store.exec(q).await?;
        Ok(())
    }

    pub async fn insert_system_event(&self, e: &SystemEvent) -> Result<(), StoreError> {
        let q = sqlx::query("INSERT INTO system_events (kind, critical, payload) VALUES ($1,$2,$3)")
            .bind(e.kind())
            .bind(e.is_critical())
            .bind(serde_json::to_value(e).unwrap_or_default());
        self.store.exec(q).await?;
        Ok(())
    }

    pub async fn insert_latency(
        &self,
        correlation_id: Option<Uuid>,
        stage: &str,
        micros: i64,
    ) -> Result<(), StoreError> {
        let q = sqlx::query("INSERT INTO latency_metrics (correlation_id, stage, micros) VALUES ($1,$2,$3)")
            .bind(correlation_id)
            .bind(stage)
            .bind(micros);
        self.store.exec(q).await?;
        Ok(())
    }

    pub async fn audit(
        &self,
        actor: &str,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<(), StoreError> {
        let q = sqlx::query("INSERT INTO audit_logs (actor, action, target, detail) VALUES ($1,$2,$3,$4)")
            .bind(actor)
            .bind(action)
            .bind(target)
            .bind(detail);
        self.store.exec(q).await?;
        Ok(())
    }

    // ---------------------------------------------------------------- wallets

    pub async fn upsert_wallet(&self, w: &TargetWallet) -> Result<(), StoreError> {
        let q = sqlx::query(
            "INSERT INTO target_wallets
             (address, nickname, enabled, sizing_mode, max_trade_usd, max_exposure_usd,
              min_trade_usd, min_source_notional_usd, allowed_markets, blocked_markets, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,NOW())
             ON CONFLICT (address) DO UPDATE SET
               nickname = EXCLUDED.nickname,
               enabled = EXCLUDED.enabled,
               sizing_mode = EXCLUDED.sizing_mode,
               max_trade_usd = EXCLUDED.max_trade_usd,
               max_exposure_usd = EXCLUDED.max_exposure_usd,
               min_trade_usd = EXCLUDED.min_trade_usd,
               min_source_notional_usd = EXCLUDED.min_source_notional_usd,
               allowed_markets = EXCLUDED.allowed_markets,
               blocked_markets = EXCLUDED.blocked_markets,
               updated_at = NOW()",
        )
        .bind(w.address.as_str())
        .bind(&w.nickname)
        .bind(w.enabled)
        .bind(serde_json::to_value(w.sizing).unwrap_or_default())
        .bind(w.max_trade_usd.get())
        .bind(w.max_exposure_usd.get())
        .bind(w.min_trade_usd.get())
        .bind(w.min_source_notional_usd.get())
        .bind(w.allowed_markets.iter().map(|m| m.to_string()).collect::<Vec<_>>())
        .bind(w.blocked_markets.iter().map(|m| m.to_string()).collect::<Vec<_>>());
        self.store.exec(q).await?;
        Ok(())
    }

    pub async fn delete_wallet(&self, a: &Address) -> Result<(), StoreError> {
        let q = sqlx::query("DELETE FROM target_wallets WHERE address = $1").bind(a.as_str());
        self.store.exec(q).await?;
        Ok(())
    }

    pub async fn load_wallets(&self) -> Result<Vec<TargetWallet>, StoreError> {
        let Some(p) = self.store.pool() else { return Ok(Vec::new()) };
        let rows = sqlx::query("SELECT * FROM target_wallets").fetch_all(p).await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let addr = Address::new(r.get::<String, _>("address")).ok()?;
                let mut w = TargetWallet::new(addr, r.get::<String, _>("nickname"));
                w.enabled = r.get("enabled");
                if let Ok(s) = serde_json::from_value(r.get::<serde_json::Value, _>("sizing_mode")) {
                    w.sizing = s;
                }
                w.max_trade_usd = Usd::new(r.get("max_trade_usd"));
                w.max_exposure_usd = Usd::new(r.get("max_exposure_usd"));
                w.min_trade_usd = Usd::new(r.get("min_trade_usd"));
                w.min_source_notional_usd = Usd::new(r.get("min_source_notional_usd"));
                w.allowed_markets = r.get::<Vec<String>, _>("allowed_markets")
                    .iter().filter_map(|m| MarketId::new(m).ok()).collect();
                w.blocked_markets = r.get::<Vec<String>, _>("blocked_markets")
                    .iter().filter_map(|m| MarketId::new(m).ok()).collect();
                Some(w)
            })
            .collect())
    }

    /// Loads everything needed to resume after a restart.
    pub async fn recover(&self, dedup_since: DateTime<Utc>) -> Result<RecoveredState, StoreError> {
        if self.store.is_ephemeral() {
            debug!("ephemeral store: nothing to recover");
            return Ok(RecoveredState::default());
        }
        let positions = self.load_positions().await?;
        let dedup_entries = self.load_dedup_window(dedup_since).await?;
        let wallets = self.load_wallets().await?;
        let (cash, realized, fees) = match self.latest_pnl_snapshot().await? {
            Some((c, r, f)) => (Some(c), r, f),
            None => (None, Usd::ZERO, Usd::ZERO),
        };
        Ok(RecoveredState {
            orders: Vec::new(), // rehydrated by the OMS from load_open_orders
            positions,
            cash,
            realized_pnl: realized,
            fees_paid: fees,
            dedup_entries,
            wallets,
        })
    }
}

/// Row shape for recovering unfinished orders.
#[derive(Debug, Clone)]
pub struct OrderRow {
    pub order_id: Uuid,
    pub state: String,
    pub venue_order_id: Option<String>,
    pub filled_qty: Decimal,
    pub filled_notional: Decimal,
    pub fees_paid: Decimal,
}

impl OrderRow {
    /// Does this recovered order need reconciling before we can trust the book?
    pub fn needs_reconciliation(&self) -> bool {
        matches!(
            self.state.as_str(),
            "SUBMITTED" | "ACKNOWLEDGED" | "PARTIALLY_FILLED" | "CANCEL_REQUESTED" | "UNKNOWN"
        )
    }

    pub fn parsed_state(&self) -> Option<OrderState> {
        Some(match self.state.as_str() {
            "CREATED" => OrderState::Created,
            "VALIDATED" => OrderState::Validated,
            "SUBMITTED" => OrderState::Submitted,
            "ACKNOWLEDGED" => OrderState::Acknowledged,
            "PARTIALLY_FILLED" => OrderState::PartiallyFilled,
            "FILLED" => OrderState::Filled,
            "CANCEL_REQUESTED" => OrderState::CancelRequested,
            "CANCELLED" => OrderState::Cancelled,
            "REJECTED" => OrderState::Rejected,
            "FAILED" => OrderState::Failed,
            "UNKNOWN" => OrderState::Unknown,
            _ => return None,
        })
    }
}

/// Maps a persisted dedup row back into a tracker key.
pub fn dedup_row_to_key(r: &DedupRow) -> Option<wallet_tracker::ContentKey> {
    Some(wallet_tracker::ContentKey {
        tx_hash: domain::TxHash::new(&r.tx_hash).ok()?,
        trader: Address::new(&r.trader).ok()?,
        token_id: TokenId::new(&r.token_id).ok()?,
        side: match r.side.as_str() { "BUY" => domain::Side::Buy, "SELL" => domain::Side::Sell, _ => return None },
        price: r.price.normalize().to_string(),
        size: r.size.normalize().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn repos() -> Repositories { Repositories::new(Store::ephemeral()) }

    #[tokio::test]
    async fn every_write_is_a_safe_noop_without_a_database() {
        // The system must keep trading when Postgres is gone.
        let r = repos();
        assert!(r.is_ephemeral());
        assert!(r.audit("op", "kill_switch", None, serde_json::json!({})).await.is_ok());
        assert!(r.insert_latency(None, "internal", 100).await.is_ok());
        assert!(r.load_positions().await.unwrap().is_empty());
        assert!(r.load_wallets().await.unwrap().is_empty());
        assert!(r.load_open_orders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ephemeral_recovery_yields_empty_state_not_an_error() {
        let s = repos().recover(Utc::now()).await.unwrap();
        assert!(s.positions.is_empty());
        assert!(s.dedup_entries.is_empty());
        assert!(s.cash.is_none(), "no snapshot means cash is unknown, not zero");
    }

    #[tokio::test]
    async fn ephemeral_source_event_insert_reports_new() {
        // Without a DB we cannot prove a duplicate, so we must not claim one.
        let r = repos();
        assert!(!r.source_event_exists("abc").await.unwrap());
    }

    #[test]
    fn recovered_orders_that_may_have_traded_are_flagged() {
        for s in ["SUBMITTED", "ACKNOWLEDGED", "PARTIALLY_FILLED", "CANCEL_REQUESTED", "UNKNOWN"] {
            let r = OrderRow { order_id: Uuid::nil(), state: s.into(), venue_order_id: None,
                filled_qty: Decimal::ZERO, filled_notional: Decimal::ZERO, fees_paid: Decimal::ZERO };
            assert!(r.needs_reconciliation(), "{s} may have executed and must be reconciled");
        }
        for s in ["FILLED", "CANCELLED", "REJECTED", "FAILED", "CREATED", "VALIDATED"] {
            let r = OrderRow { order_id: Uuid::nil(), state: s.into(), venue_order_id: None,
                filled_qty: Decimal::ZERO, filled_notional: Decimal::ZERO, fees_paid: Decimal::ZERO };
            assert!(!r.needs_reconciliation(), "{s} is settled");
        }
    }

    #[test]
    fn persisted_state_strings_round_trip_through_the_enum() {
        for s in ["CREATED","VALIDATED","SUBMITTED","ACKNOWLEDGED","PARTIALLY_FILLED",
                  "FILLED","CANCEL_REQUESTED","CANCELLED","REJECTED","FAILED","UNKNOWN"] {
            let r = OrderRow { order_id: Uuid::nil(), state: s.into(), venue_order_id: None,
                filled_qty: Decimal::ZERO, filled_notional: Decimal::ZERO, fees_paid: Decimal::ZERO };
            let parsed = r.parsed_state().unwrap_or_else(|| panic!("{s} did not parse"));
            assert_eq!(parsed.as_str(), s, "enum and DB string must agree exactly");
        }
    }

    #[test]
    fn dedup_rows_rebuild_into_tracker_keys() {
        let r = DedupRow {
            tx_hash: format!("0x{:064x}", 1),
            trader: format!("0x{:040x}", 2),
            token_id: "12345".into(),
            side: "BUY".into(),
            price: dec!(0.980),
            size: dec!(5.0),
            occurrences: 2,
            last_seen: Utc::now(),
        };
        let k = dedup_row_to_key(&r).unwrap();
        // Normalisation must match what the live path produces, or restored state
        // would not suppress the duplicate it was saved to suppress.
        assert_eq!(k.price, "0.98");
        assert_eq!(k.size, "5");
    }

    #[test]
    fn malformed_dedup_rows_are_dropped_not_panicked_on() {
        let r = DedupRow { tx_hash: "bad".into(), trader: "bad".into(), token_id: "x".into(),
            side: "SIDEWAYS".into(), price: dec!(1), size: dec!(1), occurrences: 1, last_seen: Utc::now() };
        assert!(dedup_row_to_key(&r).is_none());
    }
}
