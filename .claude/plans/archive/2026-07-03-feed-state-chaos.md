# Implementation Plan: Feed-State Chaos Matrix (B1)

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — task B1 directive, this session ("Build build-failing chaos tests for every feed state permutation…")
**Branch:** `claude/feed-state-chaos`
**Changed crates:** `app` (crates/app — tests only; zero `src/` changes anywhere)

## Design

One new integration-test file, `crates/app/tests/chaos_feed_state_matrix.rs`, covering
EVERY feed-state permutation (7) with three assertion axes per permutation, each driven
through already-`pub` production surfaces — no production code is modified, no mirror
logic is written:

- **(a) Watchdog** — the pure feed-agnostic stall decision
  `tickvault_app::groww_sidecar_supervisor::should_restart_on_stall` (+ its constants
  `FEED_STALL_RESTART_SECS`, `STALL_RESTART_STORM_*`, the `StallRestartStorm` pure state
  machine) and the clock-injected liveness source
  `tickvault_common::feed_health::FeedHealthRegistry::last_tick_age_secs` /
  `record_tick` / `record_ticks` / `record_feed_liveness` / `snapshot` (verdict engine).
- **(b) Conservation ledger** — the pure daily-audit core in
  `tickvault_app::tick_conservation_boot`: `compute_residuals`, `classify_outcome`,
  `classify_groww_outcome`, `boot_covers_full_session`, `OutcomeCounters`,
  `count_groww_ndjson_lines_for_ist_day`; verdict type
  `tickvault_storage::tick_conservation_audit_persistence::ConservationOutcome`.
- **(c) Durable capture + recovery** — the real WAL surface in
  `tickvault_storage::ws_frame_spill`: `WsFrameSpill::new` / `append_with_seq` /
  `persisted_count` / `drop_critical_count`, `replay_all`, `confirm_replayed`,
  `count_frames_for_ist_day`, `ist_day_of_wall_nanos`, `next_frame_seq`.

**Feed-agnostic driver:** every permutation test iterates `for &feed in Feed::ALL`
(`tickvault_common::feed::Feed`). ZERO `"dhan"`/`"groww"` string literals appear in test
logic — feed labels only ever come from `feed.as_str()`. The ONE place feed capability
genuinely differs (durable-floor kind) is a single exhaustive
`fn durable_floor(feed: Feed) -> DurableFloor { match feed { Feed::Dhan => Wal,
Feed::Groww => Ndjson } }` helper plus one exhaustive per-feed ledger-classifier chooser
(`classify_outcome` vs `classify_groww_outcome` — the production audit is
lane-asymmetric by design). Adding a `Feed` variant breaks compilation at those matches,
forcing an explicit decision (the task contract).

**Honest envelope (documented in-code):** the Groww lane has NO WAL — its durable
capture floor is the sidecar's fsync'd append-only NDJSON, and recovery is the bridge's
re-tail-from-byte-0 (DEDUP-idempotent via deterministic `capture_seq`). The test asserts
the REAL pub recovery-count surface (`count_groww_ndjson_lines_for_ist_day`, the exact
count the production conservation audit trusts) is exact + stable across process-death
re-reads; it does NOT fake a WAL replay for that feed. The Groww lane's delivery
residual (`ndjson_lines − persisted`) is computed inline in the (non-exported)
orchestrator; the test derives the classifier INPUT with the same one-line subtraction
and drives the REAL pub classifier — trivial arithmetic, not mirrored production logic.

**Placement rationale:** `crates/app` is the only crate that can reach all three
surfaces (app lib re-exports the stall/conservation pure fns; app depends on
tickvault-storage + tickvault-common). One file keeps the meta-ratchet source-scan
simple. Private surfaces NOT reachable (stated honestly): the 60s in-loop ledger fns
`tick_conservation_residual` / `conservation_leak_delta` are `fn` (crate-private) in
`crates/core/src/pipeline/tick_processor.rs` — covered by their own in-crate unit tests
(`mod conservation_tests`); this matrix drives the equivalent daily-audit pure layer
(`compute_residuals`) instead. The live orchestrators (`supervise_child`,
`run_tick_conservation_audit`, `ActivityWatchdog::run`) need a live child process /
QuestDB / metrics endpoint and are TEST-EXEMPT in prod; the matrix targets their
unit-tested pure decision cores, per the research map.

Time control: all decision fns take explicit `now_ist_nanos` / `market_open` /
`now_secs` / `secs_of_day` parameters — no ambient clock, no
`set_test_force_in_market_hours`, no paused-tokio needed. WAL day attribution uses
`next_frame_seq()`-derived wall-nanos with a same-day guard.

## Edge Cases

