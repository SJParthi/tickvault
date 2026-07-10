# Implementation Plan: Groww multi-connection AUTO-SCALE (contracts → supervisor/ladder → operator UX)

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — dated verbatim quotes 2026-07-03 evening (below), Mac-first execution confirmed

> **Operator directive (verbatim, 2026-07-03 evening):** "I planned to establish
> multiple connections… 100 parallel connections to test 100k instruments… every
> ten connection should hold 1k instruments per connection… incremental dynamic
> scalable connections of max 100 connections starting 10… when auto scale up
> happens if there is any failure how that can be auto corrected" + Mac-first
> execution confirmed.

Canonical design: the 2026-07-03 auto-scale design (§34 of
`.claude/rules/project/groww-second-feed-scope-2026-06-19.md` records the
authorization + tiers). Serial PR sequence per `pr-completion-protocol.md`:

- **PR-1 (this plan, items 1–4):** contracts — §34 rule edit, `[feeds.groww.scale]`
  config struct, deterministic range-based shard cutter, 4 ErrorCode variants
  GROWW-SCALE-01..04 + their rule file.
- **PR-2 (items 5–9):** N-process sidecar supervision + ladder FSM + per-shard
  bridge capture chain + `groww_scale_audit` table + metrics.
- **PR-3 (items 10–12):** `make scale-test` + runbook + 2×600 cap-probe mode +
  portal per-conn health rows.

Crates touched: `crates/common` (config.rs, error_code.rs), `crates/core`
(feed/groww/shard_cutter.rs, feed/groww/mod.rs), later `crates/app`
(groww_sidecar_supervisor.rs, groww_bridge.rs, groww_scale_ladder.rs),
`crates/storage` (groww_scale_audit_persistence.rs), `crates/api` (feeds
health), `scripts/groww-sidecar/groww_sidecar.py`, `Makefile`,
`docs/runbooks/groww-scale-test.md`, `config/base.toml`.

## Design

**Topology (design §1):** N Python sidecar PROCESSES, one per Groww NATS
connection. Each child gets `GROWW_CONN_ID` + `GROWW_SHARD_SPEC` env (per-conn
watch file `data/groww/shards/groww-watch-<date>-c<NN>.json`, per-conn NDJSON
`data/groww/shards/c<NN>/live-ticks.ndjson`, per-conn status file). Process-per-
connection gives GIL isolation, per-conn FEED-STALL-01 kill/relaunch, and a
1/N crash blast radius. The consumer side is ONE Rust process with one
supervised bridge tail-task per shard file (reusing the
`spawn_supervised_groww_bridge` pattern), all feeding the SAME shared
aggregator + ring→spill→DLQ persistence chain.

**Sharding (PR-1 cutter):** RANGE-BASED, not hash. `cut_shards` sorts the
watch-set by `(exchange, segment, security_id)` and cuts contiguous ranges of
≤ `instruments_per_conn` (default 1000, Groww SDK cap). Deterministic +
human-inspectable; re-cutting at a new N churns only the tail. Fail-closed
invariants asserted by the cutter itself: shards are DISJOINT and their union
== the input watch-set; duplicate identities in the input are rejected. Cold
path only — the per-tick path never consults the cutter.

**Ladder FSM (PR-2, design §2):** states PROBING → HOLDING → ADVANCING →
(PROBING | ROLLING_BACK) plus HALTED_AT_CEILING. Day-1 rungs `[1, 2, 5, 10]`.
Advance gate (all must hold `gate_hold_minutes` = 15): every conn health Ok,
0 fleet-wide auth rejects, per-shard capture lag p99 < 30s, CPU < 70%, disk
free > 20%, QuestDB drain healthy, bridge behind-ness < 4 MB/shard. Failure at
a rung → ROLLING_BACK to the last KNOWN-healthy N (kill newest conns only) +
exponential hold `min(10m × 2^k, 4h)`. ALL conns failing inside 60s = account-
level throttle → global cooldown 5 min + HALVE N. ADVANCING only inside the
config window (default 09:20–14:30 IST) or pre-open. Ladder state survives
restart via `groww_scale_audit` (resume at last_healthy, never the unverified
rung).

