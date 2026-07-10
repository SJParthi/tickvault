# Implementation Plan: Widen security_id u32 → u64 (Option B) so Groww native ids fold through the shared pipeline

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — grounded directive, this session

## Design

Today `pub type SecurityId = u32` (`crates/common/src/types.rs:316`). Dhan
SecurityIds fit `u32` (4-byte LE wire field per `live-market-feed.md` rule 14),
but **Groww native exchange_token ids do NOT fit `u32`** — for indices Groww
sets bit 62 (`crates/app/src/instruments.rs:318`), so the id requires `u64`.
The Groww bridge currently REJECTS any such tick:
`build_parsed_tick` does `u32::try_from(parsed.tick.security_id)` → on overflow
returns `GrowwAggregateReject::TokenExceedsU32` and the tick never folds to a
candle (`crates/app/src/groww_bridge.rs:307-309`). That is silent loss of every
Groww index tick.

**Option B (this change):** widen the shared `SecurityId` alias and every
in-memory composite key / struct field that carries a SecurityId from `u32` →
`u64`, so the SAME aggregator/registry/pipeline folds BOTH a 32-bit Dhan id and
a 64-bit Groww id with NO forked logic.

Boundaries (deliberately UNCHANGED):
- **Dhan wire read stays 4-byte `u32`.** The header parsers (`quote.rs:49`,
  `full_packet.rs:75`, …) keep `read_u32_le`; we widen LOSSLESSLY at the struct
  assignment with `u64::from(...)`. No wire-format change, no DDL change.
- **QuestDB `ticks.security_id` column is already `LONG` (i64).** ILP writes the
  id as i64 today (`GrowwLiveTickRow.security_id: i64`); no schema change.
- **Groww bridge fix (the payoff):** remove the `u32::try_from` rejection in
  `build_parsed_tick`; carry the Groww id (already a non-negative `i64`) into
  `ParsedTick.security_id` (now `u64`) via `u64::try_from(i64)` (rejects only a
  truly-negative id, which `validate_groww_tick` already filters). Widen
  `seeded: HashSet<(u32,u8)>` → `(u64,u8)` so the SAME u64 id keys both the
  `ticks` row and the candle fold.

## Edge Cases

- **Dhan id in the top u32 range** — `u64::from(u32)` is lossless; the registry,
  aggregator and dedup keys widen transparently. No behaviour change for Dhan.
- **Groww index id with bit 62 set** (e.g. `(1<<62) | 12345`) — previously
  rejected (`TokenExceedsU32`); now folds to a candle. New ratchet test asserts
  this.
- **Negative Groww id** — `validate_groww_tick` already rejects `security_id<=0`
  BEFORE `build_parsed_tick`; the `u64::try_from(i64)` is a defence-in-depth
  second gate (rejects negatives, never a real id).
- **Composite-key collision across feeds** — uniqueness is `(security_id,
  exchange_segment)` per I-P1-11; the persisted `ticks`/`candles_*` DEDUP keys
  additionally carry `feed`, so a Dhan and a Groww row never collide. Widening
  the key element from u32→u64 does not change the collision semantics.
- **Test/bench integer literals** — `u32`-typed literals that now feed a `u64`
  parameter are updated by the compiler-driven cascade.

## Failure Modes

- **Compile cascade incomplete** — any remaining `(u32, _)` SecurityId key is a
  type error; `cargo build --workspace` finds every site (compiler-driven). The
  change is not "done" until the full workspace compiles.
- **Banned-pattern scanner false-positive** — the I-P1-11 scanner targets bare
  `HashSet<u32>` / `HashMap<u32,>`; `(u64,_)` tuple keys are NOT bare-u32
  collections, so they are not flagged. If the scanner flags a `(u64,…)` key we
  STOP and report (do not weaken the change).
- **Precision loss** — none. `u64::from(u32)` (Dhan) and `u64::try_from(i64>=0)`
  (Groww) are both lossless. ILP i64 column already holds the value.

## Test Plan

- `cargo build --workspace` — full compile (the cascade gate).
- `cargo test -p tickvault-common -p tickvault-core -p tickvault-trading -p
  tickvault-storage -p tickvault-app` — paste every `test result:` line.
- **New in-process ratchet** (`crates/app/tests/groww_u64_tick_folds_to_candle.rs`
  or a unit test in `groww_bridge`): feed a Groww tick whose `security_id` is a
  64-bit value with bit 62 set `((1<<62) | 12345)` through the bridge fold path;
  assert a candle is produced for that u64 id (NOT rejected with
  `TokenExceedsU32`).
- Updated ratchet pins: `subscription_planner.rs
  test_regression_seen_ids_key_type_is_pair` → `(u64, ExchangeSegment)`.
- `cargo test --features dhat -p tickvault-core` — 0 hot-path allocations.
- `bash scripts/bench-gate.sh` if present — ≤5% regression.
- `cargo fmt --check`, banned-pattern scanner (exit 0), per-item-guarantee
  check (exit 0).

## Rollback

Single-commit, type-only widening. `git revert <sha>` restores `u32` everywhere
and re-instates the `TokenExceedsU32` rejection. No data migration (the `ticks`
column is already `LONG`), so revert is clean and immediate. No feature flag is
needed — the change is a strict superset (u64 holds every u32 value).

## Observability

