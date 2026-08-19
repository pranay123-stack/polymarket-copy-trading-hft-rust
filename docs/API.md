# API

Base URL `http://localhost:8080`.

## Authentication

Read endpoints are open, so the dashboard works with zero setup in paper mode. **Every
mutating endpoint requires a bearer token**, and config validation refuses to start live
mode without one.

```
Authorization: Bearer <API_AUTH_TOKEN>
```

Token comparison is constant-time — a timing oracle on an endpoint that can halt trading
is not a theoretical concern. When no token is configured, mutating endpoints are open in
paper/replay and **always refused in live**.

A route-coverage test asserts that every mutating route is behind the guard, so a new
endpoint cannot be added without an auth decision.

## Read endpoints

| Endpoint | Returns |
|---|---|
| `GET /api/health` | component health, uptime |
| `GET /api/status` | mode, kill switch, PnL, tracker stats, storage mode |
| `GET /api/mode` | mode, `real_money`, `live_execution_armed` — "am I live?" in one call |
| `GET /api/config` | redacted configuration |
| `GET /api/metrics` | Prometheus exposition (also at `/metrics`) |
| `GET /api/positions` | positions with unrealised/realised/total PnL |
| `GET /api/orders?limit=` | orders with per-stage latency |
| `GET /api/fills?limit=` | executed fills |
| `GET /api/trades?limit=` | copy rows joined to source trades |
| `GET /api/pnl` | snapshot, return, drawdown, available capital |
| `GET /api/latency` | per-stage percentiles from real observations |
| `GET /api/risk` | limits, utilisation, rejections by reason |
| `GET /api/target-wallets` | configured wallets with per-wallet PnL |

## Mutating endpoints

| Endpoint | Effect |
|---|---|
| `POST /api/risk/kill-switch` | halt trading; optionally cancel resting orders |
| `POST /api/risk/kill-switch/reset` | resume trading |
| `POST /api/target-wallets` | add a target wallet |
| `PATCH /api/target-wallets/:address` | update one |
| `DELETE /api/target-wallets/:address` | remove one |
| `POST /api/paper/reset` | clear simulated state (refused in live) |

```bash
# Halt everything
curl -X POST localhost:8080/api/risk/kill-switch \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"reason":"manual halt","cancel_open_orders":true}'

# Track a wallet
curl -X POST localhost:8080/api/target-wallets \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"address":"0x8a5152…","nickname":"Whale","copy_ratio":0.25,"max_trade_usd":100}'
```

Per-wallet limits are validated against the global limits — a wallet can never be given a
larger budget than the system allows:

```json
{"error":"max_trade_usd 999999 exceeds the global limit 100.00"}
```

## WebSocket `/ws`

On connect the server sends a **snapshot** so a freshly-opened dashboard is not blank, then
streams events:

```json
{ "kind": "order_filled", "critical": false, "at": "…", "payload": { … } }
```

`kind` mirrors `SystemEvent::kind()`: `source_trade_detected`, `source_trade_skipped`,
`copy_signal_generated`, `order_risk_approved`, `order_risk_rejected`, `order_submitted`,
`order_acknowledged`, `order_partially_filled`, `order_filled`, `order_cancelled`,
`position_updated`, `pnl_updated`, `risk_limit_breached`, `kill_switch_activated`,
`reconciliation_mismatch`, `health_changed`, `feed_disconnected`, `feed_reconnected`.

`critical` is set for events an operator must not miss.

### Lag, not backpressure

If a client falls behind, the server emits:

```json
{ "kind": "lagged", "payload": { "skipped": 128, "action": "resync via REST" } }
```

and keeps going. A slow browser tab must never be able to apply backpressure to order
handling — the dashboard resyncs from REST instead.

## Values are decimal strings

Money and quantities are serialised as strings, not JSON numbers, so precision survives the
wire. The dashboard parses them only for display and never for arithmetic.

`unrealized_pnl` is **nullable**: `null` means the book is unmarked and the value is
genuinely unknown, which is different from zero. This distinction is preserved through the
API, the database column, and the UI.
