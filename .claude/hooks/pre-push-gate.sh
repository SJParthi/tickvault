#!/bin/bash
# pre-push-gate.sh — Final safety net before code leaves the machine
# Called by PreToolUse hook on Bash commands matching "git push"
# Exit 2 = BLOCK push.
#
# Gate order:
#   1. cargo fmt --check
#   2. cargo clippy -D warnings
#   3. cargo test (full workspace)
#   4. Banned pattern scan (full workspace, not just staged)
#   5. Test count guard (ratcheting baseline)
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

cd "$CWD" || { echo "FAIL: cannot cd to $CWD" >&2; exit 2; }

# Check if any .rs files exist in the project
RS_EXISTS=$(find crates -name '*.rs' 2>/dev/null | head -1)
if [ -z "$RS_EXISTS" ]; then
  exit 0
fi

HOOKS_DIR="$(dirname "$0")"
FAILED=0

echo "╔══════════════════════════════════════════════╗" >&2
echo "║        PRE-PUSH SAFETY NET (8 Gates)          ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2

# Gate 1: cargo fmt (show errors on failure)
echo "  [1/8] cargo fmt --check..." >&2
FMT_OUT=$(cargo fmt --all -- --check 2>&1)
FMT_EXIT=$?
if [ "$FMT_EXIT" -ne 0 ]; then
  echo "  FAIL: Code not formatted:" >&2
  echo "$FMT_OUT" | tail -10 >&2
  FAILED=1
else
  echo "  PASS: cargo fmt" >&2
fi

# Gate 2: cargo clippy (show errors on failure)
echo "  [2/8] cargo clippy..." >&2
CLIPPY_OUT=$(cargo clippy --workspace --all-targets -- -D warnings -D clippy::arithmetic_side_effects -D clippy::indexing_slicing -D clippy::as_conversions 2>&1)
CLIPPY_EXIT=$?
if [ "$CLIPPY_EXIT" -ne 0 ]; then
  echo "  FAIL: clippy warnings found:" >&2
  echo "$CLIPPY_OUT" | tail -20 >&2
  FAILED=1
else
  echo "  PASS: cargo clippy (zero warnings)" >&2
fi

# Gate 3: cargo test (mandatory, show errors on failure)
echo "  [3/8] cargo test..." >&2
TEST_OUT=$(cargo test --workspace 2>&1)
TEST_EXIT=$?
if [ "$TEST_EXIT" -ne 0 ]; then
  echo "  FAIL: cargo test failed:" >&2
  echo "$TEST_OUT" | tail -20 >&2
  FAILED=1
else
  echo "  PASS: cargo test (100% pass)" >&2
fi

# Gate 4: Banned pattern scan (full workspace, not just staged)
echo "  [4/8] Banned pattern scan (full workspace)..." >&2
ALL_RS=$(find crates -name '*.rs' -not -path '*/target/*' 2>/dev/null | tr '\n' ' ')
if [ -n "$ALL_RS" ] && [ -x "$HOOKS_DIR/banned-pattern-scanner.sh" ]; then
  if ! echo "$ALL_RS" | "$HOOKS_DIR/banned-pattern-scanner.sh" "$CWD" "$ALL_RS" > /dev/null 2>&1; then
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
  if ! "$HOOKS_DIR/test-count-guard.sh" "$CWD" 2>&1; then
    FAILED=1
  fi
else
  echo "  SKIP: test-count-guard.sh not executable" >&2
fi

# Gate 6: cargo audit (CVEs + yanked — required if installed)
echo "  [6/8] cargo audit (CVEs + yanked)..." >&2
if command -v cargo-audit > /dev/null 2>&1; then
  AUDIT_OUT=$(cargo audit --deny yanked 2>&1)
  AUDIT_EXIT=$?
  if echo "$AUDIT_OUT" | grep -q "couldn't fetch advisory database"; then
    echo "  SKIP: Cannot reach advisory database (network issue)" >&2
  elif [ "$AUDIT_EXIT" -ne 0 ]; then
    echo "  FAIL: cargo audit found issues:" >&2
    echo "$AUDIT_OUT" | tail -15 >&2
    FAILED=1
  else
    echo "  PASS: cargo audit (no yanked crates)" >&2
  fi
else
  echo "  FAIL: cargo-audit not installed (install: cargo install cargo-audit)" >&2
  FAILED=1
fi

# Gate 7: cargo deny (advisory — only if installed)
# Note: cargo deny needs network to fetch advisory DB. If it fails due to
# network/proxy issues, treat as SKIP rather than FAIL. CI enforces this properly.
echo "  [7/8] cargo deny..." >&2
if command -v cargo-deny > /dev/null 2>&1; then
  DENY_OUTPUT=$(cargo deny check 2>&1)
  DENY_EXIT=$?
  if [ "$DENY_EXIT" -ne 0 ]; then
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
  echo "  FAIL: cargo-deny not installed (install: cargo install cargo-deny)" >&2
  FAILED=1
fi

# Gate 8: Coverage ratchet guard (only if cargo-llvm-cov installed)
echo "  [8/8] Coverage ratchet guard..." >&2
if [ -x "$HOOKS_DIR/coverage-guard.sh" ]; then
  if command -v cargo-llvm-cov > /dev/null 2>&1; then
    if ! "$HOOKS_DIR/coverage-guard.sh" "$CWD" 2>&1; then
      FAILED=1
    fi
  else
    echo "  FAIL: cargo-llvm-cov not installed (install: cargo install cargo-llvm-cov)" >&2
    FAILED=1
  fi
else
  echo "  FAIL: coverage-guard.sh not executable" >&2
  FAILED=1
fi

echo "" >&2
if [ "$FAILED" -ne 0 ]; then
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  PUSH BLOCKED — Fix issues above             ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
  exit 2
fi

# Write state file for pre-PR gate optimization
HEAD_HASH=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
TEST_COUNT=$(grep -r '#\[test\]' crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
echo "$HEAD_HASH $(date +%s) $TEST_COUNT" > "$HOOKS_DIR/.last-quality-pass"

echo "╔══════════════════════════════════════════════╗" >&2
echo "║  ALL 8 GATES PASSED — Push allowed            ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2
exit 0
