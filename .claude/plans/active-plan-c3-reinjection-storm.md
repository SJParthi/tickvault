# Implementation Plan: C3 — bound STAGE-C.2b WAL re-injection with chunked backpressure (WS-REINJECT-01)

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — C3 task directive, this session (2026-07-03)

> Per-item guarantee matrix: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row) — applies to every item in this plan. The two canonical tables are also pasted verbatim in the "Guarantee Matrices" section below.

## Design

Root cause (2026-07-03 10:35 IST incident, `dropped=1,127,801`): both
STAGE-C.2b call sites in `crates/app/src/main.rs` (fast boot inside
`async fn main()` and the slow-boot mirror inside `start_dhan_lane`)
re-injected the ENTIRE boot-time WAL replay Vec (`ws_frame_spill::replay_all`
returns one unbounded Vec — observed ~1.26M frames) through a synchronous
`sender.try_send(frame)` loop into the pool frame channel
(`FRAME_CHANNEL_CAPACITY = 131_072`). Everything past the capacity was
silently dropped; the drop made the `*_reinjection_clean` flag false, so
`confirm_replayed()` never archived the staged WAL segments — they
re-replayed AND GREW on every restart (self-feeding storm; the 10:35 firing
was a WS-GAP-09 pool-halt `process::exit(2)` restart). The drop `error!` had
no typed `code =` field.

Fix (this plan):

1. New COLD-path helper `crates/app/src/wal_reinject.rs::reinject_wal_frames(sender, frames, chunk_size, send_timeout) -> ReinjectOutcome { injected, aborted_remaining, clean }`:
   - Backpressured `sender.send(frame).await` — never drops while the
     channel is open (this is a once-per-boot recovery path, NOT the
     per-tick hot path at `connection.rs`, so awaiting is correct).
   - `tokio::task::yield_now()` every `WAL_REINJECT_CHUNK_SIZE = 8_192`
     delivered frames (new constant in `crates/common/src/constants.rs`,
     well under `FRAME_CHANNEL_CAPACITY`) so the live WS read loop + tick
     processor keep getting scheduled.
   - Each send wrapped in `tokio::time::timeout` with
     `WAL_REINJECT_SEND_TIMEOUT_SECS = 30` (new constant; parameterized in
     the helper so tests can pass 50ms). Timeout (consumer wedged) or
     `SendError` (channel closed) → STOP, count remaining as
     `aborted_remaining`, `error!(code = ErrorCode::WsReinject01Aborted.code_str(), …)`,
     return `clean = false`. Fail-closed: an abort keeps the WAL
     unconfirmed so segments re-replay next boot (durable floor preserved).
   - O(1) per frame: frames MOVED into the channel (no byte clone), no
     per-frame allocation, no `format!` in the loop.
2. Both call sites replaced with helper calls; `..._reinjection_clean =
   outcome.clean`, so a fully delivered replay finally lets
   `confirm_replayed()` archive the WAL — breaking the storm loop.
3. New typed `ErrorCode::WsReinject01Aborted` (`"WS-REINJECT-01"`,
   `Severity::High`, auto-triage-safe, runbook
   `.claude/rules/project/ws-reinject-error-codes.md`) + `all()` catalogue
   bump 117 → 118 + prefix-pattern arm `WS-REINJECT-`.
4. New rule file `.claude/rules/project/ws-reinject-error-codes.md`
   (crossref-test target + operator runbook).
5. Metrics: keep `tv_ws_frame_wal_reinjected_total` (call sites, per
   injected) and `tv_ws_frame_wal_reinjected_dropped_total` (helper, on
   `aborted_remaining` — semantic continuity); new
   `tv_ws_wal_reinject_aborted_total{reason}` (abort path) and
   `tv_ws_wal_reinject_chunks_total` (per delivered chunk).

Crates touched: `crates/app` (`crates/app/src/main.rs`,
`crates/app/src/wal_reinject.rs`, `crates/app/src/lib.rs`), `crates/common`
(`crates/common/src/constants.rs`, `crates/common/src/error_code.rs`).

## Edge Cases

- Empty replay Vec → helper returns `clean = true`, `injected = 0`; call
  sites are already gated on `!ws_wal_replay_live_feed.is_empty()`.
- Replay >> channel capacity (1M+ frames vs 131,072 slots) → backpressure
  + consumer drain deliver everything; proven by the 1M-frame unit test.
- Frames exactly == / ±1 of `chunk_size` → chunk-boundary tests pin no
  off-by-one (yield fires exactly on the boundary, remainder delivered).
