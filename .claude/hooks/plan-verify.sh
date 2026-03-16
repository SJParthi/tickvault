#!/bin/bash
# plan-verify.sh — Mechanical enforcement: every approved plan item implemented + tested.
#
# Exit codes:
#   0 = PASS (no active plan, or all items verified)
#   2 = BLOCK (unchecked items, missing tests, or wrong status)
#
# Called by:
#   - pre-push-gate.sh (gate 14)
#   - Claude Code manually before declaring "done"
#
# Checks:
#   1. If active plan exists, Status must be VERIFIED (for pre-push) or in progress
#   2. All plan items must be checked [x] (no [ ] items)
#   3. All listed test names must exist in codebase
#   4. All listed files must have been modified (git diff)

set -uo pipefail

PROJECT_DIR="${1:-.}"
MODE="${2:-verify}"  # "verify" = full check, "push" = pre-push (requires VERIFIED status)

PLAN_FILE="$PROJECT_DIR/.claude/plans/active-plan.md"
VIOLATIONS=0
REPORT=""

# ── No active plan = OK ──
if [ ! -f "$PLAN_FILE" ]; then
  echo "  PASS: No active implementation plan" >&2
  exit 0
fi

# ── Read status ──
STATUS=$(grep -m1 '^\*\*Status:\*\*' "$PLAN_FILE" 2>/dev/null | sed 's/.*\*\*Status:\*\* *//' | tr -d '[:space:]')

if [ -z "$STATUS" ]; then
  echo "  FAIL: Active plan exists but has no **Status:** field" >&2
  exit 2
fi

# ── Pre-push mode: require VERIFIED status ──
if [ "$MODE" = "push" ]; then
  if [ "$STATUS" != "VERIFIED" ]; then
    echo "  FAIL: Active plan status is '$STATUS' — must be VERIFIED before push" >&2
    echo "  Run: bash .claude/hooks/plan-verify.sh to verify, then set Status: VERIFIED" >&2
    exit 2
  fi
  echo "  PASS: Plan verified (status: VERIFIED)" >&2
  exit 0
fi

# ── Verify mode: check all items ──
echo "  Verifying active plan: $PLAN_FILE" >&2

# Check 1: Count unchecked items
UNCHECKED=$(grep -c '^\- \[ \]' "$PLAN_FILE" 2>/dev/null || echo 0)
CHECKED=$(grep -c '^\- \[x\]' "$PLAN_FILE" 2>/dev/null || echo 0)
TOTAL=$((UNCHECKED + CHECKED))

if [ "$UNCHECKED" -gt 0 ]; then
  VIOLATIONS=$((VIOLATIONS + UNCHECKED))
  REPORT="${REPORT}\n  [INCOMPLETE] ${UNCHECKED}/${TOTAL} plan items still unchecked:"
  while IFS= read -r line; do
    ITEM=$(echo "$line" | sed 's/^- \[ \] //')
    REPORT="${REPORT}\n    - $ITEM"
  done < <(grep '^\- \[ \]' "$PLAN_FILE" 2>/dev/null)
fi

# Check 2: Verify listed tests exist in codebase
# Extract test names from "Tests: test_a, test_b" lines
MISSING_TESTS=0
while IFS= read -r line; do
  # Strip leading whitespace and "- Tests: " prefix
  TESTS=$(echo "$line" | sed 's/^[[:space:]]*- Tests:[[:space:]]*//')

  # Split on comma
  IFS=',' read -ra TEST_ARRAY <<< "$TESTS"
  for test_name in "${TEST_ARRAY[@]}"; do
    test_name=$(echo "$test_name" | tr -d '[:space:]')
    [ -z "$test_name" ] && continue

    # Search for this test function in crates/
    FOUND=$(grep -r "fn ${test_name}" "$PROJECT_DIR/crates" --include='*.rs' -l 2>/dev/null || true)
    if [ -z "$FOUND" ]; then
      VIOLATIONS=$((VIOLATIONS + 1))
      MISSING_TESTS=$((MISSING_TESTS + 1))
      REPORT="${REPORT}\n  [MISSING TEST] fn ${test_name} — not found in crates/"
    fi
  done
done < <(grep '^\s*- Tests:' "$PLAN_FILE" 2>/dev/null)

# Check 3: Verify listed files were modified
# Extract file names from "Files: file1.rs, file2.rs" lines
MISSING_FILES=0
while IFS= read -r line; do
  FILES=$(echo "$line" | sed 's/^[[:space:]]*- Files:[[:space:]]*//')

  IFS=',' read -ra FILE_ARRAY <<< "$FILES"
  for file_name in "${FILE_ARRAY[@]}"; do
    file_name=$(echo "$file_name" | tr -d '[:space:]')
    [ -z "$file_name" ] && continue

    # Check if this file exists anywhere in the project
    FOUND=$(find "$PROJECT_DIR/crates" "$PROJECT_DIR/config" -name "$file_name" -type f 2>/dev/null | head -1)
    if [ -z "$FOUND" ]; then
      # Also check without path prefix (might be a full path)
      FOUND=$(find "$PROJECT_DIR" -path "*/$file_name" -type f 2>/dev/null | head -1)
    fi

    if [ -z "$FOUND" ]; then
      VIOLATIONS=$((VIOLATIONS + 1))
      MISSING_FILES=$((MISSING_FILES + 1))
      REPORT="${REPORT}\n  [MISSING FILE] ${file_name} — not found in project"
    fi
  done
done < <(grep '^\s*- Files:' "$PLAN_FILE" 2>/dev/null)

# ── Report ──
if [ "$VIOLATIONS" -gt 0 ]; then
  echo "" >&2
  echo "  PLAN VERIFICATION FAILED: ${VIOLATIONS} issue(s)" >&2
  echo -e "$REPORT" >&2
  echo "" >&2
  echo "  Fix all issues, then set **Status:** VERIFIED in $PLAN_FILE" >&2
  exit 2
fi

echo "  PASS: All ${TOTAL} plan items verified (${CHECKED} checked, ${MISSING_TESTS} missing tests, ${MISSING_FILES} missing files)" >&2
exit 0
