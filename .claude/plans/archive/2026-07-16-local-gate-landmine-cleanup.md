# Implementation Plan: Local-Gate Landmine Cleanup (comment-only exemption markers)

**Status:** DRAFT
**Date:** 2026-07-16
**Approved by:** pending (operator asked in-session 2026-07-16; coordinator-routed task — the coordinator relayed "approve plan and approve hook fix", but this worker's permission layer requires the main session / operator to flip this status directly)

> **Scope note (honest):** the "approve hook fix" half of the relayed approval
> (the `.claude/hooks/pre-commit-gate.sh` secret-scan timeout knob) was REFUSED by
> this worker session's permission layer (self-modification boundary: the original
> task message excluded `.claude/hooks/`); the hook fix is NOT in this diff and
> remains with the main session.
>
> **Plan-count note (honest):** 5 other `active-plan*.md` files exist; this file makes
> 6, which EXCEEDS the plan-gate V7 cap (`PLAN_GATE_MAX_ACTIVE = 5`) — plan-gate will
> block at push time on the count alone (and on Status: DRAFT) until the main session
> archives one and flips this to APPROVED. Recorded, not faked green.

## Design

Comment-ONLY exemption cleanup of the 47 inherited local pre-commit-gate landmines
(30 banned-pattern-scanner + 17 dedup-latency-scanner hits) enumerated in the
verified inventory (scratchpad `landmine-inventory.md`, HEAD `4f11240c`). All 47
were classified (a): legitimately cold-path / boot / midnight-rollover / test /
fuzz / doc-comment false positives inherited from merged PRs #1585/#1578 — they
passed CI (whose file-list plumbing scans effectively nothing) and detonate only
on the LOCAL pre-commit gate when a future merge re-stages one of the files.
Zero logic changes: every edit is a trailing/preceding exemption comment, a
`//!` doc-line reword, or an `O(1) EXEMPT: begin/end` block around a test module.

Touched crates/paths:
- `crates/core/src/pipeline/` — `feed_presence.rs`, `prev_oi_cache.rs`,
  `feed_lag_monitor.rs`, `prev_day_close_stamper.rs`, `volume_delta_tracker.rs`,
  `prev_close_writer.rs`
- `crates/storage/src/` — `ws_frame_spill.rs`, `partition_archive.rs`
- `crates/trading/src/oms/groww/` — `api_client.rs`, `executor.rs`, `poll_tiers.rs`
- `fuzz/fuzz_targets/ws_frame_wal_replay.rs`

## Edge Cases

- **Per-scanner marker syntax differs:** the banned-pattern scanner honors
  same-line trailing `// APPROVED:` (and `// O(1) EXEMPT:`); the dedup-latency
  scanner does NOT honor same-line `// APPROVED:` — its hits use same-line
  `// O(1) EXEMPT:` or a standalone preceding `// O(1) EXEMPT:` / `// APPROVED:`
  line, or the `O(1) EXEMPT: begin`/`end` block form.
- **rustfmt comment relocation:** the on-save formatter moves trailing comments
  off `#[allow(...)]` attributes (scanner handles this via the buffered-attr
  retro-approval) and off `let-else` / `match` heads INTO the block body (scanner
  does NOT handle that) — those sites use standalone PRECEDING marker lines.
- **Test-module leak in the dedup scanner:** its weaker `#[cfg(test)]`-strip awk
  is cancelled by a no-brace `#[allow]` line between the attr and `mod tests {`;
  fixed comment-only with an `O(1) EXEMPT: begin — test-only module` /
  `// O(1) EXEMPT: end` block around the whole tests module
  (`prev_close_writer.rs`; strategy/tests.rs precedent).
- **`//!` doc-line false positive:** the dedup scanner filters `/// ` but not
  `//!`, so the `prev_close_writer.rs` module doc mentioning `std::fs::write`
  literally was reworded ("synchronous filesystem write + rename").
- **Hardcoded-URL const twin:** `api_client.rs:42` takes the MARKER route (a
  preceding `// APPROVED:` comment) rather than an import refactor — the
  constants.rs twin is already blessed; Gate-5 of `groww_order_lattice_guard.rs`
  is untouched (path literals stay confined to api_client.rs).

## Failure Modes

- A marker on a line that LATER becomes hot-path would mask a real violation —
  mitigated by specific per-line reasons naming the cold context (constructor /
  boot replay / midnight rollover / scoreboard run / test-only), so a reviewer
  moving the code sees a reason that no longer holds.
