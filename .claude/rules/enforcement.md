---
paths:
  - ".claude/hooks/**"
  - ".claude/settings.json"
---

# Enforcement Architecture

## Active Hooks (simplified until AWS deployment)
- **PreToolUse (Edit|Write):** block-env-files.sh — prevents .env file creation
- **PostToolUse (Edit|Write):** REMOVED — pre-commit gate enforces fmt; no per-edit rustfmt
- **SubagentStart:** REMOVED — principles already in CLAUDE.md context
- **Pre-push (7 fast gates):** fmt, banned patterns, secrets, test-count ratchet, data integrity, pub fn test, financial test (CI handles clippy/test/audit/deny/coverage/loom)
- **Pre-PR (5 gates):** branch check, naming, clean tree, quality state, commit format
- **CI:** Full quality enforcement on PRs to main (fmt, clippy, test, security, coverage). Smart skip: heavy Rust steps (build/clippy/test) skipped when only config/scripts change.

## State File (.last-quality-pass)
- Written by pre-PR quality check on success
- Format: `<commit-hash> <unix-timestamp> <test-count>`
- Fresh = same HEAD + age < 5 minutes
- Gitignored (local machine state)

## Test Count Baseline (.test-count-baseline)
- Ratchet mechanism — count can only go UP
- Manual override only: `echo <count> > .claude/hooks/.test-count-baseline`
- Gitignored (local machine state)

## Untested Pub Fn Baseline (.untested-pubfn-baseline)
- Ratchet mechanism — count can only go DOWN
- Every new pub fn must have a matching #[test] or "// TEST-EXEMPT: <reason>"
- Manual override: `echo <count> > .claude/hooks/.untested-pubfn-baseline`
- Gitignored (local machine state)

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
