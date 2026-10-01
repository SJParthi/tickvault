#!/bin/bash
# plan-verify.sh — Mechanical enforcement: every ticked plan item is implemented + tested.
#
# Usage:
#   bash .claude/hooks/plan-verify.sh [PROJECT_DIR] [MODE] [PLAN_FILE...]
#
#   PROJECT_DIR  defaults to "."
#   MODE         "verify" (default) or "push"
#   PLAN_FILE    one or more plan files to check. With none, EVERY
#                .claude/plans/active-plan*.md is checked.
#
# Exit codes:
#   0 = PASS (no active plan, or every checked plan is consistent)
#   2 = BLOCK (a ticked item whose tests or files are missing, a VERIFIED
#       plan with unticked items, or a plan with no Status)
#
# Called by: Claude Code manually before declaring an item "done". No push
# gate calls this script today (pre-push-gate.sh never wired the "gate 14"
# this header used to name); the design-first wall is plan-gate.sh.
#
# 2026-10-01: this script used to read only `.claude/plans/active-plan.md`.
# No plan in this repository uses that exact name — they are all
# `active-plan-<slug>.md` — so it printed "PASS: No active implementation
# plan" for every real plan and every "plan-verify clean" claim built on it
# was vacuous (recorded in commit e1a1584). It now checks every
# `active-plan*.md`, the same set plan-gate.sh scans, and it says how many
# plans it checked so a zero can never read as a pass by accident.
#
# What each plan is checked for:
#   1. A **Status:** field exists.
#   2. Every TICKED item's `- Tests:` names exist as `fn <name>` under crates/.
#   3. Every TICKED item's `- Files:` names exist in the project.
#   4. A plan whose Status is VERIFIED has NO unticked items (a VERIFIED
#      plan with open work is a false claim). Unticked items in a plan that
#      is still APPROVED / IN_PROGRESS are listed as OPEN, not counted as
#      violations: the long-running plans here hold many items and ship
#      them one pull request at a time.
#   In "push" mode every checked plan must ALSO have Status VERIFIED. Name the
#   plan file explicitly in that mode; the default set holds plans other
#   sessions are still working.

set -uo pipefail

PROJECT_DIR="${1:-.}"
MODE="${2:-verify}"
shift 2 2>/dev/null || shift $#

if [ "$MODE" != "verify" ] && [ "$MODE" != "push" ]; then
  echo "  FAIL: unknown mode '$MODE' (expected verify or push)" >&2
  exit 2
fi

PLAN_FILES=()
if [ "$#" -gt 0 ]; then
  for f in "$@"; do
    if [ ! -f "$f" ]; then
      echo "  FAIL: plan file not found: $f" >&2
      exit 2
    fi
    PLAN_FILES+=("$f")
  done
else
  for f in "$PROJECT_DIR"/.claude/plans/active-plan*.md; do
    [ -f "$f" ] && PLAN_FILES+=("$f")
  done
fi

if [ "${#PLAN_FILES[@]}" -eq 0 ]; then
  echo "  PASS: No active implementation plan (0 plans checked)" >&2
  exit 0
fi

# Strip every parenthesised note (innermost first, so nested notes and notes
# holding commas go too), then backticks.
strip_notes() {
  local s="$1" prev=""
  while [ "$s" != "$prev" ]; do
    prev="$s"
    s=$(printf '%s' "$s" | sed -e 's/([^()]*)//g')
  done
  printf '%s' "$s" | sed -e 's/`//g'
}

# Trim whitespace and a trailing period from one comma-separated token.
normalise() {
  printf '%s' "$1" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' -e 's/\.$//'
}

