# Latency

## What is measured

| Stage | From → To |
|---|---|
| `detection` | venue publish → our ingest |
| `strategy` | ingest → signal generated |
| `risk` | signal → risk verdict |
| `submission` | verdict → handed to the execution adapter |
| `ack` | submission → venue acknowledgement |
| `execution` | submission → first fill |
| **`internal`** | **ingest → order on the wire** |
| `end_to_end` | venue publish → fill |

## The two numbers that matter, and why they are separate

`detection` is dominated by Polymarket's own publish delay — measured at **392 ms median**
from the RTDS envelope stamp to local arrival, and independently ~400 ms p50 in a live
ingest run. That is not ours to optimise.

`internal` is the part we own: ingest → order on the wire. Measured in a running paper
session:

```
strategy     p50   0.047ms
risk         p50   0.114ms
submission   p50   0.006ms
internal     p50   0.186ms   ← ours
ack          p50  59.485ms   ← simulated venue round trip
end_to_end   p50 459.662ms
```

Reporting only end-to-end would hide a 3× regression in our own code behind the venue's
400 ms. Splitting them is what makes the number actionable.

## Nothing is fabricated

- Stamps are real `DateTime<Utc>` observations taken at each stage.
- A stage with no observations reports **nothing** — not zero. `LatencyStamps` returns
  `None` unless both endpoints were recorded, the metrics layer exports only measured
  stages, and the dashboard omits them.
- Negative deltas (a clock stepping backwards) are **dropped**, not clamped to zero, so
  they cannot corrupt percentiles.
- Simulated latency in paper mode is a real `sleep`, so paper and live are measured the
  same way.
- Replay re-stamps events at processing time. An earlier implementation carried the
  recording's synthetic timestamps into the latency chain and reported a fabricated 606 ms
  "strategy" stage; the stamps now describe this run, while the recording still controls
  ordering and spacing.

## Source-stamp resolution

The RTDS envelope carries a **millisecond** timestamp; the trade payload carries only
**whole seconds**. Only the envelope is usable for latency work. When we must fall back to
the payload stamp, `source_is_coarse` is set and the measurement is flagged rather than
silently reported as precise. REST-backfilled trades are always coarse.

## Percentiles

Nearest-rank over a bounded ring of 4096 recent samples, so every reported percentile is a
**real observation** rather than an interpolation, and a recent regression shows up instead
of being diluted by a lifetime average. The lifetime `count` remains exact.

## Throughput

From `cargo test -p app --release --test load_test -- --ignored`:

```
200,000 source events, 20 tracked wallets
  events/sec        : 230,520
  detect + dedup    : p50 172ns   p99 3,640ns
  signal generation : p50 1,507ns p99 3,477ns
  risk check        : p50 331ns   p99 814ns
  headroom vs live  : ~7,000x (live feed measured ~33 msg/s)
```

The hot path is ~7,000× faster than the feed it consumes, so detection latency is bounded
by the network and the venue, not by us.

## Component benchmarks

`cargo bench -p app --bench pipeline`, criterion medians on the development machine:

| Component | Median |
|---|---|
| RTDS frame parse | 2.39 µs |
| wallet match, miss (1 / 10 / 100 wallets) | 57 / 136 / 108 ns |
| dedup identity hash | 787 ns |
| dedup observe (live path) | 1.80 µs |
| copy sizing | 376 ns |
| limit price from slippage budget | 139 ns |
| signal generation (book depth 5 / 50) | 2.79 / 2.86 µs |
| **full risk check (12 checks)** | **254 ns** |
| book sweep VWAP (depth 5 / 50 / 500) | 519 ns / 843 ns / 1.01 µs |
| parse + normalise 100-level book | 23.4 µs |
| portfolio apply fill | 691 ns |

Wallet matching is flat in the number of tracked wallets, as a hash lookup should be — the
firehose cost does not grow as more traders are copied.

These are measured, not projected. Re-run them; do not trust the table.
