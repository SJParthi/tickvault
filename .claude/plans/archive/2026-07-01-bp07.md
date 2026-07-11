# Implementation Plan: BP-07 — PROC-01 OOM-kill monitor

**Status:** APPROVED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — audit item BP-07 (`.claude/plans/permutation-coverage-audit-2026-07-01.md` §146)

> **Guarantee matrices:** carried by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory per `per-item-guarantee-check.sh`). All 15 rows of the 100% Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply to every item in this plan.

## Design

Audit BP-07: an OOM kill has **no dedicated signal** today. `wave-4-error-codes.md`
names PROC-01 "Reserved" with a `Source (planned): crates/app/src/oom_monitor.rs`
that does not exist, and there is **no `Proc01` ErrorCode variant**. An OOM is only
caught indirectly (process dies → systemd → market-hours-liveness page on missing
SLO), so an OOM-loop is indistinguishable from a panic-loop and there is zero OOM
attribution.

This item ships the PROC-01 signal, mirroring the existing
`crates/storage/src/disk_health_watcher.rs` supervised-watcher template:

1. **`Proc01OomKillDetected` ErrorCode variant** in `crates/common/src/error_code.rs`
   (`code_str()="PROC-01"`, `Severity::Critical`, `runbook_path()` →
   `.claude/rules/project/wave-4-error-codes.md`, added to `all()` + the `PROC-`
   prefix allowlist in `test_code_str_follows_expected_prefix_pattern`).
2. **`crates/storage/src/oom_monitor.rs`** — the module the runbook names:
   - **PURE parser** `parse_oom_kill_count(&str) -> Option<u64>` — reads cgroup-v2
     `memory.events` text, returns the `oom_kill` field value (unit-testable with
     fixture strings; None on missing field / malformed / non-UTF-8-handled-by-caller).
   - **PURE decision** `classify_oom_delta(baseline, current) -> OomDelta` — compares
     the current `oom_kill` count against the boot baseline; returns `NoChange` or
     `NewKills { count }` (a positive delta). Saturating so a counter reset (cgroup
     re-created) cannot underflow-panic.
   - **`read_oom_kill_count(path)`** — reads the file, returns the parsed count.
   - **counter** `tv_oom_kills_total` on a positive delta + `error!(code=PROC-01)`.
   - **market-hours-aware** poll task (`spawn_oom_monitor` / supervised wrapper),
     mirroring the disk watcher: 60s cadence, cgroup-v2 path default
     `/sys/fs/cgroup/memory.events`, POSIX/Linux only (non-cgroup env → probe fails
     softly, no panic, no false page). The baseline is captured on the FIRST
     successful read so a pre-existing lifetime OOM count never fires a spurious page.
3. **Boot wiring** in `crates/app/src/main.rs` next to the disk-health watcher spawn,
   behind the same always-on pattern (best-effort background task; a non-cgroup dev
   box just probe-fails silently). Wiring ratchet added.

## Edge Cases

- **cgroup-v1 / non-Linux / missing file** → `read_oom_kill_count` returns None; the
  task increments a probe-failure counter and logs (no page, no panic) — honest
  "no signal" per the disk-watcher precedent.
- **`memory.events` missing the `oom_kill` line** → parser returns None.
- **Counter reset** (cgroup re-created below the baseline) → `classify_oom_delta`
  saturating-subtracts → `NoChange`, and the next read re-baselines upward naturally.
- **First read after boot** → establishes the baseline; delta is 0, no page.
- **Whitespace / extra fields / trailing newline** in `memory.events` → parser is
  field-name matched (`oom_kill <N>`), tolerant of surrounding fields.

## Failure Modes

- Probe I/O error (permission, transient `/sys` read glitch) → probe-failed counter +
  `warn!`, gauge/baseline unchanged, task continues (self-heals next tick).
- Task panic → supervised wrapper respawns (mirrors DISK-WATCHER-01), so OOM
  monitoring never vanishes silently. Supervisor has no panic path of its own.
- The monitor is observability-only; it NEVER blocks boot or the hot path.

## Test Plan

