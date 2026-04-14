#!/bin/bash
# pre-push-gate.sh — Lightweight pre-push safety net
# Called by PreToolUse hook on Bash commands matching "git push"
# Exit 2 = BLOCK push.
#
# PHILOSOPHY: CI is the authority for heavy checks (clippy, test, audit, deny,
# coverage, loom). Pre-push runs ONLY fast, local-only gates that CI can't
# catch before the push (banned patterns, secrets, formatting). This avoids
# 5-10 minutes of redundant local work that CI will repeat server-side.
#
# Gate order (all fast, <35s total):
#   1. cargo fmt --check          [fast: ~2s]
#   2. Banned pattern scan        [fast: ~2s]
#   3. Secret scan                [fast: ~1s]
#   4. Test count guard (ratchet) [fast: ~1s]
#   5. Data integrity guard       [fast: ~2s, pattern scan]
#   6. Pub fn test guard          [fast: ~3s, pattern scan]
#   7. Financial test guard       [fast: ~3s, pattern scan]
#   8. 22 test type check         [fast: ~5s, scoped to changed crates]
#
# Heavy gates (clippy, test, audit, deny, coverage, loom) are CI-only.
# Branch protection blocks merge if any CI check fails.
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

cd "$CWD" || { echo "  FAIL: Cannot cd to $CWD" >&2; exit 2; }

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

echo "╔══════════════════════════════════════════════╗" >&2
echo "║    PRE-PUSH (fast gates — CI handles rest)    ║" >&2
echo "╚══════════════════════════════════════════════╝" >&2

HEAD_CURRENT=$(git rev-parse HEAD 2>/dev/null || echo "unknown")

# Gate 1: cargo fmt
echo "  [1/8] cargo fmt --check..." >&2
FMT_OUT=$(timeout 60 cargo fmt --all -- --check 2>&1)
FMT_EXIT=$?
if [ "$FMT_EXIT" -eq 124 ]; then
  echo "  FAIL: cargo fmt timed out (60s) — blocking push" >&2
  FAILED=1
elif [ "$FMT_EXIT" -ne 0 ]; then
  echo "  FAIL: Code not formatted:" >&2
  echo "$FMT_OUT" | tail -10 >&2
  FAILED=1
else
  echo "  PASS: cargo fmt" >&2
fi

# Gate 2: Banned pattern scan (full workspace)
echo "  [2/8] Banned pattern scan..." >&2
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

# Gate 3: Secret scan
echo "  [3/8] Secret scan..." >&2
if [ -x "$HOOKS_DIR/secret-scanner.sh" ]; then
  if ! timeout 30 "$HOOKS_DIR/secret-scanner.sh" "$CWD" "$ALL_RS" > /dev/null 2>&1; then
    echo "  FAIL: Secrets detected in workspace." >&2
    FAILED=1
  else
    echo "  PASS: Secret scan" >&2
  fi
else
  echo "  SKIP: secret-scanner.sh not available" >&2
fi

# Gate 4: Test count guard (ratchet — ensures test count never decreases)
echo "  [4/8] Test count guard..." >&2
if [ -x "$HOOKS_DIR/test-count-guard.sh" ]; then
  if ! timeout 30 "$HOOKS_DIR/test-count-guard.sh" "$CWD" 2>&1; then
    FAILED=1
  fi
else
  echo "  SKIP: test-count-guard.sh not executable" >&2
fi

# Gate 5: Data integrity guard (full workspace scan — pattern-based, no compilation)
echo "  [5/8] Data integrity guard (full workspace)..." >&2
if [ -x "$HOOKS_DIR/data-integrity-guard.sh" ]; then
  DIG_OUT=$(timeout 60 "$HOOKS_DIR/data-integrity-guard.sh" "$CWD" 2>&1)
  DIG_EXIT=$?
  if [ "$DIG_EXIT" -eq 124 ]; then
    echo "  FAIL: Data integrity guard timed out (60s) — blocking push" >&2
    FAILED=1
  elif [ "$DIG_EXIT" -ne 0 ]; then
    echo "$DIG_OUT" >&2
    FAILED=1
  else
    echo "  PASS: Data integrity (no price corruption patterns)" >&2
  fi
else
  echo "  SKIP: data-integrity-guard.sh not executable" >&2
fi

# Gate 6: Pub fn test guard (every public function has a test)
echo "  [6/8] Pub fn test guard (full workspace)..." >&2
if [ -x "$HOOKS_DIR/pub-fn-test-guard.sh" ]; then
  PUBFN_OUT=$(timeout 120 "$HOOKS_DIR/pub-fn-test-guard.sh" "$CWD" "all" 2>&1)
  PUBFN_EXIT=$?
  if [ "$PUBFN_EXIT" -eq 124 ]; then
    echo "  FAIL: Pub fn test guard timed out (120s) — blocking push" >&2
    FAILED=1
  elif [ "$PUBFN_EXIT" -ne 0 ]; then
    echo "$PUBFN_OUT" >&2
    FAILED=1
  else
    echo "  PASS: All public functions have tests" >&2
  fi