# Print the names in one Tests token that can be checked as `fn <name>`:
# a `path::to::name` keeps its last segment; a token that is prose keeps
# only its snake_case words (test names here always hold an underscore).
test_names_in() {
  local token="$1" word
  token="${token##*::}"
  if [[ "$token" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
    printf '%s\n' "$token"
    return
  fi
  for word in $token; do
    word="${word##*::}"
    [[ "$word" =~ ^[a-z][a-z0-9]*_[a-z0-9_]*$ ]] && printf '%s\n' "$word"
  done
}

file_exists_in_project() {
  local name="$1" dir found=""
  if [[ "$name" == */ ]]; then
    [ -d "$PROJECT_DIR/$name" ]
    return
  fi
  for dir in crates config docs deploy .claude scripts; do
    [ -d "$PROJECT_DIR/$dir" ] || continue
    if [[ "$name" == */* ]]; then
      found=$(find "$PROJECT_DIR/$dir" -path "*/$name" -type f 2>/dev/null | head -1)
    else
      found=$(find "$PROJECT_DIR/$dir" -name "$name" -type f 2>/dev/null | head -1)
    fi
    [ -n "$found" ] && return 0
  done
  [ -f "$PROJECT_DIR/$name" ]
}

TOTAL_VIOLATIONS=0

for PLAN_FILE in "${PLAN_FILES[@]}"; do
  VIOLATIONS=0
  REPORT=""
  NAME="${PLAN_FILE#"$PROJECT_DIR"/}"

  STATUS=$(grep -m1 '^\*\*Status:\*\*' "$PLAN_FILE" 2>/dev/null | sed 's/.*\*\*Status:\*\* *//' | awk '{print $1}' | tr -cd 'A-Za-z_')
  if [ -z "$STATUS" ]; then
    echo "  FAIL: $NAME has no **Status:** field" >&2
    TOTAL_VIOLATIONS=$((TOTAL_VIOLATIONS + 1))
    continue
  fi

  # `grep -c` prints "0" AND exits non-zero on zero matches; `|| true` keeps
  # the single "0" instead of appending a second one.
  UNCHECKED=$(grep -c '^\- \[ \]' "$PLAN_FILE" 2>/dev/null || true)
  CHECKED=$(grep -c '^\- \[[xX]\]' "$PLAN_FILE" 2>/dev/null || true)
  UNCHECKED=${UNCHECKED:-0}
  CHECKED=${CHECKED:-0}

  if [ "$UNCHECKED" -gt 0 ] && { [ "$STATUS" = "VERIFIED" ] || [ "$MODE" = "push" ]; }; then
    VIOLATIONS=$((VIOLATIONS + UNCHECKED))
    REPORT="${REPORT}\n  [INCOMPLETE] Status ${STATUS}, but ${UNCHECKED} item(s) are unticked"
  fi
  if [ "$MODE" = "push" ] && [ "$STATUS" != "VERIFIED" ]; then
    VIOLATIONS=$((VIOLATIONS + 1))
    REPORT="${REPORT}\n  [STATUS] '${STATUS}' — must be VERIFIED before push"
  fi

  # Walk the plan once; only `- Tests:` / `- Files:` lines that belong to a
  # TICKED item are checked. `[~]` (deferred) and `[ ]` items are skipped.
  MISSING_TESTS=0
  MISSING_FILES=0
  current_status="none"
  while IFS= read -r line; do
    case "$line" in
      "- [x] "*|"- [X] "*) current_status="done" ;;
      "- [~] "*) current_status="deferred" ;;
      "- [ ] "*) current_status="unchecked" ;;
    esac
    [ "$current_status" = "done" ] || continue
    case "$line" in
      *"- Tests:"*|"- ["[xX]"] "*"Tests:"*)
        # A `- Tests:` sub-line, or `Tests:` written inline on the item
        # line itself; either way the names are what follows the last
        # `Tests:` on the line.
        TESTS=$(strip_notes "$(echo "$line" | sed 's/^.*Tests:[[:space:]]*//')")
        # An inline list runs on into the item's prose, and often names a
        # test by a leading part of its name; there a word passes if any
        # identifier in crates/ STARTS with it. A `- Tests:` line is exact.
        case "$line" in
          "- ["[xX]"] "*) TEST_PATTERN_TAIL='' ;;
          *) TEST_PATTERN_TAIL='\b' ;;
        esac
        IFS=',' read -ra TEST_ARRAY <<< "$TESTS"
        for token in "${TEST_ARRAY[@]}"; do
          token=$(normalise "$token")
          [ -z "$token" ] && continue
          case "$token" in
            N/A*|TBD*|existing*|same*|whatever*) continue ;;
          esac
          # A test FILE named in the Tests list is checked as a file.
          if [[ "$token" == *.rs ]]; then
            if ! file_exists_in_project "$token"; then
              MISSING_TESTS=$((MISSING_TESTS + 1))
              REPORT="${REPORT}\n  [MISSING TEST] ${token} — file not found"
            fi
            continue
          fi
          while IFS= read -r test_name; do
            [ -z "$test_name" ] && continue
            # A word naming a test FILE (`foo_guard r20_case`) is the file.
            file_exists_in_project "${test_name}.rs" && continue
            if [ -n "$TEST_PATTERN_TAIL" ]; then
              PATTERN="fn ${test_name}${TEST_PATTERN_TAIL}"
            else
              PATTERN="\b${test_name}"
            fi
            if ! grep -rqE "$PATTERN" "$PROJECT_DIR/crates" --include='*.rs' 2>/dev/null; then
              MISSING_TESTS=$((MISSING_TESTS + 1))
              REPORT="${REPORT}\n  [MISSING TEST] fn ${test_name} — not found in crates/"
            fi
          done < <(test_names_in "$token")
        done
        ;;
      *"- Files:"*)
        FILES=$(strip_notes "$(echo "$line" | sed 's/^[[:space:]]*- Files:[[:space:]]*//')")
        IFS=',' read -ra FILE_ARRAY <<< "$FILES"
        for file_name in "${FILE_ARRAY[@]}"; do
          file_name=$(normalise "$file_name")
          [ -z "$file_name" ] && continue
          case "$file_name" in
            TBD*|tbd*|\.\.\.*|\*\**|*" "*) continue ;;
          esac
          if ! file_exists_in_project "$file_name"; then
            MISSING_FILES=$((MISSING_FILES + 1))
            REPORT="${REPORT}\n  [MISSING FILE] ${file_name} — not found in project"
          fi
        done
        ;;
    esac
  done < "$PLAN_FILE"
  VIOLATIONS=$((VIOLATIONS + MISSING_TESTS + MISSING_FILES))

  if [ "$VIOLATIONS" -gt 0 ]; then
    echo "  FAIL: $NAME (Status ${STATUS}) — ${VIOLATIONS} issue(s)" >&2
    echo -e "$REPORT" >&2
  else
    echo "  ok  : $NAME (Status ${STATUS}) — ${CHECKED} ticked items verified, ${UNCHECKED} open" >&2
  fi
  TOTAL_VIOLATIONS=$((TOTAL_VIOLATIONS + VIOLATIONS))
done

if [ "$TOTAL_VIOLATIONS" -gt 0 ]; then
  echo "" >&2
  echo "  PLAN VERIFICATION FAILED: ${TOTAL_VIOLATIONS} issue(s) across ${#PLAN_FILES[@]} plan(s)" >&2
  exit 2
fi

echo "  PASS: ${#PLAN_FILES[@]} plan(s) checked" >&2
exit 0
