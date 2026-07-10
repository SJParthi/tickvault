# Implementation Plan: fix audit DEDUP `ts` + close the meta-guard multi-line blind spot

**Status:** VERIFIED (operator AskUserQuestion 2026-06-23: "Fix it now")
**Date:** 2026-06-23
**Branch:** `claude/fix-audit-dedup-ts-and-scanner` (off origin/main).

## Design

Two cross_verify audit tables omit the designated timestamp `ts` from their
`DEDUP UPSERT KEYS(...)`. QuestDB **requires** the designated timestamp in the
DEDUP key (documented: `dedup_segment_meta_guard.rs` cites the 2026-05-18
production HTTP-400 incident). The violation was hidden because the meta-guard's
`collect_dedup_key_declarations` scanner only reads SINGLE-LINE consts, and both
constants are rustfmt-wrapped across two lines.

Fix = (a) make the scanner multi-line-aware so the guard can SEE these consts
(turns the latent bug into a build failure that can never hide again), and
(b) prepend `ts` to both constants + add a `DEDUP ENABLE` self-heal so existing
on-disk tables get the corrected key.

**Dedup semantics (honest):** `run_ts_ist_nanos` is the run instant. With `ts`
in the key, two runs on the same day keep DISTINCT rows (accumulate per run) —
acceptable for a once-a-day forensic audit; WITHIN one run, identical
`(date, sid, segment, minute, field)` cells collapse (ts constant for that run),
which is strictly better than today's no-dedup state. (The prior agent's
"latest run wins" claim was WRONG and is NOT relied upon.)

## Plan Items

- [x] **F1 — multi-line-aware scanner** in `crates/storage/tests/dedup_segment_meta_guard.rs`
  - Make `collect_dedup_key_declarations` find the `"..."` body on a continuation
    line; switch the per-feed test back to the shared collector; remove the
    now-redundant `find_dedup_key_body` helper.
- [x] **F2 — prepend `ts` to both cross_verify DEDUP keys**
  - `crates/storage/src/cross_verify_1m_audit_persistence.rs` (`DEDUP_KEY_CROSS_VERIFY_1M_AUDIT`)
  - `crates/storage/src/groww_cross_verify_audit_persistence.rs` (`DEDUP_KEY_GROWW_CROSS_VERIFY_1M_AUDIT`)
- [x] **F3 — DEDUP ENABLE self-heal** in both `ensure_*` fns (column already exists;
  re-apply key with `ts` for tables created with the old key). Never drops a table.
- [x] **F4 — update exact-string DEDUP tests** in both persistence files.

## Edge Cases
- Existing table created with old key → `DEDUP ENABLE UPSERT KEYS(ts, …)` re-applies in place (no drop, SEBI-safe).
- Fresh table → CREATE DDL now interpolates the corrected const → valid first time.

## Failure Modes
- The `every_audit_dedup_key_must_include_designated_timestamp_ts` guard now SEES
  the multi-line consts → fails the build until F2 lands (intended; both done in this PR).
- Live QuestDB verification PENDING (Docker offline in this env) — documented honestly.

## Test Plan
- `cargo test -p tickvault-storage --test dedup_segment_meta_guard` (all guards green, multi-line now scanned)
- `cargo test -p tickvault-storage --lib cross_verify_1m_audit groww_cross_verify`
- `cargo clippy -p tickvault-storage -- -D warnings` + `cargo fmt --check`
- 3-agent adversarial review on the diff (charter §E).

## Rollback
- Single feature branch; `git revert`. DEDUP-key flips are idempotent + in-place.

## Observability
- No new metric. Fix restores correct DEDUP on two forensic audit tables.

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | fresh boot | both tables CREATE successfully with `ts` in the key |
| 2 | existing table (old key) | `DEDUP ENABLE` self-heal re-applies the `ts` key in place |
| 3 | meta-guard | multi-line consts are now scanned; missing `ts` fails the build |