**capture_seq (design §4):** all per-shard bridge tasks draw from ONE shared
`Arc<AtomicI64>` via the existing `next_capture_seq` — globally unique +
monotonic across shards; shard disjointness is the primary guarantee that two
shards never emit the same `(security_id, segment)`.

**Config (PR-1):** `[feeds.groww.scale]` → `GrowwScaleConfig` nested under
`FeedsConfig` via a `groww` table. `enabled = false` default routes through
the existing single-conn supervisor path — byte-identical behavior, no new
process, no new file paths. Fields: `enabled`, `target_connections` (10),
`instruments_per_conn` (1000), `ladder` ([1,2,5,10]), `gate_hold_minutes`
(15), `gate_max_cpu_pct` (70), `gate_min_disk_free_pct` (20),
`gate_max_capture_lag_ms` (30000), `rollback_hold_base_minutes` (10),
`advance_window_ist` (["09:20","14:30"]). Pure `validate()` enforces the
envelope (target ≤ 100 hard max; per-conn ≤ 1000; ladder strictly increasing,
last rung ≤ target; window parses HH:MM with start < end). Tier constants:
Tier A ≤ 10 conns (Monday), Tier B ≤ 25 (gated on live measurement), Tier C
≤ 100 (infra sign-off per §34).

**ErrorCodes (PR-1):** `GrowwScale01RollbackFired` ("GROWW-SCALE-01", High),
`GrowwScale02GlobalHalve` ("GROWW-SCALE-02", High),
`GrowwScale03ShardOverlap` ("GROWW-SCALE-03", Critical),
`GrowwScale04AuditWriteFailed` ("GROWW-SCALE-04", Medium). Runbook:
`.claude/rules/project/groww-scale-error-codes.md`. Emit sites land in PR-2.

**Operator UX (PR-3):** `make scale-test` one-command Mac runbook
(groww-only profile, Dhan disabled, local QuestDB); `scale.probe_mode = true`
runs exactly 2 conns × 600 instruments and prints/logs the per-conn-vs-
per-account subscription-cap verdict then exits the ladder; portal feeds
panel renders per-conn health rows from a `connections: [...]` array.

## Edge Cases

- Empty watch-set → cutter returns zero shards (no conns spawned); ladder
  never leaves PROBING at N=0 → treated as feed-disabled.
- Watch-set smaller than `instruments_per_conn` → exactly 1 shard; ladder
  rungs above `required_connections` are skipped (ceiling = required conns).
- Duplicate `(exchange, segment, security_id)` in the input → cutter returns
  `Err(DuplicateEntry)` (fail-closed; GROWW-SCALE-03 class at runtime).
- `instruments_per_conn = 0` or > 1000 → config `validate()` rejects; cutter
  independently rejects (defense in depth).
- Ladder like `[10, 5]` (non-increasing) or last rung > target → `validate()`
  rejects at boot, before any process spawns.
- `advance_window_ist` malformed (`"9:20"`, `"25:00"`, start ≥ end) →
  `validate()` rejects.
- Mid-ladder deploy/restart → boot rehydrates last VERIFIED rung from
  `groww_scale_audit`; an interrupted ADVANCING resumes at last_healthy (PR-2
  chaos test).
- IST-midnight NDJSON rotation × N shard files → each sidecar rotates its OWN
  file; per-shard bridge offsets reset per the existing snapshot logic.
- Market close / weekend → ladder freezes (market-hours gate); no PROBING
  credit accrues off-hours.
- All conns fail within 60s (account throttle) → global cooldown + halve,
  never treated as a single-rung failure.

## Failure Modes

Full 16-row taxonomy lives in the design doc (§3) and is implemented in PR-2.
PR-1 contract failure modes:

- Config invalid → boot refuses to start the scale path (fail-closed), the
  single-conn path is unaffected when `enabled = false`.
- Cutter invariant violation (overlap / coverage gap / duplicate) → typed
  error, ladder step HALTs, GROWW-SCALE-03 (Critical) pages the operator —
  indicates a cutter bug, never silently subscribed twice.
