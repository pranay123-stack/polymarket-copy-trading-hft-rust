# Architecture

## The shape of the problem

Copy trading is three hard problems wearing a trenchcoat:

1. **Detect** somebody else's trade, fast and reliably.
2. **Decide** what to do about it, under risk constraints.
3. **Execute** it, without ever doing it twice.

Each of those met a specific obstacle in Polymarket's API, and the architecture is mostly
a record of those obstacles. `docs/POLYMARKET_API.md` has the measurements.

---

## Data flow

```
                    wss://ws-live-data.polymarket.com
                    activity/trades  —  ~33 msg/s, ALL platform trades
                                 │
                                 ▼
        ┌────────────────────────────────────────────┐
        │ market_data::rtds                          │  reconnect w/ jittered backoff
        │   parse → ParsedTrade (domain types)       │  arrival stamped BEFORE parsing
        └────────────────────────────────────────────┘
                                 │
                                 ▼
        ┌────────────────────────────────────────────┐
        │ wallet_tracker                             │
        │   1. is_tracked()  ← one hash lookup       │  ~99% exit here
        │   2. wallet admission (enabled/markets)    │
        │   3. dedup: SHA256(content ‖ occurrence)   │
        └────────────────────────────────────────────┘
                                 │ SourceTrade
                                 ▼
        ┌────────────────────────────────────────────┐
        │ strategy::CopyTrader                       │
        │   reference price from the LIVE touch      │
        │   sizing mode → caps → limit price         │
        └────────────────────────────────────────────┘
                                 │ CopySignal
                                 ▼
        ┌────────────────────────────────────────────┐
        │ risk::RiskEngine — 12 checks, ordered      │
        │   kill switch → arming → health → dedup    │
        │   → wallet → market → daily loss → slots   │
        │   → size → exposure → staleness → liquidity│
        │   → slippage                               │
        └────────────────────────────────────────────┘
                                 │ OrderRequest (Validated)
                                 ▼
        ┌────────────────────────────────────────────┐
        │ execution::OrderManager                    │
        │   state machine, venue id mapping          │
        └────────────────────────────────────────────┘
                                 │
                    ┌────────────┴────────────┐
                    ▼                         ▼
            PaperExecution              LiveExecution
         (simulator + real books)      (CLOB, EIP-712)
                    └────────────┬────────────┘
                                 │ Fill
                                 ▼
              portfolio → PnL → persistence → API → dashboard
```

---

## Why the firehose is consumed in full

The RTDS feed offers no server-side wallet filter. Three plausible spellings
(`filters:{"proxyWallet":…}`, `filters:{"user":…}`, `user:…`) were each accepted without
error and then delivered **zero frames** — they silently break the subscription rather
than narrowing it.

So every frame on the platform reaches our process, and matching is our job. That single
fact drives three decisions:

- **Matching is one `HashMap` lookup on a pre-normalised address**, performed before any
  other work, so the ~99% of frames that are not ours cost almost nothing
  (measured: 172 ns p50 including dedup).
- **Addresses are normalised at construction.** RTDS sends EIP-55 mixed case; `data-api`
  sends lowercase. Comparing them raw matches nothing at all, silently.
- **Arrival is stamped before parsing**, so detection latency measures the wire rather
  than our own CPU time.

---

## Why identity is a hash of content plus an ordinal

Polymarket publishes no unique identifier for a fill, and genuinely emits byte-identical
rows within a single transaction. Every natural key is non-unique.

Identity is `SHA-256(txHash ‖ trader ‖ token ‖ side ‖ price ‖ size ‖ occurrence)`. Because
`txHash` is part of the content, the ordinal is scoped to one transaction and stays small.

The two delivery paths are treated differently, and conflating them is the bug this design
exists to prevent:

| Path | Rule | Rationale |
|---|---|---|
| Live feed | each arrival takes the next ordinal | an identical row really is a second fill |
| REST backfill | claims ordinals from 0; already-held ones are duplicates | overlap after a reconnect is the *same* fill |

