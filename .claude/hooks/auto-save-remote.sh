#!/bin/bash
# auto-save-remote.sh — Background daemon that pushes WIP snapshots to remote
# Launched by SessionStart hook via session-sanity.sh. Runs every 2 minutes.
#
# Strategy: "Both" — single ref (latest) + last 3 timestamped refs
#
# Mechanism (git plumbing, zero disruption):
#   1. Copy index to temp file (GIT_INDEX_FILE override — real index untouched)
#   2. git add -u + untracked into temp index
#   3. git write-tree from temp index → tree SHA
#   4. git commit-tree with parent=HEAD → commit SHA (no HEAD/branch change)
#   5. git update-ref refs/auto-save/<branch>-<session>/latest $SHA
#   6. git update-ref refs/auto-save/<branch>-<session>/<timestamp> $SHA
#   7. Every 3rd cycle (~6 min): push all refs to remote, prune old timestamps
#
# Runs outside Claude Code tool pipeline → hooks do NOT fire.
# Recovery: .claude/hooks/recover-wip.sh

set -uo pipefail

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$CWD" || exit 1

# ── Config ──────────────────────────────────────────────────────
INTERVAL_SECONDS=120          # Snapshot every 2 minutes
MIN_CHANGED_FILES=1           # Minimum dirty files to trigger
PUSH_EVERY_NTH=3              # Push every Nth snapshot (~6 min)
MAX_TIMESTAMPED=3             # Keep only latest N timestamped refs
PID_FILE="$CWD/.claude/hooks/.remote-watchdog.pid"
LOG_FILE="$CWD/.claude/hooks/.remote-watchdog.log"
TEMP_INDEX="$CWD/.claude/hooks/.auto-save-index"

# Determine branch and session for ref naming
BRANCH=$(git -C "$CWD" branch --show-current 2>/dev/null || echo "detached")
BRANCH_SAFE=$(echo "$BRANCH" | tr '/' '-' | tr -cd 'a-zA-Z0-9_-')
SESSION_ID="${CLAUDE_SESSION_ID:-$(date +%s)}"
REF_BASE="refs/auto-save/${BRANCH_SAFE}-${SESSION_ID}"

# ── Guard: only one remote watchdog per repo ────────────────────
if [ -f "$PID_FILE" ]; then
  OLD_PID=$(cat "$PID_FILE" 2>/dev/null || echo "")
  if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
    exit 0
  fi
  rm -f "$PID_FILE"
fi

echo $$ > "$PID_FILE"

# ── Logging ─────────────────────────────────────────────────────
log() {
  echo "[$(date '+%H:%M:%S')] $1" >> "$LOG_FILE"
}

# Truncate log if too big (>50KB)
if [ -f "$LOG_FILE" ] && [ "$(wc -c < "$LOG_FILE" 2>/dev/null || echo 0)" -gt 51200 ]; then
  tail -50 "$LOG_FILE" > "$LOG_FILE.tmp" && mv "$LOG_FILE.tmp" "$LOG_FILE"
fi

# ── Push function ───────────────────────────────────────────────
push_to_remote() {
  local push_ok=0
  local retries=2
  local delay=5

  for attempt in $(seq 1 $retries); do
    if git -C "$CWD" push origin "${REF_BASE}/latest" 2>/dev/null; then
      push_ok=1
      break
    fi
    sleep "$delay"
    delay=$((delay * 2))
  done

  if [ "$push_ok" -eq 1 ]; then
    # Also push timestamped refs
    git -C "$CWD" push origin "${REF_BASE}/*" 2>/dev/null || true
    log "PUSHED: ${REF_BASE}/* -> origin"
  else
    log "PUSH-FAIL: Could not push ${REF_BASE} (will retry next cycle)"
  fi
}

# ── Prune old timestamped refs ──────────────────────────────────
prune_timestamped() {
  local refs=()
  while IFS= read -r ref; do
    # Skip the /latest ref
    if echo "$ref" | grep -q '/latest$'; then
      continue
    fi
    refs+=("$ref")
  done < <(git -C "$CWD" for-each-ref --sort=-committerdate --format='%(refname)' "${REF_BASE}/" 2>/dev/null)

  if [ "${#refs[@]}" -gt "$MAX_TIMESTAMPED" ]; then
    local to_drop=("${refs[@]:$MAX_TIMESTAMPED}")
    for ref in "${to_drop[@]}"; do
      git -C "$CWD" update-ref -d "$ref" 2>/dev/null || true
      git -C "$CWD" push origin --delete "$ref" 2>/dev/null || true
    done
    log "PRUNED: dropped $((${#to_drop[@]})) old timestamped refs"
  fi
}