- Rollback fired (rung failure) → GROWW-SCALE-01 (High), auto-corrected:
  newest conns killed, last-healthy N restored, expo hold, retry.
- Fleet-wide simultaneous failure → GROWW-SCALE-02 (High), global cooldown +
  halve — distinguishes "rung N+1 broke" from "provider throttling us".
- Audit-row write failure → GROWW-SCALE-04 (Medium, best-effort mirror of
  AUDIT-WS-01): ladder decisions continue on in-memory state; forensic row
  lost only for the outage window.
- Server-side conn/sub caps are UNKNOWN until live-probed — the ladder
  discovers them and holds below (honest envelope; not a defect).

## Test Plan

PR-1:
- `crates/common/src/config.rs` tests: `test_groww_scale_defaults_are_off_and_tier_a`,
  `test_groww_scale_parses_from_toml`, `test_groww_scale_missing_section_defaults`,
  `test_groww_scale_validate_accepts_defaults`,
  `test_groww_scale_validate_rejects_zero_per_conn`,
  `test_groww_scale_validate_rejects_over_hard_max`,
  `test_groww_scale_validate_rejects_non_increasing_ladder`,
  `test_groww_scale_validate_rejects_ladder_above_target`,
  `test_groww_scale_validate_rejects_bad_window`.
- `crates/core/src/feed/groww/shard_cutter.rs` tests (ratchet):
  `test_cut_shards_disjoint_and_covering`, `test_cut_shards_deterministic_order`,
  `test_cut_shards_respects_per_conn_cap`, `test_cut_shards_empty_input`,
  `test_cut_shards_rejects_zero_cap`, `test_cut_shards_rejects_over_groww_cap`,
  `test_cut_shards_rejects_duplicate_identity`,
  `test_required_connections_ceil_math`, `test_cut_shards_single_shard_small_universe`,
  `test_cut_shards_tail_churn_only_on_recut`.
- `crates/common/src/error_code.rs` existing ratchets extended (catalogue size
  119 → 123, all() list, prefix test gains `GROWW-SCALE-`), cross-ref tests
  green against the new rule file.

PR-2: ladder FSM unit tests for every transition + gate permutation; cutter
proptest (`crates/core/tests/`); supervisor respawn tests; chaos test for
mid-ladder deploy-restart resume; audit-table DDL/DEDUP tests.

PR-3: probe-mode classifier unit tests; Makefile target smoke; runbook link
checks; portal rows render test.

## Rollback

- PR-1 is contracts-only: `enabled = false` default means zero behavioral
  delta; reverting the PR restores the previous config/enum surface with no
  data migration (no table shipped in PR-1).
- PR-2: flip `[feeds.groww.scale] enabled = false` → single-conn path,
  byte-identical to today (ratchet-tested). The `groww_scale_audit` table is
  additive (DEDUP UPSERT, never read by the single-conn path).
- PR-3: `make scale-test` and probe mode are opt-in; deleting the target /
  leaving `probe_mode = false` restores prior behavior.
- Full revert path: `git revert` of each squash-merge commit in reverse
  order; no schema DROP needed (audit table is retained per SEBI discipline).

## Observability

- Metrics (PR-2): `tv_groww_conns_active`, `tv_groww_conns_target`,
  `tv_groww_ladder_state` (0..4), `tv_groww_scale_rollbacks_total{reason}`,
  `tv_groww_shard_capture_lag_ms{conn}`,
  `tv_groww_shard_bridge_behind_bytes{conn}`.
- Typed `error!`/`warn!` with `code = ErrorCode::GrowwScale0X…code_str()` at
  every ladder decision point (tag-guard enforced).
- Audit table `groww_scale_audit` (SEBI template): `ts, trading_date_ist,
  from_conns, to_conns, state, outcome, reason, gate_snapshot`, DEDUP
  `(trading_date_ist, ts, from_conns, to_conns, outcome)`.
- Telegram: edge-triggered, 10-commandments wording; log-only when the scale
  profile sets notifications off (Mac scale-test profile).
- Portal: per-conn health rows auto-extend from the feeds-health API array.

## Plan Items

### PR-1 — contracts + rule edit

