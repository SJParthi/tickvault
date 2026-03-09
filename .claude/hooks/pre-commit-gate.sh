#!/bin/bash
# pre-commit-gate.sh — Fast quality gate for git commit
# Called by PreToolUse hook on Bash commands matching "git commit"
# Exit 2 = BLOCK commit. This is the final checkpoint.
#
# FAST gates only (< 5s total):
#   1. cargo fmt --check
#   2. Banned pattern scanner
#   3. O(1) latency & dedup scanner
#   4. Secret scanner
#   5. Cargo.toml version pinning
#   6. Commit message format
#   7. Typos check
#
# Heavy gates (clippy, test, audit, deny) run on PUSH only.
# ALL gates must pass. One failure = commit blocked.
# On success, writes state file for pre-push gate optimization.

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

cd "$CWD" || { echo "  FAIL: Cannot cd to $CWD" >&2; exit 2; }

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
echo "║      PRE-COMMIT FAST GATE (7 Gates)          ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2

# ─────────────────────────────────────────────
# GATE 1: cargo fmt only (fast, no clippy/test — those run on push)
# ─────────────────────────────────────────────
if [ -n "$RS_STAGED" ]; then

  echo "  [1/7] cargo fmt --check..." >&2
  FMT_OUT=$(timeout 60 cargo fmt --all -- --check 2>&1)
  FMT_EXIT=$?
  if [ "$FMT_EXIT" -eq 124 ]; then
    echo "  FAIL: cargo fmt timed out (60s) — treating as failure" >&2
    FAILED=1
  elif [ "$FMT_EXIT" -ne 0 ]; then
    echo "  FAIL: cargo fmt check failed:" >&2
    echo "$FMT_OUT" | tail -20 >&2
    echo "  Run 'cargo fmt --all' to fix." >&2
    FAILED=1
  else
    echo "  PASS: cargo fmt" >&2
  fi

  # Gate 2: Banned pattern scanner
  echo "  [2/7] Banned pattern scan..." >&2
  SCAN_OUT=$(timeout 30 "$HOOKS_DIR/banned-pattern-scanner.sh" "$CWD" "$RS_STAGED" 2>&1)
  SCAN_EXIT=$?
  if [ "$SCAN_EXIT" -eq 124 ]; then
    echo "  FAIL: Banned pattern scanner timed out (30s)" >&2
    FAILED=1
  elif [ "$SCAN_EXIT" -ne 0 ]; then
    echo "$SCAN_OUT" >&2
    FAILED=1
  fi

  # Gate 3: O(1) latency & dedup scanner
  echo "  [3/7] O(1) latency & dedup scan..." >&2
  DEDUP_OUT=$(timeout 30 "$HOOKS_DIR/dedup-latency-scanner.sh" "$CWD" "$RS_STAGED" 2>&1)
  DEDUP_EXIT=$?
  if [ "$DEDUP_EXIT" -eq 124 ]; then
    echo "  FAIL: O(1) latency scanner timed out (30s)" >&2
    FAILED=1
  elif [ "$DEDUP_EXIT" -ne 0 ]; then
    echo "$DEDUP_OUT" >&2
    FAILED=1
  fi

else
  echo "  [1/7] No .rs files staged — skipping fmt" >&2
  echo "  [2-3/7] No .rs files staged — skipping Rust scanners" >&2
fi

# ─────────────────────────────────────────────
# GATE 4: Secret scanner (runs on ALL staged files, not just .rs)
# ─────────────────────────────────────────────
echo "  [4/7] Secret scan..." >&2
SECRET_OUT=$(timeout 30 "$HOOKS_DIR/secret-scanner.sh" "$CWD" "$ALL_STAGED" 2>&1)
SECRET_EXIT=$?
if [ "$SECRET_EXIT" -eq 124 ]; then
  echo "  FAIL: Secret scanner timed out (30s)" >&2
  FAILED=1
elif [ "$SECRET_EXIT" -ne 0 ]; then
  echo "$SECRET_OUT" >&2
  FAILED=1
fi

# ─────────────────────────────────────────────
# GATE 5: Cargo.toml version pinning (no ^, ~, *, >= in deps)
# ─────────────────────────────────────────────
TOML_STAGED=$(echo "$ALL_STAGED" | grep 'Cargo\.toml$' || true)
if [ -n "$TOML_STAGED" ]; then
  echo "  [5/7] Cargo.toml version pinning check..." >&2
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
  echo "  [5/7] No Cargo.toml staged — skipping version pin check" >&2
fi

# ─────────────────────────────────────────────
# GATE 6: Commit message format (conventional commits)
# ─────────────────────────────────────────────
echo "  [6/7] Commit message format..." >&2
# Extract -m "message" from the git commit command (macOS-compatible, no grep -P)
COMMIT_MSG=$(echo "$COMMAND" | sed -n "s/.*-m [\"'][^\"']*[\"'].*/&/p" 2>/dev/null | sed "s/.*-m [\"']//" | sed "s/[\"'].*//" | head -1 || true)
if [ -z "$COMMIT_MSG" ]; then
  # Try extracting from heredoc pattern: -m "$(cat <<'EOF' ... EOF )"
  COMMIT_MSG=$(echo "$COMMAND" | sed -n 's/.*-m "\(.*\)/\1/p' 2>/dev/null | head -1 || true)
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
# GATE 7: Typos check (staged files)
# ─────────────────────────────────────────────
echo "  [7/7] Typos check..." >&2
if command -v typos > /dev/null 2>&1; then
  # Check only staged files to keep it fast
  TYPO_FILES=$(git diff --cached --name-only --diff-filter=ACMR 2>/dev/null | grep -E '\.(rs|toml|md|yml|yaml|sh)$' || true)
  if [ -n "$TYPO_FILES" ]; then
    TYPOS_OUT=""
    TYPOS_FAIL=0
    while IFS= read -r tf; do
      [ -z "$tf" ] && continue
      [ ! -f "$tf" ] && continue
      RESULT=$(timeout 10 typos "$tf" 2>&1 || true)
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
  echo "  SKIP: typos not installed (install: cargo install typos-cli)" >&2
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
echo "║  ALL 7 GATES PASSED — Commit allowed           ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2
exit 0
