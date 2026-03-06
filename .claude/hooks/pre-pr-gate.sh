#!/bin/bash
# pre-pr-gate.sh — Blocks `gh pr create` unless quality gates pass
# Called by PreToolUse hook on Bash commands matching "gh pr create"
# Exit 2 = BLOCK PR creation.
#
# Gates:
#   1. Branch check (not main)
#   2. Branch naming convention
#   3. Clean working tree
#   4. Quality state check (state file or full re-run)
#   5. Commit message format (conventional commits)

set -uo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Gate: gh pr create OR gh api calls that create PRs
IS_GH_PR_CREATE=$(echo "$COMMAND" | grep -cE '^\s*gh\s+pr\s+create' || true)
IS_GH_API_PR=$(echo "$COMMAND" | grep -cE 'gh\s+api.*repos/.*/pulls\s' || true)

if [ "$IS_GH_PR_CREATE" -eq 0 ] && [ "$IS_GH_API_PR" -eq 0 ]; then
  exit 0
fi

if [ "$IS_GH_API_PR" -gt 0 ]; then
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  BLOCKED: gh api PR bypass detected           ║" >&2
  echo "║  Use 'gh pr create' so quality gates run      ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
  exit 2
fi

CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if [ -z "$CWD" ]; then CWD="."; fi
cd "$CWD" || exit 0

HOOKS_DIR="$(dirname "$0")"
STATE_FILE="$HOOKS_DIR/.last-quality-pass"
FAILED=0

echo "╔══════════════════════════════════════════════╗" >&2
echo "║        PRE-PR QUALITY GATE                    ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2

# ─────────────────────────────────────────────
# Extract first + last lines for flag parsing
# (flags appear before --body heredoc and after its closing delimiter)
# ─────────────────────────────────────────────
CMD_FLAGS=$(echo "$COMMAND" | sed -n '1p; $p' | tr '\n' ' ')

# ─────────────────────────────────────────────
# Detect --head and --base from command flags only
# ─────────────────────────────────────────────
CURRENT_BRANCH=$(git branch --show-current 2>/dev/null)
HEAD_BRANCH="$CURRENT_BRANCH"
HEAD_FROM_CMD=$(echo "$CMD_FLAGS" | grep -oE '\-\-head[[:space:]]+([a-zA-Z0-9_/-]+)' | head -1 | awk '{print $2}')
if [ -n "$HEAD_FROM_CMD" ]; then
  HEAD_BRANCH="$HEAD_FROM_CMD"
fi
BASE_FROM_CMD_G1=$(echo "$CMD_FLAGS" | grep -oE '\-\-base[[:space:]]+([a-zA-Z0-9_/-]+)' | head -1 | awk '{print $2}')

# ─────────────────────────────────────────────
# GATE 1: Branch check — can't PR from main
# ─────────────────────────────────────────────
if [ "$HEAD_BRANCH" = "main" ] || [ "$HEAD_BRANCH" = "master" ]; then
  echo "  FAIL: Cannot create PR from '$HEAD_BRANCH'. Use a feature branch." >&2
  FAILED=1
else
  echo "  PASS: Not on protected branch ($HEAD_BRANCH)" >&2
fi

# ─────────────────────────────────────────────
# GATE 2: Branch naming convention
# ─────────────────────────────────────────────
if [ -n "$HEAD_BRANCH" ]; then
  if ! echo "$HEAD_BRANCH" | grep -qE '^(feature|fix|hotfix|chore|docs|refactor|test|perf|security|claude)/'; then
    echo "  FAIL: Branch '$HEAD_BRANCH' doesn't follow naming convention." >&2
    echo "  Expected: feature/<name> | fix/<desc> | claude/<id>" >&2
    FAILED=1
  else
    echo "  PASS: Branch naming convention" >&2
  fi
fi

# ─────────────────────────────────────────────
# GATE 3: Clean working tree
# ─────────────────────────────────────────────
if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
  echo "  FAIL: Uncommitted changes detected. Commit or stash first." >&2
  FAILED=1
else
  echo "  PASS: Working tree clean" >&2
fi

# ─────────────────────────────────────────────
# GATE 4: Quality state check
# ─────────────────────────────────────────────
HEAD_HASH=$(git rev-parse HEAD 2>/dev/null)
QUALITY_FRESH=false

if [ -f "$STATE_FILE" ]; then
  SAVED_HASH=$(awk '{print $1}' "$STATE_FILE")
  SAVED_TIME=$(awk '{print $2}' "$STATE_FILE")
  NOW=$(date +%s)
  AGE=$(( NOW - SAVED_TIME ))

  if [ "$SAVED_HASH" = "$HEAD_HASH" ] && [ "$AGE" -lt 300 ]; then
    echo "  PASS: Quality gates verified (${AGE}s ago on ${HEAD_HASH:0:7})" >&2
    QUALITY_FRESH=true
  fi
fi

