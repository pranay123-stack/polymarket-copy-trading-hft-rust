# Live mode

## Arming requires two independent switches

```bash
APP_MODE=live
LIVE_TRADING_ENABLED=true
```

Either alone does nothing. `APP_MODE=live` without `LIVE_TRADING_ENABLED` is a startup
error; `LIVE_TRADING_ENABLED=true` in paper mode leaves execution simulated. The guard
exists so a single forgotten variable can never arm real trading, and it is enforced twice
— once in config validation, once in the risk engine (`live_not_armed`).

Live mode additionally refuses to start without:

| Variable | Why |
|---|---|
| `POLYMARKET_PRIVATE_KEY` | EIP-712 (L1) order signing |
| `POLYMARKET_API_KEY` / `_SECRET` / `_PASSPHRASE` | L2 HMAC request auth |
| `POLYMARKET_FUNDER_ADDRESS` | position reconciliation |
| `API_AUTH_TOKEN` | an unauthenticated kill switch on a live book is unacceptable |

`DEMO_DATA=true` is rejected outright in live mode.

Live mode also applies a **tighter per-order cap** (`MAX_LIVE_ORDER_USD`, default \$50) on
top of the global limits, and requires fresh market data (`require_market_data`).

## What is verified, and what is not

Probed against production on 2026-08-19:

| Endpoint | Response without credentials | Conclusion |
|---|---|---|
| `POST /order` | 401 `{"error":"missing address header"}` | real endpoint, L1 gate |
| `POST /auth/api-key` | 401 `{"error":"Invalid L1 Request headers"}` | L1 → L2 derivation |
| `GET /data/orders` | 401 `{"error":"Unauthorized/Invalid api key"}` | L2 HMAC gate |
| `GET /data/trades` | 401 `{"error":"Unauthorized/Invalid api key"}` | L2 HMAC gate |
| `GET /positions?user=` (data-api) | 200 with a user | public, used for reconciliation |

**Could not be verified without a funded account:**

1. the exact EIP-712 typed-data struct and signature encoding for an order;
2. the success-path response body of `POST /order`;
3. cancellation request/response shape;
4. the `/ws/user` fill-event payload.

## Why order signing is not implemented

Writing a plausible-looking EIP-712 signer that cannot be verified would be worse than
leaving it out. A wrong signature produces rejections indistinguishable from network
problems — the system would appear to work, place nothing, and take days to diagnose.

Instead:

```rust
pub trait OrderSigner: Send + Sync {
    fn sign(&self, order: &OrderRequest, tick: Decimal) -> Result<SignedOrder, String>;
    fn address(&self) -> &str;
}
```

Without an injected signer, `LiveExecution::submit` returns:

```
live execution is not configured; missing: an OrderSigner implementation
(EIP-712 L1 signing). See docs/LIVE_MODE.md — no order was sent.
```

`readiness_gaps()` reports **every** missing prerequisite at once, so the operator gets a
complete answer rather than discovering them one restart at a time. At startup a live
adapter with gaps logs an error and **engages the kill switch**, so the API and dashboard
still come up for inspection while nothing can trade.

## Completing the live path

1. Implement `OrderSigner` against the official client's signing scheme (secp256k1 +
   EIP-712 over Polymarket's order struct).
2. Derive L2 credentials via `POST /auth/api-key` using an L1 signature.
3. Verify the HMAC message construction in `L2Credentials::headers` against a real 200
   response. If authenticated requests return `Unauthorized/Invalid api key`, this is the
   first place to look — the field order and separators are the documented scheme but were
   never confirmed against a success path.
4. Confirm the `POST /order` success body and update the `venue_order_id` extraction.
5. Confirm cancellation semantics.
6. Inject with `LiveExecution::with_signer(...)`.
7. Test with a single minimum-size order, `MAX_LIVE_ORDER_USD` set very low, and watch
   reconciliation agree.

Every one of these is confined to `crates/execution/src/live.rs`.

## Pre-flight checklist

```bash
cargo run --release -- --check          # configuration validates
./scripts/verify_api.sh                 # API assumptions still hold
curl localhost:8080/api/mode            # real_money and live_execution_armed
curl localhost:8080/api/status          # health, reconciliation, kill switch
```

Start with `MAX_LIVE_ORDER_USD=5`, one target wallet, and a low `MAX_DAILY_LOSS_USD`.
Confirm reconciliation reports clean before raising anything.
