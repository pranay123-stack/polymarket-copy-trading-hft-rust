# Is copy-trading Polymarket profitable? — measured, not assumed

Short answer: **no evidence that it is, and the measured cost structure is hostile.**
This records what was actually measured, so the question can be argued from numbers.

## 1. What a copy costs before it can make anything

60 active markets with two-sided quotes, sampled 2026-08-19:

| Spread (bps of mid) | p10 | p25 | median | p75 | p90 |
|---|---|---|---|---|---|
| full spread | 44 | 114 | **241** | 923 | 1818 |
| cost to cross (half) | 22 | 57 | **120** | 462 | 909 |

The median active market costs **~120 bps to enter and ~120 bps to exit**. A round trip
crossing both ways starts **~241 bps (2.4%) in the hole**.

Tick size is `0.01` on roughly half of sampled markets. At a price of 0.50 one tick *is*
200 bps, so those markets cannot be tighter than ~2%.

Observed fees were **0 bps**, consistent with `fee_rate_bps: "0"` on executed fills even
where metadata advertises 1000. Fees are not the problem; the spread is.

## 2. What following actually costs

Deterministic replay of **120 real captured Polymarket trades**, 20 copies filled:

```
realized PnL          : -$0.61
fees                  :  $0.00
slippage vs the source: median +30 bps, mean +25 bps
paid MORE than the trader we copied on 19 of 20 copies
```

That is the structural penalty of being second: you buy what someone else just bought,
into the liquidity they did not take. **~25-30 bps per copy, systematically.**

Ten positions were still open, so this is the closed portion only. It is a small sample
and is not offered as a performance result — it is evidence of a directional bias.

## 3. Does being late cost anything?

Polled the tightest, most liquid books continuously for ~55 s:

```
198 snapshot pairs, 0.3-2.5 s apart
top of book changed:   0 / 198   (0%)
```

On slow, liquid markets the price **did not move at all** inside a window several times
longer than our total lag. Depth churned; price did not.

This is the counter-intuitive result: **latency is not the binding constraint.** Being
~400 ms behind costs approximately nothing on these markets. It would matter on fast
markets (5-minute crypto up/down), which are exactly where spreads are widest.

## 4. What would have to be true

For a copy at price `p` to profit:

```
target's edge  >  half-spread entering  (~120 bps median)
               +  half-spread exiting   (~120 bps median)
               +  follower slippage     (~25-30 bps measured)
               +  adverse move in the lag (about 0 on slow markets)
```

**The target's edge must exceed roughly 2.7% per round trip** on a median market. Few
systematic edges are that large. The strategy is only plausible where:

* the target holds to resolution rather than round-tripping (one spread, not two);
* the market is unusually tight (the p10 at ~44 bps, not the median at 241);
* the target's edge is large and slow — informed, not fast.

That is a **selection problem, not a speed problem**. The system already exposes the
levers: `min_source_notional_usd`, `MAX_SLIPPAGE_BPS`, `MIN_LIQUIDITY_USD`, and per-wallet
market allow-lists.

## 5. What would settle it

Nothing here is a backtest. To actually know:

1. Run paper mode for weeks, not minutes, against a curated wallet set.
2. Record every copy with source price, fill price and eventual mark — the schema already
   does this (`copy_signals`, `fills`, `pnl_snapshots`, joined by `correlation_id`).
3. Measure realised PnL **per target wallet** (`portfolio::wallet_pnl` attributes it).
4. Only then decide which wallets, if any, are worth following.

Until that exists, treat any profitability claim about this system — including a
favourable-looking paper run — as unsupported.
