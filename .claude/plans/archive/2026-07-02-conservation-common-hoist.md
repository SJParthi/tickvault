# Implementation Plan: hoist the 15:40 conservation audit to the common boot path + de-duplicate post-market spawns

**Status:** VERIFIED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — standing directive this session ("deep research… confirm the entire works… cover all extreme permutations… fix") + 100%-audit-coverage charter. Closes the CRITICAL + 2 findings from the 2026-07-02 four-agent adversarial sweep.

## Design

Three confirmed findings, one subsystem:

1. **CRITICAL — Groww-only boot runs ZERO post-market tasks.** Both
   `spawn_post_market_tasks` call sites are Dhan-gated (`main.rs:2339` behind the
   fast-cache `dhan_enabled` filter; `main.rs:6754` inside `start_dhan_lane`,
   entered only when `config.feeds.dhan_enabled`). A `dhan_enabled=false +
   groww_enabled=true` session therefore never spawns the 15:40 conservation task —
   the Groww audit (which is runtime-gated INSIDE that task) never fires even
   though Groww streams all day. Silent audit-coverage hole.
2. **HIGH — runtime Dhan disable→enable duplicates the post-market family.**
   `run_dhan_lane_cold_start` → `start_dhan_lane` → `spawn_post_market_tasks`
   again; the spawned tasks are bare `tokio::spawn`s not owned by
   `DhanLaneRunHandles`, so teardown never aborts them. N enable cycles before
   15:40 ⇒ N conservation runs (N rows: `ts=run_ts` differs per task) + duplicate
   cross-verify/EOD/orphan tasks.
3. **MEDIUM — the Dhan audit's `db_rows` is not feed-filtered.**
   `tick_conservation_boot.rs` Dhan query is `select count() from ticks where ts…`
   with no `feed='dhan'` predicate (the Groww query IS filtered). On a both-feeds
   day the Dhan forensic row's `db_rows` includes Groww rows, permanently masking
   the runbook's "db_rows vs persisted ⇒ DEDUP collapse/WAL lag" triage.

