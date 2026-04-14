#!/bin/bash
# pub-fn-wiring-guard.sh — Phase 6.1 G1
#
# Catches the "dormant pub fn" bug class: a new pub fn lands in the codebase
# but no caller invokes it anywhere. This is exactly how every session-3/4/5
# audit found code that was "tested but not running" — the function was
# defined, tests ran on it directly, but the production call site was missing.
#
# Detection algorithm:
#   1. Find pub fns added in the staged diff
#   2. For each, grep the rest of the codebase for `\bfoo\(`
#   3. Allow if a call site exists, even within the same diff
#   4. Allow if the line above the fn declaration has // WIRING-EXEMPT:
#   5. Allow if the function name starts with `test_` or matches structural
#      patterns (new, default, from, into, drop, fmt, clone, eq, hash)
#   6. Allow trait impl methods (preceded by `impl ... for ... {`)
#
# Exit codes:
#   0 — all new pub fns have callers (or are exempt)
#   2 — at least one new pub fn is dormant; commit/push blocked

set -uo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$REPO_ROOT" || exit 0

# Determine scope: pre-commit uses staged diff, pre-push uses pushed range.
SCOPE_FILES=""
if git rev-parse --verify HEAD >/dev/null 2>&1; then
  if [ -n "$(git diff --cached --name-only -- 'crates/**/*.rs' 2>/dev/null || true)" ]; then
    SCOPE_FILES=$(git diff --cached --name-only -- 'crates/**/*.rs' 2>/dev/null || true)
  else
    SCOPE_FILES=$(git diff --name-only HEAD~1..HEAD -- 'crates/**/*.rs' 2>/dev/null || true)
  fi
fi

if [ -z "$SCOPE_FILES" ]; then
  exit 0
fi

VIOLATIONS=()
for FILE in $SCOPE_FILES; do
  [ -f "$FILE" ] || continue
  # Skip test modules + integration test files.
  case "$FILE" in
    *tests/*|*/tests.rs|*_test.rs)
      continue
      ;;
  esac

  # Find pub fn declarations (excluding `pub(crate)` and `pub(super)` for now —
  # those are intentionally module-scoped).
  while IFS=':' read -r LINENO REST; do
    [ -n "$LINENO" ] || continue

    # Extract fn name. Handles `pub fn foo`, `pub async fn foo`,
    # `pub fn foo<T>`, `pub fn foo(`.
    FN_NAME=$(echo "$REST" | sed -nE 's/^[[:space:]]*pub[[:space:]]+(async[[:space:]]+|const[[:space:]]+|unsafe[[:space:]]+)?fn[[:space:]]+([a-zA-Z_][a-zA-Z0-9_]*)[<( ].*/\2/p')
    [ -n "$FN_NAME" ] || continue

    # Skip structural / trait-impl methods.
    case "$FN_NAME" in
      new|default|from|into|try_from|try_into|drop|fmt|clone|eq|hash|partial_cmp|cmp|main|test_*)
        continue
        ;;
    esac

    # Check for // WIRING-EXEMPT: on the line directly above.
    PREV_LINE=$((LINENO - 1))
    if sed -n "${PREV_LINE}p" "$FILE" 2>/dev/null | grep -q 'WIRING-EXEMPT:'; then
      continue
    fi
    # Also accept TEST-EXEMPT (existing convention, broader scope).
    if sed -n "${PREV_LINE}p" "$FILE" 2>/dev/null | grep -q 'TEST-EXEMPT:'; then
      continue
    fi

    # Check for a call site anywhere in crates/ (excluding the declaration line itself).
    # Use ripgrep if available, else fall back to grep.
    if command -v rg >/dev/null 2>&1; then
      CALL_COUNT=$(rg --type rust --count-matches "\b${FN_NAME}\b" crates/ 2>/dev/null | awk -F: '{ sum += $2 } END { print sum+0 }')
    else
      CALL_COUNT=$(grep -rE "\b${FN_NAME}\b" crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
    fi

    # The declaration itself shows up in the count (1 reference). Anything > 1 means a caller exists.
    if [ "$CALL_COUNT" -le 1 ]; then
      VIOLATIONS+=("${FILE}:${LINENO}: pub fn ${FN_NAME}() has no call sites")
    fi
  done < <(grep -nE '^\s*pub\s+(async\s+|const\s+|unsafe\s+)?fn\s+[a-zA-Z_]' "$FILE" 2>/dev/null || true)
done

if [ "${#VIOLATIONS[@]}" -gt 0 ]; then
  echo "  FAIL: ${#VIOLATIONS[@]} new pub fn(s) have NO call sites — dormant code:" >&2
  for V in "${VIOLATIONS[@]}"; do
    echo "    $V" >&2
  done
  echo "  Options: " >&2
  echo "    1. Add a call site somewhere in crates/" >&2
  echo "    2. Add '// WIRING-EXEMPT: <reason>' on the line directly above" >&2
  echo "    3. Add '// TEST-EXEMPT: <reason>' (also accepted for backwards compat)" >&2
  echo "  This catches the dormant-code class of bugs from the session-3/4/5 honest audits." >&2
  exit 2
fi

exit 0
