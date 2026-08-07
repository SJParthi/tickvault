---
paths:
  - ".claude/hooks/**"
  - ".claude/settings.json"
---

# Enforcement Architecture

> **Test scope default:** `.claude/rules/project/testing-scope.md` — tests run
> **only** for crates touched in the current diff. Workspace-wide execution is
> reserved for `/full-qa`, `FULL_QA=1`, `crates/common/` changes, or post-merge
> CI. This is the mechanically enforced default; do not escalate to the full
> workspace unless one of those triggers fires.

## Active Hooks (simplified until AWS deployment)
- **PreToolUse (Edit|Write):** block-env-files.sh — prevents .env file creation
- **PostToolUse (Edit|Write):** REMOVED — pre-commit gate enforces fmt; no per-edit rustfmt
- **SubagentStart:** REMOVED — principles already in CLAUDE.md context
- **Pre-push (8 fast gates via `.claude/hooks/pre-push-gate.sh`):** fmt, banned patterns, secrets, test count, data integrity, pub fn test guard, financial test guard, 22 test type check (scoped to changed crates). Heavy checks (clippy, test, audit, deny, loom) are CI-only.
- **Git hook pre-push (11 gates via `scripts/git-hooks/pre-push`):** Comprehensive — adds clippy, test, audit, deny, loom on top of the 7 fast gates.
- **Pre-PR (5 gates):** branch check, naming, clean tree, quality state, commit format
- **CI:** Full quality enforcement on PRs to main (fmt, clippy, test, security, coverage). Smart skip: heavy Rust steps (build/clippy/test) skipped when only config/scripts change.

## Testing Scope
- **Pre-push:** Full workspace validation (fmt, clippy, test)
- **CI:** Full quality enforcement on PRs to main (fmt, clippy, test, security, coverage)

## State File (.last-quality-pass)
- Written by pre-PR quality check on success
- Format: `<commit-hash> <unix-timestamp> <test-count>`
- Fresh = same HEAD + age < 5 minutes
- Gitignored (local machine state)

## Test Count Baseline (.test-count-baseline)
- Ratchet mechanism — count can only go UP
- Manual override only: `echo <count> > .claude/hooks/.test-count-baseline`
- Local machine state — genuinely gitignored AND untracked (was accidentally
  git-tracked until 2026-07-13 — untracked in this fix; gitignore never
  applied to already-tracked files, so a stale committed count dirtied the
  tree on every gate run on any machine whose committed baseline was below
  the live count)
- Auto-stamped by `test-count-guard.sh` on first run per machine; the ratchet
  applies per-machine from that point
- CI never runs this guard — a CI run would establish a fresh baseline and
  pass vacuously (merge-gate-lock-2026-07-04.md §3 row 6)

## Untested Pub Fn Baseline (.untested-pubfn-baseline)
- Ratchet mechanism — count can only go DOWN
- Every new pub fn must have a matching #[test] or "// TEST-EXEMPT: <reason>"
- Manual override: `echo <count> > .claude/hooks/.untested-pubfn-baseline`
- Local machine state — genuinely gitignored AND untracked (was accidentally
  git-tracked until 2026-07-13 — untracked in this fix; gitignore never
  applied to already-tracked files, so the stale committed count hard-blocked
  pre-push gate 6 over unrelated diffs on any machine whose committed
  baseline was below the live count)
- Auto-stamped by `pub-fn-test-guard.sh` (all mode) on first run per machine;
  the ratchet applies per-machine from that point
- CI never runs this guard — a CI run would establish a fresh baseline and
  pass vacuously (merge-gate-lock-2026-07-04.md §3 row 6)

### 2026-08-07 baseline RE-TRACKING — the false-OK fix (supersedes the untracking notes above and below)

The three ratchet baselines (`.test-count-baseline`, `.untested-pubfn-baseline`,
`.financial-test-baseline`) are **tracked in git again**, deliberately. This
reverses the 2026-07-13 untracking, and it fixes the reason that untracking was
survivable in the first place.

**The false-OK it closes.** Each guard, finding no baseline, WROTE one and
exited 0. Gitignored + CI checkout ⇒ no baseline ⇒ every CI run auto-established
a fresh one and passed. Three "gates" that could never fail. `merge-gate-lock-2026-07-04.md`
§3 row 6 and the two bullets above record this in writing. A ratchet whose
memory is erased before every run is not a ratchet.

