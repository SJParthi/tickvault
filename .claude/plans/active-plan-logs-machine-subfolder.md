# Implementation Plan: One human log surface — machine sinks move to data/logs/machine/

**Status:** VERIFIED
**Date:** 2026-07-05
**Approved by:** Parthiban (operator directive 2026-07-05: "one human log file; robot files into machine/ subfolder" — demanded repeatedly)

## Design

The operator's demand: `data/logs/` must show ONE rolling human log surface.
Today it shows `candles/`, `live_ticks/`, `errors.jsonl.*` (hourly),
`errors.log`, `errors.summary.md`, a 0-byte `app.log` Alloy placeholder, and
hourly `app.YYYY-MM-DD-HH` stdout captures — a mess.

**The split (LOCKED):**

| Surface | Path | Owner |
|---|---|---|
| HUMAN (stays, untouched) | `data/logs/tickvault.log` (symlink) + `data/logs/app.<IST-date>.log` (daily rolling) | launcher (local-runtime) — NOT touched by this PR |
| MACHINE (moves) | `data/logs/machine/` — `errors.jsonl.*`, `errors.log`, `errors.summary.md`, `app.log` (Alloy placeholder), `app.YYYY-MM-DD-HH` hourly app captures, `candles/`, `live_ticks/` | tickvault-app (this PR) |

**Mechanism:** repoint the existing path constants (single source of truth):
- `crates/app/src/observability.rs`: `ERRORS_JSONL_DIR` `data/logs` → `data/logs/machine`; `CATEGORY_CANDLES_DIR` / `CATEGORY_LIVE_TICKS_DIR` → `data/logs/machine/{candles,live_ticks}`. New `LEGACY_LOGS_DIR = "data/logs"` const for the grace-window sweep.
- `crates/app/src/boot_helpers.rs`: `APP_LOG_FILE_PATH` → `data/logs/machine/app.log`; `ERROR_LOG_FILE_PATH` → `data/logs/machine/errors.log`; `LOG_DIRECTORY` → `data/logs/machine`; fallback literals updated.
- `crates/app/src/infra.rs`: Alloy placeholder `app.log` created under `machine/` (dir creation idempotent, `create_dir_all`).
- `crates/app/src/main.rs`: retention sweepers sweep BOTH the machine dir AND the legacy top-level dir (grace window — old-path files age out naturally; no file moves at boot, honest and side-effect-free).
- `crates/app/src/observability.rs::sweep_app_log_retention`: hardened to skip ANY `*.log` name (not just `app.log`) so the legacy-dir grace sweep can NEVER delete the human `app.<date>.log` daily files.
- `crates/api/src/handlers/debug.rs`: `DEFAULT_LOGS_DIR` → `data/logs/machine` (`TV_LOGS_DIR` env override unchanged).
- MCP server `scripts/mcp-servers/tickvault-logs/server.py`: `_machine_logs_dir()` = `<logs>/machine` with legacy top-level fallback for `errors.jsonl.*` + `errors.summary.md` (grace window); human `app.<date>.log` tail stays at top level.
- `Makefile` `tail-errors` / `errors-summary` targets → machine paths (with legacy fallback).
- `.claude/triage/error-rules.yaml`, `.claude/triage/claude-loop-prompt.md`, `.claude/hooks/{error-triage.sh,session-context-brief.sh,session-sanity.sh}`, `scripts/{doctor.sh,mcp-doctor.sh}` → machine paths.
- `deploy/docker/alloy/alloy-config.alloy`: `errors.jsonl` glob gains `machine/` path (both kept during grace); the human `app.*.log` glob stays top-level.
- `.claude/rules/project/observability-architecture.md` canonical-paths table: dated 2026-07-05 update note.

## Edge Cases

- **Legacy files at old paths after upgrade:** left in place; BOTH dirs are swept by the retention tasks during the grace window (48h errors.jsonl / 168h app hourly + categories), so old-path files age out without a risky boot-time move.
- **Human daily log protection:** `sweep_app_log_retention` on the legacy dir must not delete `app.<date>.log` — enforced by the new skip-`*.log` guard + unit test.
- **`TV_LOGS_DIR` override (tests / prod):** still honored by the debug handler; API tests use temp dirs, unaffected.
- **MCP reads during transition:** machine dir empty right after deploy while old files still at top level → MCP falls back to legacy dir so `tail_errors` / `summary_snapshot` never go dark.
- **Missing dirs:** all appender inits + sweepers already tolerate missing dirs (`create_dir_all` / `NotFound → Ok(0)`).
- **`app.log` vs hourly `app.YYYY-MM-DD-HH`:** exact-name and suffix guards in the sweeper keep the Alloy placeholder alive.

## Failure Modes

