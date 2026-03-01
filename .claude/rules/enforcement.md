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

## Rules
- Never suppress cargo output on failure — show errors so they can be fixed
- Never skip hooks via --no-verify (banned in settings.json deny list)
- Never use --admin on gh pr merge (blocked by deny rule AND hook)
- All hooks use stderr for output (stdout reserved for hook protocol)