**Why re-tracking is safe now — the 2026-07-13 breakage is fixed at its cause.**
That untracking happened because the guards **auto-wrote** the baseline on every
improvement, so a tracked file went dirty on any machine whose count differed,
and `git pull` refused. As of 2026-08-07 the guards **no longer write on
improvement**: they PASS, print the new value, and tell you to ratchet it in the
PR. The only remaining write is the local first-run bootstrap. Same discipline
as `quality/crate-coverage-thresholds.toml`, which has always been committed and
hand-ratcheted with a dated note. Ratchet movement is now PR-visible instead of
silently absorbed into a developer's working tree.

**Fail-closed in CI.** With `CI` set, a missing baseline is now `exit 1` with a
named error, not an auto-establish. Local first-run behaviour is unchanged.

**Honest envelope.** Committing the baselines makes the comparison real and the
guards CI-viable; it does **not** by itself wire them into the CI `Repo Guards`
job. They remain local-only today. Wiring them server-side is a deliberate
follow-up (it would start blocking PRs, which needs its own review) — this note
exists so nobody reads the re-tracking as having already delivered that.
Ratchet: `crates/common/tests/audit_fix_guard.rs`.

### 2026-07-13 baseline untracking — one-time migration + fail-closed notes
*(SUPERSEDED 2026-08-07 by the section above — retained as historical audit per
house convention. The migration steps below are obsolete: the baselines are
tracked again and the guards no longer auto-write them.)*
- **One-time migration:** after pulling the 2026-07-13 untracking commit, a
  machine whose local baseline was auto-bumped by the guard may see
  `git pull` refuse over these two files (`git stash` is banned): run
  `git checkout -- .claude/hooks/.test-count-baseline .claude/hooks/.untested-pubfn-baseline`
  then re-pull; the guards re-stamp per-machine on the next run.
- **Honest envelope:** on ephemeral containers the ratchet protects only
  within-session (first run auto-stamps); the cross-machine floor is
  deliberately not claimed — per merge-gate-lock-2026-07-04.md §3 row 6
  these guards are local-only.
- **Fail-closed behavior change (same fix):** pushes from a cwd that is not
  a git repo, or a repo without crates/, now BLOCK loudly (exit 2) instead
  of silently passing — a push of an intentionally non-tickvault repo must
  run outside the gated session.

## Financial Test Baseline (.financial-test-baseline)
- Ratchet mechanism — count can only go DOWN
- Financial functions (price/order/position) must have boundary/property tests
- Manual override: `echo <count> > .claude/hooks/.financial-test-baseline`
- Gitignored (local machine state)

## Data Integrity Guard
- Blocks f64::from(f32), `as f64` on prices, .round()/.floor()/.ceil() on prices
- Scope: crates/storage/, crates/core/src/pipeline/
- Exempt: "// DATA-INTEGRITY-EXEMPT: <reason>" or "// APPROVED:" on preceding line
- Runs at commit (staged files) and push (full workspace)

## Exit Codes
- 0 = PASS (allow the action)
- 2 = BLOCK (prevent the action, show errors on stderr)

## Auto-Save Remote (WIP Snapshot to GitHub)

Background daemon (`auto-save-remote.sh`) launched at SessionStart via `session-sanity.sh`.

- **Namespace:** `refs/auto-save/<branch-safe>-<session-id>/latest` + timestamped refs
- **Mechanism:** git plumbing (`commit-tree`, `write-tree`, `update-ref`) — zero disruption to working tree/index/HEAD
- **Push frequency:** Every 15 min (3rd snapshot cycle at 5-min intervals) + on daemon exit
- **Retention:** Latest ref (always) + last 3 timestamped refs (pruned beyond)
- **Cleanup:** Pre-push gate deletes auto-save refs on successful quality-gated push
- **Hook bypass:** Runs as background process (nohup), NOT through Claude Code tool pipeline — hooks don't fire by design
- **Recovery:** `.claude/hooks/recover-wip.sh` (list/diff/apply/restore/clean)
- **Orphan detection:** `session-sanity.sh` checks for remote auto-save refs on session start, warns if found

Worst-case data loss: ≤15 minutes (machine SIGKILL with no trap). Local stash layer (auto-save-watchdog.sh) still covers 2-minute intervals.

### Session Collision Detection
- **At startup:** `session-sanity.sh` fetches remote `refs/auto-save/*`, compares branch names, warns if another session is active on same branch with file overlap
- **Continuous:** `auto-save-remote.sh` scans for other sessions' refs on each push cycle. If file overlap detected, writes `.claude/hooks/.conflict-warning` marker
- **Resolution:** `recover-wip.sh --apply` to merge other session's work, or `recover-wip.sh --clean` to discard

## Rules
- Never suppress cargo output on failure — show errors so they can be fixed
- Never skip hooks via --no-verify (banned in settings.json deny list)
- Never use --admin on gh pr merge (blocked by deny rule AND hook)
- All hooks use stderr for output (stdout reserved for hook protocol)
