# Risk

## The gate

`RiskEngine::check` runs on every order before it can be submitted, and the order state
machine makes that structural: `Submitted` is only reachable from `Validated`, and only
the risk engine issues that transition. Bypassing risk is not a matter of discipline.

Checks run cheapest and most categorical first, so an order halted by the kill switch
never reaches liquidity maths. The order also determines *which* rejection is reported —
always the first limit breached, because that is the one an operator should act on.

| # | Check | Rejection code |
|---|---|---|
| 1 | Kill switch engaged | `kill_switch` |
| 2 | Live mode armed | `live_not_armed` |
| 3 | Subsystem health | `system_unhealthy` |
| 4 | Source event already processed | `duplicate_order` |
| 5 | Wallet enabled | `wallet_disabled` |
| 6 | Market tradable | `market_not_tradable` |
| 7 | Daily loss limit | `daily_loss_limit` |
| 8 | Open order slots | `max_open_orders` |
| 9 | Trade size floor / ceiling (+ tighter live cap) | `below_min_order_size`, `max_trade_size` |
| 10 | Projected token / market / portfolio exposure | `max_position`, `max_market_exposure`, `max_portfolio_exposure` |
| 11 | Market data freshness | `stale_market_data` |
| 12 | Liquidity and expected slippage | `insufficient_liquidity`, `slippage_too_wide` |

Exposure projections assume a **full fill** — the conservative direction, since a partial
fill can only land under the limit.

Every rejection carries structured evidence (requested vs limit), is emitted as an event,
persisted to `risk_events`, and counted per reason code in `/api/risk` and Prometheus.
Nothing is refused silently.

## Two things the risk engine deliberately does *not* do

**A database outage does not stop trading.** Losing durable audit is a degraded state;
halting a live book over it is worse. It is reported as `DEGRADED`, loudly, and trading
continues.

**Ordinary limit hits do not engage the kill switch.** Only breaches that indicate the
*system* is in a bad state do — currently the daily loss limit and, in live mode, a
position reconciliation mismatch. Otherwise one oversized signal would halt everything.

## Kill switch

Backend-enforced, inside the risk engine, on the path every order must traverse. The
dashboard can trigger and display it; disabling the dashboard cannot re-enable trading.

- Engaging is always permitted, never fails, and is idempotent — a second engage keeps the
  *original* reason, because the first cause is the diagnostically useful one.
- Disengaging is a separate, explicit, attributed action. The asymmetry is intentional.
- Engaging optionally cancels resting orders; cancellation is best-effort and reported
  honestly, while the halt itself is already in force regardless.

```bash
curl -X POST localhost:8080/api/risk/kill-switch \
  -H 'Authorization: Bearer $API_AUTH_TOKEN' \
  -d '{"reason":"manual halt","cancel_open_orders":true}'
```

## Sizing caps can only reduce

Every sizing mode funnels through one path, then a fixed sequence of caps:

```
raw notional → wallet max trade → global max trade → position headroom
             → wallet exposure → remaining daily risk → liquidity (risk-adjusted only)
             → minimum floor (or refuse)
```

A cap never increases size. Shares are derived at the **worst acceptable price** and
rounded *down* to the venue's size increment, so rounding cannot breach a notional cap.
The binding constraint is recorded on the signal, so a systematically undersized copy is
visible rather than mysterious.

## Idempotency

The single most important safety property of a copy trader: one source fill must produce
at most one order.

Polymarket makes this hard. Across 1466 consecutive live trades there were 587 distinct
`transactionHash` values, one transaction carried 16 fills, and byte-identical rows
appeared within a single transaction — same wallet, side, asset, price, size. There is no
unique key to deduplicate on.

Identity is `SHA-256(txHash ‖ trader ‖ token ‖ side ‖ price ‖ size ‖ occurrence)`.

The two delivery paths get different rules, and conflating them is the trap:

- **Live feed** — each arrival takes the next ordinal. An identical row genuinely is a
  second fill; collapsing it would under-copy the target.
- **REST backfill** — claims ordinals from 0 upward; any ordinal the live feed already
  holds is the *same* fill and is dropped. This is multiset reconciliation: if the feed saw
  2 identical fills and backfill reports 2, they are the same 2, not 4.

Probing for a free ordinal on both paths would look symmetric and be wrong — a re-delivered
fill would take a fresh ordinal and be copied twice.

**A ceiling bounds malformed input.** `MAX_OCCURRENCES_PER_CONTENT = 64` (≈4× the worst
production case) caps how many identical fills one transaction may produce. Without it, a
backfill bug re-emitting rows would translate one-for-one into orders: a 50,000-row storm
produced 49,999 orders before the ceiling and 63 after. Breaches are counted and surfaced
via `suspicious_suppressions`, never silently swallowed.

Protection is enforced **twice**: in memory, and by the database
(`source_events.event_id` PRIMARY KEY plus a UNIQUE on the content tuple), so a restart or
a race surfaces as a constraint violation rather than a duplicate order.

## Reconciliation

Internal positions are compared against the venue's every 60 s.

A quantity disagreement above tolerance is always reported. An **unexpected venue
position** — exposure in a market we have no record of — always warrants a halt regardless
of tolerance, because it means our view of reality is wrong. In live mode a material
mismatch engages the kill switch and cancels open orders. Nothing is auto-corrected.

Reconciliation also resolves orders stuck in `UNKNOWN` after an ambiguous submission: if
the venue shows quantity we have not booked, the order did execute.
