#!/bin/bash
# session-status.sh — One-glance dashboard of all safety systems
#
# Run anytime, any session, zero token cost. Shows:
#   - Branch + remote sync
#   - Quality gate status
#   - Auto-save watchdogs (local + remote)
#   - Session collision warnings
#   - Test count vs baseline
#   - Phase progress
#   - Modified files
#
# Usage: .claude/hooks/session-status.sh

set -uo pipefail

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$CWD" || { echo "ERROR: Cannot cd to $CWD" >&2; exit 1; }

HOOKS_DIR="$CWD/.claude/hooks"
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color
BOLD='\033[1m'

ok() { echo -e "  ${GREEN}OK${NC}  $1" >&2; }
warn() { echo -e "  ${YELLOW}!!${NC}  $1" >&2; }
fail() { echo -e "  ${RED}XX${NC}  $1" >&2; }
info() { echo -e "  ${CYAN}--${NC}  $1" >&2; }

echo -e "${BOLD}╔══════════════════════════════════════════════════╗${NC}" >&2
echo -e "${BOLD}║         SESSION STATUS DASHBOARD                  ║${NC}" >&2
echo -e "${BOLD}╚══════════════════════════════════════════════════╝${NC}" >&2
echo "" >&2

# ── 1. Branch & Sync ────────────────────────────────────────────
echo -e "${BOLD}[Branch & Sync]${NC}" >&2
BRANCH=$(git -C "$CWD" branch --show-current 2>/dev/null || echo "detached")
info "Branch: ${BRANCH}"

# Check if branch exists on remote
REMOTE_SHA=$(git -C "$CWD" ls-remote --heads origin "$BRANCH" 2>/dev/null | awk '{print $1}')
LOCAL_SHA=$(git -C "$CWD" rev-parse HEAD 2>/dev/null || echo "unknown")
if [ -z "$REMOTE_SHA" ]; then
  warn "Branch NOT on remote — not pushed yet"
elif [ "$REMOTE_SHA" = "$LOCAL_SHA" ]; then
  ok "Synced with remote (${LOCAL_SHA:0:7})"
else
  warn "Out of sync — local: ${LOCAL_SHA:0:7}, remote: ${REMOTE_SHA:0:7}"
fi

