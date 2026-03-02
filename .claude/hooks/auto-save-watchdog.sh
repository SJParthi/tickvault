#!/bin/bash
# auto-save-watchdog.sh — Background watchdog that auto-stashes uncommitted work
# Launched by SessionStart hook. Runs every 2 minutes.
# If the session hangs/dies, stashes survive → work is recoverable.
#
# Uses "git stash create" + "git stash store" which is completely non-disruptive:
#   - Does NOT touch working tree
#   - Does NOT touch index/staging area
#   - Creates a stash snapshot silently in the background
#   - Zero interference with the running session
#
# Recovery: git stash list → git stash apply stash@{N}
# Cleanup:  git stash drop stash@{N}

set -euo pipefail

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$CWD" || exit 1

# Config
INTERVAL_SECONDS=120          # Check every 2 minutes
MIN_CHANGED_FILES=3           # Minimum dirty files to trigger auto-save
MAX_STASHES=10                # Keep only the latest N auto-save stashes
STASH_PREFIX="auto-save"      # Prefix for stash messages
PID_FILE="$CWD/.claude/hooks/.watchdog.pid"
LOG_FILE="$CWD/.claude/hooks/.watchdog.log"

# ── Guard: only one watchdog per repo ──────────────────────────────
if [ -f "$PID_FILE" ]; then
  OLD_PID=$(cat "$PID_FILE" 2>/dev/null || echo "")
  if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
    # Already running
    exit 0
  fi
  # Stale PID file, clean up
  rm -f "$PID_FILE"
fi

# Write our PID
echo $$ > "$PID_FILE"

# ── Cleanup on exit ───────────────────────────────────────────────
cleanup() {
  rm -f "$PID_FILE"
  echo "[$(date '+%H:%M:%S')] Watchdog stopped (PID $$)" >> "$LOG_FILE"
}
trap cleanup EXIT INT TERM

# ── Logging ───────────────────────────────────────────────────────
log() {
  echo "[$(date '+%H:%M:%S')] $1" >> "$LOG_FILE"
}

# Truncate log if too big (>50KB)
if [ -f "$LOG_FILE" ] && [ "$(wc -c < "$LOG_FILE" 2>/dev/null || echo 0)" -gt 51200 ]; then
  tail -50 "$LOG_FILE" > "$LOG_FILE.tmp" && mv "$LOG_FILE.tmp" "$LOG_FILE"
fi

log "Watchdog started (PID $$, interval=${INTERVAL_SECONDS}s, threshold=${MIN_CHANGED_FILES} files)"

# ── Prune old auto-save stashes ──────────────────────────────────
prune_old_stashes() {
  # List all auto-save stash indices (newest first)
  local indices=()
  local i=0
  while IFS= read -r line; do
    if echo "$line" | grep -q "${STASH_PREFIX}:"; then
      indices+=("$i")
    fi
    i=$((i + 1))
  done < <(git -C "$CWD" stash list 2>/dev/null)

  # Drop stashes beyond MAX_STASHES (drop from highest index first to avoid shift)
  if [ "${#indices[@]}" -gt "$MAX_STASHES" ]; then
    local to_drop=("${indices[@]:$MAX_STASHES}")
    local j
    for (( j=${#to_drop[@]}-1; j>=0; j-- )); do
      git -C "$CWD" stash drop "stash@{${to_drop[$j]}}" 2>/dev/null || true
    done
    log "PRUNED: dropped $((${#to_drop[@]})) old auto-save stashes"
  fi
}

# ── Main loop ─────────────────────────────────────────────────────
while true; do
  sleep "$INTERVAL_SECONDS"

  # Safety: bail if repo gone
  if [ ! -d "$CWD/.git" ]; then
    log "ERROR: .git directory gone, exiting"
    break
  fi

  # Count dirty files (tracked modifications + new untracked)
  CHANGED_FILES=$(git -C "$CWD" status --porcelain 2>/dev/null | wc -l | tr -d ' ')

  if [ "$CHANGED_FILES" -lt "$MIN_CHANGED_FILES" ]; then
    # Not enough changes to bother
    continue
  fi

  BRANCH=$(git -C "$CWD" branch --show-current 2>/dev/null || echo "unknown")
  TIMESTAMP=$(date '+%Y%m%d-%H%M%S')
  STASH_MSG="${STASH_PREFIX}: ${TIMESTAMP} branch=${BRANCH} files=${CHANGED_FILES}"

  # NON-DISRUPTIVE stash:
  # "git stash create" builds a stash commit object WITHOUT touching working tree or index.
  # "git stash store" saves that object into the stash reflog.
  # The session never notices anything happened.
  STASH_SHA=$(git -C "$CWD" stash create 2>/dev/null || echo "")

  if [ -n "$STASH_SHA" ]; then
    git -C "$CWD" stash store -m "$STASH_MSG" "$STASH_SHA" 2>/dev/null
    STORE_EXIT=$?
    if [ "$STORE_EXIT" -eq 0 ]; then
      log "SAVED: ${STASH_MSG} (${STASH_SHA:0:7})"
      # Prune old stashes to avoid unbounded growth
      prune_old_stashes
    else
      log "SKIP: stash store failed (exit=$STORE_EXIT)"
    fi
  else
    log "SKIP: stash create returned empty (nothing to stash or only untracked)"
  fi
done
