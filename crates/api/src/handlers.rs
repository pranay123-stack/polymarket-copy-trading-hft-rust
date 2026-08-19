//! HTTP handlers.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use domain::{Address, MarketId, SizingMode, TargetWallet, Usd};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::state::AppState;

type R = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": msg.into() })))
}

#[derive(Deserialize)]
pub struct LimitQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn default_limit() -> usize { 100 }

// ---------------------------------------------------------------- status

pub async fn health(State(s): State<Arc<AppState>>) -> R {
    let r = s.health.report();
    Ok(Json(json!({
        "status": match r.state {
            domain::HealthState::Healthy => "ok",
            domain::HealthState::Degraded => "degraded",
            domain::HealthState::Down => "down",
        },
        "mode": s.mode.as_str(),
        "uptime_seconds": s.uptime_seconds(),
        "components": r.components,
    })))
}

pub async fn status(State(s): State<Arc<AppState>>) -> R {
    let pnl = s.portfolio.snapshot(s.orders.open_count());
    let t = s.tracker.stats();
    Ok(Json(json!({
        "mode": s.mode.as_str(),
        "real_money": s.is_real_money(),
        "execution_adapter": s.orders.adapter_name(),
        "uptime_seconds": s.uptime_seconds(),
        "kill_switch": s.kill_switch.state(),
        "health": s.health.report(),
        "pnl": pnl,
        "tracker": {
            "wallets": s.tracker.wallet_count(),
            "frames_examined": t.frames_examined,
            "wallet_matches": t.wallet_matches,
            "actionable": t.actionable,
            "skipped": t.skipped,
            "duplicates_suppressed": t.duplicates_suppressed,
            "dedup_contents": s.tracker.dedup_size(),
        },
        "storage": { "ephemeral": s.repos.is_ephemeral() },
    })))
}

/// Deliberately its own endpoint: "am I live?" must be answerable in one call.
pub async fn mode(State(s): State<Arc<AppState>>) -> R {
    Ok(Json(json!({
        "mode": s.mode.as_str(),
        "real_money": s.is_real_money(),
        "live_execution_armed": s.config.live_execution_armed(),
        "execution_adapter": s.orders.adapter_name(),
        "demo_data": s.config.demo_data,
    })))
}

pub async fn config_view(State(s): State<Arc<AppState>>) -> R {
    Ok(Json(s.config.public_view()))
}

pub async fn metrics_prometheus(State(s): State<Arc<AppState>>) -> String {
    // Refresh gauges from live state at scrape time.
    let p = s.portfolio.snapshot(s.orders.open_count());
    s.metrics.pnl_total.set_decimal(p.realized_pnl.get() + p.unrealized_pnl.unwrap_or(Usd::ZERO).get());
    s.metrics.daily_pnl.set_decimal(p.daily_pnl.get());
    s.metrics.fees_total.set_decimal(p.fees_paid.get());
    s.metrics.equity.set_decimal(p.equity.get());
    s.metrics.gross_exposure.set_decimal(p.gross_exposure.get());
    s.metrics.active_positions.set(p.active_positions as i64);
    s.metrics.open_orders.set(p.open_orders as i64);
    s.metrics.tracked_wallets.set(s.tracker.wallet_count() as i64);
    s.metrics.kill_switch_engaged.set(s.kill_switch.is_engaged() as i64);
    s.metrics.render_prometheus()
}

// ---------------------------------------------------------------- trading data

pub async fn positions(State(s): State<Arc<AppState>>) -> R {
    let rows: Vec<Value> = s.portfolio.positions().iter().map(|p| json!({
        "market_id": p.market_id.as_str(),
        "token_id": p.token_id.as_str(),
        "outcome": p.outcome,
        "quantity": p.net_quantity,
        "avg_entry": p.avg_entry,
        "mark_price": p.mark_price.map(|m| m.get()),
        "exposure": p.exposure().get(),
        "unrealized_pnl": p.unrealized_pnl().map(|u| u.get()),
        "realized_pnl": p.realized_pnl.get(),
        "total_pnl": p.total_pnl().get(),
        "fees_paid": p.fees_paid.get(),
        "updated_at": p.updated_at,
    })).collect();
    Ok(Json(json!({ "positions": rows, "count": rows.len() })))
}