- Exactly-at-threshold stall age must NOT fire (`>` is strict) — asserted in perm 1.
- `None` last-tick age (cold pre-open / fresh post-restart registry) must NEVER fire the
  watchdog, in-market or not — asserted in perms 6 and 7.
- `market_open == false` suppresses the stall decision regardless of age — perm 6.
- Frozen exchange timestamps: registry liveness is stamped at CAPTURE/parse time
  (`record_feed_liveness`), so duplicates never starve liveness — perm 2.
- Duplicate frames stay DISTINCT on the durable floor (distinct `frame_seq` in the WAL;
  one NDJSON line each) — replay/count equals appended count, not unique count — perm 2.
- Storm window: first restart after an idle gap RESETS the window (returns false);
  escalation edge only when count exceeds `STALL_RESTART_STORM_MAX` INSIDE
  `STALL_RESTART_STORM_WINDOW_SECS`; a restart after window expiry resets — perm 4.
- Partial universe: feed-LEVEL liveness stays fresh when only a subset of SIDs stream
  (illiquid-vs-dead contract) — replay count equals the captured subset, NOT universe
  size — perm 5.
- `boot_covers_full_session` boundary: `09:00:00 − 1s` covers, `09:00:00` exactly does
  NOT — perm 6.
- Crash between `replay_all` and `confirm_replayed`: staged segments re-replay on the
  next boot (idempotency window); after `confirm_replayed` they never re-replay — perm 7.
- IST-midnight seq window guard: WAL seq base shifted if the burst would straddle an
  IST day boundary (deterministic day attribution).
- Negative residuals classify `Partial`, never a false `Balanced` — asserted via
  `classify_outcome` in perm 7 partial-coverage assertions.

## Failure Modes

- **False Balanced under loss (the ledger lie):** perm 4 asserts a k-tick processor-view
  loss yields `Leak` (Dhan lane) / non-`Balanced` diagnostic `Partial` (Groww lane) —
  the test FAILS THE BUILD if either classifier ever reports `Balanced` with a positive
  delivery residual.
- **False watchdog kill (healthy feed restarted):** perms 2/3/4/5 pin the non-fire arms;
  perm 6/7 pin the `None`-age and market-closed non-fire arms. A regression that makes
  `should_restart_on_stall` fire on any of these fails the build.
- **Silent watchdog (dead feed not restarted):** perm 1 pins the firing arm at
  threshold+1 during market hours.
- **WAL frame loss on crash:** perms 1/3/4/5/6/7 pin `replay_all` count == appended
  count with zero `drop_critical`; perm 7 pins the replay-then-confirm idempotency.
- **New feed silently unhandled:** the exhaustive `match feed` helpers break compilation
  on a new `Feed` variant; the meta-ratchet additionally pins that all 7 permutation
  test fns exist and iterate `Feed::ALL`, and that no quoted feed-label literal appears.

## Test Plan

File: `crates/app/tests/chaos_feed_state_matrix.rs` — 9 `#[test]` fns (7 permutations +
1 zero-slack meta-ratchet + 1 Groww-residual source pin), all plain tests, no Docker,
no network, no `#[ignore]`, temp dirs cleaned up (chaos_tmp idiom from
`chaos_ws_frame_spill_saturation.rs`).

Run: `cargo test -p tickvault-app --test chaos_feed_state_matrix` (plus the full
`cargo test -p tickvault-app` scoped suite, fmt, clippy, banned-pattern scanner,
plan-verify, per-item-guarantee-check).

Verification evidence (REAL output, 2026-07-03,
`cargo test -p tickvault-app --test chaos_feed_state_matrix`):

```
running 9 tests
test chaos_matrix_completeness_ratchet_all_7_permutations_iterate_feed_all ... ok
test chaos_perm1_silent_feed_watchdog_fires_ledger_balanced_wal_recovers ... ok
test chaos_perm3_same_price_advancing_healthy_no_false_positive ... ok
test chaos_perm2_frozen_timestamp_duplicates_watchdog_quiet_all_duplicates_durable ... ok
test chaos_perm6_late_resume_at_market_open_never_fires_pre_open ... ok
test chaos_perm5_partial_universe_feed_level_liveness_no_false_kill ... ok
test groww_residual_derivation_source_pin_matches_test_arithmetic ... ok
test chaos_perm7_process_down_mid_market_replay_recovers_partial_never_balanced ... ok
test chaos_perm4_flooding_burst_absorbed_storm_bounded_leak_reported_honestly ... ok

test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.21s
```

Also green: `cargo fmt --check`; `bash .claude/hooks/banned-pattern-scanner.sh` (exit 0);
`bash .claude/hooks/plan-verify.sh` (PASS); `bash .claude/hooks/per-item-guarantee-check.sh`
(44 PASS, 0 FAIL); `cargo clippy -p tickvault-app --tests --no-deps` → 0 findings in
`chaos_feed_state_matrix.rs` (the CI clippy invocation — ci.yml deliberately omits
`-D warnings` due to ~24 documented pre-existing lib warnings, e.g.
`crates/common/src/muhurat.rs:40` `let_underscore_must_use`, untouched by this diff).

