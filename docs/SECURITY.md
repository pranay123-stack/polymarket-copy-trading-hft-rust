# Security

## Secrets

The signing key and API credentials live behind `Secret`, whose `Debug` and `Display` are
redacted:

```rust
assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
```

Since `tracing` renders values via `Debug`, a secret cannot reach a log line even by
accident. `L2Credentials` implements `Debug` manually for the same reason. A test asserts
that `GET /api/config` leaks no credential, database URL or token.

Secrets are read from the environment only. `.env` is gitignored; `.env.example` contains
no real values.

## Authentication

Read endpoints are open so the dashboard works with no setup in paper mode. Every mutating
endpoint — kill switch, wallet configuration, paper reset — requires a bearer token, and:

- comparison is **constant-time**, examining every byte;
- live mode **refuses to start** without `API_AUTH_TOKEN` configured;
- with no token configured, mutating endpoints are open in paper/replay and **always
  refused in live**;
- a route-coverage test asserts every mutating route sits behind the guard.

## Trading-safety controls

| Control | Mechanism |
|---|---|
| Accidental live start | two independent switches, enforced in config *and* the risk engine |
| Runaway size | per-wallet and global caps that can only reduce; extra live-only cap |
| Runaway loss | daily loss limit auto-engages the kill switch |
| Duplicate orders | content+occurrence hashing, enforced in memory **and** by DB constraints |
| Malformed upstream data | occurrence ceiling caps a repeat storm and reports it |
| Position drift | reconciliation every 60s; unexpected venue positions always halt |
| Lost orders | ambiguous submissions become `UNKNOWN`, never `FAILED` |
| Emergency stop | backend-enforced kill switch, independent of the UI |

## Input validation

Everything from the wire is validated at construction: addresses (0x + 40 hex, normalised
lowercase), condition ids and tx hashes (0x + 64 hex), token ids (decimal uint256), prices
(strictly within 0–1), quantities (non-negative). Malformed frames are rejected, not
coerced.

The `outcomeIndex: 999` sentinel is never used to index an array, and Gamma's
double-encoded fields are decoded explicitly with a length-agreement check.

## Threat notes

- **No `unsafe`** anywhere in the workspace.
- **No `unwrap()` on the trading path**; results and options are handled explicitly.
- The dashboard WebSocket is read-only — it broadcasts, it accepts no commands.
- A slow WebSocket client cannot apply backpressure to order handling.
- Bounded memory: the dedup index evicts on a retention window, latency rings are fixed
  size, recent-activity buffers are capped.
- The `Dockerfile` runs as a non-root user with no shell.

## Reporting

If you find a vulnerability, do not open a public issue — contact the maintainer directly.
