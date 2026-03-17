#!/usr/bin/env bash
# =============================================================================
# sync-to-integration.sh — Sync current session branch to claude/integration
# =============================================================================
# Uses git worktree — NEVER touches the main working directory.
# Claude keeps working on the session branch with zero interference.
#
# Safety guarantees:
#   1. Worktree = isolated directory. No checkout in main repo.
#   2. Lock file = no concurrent syncs (mkdir atomic lock + stale PID recovery).
#   3. -X theirs = session branch always wins conflicts.
#   4. Cleanup trap = worktree removed on any exit (success, error, signal).
#   5. Called via nohup/disown + timeout = never blocks, never lingers.
#   6. All git commands have stderr/stdout redirected = never hangs on prompt.
#   7. Explicit fetch of session branch = never merges stale ref.
# =============================================================================

set -uo pipefail

# --- Resolve REPO_ROOT safely ---
REPO_ROOT="${CLAUDE_PROJECT_DIR:-}"
if [ -z "$REPO_ROOT" ]; then
  REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null) || true
fi
if [ -z "$REPO_ROOT" ] || [ ! -d "$REPO_ROOT/.git" ]; then
  echo "FATAL: cannot determine repo root" >&2
  exit 0
fi

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
  # Use mkdir as atomic lock — works on all platforms, race-free
  if mkdir "$LOCK_FILE" 2>/dev/null; then
    echo $$ > "$LOCK_FILE/pid" 2>/dev/null
    return 0
  fi

  # Lock exists — check if holder is still alive
  if [ -f "$LOCK_FILE/pid" ]; then
    OLD_PID=$(cat "$LOCK_FILE/pid" 2>/dev/null || echo "")
    if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
      log "SKIP: another sync is running (PID $OLD_PID)"
      return 1
    fi
    # Stale lock — previous sync crashed or was killed by timeout
    rm -rf "$LOCK_FILE" 2>/dev/null
    if mkdir "$LOCK_FILE" 2>/dev/null; then
      echo $$ > "$LOCK_FILE/pid" 2>/dev/null
      log "RECOVERED: stale lock from PID ${OLD_PID:-unknown}"
      return 0
    fi
  fi

  log "SKIP: could not acquire lock"
  return 1
}

release_lock() {
  rm -rf "$LOCK_FILE" 2>/dev/null
}

# --- Cleanup (runs on ANY exit: success, error, signal, timeout kill) ---
cleanup() {
  if [ -d "$WORKTREE_DIR" ]; then
    # Try git worktree remove first (updates bookkeeping)
    git -C "$REPO_ROOT" worktree remove "$WORKTREE_DIR" --force 2>/dev/null || true
    # Fallback: brute-force removal if git command fails
    rm -rf "$WORKTREE_DIR" 2>/dev/null || true
    # Clean up any stale worktree bookkeeping entries
    git -C "$REPO_ROOT" worktree prune 2>/dev/null || true
  fi
  release_lock
}
trap cleanup EXIT INT TERM HUP

# --- Pre-checks ---
CURRENT_BRANCH=$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")

# Skip if HEAD is detached or empty
if [ -z "$CURRENT_BRANCH" ] || [ "$CURRENT_BRANCH" = "HEAD" ]; then
  exit 0
fi

# Skip if on integration branch (avoid infinite loop)
if [ "$CURRENT_BRANCH" = "$INTEGRATION_BRANCH" ]; then
  exit 0
fi

