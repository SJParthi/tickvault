# Implementation Plan: QuestDB WAL-suspended table probe (WAL-SUSPEND-01) — W2 PR #6

**Status:** VERIFIED
**Date:** 2026-07-10
**Approved by:** Parthiban (operator) — 2026-07-09 audit follow-up row 10, coordinator-approved WAVE-2 PR #6 brief (this session)

## Design

QuestDB tables can enter **WAL-suspended** state (disk-full episode, apply
error): ILP ingestion keeps ACKing rows into the table's WAL, but WAL APPLY is
suspended — rows silently stop becoming visible/durable-applied. Nothing in
tickvault probes for this today: the boot probe and `make doctor` check
reachability (`SELECT 1`), and `questdb_health.rs` tracks the ILP writer's
CONNECTION state — none of them see per-table WAL apply health. A suspended
`ticks` or `candles_1m` table = silent data-visibility loss until someone
manually notices.

The fix mirrors the process-global supervised-monitor house family
(`disk_health_watcher.rs` / `oom_monitor.rs` / `resource_monitor.rs`):

1. **New module `crates/storage/src/wal_suspension_watcher.rs`:**
   - Pure parse core `parse_wal_tables_response(&serde_json::Value)` —
     resolves column indices BY NAME from the `/exec` JSON `columns` header
     (`name`, `suspended` mandatory; `writerTxn`, `sequencerTxn`, `errorTag`,
     `errorMessage` optional diagnostics), NEVER by position. Column set
     verified against upstream QuestDB source
     (`WalTableListFunctionFactory.java`: name STRING, suspended BOOLEAN,
     writerTxn LONG, bufferedTxnSize LONG, sequencerTxn LONG, errorTag STRING,
     errorMessage STRING, memoryPressure INT). The live 9.3.5 response shape
     is NOT live-verified from this sandbox — defensive by-name parsing +
     the fail-soft ProbeFailed path cover any drift honestly.
   - Pure edge-latch state machine `WalSuspensionTracker::observe(rows) ->
     WalSuspensionDelta { newly_suspended, recovered, currently_suspended }` —
     per-table episodes: ONE rising edge per (table, suspension episode),
     falling edge on recovery/disappearance (audit-findings Rule 4).
   - Supervised 60s poll task `spawn_wal_suspension_watcher(QuestDbConfig)` +
     `spawn_supervised_wal_suspension_watcher(...)` (respawn counter
     `tv_wal_suspension_watcher_respawn_total{reason}`, reuses
     `disk_health_watcher::classify_join_exit`, 5s backoff — the
     resource_monitor supervisor shape). Probe issues
     `select * from wal_tables()` via the SHARED `http_client::
     shared_probe_client()` (HTTP-CLIENT-01 contract: build failure = typed
     degrade + `tv_http_client_build_failed_total{site="wal_suspension_probe"}`,
     skip the tick, never panic).
2. **Suspension is only ever asserted from a SUCCESSFUL 2xx response whose
   parsed rows show `suspended=true`** — an unreachable/erroring QuestDB is
   BOOT-01/02 + `tv_questdb_connected` territory and increments only
   `tv_wal_suspension_probe_failed_total{reason}` at debug level (no
   double-page of a down DB as "suspended").
3. **New ErrorCode `WalSuspend01TableSuspended`** (`code_str
   "WAL-SUSPEND-01"`, Severity::High, `is_auto_triage_safe() == false`
   severity-independent override — the recovery action `ALTER TABLE <t>
   RESUME WAL` is an OPERATOR decision; auto-resume can replay into a
   still-broken disk). STORAGE-GAP-05 was NOT used — wave-4-error-codes.md
   reserves it for the disk-full pre-flight.