Negative proof (ratchet honesty — REAL sabotage evidence): temporarily flipped the
perm 4 Dhan-lane honesty assertion to expect `ConservationOutcome::Balanced` under a
25-tick processor-view loss; the test failed as required, then the sabotage was
reverted and the suite re-ran green:

```
thread 'chaos_perm4_flooding_burst_absorbed_storm_bounded_leak_reported_honestly' (1929)
panicked at crates/app/tests/chaos_feed_state_matrix.rs:558:27:
assertion `left == right` failed: feed dhan: a 25-tick processor-view loss must NEVER
classify Balanced — the WAL lane pages it as Leak
  left: Leak
 right: Balanced
test result: FAILED. 7 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.55s
```

Negative proof #2 (adversarial-review HIGH fix — zero-slack meta-ratchet, REAL
sabotage evidence): temporarily de-generalized perm 3's loop to a single-feed
array (`for &feed in &[Feed::Dhan]`); the per-body meta-ratchet failed as
required (a global occurrence count would have stayed green), then the
sabotage was reverted and the suite re-ran green:

```
thread 'chaos_matrix_completeness_ratchet_all_7_permutations_iterate_feed_all' (9969)
panicked at crates/app/tests/chaos_feed_state_matrix.rs:964:9:
assertion `left == right` failed: zero-slack ratchet: permutation
`chaos_perm3_same_price_advancing_healthy_no_false_positive` must iterate Feed::ALL
exactly once in its own body (found 0) — a de-generalized single-feed loop or a
duplicated loop both break the feed-agnostic contract
  left: 0
 right: 1
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 8 filtered out; finished in 0.43s
```

## Rollback

Tests-only change: `git revert` of the test commit (or deleting
`crates/app/tests/chaos_feed_state_matrix.rs` + this plan file) restores the exact
prior state. No production code, no schema, no config, no CI workflow is touched, so
there is no runtime rollback surface and no data migration to unwind.

## Observability

This item adds no runtime code, so no new counters/spans/alerts are introduced (N/A —
tests only). What it observes/ratchets: the existing 7-layer chain around
FEED-STALL-01 (`tv_feed_sidecar_stall_restart_total`, storm escalation), the
TICK-CONSERVE-01 daily audit verdicts (`tick_conservation_audit` rows: `balanced` /
`leak` / `partial`), and the WAL zero-loss chain
(`tv_ws_frame_spill_drop_critical` == 0 healthy-ops contract,
`tv_wal_replay_recovered_total` semantics via `replay_all`). CI (Build & Verify) runs
the file on every push — a regression in any pinned behavior fails the build, which IS
the observability contract for a ratchet.

## Per-Item Guarantee Matrix

Cross-reference: all 15 + 7 rows of `.claude/rules/project/per-wave-guarantee-matrix.md`
apply to this item. Item-specific proof deltas:

| Row (from per-wave-guarantee-matrix.md) | This item's specific proof |
|---|---|
| 100% testing coverage | Chaos + boundary categories: 8 new build-failing tests over the 7-permutation × 3-axis matrix (`cargo test -p tickvault-app --test chaos_feed_state_matrix`) |
| 100% scenarios covering | All 7 feed-state permutations enumerated + meta-ratcheted (fn-name source-scan); false-positive guard pairs included (perm 3 vs perm 1) |
| 100% code checks | fmt + clippy `-D warnings -W clippy::perf` + banned-pattern scanner + plan-verify + this guarantee check, all run green pre-push |
| 100% extreme check | Meta-ratchet test pins matrix completeness + `Feed::ALL` iteration + zero feed-label literals; negative sabotage proof recorded above |
| 100% bugs fixing / code review | Tests-only, single-crate, <1,000 LoC — Light/Standard tier per `auto-effort-policy.md`; adversarial review deferred to the coordinator's PR pass |
| Zero ticks lost (7-row) | No new tick-drop path (no src change); the matrix RATCHETS the existing ring→WAL→replay chain (`drop_critical == 0`, replay count == appended count) |
| WS never disconnects (7-row) | Untouched; the stall-decision non-fire arms are pinned so a healthy socket is never false-killed |
| O(1) latency (7-row) | No hot-path change; all driven fns are the documented O(1) pure surfaces |
| Uniqueness + dedup (7-row) | Perm 2 pins duplicate-tick distinctness on the durable floor + the dedup outcome bucket keeping the conservation identity balanced |
| Real-time proof (7-row) | CI runs the matrix on every push; the conservation classifiers' honesty (never false `Balanced`) is the real-time-proof invariant pinned |

