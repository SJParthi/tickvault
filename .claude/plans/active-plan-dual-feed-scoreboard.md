# Implementation Plan: Dual-Feed Scoreboard PR-A — tables + blame classifier + process-death reconciler + 15:45 IST daily task + Telegram scorecard

**Status:** VERIFIED
**Date:** 2026-07-10
**Approved by:** Parthiban (operator) — 2026-07-10 dual-feed scoreboard directive ("run these two websockets live for a month... all tracked, captured, visualized, logged, monitored, 100% automated") + the blame-attribution directive ("ensure and CAPTURE that the issue really arose from the broker side")

> Per-item guarantee matrix: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row 100% matrix + the 7-row resilience matrix) — applies to every item in this plan. Contract source: the synthesized final design (scoreboard-final-design, PR-1 scope). This is PR-A of the 4-PR serial sequence; PR-2 (stall event kind), PR-3 (Groww lag ring + day histograms), PR-4 (presence registry + per-instrument unique-wins) follow serially.

## Design

The operator runs Dhan + Groww live for a month and needs a daily, durable,
blame-attributed scorecard. PR-A delivers the value-first core that works
day-1 from EXISTING tables (`ws_event_audit`, `ticks`) plus three new
forensic tables:

1. **Storage (`crates/storage`)** — template = `tick_conservation_audit_persistence.rs`
   (pure DDL fn + `ensure_*` self-heal + DEDUP UPSERT KEYS + lazy ILP writer;
   transport = ILP-over-HTTP per the 2026-07-05 ws_event_audit ACK lesson):
   - `feed_scoreboard_persistence.rs`: `feed_scoreboard_daily` (one row per
     `(trading_date_ist, feed)`; deterministic `ts` = trading-date 15:45:00
     IST nanos so re-runs UPSERT instead of duplicating; DEDUP
     `ts, trading_date_ist, feed`) + `feed_coverage_daily` (per-instrument
     detail, populated by PR-4; DEDUP
     `ts, trading_date_ist, security_id, exchange_segment, feed` — full
     I-P1-11 pair + feed).
   - `feed_episode_audit_persistence.rs`: one row per disconnect /
     off-hours-disconnect / stall-restart / never-streamed-restart /
     process-death episode with the PERSISTED blame verdict. DEDUP
     `ts, trading_date_ist, feed, ws_type, connection_index, episode_kind`
     (mirrors `DEDUP_KEY_WS_EVENT_AUDIT` → re-aggregation idempotent). The
     append signature takes `blame: BlameClass` — persisting without a blame
     class is a COMPILE error (no-blank-blame ratchet). `evidence` is bounded
     ≤200 chars and routed through the `capture_rest_error_body` redaction
     choke point.
2. **Blame classifier (`crates/common/src/feed_blame.rs`)** — pure, dep-free,
   TOTAL `classify_episode(&EpisodeEvidence) -> (BlameClass, &'static str)`.
   3-variant `BlameClass { Broker, Ours, Indeterminate }` (no Option;
   `as_str()` returns non-empty `&'static str`). Exhaustive arm table per the
   contract §4 (805/807/806+808-814/RST±WS-GAP-09/network/unknown/
   feed_toggle/stall slugs/resource-pressure/process-death deploy-vs-crash)
   with `_ => Indeterminate("unclassified")` as the fail-closed floor.
   Classification happens in ONE place (the aggregation/reconcile paths) —
   emit sites are untouched in PR-A.
3. **Process-death reconciler (`crates/app/src/feed_scoreboard_boot.rs`)** —
   a dying process writes NO disconnect row, so the BOOTING process is its
   own correlation evidence: once per boot (delayed ~3 min so this boot's
   `connected` rows land), query today's `ws_event_audit`; for each
   `(feed, ws_type, connection_index)` whose last pre-boot row was an
   "up" kind (`connected`/`reconnected`/`sleep_resumed`) and whose first
   post-boot `connected` row exists, synthesize ONE `process_death` episode
   at ts = that first post-boot `connected` row's ts (deterministic ⇒
   DEDUP-idempotent), `down_secs` = gap, blame ALWAYS `ours` with
   deploy-vs-crash sub-reason via `build_info::BUILD_GIT_SHA` vs the SSM
   `/tickvault/<env>/deploy/binary-git-sha` param (control-plane read,
   fail-soft to `process_restart` on any lookup failure).
