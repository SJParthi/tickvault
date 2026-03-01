#!/bin/bash
# pre-commit-gate.sh — Master quality gate for git commit
# Called by PreToolUse hook on Bash commands matching "git commit"
# Exit 2 = BLOCK commit. This is the final checkpoint.
#
# Gate order:
#   1. cargo fmt --check
#   2. cargo clippy -D warnings
#   3. cargo test
#   4. Banned pattern scanner (unwrap, expect, println, localhost, DashMap, hardcoded values, etc.)
#   5. O(1) latency & dedup scanner (linear search, blocking I/O, missing dedup)
#   6. Secret scanner (API keys, tokens, passwords)
#   7. Cargo.toml version pinning (no ^, ~, *, >= in deps)
#   8. Test count guard (ratcheting baseline — count can only go up)
#
# ALL gates must pass. One failure = commit blocked.
# On success, writes state file for pre-PR gate optimization.

set -uo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Only gate git commit commands
if ! echo "$COMMAND" | grep -q 'git commit'; then
  exit 0
fi

CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if [ -z "$CWD" ]; then
  CWD="."
fi

cd "$CWD" || exit 0

# Check if any .rs files are staged
RS_STAGED=$(git diff --cached --name-only --diff-filter=ACMR 2>/dev/null | grep '\.rs$' || true)
ALL_STAGED=$(git diff --cached --name-only --diff-filter=ACMR 2>/dev/null || true)

# If no files staged, allow commit (might be a docs-only commit)
if [ -z "$ALL_STAGED" ]; then
  exit 0
fi

HOOKS_DIR="$(dirname "$0")"
FAILED=0

echo "╔══════════════════════════════════════════════╗" >&2
echo "║        PRE-COMMIT QUALITY GATE               ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2

# ─────────────────────────────────────────────
# GATE 1-3: Only if Rust files are staged
# ─────────────────────────────────────────────
if [ -n "$RS_STAGED" ]; then

  # Gate 1: cargo fmt (show errors on failure)
  echo "  [1/8] cargo fmt --check..." >&2
  FMT_OUT=$(cargo fmt --all -- --check 2>&1)
  FMT_EXIT=$?
  if [ "$FMT_EXIT" -ne 0 ]; then
    echo "  FAIL: cargo fmt check failed:" >&2
    echo "$FMT_OUT" | tail -20 >&2
    echo "  Run 'cargo fmt --all' to fix." >&2
    FAILED=1
  else
    echo "  PASS: cargo fmt" >&2
  fi

  # Gate 2: cargo clippy (show errors on failure)
  echo "  [2/8] cargo clippy..." >&2
  CLIPPY_OUT=$(cargo clippy --workspace --all-targets -- -D warnings 2>&1)
  CLIPPY_EXIT=$?
  if [ "$CLIPPY_EXIT" -ne 0 ]; then
    echo "  FAIL: cargo clippy has warnings:" >&2
    echo "$CLIPPY_OUT" | tail -20 >&2
    FAILED=1
  else
    echo "  PASS: cargo clippy (zero warnings)" >&2
  fi

  # Gate 3: cargo test (show errors on failure)
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

  # Gate 4: Banned pattern scanner
  echo "  [4/8] Banned pattern scan..." >&2
  if ! echo "$RS_STAGED" | "$HOOKS_DIR/banned-pattern-scanner.sh" "$CWD" "$RS_STAGED" 2>&1; then
    FAILED=1
  fi

  # Gate 5: O(1) latency & dedup scanner
  echo "  [5/8] O(1) latency & dedup scan..." >&2
  if ! echo "$RS_STAGED" | "$HOOKS_DIR/dedup-latency-scanner.sh" "$CWD" "$RS_STAGED" 2>&1; then
    FAILED=1
  fi

else
  echo "  [1-3/8] No .rs files staged — skipping cargo gates" >&2
  echo "  [4-5/8] No .rs files staged — skipping Rust scanners" >&2
fi

# ─────────────────────────────────────────────
# GATE 6: Secret scanner (runs on ALL staged files, not just .rs)
# ─────────────────────────────────────────────
echo "  [6/8] Secret scan..." >&2
if ! echo "$ALL_STAGED" | "$HOOKS_DIR/secret-scanner.sh" "$CWD" "$ALL_STAGED" 2>&1; then
  FAILED=1
fi

# ─────────────────────────────────────────────
# GATE 7: Cargo.toml version pinning (no ^, ~, *, >= in deps)
# ─────────────────────────────────────────────
TOML_STAGED=$(echo "$ALL_STAGED" | grep 'Cargo\.toml$' || true)
if [ -n "$TOML_STAGED" ]; then
  echo "  [7/8] Cargo.toml version pinning check..." >&2
  TOML_VIOLATIONS=0
  while IFS= read -r toml_file; do
    [ -z "$toml_file" ] && continue
    local_toml="$CWD/$toml_file"
    [ ! -f "$local_toml" ] && continue

    # Check for ^, ~, *, >= in version specs (but not in comments)
    pin_matches=$(grep -n -E 'version\s*=\s*"[\^~*>]' "$local_toml" 2>/dev/null | grep -v '^[[:space:]]*#' || true)
    if [ -n "$pin_matches" ]; then
      echo "  FAIL: Unpinned versions in $toml_file:" >&2
      echo "$pin_matches" >&2
      TOML_VIOLATIONS=$((TOML_VIOLATIONS + 1))
    fi
  done <<< "$TOML_STAGED"

  if [ "$TOML_VIOLATIONS" -gt 0 ]; then
    echo "  All Cargo.toml dependencies must use exact versions. ^, ~, *, >= are BANNED." >&2
    FAILED=1
  else
    echo "  PASS: Cargo.toml versions pinned" >&2
  fi
else
  echo "  [7/8] No Cargo.toml staged — skipping version pin check" >&2
fi

# ─────────────────────────────────────────────
# GATE 8: Test count guard (ratcheting baseline)
# ─────────────────────────────────────────────
if [ -n "$RS_STAGED" ]; then
  echo "  [8/8] Test count guard..." >&2
  if [ -x "$HOOKS_DIR/test-count-guard.sh" ]; then
    if ! "$HOOKS_DIR/test-count-guard.sh" "$CWD" 2>&1; then
      FAILED=1
    fi
  else
    echo "  SKIP: test-count-guard.sh not executable" >&2
  fi
else
  echo "  [8/8] No .rs files staged — skipping test count guard" >&2
fi

# ─────────────────────────────────────────────
# FINAL VERDICT
# ─────────────────────────────────────────────
echo "" >&2
if [ "$FAILED" -ne 0 ]; then
  echo "╔══════════════════════════════════════════════╗" >&2
  echo "║  COMMIT BLOCKED — Fix violations above       ║" >&2
  echo "╚══════════════════════════════════════════════╝" >&2
  exit 2
fi

# Write state file for pre-PR gate optimization
HEAD_HASH=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
TEST_COUNT=$(grep -r '#\[test\]' crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
echo "$HEAD_HASH $(date +%s) $TEST_COUNT" > "$HOOKS_DIR/.last-quality-pass"

echo "╔══════════════════════════════════════════════╗" >&2
echo "║  ALL 8 GATES PASSED — Commit allowed         ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2
exit 0