**Fix (mirrors PR #1296 "spawn process-global monitors once in main(), not per
Dhan lane"):**

- **Hoist** the daily tick-conservation spawn block OUT of
  `spawn_post_market_tasks` into a new
  `spawn_daily_tick_conservation_task(config, trading_calendar, feed_runtime)`
  called ONCE from the common boot path in `main()` (the shared-infra section that
  runs regardless of which feeds are enabled). Inside the task, at 15:40, gate the
  Dhan run on `feed_runtime.is_enabled(Feed::Dhan)` — symmetric with the existing
  Groww gate — so a disabled lane writes no misleading zero row. Honest envelope:
  a feed toggled OFF at 15:40 after a full ON day skips its audit row that day
  (documented; threshold/gating refinement is on-box work).
- **Once-guard** `spawn_post_market_tasks` with a process-global
  `static POST_MARKET_TASKS_SPAWNED: AtomicBool` — the second and later calls
  log INFO and return. This kills the duplication for the remaining Dhan-scoped
  family (15:25 orphan watchdog, 15:31 cross-verify, EOD digest) across runtime
  cold-start cycles while preserving boot-symmetry (whichever path runs first
  wins).
- **Feed-filter** the Dhan `db_rows` query with `feed='dhan'`
  (`CONSERVATION_FEED_DHAN`), extracting BOTH count queries into pure
  `build_conservation_ticks_count_sql(feed, target_ist_day)` so the SQL is
  unit-testable.
- **Timeout** the Groww audit's reqwest client (10s total, mirroring the Dhan
  run's client) — the current `Client::new()` has no total-request timeout.
- **Comment honesty:** fix the `tick_conservation_boot.rs` comment that claims the
  bridge re-tails "from the persisted byte-offset" — the offset is in-memory only
  (rotation/offset persistence is a separate planned PR).

Out of scope (separate follow-ups, tracked): supervising the Groww bridge task,
parse-time (not persist-time) stall-watchdog liveness, NDJSON rotation + offset
persistence, sync-ILP-connect timeout in `TickConservationAuditWriter`.

## Edge Cases
- Groww-only boot → conservation task now spawns (common path), Dhan run skipped
  by its gate, Groww run fires. Dhan-only boot → inverse. Both → both, Dhan first.
- Both feeds OFF at 15:40 (toggled off intraday) → task fires, both runs skipped,
  one INFO line, no rows — honest (no false zero-balanced rows).
- Runtime Dhan enable AFTER boot (cold start) → `spawn_post_market_tasks` is
  called again → once-guard no-ops → exactly one task family alive.
- Fast-boot path and standard path both reach the common spawn exactly once (it
  lives before the per-feed dispatch).
- Non-trading day / past-15:40 boot → unchanged `decide_conservation_start` logic.

## Failure Modes
- If the hoisted spawn site were accidentally removed → updated source-scan
  ratchet (`tick_conservation_wiring_guard.rs`) fails the build.
- If someone re-adds a conservation spawn inside `spawn_post_market_tasks` →
  ratchet asserts the conservation call does NOT appear after the
  `fn spawn_post_market_tasks(` definition point.
- Once-guard swallowing the FIRST call is impossible: `swap(true)` returns prior
  value; only `false→true` proceeds.
- QuestDB wedge during the Dhan run can still delay the Groww run (sync ILP flush
  has no timeout — known, deferred); the Groww HTTP leg itself is now bounded.

## Test Plan
- Unit: `build_conservation_ticks_count_sql` — Dhan SQL contains `feed = 'dhan'`,
  Groww SQL contains `feed = 'groww'`, day window bounds exact.
- Ratchet (`crates/app/tests/tick_conservation_wiring_guard.rs`, updated):
  (a) conservation spawn exists in main.rs common path and NOT inside
  `spawn_post_market_tasks`; (b) both feed gates present
  (`is_enabled(tickvault_common::feed::Feed::Dhan)` + `…Feed::Groww)`);
  (c) `spawn_post_market_tasks` contains the `POST_MARKET_TASKS_SPAWNED`
  once-guard; (d) Groww run still sequenced after the Dhan run.
- Existing 15 unit + 3 wiring tests stay green; `cargo build -p tickvault-app`;
  scoped `cargo test -p tickvault-app --lib tick_conservation_boot` +
  `--test tick_conservation_wiring_guard`.

## Rollback
`git revert` — the hoist is a code-motion + gate + guard; no schema change, no
config change. Reverting restores the (broken) Dhan-gated wiring; the audit table
is untouched.

## Observability
- Unchanged counters (`tv_tick_conservation_audit_runs_total{outcome, feed}`) now
  actually fire in Groww-only sessions.
- Once-guard skip logs `info!("post-market tasks already spawned — skipping duplicate")`.
- Dhan forensic row `db_rows` becomes feed-accurate on both-feeds days (runbook
  triage step 5 valid again).

## Plan Items
- [x] Extract `build_conservation_ticks_count_sql` + feed-filter the Dhan query + Groww client timeout + comment fix
  - Files: crates/app/src/tick_conservation_boot.rs
  - Tests: test_build_conservation_ticks_count_sql_feed_filtered
- [x] Hoist conservation spawn to common boot path as `spawn_daily_tick_conservation_task`; Dhan-run gate; remove from `spawn_post_market_tasks`
  - Files: crates/app/src/main.rs
  - Tests: test_conservation_spawn_lives_in_common_path_not_post_market
- [x] Once-guard `spawn_post_market_tasks` (process-global AtomicBool)
  - Files: crates/app/src/main.rs
  - Tests: test_post_market_tasks_has_process_global_once_guard
- [x] Update wiring ratchet for the new topology
  - Files: crates/app/tests/tick_conservation_wiring_guard.rs
  - Tests: test_dhan_tick_conservation_audit_is_wired_into_main, test_groww_tick_conservation_audit_is_wired_into_main, test_both_feed_gates_present_and_groww_after_dhan

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Groww-only boot, trading day | conservation task spawns; 15:40 → Groww row written, Dhan run skipped |
| 2 | Dhan-only boot | Dhan row written (feed-filtered db_rows), Groww run skipped |
| 3 | Both feeds, Dhan toggled off→on twice before 15:40 | exactly ONE conservation run; one row per feed |
| 4 | Both feeds on, both-feeds day | Dhan db_rows counts ONLY feed='dhan' rows |
| 5 | Both feeds off at 15:40 | task fires, both runs skipped, INFO only |