4. **15:45 IST daily task (same module)** — `tick_conservation_boot.rs`
   idiom: `SCOREBOARD_TRIGGER_SECS_OF_DAY_IST = 56_700`, pure
   `decide_scoreboard_start` with the `RunCatchUp` late-boot variant,
   `TICKVAULT_SCOREBOARD_NOW` env override, trading-day gate. Steps (all
   cold-path QuestDB `/exec` reads, 10s timeout, fail-to-sentinel parses):
   episodes from today's `ws_event_audit` + same-day errors.jsonl
   correlation scan (RESILIENCE-01/03, WS-GAP-09 reasons ±120s, PROC-01 /
   RESOURCE-01..03 ±300s) → classify → UPSERT `feed_episode_audit`; blame
   aggregate read back from `feed_episode_audit` (includes the boot-written
   process-death rows); per-feed ticks / distinct-instrument / session
   distinct-minute SQL over `ticks` (feed-level unique-win + both minutes
   computed in Rust from the two ≤375-entry minute sets); lag columns = −1
   sentinels with `lag_floor_ms` honesty column (PR-3 lands the histograms);
   `tv_ws_event_audit_dropped_total` self-scrape > 0 ⇒ `outcome='degraded'`;
   UPSERT 2 `feed_scoreboard_daily` rows; return the summary.
5. **Telegram** — `NotificationEvent::DualFeedDailyScorecard` (Severity::Info
   + `DispatchPolicy::Immediate`, the CrossVerify1mSummary precedent) built
   from the summary via the main.rs inner/outer supervisor idiom; on
   Err/panic the outer emits `DualFeedScorecardAborted` (High) so the daily
   signal can never be silently dropped. Body obeys the 10 Telegram
   commandments (plain English, emoji, IST 12-hour, specific numbers, ONE
   decision = the verdict line via the tiebreak ladder: exclusive minutes →
   worst-1% delay → broker-blamed drops → "🤝 Even day."), with partial-day
   + Dhan whole-second lag-floor footnotes when applicable.
6. **ErrorCode** — `Scoreboard01AggregationDegraded` ("SCOREBOARD-01",
   Severity::Medium, auto-triage-safe — best-effort forensic aggregate,
   AUDIT-WS-01 class) + rule file
   `.claude/rules/project/dual-feed-scoreboard-error-codes.md`. Every new
   `error!` carries `code =`.
7. **Config** — `[scoreboard]` → `ScoreboardConfig` (all serde-defaulted,
   `enabled = true` safe-on: the aggregation only READS existing tables).
8. **Runbook** — `docs/runbooks/dual-feed-scoreboard.md` (month-end
   cumulative verdict SQL, indeterminate-review procedure, day-1 notes,
   backfill semantics).

## Edge Cases

- Late boot past 15:45 IST → `RunCatchUp` (one immediate run; the row is
  honest via query-backed totals, never skipped).
- Non-trading day → skip (no weekend rows); `TICKVAULT_SCOREBOARD_NOW`
  overrides both gates for operator dry-runs.
- Groww emits `connected` TWICE per episode by design (`groww_subscribed`
  then `groww_sidecar`) — the reconciler pairs on the EARLIEST post-boot
  `connected` per key, and connect rows are never counted as disconnects.
- `dhan_code` −1 sentinel rows (transport errors, all order-update + groww
  rows) classify via source string, never via scraped digits.
- Unknown/future source strings or event kinds → `Indeterminate`/
  `unclassified` (total fn; proptest-pinned never-panic).
- Same-second same-kind episodes on one connection collapse via DEDUP (ts is
  in the key at micro precision — acceptable, documented).
- errors.jsonl absent / >48h aged out (backfill day) → empty correlation
  evidence: 805 defaults broker, RSTs default indeterminate (runbook note).
- QuestDB unreachable mid-run → −1 sentinels + `outcome='partial'`, row
  still written when the writer flush succeeds; SCOREBOARD-01 error!
  otherwise. NEVER fabricated zeros.
- Boot reconciler: no post-boot `connected` row within the delay window →
  skip that key (no deterministic ts to stamp; honest, documented).
- Feed disabled all day → its row shows 0 ticks / 0 minutes (query-backed
  truth, not fabricated).

## Failure Modes

- **QuestDB down at 15:45** → every query fails → sentinels + partial
  outcome + `error!(code = SCOREBOARD-01)`; Telegram still fires with the
  partial numbers (never silent).
- **ws_event_audit under-count** (AUDIT-WS-01 drops that day) → cross-check
  `tv_ws_event_audit_dropped_total` ⇒ `outcome='degraded'` + body warning.
- **Task panic** → outer JoinHandle watcher fires `DualFeedScorecardAborted`
  (High) — the missing scorecard is impossible to miss; graceful-shutdown
  cancellation stays silent (normal teardown).
