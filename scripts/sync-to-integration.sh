#!/usr/bin/env bash
# =============================================================================
# sync-to-integration.sh — Sync current session branch to claude/integration
# =============================================================================
# Uses git worktree — NEVER touches the main working directory.
# Claude keeps working on the session branch with zero interference.
#
# Safety guarantees:
#   1. Worktree = isolated directory. No checkout in main repo.
#   2. Lock file = no concurrent syncs. Second push waits or skips.
#   3. -X theirs = session branch always wins conflicts.
#   4. Cleanup trap = worktree removed on any exit (success, error, signal).
#   5. Background execution = never blocks Claude's session.
#   6. All errors caught = never hangs, never corrupts.
# =============================================================================

set -uo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null)}"
INTEGRATION_BRANCH="claude/integration"
LOG_FILE="$REPO_ROOT/.claude/hooks/.integration-sync.log"
LOCK_FILE="$REPO_ROOT/.claude/hooks/.integration-sync.lock"
WORKTREE_DIR="$REPO_ROOT/.claude/.integration-worktree"

# --- Logging ---
log() {
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$LOG_FILE" 2>/dev/null
}

# --- Lock (prevent concurrent syncs) ---
acquire_lock() {
  # Use mkdir as atomic lock — works on all platforms
  if mkdir "$LOCK_FILE" 2>/dev/null; then
    echo $$ > "$LOCK_FILE/pid"
    return 0
  fi

  # Lock exists — check if holder is still alive
  if [ -f "$LOCK_FILE/pid" ]; then
    OLD_PID=$(cat "$LOCK_FILE/pid" 2>/dev/null)
    if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
      # Previous sync still running — skip this one
      log "SKIP: another sync is running (PID $OLD_PID)"
      return 1
    fi
    # Stale lock — previous sync crashed. Clean up and take it.
    rm -rf "$LOCK_FILE" 2>/dev/null
    if mkdir "$LOCK_FILE" 2>/dev/null; then
      echo $$ > "$LOCK_FILE/pid"
      log "RECOVERED: stale lock from PID $OLD_PID"
      return 0
    fi
  fi

  log "SKIP: could not acquire lock"
  return 1
}

release_lock() {
  rm -rf "$LOCK_FILE" 2>/dev/null
}

# --- Cleanup (runs on ANY exit) ---
cleanup() {
  # Remove worktree if it exists
  if [ -d "$WORKTREE_DIR" ]; then
    git -C "$REPO_ROOT" worktree remove "$WORKTREE_DIR" --force 2>/dev/null
    # Fallback: manual removal if git worktree remove fails
    rm -rf "$WORKTREE_DIR" 2>/dev/null
    # Prune worktree bookkeeping
    git -C "$REPO_ROOT" worktree prune 2>/dev/null
  fi
  release_lock
}
trap cleanup EXIT INT TERM

# --- Pre-checks ---
CURRENT_BRANCH=$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null)

# Skip if on integration branch (avoid infinite loop)
if [ "$CURRENT_BRANCH" = "$INTEGRATION_BRANCH" ]; then
  exit 0
fi