pub async fn orders(State(s): State<Arc<AppState>>, Query(q): Query<LimitQuery>) -> R {
    let rows: Vec<Value> = s.orders.all().iter().take(q.limit).map(|o| json!({
        "order_id": o.id().to_string(),
        "correlation_id": o.request.correlation_id.to_string(),
        "venue_order_id": o.venue_order_id,
        "market_id": o.request.market_id.as_str(),
        "token_id": o.request.token_id.as_str(),
        "side": o.request.side.as_str(),
        "type": format!("{:?}", o.request.order_type).to_uppercase(),
        "quantity": o.request.quantity.get(),
        "limit_price": o.request.limit_price.get(),
        "state": o.state.as_str(),
        "filled_qty": o.filled_qty.get(),
        "avg_fill_price": o.avg_fill_price().map(|p| p.get()),
        "fees_paid": o.fees_paid.get(),
        "reject_reason": o.reject_reason,
        "mode": s.mode.as_str(),
        "created_at": o.request.created_at,
        "updated_at": o.updated_at,
        "latency_ms": {
            "detection": o.latency.detection_us().map(|u| u as f64 / 1000.0),
            "internal": o.latency.internal_us().map(|u| u as f64 / 1000.0),
            "ack": o.latency.ack_us().map(|u| u as f64 / 1000.0),
            "execution": o.latency.execution_us().map(|u| u as f64 / 1000.0),
            "end_to_end": o.latency.end_to_end_us().map(|u| u as f64 / 1000.0),
        },
    })).collect();
    Ok(Json(json!({ "orders": rows, "count": rows.len(), "open": s.orders.open_count() })))
}

pub async fn fills(State(s): State<Arc<AppState>>, Query(q): Query<LimitQuery>) -> R {
    // Fills are derived from orders that have executed.
    let rows: Vec<Value> = s.orders.all().iter()
        .filter(|o| !o.filled_qty.is_zero())
        .take(q.limit)
        .map(|o| json!({
            "order_id": o.id().to_string(),
            "correlation_id": o.request.correlation_id.to_string(),
            "market_id": o.request.market_id.as_str(),
            "side": o.request.side.as_str(),
            "quantity": o.filled_qty.get(),
            "price": o.avg_fill_price().map(|p| p.get()),
            "fee": o.fees_paid.get(),
            "at": o.updated_at,
        })).collect();
    Ok(Json(json!({ "fills": rows, "count": rows.len() })))
}

/// The copy-trading panel: source trade joined to our copy.
pub async fn trades(State(s): State<Arc<AppState>>, Query(q): Query<LimitQuery>) -> R {
    Ok(Json(json!({
        "copies": s.recent.copies(q.limit),
        "source_trades": s.recent.source_trades(q.limit).iter().map(|t| json!({
            "event_id": t.event_id.as_str(),
            "correlation_id": t.correlation_id.to_string(),
            "trader": t.trader.as_str(),
            "market_title": t.market_title,
            "outcome": t.outcome,
            "side": t.side.as_str(),
            "price": t.price.get(),
            "quantity": t.quantity.get(),
            "notional": t.notional().get(),
            "tx_hash": t.tx_hash.as_str(),
            "occurrence": t.occurrence,
            "source": t.source.as_str(),
            "source_ts": t.source_ts,
            "detected_ts": t.detected_ts,
        })).collect::<Vec<_>>(),
    })))
}

pub async fn pnl(State(s): State<Arc<AppState>>) -> R {
    let snap = s.portfolio.snapshot(s.orders.open_count());
    Ok(Json(json!({
        "snapshot": snap,
        "total_pnl": s.portfolio.total_pnl().get(),
        "return_pct": s.portfolio.return_pct(),
        "drawdown_pct": snap.drawdown_pct(),
        "available_capital": snap.available_capital().get(),
    })))
}

pub async fn latency(State(s): State<Arc<AppState>>) -> R {
    let stages: Vec<Value> = s.metrics.latency.all_stats().iter().map(|(stage, st)| json!({
        "stage": stage.as_str(),
        "count": st.count,
        "min_ms": st.min_us as f64 / 1000.0,
        "mean_ms": st.mean_us as f64 / 1000.0,
        "p50_ms": st.p50_us as f64 / 1000.0,
        "p95_ms": st.p95_us as f64 / 1000.0,
        "p99_ms": st.p99_us as f64 / 1000.0,
        "max_ms": st.max_us as f64 / 1000.0,
    })).collect();
    Ok(Json(json!({
        "stages": stages,
        // Absent stages are genuinely unmeasured, not zero.
        "note": "only stages with real observations are reported",
    })))
}

pub async fn risk_view(State(s): State<Arc<AppState>>) -> R {
    let snap = s.portfolio.snapshot(s.orders.open_count());
    let l = s.risk.read().limits().clone();
    Ok(Json(json!({
        "kill_switch": s.kill_switch.state(),
        "limits": l,
        "current": {
            "daily_pnl": snap.daily_pnl.get(),
            "gross_exposure": snap.gross_exposure.get(),
            "open_orders": snap.open_orders,
            "equity": snap.equity.get(),
            "drawdown_pct": snap.drawdown_pct(),
        },
        "utilisation": {
            "daily_loss": utilisation(-snap.daily_pnl.get(), l.max_daily_loss_usd.get()),
            "exposure": utilisation(snap.gross_exposure.get(), l.max_portfolio_exposure_usd.get()),
            "open_orders": utilisation(Decimal::from(snap.open_orders), Decimal::from(l.max_open_orders)),
        },
        "rejections": s.metrics.rejection_breakdown(),
        "rejections_total": s.metrics.risk_rejections_total.get(),
    })))
}