- `chunk_size == 0` → clamped to 1 (no divide-by-zero-analogue, no
  yield-every-frame-starvation of the injector itself beyond correctness).
- Consumer (tick processor) drops its Receiver BEFORE or MID-replay →
  `SendError` → abort, `injected + aborted_remaining == total`, no panic.
- Consumer alive but wedged (never recv) → per-send timeout fires after
  the channel buffer is absorbed → abort with `reason="send_timeout"`.
- Live frames arriving on the SAME channel during a huge replay → mpsc
  waiter fairness + per-chunk yield keep the live path scheduled; the
  liveness test asserts interleaving (first live frame arrives in the
  first half of all arrivals).
- Pool build failed (`ws_pool_ready == None`) → pre-existing branch
  unchanged: frames stay staged, flag false, warn logged.
- Duplicate delivery across boots (replay of unconfirmed segments) →
  absorbed by QuestDB DEDUP keys (STORAGE-GAP-01) — pre-existing contract.

## Failure Modes

- **Consumer dead (`channel_closed`)** → WS-REINJECT-01 `error!` (typed,
  Telegram via the 5-sink chain) + `tv_ws_wal_reinject_aborted_total{reason="channel_closed"}`
  + `tv_ws_frame_wal_reinjected_dropped_total` += aborted_remaining; WAL
  stays staged in `replaying/`; next boot re-replays. Cross-check WS-GAP-07.
- **Consumer wedged (`send_timeout`)** → same abort chain with
  `reason="send_timeout"`; operator triages QuestDB / host per the runbook.
- **Abort mid-run** → `clean = false` → `confirm_replayed()` skipped at
  BOTH boot paths (fast: the confirm block before `notify_systemd_ready`;
  slow: the end-of-main confirm) — never archives a partial delivery, so
  no silent loss is possible.
- **Injector monopolizing the runtime** → prevented by per-chunk
  `yield_now()`; the liveness unit test is the regression guard.
- **Regression back to try_send** → build-failing ratchet
  `wal_reinject::tests::ratchet_main_rs_uses_bounded_reinject_helper`
  (≥2 `reinject_wal_frames(` calls in main.rs, zero `sender.try_send(frame)`).

## Test Plan

All in `crates/app/src/wal_reinject.rs::tests` (run: `cargo test -p tickvault-app --lib wal_reinject`):

- `test_million_frames_all_delivered_zero_drops` — 1,000,000 frames through
  a `FRAME_CHANNEL_CAPACITY` channel with a draining consumer: injected ==
  1,000,000, aborted_remaining == 0, clean == true, consumer receives all.
- `test_receiver_dropped_before_run_aborts_immediately` — clean == false,
  injected == 0, aborted_remaining == 100.
- `test_consumer_dropped_mid_replay_aborts_cleanly` — clean == false,
  injected < total, injected + aborted_remaining == total, no panic.
- `test_wedged_consumer_times_out_and_aborts` — cap-4 channel, 50ms test
  timeout: injected == 4, aborted_remaining == 6, clean == false.
- `test_reinject_wal_frames_empty_vec_is_clean_noop` — injected == 0, clean == true.
- `test_chunk_boundary_exactness` — frames == chunk_size and ±1 all clean.
- `test_zero_chunk_size_is_clamped` — no panic, all delivered.
- `test_live_frames_interleave_with_large_replay` — 500K replay + live
  producer every 5ms into the same channel; all frames of both classes
  arrive; first live frame arrives in the first half (no starvation).
- `ratchet_main_rs_uses_bounded_reinject_helper` — source-scan ratchet
  (both call sites use the helper; raw try_send loop banned;
  `WsReinject01Aborted` exists in crates/common).
- `crates/common`: `test_ws_reinject_01_aborted_contract` (code_str,
  severity, runbook-on-disk, catalogue membership) + the pre-existing
  error-code ratchet suite (unique code_str, roundtrip, prefix pattern,
  `all()` length 118) + `error_code_rule_file_crossref` +
  `error_code_tag_guard`.
- Scoped runs per `testing-scope.md`: `cargo test -p tickvault-common`,
  `cargo test -p tickvault-app`, `cargo check --workspace`, fmt + clippy.

## Rollback

Single-PR, additive-plus-two-call-site change. Rollback = `git revert` of
the PR's commits: the two STAGE-C.2b blocks return to the previous try_send
form, the helper module + constants + ErrorCode variant + rule file are
removed together (the crossref test keeps enum ↔ rule file consistent in
both directions, so a partial revert fails the build loudly). No schema, no
config, no data migration — the WAL on-disk format and `confirm_replayed()`
semantics are untouched, so a rollback boots cleanly against any existing
`replaying/` state (it just re-inherits the old drop behaviour).

