# Status

Measured 2026-08-19 against **live Polymarket production**. Re-run everything; do not
trust this file.

## Completion

| Track | Complete | Basis |
|---|---|---|
| **Paper mode on real Polymarket data** | **~98%** | full loop proven live; books now streamed |
| **Ready for live testing** | **~85%** | signing recovered and cryptographically proven |

## Paper mode on real data

Proven live end-to-end: real RTDS firehose → wallet match → dedup → streamed book →
sizing → risk → simulated execution against the real book → portfolio → PnL → Postgres →
API → dashboard.

Observed in one session (6 real target wallets, no demo data):

```
frames examined : 5,958
wallet matches  : 107
copies          : 3 FILLED, 1 PARTIALLY_FILLED, 3 REJECTED (slippage, correctly)
positions       : 3 real positions at real entry prices
slippage        : +10bps, +267bps, -23bps vs the source fill
```

Postgres verified: 12 tables, 41 indexes, migrations on connect, the full
`source_event → copy_signal → order → fill` chain joinable by `correlation_id`, and crash
recovery restoring dedup state, positions and exact cash.

**Order books are now streamed**, not fetched per signal. The subscription set is warmed
at startup from each target wallet's own recent trade history (measured: 218 tokens added
beyond the 144 seeded from liquidity), and any newly-seen token is followed automatically,
so a market pays a REST round trip at most once.

### Remaining ~2%

- The first-ever trade in a market not covered by warming still pays one REST fetch
  (~200ms). Structural, and bounded to once per market.
- Redis is configured but unused.

## Ready for live testing

**The signing blocker is resolved.** The deployed EIP-712 scheme was recovered from chain
and proved by recovering the signers of real settled orders — see
[`docs/SIGNING.md`](SIGNING.md). This mattered: public clients document domain
`version = "1"`, the deployed contract uses **`"3"`**, and the order struct differs in four
fields. Building from the docs would have produced silently-rejected signatures.

Done and verified: the two-switch arming interlock, all credential preconditions, the
live-only order cap, `require_market_data`, the full risk engine, kill switch,
reconciliation with auto-halt, L2 HMAC (RFC 4231 vectors), and EIP-712 order signing
(EOA and POLY_PROXY).

**Still unverified — needs a funded account:**

1. `POST /order` has never *accepted* a signature from this module. The cryptography is
   proven; the JSON envelope and the venue's acceptance are not.
2. The L2 HMAC message construction has never seen an authenticated `200`.
3. The `POST /order` success body, so `venue_order_id` extraction is defensive guesswork.
4. Cancellation request/response shape.
5. `GET /data/trades`, so `poll_fills` returns empty and fills arrive via reconciliation.
6. `signatureType = 3` (the dominant on-chain type) is a delegated/session-key scheme this
   module deliberately does not attempt.

## Recommended first live test

`MAX_LIVE_ORDER_USD=5`, one target wallet, `MAX_DAILY_LOSS_USD` low. Place one
minimum-size order and confirm reconciliation agrees before raising anything.
