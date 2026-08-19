# Polymarket API — Verified Surface

**All statements below were verified live against production on 2026-08-19** from this
machine. Nothing here is copied from prose docs; every claim has a probe behind it.
Re-run `scripts/verify_api.sh` to re-check before trusting this document.

Where a fact could not be verified without funded credentials (order placement,
private-channel fills) it is marked **UNVERIFIED** and isolated behind an adapter.

---

## 1. Hosts

| Purpose | Host | Auth |
|---|---|---|
| Market metadata | `https://gamma-api.polymarket.com` | none |
| CLOB REST (books, markets, orders) | `https://clob.polymarket.com` | none for reads |
| Wallet-attributed trade history | `https://data-api.polymarket.com` | none |
| **Real-time activity feed (RTDS)** | `wss://ws-live-data.polymarket.com` | none |
| Market order-book feed | `wss://ws-subscriptions-clob.polymarket.com/ws/market` | none |
| Own-account feed | `wss://ws-subscriptions-clob.polymarket.com/ws/user` | L2 HMAC |

**DNS:** all hosts now publish **both A and AAAA** records (Cloudflare). An earlier
observation that they were IPv6-only no longer holds. Verified: `104.18.34.205` /
`2606:4700:4408::ac40:9933`. No `curl -4` workaround is needed, but the client pins a
connect timeout anyway.

---

## 2. The copy-trading signal source — RTDS `activity/trades`

This is **the** discovery that makes an event-driven (non-polling) copy trader possible.

```
wss://ws-live-data.polymarket.com
-> {"action":"subscribe","subscriptions":[{"topic":"activity","type":"trades"}]}
```

Pushes **every trade on the platform, wallet-attributed**, with no authentication.

Measured: **~33 trades/sec**, 250 distinct wallets in a 14 s window;
1466 frames in 45 s.

Frame shape (complete, all keys observed):

```json
{
  "connection_id": "gXseIO-NQWeIKEhJwA==",
  "topic": "activity",
  "type": "trades",
  "timestamp": 1787102287053,        // envelope: MILLISECONDS
  "payload": {
    "proxyWallet": "0x510F4963b66B1B18505faaB74b0bB943D1dDa43C",
    "side": "BUY",
    "asset": "72551024098...",        // ERC-1155 token id (the outcome leg)
    "conditionId": "0x1d872089...",   // the market
    "outcome": "No",
    "outcomeIndex": 1,
    "price": 0.26,
    "size": 2.7027,
    "timestamp": 1787102287,          // payload: WHOLE SECONDS
    "transactionHash": "0xb6acf685...",
    "title": "...", "slug": "...", "eventSlug": "...", "icon": "...",
    "name": "PPMT", "pseudonym": "Rowdy-Tenement", "bio": "", "profileImage": "",
    "fee": 0.0                        // PRESENT ON ONLY ~40% OF FRAMES (584/1466)
  }
}
```

### Gotchas that cost real time to find

1. **There is no server-side wallet filter.** Three plausible filter spellings
   (`filters:{"proxyWallet":…}`, `filters:{"user":…}`, `user:…`) were each accepted
   without error and then delivered **zero frames** — they silently break the
   subscription rather than narrowing it. You must consume the **firehose** and match
   target wallets **client-side**. This is why `wallet_tracker` uses a lock-free
   `HashSet<Address>` lookup on the hot path: it runs on all ~33 msg/s.

2. **The first frame after subscribing is an empty string**, not JSON. Parsing it
   throws. Tolerate empty/non-JSON text frames.

3. **`payload.timestamp` is whole seconds — useless for latency work.** Use the
   **envelope `timestamp`, which is milliseconds**. Measured wire latency
   (envelope stamp → local arrival) median **≈392 ms**, and 0.25–1.56 s measured
   against the second-resolution payload stamp. Detection latency is reported
   against the envelope stamp for this reason.

4. **`outcomeIndex` is sometimes `999`** (15/500 rows) — a sentinel, not a real index.
   Never index `outcomes[]` with it. Resolve the leg by `asset` (token id) instead.

5. `activity/orders_matched` is a second real topic (~6 frames/s). `comments` and
   `rfq` with `type:"*"` produced nothing.

---

## 3. Idempotency — **no natural unique key exists**

This is the single most important correctness finding in the project.

On the RTDS feed, across 1466 consecutive trades:

* distinct `transactionHash` = **587** → 586 rows shared a hash with another row
* worst case: **one transaction carried 16 fill rows**
* within a single transaction the **same wallet recurs** with the **same
  side, asset and price**, differing only by size — and sometimes **not even by size**:

```
tx 0x5acd4332…  0x8a5152d0… BUY 0.98  size 12.55
tx 0x5acd4332…  0x8a5152d0… BUY 0.98  size 10
tx 0x5acd4332…  0x8a5152d0… BUY 0.98  size 20
tx 0x5acd4332…  0x8a5152d0… BUY 0.98  size 7
tx 0x5acd4332…  0xe6F7E1Ab… BUY 0.98  size 5     <-- identical pair
tx 0x5acd4332…  0xe6F7E1Ab… BUY 0.98  size 5     <-- identical pair
```

