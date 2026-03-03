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
#   9. Commit message format (conventional commits)
#  10. Typos check (staged files)
#  11. Enforcement file integrity (deny list + hook script content)
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

cd "$CWD" || { echo "FAIL: cannot cd to $CWD" >&2; exit 2; }

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
echo "║        PRE-COMMIT QUALITY GATE (11 Gates)    ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2

# ─────────────────────────────────────────────
# GATE 1-3: Only if Rust files are staged
# ─────────────────────────────────────────────
if [ -n "$RS_STAGED" ]; then

  # Gate 1: cargo fmt (show errors on failure)
  echo "  [1/10] cargo fmt --check..." >&2
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
  echo "  [2/10] cargo clippy..." >&2
  CLIPPY_OUT=$(cargo clippy --workspace --all-targets -- -D warnings -D clippy::arithmetic_side_effects -D clippy::indexing_slicing -D clippy::as_conversions 2>&1)
  CLIPPY_EXIT=$?
  if [ "$CLIPPY_EXIT" -ne 0 ]; then
    echo "  FAIL: cargo clippy has warnings:" >&2
    echo "$CLIPPY_OUT" | tail -20 >&2
    FAILED=1
  else
    echo "  PASS: cargo clippy (zero warnings)" >&2
  fi

  # Gate 3: cargo test (show errors on failure)
  echo "  [3/10] cargo test..." >&2
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
  echo "  [4/10] Banned pattern scan..." >&2
  if ! echo "$RS_STAGED" | "$HOOKS_DIR/banned-pattern-scanner.sh" "$CWD" "$RS_STAGED" 2>&1; then
    FAILED=1
  fi

  # Gate 5: O(1) latency & dedup scanner
  echo "  [5/10] O(1) latency & dedup scan..." >&2
  if ! echo "$RS_STAGED" | "$HOOKS_DIR/dedup-latency-scanner.sh" "$CWD" "$RS_STAGED" 2>&1; then
    FAILED=1
  fi

else
  echo "  [1-3/10] No .rs files staged — skipping cargo gates" >&2
  echo "  [4-5/10] No .rs files staged — skipping Rust scanners" >&2
fi

# ─────────────────────────────────────────────
# GATE 6: Secret scanner (runs on ALL staged files, not just .rs)
# ─────────────────────────────────────────────
echo "  [6/10] Secret scan..." >&2
if ! echo "$ALL_STAGED" | "$HOOKS_DIR/secret-scanner.sh" "$CWD" "$ALL_STAGED" 2>&1; then
  FAILED=1
fi

# ─────────────────────────────────────────────
# GATE 7: Cargo.toml version pinning (no ^, ~, *, >= in deps)
# ─────────────────────────────────────────────
TOML_STAGED=$(echo "$ALL_STAGED" | grep 'Cargo\.toml$' || true)
if [ -n "$TOML_STAGED" ]; then
  echo "  [7/10] Cargo.toml version pinning check..." >&2
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
  echo "  [7/10] No Cargo.toml staged — skipping version pin check" >&2
fi

# ─────────────────────────────────────────────
# GATE 8: Test count guard (ratcheting baseline)
# ─────────────────────────────────────────────
if [ -n "$RS_STAGED" ]; then
  echo "  [8/10] Test count guard..." >&2
  if [ -x "$HOOKS_DIR/test-count-guard.sh" ]; then
    if ! "$HOOKS_DIR/test-count-guard.sh" "$CWD" 2>&1; then
      FAILED=1
    fi
  else
    echo "  SKIP: test-count-guard.sh not executable" >&2
  fi
else
  echo "  [8/10] No .rs files staged — skipping test count guard" >&2
fi

# ─────────────────────────────────────────────
# GATE 9: Commit message format (conventional commits)
# Validates the commit message being created follows convention.
# We extract the message from the git commit command itself.
# ─────────────────────────────────────────────
echo "  [9/10] Commit message format..." >&2
# Extract -m "message" from the git commit command
COMMIT_MSG=$(echo "$COMMAND" | grep -oP '(?<=-m\s["\x27])[^"\x27]+' 2>/dev/null | head -1 || true)
if [ -z "$COMMIT_MSG" ]; then
  # Try extracting from heredoc pattern: -m "$(cat <<'EOF' ... EOF )"
  COMMIT_MSG=$(echo "$COMMAND" | grep -oP '(?<=-m\s").*' 2>/dev/null | head -1 || true)