- [x] Item 1: §34 multi-connection authorization appended to the Groww scope lock
  - Files: .claude/rules/project/groww-second-feed-scope-2026-06-19.md
  - Tests: n/a — rule-file edit (dated operator quotes + honest envelope + tiers A/B/C)
- [x] Item 2: `[feeds.groww.scale]` config struct, default OFF, pure validation
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_groww_scale_defaults_are_off_and_tier_a, test_groww_scale_parses_from_toml, test_groww_scale_missing_section_defaults, test_groww_scale_validate_accepts_defaults, test_groww_scale_validate_rejects_zero_per_conn, test_groww_scale_validate_rejects_over_hard_max, test_groww_scale_validate_rejects_non_increasing_ladder, test_groww_scale_validate_rejects_ladder_above_target, test_groww_scale_validate_rejects_bad_window
- [x] Item 3: deterministic range-based shard cutter (pure fn) + disjointness/coverage ratchets
  - Files: crates/core/src/feed/groww/shard_cutter.rs, crates/core/src/feed/groww/mod.rs
  - Tests: test_cut_shards_disjoint_and_covering, test_cut_shards_deterministic_order, test_cut_shards_respects_per_conn_cap, test_cut_shards_empty_input, test_cut_shards_rejects_zero_cap, test_cut_shards_rejects_over_groww_cap, test_cut_shards_rejects_duplicate_identity, test_required_connections_ceil_math, test_cut_shards_single_shard_small_universe, test_cut_shards_tail_churn_only_on_recut
- [x] Item 4: 4 ErrorCode variants GROWW-SCALE-01..04 + rule file
  - Files: crates/common/src/error_code.rs, .claude/rules/project/groww-scale-error-codes.md
  - Tests: test_all_list_length_matches_catalogue_size (119→123), test_code_str_follows_expected_prefix_pattern (GROWW-SCALE-), every_error_code_variant_appears_in_a_rule_file

### PR-2 — supervisor + ladder + capture chain

- [x] Item 5: N-process sidecar supervision (per-conn child, conn_id + shard env; sidecar reads GROWW_SHARD_SPEC)
  - Files: crates/app/src/groww_sidecar_supervisor.rs, scripts/groww-sidecar/groww_sidecar.py
  - Tests: test_shard_child_env_carries_conn_id_and_spec, test_single_conn_path_unchanged_when_scale_disabled
- [x] Item 6: ladder FSM (PROBING/HOLDING/ADVANCING/ROLLING_BACK/HALTED_AT_CEILING) + gates + rollback + global halve
  - Files: crates/app/src/groww_scale_ladder.rs
  - Tests: ladder FSM unit tests (every transition + gate permutation), test_global_failure_halves_and_cooldowns, test_rollback_returns_to_last_healthy, test_advance_window_gate
- [x] Item 7: per-shard bridge tail-tasks + shared capture_seq atomic
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_shared_capture_seq_monotonic_across_shards, supervisor respawn tests
- [x] Item 8: `groww_scale_audit` table (DEDUP keys per repo rules) + metrics + log-only Telegram profile
  - Files: crates/storage/src/groww_scale_audit_persistence.rs, crates/app/src/metrics_catalog.rs
  - Tests: DDL/DEDUP/outcome-enum tests per audit-table template
- [x] Item 9: cutter proptest + chaos test for mid-ladder deploy-restart resume
  - Files: crates/core/tests/shard_cutter_property.rs, crates/app/src/groww_scale_ladder.rs (rehydrate tests)
  - Tests: proptest arbitrary watch-sets always disjoint+covering, test_restart_mid_ladder_resumes_last_healthy

### PR-3 — operator UX

- [x] Item 10: `make scale-test` target + Mac runbook
  - Files: Makefile, docs/runbooks/groww-scale-test.md
  - Tests: n/a — docs + make target (shell smoke)
- [x] Item 11: 2×600 cap-probe mode (`scale.probe_mode = true`) with per-conn-vs-per-account verdict
  - Files: crates/common/src/config.rs (probe_mode), crates/app/src/groww_scale_ladder.rs (probe short-circuit)
  - Tests: test_probe_mode_runs_two_conns_600, test_probe_verdict_classification
