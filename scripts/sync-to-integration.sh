#!/usr/bin/env bash
# =============================================================================
# sync-to-integration.sh — Sync current session branch to claude/integration
# =============================================================================
# Called by PostToolUse hook after every git push.
# Merges the session branch into claude/integration and pushes.
#
# Conflict resolution: -X theirs (session branch wins — latest work takes priority)
# If merge still fails: checkout --theirs on conflicted files, then force-add.
# If THAT fails: abort and log warning (manual intervention needed).
# =============================================================================

set -uo pipefail

INTEGRATION_BRANCH="claude/integration"
LOG_FILE="${CLAUDE_PROJECT_DIR:-.}/.claude/hooks/.integration-sync.log"

log() {
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$LOG_FILE"
}

# Get current branch
CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)

# Skip if we're already on integration branch (avoid infinite loop)
if [ "$CURRENT_BRANCH" = "$INTEGRATION_BRANCH" ]; then
  exit 0
fi

# Skip if not a claude/* session branch
case "$CURRENT_BRANCH" in
  claude/*) ;;
  *) exit 0 ;;
esac

log "START: syncing $CURRENT_BRANCH → $INTEGRATION_BRANCH"

# Stash any uncommitted changes (safety net)
STASH_NEEDED=false
if ! git diff --quiet 2>/dev/null || ! git diff --cached --quiet 2>/dev/null; then
  git stash push -m "sync-to-integration-autostash" --quiet 2>/dev/null
  STASH_NEEDED=true
fi

cleanup() {
  # Always return to session branch
  git checkout "$CURRENT_BRANCH" --quiet 2>/dev/null
  if [ "$STASH_NEEDED" = true ]; then
    git stash pop --quiet 2>/dev/null || true
  fi
}
trap cleanup EXIT

# Fetch integration branch (create from main if it doesn't exist remotely)
if git fetch origin "$INTEGRATION_BRANCH" 2>/dev/null; then
  git checkout "$INTEGRATION_BRANCH" --quiet 2>/dev/null
  git reset --hard "origin/$INTEGRATION_BRANCH" --quiet 2>/dev/null
else
  # Integration branch doesn't exist yet — create from main
  git fetch origin main 2>/dev/null
  git checkout -b "$INTEGRATION_BRANCH" origin/main --quiet 2>/dev/null
fi

# Merge session branch with auto-conflict resolution (session wins)
if git merge "$CURRENT_BRANCH" -X theirs --no-edit \
    -m "chore(integration): merge $CURRENT_BRANCH" 2>/dev/null; then
  log "MERGED: clean merge of $CURRENT_BRANCH"
else
  log "CONFLICT: attempting auto-resolve for $CURRENT_BRANCH"

  # Force-resolve all conflicts by taking session branch version
  CONFLICTED=$(git diff --name-only --diff-filter=U 2>/dev/null)
  if [ -n "$CONFLICTED" ]; then
    echo "$CONFLICTED" | while IFS= read -r file; do
      git checkout --theirs "$file" 2>/dev/null && git add "$file" 2>/dev/null
      log "  RESOLVED: $file (took session version)"
    done

    # Check if there are still unresolved conflicts
    REMAINING=$(git diff --name-only --diff-filter=U 2>/dev/null)
    if [ -n "$REMAINING" ]; then
      log "ABORT: unresolvable conflicts in: $REMAINING"
      git merge --abort 2>/dev/null
      echo '{"systemMessage":"Integration sync: merge conflicts could not be auto-resolved. Run ./scripts/integrate-sessions.sh manually."}'
      exit 0
    fi

    git commit --no-edit -m "chore(integration): merge $CURRENT_BRANCH (conflicts auto-resolved)" 2>/dev/null
    log "RESOLVED: all conflicts auto-resolved for $CURRENT_BRANCH"
  else
    # No conflicted files but merge failed — unknown error
    git merge --abort 2>/dev/null
    log "ABORT: merge failed for unknown reason"
    exit 0
  fi
fi

# Push integration branch (with retry)
PUSH_OK=false
for i in 1 2 3 4; do
  if git push -u origin "$INTEGRATION_BRANCH" 2>/dev/null; then
    PUSH_OK=true
    log "PUSHED: $INTEGRATION_BRANCH (attempt $i)"
    break
  fi
  WAIT=$((2 ** i))
  log "RETRY: push failed, waiting ${WAIT}s (attempt $i)"
  sleep "$WAIT"
done

if [ "$PUSH_OK" = false ]; then
  log "FAIL: could not push $INTEGRATION_BRANCH after 4 attempts"
  echo '{"systemMessage":"Integration sync: push to claude/integration failed after 4 retries."}'
fi

log "END: sync complete"
exit 0
