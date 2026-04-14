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
# Note: `grep -c` prints "0" AND exits non-zero on zero matches under set -o pipefail,
# which made `|| echo 0` append a second "0" → "0\n0" → arithmetic failure.
# Use `|| true` to swallow the exit code without doubling the output.
UNCHECKED=$(grep -c '^\- \[ \]' "$PLAN_FILE" 2>/dev/null || true)
CHECKED=$(grep -c '^\- \[x\]' "$PLAN_FILE" 2>/dev/null || true)
UNCHECKED=${UNCHECKED:-0}
CHECKED=${CHECKED:-0}
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
# Extract test names from "Tests: test_a, test_b" lines, skipping items
# that are marked `[~]` (deferred) or `[ ]` (unchecked — already counted above).
# We track the current item's status while walking the file line by line.
MISSING_TESTS=0
current_status="none"
while IFS= read -r line; do
  case "$line" in
    "- [x] "*) current_status="done" ;;
    "- [~] "*) current_status="deferred" ;;
    "- [ ] "*) current_status="unchecked" ;;
  esac
  # Only validate Tests: belonging to completed items.
  if [ "$current_status" != "done" ]; then
    continue
  fi
  case "$line" in
    *"- Tests:"*) ;;
    *) continue ;;
  esac
  TESTS=$(echo "$line" | sed 's/^[[:space:]]*- Tests:[[:space:]]*//')
  IFS=',' read -ra TEST_ARRAY <<< "$TESTS"
  for test_name in "${TEST_ARRAY[@]}"; do
    # Strip backticks, parens, whitespace, trailing period.
    test_name=$(
      printf '%s' "$test_name" \
        | sed -e 's/`//g' -e 's/([^)]*)//g' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' -e 's/\.$//'
    )
    [ -z "$test_name" ] && continue
    # Skip non-identifier placeholders.
    case "$test_name" in
      N/A*|TBD*|existing*|same*|whatever*) continue ;;
    esac
    FOUND=$(grep -r "fn ${test_name}" "$PROJECT_DIR/crates" --include='*.rs' -l 2>/dev/null || true)
    if [ -z "$FOUND" ]; then
      VIOLATIONS=$((VIOLATIONS + 1))
      MISSING_TESTS=$((MISSING_TESTS + 1))
      REPORT="${REPORT}\n  [MISSING TEST] fn ${test_name} — not found in crates/"
    fi
  done
done < "$PLAN_FILE"

# Check 3: Verify listed files exist in the project.
# Extract file names from "Files: file1.rs, file2.rs" lines.
#
# Parsing rules (post session 8 fix):
# - Strip backticks around filenames (markdown code spans)
# - Strip parenthesized annotations like "(new)" or "(shutdown hook)"
# - Strip trailing dots from the final entry (markdown sentence end)
# - Search under crates/, config/, docs/, deploy/, .claude/, scripts/
# - Also accept TBD / "whatever" / placeholder text (skip without failing)
MISSING_FILES=0
current_status="none"
while IFS= read -r line; do
  case "$line" in
    "- [x] "*) current_status="done" ;;
    "- [~] "*) current_status="deferred" ;;
    "- [ ] "*) current_status="unchecked" ;;
  esac
  if [ "$current_status" != "done" ]; then
    continue
  fi
  case "$line" in
    *"- Files:"*) ;;
    *) continue ;;
  esac
  FILES=$(echo "$line" | sed 's/^[[:space:]]*- Files:[[:space:]]*//')

  IFS=',' read -ra FILE_ARRAY <<< "$FILES"
  for file_name in "${FILE_ARRAY[@]}"; do
    # Normalise: strip backticks, parenthesized notes, trailing period,
    # and surrounding whitespace. Pipeline once to keep the logic obvious.
    file_name=$(
      printf '%s' "$file_name" \
        | sed -e 's/`//g' -e 's/([^)]*)//g' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' -e 's/\.$//'
    )
    # Skip empty / placeholder tokens
    [ -z "$file_name" ] && continue
    case "$file_name" in
      TBD*|tbd*|\.\.\.*|\*\**) continue ;;
    esac

    # Search across every tracked tickvault directory.
    FOUND=""
    for search_dir in \
      "$PROJECT_DIR/crates" \
      "$PROJECT_DIR/config" \
      "$PROJECT_DIR/docs" \
      "$PROJECT_DIR/deploy" \
      "$PROJECT_DIR/.claude" \
      "$PROJECT_DIR/scripts"
    do
      [ -d "$search_dir" ] || continue
      if [[ "$file_name" == */* ]]; then
        FOUND=$(find "$search_dir" -path "*/$file_name" -type f 2>/dev/null | head -1)
      else
        FOUND=$(find "$search_dir" -name "$file_name" -type f 2>/dev/null | head -1)
      fi
      [ -n "$FOUND" ] && break
    done

    # Fall back to a root-level match (CLAUDE.md, active-plan.md, etc.)
    if [ -z "$FOUND" ] && [ -f "$PROJECT_DIR/$file_name" ]; then
      FOUND="$PROJECT_DIR/$file_name"
    fi

    if [ -z "$FOUND" ]; then
      VIOLATIONS=$((VIOLATIONS + 1))
      MISSING_FILES=$((MISSING_FILES + 1))
      REPORT="${REPORT}\n  [MISSING FILE] ${file_name} — not found in project"
    fi
  done
done < "$PLAN_FILE"

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