fn utilisation(used: Decimal, limit: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    if limit.is_zero() { return 0.0; }
    (used / limit).to_f64().unwrap_or(0.0).clamp(0.0, 10.0)
}

// ---------------------------------------------------------------- kill switch

#[derive(Deserialize)]
pub struct KillSwitchBody {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default = "yes")]
    pub cancel_open_orders: bool,
}
fn yes() -> bool { true }

pub async fn kill_switch_engage(
    State(s): State<Arc<AppState>>,
    Json(b): Json<KillSwitchBody>,
) -> R {
    let reason = b.reason.unwrap_or_else(|| "manual activation via API".into());
    s.kill_switch.set_cancel_open_orders(b.cancel_open_orders);
    let st = s.kill_switch.engage(&reason, "api");
    s.metrics.kill_switch_activations_total.inc();
    s.metrics.kill_switch_engaged.set(1);
    warn!(%reason, "KILL SWITCH ENGAGED");

    let _ = s.events.send(domain::SystemEvent::KillSwitchActivated {
        reason: reason.clone(), by: "api".into(), at: chrono::Utc::now() });
    let _ = s.repos.audit("api", "kill_switch_engage", None, json!({ "reason": reason })).await;

    // Cancelling is best-effort and reported honestly; the halt itself is already in force.
    let cancelled = if b.cancel_open_orders {
        let (ok, fail) = s.orders.cancel_all().await;
        json!({ "cancelled": ok, "failed": fail })
    } else {
        json!({ "cancelled": 0, "failed": 0, "skipped": true })
    };
    Ok(Json(json!({ "kill_switch": st, "open_orders": cancelled })))
}

pub async fn kill_switch_reset(State(s): State<Arc<AppState>>) -> R {
    let st = s.kill_switch.reset("api");
    s.metrics.kill_switch_engaged.set(0);
    info!("kill switch reset");
    let _ = s.events.send(domain::SystemEvent::KillSwitchReset {
        by: "api".into(), at: chrono::Utc::now() });
    let _ = s.repos.audit("api", "kill_switch_reset", None, json!({})).await;
    Ok(Json(json!({ "kill_switch": st })))
}

// ---------------------------------------------------------------- wallets

#[derive(Deserialize)]
pub struct WalletBody {
    pub address: String,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub copy_ratio: Option<Decimal>,
    #[serde(default)]
    pub fixed_usd: Option<Decimal>,
    #[serde(default)]
    pub portfolio_pct: Option<Decimal>,
    #[serde(default)]
    pub max_trade_usd: Option<Decimal>,
    #[serde(default)]
    pub max_exposure_usd: Option<Decimal>,
    #[serde(default)]
    pub min_source_notional_usd: Option<Decimal>,
    #[serde(default)]
    pub allowed_markets: Option<Vec<String>>,
    #[serde(default)]
    pub blocked_markets: Option<Vec<String>>,
}

fn wallet_json(w: &TargetWallet) -> Value {
    json!({
        "address": w.address.as_str(),
        "nickname": w.nickname,
        "enabled": w.enabled,
        "sizing": w.sizing,
        "max_trade_usd": w.max_trade_usd.get(),
        "max_exposure_usd": w.max_exposure_usd.get(),
        "min_trade_usd": w.min_trade_usd.get(),
        "min_source_notional_usd": w.min_source_notional_usd.get(),
        "allowed_markets": w.allowed_markets.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
        "blocked_markets": w.blocked_markets.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
    })
}

pub async fn list_wallets(State(s): State<Arc<AppState>>) -> R {
    let rows: Vec<Value> = s.tracker.list_wallets().iter().map(|w| {
        let mut v = wallet_json(w);
        v["pnl"] = json!(s.portfolio.wallet_pnl(&w.address).get());
        v
    }).collect();
    Ok(Json(json!({ "wallets": rows, "count": rows.len() })))
}