## Observability

- `error!` with `code = ErrorCode::WsReinject01Aborted.code_str()` +
  `reason` / `injected` / `aborted_remaining` fields on the abort path
  (routes through the 5-sink chain → errors.jsonl → Telegram).
- Counters: `tv_ws_frame_wal_reinjected_total{ws_type="live_feed"}`
  (delivered), `tv_ws_frame_wal_reinjected_dropped_total{ws_type="live_feed"}`
  (aborted remainder — semantic continuity with the pre-fix counter),
  `tv_ws_wal_reinject_aborted_total{reason}` (new, per abort),
  `tv_ws_wal_reinject_chunks_total` (new, per delivered 8,192-frame chunk —
  progress signal during a large replay). All static labels, zero
  per-frame allocation.
- `info!` success line ("all replayed frames delivered with backpressure")
  and `warn!` skip-confirm line at each call site; the pre-existing
  confirm/no-confirm warn blocks are unchanged.
- Runbook: `.claude/rules/project/ws-reinject-error-codes.md`
  (`mcp__tickvault-logs__find_runbook_for_code "WS-REINJECT-01"` resolves).
- Storm-broken verification signal: after a healthy boot,
  `tv_wal_replay_confirmed_segments_total` rises and `replaying/` empties.

## Plan Items

- [x] Item 1 — Constants + typed ErrorCode + rule file
  - Files: crates/common/src/constants.rs, crates/common/src/error_code.rs, .claude/rules/project/ws-reinject-error-codes.md
  - Tests: test_ws_reinject_01_aborted_contract, test_all_list_length_matches_catalogue_size, test_code_str_follows_expected_prefix_pattern, every_error_code_variant_appears_in_a_rule_file
- [x] Item 2 — Bounded chunked-backpressure helper module + tests
  - Files: crates/app/src/wal_reinject.rs, crates/app/src/lib.rs
  - Tests: test_million_frames_all_delivered_zero_drops, test_receiver_dropped_before_run_aborts_immediately, test_consumer_dropped_mid_replay_aborts_cleanly, test_wedged_consumer_times_out_and_aborts, test_reinject_wal_frames_empty_vec_is_clean_noop, test_chunk_boundary_exactness, test_zero_chunk_size_is_clamped, test_live_frames_interleave_with_large_replay
- [x] Item 3 — Replace both STAGE-C.2b call sites (fast boot + start_dhan_lane mirror) with the helper
  - Files: crates/app/src/main.rs
  - Tests: ratchet_main_rs_uses_bounded_reinject_helper
- [x] Item 4 — Plan file + verification runs (fmt, clippy, scoped tests, workspace check)
  - Files: .claude/plans/active-plan-c3-reinjection-storm.md
  - Tests: cargo test -p tickvault-common, cargo test -p tickvault-app, error_code_rule_file_crossref, error_code_tag_guard

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot with 1.26M-frame WAL, healthy tick processor | ALL frames delivered (backpressured, chunk-yielded), clean == true, `confirm_replayed()` archives segments — storm loop broken |
| 2 | Boot replay, tick processor dies mid-replay | Abort with `reason="channel_closed"`, WS-REINJECT-01 paged, remainder stays staged, next boot re-replays |
| 3 | Boot replay, tick processor wedged 30s+ | Abort with `reason="send_timeout"`, same fail-closed staging |
| 4 | Live frames arrive during a huge replay | Interleaved, not starved (per-chunk yield + mpsc waiter fairness) |
| 5 | Empty WAL | No-op (pre-existing gate), clean path |
| 6 | Future edit reverts to try_send | Build fails via the source-scan ratchet |

## Guarantee Matrices (from `.claude/rules/project/per-wave-guarantee-matrix.md`)