- A begin/end block that drifts (end marker deleted) would exempt trailing
  production code — bounded here: the block wraps only the final `#[cfg(test)]`
  module at end-of-file.
- The formatter re-moving a same-line marker into a non-honored position — every
  scanner was re-run tree-wide AFTER the formatter passes (Test Plan below).

## Test Plan

Tree-wide scanner re-runs (the same commands the inventory used), all must exit 0:

```bash
git ls-files '*.rs' > /tmp/rs.txt
bash .claude/hooks/banned-pattern-scanner.sh "$PWD" "$(cat /tmp/rs.txt)"   # EXIT=0
bash .claude/hooks/dedup-latency-scanner.sh  "$PWD" "$(cat /tmp/rs.txt)"   # EXIT=0 (warnings ok)
bash .claude/hooks/data-integrity-guard.sh   "$PWD" "$(cat /tmp/rs.txt)"   # EXIT=0
bash .claude/hooks/secret-scanner.sh "$PWD" "$(git ls-files)"              # EXIT=0
cargo fmt --all -- --check                                                 # EXIT=0
```

Plus the REAL end-to-end verification: the full pre-commit hook battery fires on
the commit itself (fmt / banned / data-integrity / dedup / secrets / pinning /
commit-msg / typos / invariant tests) — the commit landing IS the gate proof.
Comment-only changes are compile-safe by construction; `cargo check` on the
touched crates run if disk allows.

## Rollback

Revert the single commit (`git revert <sha>`). There is zero runtime behavior
change — every edit is a comment or doc-line reword — so rollback risk is nil;
reverting merely re-arms the local-gate landmines.

## Observability

N/A — comment-only, zero runtime delta: no metric, log line, alert, or table is
added or changed. The pre-commit hooks themselves ARE the observability surface
for this change (they now pass tree-wide instead of false-blocking).

## Plan Items

- [x] Banned-pattern exemptions (30 hits) — same-line/preceding `// APPROVED:` markers
  - Files: crates/core/src/pipeline/feed_presence.rs, crates/core/src/pipeline/prev_oi_cache.rs,
    crates/core/src/pipeline/feed_lag_monitor.rs, crates/core/src/pipeline/prev_day_close_stamper.rs,
    crates/core/src/pipeline/volume_delta_tracker.rs, crates/storage/src/ws_frame_spill.rs,
    crates/storage/src/partition_archive.rs, crates/trading/src/oms/groww/api_client.rs,
    crates/trading/src/oms/groww/executor.rs, crates/trading/src/oms/groww/poll_tiers.rs,
    fuzz/fuzz_targets/ws_frame_wal_replay.rs
  - Tests: tree-wide `banned-pattern-scanner.sh` run (EXIT=0), pre-commit gate on the commit
- [x] Dedup-latency exemptions (17 hits) — `// O(1) EXEMPT:` markers + test-mod block + doc reword
  - Files: crates/storage/src/ws_frame_spill.rs, crates/core/src/pipeline/prev_close_writer.rs
  - Tests: tree-wide `dedup-latency-scanner.sh` run (EXIT=0), pre-commit gate on the commit
- [x] Verify the untouched gates stay green (data-integrity, secrets, fmt)
  - Files: (no additional files)
  - Tests: `data-integrity-guard.sh`, `secret-scanner.sh`, `cargo fmt --all -- --check`

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row +
7-row matrices apply by cross-reference, as that rule explicitly allows). This is a
COMMENT-ONLY change with zero runtime behavior delta, so:

- **Applicable rows:** 100% code checks (the banned-pattern + dedup-latency +
  data-integrity + secret scanners re-run tree-wide, all green — the scanners ARE
  the tests for this diff) and 100% code review (adversarial marker-placement
  re-verification in-session after each formatter pass).
- **All other rows (coverage / audit / performance / monitoring / logging /
  alerting / security hardening / scenarios / functionalities / resilience
  7-row):** N/A — no code path, metric, table, alert, or hot-path instruction
  changed; no new pub fn; no new failure mode.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Future merge re-stages any of the 11 files | Local pre-commit gates pass (no inherited false-block) |
| 2 | A marked line later moves onto the hot path | The specific per-line reason no longer matches — reviewer catches it |
| 3 | Revert of this commit | Zero runtime change; landmines re-arm (accepted) |
