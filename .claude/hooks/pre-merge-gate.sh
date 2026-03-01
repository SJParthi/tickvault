#!/bin/bash
# pre-merge-gate.sh — Blocks `gh pr merge` unless CI passed
# Called by PreToolUse hook on Bash commands matching "gh pr merge"
# Exit 2 = BLOCK merge.
#
# Gates:
#   1. Block --admin flag (no bypassing branch protection)
#   2. Extract PR number
#   3. Verify CI status via gh API

set -uo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Only gate commands that START with gh pr merge (not mentioned in commit messages)
if ! echo "$COMMAND" | grep -qE '^\s*gh\s+pr\s+merge'; then
  exit 0
fi

CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if [ -z "$CWD" ]; then CWD="."; fi
cd "$CWD" || exit 0

FAILED=0

echo "╔══════════════════════════════════════════════╗" >&2
echo "║        PRE-MERGE SAFETY GATE                  ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2

# ─────────────────────────────────────────────
# GATE 1: Block --admin flag
# ─────────────────────────────────────────────
if echo "$COMMAND" | grep -q '\-\-admin'; then
  echo "  BLOCKED: --admin flag is banned. CI must pass. No bypassing." >&2
  FAILED=1
else
  echo "  PASS: No --admin bypass" >&2
fi

# ─────────────────────────────────────────────
# EARLY EXIT: Allow --auto (GitHub enforces CI)
# ─────────────────────────────────────────────
if echo "$COMMAND" | grep -q '\-\-auto'; then
  if [ "$FAILED" -ne 0 ]; then
    echo "  BLOCKED: --admin + --auto is not allowed." >&2
    exit 2
  fi
  echo "  INFO: Auto-merge requested — GitHub will enforce CI before merging." >&2
  echo "" >&2
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  AUTO-MERGE ALLOWED — GitHub enforces CI     ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
  exit 0
fi

# ─────────────────────────────────────────────
# GATE 2: Extract PR number
# ─────────────────────────────────────────────
PR_NUM=""

# Try to extract PR number from command (e.g., "gh pr merge 123")
PR_NUM=$(echo "$COMMAND" | grep -oE 'merge[[:space:]]+([0-9]+)' | awk '{print $2}')

# Try --repo flag extraction for repo context
REPO_FLAG=""
if echo "$COMMAND" | grep -q '\-\-repo'; then
  REPO_FLAG=$(echo "$COMMAND" | grep -oE '\-\-repo[[:space:]]+[^ ]+' | awk '{print $2}')
fi

if [ -z "$PR_NUM" ]; then
  echo "  INFO: No PR number in command — gh will use current branch's PR" >&2
fi

# ─────────────────────────────────────────────
# GATE 3: CI status check via gh API
# ─────────────────────────────────────────────
echo "  Checking CI status..." >&2

GH_ARGS=""
if [ -n "$PR_NUM" ]; then
  GH_ARGS="$PR_NUM"
fi
if [ -n "$REPO_FLAG" ]; then
  GH_ARGS="$GH_ARGS --repo $REPO_FLAG"
fi

# Get check results
CHECKS_OUTPUT=$(gh pr checks $GH_ARGS 2>&1)
CHECKS_EXIT=$?

if [ "$CHECKS_EXIT" -ne 0 ]; then
  # gh pr checks exits non-zero if any check is not passing
  # Parse output to find failures
  FAILED_CHECKS=$(echo "$CHECKS_OUTPUT" | grep -E 'fail|pending' | head -10)
  if [ -n "$FAILED_CHECKS" ]; then
    echo "  FAIL: CI checks not all passing:" >&2
    echo "$FAILED_CHECKS" | while IFS= read -r line; do
      echo "    $line" >&2
    done
    FAILED=1
  else
    # gh pr checks might fail for other reasons (no PR, auth issues)
    echo "  WARN: Could not verify CI status:" >&2
    echo "  $CHECKS_OUTPUT" | head -5 >&2
    echo "  Proceeding with caution — CI status unknown." >&2
  fi
else
  echo "  PASS: All CI checks passed" >&2
fi

# ─────────────────────────────────────────────
# FINAL VERDICT
# ─────────────────────────────────────────────
echo "" >&2
if [ "$FAILED" -ne 0 ]; then
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  MERGE BLOCKED — CI must pass first          ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
  exit 2
fi

echo "╔══════════════════════════════════════════════╗" >&2
echo "║  ALL GATES PASSED — Merge allowed             ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2
exit 0