### 15-row 100% Guarantee Matrix

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` 100% min per crate; `scripts/coverage-gate.sh` | post-merge llvm-cov report | item PR includes coverage delta |
| 100% audit coverage | `<event>_audit` table per typed event with DEDUP UPSERT KEYS | `mcp__tickvault-logs__questdb_sql` | N/A — cold boot-recovery path; forensic record is the WAL `replaying/` staging + errors.jsonl typed event (no new QuestDB event class) |
| 100% testing coverage | 22 test categories per `testing.md` (unit/integration/property/loom/dhat/fuzz/mutation/sanitizer/coverage/etc.) | `cargo test --workspace` green | unit + integration-shaped concurrency tests + source-scan ratchet in this PR |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 8 pre-commit gates | pre-push mandatory | all gates green |
| 100% code performance | DHAT zero-alloc + Criterion p99 budgets + bench-gate ≤5% regression | `cargo bench` + `scripts/bench-gate.sh` | N/A — cold path (per-tick hot path untouched at `connection.rs`); helper is O(1)/frame, move-only |
| 100% monitoring | 7-layer telemetry (Prom counter + gauge + tracing span + Loki log + Telegram event + Grafana panel + audit table) | `mcp__tickvault-logs__run_doctor` | 4 counters + typed error! (Telegram via 5-sink) + runbook |
| 100% logging | tracing macros mandatory (no println!/dbg!); ERROR → Telegram | hourly errors.jsonl rotation | abort path uses `error!` with `code =` field |
| 100% alerting | `alerts.yml` Prom rule + `resilience_sla_alert_guard.rs` ratchet | `mcp__tickvault-logs__run_doctor` (CloudWatch alarms) | typed WS-REINJECT-01 routes through the existing error→Telegram chain; CloudWatch alarm on the abort counter is a follow-up |
| 100% security | banned-pattern + secret-scan + `Secret<T>` + security-reviewer agent | `cargo audit` post-deploy | no secrets, no new I/O surface; frame bytes never logged |
| 100% security hardening | static IP enforcement + pre-commit secret scan + `unused_must_use` lint | post-deploy IP verify | attack-surface delta: none (internal channel plumbing) |
| 100% bugs fixing | adversarial 3-agent review (proven 4-bug catch rate) | pre-PR + post-impl agent pass | worker-task scope: self-review + ratchet tests; 3-agent pass at PR stage |
| 100% scenarios covering | 9-box + chaos test for new failure mode | chaos suite | 6-scenario table above; wedged/dead/interleave covered by tests |
| 100% functionalities covering | every pub fn has call site + test (`pub-fn-wiring-guard.sh` + `pub-fn-test-guard.sh`) | pre-push gates 6+11 | `reinject_wal_frames` has 2 prod call sites + 9 tests |
| 100% code review | adversarial 3-agent on diff before AND after impl | per-PR | at PR stage per pr-completion-protocol |
| 100% extreme check | all of above + ratchet tests fail build on regression | every commit | `ratchet_main_rs_uses_bounded_reinject_helper` + error-code ratchet suite |

### 7-row Resilience Demand Matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | Bounded zero loss inside chaos envelope: ring 2M → spill NDJSON → DLQ | this fix REMOVES a drop path: replay frames are backpressure-delivered or stay durably staged in the WAL; never dropped while the channel is open |
| WS never disconnects | DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close | untouched — no WS lifecycle change |
| Never slow/locked/hanged | DHAT ≤4 alloc blocks/8KB across 10K calls; Criterion p99 ≤100ns; tick-gap >30s Telegram; core_affinity Core 0 | per-chunk `yield_now()` prevents runtime monopoly; per-send 30s timeout prevents a wedged-forever boot; liveness test pins interleaving |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal | untouched; duplicate replay absorbed by DEDUP keys |
| O(1) latency | `from_le_bytes` + `papaya` + `Arc<HashMap>` + SPSC bounded; bench-gate ≤5% regression | helper is O(1) per frame (move, no clone, no alloc, no format!); hot path untouched |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS + meta-guard | untouched — re-delivered frames dedup via STORAGE-GAP-01 keys |
| Real-time proof | 7-layer telemetry + SLO-01/SLO-02 @ 10s + market-open self-test @ 09:16:30 IST | abort counters + chunk progress counter + typed error! + confirmed-segments counter as the storm-broken signal |

## Honest envelope claim

100% inside the tested envelope, with ratcheted regression coverage: a
1,000,000-frame WAL replay (>7× `FRAME_CHANNEL_CAPACITY = 131_072`) is
delivered with zero drops via backpressured send + 8,192-frame chunk yields
(unit-proven); a dead or ≥30s-wedged consumer aborts fail-closed with the
undelivered remainder durably staged in the WAL `replaying/` directory for
re-replay next boot (constant `WAL_REINJECT_SEND_TIMEOUT_SECS`, ratcheted by
`wal_reinject::tests` + `ratchet_main_rs_uses_bounded_reinject_helper`).
Beyond the envelope, the WAL frame-spill floor (`ws_frame_spill.rs`) keeps
every unconfirmed segment on disk as recoverable bytes — no claim is made
that the consumer can never die, only that its death can no longer cause a
silent drop or an archive of undelivered frames.
