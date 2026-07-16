# Implementation Plan: CI Gate Integrity — scanner quoting + honest timeouts (PR-A)

**Status:** IN_PROGRESS
**Date:** 2026-07-16
**Approved by:** pending (operator approval was relayed via a coordinator
session on 2026-07-16; per the user-intent rule a relayed message does not
establish direct approval — the operator must confirm in-session to flip
this to APPROVED)

> Active-plan cap check (design-first-wall V7): 4 existing active-plan files +
> this one = 5, exactly at `PLAN_GATE_MAX_ACTIVE` (≤5 passes).

## Design

The 2026-07-16 gate-integrity sweep found the CI `Repo Guards` job and the
local pre-push/pre-commit gates were structurally vacuous or dishonest:

1. **CI quoting bug (Critical):** `ci.yml` built `ALL_RS` with `tr '\n' ' '`
   and passed it UNQUOTED to `banned-pattern-scanner.sh` /
   `dedup-latency-scanner.sh`. The scanners' contract is `$2` = ONE
   newline-separated file-list string iterated via a `while read`
   here-string — word-splitting meant only the FIRST file reached the
   scanner as `$2`: CI scanned 1 of ~551 `.rs` files. Fix: newline-preserving
   `find | sort`, quoted `"$ALL_RS"`, a FATAL empty-list guard (no vacuous
   pass), and a "Scanning N Rust files" evidence line. `timeout-minutes`
   10 → 15 for the now-real scans.
2. **pre-push-gate.sh Gate 2 (banned patterns):** the same space-joined form
   made the scanner see one bogus mega-filename → ZERO files scanned, which
   is why the fixed 60s timeout "worked". Fix: newline list + empty-list
   FAIL + scaled timeout `60 + RS_COUNT/2` (measured ~0.3s/file worst-case;
   2x headroom), timeout value echoed in the FAIL message.
3. **pre-push-gate.sh Gate 3 (secret scan):** full-tree scan measured
   ~4m19s/551 files vs a fixed 30s timeout. Fix:
   `${TICKVAULT_SECRET_SCAN_TIMEOUT_SECS:-60} + RS_COUNT/2`, hard floor 60s,
   findings always block.
4. **pre-commit-gate.sh Gate 5 (secret scan):** same knob +
   `STAGED_COUNT/8` scaling (staged sets are small; the full-tree pre-push
   lane uses /2), hard floor 60s, computed timeout echoed in the exit-124
   FAIL message. No skip path — exit 124 and findings both block.

Files: `.github/workflows/ci.yml`, `.claude/hooks/pre-push-gate.sh`,
`.claude/hooks/pre-commit-gate.sh`. No `crates/` source is touched; no gate
is weakened — every change makes a previously-vacuous or
guaranteed-to-time-out scan real and honestly bounded.

## Edge Cases

- Empty file list (`find` returns nothing): CI step exits 1 FATAL; pre-push
  Gate 2 FAILs — a repo layout change can never produce a vacuous green.
