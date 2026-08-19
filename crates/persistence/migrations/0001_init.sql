-- Polymarket copy-trader schema.
--
-- Design notes that matter:
--
-- * `source_events.event_id` is the SHA-256 of (content ‖ occurrence). It is the
--   PRIMARY KEY, which makes duplicate protection a *database* guarantee rather than
--   only an in-memory one. Polymarket publishes no unique fill id, so this derived id
--   is the only thing standing between a re-delivered fill and a duplicate order.
-- * `(tx_hash, trader, token_id, side, price, size, occurrence)` carries a UNIQUE
--   constraint too: it is the tuple the id is derived from, so a hash collision or a
--   derivation bug surfaces as a constraint violation instead of silent double-trading.
-- * Every table carries `correlation_id` so one source trade can be traced end to end.

CREATE TABLE IF NOT EXISTS target_wallets (
    address                  TEXT PRIMARY KEY,
    nickname                 TEXT NOT NULL,
    enabled                  BOOLEAN NOT NULL DEFAULT TRUE,
    sizing_mode              JSONB NOT NULL,
    max_trade_usd            NUMERIC(20,6) NOT NULL,
    max_exposure_usd         NUMERIC(20,6) NOT NULL,
    min_trade_usd            NUMERIC(20,6) NOT NULL,
    min_source_notional_usd  NUMERIC(20,6) NOT NULL DEFAULT 0,
    allowed_markets          TEXT[] NOT NULL DEFAULT '{}',
    blocked_markets          TEXT[] NOT NULL DEFAULT '{}',
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS markets (
    market_id        TEXT PRIMARY KEY,
    slug             TEXT NOT NULL DEFAULT '',
    title            TEXT NOT NULL DEFAULT '',
    outcomes         JSONB NOT NULL DEFAULT '[]',
    tick_size        NUMERIC(12,6) NOT NULL DEFAULT 0.01,
    min_order_size   NUMERIC(20,6) NOT NULL DEFAULT 5,
    neg_risk         BOOLEAN NOT NULL DEFAULT FALSE,
    active           BOOLEAN NOT NULL DEFAULT FALSE,
    closed           BOOLEAN NOT NULL DEFAULT TRUE,
    accepting_orders BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The idempotency table. A row here means "we have already processed this fill".
CREATE TABLE IF NOT EXISTS source_events (
    event_id        TEXT PRIMARY KEY,
    correlation_id  UUID NOT NULL,
    trader          TEXT NOT NULL,
    market_id       TEXT NOT NULL,
    token_id        TEXT NOT NULL,
    outcome         TEXT NOT NULL DEFAULT '',
    side            TEXT NOT NULL,
    price           NUMERIC(20,10) NOT NULL,
    size            NUMERIC(20,10) NOT NULL,
    notional_usd    NUMERIC(20,6) NOT NULL,
    tx_hash         TEXT NOT NULL,
    occurrence      INTEGER NOT NULL,
    source          TEXT NOT NULL,
    source_ts       TIMESTAMPTZ NOT NULL,
    detected_ts     TIMESTAMPTZ NOT NULL,
    processed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT source_events_content_unique
        UNIQUE (tx_hash, trader, token_id, side, price, size, occurrence)
);
CREATE INDEX IF NOT EXISTS idx_source_events_trader     ON source_events (trader, source_ts DESC);
CREATE INDEX IF NOT EXISTS idx_source_events_market     ON source_events (market_id, source_ts DESC);
CREATE INDEX IF NOT EXISTS idx_source_events_source_ts  ON source_events (source_ts DESC);
CREATE INDEX IF NOT EXISTS idx_source_events_corr       ON source_events (correlation_id);
CREATE INDEX IF NOT EXISTS idx_source_events_tx         ON source_events (tx_hash);

CREATE TABLE IF NOT EXISTS copy_signals (
    signal_id        UUID PRIMARY KEY,
    correlation_id   UUID NOT NULL,
    source_event_id  TEXT NOT NULL REFERENCES source_events(event_id) ON DELETE CASCADE,
    target_wallet    TEXT NOT NULL,
    market_id        TEXT NOT NULL,
    token_id         TEXT NOT NULL,
    outcome          TEXT NOT NULL DEFAULT '',
    side             TEXT NOT NULL,
    target_price     NUMERIC(20,10) NOT NULL,
    target_quantity  NUMERIC(20,10) NOT NULL,
    target_notional  NUMERIC(20,6) NOT NULL,
    copy_quantity    NUMERIC(20,10) NOT NULL,
    copy_notional    NUMERIC(20,6) NOT NULL,
    limit_price      NUMERIC(20,10) NOT NULL,
    sizing_mode      TEXT NOT NULL,
    confidence       DOUBLE PRECISION NOT NULL,
    metadata         JSONB NOT NULL DEFAULT '{}',
    source_ts        TIMESTAMPTZ NOT NULL,
    detection_ts     TIMESTAMPTZ NOT NULL,
    signal_ts        TIMESTAMPTZ NOT NULL,
    -- One signal per source event: the second line of duplicate defence.
    CONSTRAINT copy_signals_one_per_source UNIQUE (source_event_id)
);
CREATE INDEX IF NOT EXISTS idx_copy_signals_wallet ON copy_signals (target_wallet, signal_ts DESC);
CREATE INDEX IF NOT EXISTS idx_copy_signals_corr   ON copy_signals (correlation_id);

CREATE TABLE IF NOT EXISTS orders (
    order_id         UUID PRIMARY KEY,
    correlation_id   UUID NOT NULL,
    signal_id        UUID REFERENCES copy_signals(signal_id) ON DELETE SET NULL,
    venue_order_id   TEXT,
    market_id        TEXT NOT NULL,
    token_id         TEXT NOT NULL,
    side             TEXT NOT NULL,
    order_type       TEXT NOT NULL,
    time_in_force    TEXT NOT NULL,
    quantity         NUMERIC(20,10) NOT NULL,
    limit_price      NUMERIC(20,10) NOT NULL,
    reference_price  NUMERIC(20,10) NOT NULL,
    state            TEXT NOT NULL,
    filled_qty       NUMERIC(20,10) NOT NULL DEFAULT 0,
    filled_notional  NUMERIC(20,6) NOT NULL DEFAULT 0,
    fees_paid        NUMERIC(20,6) NOT NULL DEFAULT 0,
    reject_reason    TEXT,
    mode             TEXT NOT NULL,
    latency          JSONB NOT NULL DEFAULT '{}',
    created_at       TIMESTAMPTZ NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_orders_state    ON orders (state, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_orders_market   ON orders (market_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_orders_corr     ON orders (correlation_id);
CREATE INDEX IF NOT EXISTS idx_orders_created  ON orders (created_at DESC);
-- Recovering unfinished orders on startup must be fast and exact.
CREATE INDEX IF NOT EXISTS idx_orders_open ON orders (state)
    WHERE state IN ('CREATED','VALIDATED','SUBMITTED','ACKNOWLEDGED','PARTIALLY_FILLED','CANCEL_REQUESTED','UNKNOWN');

CREATE TABLE IF NOT EXISTS fills (
    fill_id         UUID PRIMARY KEY,
    order_id        UUID NOT NULL REFERENCES orders(order_id) ON DELETE CASCADE,
    correlation_id  UUID NOT NULL,
    market_id       TEXT NOT NULL,
    token_id        TEXT NOT NULL,
    side            TEXT NOT NULL,
    quantity        NUMERIC(20,10) NOT NULL,
    price           NUMERIC(20,10) NOT NULL,
    fee             NUMERIC(20,6) NOT NULL DEFAULT 0,
    venue_fill_id   TEXT,
    is_maker        BOOLEAN NOT NULL DEFAULT FALSE,
    filled_at       TIMESTAMPTZ NOT NULL,
    -- A venue fill id must never be booked twice.
    CONSTRAINT fills_venue_unique UNIQUE (venue_fill_id)
);
CREATE INDEX IF NOT EXISTS idx_fills_order  ON fills (order_id);
CREATE INDEX IF NOT EXISTS idx_fills_time   ON fills (filled_at DESC);
CREATE INDEX IF NOT EXISTS idx_fills_corr   ON fills (correlation_id);

CREATE TABLE IF NOT EXISTS positions (
    token_id      TEXT PRIMARY KEY,
    market_id     TEXT NOT NULL,
    outcome       TEXT NOT NULL DEFAULT '',
    net_quantity  NUMERIC(20,10) NOT NULL,
    avg_entry     NUMERIC(20,10) NOT NULL,
    realized_pnl  NUMERIC(20,6) NOT NULL DEFAULT 0,
    fees_paid     NUMERIC(20,6) NOT NULL DEFAULT 0,
    mark_price    NUMERIC(20,10),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_positions_market ON positions (market_id);

CREATE TABLE IF NOT EXISTS pnl_snapshots (
    id              BIGSERIAL PRIMARY KEY,
    at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    cash            NUMERIC(20,6) NOT NULL,
    position_value  NUMERIC(20,6) NOT NULL,
    equity          NUMERIC(20,6) NOT NULL,
    realized_pnl    NUMERIC(20,6) NOT NULL,
    unrealized_pnl  NUMERIC(20,6),          -- NULL when the book is unmarked
    fees_paid       NUMERIC(20,6) NOT NULL,
    gross_exposure  NUMERIC(20,6) NOT NULL,
    daily_pnl       NUMERIC(20,6) NOT NULL,
    peak_equity     NUMERIC(20,6) NOT NULL,
    open_orders     INTEGER NOT NULL,
    active_positions INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pnl_at ON pnl_snapshots (at DESC);

CREATE TABLE IF NOT EXISTS risk_events (
    id              BIGSERIAL PRIMARY KEY,
    at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    correlation_id  UUID,
    signal_id       UUID,
    reason_code     TEXT NOT NULL,
    detail          JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_risk_events_at   ON risk_events (at DESC);
CREATE INDEX IF NOT EXISTS idx_risk_events_code ON risk_events (reason_code, at DESC);
CREATE INDEX IF NOT EXISTS idx_risk_events_corr ON risk_events (correlation_id);

CREATE TABLE IF NOT EXISTS system_events (
    id       BIGSERIAL PRIMARY KEY,
    at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    kind     TEXT NOT NULL,
    critical BOOLEAN NOT NULL DEFAULT FALSE,
    payload  JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_system_events_at   ON system_events (at DESC);
CREATE INDEX IF NOT EXISTS idx_system_events_kind ON system_events (kind, at DESC);

CREATE TABLE IF NOT EXISTS latency_metrics (
    id              BIGSERIAL PRIMARY KEY,
    at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    correlation_id  UUID,
    stage           TEXT NOT NULL,
    micros          BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_latency_stage ON latency_metrics (stage, at DESC);
CREATE INDEX IF NOT EXISTS idx_latency_corr  ON latency_metrics (correlation_id);

CREATE TABLE IF NOT EXISTS audit_logs (
    id      BIGSERIAL PRIMARY KEY,
    at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    actor   TEXT NOT NULL,
    action  TEXT NOT NULL,
    target  TEXT,
    detail  JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_audit_at     ON audit_logs (at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_action ON audit_logs (action, at DESC);