Pure-function unit tests in `oom_monitor.rs` (fixture strings — no live box):
- `test_parse_oom_kill_count_happy_path` — real cgroup-v2 `memory.events` fixture.
- `test_parse_oom_kill_count_missing_field` — no `oom_kill` line → None.
- `test_parse_oom_kill_count_malformed_value` — `oom_kill notanumber` → None.
- `test_parse_oom_kill_count_zero` — `oom_kill 0` → Some(0).
- `test_parse_oom_kill_count_tolerates_other_fields_and_whitespace`.
- `test_classify_oom_delta_no_change` / `_positive_delta` / `_counter_reset_saturates`.
- `test_poll_interval_constant_reasonable`, `test_default_cgroup_path_is_v2`.
- supervisor: `classify_join_exit` reuse + `spawn_supervised_oom_monitor` keeps-running.
ErrorCode ratchets already cover the new variant (unique code_str, runbook path,
severity, all()-exhaustive, prefix pattern). Wiring ratchet:
`test_oom_monitor_is_wired_into_main` (source-scan in `secret_manager.rs` tests, the
established pattern) OR a storage-side source-scan if that file is not the right home
— confirmed home is `crates/core/src/auth/secret_manager.rs::tests` per existing
`test_*_is_wired_into_main` precedents.

## Rollback

Single self-contained module + one boot-spawn line + one ErrorCode variant. Revert the
commit; the disk-watcher template it mirrors is untouched, no shared state, no schema,
no hot-path change — nothing else depends on it.

## Observability

- Counter `tv_oom_kills_total` (new kills since boot baseline).
- Counter `tv_oom_monitor_probe_failed_total` (probe I/O/parse failures).
- Counter `tv_oom_monitor_respawn_total{reason}` (supervisor respawn — flap signal).
- `error!(code=PROC-01)` on a positive delta → routes to Telegram via the 5-sink
  pipeline (Severity::Critical). Runbook already exists in
  `.claude/rules/project/wave-4-error-codes.md` (PROC-01 section — promoted from
  "Reserved" to live in this PR).

## Plan Items

- [ ] Item 1 — add `Proc01OomKillDetected` ErrorCode variant + all match arms + prefix allowlist
  - Files: crates/common/src/error_code.rs
  - Tests: test_code_str_follows_expected_prefix_pattern (existing, extended), test_all_variants_have_unique_code_str (existing)
- [ ] Item 2 — `oom_monitor.rs` pure parser + decision + counters + supervised task
  - Files: crates/storage/src/oom_monitor.rs, crates/storage/src/lib.rs
  - Tests: test_parse_oom_kill_count_happy_path, test_parse_oom_kill_count_missing_field, test_parse_oom_kill_count_malformed_value, test_parse_oom_kill_count_zero, test_parse_oom_kill_count_tolerates_other_fields_and_whitespace, test_classify_oom_delta_no_change, test_classify_oom_delta_positive_delta, test_classify_oom_delta_counter_reset_saturates, test_oom_poll_interval_constant_reasonable, test_default_cgroup_path_is_v2, test_spawn_supervised_oom_monitor_keeps_running
- [ ] Item 3 — boot wiring in main.rs + wiring ratchet
  - Files: crates/app/src/main.rs, crates/core/src/auth/secret_manager.rs
  - Tests: test_oom_monitor_is_wired_into_main
- [ ] Item 4 — promote PROC-01 from "Reserved" to live in the runbook
  - Files: .claude/rules/project/wave-4-error-codes.md

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | OOM kill during market hours | oom_kill counter increments in cgroup → next poll delta>0 → error!(PROC-01) + tv_oom_kills_total++ → Telegram Critical |
| 2 | Non-cgroup dev box (macOS) | read fails softly → probe_failed counter, no page, no panic |
| 3 | Pre-existing lifetime OOM count at boot | first read sets baseline; delta 0; no spurious page |
| 4 | cgroup re-created below baseline | saturating delta → NoChange; re-baseline upward |
| 5 | monitor task panics | supervisor respawns; tv_oom_monitor_respawn_total++ |