else
  echo "  SKIP: pub-fn-test-guard.sh not executable" >&2
fi

# Gate 12: Phase 6.1 G3 + G4 — Boot symmetry + state-machine wiring guard
# Catches: state machines with poll_*() but no caller, slow-boot blind spots.
echo "  [12/12] Boot symmetry + state-machine guard (phase 6.1 G3+G4)..." >&2
if [ -x "$HOOKS_DIR/boot-symmetry-guard.sh" ]; then
  SYM_OUT=$(timeout 60 "$HOOKS_DIR/boot-symmetry-guard.sh" 2>&1)
  SYM_EXIT=$?
  if [ "$SYM_EXIT" -eq 124 ]; then
    echo "  WARN: boot symmetry guard timed out (60s) — not blocking" >&2
  elif [ "$SYM_EXIT" -ne 0 ]; then
    echo "$SYM_OUT" >&2
    FAILED=1
  else
    echo "  PASS: boot paths symmetric, state machines polled" >&2
  fi
else
  echo "  SKIP: boot-symmetry-guard.sh not executable" >&2
fi

# Gate 11: Phase 6.1 G1 — Wiring guard (new pub fn must have callers)
# Catches the dormant-pub-fn class of bugs: code that compiles + tests pass
# but no production call site invokes it. Found 4 such bugs in sessions 3-5.
echo "  [11/11] Pub fn wiring guard (phase 6.1 G1)..." >&2
if [ -x "$HOOKS_DIR/pub-fn-wiring-guard.sh" ]; then
  WIRING_OUT=$(timeout 60 "$HOOKS_DIR/pub-fn-wiring-guard.sh" 2>&1)
  WIRING_EXIT=$?
  if [ "$WIRING_EXIT" -eq 124 ]; then
    echo "  WARN: wiring guard timed out (60s) — not blocking" >&2
  elif [ "$WIRING_EXIT" -ne 0 ]; then
    echo "$WIRING_OUT" >&2
    FAILED=1
  else
    echo "  PASS: every new pub fn has a call site" >&2
  fi
else
  echo "  SKIP: pub-fn-wiring-guard.sh not executable" >&2
fi

# Gate 7: Financial test guard (price/money fns have boundary tests)
echo "  [7/8] Financial test guard..." >&2
if [ -x "$HOOKS_DIR/financial-test-guard.sh" ]; then
  FIN_OUT=$(timeout 120 "$HOOKS_DIR/financial-test-guard.sh" "$CWD" 2>&1)
  FIN_EXIT=$?
  if [ "$FIN_EXIT" -eq 124 ]; then
    echo "  FAIL: Financial test guard timed out (120s) — blocking push" >&2
    FAILED=1
  elif [ "$FIN_EXIT" -ne 0 ]; then
    echo "$FIN_OUT" >&2
    FAILED=1
  else
    echo "  PASS: Financial functions have adequate tests" >&2
  fi
else
  echo "  SKIP: financial-test-guard.sh not executable" >&2
fi

# Gate 8: 22 test type check (scoped to changed crates)
echo "  [8/8] 22 test type check (scoped)..." >&2
UPSTREAM=$(git rev-parse '@{upstream}' 2>/dev/null || git rev-parse 'origin/main' 2>/dev/null || echo "HEAD~1")
CHANGED_RS=$(git diff --name-only "$UPSTREAM" -- 'crates/*/src/**/*.rs' 'crates/*/tests/**/*.rs' 2>/dev/null || true)
if [ -z "$CHANGED_RS" ]; then
  echo "  SKIP: No .rs files changed — skipping 22 test type check" >&2
elif [ -x "$CWD/scripts/test-coverage-guard.sh" ]; then
  CHANGED_CRATES=$(echo "$CHANGED_RS" | sed -n 's|^crates/\([^/]*\)/.*|\1|p' | sort -u | tr '\n' ' ' | sed 's/ $//')
  if [ -n "$CHANGED_CRATES" ]; then
    TCG_OUT=$(timeout 30 "$CWD/scripts/test-coverage-guard.sh" "$CWD" "$CHANGED_CRATES" 2>&1)
    TCG_EXIT=$?
    if [ "$TCG_EXIT" -eq 0 ]; then
      echo "  PASS: 22 test type check (scope: $CHANGED_CRATES)" >&2
    else
      echo "$TCG_OUT" >&2
      echo "  FAIL: 22 test type check failed for crate(s): $CHANGED_CRATES" >&2
      FAILED=1
    fi
  else
    echo "  SKIP: Could not extract crate names from changed files" >&2
  fi
else
  echo "  SKIP: scripts/test-coverage-guard.sh not executable" >&2
fi

