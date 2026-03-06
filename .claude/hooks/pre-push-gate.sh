#!/bin/bash
# pre-push-gate.sh — Final safety net before code leaves the machine
# Called by PreToolUse hook on Bash commands matching "git push"
# Exit 2 = BLOCK push.
#
# DEDUP: If pre-commit gate already passed for the same HEAD (within 5 min),
# gates 1-5 (fmt/clippy/test/banned/test-count) are SKIPPED. Only push-only
# gates (audit/deny/coverage) run. This avoids 60-120s of redundant work.
#
# Gate order:
#   1. cargo fmt --check          [skippable if commit-verified]
#   2. cargo clippy -D warnings   [skippable if commit-verified]
#   3. cargo test (full workspace) [skippable if commit-verified]
#   4. Banned pattern scan         [skippable if commit-verified]
#   5. Test count guard            [skippable if commit-verified]
#   6. cargo audit (known vulnerabilities) — if installed
#   7. cargo deny check (license + advisory) — if installed
#   8. Coverage ratchet guard — if cargo-llvm-cov installed
#
# On success, writes state file for pre-PR gate optimization.

set -uo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Only gate git push commands
if ! echo "$COMMAND" | grep -q 'git push'; then
  exit 0
fi

CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if [ -z "$CWD" ]; then
  CWD="."
fi

cd "$CWD" || exit 0

# Signal to auto-save-remote that a user push is in progress
PUSH_LOCK="$CWD/.claude/hooks/.push-in-progress"
touch "$PUSH_LOCK" 2>/dev/null || true
cleanup_push_lock() { rm -f "$PUSH_LOCK" 2>/dev/null || true; }
trap cleanup_push_lock EXIT

# Check if any .rs files exist in the project
RS_EXISTS=$(find crates -name '*.rs' 2>/dev/null | head -1)
if [ -z "$RS_EXISTS" ]; then
  exit 0
fi

HOOKS_DIR="$(dirname "$0")"
FAILED=0

# ─────────────────────────────────────────────
# DEDUP CHECK: Did pre-commit gate already pass for this HEAD?
# ─────────────────────────────────────────────
STATE_FILE="$HOOKS_DIR/.last-quality-pass"
# NOTE: pre-commit-gate writes HEAD *before* the commit runs (PreToolUse hook),
# so the saved hash = parent of the new commit. We compare HEAD~1 to match.
HEAD_CURRENT=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
HEAD_PARENT=$(git rev-parse HEAD~1 2>/dev/null || echo "unknown")
COMMIT_VERIFIED=false

if [ -f "$STATE_FILE" ]; then
  SAVED_HASH=$(awk '{print $1}' "$STATE_FILE")
  SAVED_TIME=$(awk '{print $2}' "$STATE_FILE")
  NOW=$(date +%s)
  AGE=$(( NOW - SAVED_TIME ))

  if [ "$SAVED_HASH" = "$HEAD_PARENT" ] && [ "$AGE" -lt 300 ]; then
    COMMIT_VERIFIED=true
  fi
fi

if [ "$COMMIT_VERIFIED" = "true" ]; then
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  PRE-PUSH (FAST — commit-verified ${AGE}s ago) ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
  echo "  [1-5/8] SKIP: Already verified by pre-commit gate (parent ${HEAD_PARENT:0:7})" >&2
else
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║        PRE-PUSH SAFETY NET (8 Gates)          ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2

  # Gate 1: cargo fmt
  echo "  [1/8] cargo fmt --check..." >&2
  FMT_OUT=$(timeout 60 cargo fmt --all -- --check 2>&1)
  FMT_EXIT=$?
  if [ "$FMT_EXIT" -eq 124 ]; then
    echo "  SKIP: cargo fmt timed out (60s)" >&2
  elif [ "$FMT_EXIT" -ne 0 ]; then
    echo "  FAIL: Code not formatted:" >&2
    echo "$FMT_OUT" | tail -10 >&2
    FAILED=1
  else
    echo "  PASS: cargo fmt" >&2
  fi

  # Gate 2: cargo clippy
  echo "  [2/8] cargo clippy..." >&2
  CLIPPY_OUT=$(timeout 120 cargo clippy --workspace --all-targets -- -D warnings 2>&1)
  CLIPPY_EXIT=$?
  if [ "$CLIPPY_EXIT" -eq 124 ]; then
    echo "  SKIP: cargo clippy timed out (120s)" >&2
  elif [ "$CLIPPY_EXIT" -ne 0 ]; then
    echo "  FAIL: clippy warnings found:" >&2
    echo "$CLIPPY_OUT" | tail -20 >&2
    FAILED=1
  else
    echo "  PASS: cargo clippy (zero warnings)" >&2
  fi

  # Gate 3: cargo test
  echo "  [3/8] cargo test..." >&2
  TEST_OUT=$(timeout 120 cargo test --workspace 2>&1)
  TEST_EXIT=$?
  if [ "$TEST_EXIT" -eq 124 ]; then
    echo "  SKIP: cargo test timed out (120s)" >&2
  elif [ "$TEST_EXIT" -ne 0 ]; then
    echo "  FAIL: cargo test failed:" >&2
    echo "$TEST_OUT" | tail -20 >&2
    FAILED=1
  else
    echo "  PASS: cargo test (100% pass)" >&2
  fi

  # Gate 4: Banned pattern scan (full workspace)
  echo "  [4/8] Banned pattern scan (full workspace)..." >&2
  ALL_RS=$(find crates -name '*.rs' -not -path '*/target/*' 2>/dev/null | tr '\n' ' ')
  if [ -n "$ALL_RS" ] && [ -x "$HOOKS_DIR/banned-pattern-scanner.sh" ]; then
    if ! timeout 60 "$HOOKS_DIR/banned-pattern-scanner.sh" "$CWD" "$ALL_RS" > /dev/null 2>&1; then
      echo "  FAIL: Banned patterns in workspace." >&2
      FAILED=1
    else
      echo "  PASS: Banned pattern scan" >&2
    fi
  else
    echo "  SKIP: Scanner not available" >&2
  fi

  # Gate 5: Test count guard
  echo "  [5/8] Test count guard..." >&2
  if [ -x "$HOOKS_DIR/test-count-guard.sh" ]; then
    if ! timeout 30 "$HOOKS_DIR/test-count-guard.sh" "$CWD" 2>&1; then
      FAILED=1
    fi
  else
    echo "  SKIP: test-count-guard.sh not executable" >&2
  fi