- Appender init failure under `machine/` (unwritable mount): unchanged best-effort semantics — boot proceeds on stdout + remaining sinks (existing behaviour, no new failure class).
- Sweep of legacy dir fails (permissions): logs at WARN, next hourly tick retries — never halts.
- Alloy tailing: if only the old glob were kept, new `machine/errors.jsonl.*` files would be invisible to Loki — fixed by adding the machine glob; both globs present during grace so neither old nor new files are missed.
- MCP `find` on wrong dir returns empty → false "no errors" — prevented by machine-first-then-legacy fallback.

## Test Plan

- Update pinned-path unit tests: `observability.rs` (const values, category dirs), `boot_helpers.rs` (APP_LOG path segments / `data/` prefix), keep `loki_alloy_profile_guard.rs` + `claude_session_bootstrap_guard.rs` green (assertions are substring-based; verify).
- NEW test `test_all_machine_sink_dirs_live_under_machine_subdir` in `observability.rs`: every appender dir constant (`ERRORS_JSONL_DIR`, every `LogCategory::dir()`) starts with `data/logs/machine`.
- NEW test in `observability.rs`: `sweep_app_log_retention` skips `app.2026-07-05.log` (human daily) even when older than retention.
- Scoped runs: `cargo test -p tickvault-app --lib` (observability + boot_helpers), `cargo test -p tickvault-core --lib notification::summary_writer`, `cargo test -p tickvault-api`, plus the two `tickvault-common` guard tests. `cargo fmt --check` + clippy on touched crates.

## Rollback

Single revert of this PR restores every constant to `data/logs` top level; no data migration was performed (files were never moved), so rollback has zero data-loss risk. Legacy files still present at old paths keep working immediately after revert; files written under `machine/` during the window remain readable there and age out via mtime (or can be removed manually).

## Observability

- No new ErrorCode: no new `error!` emit site (sweep/init failures reuse existing WARN paths).
- Rule file `.claude/rules/project/observability-architecture.md` canonical-paths table updated with a dated note so future sessions + Alloy/Loki maintainers see the authoritative machine paths.
- `make doctor` logs section + `mcp-doctor.sh` updated to check the machine dir, so the zero-touch health chain keeps answering "is the ERROR stream alive?" truthfully.
- Alloy config tails both old + new `errors.jsonl` globs during the grace window — Loki/CloudWatch routing unbroken.

## Plan Items

- [x] Repoint machine sink constants (observability.rs, boot_helpers.rs, infra.rs)
  - Files: crates/app/src/observability.rs, crates/app/src/boot_helpers.rs, crates/app/src/infra.rs
  - Tests: test_all_machine_sink_dirs_live_under_machine_subdir, test_log_category_dir_and_prefix_stable_for_each_variant
- [x] Grace-window dual-dir retention sweep + human-log protection
  - Files: crates/app/src/main.rs, crates/app/src/observability.rs
  - Tests: test_sweep_app_log_retention_skips_human_daily_log
- [x] Debug API handler default dir
  - Files: crates/api/src/handlers/debug.rs
  - Tests: existing debug_spill_and_health_detail (TV_LOGS_DIR temp-dir based)
- [x] MCP server machine-dir with legacy fallback
  - Files: scripts/mcp-servers/tickvault-logs/server.py
  - Tests: n/a (Python; manual `_logs_dir` smoke via server main)
- [x] Automation consumers: Makefile, triage configs, hooks, doctor scripts, Alloy config, rule file
  - Files: Makefile, .claude/triage/error-rules.yaml, .claude/triage/claude-loop-prompt.md, .claude/hooks/error-triage.sh, .claude/hooks/session-context-brief.sh, .claude/hooks/session-sanity.sh, scripts/doctor.sh, scripts/mcp-doctor.sh, deploy/docker/alloy/alloy-config.alloy, .claude/rules/project/observability-architecture.md
  - Tests: loki_alloy_profile_guard, claude_session_bootstrap_guard (must stay green)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Fresh boot, empty data/logs | machine/ created; all robot files under machine/; top level stays human-only |
| 2 | Upgrade boot with legacy files at old paths | legacy files untouched at boot; swept by retention (48h/168h); MCP + Makefile fall back to legacy while machine/ is empty |
| 3 | Human app.<date>.log older than 168h in data/logs | NEVER deleted by the app sweeper (skip-*.log guard) |
| 4 | Alloy tails logs | both data/logs/errors.jsonl.* (legacy) and data/logs/machine/errors.jsonl.* ship to Loki |
| 5 | Revert of this PR | constants back to top level; no data loss (no move was ever performed) |

## Per-Item Guarantee Matrix

This plan cross-references the canonical 15-row + 7-row matrices in
`.claude/rules/project/per-wave-guarantee-matrix.md` (per its cross-reference
allowance). Item-specific notes: cold-path only (logging bootstrap +
hourly sweepers) — zero hot-path change, no new allocation on any tick path
(7-row "Never slow" row unaffected); no QuestDB schema or DEDUP-key change;
no new pub fn without test+call site; ratchet tests added for the machine-dir
prefix and the human-log sweep protection (15-row "extreme check" row).
