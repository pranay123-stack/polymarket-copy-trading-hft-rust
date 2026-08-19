# Deployment

## Docker Compose

```bash
docker compose up
```

Starts PostgreSQL, Redis, the backend (paper + demo by default) and the dashboard on
`http://localhost:5173`. The backend waits on Postgres's healthcheck, so migrations never
race an unready database.

Overrides come from the environment:

```bash
APP_MODE=paper TARGET_WALLETS=0xabc…:Whale:0.25:100:1000 \
API_AUTH_TOKEN=$(openssl rand -hex 32) docker compose up
```

The default is safe on purpose: `docker compose up` with no configuration produces a
populated dashboard and cannot touch real funds.

## Migrations

Applied automatically on connect, idempotently (`CREATE TABLE IF NOT EXISTS`, `CREATE INDEX
IF NOT EXISTS`). No separate migration step, and a restart against an existing database is
a no-op.

## Running without a database

```bash
EPHEMERAL_STORAGE=true cargo run --release -- --mode paper --demo
```

Every repository call becomes a no-op and trading continues. **Crash recovery and durable
audit are unavailable**, and the health endpoint reports `database: DEGRADED` with the
reason rather than implying otherwise. The system also falls back to this automatically if
Postgres is unreachable at startup — a database outage should not halt a live book.

## Crash recovery

On startup, in this order:

1. rebuild the dedup index from `source_events` over the retention window — first, because
   it is what stops a replayed feed re-copying old trades;
2. restore positions, cash, realised PnL and fees from the latest snapshot;
3. restore target wallets;
4. load orders that were still working and flag any that may have executed
   (`SUBMITTED`, `ACKNOWLEDGED`, `PARTIALLY_FILLED`, `CANCEL_REQUESTED`, `UNKNOWN`) for
   reconciliation.

On shutdown (SIGINT/SIGTERM) background tasks are stopped with a timeout, then a final PnL
snapshot and all positions are persisted, so the next start resumes from an accurate book.

## Operations

| Endpoint | Use |
|---|---|
| `GET /api/health` | liveness/readiness probe |
| `GET /api/mode` | confirm whether real money is at stake |
| `GET /metrics` | Prometheus scrape |

Alerts worth wiring:

- `kill_switch_engaged == 1`
- `reconciliation_mismatches_total` increasing
- `feed_connected == 0`
- `risk_rejections_by_reason{reason="daily_loss_limit"}` > 0
- `end_to_end_latency_ms{quantile="0.99"}` above your tolerance

## Hardening

- The container runs as a non-root user (uid 10001) with no shell.
- Only the API port is exposed.
- Secrets come from the environment; `.env` is gitignored.
- Put TLS in front of the API in any deployment that is not localhost — the bearer token
  is sent as a header.