Assigning ordinals by probing for a free slot on both paths would look symmetric and be
wrong: a re-delivered fill would take a fresh ordinal and be copied twice.

A ceiling (`MAX_OCCURRENCES_PER_CONTENT = 64`, ~4× the worst observed in production) bounds
the damage if upstream data is malformed — a paging loop re-emitting rows would otherwise
translate one-for-one into orders. Breaches are counted and surfaced, not swallowed.

Duplicate protection is enforced **twice**: in memory, and by the database
(`source_events.event_id` primary key plus a UNIQUE on the content tuple). A restart, a
race, or a bug in the in-memory index all surface as a constraint violation rather than a
duplicate order.

---

## The execution seam

`ExecutionAdapter` is the boundary between the strategy and reality:

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

Everything above it is identical in paper, replay and live. That is not tidiness for its
own sake: it is what makes paper results meaningful, because a paper fill travelled the
same code path, through the same risk checks, in the same state machine, as a live one
would have.

**Ambiguity is a first-class outcome.** A timeout after the request went out does not mean
no order exists. `ExecutionError::Ambiguous` moves the order to `UNKNOWN`, which
`may_have_executed()` reports as true, and reconciliation resolves it. Treating that as
`FAILED` is how a system loses a real position.

---

## State machines over strings

Order state is a validated enum. The transition table makes one property structural rather
than conventional: **an order cannot reach `Submitted` without passing through
`Validated`, and only the risk engine issues that transition.** Skipping risk is a compile-
and runtime-level impossibility, not a code-review promise.

`apply_fill` refuses fills on orders that were never submitted or have already terminated,
and refuses overfills, without mutating anything. Both are integrity failures that must
surface rather than quietly corrupting the position.

---

## Money

Every monetary value is `rust_decimal::Decimal`. Floats appear at exactly one place — the
ingest boundary, because Polymarket sends JSON numbers — and are converted through their
shortest round-trip string form, so `0.5599999776` on the wire becomes exactly
`0.5599999776` and not `0.55999997760000001`.

Direction is a `Side`, never the sign of a quantity: `Qty` cannot be negative. Position
sign lives in one place, `Position::net_quantity`.

---

## Measurement honesty

Absent measurements are `None`, never a plausible zero. `LatencyStamps` returns `None` for
any stage whose endpoints were not both observed, the metrics layer exports only stages
with real samples, and `PnlSnapshot::unrealized_pnl` is `Option` so an unmarked book stays
distinguishable from a flat one — through the API, the database column, and the dashboard.

Detection latency is separated from the latency we actually control:

- `detection_us` — venue publish → our ingest. Mostly Polymarket's (~392 ms median).
- `internal_us` — our ingest → order on the wire. **This is the number to optimise**
  (measured 0.174 ms p50 in a running paper session).

---

## Concurrency

- The trading path holds no lock across an `await`.
- Events go out on a `broadcast` channel, so a stalled dashboard client **lags and skips**
  rather than applying backpressure to order handling. A browser tab must never be able to
  slow down trading.
- Shared state is behind `parking_lot` locks scoped to the smallest possible region;
  hot-path reads (`is_tracked`) take a read lock and return immediately.
- The kill switch is atomic and consulted first in the risk engine.

---

## Failure posture

| Failure | Response |
|---|---|
| Source feed drops | reconnect with jittered backoff; backfill the gap by timestamp, `takerOnly=false` |
| Database unreachable | degrade to ephemeral; keep trading; report `DEGRADED` honestly |
| Submission times out | order → `UNKNOWN`; reconcile; never assume "no order" |
| Venue reports an overfill | refuse the fill, escalate to `UNKNOWN` |
| Position mismatch | alert always; in live, halt and cancel |
| Daily loss breached | auto-engage the kill switch |
| Ordinary limit hit | reject that order only — one oversized signal must not halt everything |
