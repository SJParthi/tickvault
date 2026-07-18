# Implementation Plan: Cleanup Wave — QuestDB Table Drops + Ensure-DDL Rewire (A) and Dashboard/Portal Tidy (B)

**Status:** APPROVED
**Date:** 2026-07-17
**Approved by:** Parthiban (operator, relayed via coordinator 2026-07-17 — tables cleanup + DDL-rewire + dashboard tidy ordered worked now, in parallel)

> Two parallel implementation PRs off origin/main share this ONE plan
> (committed byte-identical on both branches so the double-merge is clean):
> **Track A** = QuestDB retired-table drops + retired-tables-sweep re-wire +
> dormant ensure-DDL re-wire (crates `tickvault-storage`, `tickvault-app`,
> `scripts/questdb-init.sh`). **Track B** = dashboard tidy (CloudWatch
> dashboards/alarms terraform, the dead Dhan-lag chain in `tickvault-core`,
> operator-portal/API pages in `tickvault-api`, guard tests). Shared-crate
> touches (`tickvault-common`, `tickvault-core`) are named per item below.

## Plan Items

### Track A — tables cleanup + DDL rewire

- [x] A1 (TOP PRIORITY — correctness): re-wire the DORMANT ensure-DDL fns so a
  fresh-volume boot re-creates every live table WITH `DEDUP ENABLE UPSERT
  KEYS` (the fresh-volume no-DEDUP duplicate-row window fix). Every live
  table's idempotent `CREATE TABLE IF NOT EXISTS` + `ALTER ADD COLUMN IF NOT
  EXISTS` + `DEDUP ENABLE` ensure fn must have a boot call site again.
  - Files: crates/storage/src/shadow_persistence.rs,
    crates/storage/src/instrument_persistence.rs (and sibling
    `*_persistence.rs` ensure fns found dormant), crates/app/src/main.rs
    (boot Step 6b DDL join), crates/app/src/dhan_rest_stack.rs
  - Tests: ensure_ddl_boot_wiring_guard (new, crates/app/tests — source-scan
    that every live ensure fn has a production call site), existing
    per-module ensure/DDL unit tests in tickvault-storage
  - Impl: crates/app/src/candle_ddl_boot.rs (run_candle_ddl_at_boot, awaited
    in build_shared_infra before spawn_seal_writer_loop — main.rs) +
    crates/app/tests/ensure_ddl_boot_wiring_guard.rs (coverage table)
- [x] A2: re-wire the `RETIRED_QUESTDB_TABLES` drop sweep
  (shadow_persistence.rs:369, currently `[&str; 14]`) so it runs again on
  boot for the NEW drop set, and VERSION the one-shot marker (a bumped
  marker version re-arms the sweep exactly once per version — the
  drop-legacy marker precedent).
  - Files: crates/storage/src/shadow_persistence.rs, crates/app/src/main.rs
  - Tests: retired_tables_sweep unit tests in shadow_persistence.rs
    (marker-version re-arm + idempotent second boot), scoped
    `cargo test -p tickvault-storage`
  - Impl: shadow_persistence.rs LEGACY_DROP_SWEEP_VERSION=2 +
    marker_records_current_version + boot call via candle_ddl_boot
- [x] A3: extend the drop set with `historical_candles` (no live writer since
  the 2026-05-26 Dhan historical chain deletion; grep-verify remaining
  references in crates/common/src/constants.rs, crates/core, crates/app are
  comment/history-only or removed in the same PR) + the on-box-enumerated
  dead `movers_*` materialized views (movers runtime deleted 2026-05-19,
  AWS-lifecycle PRs #2-#4).
  - Files: crates/storage/src/shadow_persistence.rs (RETIRED_QUESTDB_TABLES
    list), crates/common/src/constants.rs (retired table-name consts, if
    any remain), crates/storage/src/materialized_views.rs
  - Tests: retired-tables list ratchet (table names pinned; sweep never
    names a LIVE table — a build-failing intersection check against the
    ensure-DDL set), scoped `cargo test -p tickvault-storage`
  - Impl: RETIRED_QUESTDB_TABLES 14→18 (historical_candles, stock_movers,
    option_movers, 2 movers marker tables) + retired_movers_object_names()
    53-name matview+table dual-drop grid (movers_1s moved into the grid)
- [x] A4: prune scripts/questdb-init.sh of the 13 retired table re-creates
  (14 `CREATE TABLE` statements today; only the live set survives) so a
  fresh box never resurrects a dropped table without DEDUP.
  - Files: scripts/questdb-init.sh
  - Tests: init-script/live-set consistency check (the pruned script's
    CREATE list ⊆ the ensure-DDL live set; source-scan test in
    crates/storage/tests)
  - Impl: scripts/questdb-init.sh pruned to ticks-only (final schema +
    5-key DEDUP) + crates/storage/tests/questdb_init_script_guard.rs

### Track B — dashboard tidy

- [x] B1: retire the `tv-${var.environment}-scoreboard` CloudWatch dashboard
  (deploy/aws/terraform/dashboard.tf:281 — free-tier slot 2; the dual-feed
  comparison concluded with the live-feed retirements) with a dated note.
  - Files: deploy/aws/terraform/dashboard.tf
  - Tests: dashboard guard test updates (crates/common/tests /
    crates/app/tests terraform-shape guards that pin dashboard widgets)
  - Impl: dashboard.tf scoreboard resource deleted + dated 2026-07-17
    retirement note (frees free-tier slot #2); aws_infra_wiring.rs
    operator-dashboard pin unchanged (all pinned metrics still charted —
    35/35 green)
- [x] B2: delete the dead Dhan-lag chain — `run_dhan_lag_publisher`
  (crates/core/src/pipeline/feed_lag_monitor.rs:626; its Dhan tick feeder
  died with the 2026-07-13 lane retirement + the #1631 stage-2 sweep) + the
  2 EMF allowlist entries (`tv_dhan_exchange_lag_p99_seconds`,
  `tv_dhan_lag_samples_excluded_total`) + the 2 alarms in
  deploy/aws/terraform/silent-feed-alarms.tf, with a dated COST NOTE in
  .claude/rules/project/aws-budget.md (−2 alarms ≈ −$0.20/mo, −2 EMF
  series ≈ −$0.60/mo — Assumed at CloudWatch list rates, the 2026-07-15
  precedent wording).
  - Files: crates/core/src/pipeline/feed_lag_monitor.rs,
    crates/app/src/main.rs (spawn sites),
    deploy/aws/terraform/silent-feed-alarms.tf,
    deploy/aws/terraform/metrics-log-metric-filters.tf (EMF allowlist),
    .claude/rules/project/aws-budget.md
  - Tests: crates/common/tests/metrics_catalog.rs update, alarm/EMF
    lockstep guard updates (crates/common/tests terraform wiring guards),
    scoped `cargo test -p tickvault-core -p tickvault-app`
  - Impl: feed_lag_monitor.rs ring/publisher half deleted (day-histogram
    scoreboard half KEPT — DailyLagHistogram/day_lag_summary; 4 tests
    green); benches/feed_lag_ring.rs + tests/dhat_feed_lag_ring.rs git
    rm'd + benchmark-budgets.toml key removed; EMF allowlist 19→17 in
    BOTH cloudwatch-agent.json + user-data.sh.tftpl (byte-identical,
    cw_agent_selector_lockstep_guard 4/4); silent-feed-alarms.tf S3
    dhan-lag alarm retired with dated note (DEVIATION: only ONE lag alarm
    existed — the Groww S4 twin was already retired 2026-07-15, so the
    delta is −1 alarm not −2); market-hours-liveness-alarm.tf window-gate
    ALARM_NAMES 4→3; aws-budget.md COST NOTE 2026-07-17 (−~$0.70/mo)
- [x] B3: remove dead widgets from the `tv-${var.environment}-operator`
  dashboard (dashboard.tf:27) whose metrics lost their producers in the
  live-WS retirements.
  - Files: deploy/aws/terraform/dashboard.tf
  - Tests: dashboard widget guard updates (same guard files as B1)
  - Impl: operator dashboard WebSocket-health widget (tv_websocket_* — WS
    pool deleted PR-C2/#1581) + spill/DLQ widget (tv_spill_*/tv_dlq_* —
    tick writer deleted #1631) removed with dated notes; every widget
    whose metric still has a live producer KEPT (per-widget verification
    in the PR body); the sibling app-alarms.tf dead-monitor alarms are
    deliberately NOT touched (flagged follow-up in the cost note)
- [x] B4: remove /board page dead sections §3-4 (live-feed-era panels with
  no producer).
  - Files: crates/api/src/handlers/board.rs,
    crates/api/src/handlers/board_page.rs
  - Tests: board page unit tests in tickvault-api (render + section
    assertions updated), scoped `cargo test -p tickvault-api`
  - Impl: board_page.rs sections 3 (board-conns) + 4 (board-race) HTML/JS
    removed with dated note; board.rs connections/race response fields +
    the whole file-view scan chain (groww_dir/scan_connections/
    newest_parity_race etc.) deleted; section-marker test now asserts the
    retired markers ABSENT; api suite green
- [x] B5: remove dead /dashboard status chips (retired-feed chips).
  - Files: crates/api/src/handlers/dashboard_page.rs
  - Tests: dashboard page unit tests in tickvault-api
  - Impl: NO-OP by verification (DEVIATION) — every subsystem chip on
    /dashboard maps to a LIVE health.rs verdict (websocket/order_update/
    questdb/token/pipeline/tick_persistence render real SubsystemInfo
    state); no dead chip exists, so no edit — recorded here per the
    unsure-=-KEEP rule
- [x] B6: remove dead /feeds toggles for retired lanes (both live-WS enables
  are 409-refused — the dead toggle UI goes; the read-only health view
  stays).
  - Files: crates/api/src/handlers/feeds.rs,
    crates/api/src/handlers/feeds_page.rs, crates/api/src/lib.rs (router,
    only if a route is dropped)
  - Tests: existing feeds handler tests updated (the 409-refusal tests are
    KEPT), scoped `cargo test -p tickvault-api`
  - Impl: KEEP + note (DEVIATION) — the toggle UI is generic over
    `Feed::ALL` with `is_runtime_toggleable()` from the protected
    crates/common/src/feed.rs (DO-NOT-TOUCH surface); the enable
    direction is already 409-refused by the retained retirement refusals
    whose tests the brief mandates KEEPING, and the disable direction
    remains a legitimate operator action — removing the buttons would
    orphan the kept 409 tests and require a common-crate edit; no route
    dropped, no edit
- [x] B7: update every guard test pinned to the pre-tidy shapes (terraform
  wiring guards, page/body guards, metrics catalog) in the SAME PR so All
  Green stays the choke point — never `continue-on-error`, never a weakened
  needle.
  - Files: crates/common/tests/*.rs (terraform + metrics guards),
    crates/app/tests/*.rs, crates/api/tests/*.rs (as matched)
  - Tests: the guards themselves (build-failing)
  - Impl: cloudwatch_app_alarms_wiring.rs EMF count 19→17 (16/16 green);
    cw_agent_selector_lockstep_guard.rs (4/4); metrics_catalog.rs lag
    entries removed (9/9); aws_infra_wiring.rs unchanged-and-green
    (35/35); board_page section-marker test updated (11 board tests
    green); observability-architecture.md paging list grep-verified — NO
    lag/scoreboard entry exists, so no rule-file paging edit owed

### Stage-4 — dead-producer observability sweep (2026-07-18)

- [x] S1: retire the 4 dead-tick alarm chains (tv_spill_dropped_total,
  tv_dlq_ticks_total, tv_ticks_dropped_total,
  tv_late_tick_after_boundary_total) — zero emit sites since the stage-2
  tick-chain deletion 2026-07-17; delete the 4 app-alarms.tf resources,
  remove the 4 names from BOTH EMF selector copies (byte-identical), empty
  the stage-2 dead-monitor allowlist, re-pin alarm count 9→5 + EMF 15→11,
  factual dated notes in guarantees.md / zero-tick-loss runbook /
  operator docs / aws-budget.md COST NOTE 2026-07-18.
  - Files: deploy/aws/terraform/app-alarms.tf, deploy/aws/cloudwatch-agent.json,
    deploy/aws/terraform/user-data.sh.tftpl,
    crates/common/tests/cloudwatch_app_alarms_wiring.rs,
    deploy/grafana-cloud/tickvault-operator-dashboard.json,
    docs/architecture/guarantees.md, docs/runbooks/zero-tick-loss.md,
    .claude/rules/project/aws-budget.md
  - Tests: test_every_alarm_metric_has_a_rust_emit_site,
    test_app_alarms_count_is_twenty_two (pin 5),
    test_emf_metric_selectors_name_count_is_pinned (pin 11),
    cw_agent_selector_lockstep_guard (4 tests)
- [x] S2: delete the producer-less feed-presence registry
  (feed_presence.rs + presence_registration.rs + dhat_feed_presence.rs +
  bench + budget + config flag + main.rs/scoreboard wiring); the
  scoreboard coverage path degrades honestly to its existing sql_backfill
  arm; ci.yml DHAT lane drift list + count updated.
  - Files: crates/core/src/pipeline/feed_presence.rs (delete),
    crates/core/src/instrument/presence_registration.rs (delete),
    crates/core/tests/dhat_feed_presence.rs (delete),
    crates/core/benches/feed_presence.rs (delete), crates/app/src/main.rs,
    crates/app/src/feed_scoreboard_boot.rs, crates/common/src/config.rs,
    config/base.toml, .github/workflows/ci.yml,
    quality/benchmark-budgets.toml,
    crates/core/src/auth/secret_manager.rs
  - Tests: feed_scoreboard_boot unit tests, bench_budget guard,
    cargo test -p tickvault-core --features dhat --no-run
- [ ] S3: MED-2 — delete the consumer-less TICK_BUFFER_CAPACITY orphan
  (constants.rs, minimal flagged edit) + zero_tick_loss_alert_guard.rs
  (its seal-ring rationale is factually false) +
  chaos_burst_indices_only.rs; re-point every honest-100% template line
  at the LIVE envelope (SEAL_BUFFER_CAPACITY 200_000, ratcheted by
  seal_ring.rs) and rewrite wave4_section8_wording_guard.rs in lockstep.
  - Files: crates/common/src/constants.rs,
    crates/storage/tests/zero_tick_loss_alert_guard.rs (delete),
    crates/storage/tests/chaos_burst_indices_only.rs (delete),
    crates/common/tests/wave4_section8_wording_guard.rs,
    .claude/rules/project/operator-charter-forever.md,
    .claude/rules/project/per-wave-guarantee-matrix.md,
    .claude/rules/project/wave-4-shared-preamble.md (+ sibling rule lines)
  - Tests: wave4_section8_wording_guard (rewritten),
    seal_ring.rs::test_seal_buffer_capacity_constant_is_locked_value,
    constants.rs unit tests

## Design

**Track A (tickvault-storage + tickvault-app + scripts).** The runtime is
REST-only (both live feeds retired); QuestDB carries retired tables and a
14-entry `RETIRED_QUESTDB_TABLES` drop sweep in
`crates/storage/src/shadow_persistence.rs` that already ran under a one-shot
marker. A1 is the correctness core: several ensure-DDL fns went dormant when
their boot call sites died with the deleted lanes — on a FRESH volume (the
pre-staged 20 GB terminate-and-recreate, `daily-universe-scope-expansion`
§7 Rule 3) the first ILP write would auto-create those tables WITHOUT
`DEDUP UPSERT KEYS` (the HTTP-CLIENT-01-class duplicate-row window, made
permanent). A1 re-wires every LIVE table's ensure fn into the boot DDL join
(main.rs Step 6b / dhan_rest_stack.rs) so DEDUP is guaranteed from first
boot. A2 versions the sweep marker so the NEW drop set (A3:
`historical_candles` + the dead `movers_*` matviews enumerated live on the
box) runs exactly once per marker version, idempotently. A4 prunes
`scripts/questdb-init.sh` (14 CREATE TABLE statements today) down to the
live set so init and ensure-DDL can never disagree. Retired tables are
DROPPED per the operator's cleanup order; SEBI never-delete tables
(`instrument_lifecycle`, audit tables) are NOT in the drop set — the
live/retired intersection ratchet makes that unrepresentable.

**Track B (tickvault-api + tickvault-core + terraform).** Post-retirement,
the `tv-scoreboard` dashboard, several operator-dashboard widgets, the
Dhan-lag publisher chain (`run_dhan_lag_publisher` in
`crates/core/src/pipeline/feed_lag_monitor.rs` + its 2 EMF series + 2 alarms
in silent-feed-alarms.tf), and the live-feed-era portal surfaces (/board
§3-4, /dashboard chips, /feeds dead toggles) are producer-less dead
monitors — exactly the dead-filter class the paging drift guard forbids.
Track B deletes them with dated notes at every lockstep surface
(terraform + aws-budget.md cost note + rule-file paging list where touched)
and updates the pinned guards in the same PR. No NEW alert, page, metric, or
route is added anywhere in either track.

## Edge Cases

- Fresh-volume boot with QuestDB slow: ensure-DDL runs behind the existing
  BOOT-01/BOOT-02 readiness probe; a failed ensure keeps the existing
  degrade semantics (coded error + retry next boot) — never a silent
  DEDUP-less table (A1's whole point).
- Sweep marker file corrupt/missing: treated as version-0 → sweep re-runs;
  `DROP TABLE IF EXISTS` is idempotent, so a double-run is a no-op.
- A retired table already absent on the box (e.g. dropped manually or by
  #1615): `IF EXISTS` no-ops; the sweep logs per-table outcome.
- `historical_candles` still referenced by comment/history text in
  crates/common / crates/core: only LIVE code paths (writers/readers) gate
  the drop; doc/comment mentions stay as history per house convention.
- WAL-suspended retired table at drop time: DROP fails → coded error +
  counted, next boot retries (never a boot block).
- Terraform apply ordering (B): dashboard/alarm deletions are pure resource
  removals — no state migration; the path-filtered terraform-apply lane +
  postmerge dispatcher cover a bot-merged change.
- /feeds page: the enable-refusal (409) handlers are KEPT — only the dead
  toggle UI is removed, so a scripted enable still gets the documented
  refusal, never a 404 surprise.
- Both branches commit this identical plan file: git content-merge is clean
  (byte-identical); the second merge is a no-op on this path.

## Failure Modes

- Ensure-DDL rewire misses a live table → the duplicate-row window persists
  on fresh volumes. Mitigation: the A1 wiring guard enumerates ensure fns vs
  call sites (build-failing); the day-1 DEDUP-verify checklist (CHAIN-03
  precedent) is the operator backstop.
- Sweep drops a LIVE table (worst case): prevented structurally by the A3
  ratchet asserting `RETIRED_QUESTDB_TABLES ∩ live-ensure set = ∅`; even if
  reached, ensure-DDL re-creates the table next boot (data loss bounded to
  the retired-classification error — the ratchet exists so this is
  unrepresentable).
- questdb-init.sh prune removes a live CREATE → fresh box relies on
  ensure-DDL (A1) which now covers every live table; the A4 consistency
  test pins script ⊆ ensure set.
- B2 deletes the lag publisher while some consumer still reads the gauges →
  prevented by grep-verifying zero remaining consumers (metrics_catalog +
  the alarm guards fail the build if a filter references a deleted series).
- Guard drift (B7): a terraform deletion without its guard update fails the
  build — that is the designed direction (fail-closed, never a silently
  weakened guard).
- Cross-track merge conflict: tracks touch disjoint files except this plan
  file (byte-identical) — no shared code file is edited by both.

## Test Plan

- Scoped suites per testing-scope.md: `cargo test -p tickvault-storage`
  + `-p tickvault-app` (Track A); `cargo test -p tickvault-api`
  + `-p tickvault-core` + `-p tickvault-app` (Track B). A
  `tickvault-common` touch (constants/metrics catalog) escalates to
  `cargo test --workspace` per the common-crate rule.
- New build-failing ratchets: ensure_ddl_boot_wiring_guard (A1),
  sweep marker-version re-arm tests + retired/live intersection ratchet
  (A2/A3), init-script consistency test (A4).
- Updated guards: metrics_catalog.rs, terraform alarm/EMF/dashboard wiring
  guards, board/dashboard/feeds page tests (B1-B7) — updated in the same
  PR as the shapes they pin.
- `cargo fmt --check` + `cargo clippy --workspace -- -D warnings` + the
  pre-push battery (banned-pattern, secret scan, test-count ratchet,
  plan-verify, guarantee-check) all green before push; All Green is the
  merge choke point on both PRs.

## Rollback

Each track lands as ONE squash-merge; a single `git revert` of either merge
commit restores that track completely (code + guards + terraform + this
plan's checkboxes). Track A data rollback: dropped tables are re-creatable
via the reverted ensure-DDL/init script (schema); dropped DATA is
recoverable only from the S3 cold archive for archived partitions — the
drop set is retired-and-dead-by-evidence precisely so no live data is at
stake. The sweep marker versioning means a revert-then-re-land re-arms
cleanly (bump the version again). Track B terraform rollback: revert +
`terraform apply` re-creates the dashboards/alarms (stateless resources);
the aws-budget cost note gets a dated correction rather than deletion, per
house convention.

## Observability

- Track A: per-table sweep outcomes logged with the existing coded
  error/counter pattern (drop failures loud, never silent); ensure-DDL
  failures keep their existing HTTP-CLIENT-01-class coded stages; the day-1
  DEDUP-verify SQL checklist is recorded in the PR body. No new ErrorCode
  variant is needed (existing stages cover every arm); if an implementer
  finds one is, it lands with its rule-file mention in the same PR per the
  cross-ref test.
- Track B: deletions carry dated notes at every lockstep surface
  (silent-feed-alarms.tf, metrics-log-metric-filters.tf, dashboard.tf,
  aws-budget.md COST NOTE, and observability-architecture.md if a paging
  entry is touched) so the paging drift guard sees tf ↔ doc ↔ emit in
  lockstep. `mcp__tickvault-logs__run_doctor` + the metrics catalog remain
  the post-merge verification surface; no new metric, alarm, or Telegram
  event is added by either track.

## Per-Item Guarantee Matrix

See `per-wave-guarantee-matrix.md` — the canonical 15-row 100% Guarantee
Matrix and the 7-row Resilience Demand Matrix both apply to every item in
this plan. Cleanup-wave mapping (honest, per row class):

- Coverage / testing / checks rows: proven by the scoped crate suites
  (storage / app / api / core green; common escalates to workspace) + the
  updated lockstep guards (metrics catalog, terraform wiring, page guards)
  + fmt/clippy clean — the only NEW logic is the ensure-DDL rewire and the
  sweep marker versioning, both ratchet-tested.
- Monitoring / logging / alerting rows: retirement-only on Track B — every
  dead dashboard/alarm/EMF entry is retired with dated notes at every
  lockstep surface; no new failure mode, so no new alert is owed. Track A's
  sweep/ensure failures reuse the existing coded stages.
- Resilience rows (Zero ticks lost, WS, QuestDB, O(1), dedup, real-time
  proof): no tick-drop path, hot-path file, or aggregator code is touched;
  the DEDUP row is STRENGTHENED (A1 closes the fresh-volume no-DEDUP
  window); dropped tables are dead-by-evidence with zero production
  writers/readers, so every other resilience envelope is byte-identical
  before/after.
- Rows genuinely inapplicable to a cleanup PR (new audit table, new bench,
  new scenario) are N/A per the z-plus-defense-doctrine "when Z+ conflicts
  with speed" table — recorded here, never silently skipped.