fi
if [ -n "$COMMIT_MSG" ]; then
  # Extract first line only
  FIRST_LINE=$(echo "$COMMIT_MSG" | head -1)
  # Allow: conventional commits, merge commits, revert commits
  if echo "$FIRST_LINE" | grep -qE '^(Merge|Revert) '; then
    echo "  PASS: Merge/Revert commit" >&2
  elif echo "$FIRST_LINE" | grep -qE '^(feat|fix|refactor|test|docs|chore|perf|security)(\([a-z0-9_/-]+\))?: .+'; then
    echo "  PASS: Conventional commit format" >&2
  else
    echo "  FAIL: Commit message does not follow conventional format." >&2
    echo "  Expected: <type>(<scope>): <description>" >&2
    echo "  Got: $FIRST_LINE" >&2
    FAILED=1
  fi
else
  echo "  SKIP: Could not extract commit message from command" >&2
fi

# ─────────────────────────────────────────────
# GATE 10: Typos check (staged files)
# ─────────────────────────────────────────────
echo "  [10/10] Typos check..." >&2
if command -v typos > /dev/null 2>&1; then
  # Check only staged files to keep it fast
  TYPO_FILES=$(git diff --cached --name-only --diff-filter=ACMR 2>/dev/null | grep -E '\.(rs|toml|md|yml|yaml|sh)$' || true)
  if [ -n "$TYPO_FILES" ]; then
    TYPOS_OUT=""
    TYPOS_FAIL=0
    while IFS= read -r tf; do
      [ -z "$tf" ] && continue
      [ ! -f "$tf" ] && continue
      RESULT=$(typos "$tf" 2>&1 || true)
      if [ -n "$RESULT" ]; then
        TYPOS_OUT="${TYPOS_OUT}${RESULT}\n"
        TYPOS_FAIL=1
      fi
    done <<< "$TYPO_FILES"
    if [ "$TYPOS_FAIL" -ne 0 ]; then
      echo "  FAIL: Typos found:" >&2
      echo -e "$TYPOS_OUT" | head -20 >&2
      FAILED=1
    else
      echo "  PASS: No typos in staged files" >&2
    fi
  else
    echo "  SKIP: No checkable files staged" >&2
  fi
else
  echo "  FAIL: typos-cli not installed (install: cargo install typos-cli)" >&2
  FAILED=1
fi

# ─────────────────────────────────────────────
# GATE 11: Enforcement file integrity
# Prevents weakening of settings.json deny list or hook scripts
# ─────────────────────────────────────────────
echo "  [11/11] Enforcement integrity check..." >&2
ENFORCEMENT_FILES=$(git diff --cached --name-only | grep -E '^\.claude/(settings\.json|hooks/.*\.sh)$|^scripts/git-hooks/' || true)
if [ -n "$ENFORCEMENT_FILES" ]; then
  # Check that settings.json deny list hasn't shrunk
  if echo "$ENFORCEMENT_FILES" | grep -q 'settings.json'; then
    OLD_DENY=$(git show HEAD:.claude/settings.json 2>/dev/null | jq '.permissions.deny | length' 2>/dev/null || echo 0)
    NEW_DENY=$(jq '.permissions.deny | length' .claude/settings.json 2>/dev/null || echo 0)
    if [ "$NEW_DENY" -lt "$OLD_DENY" ]; then
      echo "  FAIL: .claude/settings.json deny list shrunk ($OLD_DENY -> $NEW_DENY). Enforcement weakened." >&2
      FAILED=1
    else
      echo "  PASS: settings.json deny list intact ($OLD_DENY -> $NEW_DENY)" >&2
    fi
  fi
  # Check that no hook script had gates removed (line count shouldn't drop >20%)
  while IFS= read -r ef; do
    [ -z "$ef" ] && continue
    if echo "$ef" | grep -qE '\.sh$'; then
      OLD_LINES=$(git show "HEAD:$ef" 2>/dev/null | wc -l || echo 0)
      NEW_LINES=$(wc -l < "$ef" 2>/dev/null || echo 0)
      THRESHOLD=$(( OLD_LINES * 80 / 100 ))
      if [ "$OLD_LINES" -gt 0 ] && [ "$NEW_LINES" -lt "$THRESHOLD" ]; then
        echo "  FAIL: $ef lost >20% of content ($OLD_LINES -> $NEW_LINES lines). Possible gate removal." >&2
        FAILED=1
      fi
    fi
  done <<< "$ENFORCEMENT_FILES"
  if [ "$FAILED" -eq 0 ]; then
    echo "  PASS: Enforcement files intact" >&2
  fi
else
  echo "  PASS: No enforcement files staged" >&2
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
echo "║  ALL 11 GATES PASSED — Commit allowed         ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2
exit 0
