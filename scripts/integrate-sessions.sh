#!/usr/bin/env bash
# =============================================================================
# integrate-sessions.sh — Merge all claude/* session branches into one branch
# =============================================================================
# Runs locally. Zero CI cost. Zero GitHub Actions minutes.
#
# Usage:
#   ./scripts/integrate-sessions.sh              # merge all claude/* branches
#   ./scripts/integrate-sessions.sh --push       # merge and push to remote
#   ./scripts/integrate-sessions.sh --dry-run    # show what would be merged
#
# The integration branch (claude/integration) collects all session work.
# When ready, raise ONE PR from claude/integration → main.
#
# WARNING: Do NOT run this while Claude is actively pushing. The automated
# sync (post-push-sync.sh) handles that. Use this for manual bulk integration.
# =============================================================================

set -uo pipefail

INTEGRATION_BRANCH="claude/integration"
LOCK_FILE=".claude/hooks/.integration-sync.lock"
PUSH=false
DRY_RUN=false

for arg in "$@"; do
  case "$arg" in
    --push) PUSH=true ;;
    --dry-run) DRY_RUN=true ;;
    *) echo "Unknown arg: $arg"; exit 1 ;;
  esac
done

# --- Mutual exclusion with auto-sync (sync-to-integration.sh) ---
if [ -d "$LOCK_FILE" ]; then
  if [ -f "$LOCK_FILE/pid" ]; then
    LOCK_PID=$(cat "$LOCK_FILE/pid" 2>/dev/null || echo "")
    if [ -n "$LOCK_PID" ] && kill -0 "$LOCK_PID" 2>/dev/null; then
      echo "ERROR: Auto-sync is running (PID $LOCK_PID). Wait for it to finish." >&2
      exit 1
    fi
    echo "WARNING: Stale lock from PID ${LOCK_PID:-unknown}. Removing."
    rm -rf "$LOCK_FILE" 2>/dev/null
  fi
fi

# Acquire lock (same lock as sync-to-integration.sh)
if ! mkdir "$LOCK_FILE" 2>/dev/null; then
  echo "ERROR: Could not acquire integration lock. Another sync may be running." >&2
  exit 1
fi
echo $$ > "$LOCK_FILE/pid" 2>/dev/null

# Release lock on exit
release_lock() { rm -rf "$LOCK_FILE" 2>/dev/null; }

# Fetch latest from remote
echo "Fetching all remote branches..."
if ! git fetch origin --prune; then
  echo "ERROR: git fetch failed (network down?). Cannot proceed with stale refs." >&2
  release_lock
  exit 1
fi

# Find all claude/* session branches (exclude integration itself)
# Use exact match with $ anchor to avoid excluding claude/integration-v2
SESSION_BRANCHES=$(git branch -r \
  | grep 'origin/claude/' \
  | grep -v "origin/${INTEGRATION_BRANCH}$" \
  | sed 's|^ *origin/||' \
  | tr -s ' ' '\n' \
  | grep -v '^$') || true

if [ -z "$SESSION_BRANCHES" ]; then
  echo "No claude/* session branches found. Nothing to integrate."
  release_lock
  exit 0
fi

# Detect default branch for commit count display
_DEFAULT=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's|refs/remotes/origin/||')
if [ -z "$_DEFAULT" ]; then
  if git show-ref --verify --quiet refs/remotes/origin/main 2>/dev/null; then
    _DEFAULT="main"
  else
    _DEFAULT="master"
  fi
fi

echo ""
echo "Session branches found:"
while IFS= read -r branch; do
  [ -z "$branch" ] && continue
  COMMITS=$(git log --oneline "origin/${_DEFAULT}..origin/${branch}" 2>/dev/null | wc -l | tr -d ' ') || true
  printf '  - %s (%s commits ahead of %s)\n' "$branch" "${COMMITS:-0}" "$_DEFAULT"
done <<< "$SESSION_BRANCHES"

if [ "$DRY_RUN" = true ]; then
  echo ""
  echo "[dry-run] Would merge the above branches into ${INTEGRATION_BRANCH}"
  release_lock
  exit 0
fi

# Save current ref to return later (handles detached HEAD too)
ORIGINAL_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
ORIGINAL_REF=$(git rev-parse HEAD 2>/dev/null || echo "")

