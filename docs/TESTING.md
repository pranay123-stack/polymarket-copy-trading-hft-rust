# Testing

```bash
cargo test --workspace          # 314 unit + integration tests, ~9s
```

## Layers

**Unit tests** live beside the code they cover. They target *invariants*, not lines: that
terminal order states are absorbing, that a reduction never moves average entry, that
rounding cannot breach a notional cap, that secrets never render their value.

**Integration tests** (`crates/app/tests/pipeline_integration.rs`, 14 tests) wire the real
components together — detection → dedup → strategy → risk → OMS → paper execution →
portfolio → PnL — with only the market data fixed, so outcomes are deterministic.

**Load tests** (`crates/app/tests/load_test.rs`, `#[ignore]`d) drive 200k events and assert
correctness *under load*: no duplicates, no limit breach, no lost events.

```bash
cargo test -p app --release --test load_test -- --ignored --nocapture
```

**Benchmarks** (`cargo bench -p app`) measure the hot path with criterion: frame parsing,
wallet matching at 1/10/100 wallets, dedup hashing, sizing, signal generation at varying
book depth, the full risk check, book normalisation, and position accounting.

**Live verification**:

```bash
./scripts/verify_api.sh                                   # re-checks every API claim
cargo run -p wallet_tracker --example live_ingest_smoke   # production feed → tracker
```

The smoke example connects to the real RTDS feed, samples it to find the busiest wallets,
tracks them, and shows genuine copies being detected. Its output includes real cases of
byte-identical fills receiving distinct event ids — the dedup design proving itself on
production data.

## Tests that encode a hard-won fact

Some tests exist because getting the behaviour wrong is silent and expensive:

| Test | Guards |
|---|---|
| `book_is_normalised_to_best_first` | the venue sorts both sides worst-first; `bids[0]` is the *worst* bid |
| `book_normalisation_is_order_independent` | we sort explicitly, so a venue change is a no-op not a corruption |
| `mixed_case_addresses_from_rtds_still_match` | RTDS sends EIP-55, data-api sends lowercase |
| `genuinely_identical_live_fills_are_both_copied` | collapsing them would under-copy the target |
| `redelivered_fill_produces_exactly_one_order` | backfill overlap must not double-trade |
| `backfill_reconciles_against_the_live_feed_as_a_multiset` | 2 seen + 3 reported = 1 new |
| `a_repeat_storm_is_capped_rather_than_copied_unbounded` | malformed input cannot mint unbounded orders |
| `the_cap_sits_well_above_real_production_volume` | the cap must never reject the 16-fill real case |
| `ambiguous_submission_becomes_unknown_not_failed` | a timeout is not "no order" |
| `venue_overfill_is_refused_and_flagged` | a bogus fill must not enter the position |
| `submitting_without_market_data_refuses_instead_of_inventing_a_fill` | the paper-engine failure mode |
| `simulated_latency_is_actually_elapsed_not_faked` | paper latency must be real |
| `skipping_risk_validation_is_impossible` | `CREATED` cannot reach `SUBMITTED` |
| `unexpected_venue_position_always_warrants_a_halt` | exposure we do not know about |
| `every_mutating_route_is_protected` | no new endpoint escapes an auth decision |
| `unmeasured_stages_report_nothing` | never invent a plausible zero |
| `public_config_view_leaks_no_secrets` | `/api/config` must stay safe |
| `hmac_matches_rfc4231_vector` | the hand-rolled HMAC is verified, not assumed |

## Determinism

- The paper simulator takes a seeded `ChaCha8Rng` (`SIM_RNG_SEED`); a session replays
  identically.
- The demo generator is seeded and produces the same sequence per seed.
- Replay event ids depend only on position and content, never on wall clock.
- Nothing in the test suite requires network or a database. `cargo test` is hermetic.
