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
# 2026-08-10: fail CLOSED. `|| exit 0` reported success while having scanned
# nothing — indistinguishable from a clean run.
cd "$REPO_ROOT" || { echo "FATAL: cannot cd to $REPO_ROOT — refusing to pass vacuously" >&2; exit 1; }

# Determine scope: pre-commit uses the staged diff, pre-push the pushed range,
# and CI the pull request's own diff.
#
# 2026-08-10: the CI branch is NEW. Of the ten local guards, this was the ONLY
# one with no server-side twin - verified by scanning every workflow - so a
# gate-skipping push, or any edit made through the web UI, missed it entirely.
# That is precisely the bypass class the Repo Guards job exists to close
# (merge-gate-lock section 3 row 6), and this guard is not a ratchet-baseline
# guard, so nothing prevented it running there.
#
# GITHUB_BASE_REF is set only on pull_request events and is checked FIRST,
# because CI's HEAD~1..HEAD is meaningless there: actions/checkout defaults to
# depth 1, so HEAD~1 does not exist and the range silently yields nothing - a
# vacuous pass wearing the shape of a real scan. The CI step deepens the fetch
# before calling this.
SCOPE_FILES=""
if [ -n "${GITHUB_BASE_REF:-}" ] && git rev-parse --verify "origin/${GITHUB_BASE_REF}" >/dev/null 2>&1; then
  SCOPE_FILES=$(git diff --name-only "origin/${GITHUB_BASE_REF}...HEAD" -- 'crates/**/*.rs' 2>/dev/null || true)
elif git rev-parse --verify HEAD >/dev/null 2>&1; then
  if [ -n "$(git diff --cached --name-only -- 'crates/**/*.rs' 2>/dev/null || true)" ]; then
    SCOPE_FILES=$(git diff --cached --name-only -- 'crates/**/*.rs' 2>/dev/null || true)
  else
    SCOPE_FILES=$(git diff --name-only HEAD~1..HEAD -- 'crates/**/*.rs' 2>/dev/null || true)
  fi
fi

# Under CI on a pull request, an empty scope is only legitimate when the PR
# genuinely touched no Rust. If the base ref could not be resolved we cannot
# tell the two apart, so refuse rather than report a pass we did not earn.
if [ -n "${GITHUB_BASE_REF:-}" ] && ! git rev-parse --verify "origin/${GITHUB_BASE_REF}" >/dev/null 2>&1; then
  echo "FATAL: GITHUB_BASE_REF=${GITHUB_BASE_REF} is set but origin/${GITHUB_BASE_REF} is not fetched - cannot compute the PR diff, and an empty scan here would be a false pass" >&2
  exit 1
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
  while IFS=':' read -r FN_LINE REST; do
    [ -n "$FN_LINE" ] || continue

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
    PREV_LINE=$((FN_LINE - 1))
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
      VIOLATIONS+=("${FILE}:${FN_LINE}: pub fn ${FN_NAME}() has no call sites")
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