- Logging: the bridge fold-reject path keeps its `error!` with `security_id =
  parsed.tick.security_id`; the `TokenExceedsU32` variant is removed (the
  width-overflow can no longer occur), so the only remaining reject is the
  defence-in-depth negative-id guard.
- Monitoring/audit: unchanged — the `ticks` row + `candles_*` seals + the
  per-feed DEDUP keys (with `feed`) continue to fire. No new failure mode is
  introduced (this change REMOVES a loss path), so no new counter/alert is
  required; the existing Groww live-feed-health `record_tick` signal now fires
  for index ticks that were previously dropped.
- Real-time proof: the new in-process candle test is the ratchet that fails the
  build if the u64 fold ever regresses.

---

## Per-Item Guarantee Matrix — this is a single-item plan; all rows apply to the one item

### 15-row "100% Guarantee Matrix" (from per-wave-guarantee-matrix.md)

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` 100% min per crate; `scripts/coverage-gate.sh` | post-merge llvm-cov report | item PR includes coverage delta |
| 100% audit coverage | `<event>_audit` table per typed event with DEDUP UPSERT KEYS | `mcp__tickvault-logs__questdb_sql` | no new audit table — widening only; existing `ticks` DEDUP key unchanged |
| 100% testing coverage | 22 test categories per `testing.md` (unit/integration/property/loom/dhat/fuzz/mutation/sanitizer/coverage/etc.) | `cargo test --workspace` green | item covers unit + integration + dhat |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 8 pre-commit gates | pre-push mandatory | all gates green |
| 100% code performance | DHAT zero-alloc + Criterion p99 budgets + bench-gate ≤5% regression | `cargo bench` + `scripts/bench-gate.sh` | DHAT test re-run (hot path) |
| 100% monitoring | 7-layer telemetry (Prom counter + gauge + tracing span + Loki log + Telegram event + Grafana panel + audit table) | `mcp__tickvault-logs__run_doctor` | existing 7 layers preserved |
| 100% logging | tracing macros mandatory (no println!/dbg!); ERROR → Telegram | hourly errors.jsonl rotation | reject path keeps `error!` |
| 100% alerting | `alerts.yml` Prom rule + `resilience_sla_alert_guard.rs` ratchet | `mcp__tickvault-logs__run_doctor` (CloudWatch alarms) | no new failure mode (loss path removed) |
| 100% security | banned-pattern + secret-scan + `Secret<T>` + security-reviewer agent | `cargo audit` post-deploy | type widening, no new attack surface |
| 100% security hardening | static IP enforcement + pre-commit secret scan + `unused_must_use` lint | post-deploy IP verify | no attack-surface delta |
| 100% bugs fixing | adversarial 3-agent review (proven 4-bug catch rate) | pre-PR + post-impl agent pass | hot-path correctness reviewed |
| 100% scenarios covering | 9-box + chaos test for new failure mode | chaos suite | new bit-62 fold scenario test |
| 100% functionalities covering | every pub fn has call site + test (`pub-fn-wiring-guard.sh` + `pub-fn-test-guard.sh`) | pre-push gates 6+11 | no new pub fn (signature widening only) |
| 100% code review | adversarial 3-agent on diff before AND after impl | per-PR | reviewed on diff |
| 100% extreme check | all of above + ratchet tests fail build on regression | every commit | u64 fold ratchet + key-type pin |

### 7-row "Resilience Demand Matrix" (from per-wave-guarantee-matrix.md)

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | Bounded zero loss inside chaos envelope: ring 2M → spill NDJSON → DLQ | this item REMOVES a loss path (Groww index ticks no longer rejected); introduces no new drop path |
| WS never disconnects | DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close | untouched — no WS lifecycle change |
| Never slow/locked/hanged | DHAT ≤4 alloc blocks/8KB across 10K calls; Criterion p99 ≤100ns; tick-gap >30s Telegram; core_affinity Core 0 | `u64` key is the same size class; DHAT re-run confirms no new hot-path allocation |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal | no DDL change (column already LONG); self-heal untouched |
| O(1) latency | `from_le_bytes` + `papaya` + `Arc<HashMap>` + SPSC bounded; bench-gate ≤5% regression | hash of a `u64` key is O(1), same as `u32`; bench-gate re-run |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS + meta-guard | key element widened u32→u64; collision semantics unchanged; `feed` still in persisted DEDUP key |
| Real-time proof | 7-layer telemetry + SLO-01/SLO-02 @ 10s + market-open self-test @ 09:16:30 IST | in-process candle test pins the u64 fold |

## Honest 100% claim

This change's correctness is guaranteed 100% inside the tested envelope, with
ratcheted regression coverage: the new in-process bit-62 fold test fails the
build if a 64-bit Groww id ever stops folding to a candle; the
`test_regression_seen_ids_key_type_is_pair` pin fails the build if the composite
key element regresses from `u64`; the DHAT zero-alloc re-run confirms no new
hot-path allocation. Beyond the envelope, the existing ring → spill → DLQ chain
catches every payload as recoverable text. Outstanding: none specific to this
widening.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan tick, id fits u32 | folds to candle exactly as before (u64::from lossless) |
| 2 | Groww index tick, id has bit 62 set `((1<<62)\|12345)` | folds to a candle (NOT rejected) — new behaviour |
| 3 | Groww tick, negative id | rejected by `validate_groww_tick` before fold (unchanged) |
| 4 | Cross-feed same logical instrument | distinct rows (feed in persisted DEDUP key) |