# Cleanup trap — always return to original branch/ref and release lock on exit
cleanup() {
  if [ -n "${ORIGINAL_BRANCH:-}" ] && [ "$ORIGINAL_BRANCH" != "HEAD" ]; then
    git checkout "$ORIGINAL_BRANCH" --quiet 2>/dev/null || true
  elif [ -n "${ORIGINAL_REF:-}" ]; then
    git checkout "$ORIGINAL_REF" --quiet 2>/dev/null || true
  fi
  release_lock
}
trap cleanup EXIT INT TERM HUP

# Create or checkout integration branch
echo ""
if git show-ref --verify --quiet "refs/remotes/origin/${INTEGRATION_BRANCH}"; then
  echo "Checking out existing ${INTEGRATION_BRANCH}..."
  git checkout "$INTEGRATION_BRANCH" 2>/dev/null || git checkout -b "$INTEGRATION_BRANCH" "origin/$INTEGRATION_BRANCH"
  git pull --ff-only origin "$INTEGRATION_BRANCH" 2>/dev/null || {
    echo "WARNING: integration branch has diverged from remote."
    echo "Deleting local branch and re-creating from remote (preserves reflog)."
    git checkout --detach 2>/dev/null
    git branch -D "$INTEGRATION_BRANCH" 2>/dev/null || true
    git checkout -b "$INTEGRATION_BRANCH" "origin/$INTEGRATION_BRANCH"
  }
else
  # Detect default branch (main or master)
  DEFAULT_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's|refs/remotes/origin/||')
  if [ -z "$DEFAULT_BRANCH" ]; then
    # Fallback: try main, then master
    if git show-ref --verify --quiet refs/remotes/origin/main; then
      DEFAULT_BRANCH="main"
    elif git show-ref --verify --quiet refs/remotes/origin/master; then
      DEFAULT_BRANCH="master"
    else
      echo "ERROR: Cannot find origin/main or origin/master." >&2
      exit 1
    fi
  fi
  echo "Creating ${INTEGRATION_BRANCH} from ${DEFAULT_BRANCH}..."
  if ! git checkout -b "$INTEGRATION_BRANCH" "origin/$DEFAULT_BRANCH"; then
    echo "ERROR: Could not create integration branch from origin/$DEFAULT_BRANCH" >&2
    exit 1
  fi
fi

# Validate we are actually on the integration branch before merging
ACTUAL_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
if [ "$ACTUAL_BRANCH" != "$INTEGRATION_BRANCH" ]; then
  echo "ERROR: Expected to be on '$INTEGRATION_BRANCH' but on '$ACTUAL_BRANCH'." >&2
  echo "Aborting to prevent merging into wrong branch." >&2
  exit 1
fi

# Merge each session branch
MERGED=0
FAILED=0
while IFS= read -r branch; do
  [ -z "$branch" ] && continue
  echo ""
  echo "--- Merging ${branch} ---"
  if git merge "origin/${branch}" --no-ff --no-edit -m "chore(integration): merge ${branch}"; then
    MERGED=$((MERGED + 1))
    echo "OK: ${branch} merged"
  else
    echo "CONFLICT: ${branch} has merge conflicts. Aborting this merge."
    git merge --abort 2>/dev/null || true
    FAILED=$((FAILED + 1))
  fi
done <<< "$SESSION_BRANCHES"

echo ""
echo "=== Summary ==="
echo "  Merged: ${MERGED}"
echo "  Failed: ${FAILED}"

# Push if requested
if [ "$PUSH" = true ] && [ "$MERGED" -gt 0 ]; then
  echo ""
  echo "Pushing ${INTEGRATION_BRANCH} to remote..."
  if ! git push -u origin "$INTEGRATION_BRANCH"; then
    echo "ERROR: Push failed." >&2
    exit 1
  fi
fi

# Return to original branch (EXIT trap handles this automatically)

echo ""
echo "Done. Integration branch: ${INTEGRATION_BRANCH}"
if [ "$PUSH" = false ] && [ "$MERGED" -gt 0 ]; then
  echo "Run with --push to push to remote."
fi

# Exit with failure if any branches couldn't be merged
if [ "$FAILED" -gt 0 ]; then
  exit 1
fi