4. **Paging (all three surfaces in lockstep, per the #1468 drift ratchet):**
   `error_code_alerts` map entry `"wal-suspend-01"` in
   `deploy/aws/terraform/error-code-alarms.tf` (coded JSON filter, period 300,
   threshold 1, eval 3/dta 1, `ok_recovery = false` — the code fires once per
   suspension episode, so the auto-OK ~15 min later would be a Rule-11 false
   recovery; the real recovery signal is the falling-edge info! + the gauge
   returning to 0) + the observability-architecture.md "Which codes page"
   list + the new runbook rule file
   `.claude/rules/project/wal-suspension-error-codes.md` (triage documents
   `ALTER TABLE <t> RESUME WAL`, never auto-executed).
5. **Boot wiring:** ONE spawn call in `crates/app/src/main.rs` immediately
   next to the disk-health watcher in the PROCESS-GLOBAL supervised-monitor
   block (always-on like its siblings — WAL suspension at any hour stops data
   applying), + house wiring-guard
   `test_wal_suspension_watcher_is_wired_into_main` in
   `crates/core/src/auth/secret_manager.rs`.

Cold path only (one HTTP GET per 60s); zero hot-path impact; O(1) per poll in
allocations proportional to table count (~30 tables — honestly O(N tables) per
poll, flagged, never claimed O(1)).

## Edge Cases

- **QuestDB legitimately DOWN:** HTTP send error / timeout → `ProbeFailed
  {reason="http"}` counter + debug! only — BOOT-01/02 + `tv_questdb_connected`
  own that page; the latch is PRESERVED (no false recovery, no re-fire when
  the DB comes back still-suspended).
- **Non-2xx from a reachable server:** `reason="status"` counter + debug!.
- **Malformed JSON body:** `reason="parse"` — edge-latched warn! (once per
  contiguous failure run) so a server-version drift is visible without a
  60s-cadence log storm.
- **`columns` header missing `name`/`suspended`:** `reason="missing_column"`
  — same edge-latched warn! (schema drift on 9.3.5 → fail-soft, honest).
- **Malformed individual row (non-array / wrong cell types):** row skipped
  defensively; remaining rows still evaluated.
- **Table dropped while suspended:** falls out of `wal_tables()` → treated as
  a falling edge (info! "no longer reported"); honest, never a stuck episode.
- **Multiple tables suspend in one poll:** one error! per table (bounded by
  table count ~30); the CloudWatch alarm state machine pages once per ALARM
  transition regardless.
- **Flapping suspension (suspend→resume→suspend):** each new episode is a
  genuine rising edge and re-fires — correct (each is a real new incident);
  the alarm holds ALARM across ≤15-min gaps (eval 3/dta 1).
- **Watcher panic / respawn while a table is still suspended:** the fresh
  tracker re-fires ONE error per still-suspended table — bounded, documented
  (the supervisor also covers the gauge going stale on task death: the
  respawned watcher re-sets the gauge on its first successful probe).
- **Non-WAL tables:** never appear in `wal_tables()` — no false signal.
- **errorTag/errorMessage carry server text:** sanitized + truncated via
  `tickvault_common::sanitize::capture_rest_error_body` (≤300 chars,
  JWT/credential-redacted) before logging.

## Failure Modes

| Failure | Behaviour |
|---|---|
| shared_probe_client build fails (fd/TLS exhaustion) | HTTP-CLIENT-01 error! + `tv_http_client_build_failed_total{site="wal_suspension_probe"}`, tick skipped, next tick retries (OnceLock caches only success) |
| QuestDB unreachable | debug! + `tv_wal_suspension_probe_failed_total{reason="http"}`; no WAL-SUSPEND-01 (down ≠ suspended) |
| Response schema drift | fail-soft ProbeFailed(parse/missing_column) + edge-latched warn!; gauge holds last known value (documented staleness) |
| Watcher task dies (panic/cancel) | supervisor logs warn! + `tv_wal_suspension_watcher_respawn_total{reason}` + respawns after 5s (resource_monitor shape) |
| Genuine suspension | ONE error!(code = "WAL-SUSPEND-01", table, error_tag, error_message, writer_txn, sequencer_txn) per table per episode → errors.jsonl → CW filter → alarm ≤5 min → SNS → Telegram |
| Recovery (RESUME WAL applied / table catches up) | falling-edge info! + gauge decremented; no OK page by design (ok_recovery=false, once-per-episode emitter) |

## Test Plan

- `crates/storage/src/wal_suspension_watcher.rs` unit tests: parse happy path
  (by-name, shuffled column order), missing `suspended` column → Err, missing
  `columns` header → Err, malformed row skipped, boolean/long/string cell
  extraction, tracker rising edge once per episode, falling edge on
  recovery + on disappearance, latch preserved across identical polls,
  flapping re-fires, multi-table, constants sanity, supervisor
  keeps-running guard (house `test_spawn_supervised_*_keeps_running` shape).
  Tests: `test_parse_wal_tables_by_column_name_any_order`,
  `test_parse_missing_suspended_column_fails_soft`,
  `test_parse_missing_columns_header_fails_soft`,
  `test_parse_skips_malformed_rows`,
  `test_tracker_rising_edge_fires_once_per_episode`,
  `test_tracker_falling_edge_on_recovery_and_disappearance`,
  `test_tracker_flapping_refires_per_new_episode`,
  `test_poll_interval_and_backoff_sane`,
  `test_spawn_supervised_wal_suspension_watcher_keeps_running`.
- `crates/common/src/error_code.rs` existing 12-ratchet battery covers the new
  variant (unique code_str, roundtrip, runbook path exists, severity,
  auto-triage override, all() length, prefix pattern with `WAL-SUSPEND-`).
- `crates/common/tests/error_code_paging_filter_drift_guard.rs` (unchanged)
  now enforces the three-surface lockstep for the new entry.
- Wiring guard `test_wal_suspension_watcher_is_wired_into_main`
  (`crates/core/src/auth/secret_manager.rs`).
- Scoped: `cargo test -p tickvault-storage`, `-p tickvault-common`,
  `-p tickvault-core --lib` (guard test), `-p tickvault-app --lib` if touched;
  clippy -D warnings; fmt; banned-pattern scan; plan-verify.
- Honest live-unverified: the actual `wal_tables()` response shape on the
  QuestDB 9.3.5 image is not probeable from this sandbox — the by-name
  defensive parser + ProbeFailed fail-soft path cover the drift case; first
  live boot is the probe.

## Rollback

Single revert of this PR removes: the module, the lib.rs mod line, the one
main.rs spawn line, the ErrorCode variant, the tf map entry, the doc list
line, the runbook file, and the wiring guard — no data migration, no schema
change, no config change. The drift ratchet passes again after a full revert
(all three surfaces removed together). Partial rollback (e.g. tf entry only)
is BLOCKED by the #1468 drift ratchet — by design.

## Observability

- Gauge `tv_questdb_wal_suspended_tables` (count currently suspended; set on
  every successful probe including 0).
- Counter `tv_wal_suspension_probe_failed_total{reason}` (static labels:
  http / status / parse / missing_column).
- Counter `tv_wal_suspension_watcher_respawn_total{reason}` (supervisor).
- Counter `tv_http_client_build_failed_total{site="wal_suspension_probe"}`
  (HTTP-CLIENT-01 site table updated in lockstep).
- `error!(code = "WAL-SUSPEND-01", …)` → errors.jsonl → CloudWatch filter
  `tv-<env>-errcode-wal-suspend-01` → alarm → SNS → Telegram (≤~5 min).
- Falling-edge recovery info! + runbook
  `.claude/rules/project/wal-suspension-error-codes.md`.
- Cost: +1 log metric filter (free) + 1 alarm ≈ $0.10/mo — inside the $35/mo
  pre-GST budget ceiling.

## Plan Items

- [x] New module `wal_suspension_watcher.rs` (pure parse + tracker + task + supervisor + tests)
  - Files: crates/storage/src/wal_suspension_watcher.rs, crates/storage/src/lib.rs
  - Tests: test_parse_wal_tables_by_column_name_any_order, test_parse_missing_suspended_column_fails_soft, test_parse_missing_columns_header_fails_soft, test_parse_skips_malformed_rows, test_tracker_rising_edge_fires_once_per_episode, test_tracker_falling_edge_on_recovery_and_disappearance, test_tracker_flapping_refires_per_new_episode, test_poll_interval_and_backoff_sane, test_spawn_supervised_wal_suspension_watcher_keeps_running
- [x] ErrorCode variant WalSuspend01TableSuspended ("WAL-SUSPEND-01", High, manual-triage override)
  - Files: crates/common/src/error_code.rs
  - Tests: test_code_str_follows_expected_prefix_pattern (extended), existing error_code ratchet battery
- [x] Paging lockstep: tf map entry + doc list + runbook rule file + HTTP-CLIENT-01 site row
  - Files: deploy/aws/terraform/error-code-alarms.tf, .claude/rules/project/observability-architecture.md, .claude/rules/project/wal-suspension-error-codes.md, .claude/rules/project/http-client-error-codes.md
  - Tests: error_code_paging_filter_drift_guard (existing, now covers the new entry), error_code_rule_file_crossref (existing)
- [x] Boot wiring (one spawn next to the disk-health watcher) + wiring guard
  - Files: crates/app/src/main.rs, crates/core/src/auth/secret_manager.rs
  - Tests: test_wal_suspension_watcher_is_wired_into_main

## Per-Item Guarantee Matrix

This plan carries the 15-row + 7-row guarantee matrices by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical matrix);
the item-specific proofs are the Test Plan + Observability sections above.
Zero-loss row: this PR introduces NO new tick-drop path (pure cold-path
monitor); WS/SubscribeRxGuard untouched; no hot-path allocation (60s cold
poll); QuestDB schema self-heal untouched; composite-key uniqueness N/A (no
new table); real-time proof = the 7-layer chain in Observability.

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: a
WAL-suspended table reported by a REACHABLE QuestDB pages within ~5 minutes
via the errcode filter chain (once per suspension episode per table,
edge-latched, ratcheted by the paging-filter drift guard + the wiring guard);
probe/schema failures degrade fail-soft with counters, never a false
"suspended" page and never a page for a merely-DOWN QuestDB (BOOT-01/02 own
that). NOT claimed: detection of suspension during a QuestDB outage window
(the probe cannot query a down server — it catches up on the first successful
poll after recovery, ≤60s later); the live 9.3.5 `wal_tables()` response
shape (defensively parsed by column name; drift degrades to a loud
probe-failed signal, never a silent miss of a page the schema still permits).