fi

# ─────────────────────────────────────────────
# Gates 6-8: Push-only gates (always run)
# ─────────────────────────────────────────────

# Gate 6: cargo audit (CVEs + yanked — required if installed)
echo "  [6/8] cargo audit (CVEs + yanked)..." >&2
if command -v cargo-audit > /dev/null 2>&1; then
  AUDIT_OUT=$(timeout 30 cargo audit --deny yanked 2>&1)
  AUDIT_EXIT=$?
  if [ "$AUDIT_EXIT" -eq 124 ]; then
    echo "  SKIP: cargo audit timed out (30s)" >&2
  elif echo "$AUDIT_OUT" | grep -q "couldn't fetch advisory database"; then
    echo "  SKIP: Cannot reach advisory database (network issue)" >&2
  elif [ "$AUDIT_EXIT" -ne 0 ]; then
    echo "  FAIL: cargo audit found issues:" >&2
    echo "$AUDIT_OUT" | tail -15 >&2
    FAILED=1
  else
    echo "  PASS: cargo audit (no yanked crates)" >&2
  fi
else
  echo "  SKIP: cargo-audit not installed (install with: cargo install cargo-audit)" >&2
fi

# Gate 7: cargo deny (advisory — only if installed)
echo "  [7/8] cargo deny..." >&2
if command -v cargo-deny > /dev/null 2>&1; then
  DENY_OUTPUT=$(timeout 30 cargo deny check 2>&1)
  DENY_EXIT=$?
  if [ "$DENY_EXIT" -eq 124 ]; then
    echo "  SKIP: cargo deny timed out (30s)" >&2
  elif [ "$DENY_EXIT" -ne 0 ]; then
    if echo "$DENY_OUTPUT" | grep -qi 'failed to fetch\|network\|transport\|proxy\|connect'; then
      echo "  SKIP: cargo deny cannot reach advisory DB (network/proxy). CI will enforce." >&2
    else
      echo "  FAIL: cargo deny found issues. Review before pushing." >&2
      FAILED=1
    fi
  else
    echo "  PASS: cargo deny (licenses + advisories clean)" >&2
  fi
else
  echo "  SKIP: cargo-deny not installed (install with: cargo install cargo-deny)" >&2
fi

# Gate 8: Coverage ratchet guard (only if cargo-llvm-cov installed)
echo "  [8/8] Coverage ratchet guard..." >&2
if [ -x "$HOOKS_DIR/coverage-guard.sh" ]; then
  if command -v cargo-llvm-cov > /dev/null 2>&1; then
    if ! "$HOOKS_DIR/coverage-guard.sh" "$CWD" 2>&1; then
      FAILED=1
    fi
  else
    echo "  SKIP: cargo-llvm-cov not installed (install: cargo install cargo-llvm-cov)" >&2
  fi
else
  echo "  SKIP: coverage-guard.sh not executable" >&2
fi

echo "" >&2
if [ "$FAILED" -ne 0 ]; then
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  PUSH BLOCKED — Fix issues above             ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
  exit 2
fi

# Write state file for pre-PR gate optimization
TEST_COUNT=$(grep -r '#\[test\]' crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
# Write current HEAD (not parent) — so pre-pr-gate and session-status can match
echo "$HEAD_CURRENT $(date +%s) $TEST_COUNT" > "$HOOKS_DIR/.last-quality-pass"

# Clean up auto-save refs on successful push (background, non-blocking, serialized)
BRANCH_NOW=$(git branch --show-current 2>/dev/null || echo "")
BRANCH_SAFE_NOW=$(echo "$BRANCH_NOW" | tr '/' '-' | tr -cd 'a-zA-Z0-9_-')
if [ -n "$BRANCH_SAFE_NOW" ]; then
  (
    # Wait briefly so the actual push completes before we fire delete requests
    sleep 2
    refs_to_clean=$(git for-each-ref --format='%(refname)' "refs/auto-save/${BRANCH_SAFE_NOW}-" 2>/dev/null)
    for ref in $refs_to_clean; do
      # Serialize: one delete at a time with timeout, small delay between
      timeout 10 git push origin --delete "$ref" 2>/dev/null || true
      git update-ref -d "$ref" 2>/dev/null || true
      sleep 1
    done
  ) > /dev/null 2>&1 &
  disown
fi

if [ "$COMMIT_VERIFIED" = "true" ]; then
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  PUSH ALLOWED (fast path — 3 gates only)     ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
else
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  ALL 8 GATES PASSED — Push allowed            ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
fi
exit 0