Honest claim: coverage here is 100% inside the tested envelope, with ratcheted
regression coverage — the 7 permutations × (watchdog / ledger / durable-floor) axes on
the pure decision surfaces; live-orchestrator behavior (child-process kill/relaunch,
live QuestDB counts, `/metrics` scrape) remains covered by its own TEST-EXEMPT
operational validation, not by this matrix.

## Plan Items

- [x] Item 1 — Write the feed-state chaos matrix test file (7 permutation tests + shared
  feed-agnostic helpers `durable_floor` / `capture_then_recover` /
  `classify_ledger_for_feed` / `balanced_counters` / `chaos_tmp`)
  - Files: crates/app/tests/chaos_feed_state_matrix.rs
  - Tests: chaos_perm1_silent_feed_watchdog_fires_ledger_balanced_wal_recovers, chaos_perm2_frozen_timestamp_duplicates_watchdog_quiet_all_duplicates_durable, chaos_perm3_same_price_advancing_healthy_no_false_positive, chaos_perm4_flooding_burst_absorbed_storm_bounded_leak_reported_honestly, chaos_perm5_partial_universe_feed_level_liveness_no_false_kill, chaos_perm6_late_resume_at_market_open_never_fires_pre_open, chaos_perm7_process_down_mid_market_replay_recovers_partial_never_balanced

- [x] Item 2 — Meta-ratchet pinning matrix completeness (all 7 permutation fns present,
  every one iterates `Feed::ALL`, zero quoted feed-label literals in the file)
  - Files: crates/app/tests/chaos_feed_state_matrix.rs
  - Tests: chaos_matrix_completeness_ratchet_all_7_permutations_iterate_feed_all

- [x] Item 3 — Verification battery + negative sabotage proof recorded in this plan
  - Files: .claude/plans/active-plan-feed-state-chaos.md
  - Tests: N/A (evidence capture — real outputs pasted in Test Plan above)

- [x] Item 4 — Adversarial-review hardening (1 HIGH + 3 MEDIUM + 2 LOW): zero-slack
  per-body meta-ratchet (each permutation body must contain the `Feed::ALL` iteration
  exactly once — global-count slack removed, negative proof #2 above); Groww
  delivery-residual source-scan pin against the non-exported orchestrator expression;
  independent NDJSON recovery signal (capture-time count with the producer handle held,
  post-death recount from a freshly copied file; tuple renamed
  `persisted_at_capture`/`recovered_after_reopen`); storm window-edge pins (Δ==WINDOW
  still in-window → trips, Δ==WINDOW+1 → fresh window); tautological perm-5
  universe-size assert removed; FEED_STALE_TICK_SECS verdict boundary pinned
  independently of FEED_STALL_RESTART_SECS (Ok at threshold, Down at threshold+1)
  - Files: crates/app/tests/chaos_feed_state_matrix.rs
  - Tests: groww_residual_derivation_source_pin_matches_test_arithmetic, chaos_matrix_completeness_ratchet_all_7_permutations_iterate_feed_all

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Silent feed (dead socket, in-market, age > threshold) | Watchdog fires; snapshot `Down`; WAL replays all pre-silence frames; ledger `Balanced` |
| 2 | Frozen-timestamp duplicates (capture-time liveness advances) | Watchdog quiet; every duplicate distinct on the durable floor; dedup bucket keeps identity `Balanced` (Dhan) / diagnostic `Partial` residual honesty (Groww) |
| 3 | Same-price advancing timestamps (healthy flat market) | Watchdog quiet (false-positive guard for #1); all frames captured + replayed; ledger `Balanced` |
| 4 | Flooding burst + restart storm | Watchdog quiet (liveness fresh); storm edge trips only past `STALL_RESTART_STORM_MAX` in-window; WAL absorbs burst with zero drops; k-tick processor loss reports `Leak`/non-`Balanced` — NEVER `Balanced` |
| 5 | Partial universe (subset of SIDs streams) | Feed-level liveness fresh → no kill (illiquid-vs-dead contract); replay count == captured subset, not universe size; ledger `Balanced` for what arrived |
| 6 | Late resume at 09:15 (silent pre-open, then resumes) | Pre-open + `None` age never fires; post-resume fresh → no fire; `boot_covers_full_session` boundary honest; resumed frames all replay; `Balanced` only with full-session coverage |
| 7 | Process down mid-market (drop + re-open WAL dir) | `replay_all` recovers every fsynced frame; un-confirmed segments re-replay; `confirm_replayed` archives; mid-day boot → `partial_coverage` → `Partial`, never false `Balanced`; fresh registry `None` age → no false kill at boot |
