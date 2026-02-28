#!/bin/bash
# pre-push-gate.sh — Final safety net before code leaves the machine
# Called by PreToolUse hook on Bash commands matching "git push"
# Exit 2 = BLOCK push.
#
# Gate order:
#   1. cargo test (full workspace)
#   2. cargo audit (known vulnerabilities) — if installed
#   3. cargo deny check (license + advisory) — if installed

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

# Check if any .rs files exist in the project
RS_EXISTS=$(find crates -name '*.rs' 2>/dev/null | head -1)
if [ -z "$RS_EXISTS" ]; then
  exit 0
fi

FAILED=0

echo "╔══════════════════════════════════════════════╗" >&2
echo "║        PRE-PUSH SAFETY NET                   ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2

# Gate 1: cargo test (mandatory)
echo "  [1/3] cargo test..." >&2
if ! cargo test --workspace > /dev/null 2>&1; then
  echo "  FAIL: cargo test failed. Cannot push broken code." >&2
  FAILED=1
else
  echo "  PASS: cargo test (100% pass)" >&2
fi

# Gate 2: cargo audit (advisory — only if installed)
echo "  [2/3] cargo audit..." >&2
if command -v cargo-audit > /dev/null 2>&1; then
  if ! cargo audit > /dev/null 2>&1; then
    echo "  FAIL: cargo audit found vulnerabilities. Review before pushing." >&2
    FAILED=1
  else
    echo "  PASS: cargo audit (no known vulnerabilities)" >&2
  fi
else
  echo "  SKIP: cargo-audit not installed (install with: cargo install cargo-audit)" >&2
fi

# Gate 3: cargo deny (advisory — only if installed)
# Note: cargo deny needs network to fetch advisory DB. If it fails due to
# network/proxy issues (exit code != 0 but stderr contains "fetch"), treat
# as SKIP rather than FAIL. CI enforces this properly.
echo "  [3/3] cargo deny..." >&2
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
  echo "  SKIP: cargo-deny not installed (install with: cargo install cargo-deny)" >&2
fi

echo "" >&2
if [ "$FAILED" -ne 0 ]; then
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  PUSH BLOCKED — Fix issues above             ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
  exit 2
fi

echo "╔══════════════════════════════════════════════╗" >&2
echo "║  ALL GATES PASSED — Push allowed             ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2
exit 0