- [x] Item 12: portal feeds panel per-conn health rows (app-side)
  - Files: crates/api/src/feed_state.rs / handlers, crates/app wiring
  - Tests: test_feeds_health_includes_connections_array
- [x] Addendum A (coordinator 2026-07-03): Mac preflight in app boot — shards-dir writable + disk headroom + QuestDB ILP/HTTP reachability + PROD-IS-UNTOUCHED banner; any FAIL falls back to the single-conn Groww path
  - Files: crates/app/src/scale_test_preflight.rs, crates/app/src/main.rs, crates/app/src/lib.rs
  - Tests: test_preflight_report_all_ok_classification, test_classify_disk_check_boundaries, test_is_local_host_classification, test_check_shards_dir_writable_pass_and_fail, test_check_tcp_reachable_against_live_listener_and_dead_port, test_run_scale_preflight_reports_four_checks
- [x] Addendum B (coordinator 2026-07-03): IntelliJ run configuration
  - Files: .run/Groww Scale Test.run.xml
  - Tests: n/a — IDE config (mirrors the existing .run/*.xml shape)
- [x] Addendum C (coordinator 2026-07-03): weekend SMOKE mode — market closed => machinery-validated run with tick gates honestly SKIPPED (auth + host gates stay real); outcomes labelled SMOKE; no effect while market open
  - Files: crates/common/src/config.rs (weekend_smoke), crates/app/src/groww_scale_ladder.rs
  - Tests: test_smoke_mode_gate_inputs_pass_tick_gates_keep_host_gates, test_groww_scale_probe_mode_and_weekend_smoke_parse_from_toml

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `enabled=false` (default) | byte-identical single-conn behavior; no shard files |
| 2 | 768-SID universe, per_conn=1000 | 1 shard, ladder ceiling 1 |
| 3 | 10k universe, ladder [1,2,5,10] | rungs climb only when all gates hold 15 min |
| 4 | conn 7 stalls mid-market | FEED-STALL-01 kills+relaunches conn 7 only |
| 5 | rung 5→10 spawn fails | rollback to 5, expo hold, GROWW-SCALE-01 |
| 6 | all conns die inside 60s | cooldown 5 min + halve, GROWW-SCALE-02 |
| 7 | cutter emits overlapping shards | HALT step, GROWW-SCALE-03 Critical |
| 8 | audit ILP down | ladder continues, GROWW-SCALE-04 Medium |
| 9 | deploy mid-ladder | resume at last_healthy rung from audit table |
| 10 | probe_mode=true | exactly 2 conns × 600, verdict printed, ladder exits |

## Per-Item Guarantee Matrix

Every item above carries the canonical 15-row 100% Guarantee Matrix + 7-row
Resilience Demand Matrix by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (per that rule's
cross-reference provision). Item-specific rows:

- Zero ticks lost: per-shard capture-at-receipt NDJSON → the existing
  ring→spill→DLQ chain, unchanged per shard; the cutter/ladder never touch
  the per-tick path.
- Uniqueness + dedup: shard disjointness (ratchet-tested) + the shared
  capture_seq atomic + the `ticks` DEDUP key
  `(ts, security_id, segment, capture_seq, feed)`.
- O(1) latency: shard decision is a cold boot/ladder-step computation
  (flagged O(N log N) sort, honest — never per-tick); per-tick path unchanged.
- Real-time proof: metrics + typed codes + audit rows + portal rows (PR-2/3).

Honest 100% claim (mandatory §F wording): 100% inside the tested envelope,
with ratcheted regression coverage: auto-correction covers the 16 failure
classes in the design taxonomy — detect ≤30s (per-conn stall watchdog),
rollback to last-healthy rung, expo hold 10m→4h, retry forever in market
hours; capture-at-receipt durability per shard → the existing ring→spill→DLQ.
NOT claimed: Groww's server-side per-account connection limit is UNKNOWN
until live-probed (the ladder discovers it and holds below it — that is the
design, not a defect); the Python hop is not O(1) (GIL jitter, existing §32
envelope); upstream ticks Groww never delivers are invisible (existing §5
CAPTURE vs UPSTREAM split).