- **ILP write reject** → ILP-over-HTTP flush surfaces server rejects as
  `Err` → SCOREBOARD-01 error! (the 2026-07-05 fire-and-forget lesson).
- **SSM sha lookup failure** → fail-soft `process_restart` sub-reason
  (blame stays `ours` either way).
- **Classifier drift** (new source label appears) → `unclassified`
  indeterminate rows, visible in the runbook's indeterminate-review drill —
  never a blank or a panic.

## Test Plan

- Storage: DDL column pins, DEDUP-key pins (ts-first + feed-in-key +
  I-P1-11 pair on the per-instrument table), outcome/coverage-source label
  stability, append-shape ILP buffer assertions (symbols before columns,
  feed tag present, blame tag present), disconnected-flush error, ILP-HTTP
  conf ratchet — all without live QuestDB (`for_test()` writers).
- Classifier: exhaustive table test over EVERY known (episode_kind, source,
  dhan_code, overlap-flag) combination from the contract mapping;
  `test_classifier_total_unknown_inputs_map_to_indeterminate`; proptest over
  arbitrary strings/codes (always classifies, never panics, reason slug
  never empty); BlameClass label pins.
- Reconciler: pure `synthesize_process_death_episodes` unit tests (up-state
  prior → synthesized; down-state prior → none; deterministic ts; Groww
  double-connect earliest pairing; out-of-session boot → none) +
  deploy-vs-crash sub-reason pure fn tests.
- Daily task: `decide_scoreboard_start` boundary tests (trigger constant
  pin, RunCatchUp band, non-trading skip, force override); SQL-builder
  string pins (feed filter + exact day/session windows); `/exec` row parser
  tests; minute-overlap pure fn; errors.jsonl line parser + correlation
  window tests; prom label-sum parser.
- Events: topic/severity/policy arms + body tests (verdict ladder branches,
  −1 "not measured yet" rendering, partial/degraded footnotes, lag-floor
  footnote, 10-commandments litmus: no file paths/library names).
- Config: serde-default rollback test (`scoreboard_flag_rollback` per B12) +
  empty-TOML parse.
- Wiring ratchet: `test_feed_scoreboard_task_is_wired_into_main`
  (source-scan in `secret_manager.rs` tests, house pattern).
- Gates: fmt, scoped clippy `-D warnings`, `cargo test --workspace`
  (crates/common touched ⇒ escalation per testing-scope), banned-pattern
  scanner, plan-verify.

## Rollback

- Config: set `[scoreboard] enabled = false` → the daily task + reconciler
  + Telegram are never spawned; zero behavioral change (rollback path
  covered by the `scoreboard_flag_rollback` serde test).
- Code: `git revert` of the PR — the three new tables are additive
  (CREATE IF NOT EXISTS; no existing table/DDL touched, no hot-path code
  touched), so a revert leaves no dangling writers. Rows already written
  stay (SEBI-safe, never dropped).
- The new NotificationEvent variants / ErrorCode variant are additive enum
  arms — revert removes emit sites with them.

## Observability

- `error!(code = ErrorCode::Scoreboard01AggregationDegraded.code_str())` on
  every degraded/failed aggregation leg (5-sink chain; log-sink-only — no
  new CloudWatch alarm per the exhausted alarm budget; the daily Telegram
  IS the operator signal).
- Counters (/metrics-only, ₹0 EMF): `tv_feed_scoreboard_runs_total{outcome}`,
  `tv_feed_scoreboard_episode_rows_total`,
  `tv_feed_scoreboard_process_death_synthesized_total`.
- Telegram: `DualFeedDailyScorecard` (Info, Immediate) daily positive
  signal; `DualFeedScorecardAborted` (High) on task death.
- Forensic system-of-record: `feed_scoreboard_daily` +
  `feed_episode_audit` (+ `feed_coverage_daily` from PR-4) QuestDB tables,
  DEDUP-idempotent, queryable via `mcp__tickvault-logs__questdb_sql`;
  month-end SQL in `docs/runbooks/dual-feed-scoreboard.md`.
- Rule file `.claude/rules/project/dual-feed-scoreboard-error-codes.md`
  satisfies the error-code cross-ref test + carries triage.

## Plan Items

- [x] Item 1 — Storage tables + writers (scoreboard daily + coverage detail + episode audit)
  - Files: crates/storage/src/feed_scoreboard_persistence.rs, crates/storage/src/feed_episode_audit_persistence.rs, crates/storage/src/lib.rs
  - Tests: test_feed_scoreboard_daily_ddl_contains_expected_columns, test_feed_scoreboard_dedup_keys_ts_first_and_feed_in_key, test_feed_coverage_daily_dedup_key_full_instrument_pair_plus_feed, test_feed_episode_audit_ddl_contains_expected_columns, test_episode_append_row_writes_blame_and_feed_symbols, test_episode_flush_when_disconnected_errors, test_scoreboard_writers_use_ilp_http_conf