# Skip if not a claude/* session branch
case "$CURRENT_BRANCH" in
  claude/*) ;;
  *) exit 0 ;;
esac

# Acquire lock or skip (another sync is already running)
if ! acquire_lock; then
  exit 0
fi

log "START: syncing $CURRENT_BRANCH → $INTEGRATION_BRANCH"

# --- Fetch both branches to ensure refs are current ---
# Fetch integration branch (or create it if first time)
if ! git -C "$REPO_ROOT" fetch origin "$INTEGRATION_BRANCH" 2>/dev/null; then
  # First time: create integration branch from main
  if git -C "$REPO_ROOT" fetch origin main 2>/dev/null; then
    git -C "$REPO_ROOT" branch "$INTEGRATION_BRANCH" origin/main 2>/dev/null || true
    if ! git -C "$REPO_ROOT" push -u origin "$INTEGRATION_BRANCH" 2>/dev/null; then
      log "FAIL: could not create remote $INTEGRATION_BRANCH"
      exit 0
    fi
    # Re-fetch so origin/claude/integration ref exists locally
    git -C "$REPO_ROOT" fetch origin "$INTEGRATION_BRANCH" 2>/dev/null
    log "CREATED: $INTEGRATION_BRANCH from main"
  else
    log "FAIL: could not fetch origin/main to create integration branch"
    exit 0
  fi
fi

# Fetch session branch to ensure origin/$CURRENT_BRANCH is up-to-date
# (push just happened, so remote has it, but local ref might be stale)
git -C "$REPO_ROOT" fetch origin "$CURRENT_BRANCH" 2>/dev/null || true

# --- Clean up any leftover worktree from a previous crashed run ---
if [ -d "$WORKTREE_DIR" ]; then
  git -C "$REPO_ROOT" worktree remove "$WORKTREE_DIR" --force 2>/dev/null || true
  rm -rf "$WORKTREE_DIR" 2>/dev/null || true
  git -C "$REPO_ROOT" worktree prune 2>/dev/null || true
fi

# --- Create isolated worktree for integration branch ---
if ! git -C "$REPO_ROOT" worktree add "$WORKTREE_DIR" "origin/$INTEGRATION_BRANCH" --detach 2>/dev/null; then
  log "FAIL: could not create worktree (origin/$INTEGRATION_BRANCH may not exist)"
  exit 0
fi

# Inside the worktree: set up integration branch
cd "$WORKTREE_DIR" || { log "FAIL: could not cd to worktree"; exit 0; }

# Create/reset local integration branch tracking remote
# -B = create or reset if exists, avoids "already checked out" errors
git checkout -B "$INTEGRATION_BRANCH" "origin/$INTEGRATION_BRANCH" 2>/dev/null || {
  log "FAIL: could not checkout $INTEGRATION_BRANCH in worktree"
  exit 0
}

# --- Merge session branch (session wins all conflicts) ---
MERGE_OK=false

if git merge "origin/$CURRENT_BRANCH" -X theirs --no-edit \
    -m "chore(integration): merge $CURRENT_BRANCH" 2>/dev/null; then
  MERGE_OK=true
  log "MERGED: clean merge of $CURRENT_BRANCH"
else
  log "CONFLICT: attempting auto-resolve for $CURRENT_BRANCH"

  # Strategy 1: checkout --theirs on each conflicted file individually
  CONFLICTED=$(git diff --name-only --diff-filter=U 2>/dev/null || echo "")
  if [ -n "$CONFLICTED" ]; then
    echo "$CONFLICTED" | while IFS= read -r file; do
      if [ -e "$file" ]; then
        git checkout --theirs -- "$file" 2>/dev/null && git add "$file" 2>/dev/null
        log "  RESOLVED: $file (took session version)"
      else
        # File deleted in one side — accept whichever state exists
        git rm -f "$file" 2>/dev/null || git add "$file" 2>/dev/null
        log "  RESOLVED: $file (accepted delete/add)"
      fi
    done

    # Check if all conflicts resolved
    REMAINING=$(git diff --name-only --diff-filter=U 2>/dev/null || echo "")
    if [ -z "$REMAINING" ]; then
      git commit --no-edit \
        -m "chore(integration): merge $CURRENT_BRANCH (conflicts auto-resolved)" 2>/dev/null
      MERGE_OK=true
      log "RESOLVED: all conflicts auto-resolved"
    else
      # Strategy 2: Nuclear — accept ALL theirs for everything
      git checkout --theirs -- . 2>/dev/null || true
      git add -A 2>/dev/null
      STILL=$(git diff --name-only --diff-filter=U 2>/dev/null || echo "")
      if [ -z "$STILL" ]; then
        git commit --no-edit \
          -m "chore(integration): merge $CURRENT_BRANCH (force-resolved)" 2>/dev/null
        MERGE_OK=true
        log "FORCE-RESOLVED: took all session changes"
      else
        git merge --abort 2>/dev/null || true
        log "ABORT: unresolvable conflicts: $STILL"
      fi
    fi
  else
    # Merge failed but no conflicted files — could be index.lock contention
    git merge --abort 2>/dev/null || true
    log "ABORT: merge failed (no conflicts detected — possible git lock contention)"
  fi
fi

# --- Push integration branch (with retry + exponential backoff) ---
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
else
  log "SKIP-PUSH: merge did not succeed"
fi

log "END: sync complete for $CURRENT_BRANCH"
exit 0
