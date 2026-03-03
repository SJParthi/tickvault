---
paths:
  - ".claude/hooks/**"
  - ".claude/settings.json"
---

# Enforcement Architecture

## Hook Pyramid (layered, not duplicated)
- **Pre-commit (8 gates):** FULL quality — fmt, clippy, test, banned, O(1), secrets, version pin, test count
- **Pre-push (7 gates):** VERIFY + security — fmt, clippy, test, banned, test count, audit, deny
- **Pre-PR (5 gates):** VERIFY state + conventions — uses state file to skip redundant checks
- **Pre-merge (3 gates):** CI status — blocks --admin, verifies CI passed via API

## State File (.last-quality-pass)
- Written by pre-commit and pre-push on success
- Read by pre-PR to avoid triple-testing
- Format: `<commit-hash> <unix-timestamp> <test-count>`
- Fresh = same HEAD + age < 5 minutes
- Gitignored (local machine state)

## Test Count Baseline (.test-count-baseline)
- Ratchet mechanism — count can only go UP
- Manual override only: `echo <count> > .claude/hooks/.test-count-baseline`
- Gitignored (local machine state)

## Exit Codes
- 0 = PASS (allow the action)
- 2 = BLOCK (prevent the action, show errors on stderr)

## Auto-Save Remote (WIP Snapshot to GitHub)

Background daemon (`auto-save-remote.sh`) launched at SessionStart via `session-sanity.sh`.

- **Namespace:** `refs/auto-save/<branch-safe>-<session-id>/latest` + timestamped refs
- **Mechanism:** git plumbing (`commit-tree`, `write-tree`, `update-ref`) — zero disruption to working tree/index/HEAD
- **Push frequency:** Every 6 min (3rd snapshot cycle) + on daemon exit
- **Retention:** Latest ref (always) + last 3 timestamped refs (pruned beyond)
- **Cleanup:** Pre-push gate deletes auto-save refs on successful quality-gated push
- **Hook bypass:** Runs as background process (nohup), NOT through Claude Code tool pipeline — hooks don't fire by design
- **Recovery:** `.claude/hooks/recover-wip.sh` (list/diff/apply/restore/clean)
- **Orphan detection:** `session-sanity.sh` checks for remote auto-save refs on session start, warns if found

Worst-case data loss: ≤6 minutes (machine SIGKILL with no trap).

## Rules
- Never suppress cargo output on failure — show errors so they can be fixed
- Never skip hooks via --no-verify (banned in settings.json deny list)
- Never use --admin on gh pr merge (blocked by deny rule AND hook)
- All hooks use stderr for output (stdout reserved for hook protocol)
