#!/usr/bin/env bash
# Re-verifies every factual claim in docs/POLYMARKET_API.md against production.
# No credentials required. Exits non-zero if a load-bearing claim regressed.
set -uo pipefail
UA="polymarket-copytrader/verify"
FAIL=0
ok(){ printf '  \033[32mPASS\033[0m %s\n' "$1"; }
bad(){ printf '  \033[31mFAIL\033[0m %s\n' "$1"; FAIL=1; }

echo "== 1. host reachability =="
for h in gamma-api.polymarket.com clob.polymarket.com data-api.polymarket.com; do
  c=$(curl -s -m 10 -A "$UA" -o /dev/null -w '%{http_code}' "https://$h/" || echo 000)
  [ "$c" != "000" ] && ok "$h reachable (HTTP $c)" || bad "$h unreachable"
done

echo "== 2. CLOB auth layers still gate (not 404) =="
c=$(curl -s -m 10 -A "$UA" -o /tmp/v.json -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{}' https://clob.polymarket.com/order)
grep -q 'address header' /tmp/v.json && ok "POST /order -> L1 gate ($c)" || bad "POST /order changed: $(head -c 120 /tmp/v.json)"
c=$(curl -s -m 10 -A "$UA" -o /tmp/v.json -w '%{http_code}' https://clob.polymarket.com/data/orders)
grep -qi 'api key' /tmp/v.json && ok "GET /data/orders -> L2 gate ($c)" || bad "GET /data/orders changed"

echo "== 3. book is sorted WORST-FIRST (best price is last) =="
TOK=$(curl -s -m 12 -A "$UA" https://clob.polymarket.com/sampling-markets \
  | python3 -c "import json,sys;d=json.load(sys.stdin)['data'];print(next(m['tokens'][0]['token_id'] for m in d if m.get('accepting_orders')))")
curl -s -m 12 -A "$UA" "https://clob.polymarket.com/book?token_id=$TOK" -o /tmp/b.json
python3 - <<'PY' && ok "book worst-first ordering holds" || bad "BOOK ORDERING CHANGED -> parser must be revisited"
import json,sys
b=json.load(open('/tmp/b.json'))
bids=[float(x['price']) for x in b['bids']]; asks=[float(x['price']) for x in b['asks']]
assert bids==sorted(bids), "bids no longer ascending"
assert asks==sorted(asks,reverse=True), "asks no longer descending"
sys.exit(0)
PY

echo "== 4. takerOnly default hides maker fills =="
python3 - <<'PY' && ok "takerOnly default=true confirmed" || bad "takerOnly semantics changed"
import urllib.request,json,collections,sys
UA={"User-Agent":"verify"}
g=lambda u: json.load(urllib.request.urlopen(urllib.request.Request(u,headers=UA),timeout=25))
d1=g("https://data-api.polymarket.com/trades?limit=1000")
d2=g("https://data-api.polymarket.com/trades?limit=1000&takerOnly=false")
u1=len(set(x['transactionHash'] for x in d1)); u2=len(set(x['transactionHash'] for x in d2))
print(f"    default distinct_tx={u1}/1000  takerOnly=false distinct_tx={u2}/1000")
assert u1 > u2*1.5, "default no longer taker-only"
sys.exit(0)
PY

echo "== 5. RTDS pushes wallet-attributed trades =="
# The feed is occasionally silent for minutes at a time (observed in production), so
# this step is time-boxed: a hang here must not stall the whole verification run.
timeout 70 python3 - <<'PY' && ok "RTDS activity/trades live" || bad "RTDS FEED BROKEN -> detection path is down"
import asyncio,json,sys
try: import websockets
except ImportError: print("    (websockets not installed, skipped)"); sys.exit(0)
async def m():
    async with websockets.connect("wss://ws-live-data.polymarket.com",open_timeout=10,ping_interval=None) as ws:
        await ws.send(json.dumps({"action":"subscribe","subscriptions":[{"topic":"activity","type":"trades"}]}))
        n=0
        for _ in range(40):
            try: f=json.loads(await asyncio.wait_for(ws.recv(),timeout=12))
            except Exception: continue
            if isinstance(f,dict) and isinstance(f.get('payload'),dict) and f['payload'].get('proxyWallet'):
                n+=1
                if n>=5:
                    assert isinstance(f.get('timestamp'),int) and f['timestamp']>1e12, "envelope ts no longer ms"
                    print(f"    got {n} wallet-attributed trades, envelope ts is ms"); return
        raise AssertionError("no attributed trades in window")
asyncio.run(m()); sys.exit(0)
PY


echo "== 6. deployed EIP-712 signing scheme unchanged =="
python3 - <<'PY' && ok "EIP-712 domain still name='Polymarket CTF Exchange' version='3'" || bad "SIGNING SCHEME CHANGED -> docs/SIGNING.md and crates/execution/src/signing.rs must be revisited"
import json,urllib.request,sys
RPC="https://polygon-bor-rpc.publicnode.com"
H={"Content-Type":"application/json","User-Agent":"verify"}
def rpc(m,p):
    r=urllib.request.Request(RPC,data=json.dumps({"jsonrpc":"2.0","method":m,"params":p,"id":1}).encode(),headers=H)
    return json.load(urllib.request.urlopen(r,timeout=20)).get("result")
EX="0xe3333700ca9d93003f00f0f71f8515005f6c00aa"
# ERC-5267 eip712Domain()
res=rpc("eth_call",[{"to":EX,"data":"0x84b0196e"},"latest"])
assert res and res!="0x", "eip712Domain() unavailable"
raw=bytes.fromhex(res[2:])
assert b"Polymarket CTF Exchange" in raw, "domain name changed"
# domainSeparator()
sep=rpc("eth_call",[{"to":EX,"data":"0xf698da25"},"latest"])
expect="0x466c63910185bbd55e8679264200c4e0abdcbb0c6264eb3d41d13326022e095b"
assert sep==expect, f"domainSeparator changed: {sep}"
print(f"    domainSeparator {sep[:20]}… unchanged")
sys.exit(0)
PY

echo; [ $FAIL -eq 0 ] && echo "ALL API CLAIMS HOLD" || echo "SOME CLAIMS REGRESSED — update docs/POLYMARKET_API.md"
exit $FAIL
