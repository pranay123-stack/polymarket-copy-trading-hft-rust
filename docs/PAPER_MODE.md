# Paper mode

## What it is not

It is not "assume every order fills instantly at the requested price". That produces
flattering numbers that vanish in live trading.

## What it is

Orders are matched against the **same order books the live strategy prices against**,
fetched from the real CLOB. The simulator:

- sweeps real price levels and returns the true volume-weighted price;
- **partially fills** when the book is too thin, rather than inventing depth;
- rests unmarketable GTC orders and fills them only when the market genuinely trades
  through them (those fills are marked `is_maker`);
- kills unmarketable IOC/FOK orders immediately;
- applies configurable fees, adverse slippage, and rejection probability;
- **refuses to fill at all if no book is cached** — the failure mode being guarded against
  is a paper engine that fills everything.

A fill can never be better than the order's own limit price, even with slippage disabled.

## Parameters

| Variable | Default | Meaning |
|---|---|---|
| `PAPER_STARTING_CASH_USD` | 10000 | starting cash |
| `SIMULATED_LATENCY_MS` | 45 | round-trip, applied as a real sleep |
| `SIMULATED_LATENCY_JITTER_MS` | 20 | uniform jitter on top |
| `SIMULATED_FEE_BPS` | 0 | matches observed live `fee_rate_bps` of `"0"` |
| `SIMULATED_SLIPPAGE_BPS` | 10 | adverse move on top of the book walk |
| `PARTIAL_FILL_ENABLED` | true | allow short fills |
| `FILL_PROBABILITY` | 0.92 | chance a fillable order fills fully |
| `REJECT_PROBABILITY` | 0.01 | venue rejection rate |
| `SIM_RNG_SEED` | 42 | fix for reproducible sessions |

All are visible on the dashboard via `GET /api/config`.

Defaults are deliberately pessimistic: a paper fill should be *harder* to obtain than a
live one, so paper performance understates rather than flatters.

## Latency is measured, not fabricated

Simulated latency is a real `tokio::time::sleep`, so the same instrumentation produces the
same kind of number in paper and live. A running paper session reports, for example:

```
strategy     p50   0.047ms      ← our sizing work
risk         p50   0.114ms      ← 12 pre-trade checks
submission   p50   0.006ms
internal     p50   0.186ms      ← ingest → wire: the part we own
ack          p50  59.485ms      ← the configured simulated venue latency
end_to_end   p50 459.662ms      ← includes the modelled ~400ms publish delay
```

`detection` in paper/replay reflects the modelled publish delay; on the live feed it is the
real measured one (~392 ms median).

## Reproducibility

Fix `SIM_RNG_SEED` and an entire paper session replays identically — same fills, same
partials, same rejections. A paper run that cannot be reproduced is not evidence.

## Reset

```bash
curl -X POST localhost:8080/api/paper/reset -H "Authorization: Bearer $API_AUTH_TOKEN"
```

Clears simulated positions, resting orders and portfolio state. Refused in live mode,
where wiping the book would desynchronise us from the venue.
