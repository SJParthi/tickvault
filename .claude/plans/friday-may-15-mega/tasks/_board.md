# Task Board — Friday May 15 Mega Plan

> **Live board** for 5+ parallel Claude Code sessions executing tasks
> from this workspace. Each row = one task file in `tasks/`.
> Updated whenever a task changes status. Sort: Status group → ID.

**Last refreshed:** 2026-05-11 20:50 IST (initial creation, no tasks yet)

---

## Status legend

| Status | Meaning |
|---|---|
| **DRAFT** | task being written by operator + Claude during Mon-Wed planning |
| **AVAILABLE** | task is published and ready — any session can claim |
| **CLAIMED** | a session committed `Owner` + `ClaimedAt` to the task file |
| **IN_PROGRESS** | claimed session has pushed at least one commit to its task branch |
| **REVIEW** | session opened a draft PR; waiting for operator approval to merge |
| **DONE** | PR merged to main; downstream tasks unblocked |
| **BLOCKED** | `DependsOn` not yet `DONE`; cannot start |
| **STALE** | `CLAIMED` > 30 min ago with no commits — sweeper auto-releases |

---

## The board

| ID | Title | Status | Owner | Deps | Branch | ETA |
|---|---|---|---|---|---|---|
| _(no tasks defined yet — planning underway)_ | | | | | | |

---

## Lifecycle (read before claiming a task)

```
1. git pull origin claude/trading-tick-vault-BkvpS
2. read THIS file → pick an AVAILABLE task with no unmet Deps
3. open tasks/T<NN>-*.md → read in full
4. EDIT the task file header:
     Status: CLAIMED
     Owner: claude-sess-<your-unique-id>
     ClaimedAt: <iso-8601 now>
5. git add tasks/T<NN>-*.md && git commit + push IMMEDIATELY
   (this is the atomic claim — if push fails, someone beat you, abandon)
6. git checkout -b <task's Branch field value>
7. Implement task body. Push frequently to your task branch.
8. Edit task header: Status: IN_PROGRESS (after first impl commit)
9. When done: open draft PR via mcp__github__create_pull_request
   → edit task: Status: REVIEW
10. After operator merges PR to main → edit task: Status: DONE
11. Update _board.md row + any DependsOn dependents that now unblock
```

## Stale-claim recovery (any session can run)

```
For each task with Status=CLAIMED|IN_PROGRESS:
  if (now - ClaimedAt) > 30 minutes AND
     no commits pushed to its Branch:
    → flip Status: STALE
    → clear Owner
    → log to decisions-log
  After 60 minutes total stale → flip back to AVAILABLE
```

(Currently manual — automated sweeper script will be added if we end
up with > 3 parallel sessions in practice.)

## Parallel-session etiquette

- One session = one task at a time. No multi-claiming.
- Sessions DO NOT edit each other's task files except the
  stale-claim recovery flow above.
- Sessions DO update `_board.md` (the row for their task) when their
  status changes.
- All session work happens on the task's own branch — never push
  task work to `claude/trading-tick-vault-BkvpS` (the planning branch).
- Planning branch is for: this directory ONLY (planning artifacts).
- Task branches are for: code changes per the task spec.