# ── Cleanup on exit ─────────────────────────────────────────────
cleanup() {
  rm -f "$PID_FILE" "$TEMP_INDEX"
  # Final push on exit (best-effort)
  push_to_remote
  log "Remote watchdog stopped (PID $$)"
}
trap cleanup EXIT INT TERM

log "Remote watchdog started (PID $$, interval=${INTERVAL_SECONDS}s, push_every=${PUSH_EVERY_NTH}, branch=${BRANCH})"
log "Ref base: ${REF_BASE}"

# ── Main loop ───────────────────────────────────────────────────
snapshot_count=0

while true; do
  sleep "$INTERVAL_SECONDS"

  # Safety: bail if repo gone
  if [ ! -d "$CWD/.git" ]; then
    log "ERROR: .git directory gone, exiting"
    break
  fi

  # Count dirty files (tracked modifications + untracked)
  CHANGED_FILES=$(git -C "$CWD" status --porcelain 2>/dev/null | wc -l | tr -d ' ')

  if [ "$CHANGED_FILES" -lt "$MIN_CHANGED_FILES" ]; then
    continue
  fi

  # Step 1: Create temporary index from HEAD
  rm -f "$TEMP_INDEX"
  GIT_INDEX_FILE="$TEMP_INDEX" git -C "$CWD" read-tree HEAD 2>/dev/null || {
    log "SKIP: read-tree failed"
    continue
  }

  # Step 2: Add all tracked modified files to temp index
  GIT_INDEX_FILE="$TEMP_INDEX" git -C "$CWD" add -u 2>/dev/null || true

  # Step 3: Add untracked files (respecting .gitignore)
  while IFS= read -r f; do
    if [ -n "$f" ] && [ -f "$CWD/$f" ]; then
      GIT_INDEX_FILE="$TEMP_INDEX" git -C "$CWD" add -- "$f" 2>/dev/null || true
    fi
  done < <(git -C "$CWD" ls-files --others --exclude-standard 2>/dev/null)

  # Step 4: Write tree from temp index
  TREE_SHA=$(GIT_INDEX_FILE="$TEMP_INDEX" git -C "$CWD" write-tree 2>/dev/null || echo "")
  if [ -z "$TREE_SHA" ]; then
    log "SKIP: write-tree failed"
    rm -f "$TEMP_INDEX"
    continue
  fi

  # Step 5: Create commit object (parent = current HEAD)
  HEAD_SHA=$(git -C "$CWD" rev-parse HEAD 2>/dev/null || echo "")
  TIMESTAMP=$(date '+%Y%m%d-%H%M%S')
  COMMIT_MSG="auto-save: ${TIMESTAMP} branch=${BRANCH} files=${CHANGED_FILES}"

  if [ -n "$HEAD_SHA" ]; then
    COMMIT_SHA=$(git -C "$CWD" commit-tree "$TREE_SHA" -p "$HEAD_SHA" -m "$COMMIT_MSG" 2>/dev/null || echo "")
  else
    COMMIT_SHA=$(git -C "$CWD" commit-tree "$TREE_SHA" -m "$COMMIT_MSG" 2>/dev/null || echo "")
  fi

  rm -f "$TEMP_INDEX"

  if [ -z "$COMMIT_SHA" ]; then
    log "SKIP: commit-tree failed"
    continue
  fi

  # Step 6: Update primary ref (latest)
  git -C "$CWD" update-ref "${REF_BASE}/latest" "$COMMIT_SHA" 2>/dev/null || {
    log "SKIP: update-ref latest failed"
    continue
  }

  # Step 7: Create timestamped ref
  git -C "$CWD" update-ref "${REF_BASE}/${TIMESTAMP}" "$COMMIT_SHA" 2>/dev/null || true

  snapshot_count=$((snapshot_count + 1))
  log "SNAPSHOT #${snapshot_count}: ${COMMIT_SHA:0:7} (${CHANGED_FILES} files) -> ${REF_BASE}/latest + ${REF_BASE}/${TIMESTAMP}"

  # Step 8: Push to remote every Nth snapshot
  if [ $((snapshot_count % PUSH_EVERY_NTH)) -eq 0 ]; then
    push_to_remote
    prune_timestamped
  fi
done