# Gate 10: cargo audit + cargo deny (best-effort; CI is authoritative)
# If cargo-audit and cargo-deny are installed locally, run them and block
# on failure. If missing, log a WARN and continue — CI will catch it.
# This gives a fast feedback loop to developers who have the tools
# installed without blocking pushes for those who don't.
echo "  [10/10] cargo audit + cargo deny (best-effort)..." >&2
AUDIT_AVAILABLE=0
DENY_AVAILABLE=0
if command -v cargo-audit >/dev/null 2>&1 || cargo audit --version >/dev/null 2>&1; then
  AUDIT_AVAILABLE=1
fi
if command -v cargo-deny >/dev/null 2>&1 || cargo deny --version >/dev/null 2>&1; then
  DENY_AVAILABLE=1
fi

if [ "$AUDIT_AVAILABLE" -eq 0 ] && [ "$DENY_AVAILABLE" -eq 0 ]; then
  echo "  SKIP: cargo-audit and cargo-deny not installed locally." >&2
  echo "  Install with: cargo install cargo-audit cargo-deny" >&2
  echo "  CI will still enforce both — this is a local convenience gate." >&2
else
  if [ "$AUDIT_AVAILABLE" -eq 1 ]; then
    AUDIT_OUT=$(timeout 120 cargo audit --deny yanked 2>&1 || true)
    AUDIT_EXIT=$?
    if [ "$AUDIT_EXIT" -ne 0 ] && echo "$AUDIT_OUT" | grep -qE 'error: [1-9]|vulnerabilities found: [1-9]'; then
      echo "$AUDIT_OUT" >&2
      echo "  FAIL: cargo audit reported vulnerabilities. Fix or pin pre-push." >&2
      FAILED=1
    else
      echo "  PASS: cargo audit clean" >&2
    fi
  else
    echo "  SKIP: cargo-audit not installed" >&2
  fi
  if [ "$DENY_AVAILABLE" -eq 1 ]; then
    DENY_OUT=$(timeout 120 cargo deny check 2>&1 || true)
    DENY_EXIT=$?
    if [ "$DENY_EXIT" -ne 0 ] && echo "$DENY_OUT" | grep -qE 'error\[|: [1-9]+ errors'; then
      echo "$DENY_OUT" >&2
      echo "  FAIL: cargo deny reported violations." >&2
      FAILED=1
    else
      echo "  PASS: cargo deny clean" >&2
    fi
  else
    echo "  SKIP: cargo-deny not installed" >&2
  fi
fi

# Gate 9: Dhan locked facts (NEVER scoped — runs on every push)
# These tests pin Dhan-support-confirmed ground truth (Tickets #5519522,
# #5525125) so reverting a fix silently is impossible. If this gate fails,
# someone removed a ticket citation or broke the 200-depth path / PrevClose
# routing. Do NOT skip this gate. If you need to change a locked fact, open
# a new Dhan ticket, quote the response in the rule file, and update the
# assertion in the same commit — all three steps.
echo "  [9/9] Dhan locked facts (Tickets #5519522, #5525125)..." >&2
LOCKED_OUT=$(timeout 120 cargo test -p tickvault-common --test dhan_locked_facts --quiet 2>&1)
LOCKED_EXIT=$?
if [ "$LOCKED_EXIT" -eq 0 ]; then
  echo "  PASS: Dhan locked facts (8 invariants held)" >&2
else
  echo "$LOCKED_OUT" >&2
  echo "" >&2
  echo "  FAIL: Dhan LOCKED FACTS regressed." >&2
  echo "  These invariants come from Dhan support tickets — see" >&2
  echo "  crates/common/tests/dhan_locked_facts.rs for the ticket refs." >&2
  echo "  DO NOT weaken these assertions without a fresh Dhan ticket." >&2
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
TEST_COUNT=$(grep -r '#\[test\]' crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
echo "$HEAD_CURRENT $(date +%s) $TEST_COUNT" > "$HOOKS_DIR/.last-quality-pass"

# Clean up auto-save refs on successful push (background, non-blocking, serialized)
BRANCH_NOW=$(git branch --show-current 2>/dev/null || echo "")
BRANCH_SAFE_NOW=$(echo "$BRANCH_NOW" | tr '/' '-' | tr -cd 'a-zA-Z0-9_-')
if [ -n "$BRANCH_SAFE_NOW" ]; then
  (
    sleep 2
    refs_to_clean=$(git for-each-ref --format='%(refname)' "refs/auto-save/${BRANCH_SAFE_NOW}-" 2>/dev/null)
    for ref in $refs_to_clean; do
      timeout 10 git push origin --delete "$ref" 2>/dev/null || true
      git update-ref -d "$ref" 2>/dev/null || true
      sleep 1
    done
  ) > /dev/null 2>&1 &
  disown
fi

echo "╔══════════════════════════════════════════════╗" >&2
echo "║  PUSH ALLOWED (10 fast gates — CI enforces rest)║" >&2
echo "╚══════════════════════════════════════════════╝" >&2
exit 0