- Filenames are repo-tracked `crates/**/*.rs` paths (no spaces/newlines in
  this repo; the scanners' here-string iteration handles arbitrary counts).
- `TICKVAULT_SECRET_SCAN_TIMEOUT_SECS` unset → default 60; set below 60 →
  the hard floor clamps back to 60 (the knob can extend, never shrink below
  the historical baseline).
- Zero staged files in pre-commit Gate 5 → `STAGED_COUNT=0` → timeout 60s
  (identical to today).
- `sort` makes the scan order deterministic across environments.
- Arithmetic is POSIX `$(( ))` integer math — no bc/awk dependency.

## Failure Modes

- Scanner script missing/non-executable: pre-existing `[ -x ]` guards keep
  their behavior (unchanged by this PR).
- Genuine timeout at the new scaled bound: FAIL with the computed timeout
  value in the message — loud, never a skip.
- A hostile/very-large tree inflates `RS_COUNT`: the timeout grows linearly
  and boundedly (~count/2 seconds); CI job cap 15 min is the outer bound.
- Findings (exit ≠ 0, ≠ 124): block exactly as before — no new pass path
  was added anywhere.

## Test Plan

- `bash -n` on both hook scripts (syntax).
- YAML validity check on `.github/workflows/ci.yml`.
- Replicated Gate-2 run: execute `banned-pattern-scanner.sh` with the FIXED
  newline-quoted list against the known pre-#1598 baseline — expect the
  scanner to actually iterate all ~551 files and report the known 29
  violations on that base (proving the old form scanned ~0 and the new form
  scans everything).
- Arithmetic stubs: evaluate the `SECRET_TIMEOUT` / `BANNED_TIMEOUT`
  expressions with representative counts (551 → 335s; staged 284 → 95s;
  staged 10 → 61s / floor cases) in a throwaway shell.
- `bash .claude/hooks/plan-gate.sh` — passes (no `crates/*/src/*.rs`
  changed; plan count at cap 5).

## Rollback

`git revert` of the single commit restores the prior ci.yml + hook scripts
byte-for-byte. No schema, config, runtime, or data-path change — the blast
radius is CI + local hooks only. Reverting re-opens the vacuous-scan hole,
so a revert must be paired with an issue re-flagging the Critical finding.

## Observability

- CI: the "Scanning N Rust files" line in the Repo Guards log is the
  per-run evidence that the scan is non-vacuous (N ≈ 551, never 1).
- Local: FAIL messages now carry the computed timeout
  (`timeout ${BANNED_TIMEOUT}s` / `timed out (${SECRET_TIMEOUT}s)`), so a
  timeout is attributable to the actual bound, not a mystery 30/60s.
- The empty-list FATAL lines are grep-able sentinels
  (`refusing vacuous pass`, `banned-pattern scan cannot run`).

## Plan Items

- [x] ci.yml Repo Guards quoting fix + empty-list guard + evidence line + 15-min cap
  - Files: .github/workflows/ci.yml
  - Tests: YAML validity check, replicated Gate-2 run (29 violations on pre-#1598 base)
- [x] pre-push-gate.sh Gate 2 newline list + empty-list FAIL + scaled timeout
  - Files: .claude/hooks/pre-push-gate.sh
  - Tests: bash -n, arithmetic stubs
- [x] pre-push-gate.sh Gate 3 env knob + count/2 scaling + 60s floor
  - Files: .claude/hooks/pre-push-gate.sh
  - Tests: bash -n, arithmetic stubs
- [ ] pre-commit-gate.sh Gate 5 env knob + staged/8 scaling + 60s floor + computed value in the exit-124 message
  - Files: .claude/hooks/pre-commit-gate.sh
  - Tests: bash -n, arithmetic stubs
  - NOTE: edit NOT applied — blocked by the auto-mode permission classifier
    (Self Modification of a hooks file); requires direct in-session user
    approval before it can land.
- [x] Review round-1 fixes: push-scoped local scans (harness hook budget), sibling callers (scripts/git-hooks/pre-push, pre-pr-gate.sh), timeout-vs-findings messages, honesty comments
  - Files: .claude/hooks/pre-push-gate.sh, scripts/git-hooks/pre-push, .claude/hooks/pre-pr-gate.sh, CLAUDE.md
  - Tests: bash -n, push-range list-build replication, single-file scanner invocation

## Per-Item Guarantee Matrix

See `per-wave-guarantee-matrix.md` (`.claude/rules/project/`) — all 15 rows of
the 100% Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply
to every item in this plan. Plan-specific notes:

- This plan changes CI/hook shell only (no `crates/` code): the hot-path,
  DHAT/Criterion, and audit-table rows read `N/A — CI/hook shell change, no
  runtime code path touched`; the "100% code checks" row is the DELIVERABLE
  itself (the scanner now genuinely scans every file instead of 1).
- Resilience rows: no tick-drop path, no WS/QuestDB path, and no DEDUP key is
  touched; the change strictly STRENGTHENS enforcement (a previously-vacuous
  gate now blocks real findings) and weakens nothing.
