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
    # FIXED 2026-08-10. This was HEAD~1..HEAD — the LAST COMMIT ONLY. On a
    # branch of N commits (the normal PR shape here) a pre-push run inspected
    # one commit and never saw pub fns added in the others. Now it scans the
    # whole branch against its merge-base with the default branch, so every
    # commit in the PR is covered. Falls back to the old single-commit range
    # when no merge-base resolves (detached CI ref, shallow clone).
    MERGE_BASE=$(git merge-base HEAD origin/main 2>/dev/null || git merge-base HEAD main 2>/dev/null || true)
    if [ -n "$MERGE_BASE" ] && [ "$MERGE_BASE" != "$(git rev-parse HEAD)" ]; then
      SCOPE_FILES=$(git diff --name-only "$MERGE_BASE"..HEAD -- 'crates/**/*.rs' 2>/dev/null || true)
    else
      SCOPE_FILES=$(git diff --name-only HEAD~1..HEAD -- 'crates/**/*.rs' 2>/dev/null || true)
    fi
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

    # FIXED 2026-08-10. This previously counted BARE IDENTIFIER MENTIONS and
    # treated any count > 1 as "a caller exists". Comments and doc comments
    # counted. Proven miss: `run_cross_verification` in dhan_live_crossverify.rs
    # — whose own header calls it "the only ground truth available" — scored 2
    # (its declaration plus ONE doc comment) and sailed through, while having
    # zero real callers. The guard built to catch dormant pub fns waved through
    # the most consequential dormant function in the repo.
    #
    # Now: strip // comments, drop the declaration line itself, and require a
    # CALL shape — `name(` or `name (` — not a bare mention. A function called
    # only from its own `#[cfg(test)]` module still counts as wired here; that
    # is a separate, larger tightening deliberately not bundled in.
    # 2026-08-11 — string literals are stripped too, and for the same reason
    # the comment strip was added.
    #
    # `install_crossverify_deps` scored CALL_COUNT=1 with zero real callers.
    # The single "call" was its own name inside the error message that fires
    # when nobody has called it:
    #
    #     "... Call install_crossverify_deps() during boot, before this stack
    #      spawns."
    #
    # A message whose whole purpose is to say "this was never called" was
    # counted as proof that it was. The same function family defeated this
    # guard through a doc comment the week before, which is the part worth
    # noticing: the bypass was not clever, it was just a channel nobody had
    # stripped yet. Comments and strings are now both removed before counting.
    #
    # `s:"[^"]*"::g` is deliberately simple — it does not understand raw
    # strings or escaped quotes. It runs BEFORE the comment strip so a quoted
    # `//` inside a string cannot truncate the line early.
    CALL_COUNT=$(
      grep -rn --include='*.rs' -E "\b${FN_NAME}\b" crates/ 2>/dev/null \
        | sed 's:"[^"]*"::g' \
        | sed 's://.*::' \
        | grep -vE "^${FILE}:${FN_LINE}:" \
        | grep -cE "\b${FN_NAME}\s*\(" \
        || true
    )
    CALL_COUNT=${CALL_COUNT:-0}

    # The declaration line is already excluded, so ANY remaining call-shaped
    # reference means a caller exists. Zero means dormant.
    if [ "$CALL_COUNT" -le 0 ]; then
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