# Dirty files
DIRTY_COUNT=$(git -C "$CWD" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
if [ "$DIRTY_COUNT" -eq 0 ]; then
  ok "Working tree: clean"
else
  warn "Working tree: ${DIRTY_COUNT} uncommitted files"
fi
echo "" >&2

# ── 2. Quality Gate ─────────────────────────────────────────────
echo -e "${BOLD}[Quality Gate]${NC}" >&2
STATE_FILE="$HOOKS_DIR/.last-quality-pass"
if [ -f "$STATE_FILE" ]; then
  STATE_CONTENT=$(cat "$STATE_FILE" 2>/dev/null)
  STATE_HASH=$(echo "$STATE_CONTENT" | awk '{print $1}')
  STATE_TS=$(echo "$STATE_CONTENT" | awk '{print $2}')
  STATE_TESTS=$(echo "$STATE_CONTENT" | awk '{print $3}')
  NOW=$(date +%s)
  AGE=$((NOW - STATE_TS))
  AGE_MIN=$((AGE / 60))

  if [ "$STATE_HASH" = "$LOCAL_SHA" ]; then
    ok "Last pass: current HEAD (${AGE_MIN}m ago, ${STATE_TESTS} tests)"
  else
    warn "Last pass: ${STATE_HASH:0:7} (${AGE_MIN}m ago) — HEAD has moved since"
  fi
else
  warn "No quality gate pass recorded yet"
fi
echo "" >&2

# ── 3. Auto-Save Watchdogs ─────────────────────────────────────
echo -e "${BOLD}[Auto-Save Watchdogs]${NC}" >&2

# Local stash watchdog
LOCAL_PID_FILE="$HOOKS_DIR/.watchdog.pid"
if [ -f "$LOCAL_PID_FILE" ]; then
  LOCAL_PID=$(cat "$LOCAL_PID_FILE" 2>/dev/null || echo "")
  if [ -n "$LOCAL_PID" ] && kill -0 "$LOCAL_PID" 2>/dev/null; then
    ok "Local stash watchdog: running (PID ${LOCAL_PID})"
  else
    fail "Local stash watchdog: dead (stale PID ${LOCAL_PID})"
  fi
else
  warn "Local stash watchdog: not running"
fi

# Check last local stash
LAST_STASH=$(git -C "$CWD" stash list 2>/dev/null | grep "auto-save:" | head -1)
if [ -n "$LAST_STASH" ]; then
  info "Last stash: ${LAST_STASH}"
fi

# Remote watchdog
REMOTE_PID_FILE="$HOOKS_DIR/.remote-watchdog.pid"
if [ -f "$REMOTE_PID_FILE" ]; then
  REMOTE_PID=$(cat "$REMOTE_PID_FILE" 2>/dev/null || echo "")
  if [ -n "$REMOTE_PID" ] && kill -0 "$REMOTE_PID" 2>/dev/null; then
    ok "Remote push watchdog: running (PID ${REMOTE_PID})"
  else
    fail "Remote push watchdog: dead (stale PID ${REMOTE_PID})"
  fi
else
  warn "Remote push watchdog: not running"
fi

# Check last remote log entry
REMOTE_LOG="$HOOKS_DIR/.remote-watchdog.log"
if [ -f "$REMOTE_LOG" ]; then
  LAST_PUSH=$(grep "PUSHED:" "$REMOTE_LOG" 2>/dev/null | tail -1)
  LAST_SNAP=$(grep "SNAPSHOT" "$REMOTE_LOG" 2>/dev/null | tail -1)
  if [ -n "$LAST_PUSH" ]; then
    info "Last remote push: ${LAST_PUSH}"
  fi
  if [ -n "$LAST_SNAP" ]; then
    info "Last snapshot: ${LAST_SNAP}"
  fi
fi

# Check remote auto-save refs
REMOTE_AUTO_REFS=$(git -C "$CWD" ls-remote origin 'refs/auto-save/*' 2>/dev/null | wc -l | tr -d ' ')
if [ "$REMOTE_AUTO_REFS" -gt 0 ]; then
  ok "Remote auto-save refs: ${REMOTE_AUTO_REFS} snapshots on GitHub"
else
  info "Remote auto-save refs: none yet"
fi
echo "" >&2

# ── 4. Collision Detection ─────────────────────────────────────
echo -e "${BOLD}[Session Collisions]${NC}" >&2
CONFLICT_FILE="$HOOKS_DIR/.conflict-warning"
if [ -f "$CONFLICT_FILE" ]; then
  fail "COLLISION DETECTED:"
  cat "$CONFLICT_FILE" | sed 's/^/       /' >&2
else
  ok "No session collisions detected"
fi
echo "" >&2

# ── 5. Test Baseline ───────────────────────────────────────────
echo -e "${BOLD}[Test Count]${NC}" >&2
BASELINE_FILE="$HOOKS_DIR/.test-count-baseline"
if [ -f "$BASELINE_FILE" ]; then
  BASELINE=$(cat "$BASELINE_FILE" | tr -d ' \n')
  CURRENT_TESTS=$(grep -r '#\[test\]' crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
  if [ "$CURRENT_TESTS" -ge "$BASELINE" ]; then
    ok "Tests: ${CURRENT_TESTS} (baseline: ${BASELINE}) — ratchet OK"
  else
    fail "Tests: ${CURRENT_TESTS} < baseline ${BASELINE} — REGRESSION"
  fi
else
  CURRENT_TESTS=$(grep -r '#\[test\]' crates/ --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
  info "Tests: ${CURRENT_TESTS} (no baseline set)"
fi
echo "" >&2

# ── 6. Phase Progress ──────────────────────────────────────────
echo -e "${BOLD}[Phase]${NC}" >&2
PHASE_FILE="$CWD/docs/phases/PHASE_1_LIVE_TRADING.md"
if [ -f "$PHASE_FILE" ]; then
  # Count completed tasks (lines with [x]) vs total (lines with [ ] or [x])
  DONE=$(grep -c '\[x\]' "$PHASE_FILE" 2>/dev/null || echo "0")
  TOTAL=$(grep -c '\[.\]' "$PHASE_FILE" 2>/dev/null || echo "0")
  if [ "$TOTAL" -gt 0 ]; then
    PCT=$((DONE * 100 / TOTAL))
    info "Phase 1 — Live Trading: ${DONE}/${TOTAL} tasks (${PCT}%)"
  else
    info "Phase 1 — Live Trading: progress format not detected"
  fi
else
  warn "Phase doc not found"
fi
echo "" >&2

# ── 7. Recent Commits ──────────────────────────────────────────
echo -e "${BOLD}[Recent Commits]${NC}" >&2
git -C "$CWD" log --oneline -5 2>/dev/null | while read -r line; do
  info "$line"
done
echo "" >&2

echo -e "${BOLD}╔══════════════════════════════════════════════════╗${NC}" >&2
echo -e "${BOLD}║  Three Principles: Zero alloc | O(1) | Pinned    ║${NC}" >&2
echo -e "${BOLD}╚══════════════════════════════════════════════════╝${NC}" >&2