/// Applies a body onto a wallet, validating against global limits.
fn apply_body(w: &mut TargetWallet, b: &WalletBody, s: &AppState) -> Result<(), String> {
    if let Some(n) = &b.nickname { w.nickname = n.clone(); }
    if let Some(e) = b.enabled { w.enabled = e; }
    if let Some(r) = b.copy_ratio {
        if r <= Decimal::ZERO || r > Decimal::from(10) {
            return Err(format!("copy_ratio {r} must be within (0, 10]"));
        }
        w.sizing = SizingMode::FixedRatio { ratio: r };
    }
    if let Some(a) = b.fixed_usd { w.sizing = SizingMode::FixedUsd { amount: a }; }
    if let Some(p) = b.portfolio_pct { w.sizing = SizingMode::PortfolioPercent { pct: p }; }
    if let Some(v) = b.max_trade_usd {
        let global = s.risk.read().limits().max_trade_usd;
        if Usd::new(v) > global {
            // A per-wallet limit must never exceed the global one.
            return Err(format!("max_trade_usd {v} exceeds the global limit {global}"));
        }
        w.max_trade_usd = Usd::new(v);
    }
    if let Some(v) = b.max_exposure_usd { w.max_exposure_usd = Usd::new(v); }
    if let Some(v) = b.min_source_notional_usd { w.min_source_notional_usd = Usd::new(v); }
    if let Some(ms) = &b.allowed_markets {
        w.allowed_markets = ms.iter().filter_map(|m| MarketId::new(m).ok()).collect();
    }
    if let Some(ms) = &b.blocked_markets {
        w.blocked_markets = ms.iter().filter_map(|m| MarketId::new(m).ok()).collect();
    }
    Ok(())
}

pub async fn add_wallet(State(s): State<Arc<AppState>>, Json(b): Json<WalletBody>) -> R {
    let addr = Address::new(&b.address)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid address: {e}")))?;
    if s.tracker.get_wallet(&addr).is_some() {
        return Err(err(StatusCode::CONFLICT, "wallet already tracked"));
    }
    let nickname = b.nickname.clone().unwrap_or_else(|| format!("{}…", &addr.as_str()[..8]));
    let mut w = TargetWallet::new(addr.clone(), nickname);
    apply_body(&mut w, &b, &s).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    s.tracker.upsert_wallet(w.clone());
    let _ = s.repos.upsert_wallet(&w).await;
    let _ = s.repos.audit("api", "wallet_add", Some(addr.as_str()), wallet_json(&w)).await;
    info!(wallet = %addr, "target wallet added");
    Ok(Json(wallet_json(&w)))
}

pub async fn update_wallet(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(b): Json<WalletBody>,
) -> R {
    let addr = Address::new(&id).map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut w = s.tracker.get_wallet(&addr)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "wallet not tracked"))?;
    apply_body(&mut w, &b, &s).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    s.tracker.upsert_wallet(w.clone());
    let _ = s.repos.upsert_wallet(&w).await;
    let _ = s.repos.audit("api", "wallet_update", Some(addr.as_str()), wallet_json(&w)).await;
    Ok(Json(wallet_json(&w)))
}

pub async fn delete_wallet(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> R {
    let addr = Address::new(&id).map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    match s.tracker.remove_wallet(&addr) {
        None => Err(err(StatusCode::NOT_FOUND, "wallet not tracked")),
        Some(_) => {
            let _ = s.repos.delete_wallet(&addr).await;
            let _ = s.repos.audit("api", "wallet_delete", Some(addr.as_str()), json!({})).await;
            Ok(Json(json!({ "deleted": addr.as_str() })))
        }
    }
}

// ---------------------------------------------------------------- paper

pub async fn paper_reset(State(s): State<Arc<AppState>>) -> R {
    if s.mode.is_live() {
        // Wiping the book in live mode would desynchronise us from the venue.
        return Err(err(StatusCode::FORBIDDEN, "paper reset is refused in LIVE mode"));
    }
    let Some(p) = &s.paper else {
        return Err(err(StatusCode::BAD_REQUEST, "no paper adapter is active"));
    };
    p.reset();
    s.portfolio.reset();
    let _ = s.repos.audit("api", "paper_reset", None, json!({})).await;
    info!("paper state reset");
    Ok(Json(json!({ "reset": true, "cash": s.portfolio.cash().get() })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn utilisation_is_a_bounded_fraction() {
        assert_eq!(utilisation(dec!(50), dec!(100)), 0.5);
        assert_eq!(utilisation(dec!(0), dec!(100)), 0.0);
        assert_eq!(utilisation(dec!(100), dec!(100)), 1.0);
        // Over-limit is visible rather than clipped to 1.
        assert_eq!(utilisation(dec!(150), dec!(100)), 1.5);
        // No division by zero.
        assert_eq!(utilisation(dec!(50), dec!(0)), 0.0);
    }

    #[test]
    fn limit_query_defaults_are_sane() {
        let q: LimitQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 100);
    }

    #[test]
    fn kill_switch_body_defaults_to_cancelling_orders() {
        let b: KillSwitchBody = serde_json::from_str("{}").unwrap();
        assert!(b.cancel_open_orders, "the safe default is to pull resting orders");
        assert!(b.reason.is_none());
    }
}