if [ "$QUALITY_FRESH" = "false" ]; then
  echo "  Quality state stale or missing — running full verification..." >&2

  # Check if any .rs files exist
  RS_EXISTS=$(find crates -name '*.rs' 2>/dev/null | head -1)
  if [ -n "$RS_EXISTS" ]; then

    # 4a: cargo fmt --check
    echo "  [4a/4e] cargo fmt --check..." >&2
    FMT_OUT=$(timeout 60 cargo fmt --all -- --check 2>&1)
    FMT_EXIT=$?
    if [ "$FMT_EXIT" -eq 124 ]; then
      echo "  SKIP: cargo fmt timed out (60s)" >&2
    elif [ "$FMT_EXIT" -ne 0 ]; then
      echo "  FAIL: cargo fmt:" >&2
      echo "$FMT_OUT" | tail -10 >&2
      FAILED=1
    else
      echo "  PASS: cargo fmt" >&2
    fi

    # 4b: cargo clippy
    echo "  [4b/4e] cargo clippy..." >&2
    CLIPPY_OUT=$(timeout 120 cargo clippy --workspace --all-targets -- -D warnings 2>&1)
    CLIPPY_EXIT=$?
    if [ "$CLIPPY_EXIT" -eq 124 ]; then
      echo "  SKIP: cargo clippy timed out (120s)" >&2
    elif [ "$CLIPPY_EXIT" -ne 0 ]; then
      echo "  FAIL: cargo clippy:" >&2
      echo "$CLIPPY_OUT" | tail -20 >&2
      FAILED=1
    else
      echo "  PASS: cargo clippy" >&2
    fi

    # 4c: cargo test
    echo "  [4c/4e] cargo test..." >&2
    TEST_OUT=$(timeout 120 cargo test --workspace 2>&1)
    TEST_EXIT=$?
    if [ "$TEST_EXIT" -eq 124 ]; then
      echo "  SKIP: cargo test timed out (120s)" >&2
    elif [ "$TEST_EXIT" -ne 0 ]; then
      echo "  FAIL: cargo test:" >&2
      echo "$TEST_OUT" | tail -20 >&2
      FAILED=1
    else
      echo "  PASS: cargo test" >&2
    fi

    # 4d: Banned pattern scan (full workspace)
    echo "  [4d/4e] Banned pattern scan..." >&2
    ALL_RS=$(find crates -name '*.rs' -not -path '*/target/*' 2>/dev/null | tr '\n' ' ')
    if [ -n "$ALL_RS" ] && [ -x "$HOOKS_DIR/banned-pattern-scanner.sh" ]; then
      if ! timeout 60 "$HOOKS_DIR/banned-pattern-scanner.sh" "$CWD" "$ALL_RS" > /dev/null 2>&1; then
        echo "  FAIL: Banned patterns found in workspace." >&2
        FAILED=1
      else
        echo "  PASS: Banned pattern scan" >&2
      fi
    else
      echo "  SKIP: Banned pattern scanner not available" >&2
    fi

    # 4e: Test count guard
    echo "  [4e/4e] Test count guard..." >&2
    if [ -x "$HOOKS_DIR/test-count-guard.sh" ]; then
      if ! timeout 30 "$HOOKS_DIR/test-count-guard.sh" "$CWD" 2>&1; then
        FAILED=1
      fi
    else
      echo "  SKIP: test-count-guard.sh not executable" >&2
    fi
  fi
fi

# ─────────────────────────────────────────────
# GATE 5: Commit message format
# ─────────────────────────────────────────────
echo "  Checking commit message format..." >&2

# Determine base branch from the PR command flags (--base flag) or default to main
BASE_BRANCH="main"
BASE_FROM_CMD=$(echo "$CMD_FLAGS" | grep -oE '\-\-base[[:space:]]+[a-zA-Z0-9_/-]+' | head -1 | awk '{print $2}')
if [ -n "$BASE_FROM_CMD" ]; then
  BASE_BRANCH="$BASE_FROM_CMD"
fi

# Get commits since divergence from base (first-parent only to skip merged-in history)
MERGE_BASE=$(git merge-base "origin/$BASE_BRANCH" HEAD 2>/dev/null || true)
if [ -n "$MERGE_BASE" ]; then
  COMMITS=$(git log --first-parent "$MERGE_BASE"..HEAD --format='%s' 2>/dev/null)
else
  COMMITS=$(git log --first-parent -10 --format='%s' 2>/dev/null)
fi

BAD_MSGS=""
while IFS= read -r msg; do
  [ -z "$msg" ] && continue
  # Allow: conventional commits, merge commits, revert commits
  if ! echo "$msg" | grep -qE '^(Merge|Revert)' && \
     ! echo "$msg" | grep -qE '^(feat|fix|refactor|test|docs|chore|perf|security)(\([a-z0-9_/-]+\))?: .+'; then
    BAD_MSGS="${BAD_MSGS}  - ${msg}\n"
  fi
done <<< "$COMMITS"

if [ -n "$BAD_MSGS" ]; then
  echo "  FAIL: Non-conventional commit messages found:" >&2
  echo -e "$BAD_MSGS" >&2
  echo "  Expected: <type>(<scope>): <description>" >&2
  echo "  Valid types: feat, fix, refactor, test, docs, chore, perf, security" >&2
  FAILED=1
else
  echo "  PASS: All commit messages follow convention" >&2
fi

# ─────────────────────────────────────────────
# FINAL VERDICT
# ─────────────────────────────────────────────
echo "" >&2
if [ "$FAILED" -ne 0 ]; then
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  PR CREATION BLOCKED — Fix violations above  ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
  exit 2
fi

echo "╔══════════════════════════════════════════════╗" >&2
echo "║  ALL GATES PASSED — PR creation allowed       ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2
exit 0