- [x] Item 2 — Pure total blame classifier
  - Files: crates/common/src/feed_blame.rs, crates/common/src/lib.rs
  - Tests: test_blame_class_labels_stable_and_nonempty, test_classify_dhan_805_with_peer_evidence_is_ours_dual_instance, test_classify_dhan_805_without_evidence_is_broker, test_classify_dhan_807_is_broker_auth_token_expired, test_classify_dhan_auth_entitlement_codes_are_broker, test_classify_rst_with_ws_gap9_overlap_is_broker, test_classify_rst_without_overlap_is_indeterminate, test_classify_network_and_unknown_are_indeterminate, test_classify_groww_feed_toggle_is_ours, test_classify_stall_reasons, test_classify_resource_pressure_is_ours, test_classify_process_death_deploy_vs_crash, test_classify_off_hours_disconnect_is_indeterminate, test_classifier_total_unknown_inputs_map_to_indeterminate, prop_classifier_is_total_never_panics_never_blank

- [x] Item 3 — SCOREBOARD-01 error code + rule file
  - Files: crates/common/src/error_code.rs, .claude/rules/project/dual-feed-scoreboard-error-codes.md
  - Tests: test_all_variants_have_unique_code_str, every_error_code_variant_appears_in_a_rule_file, every_runbook_path_exists_on_disk

- [x] Item 4 — [scoreboard] config section (serde-defaulted, toggleable)
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_scoreboard_config_defaults_enabled_safe_on, scoreboard_flag_rollback

- [x] Item 5 — Process-death reconciler + 15:45 IST daily task
  - Files: crates/app/src/feed_scoreboard_boot.rs, crates/app/src/lib.rs
  - Tests: test_decide_scoreboard_start_boundaries, test_scoreboard_trigger_constant_is_1545_ist, test_build_scoreboard_sql_builders_feed_filtered_and_windowed, test_parse_ws_events_from_exec_body, test_compute_minute_overlap, test_parse_errors_jsonl_line_extracts_code_reason_ts, test_correlation_overlap_windows, test_synthesize_process_death_up_state_prior, test_synthesize_process_death_skips_down_state_and_out_of_session, test_synthesize_process_death_deterministic_ts_and_groww_double_connect, test_deploy_vs_crash_sub_reason, test_parse_prom_counter_sum_labeled_series

- [x] Item 6 — Telegram scorecard events + main.rs wiring
  - Files: crates/core/src/notification/events.rs, crates/app/src/main.rs, crates/core/src/auth/secret_manager.rs
  - Tests: test_dual_feed_scorecard_topic_severity_policy, test_dual_feed_scorecard_body_verdict_ladder, test_dual_feed_scorecard_body_sentinels_and_footnotes, test_dual_feed_scorecard_body_obeys_telegram_commandments, test_dual_feed_scorecard_aborted_event, test_feed_scoreboard_task_is_wired_into_main

- [x] Item 7 — Month-end runbook
  - Files: docs/runbooks/dual-feed-scoreboard.md
  - Tests: every_runbook_path_exists_on_disk (rule file cross-ref covers the rule doc; the runbook is prose)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Clean trading day, both feeds up | 2 `complete` daily rows, scorecard Telegram with verdict |
| 2 | Mid-day process death + restart | boot reconciler synthesizes 1 process_death episode per up-key, blame `ours`, deterministic ts (re-boot idempotent) |
| 3 | Dhan 805 with same-day RESILIENCE line | episode blame `ours` / `dual_instance` |
| 4 | Dhan 805, no peer evidence | episode blame `broker` / `rate_limit_805` |
| 5 | Mid-stream RST + WS-GAP-09 bare-reset within ±120s | blame `broker` / `bare_rst` |
| 6 | Mid-stream RST alone | blame `indeterminate` / `transport_ambiguous` |
| 7 | QuestDB down at 15:45 | sentinels + `partial`, SCOREBOARD-01 error!, Telegram still fires |
| 8 | Task panic | `DualFeedScorecardAborted` High Telegram |
| 9 | ws_event_audit drops that day | `outcome='degraded'` + body warning |
| 10 | Novel source string | `indeterminate` / `unclassified` (never blank, never panic) |
| 11 | `[scoreboard] enabled = false` | nothing spawns; zero behavior change |
| 12 | Weekend / holiday boot | task skips; no rows |
