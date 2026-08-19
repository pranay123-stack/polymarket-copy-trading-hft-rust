// Mirrors the JSON the Rust API emits. Keep in sync with crates/api/src/handlers.rs.

export type Mode = 'PAPER' | 'LIVE' | 'REPLAY'
export type HealthState = 'HEALTHY' | 'DEGRADED' | 'DOWN'

export interface KillSwitchState {
  engaged: boolean
  reason: string | null
  engaged_by: string | null
  engaged_at: string | null
  cancel_open_orders: boolean
  activations: number
}

export interface PnlSnapshot {
  at: string
  cash: string
  position_value: string
  equity: string
  realized_pnl: string
  /** null means the book is unmarked — genuinely unknown, not zero. */
  unrealized_pnl: string | null
  fees_paid: string
  gross_exposure: string
  daily_pnl: string
  peak_equity: string
  open_orders: number
  active_positions: number
}

export interface ComponentStatus {
  name: string
  state: HealthState
  detail: string
  last_ok: string | null
  last_change: string
}

export interface StatusResponse {
  mode: Mode
  real_money: boolean
  execution_adapter: string
  uptime_seconds: number
  kill_switch: KillSwitchState
  health: { state: HealthState; components: ComponentStatus[]; mode: string; at: string }
  pnl: PnlSnapshot
  tracker: {
    wallets: number
    frames_examined: number
    wallet_matches: number
    actionable: number
    skipped: number
    duplicates_suppressed: number
    dedup_contents: number
  }
  storage: { ephemeral: boolean }
}

export interface CopyRow {
  correlation_id: string
  source_event_id: string
  wallet: string
  wallet_nickname: string
  market_title: string
  outcome: string
  side: 'BUY' | 'SELL'
  source_notional: string
  copy_notional: string
  source_price: string
  copy_price: string | null
  slippage_bps: number | null
  status: string
  detection_latency_ms: number | null
  execution_latency_ms: number | null
  end_to_end_latency_ms: number | null
  at: string
}

export interface SourceTradeRow {
  event_id: string
  correlation_id: string
  trader: string
  market_title: string
  outcome: string
  side: 'BUY' | 'SELL'
  price: string
  quantity: string
  notional: string
  tx_hash: string
  occurrence: number
  source: string
  source_ts: string
  detected_ts: string
}

export interface PositionRow {
  market_id: string
  token_id: string
  outcome: string
  quantity: string
  avg_entry: string
  mark_price: string | null
  exposure: string
  unrealized_pnl: string | null
  realized_pnl: string
  total_pnl: string
  fees_paid: string
  updated_at: string
}

export interface OrderRow {
  order_id: string
  correlation_id: string
  venue_order_id: string | null
  market_id: string
  token_id: string
  side: 'BUY' | 'SELL'
  type: string
  quantity: string
  limit_price: string
  state: string
  filled_qty: string
  avg_fill_price: string | null
  fees_paid: string
  reject_reason: string | null
  mode: Mode
  created_at: string
  updated_at: string
  latency_ms: {
    detection: number | null
    internal: number | null
    ack: number | null
    execution: number | null
    end_to_end: number | null
  }
}

export interface LatencyStage {
  stage: string
  count: number
  min_ms: number
  mean_ms: number
  p50_ms: number
  p95_ms: number
  p99_ms: number
  max_ms: number
}

export interface RiskView {
  kill_switch: KillSwitchState
  limits: Record<string, unknown>
  current: {
    daily_pnl: string
    gross_exposure: string
    open_orders: number
    equity: string
    drawdown_pct: string
  }
  utilisation: { daily_loss: number; exposure: number; open_orders: number }
  rejections: Record<string, number>
  rejections_total: number
}

export interface WalletRow {
  address: string
  nickname: string
  enabled: boolean
  sizing: Record<string, unknown>
  max_trade_usd: string
  max_exposure_usd: string
  min_source_notional_usd: string
  allowed_markets: string[]
  blocked_markets: string[]
  pnl: string
}

export interface WsEnvelope {
  kind: string
  critical: boolean
  at: string
  payload: unknown
}
