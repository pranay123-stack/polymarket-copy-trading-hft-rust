# Execution

## The seam

```rust
#[async_trait]
pub trait ExecutionAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> AdapterCapabilities;
    async fn is_ready(&self) -> bool;
    async fn submit(&self, order: &OrderRequest) -> Result<Acknowledgement, ExecutionError>;
    async fn cancel(&self, id: OrderId, venue_id: Option<&str>) -> Result<(), ExecutionError>;
    async fn positions(&self) -> Result<Vec<VenuePosition>, ExecutionError>;
    async fn poll_fills(&self) -> Result<Vec<Fill>, ExecutionError>;
}
```

The strategy submits an `OrderRequest` and never learns which implementation handled it.
Strategy, risk, the order manager and the portfolio are byte-identical across paper,
replay and live — which is what makes paper results evidence rather than decoration.

`capabilities().is_real_money` is the single source of truth for "are we actually live",
and drives the dashboard's banner.

## Order state machine

```
CREATED ──▶ VALIDATED ──▶ SUBMITTED ──▶ ACKNOWLEDGED ──▶ PARTIALLY_FILLED ──▶ FILLED
   │            │             │              │                  │
   │            │             │              ├──▶ CANCEL_REQUESTED ──▶ CANCELLED
   ▼            ▼             ▼              ▼                  ▼
REJECTED     REJECTED      REJECTED       REJECTED          CANCELLED
FAILED       FAILED        FAILED
                              └──────────▶ UNKNOWN ◀──────────┘
                                              │
                              (reconciliation resolves)
```

Enforced properties:

- **Terminal states are absorbing.** Nothing leaves `FILLED`, `CANCELLED`, `REJECTED`, `FAILED`.
- **Risk cannot be skipped.** `CREATED` cannot reach `SUBMITTED`.
- **Any working state can degrade to `UNKNOWN`**, and `UNKNOWN` reports
  `may_have_executed() == true` so it is never mistaken for "nothing happened".
- **Fills are refused, not absorbed**, on orders never submitted or already terminal, and
  overfills are refused outright — without mutating anything.
- **A fill racing a cancel is legal**, because at a real venue it happens.

## Ambiguity is a first-class outcome

A timeout *after* the request went out does not mean no order exists.

```rust
Err(e) if e.requires_reconciliation() => {
    // Do NOT mark this failed: an order may exist at the venue.
    order.transition(OrderState::Unknown, at);
}
```

`FAILED` asserts "nothing exists at the venue". Being wrong about that loses a real
position. `Ambiguous` and `Transport` both route to `UNKNOWN` for reconciliation.

## Paper adapter

Fills against the same order books the live strategy prices against. It can rest, partially
fill, reject, and run out of liquidity. **Submitting with no cached book is refused** — a
paper engine that fills everything teaches nothing.

Simulated latency is a real `await sleep`, so latency is measured identically in paper and
live rather than substituted with a constant.

## Live adapter

Verified against production: the endpoints exist and their auth layers gate correctly
(`POST /order` → 401 `missing address header` (L1 EIP-712); `GET /data/orders` → 401
`Unauthorized/Invalid api key` (L2 HMAC)). The L2 HMAC implementation is checked against
RFC 4231 test vectors in unit tests.

**What is not implemented, deliberately:** EIP-712 order signing. Its exact typed-data
struct could not be confirmed without funded credentials, and a wrong signature produces
silent rejections that look like connectivity problems. Signing is an `OrderSigner` trait
with no implementation; without one the adapter refuses to submit and lists precisely what
is missing. See `docs/LIVE_MODE.md`.

Everything else unverifiable is marked `// UNVERIFIED:` inline, and nothing outside
`crates/execution/src/live.rs` depends on any of it.
