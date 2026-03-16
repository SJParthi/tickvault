# Plan Enforcement Rules

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** Any multi-file implementation task (3+ files or 3+ logical changes).

## Purpose

Prevents the exact failure mode where a plan is approved but items are silently
skipped during implementation. Every approved plan item MUST be implemented and
verified before the work is declared complete.

## Workflow (Mechanical — No Exceptions)

### Phase 1: Plan

1. **Write the plan** to `.claude/plans/active-plan.md` using the exact format below.
2. **Set Status: DRAFT**.
3. **Present the plan to the user.** Do NOT start implementation until approval.
4. **On user approval**, set Status: APPROVED.

### Phase 2: Implement

5. **Implement each item.** As each item is completed:
   - Check it off: `- [ ]` → `- [x]`
   - Add the implementation reference (file:line or function name)
6. **Set Status: IN_PROGRESS** when starting.

### Phase 3: Verify

7. **Before declaring "done"**, run the plan verification:
   ```
   bash .claude/hooks/plan-verify.sh
   ```
8. The script checks:
   - All plan items are `[x]` (no unchecked `[ ]` items)
   - All listed tests exist in the codebase (grep for test function names)
   - All listed files were actually modified (git diff check)
9. **If verification passes**, set Status: VERIFIED.
10. **If verification fails**, fix the gaps before proceeding.

### Phase 4: Complete

11. Pre-push gate checks: if active plan exists, Status MUST be VERIFIED.
12. After successful push, archive the plan:
    - Move to `.claude/plans/archive/YYYY-MM-DD-<slug>.md`
    - Remove `.claude/plans/active-plan.md`

## Plan File Format

```markdown
# Implementation Plan: <Short Title>

**Status:** DRAFT | APPROVED | IN_PROGRESS | VERIFIED
**Date:** YYYY-MM-DD
**Approved by:** <user name or "pending">

## Plan Items

- [ ] <Item 1 description>
  - Files: file1.rs, file2.rs
  - Tests: test_name_1, test_name_2

- [ ] <Item 2 description>
  - Files: file3.rs
  - Tests: test_name_3

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | ... | ... |
```

## Rules

1. **Every implementation task with 3+ changes MUST have a plan file.**
   Skip only for trivial single-file fixes.

2. **Plan items MUST list files and tests.**
   "Add idempotency guard" is not enough.
   "Add idempotency guard — Files: candle_fetcher.rs, main.rs — Tests: test_idempotency_query_response_parsing" is correct.

3. **Do NOT declare implementation complete until plan-verify.sh passes.**
   "All tests pass" is necessary but NOT sufficient.
   The plan itself must be verified item-by-item.

4. **Unchecked items block push.**
   Pre-push gate 14 checks for unchecked plan items.
   No override. Fix the gaps or explicitly remove the item with user approval.

5. **User can add/remove items during implementation.**
   Update the plan file. New items start unchecked.
   Removed items require user confirmation (not silent deletion).

6. **Archive after push, not before.**
   The active plan persists until the push succeeds.

## What This Prevents

- Approved plan items silently skipped during implementation
- "Implementation complete" without verifying every plan item
- Missing tests that were promised in the plan
- Files that were supposed to be modified but weren't touched

## Trigger

This rule activates for ALL implementation tasks. It is always loaded.