# Skip if not a claude/* session branch
case "$CURRENT_BRANCH" in
  claude/*) ;;
  *) exit 0 ;;
esac

# Acquire lock or skip
if ! acquire_lock; then
  exit 0
fi

log "START: syncing $CURRENT_BRANCH → $INTEGRATION_BRANCH"

# --- Ensure integration branch exists ---
if ! git -C "$REPO_ROOT" fetch origin "$INTEGRATION_BRANCH" 2>/dev/null; then
  # Create integration branch from main
  git -C "$REPO_ROOT" fetch origin main 2>/dev/null
  git -C "$REPO_ROOT" branch "$INTEGRATION_BRANCH" origin/main 2>/dev/null
  git -C "$REPO_ROOT" push -u origin "$INTEGRATION_BRANCH" 2>/dev/null
  log "CREATED: $INTEGRATION_BRANCH from main"
fi

# --- Clean up any leftover worktree ---
if [ -d "$WORKTREE_DIR" ]; then
  git -C "$REPO_ROOT" worktree remove "$WORKTREE_DIR" --force 2>/dev/null
  rm -rf "$WORKTREE_DIR" 2>/dev/null
  git -C "$REPO_ROOT" worktree prune 2>/dev/null
fi

# --- Create worktree for integration branch ---
if ! git -C "$REPO_ROOT" worktree add "$WORKTREE_DIR" "origin/$INTEGRATION_BRANCH" --detach 2>/dev/null; then
  log "FAIL: could not create worktree"
  exit 0
fi

# Inside the worktree: checkout integration branch
cd "$WORKTREE_DIR" || { log "FAIL: could not cd to worktree"; exit 0; }

# Create local integration branch tracking remote
git checkout -B "$INTEGRATION_BRANCH" "origin/$INTEGRATION_BRANCH" 2>/dev/null

# --- Merge session branch (session wins all conflicts) ---
MERGE_OK=false

if git merge "origin/$CURRENT_BRANCH" -X theirs --no-edit \
    -m "chore(integration): merge $CURRENT_BRANCH" 2>/dev/null; then
  MERGE_OK=true
  log "MERGED: clean merge of $CURRENT_BRANCH"
else
  log "CONFLICT: attempting auto-resolve for $CURRENT_BRANCH"

  # Strategy 1: checkout --theirs on each conflicted file
  CONFLICTED=$(git diff --name-only --diff-filter=U 2>/dev/null)
  if [ -n "$CONFLICTED" ]; then
    RESOLVED_ALL=true
    echo "$CONFLICTED" | while IFS= read -r file; do
      if [ -f "$file" ]; then
        # File exists in both — take session version
        git checkout --theirs "$file" 2>/dev/null && git add "$file" 2>/dev/null
        log "  RESOLVED: $file (took session version)"
      else
        # File deleted in one branch — accept deletion or addition
        git rm "$file" 2>/dev/null || git add "$file" 2>/dev/null
        log "  RESOLVED: $file (accepted delete/add)"
      fi
    done

    # Verify no conflicts remain
    REMAINING=$(git diff --name-only --diff-filter=U 2>/dev/null)
    if [ -z "$REMAINING" ]; then
      git commit --no-edit \
        -m "chore(integration): merge $CURRENT_BRANCH (conflicts auto-resolved)" 2>/dev/null
      MERGE_OK=true
      log "RESOLVED: all conflicts auto-resolved"
    else
      # Strategy 2: Nuclear option — accept ALL theirs
      git checkout --theirs . 2>/dev/null
      git add -A 2>/dev/null
      STILL_REMAINING=$(git diff --name-only --diff-filter=U 2>/dev/null)
      if [ -z "$STILL_REMAINING" ]; then
        git commit --no-edit \
          -m "chore(integration): merge $CURRENT_BRANCH (force-resolved)" 2>/dev/null
        MERGE_OK=true
        log "FORCE-RESOLVED: took all session changes"
      else
        git merge --abort 2>/dev/null
        log "ABORT: unresolvable conflicts remain: $STILL_REMAINING"
      fi
    fi
  else
    # Merge failed but no conflicted files — unknown error
    git merge --abort 2>/dev/null
    log "ABORT: merge failed for unknown reason"
  fi
fi

# --- Push integration branch ---
if [ "$MERGE_OK" = true ]; then
  PUSH_OK=false
  for attempt in 1 2 3 4; do
    if git push origin "$INTEGRATION_BRANCH" 2>/dev/null; then
      PUSH_OK=true
      log "PUSHED: $INTEGRATION_BRANCH (attempt $attempt)"
      break
    fi
    WAIT=$((2 ** attempt))
    log "RETRY: push failed, waiting ${WAIT}s (attempt $attempt)"
    sleep "$WAIT"
  done

  if [ "$PUSH_OK" = false ]; then
    log "FAIL: could not push $INTEGRATION_BRANCH after 4 attempts"
  fi
fi

log "END: sync complete for $CURRENT_BRANCH"
exit 0