Therefore **every** candidate business key is non-unique:
`txHash`, `(txHash,wallet)`, `(txHash,wallet,asset,side)`,
and even `(txHash,wallet,asset,side,price,size)`.

**Consequence for the design.** Dedup cannot be a pure content key. `wallet_tracker`
derives a `SourceEventId` as a SHA-256 over the canonical content tuple **plus an
occurrence ordinal** — a per-`(txHash, content)` counter that distinguishes the Nth
genuinely-identical fill inside one transaction. The ordinal is what makes the key
both *stable across a restart or a WS/REST replay of the same transaction* and
*non-collapsing across legitimately repeated fills*. See `docs/RISK.md` §Idempotency
and `crates/wallet_tracker/src/dedup.rs`.

---

## 4. REST `data-api.polymarket.com/trades`

Used for **backfill and reconciliation only**, never as the primary path.

`GET /trades?user=<addr>&limit=&offset=&side=&market=&takerOnly=`

`user`, `side` and `market` are genuinely applied (verified by inspecting returned
rows), unlike the WS filters.

### `takerOnly` defaults to **true** — and that silently hides maker fills

| query | rows | distinct tx | max rows/tx |
|---|---|---|---|
| `limit=1000` (default) | 1000 | 1000 | 1 |
| `limit=1000&takerOnly=true` | 1000 | 1000 | 1 |
| `limit=1000&takerOnly=false` | 1000 | **336** | **27** |

The default view returns **one row per transaction — the taker side only**. If a target
trader provides liquidity (rests a limit order that gets hit), that fill is **invisible**
in the default REST view but **present** on the RTDS feed. The backfill client therefore
always sends `takerOnly=false`, otherwise reconciliation would silently disagree with the
live feed. This also explains why REST looks like it has a unique `transactionHash` and
the WS feed does not: they are two different views of the same events.

### Offset paging is unsafe over a live feed

Paging 6×500 rows produced **160 duplicate rows**, while any single atomic
`limit=1000` response had **zero** duplicates. New trades arrive at the head between
requests and shift the window, so offset paging both **duplicates and skips**. Backfill
is therefore bounded by `timestamp`, and every backfilled row goes through the same
dedup gate as the live feed.

Row schema adds `transactionHash`, `name`, `pseudonym`, `profileImage` to the WS payload
fields; `size`/`price` are JSON numbers (floats) — parsed via string into `Decimal`,
never through `f64` arithmetic.

---

## 5. CLOB REST — books and markets

`GET /book?token_id=…` returns:

```
market, asset_id, timestamp (ms), hash, bids[], asks[],
min_order_size, tick_size, neg_risk, last_trade_price
```

> **`bids` and `asks` are BOTH sorted worst-first.**
> `bids` ascending, `asks` descending — **the best price on each side is the LAST
> element.** Verified live: `bids[0]=0.001`, `bids[-1]=0.044`;
> `asks[0]=0.999`, `asks[-1]=0.045`.

Reading `bids[0]` as the best bid is a silent, catastrophic mispricing bug. The book is
normalised into a best-first structure at ingest (`market_data::parser`), and a unit test
pins the ordering against a recorded fixture.

* `tick_size` and `min_order_size` are **per market** (`0.01` vs `0.001` both common);
  quoting against a stale tick gets orders rejected. A `tick_size_change` event exists
  on the market channel.
* `GET /time` is whole seconds; use `/book`'s ms `timestamp` for clock probes.
* `GET /sampling-markets` returns markets with active books — used to seed the paper
  simulator with real liquidity.

## 6. Gamma metadata

`GET /markets?closed=false&order=volumeNum&ascending=false&limit=`

**`clobTokenIds` and `outcomes` are double-encoded** — JSON *strings* containing JSON
arrays: `'["27146…", "33216…"]'`. They must be parsed a second time, and the outcome leg
must be matched **by name**, never by array position.

Gamma 403s some default user agents; `reqwest` sends none, so an explicit UA is always set.

---

## 7. Authentication — required for LIVE only

Two layers, both confirmed by their distinct rejection messages (real endpoints, not 404s):

| Endpoint | Response without creds | Layer |
|---|---|---|
| `POST /order` | 401 `{"error":"missing address header"}` | **L1** EIP-712 wallet signature |
| `POST /auth/api-key` | 401 `{"error":"Invalid L1 Request headers"}` | L1 → derives L2 |
| `GET /auth/derive-api-key` | 401 `{"error":"Invalid L1 Request headers"}` | L1 → derives L2 |
| `GET /data/orders` | 401 `{"error":"Unauthorized/Invalid api key"}` | **L2** HMAC api key |
| `GET /data/trades` | 401 `{"error":"Unauthorized/Invalid api key"}` | L2 HMAC |

**UNVERIFIED without funded credentials:** the exact EIP-712 order struct, signature
encoding, the success-path response body of `POST /order`, cancellation semantics, and
the `/ws/user` fill-event payload. Everything in that set is confined to
`crates/execution/src/live.rs` behind the `ExecutionAdapter` trait, and each unverified
element is marked with a `// UNVERIFIED:` comment naming exactly what must be confirmed
against a funded account. Nothing outside that file depends on those details.
See `docs/LIVE_MODE.md`.
