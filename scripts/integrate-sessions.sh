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
# =============================================================================

set -euo pipefail

INTEGRATION_BRANCH="claude/integration"
PUSH=false
DRY_RUN=false

for arg in "$@"; do
  case "$arg" in
    --push) PUSH=true ;;
    --dry-run) DRY_RUN=true ;;
    *) echo "Unknown arg: $arg"; exit 1 ;;
  esac
done

# Fetch latest from remote
echo "Fetching all remote branches..."
git fetch origin

# Find all claude/* session branches (exclude integration itself)
SESSION_BRANCHES=$(git branch -r \
  | grep 'origin/claude/' \
  | grep -v "origin/${INTEGRATION_BRANCH}" \
  | sed 's|origin/||' \
  | xargs)

if [ -z "$SESSION_BRANCHES" ]; then
  echo "No claude/* session branches found. Nothing to integrate."
  exit 0
fi

echo ""
echo "Session branches found:"
for branch in $SESSION_BRANCHES; do
  COMMITS=$(git log --oneline "origin/main..origin/${branch}" 2>/dev/null | wc -l | tr -d ' ')
  echo "  - ${branch} (${COMMITS} commits ahead of main)"
done

if [ "$DRY_RUN" = true ]; then
  echo ""
  echo "[dry-run] Would merge the above branches into ${INTEGRATION_BRANCH}"
  exit 0
fi

# Save current branch to return later
ORIGINAL_BRANCH=$(git rev-parse --abbrev-ref HEAD)

# Create or checkout integration branch
echo ""
if git show-ref --verify --quiet "refs/remotes/origin/${INTEGRATION_BRANCH}"; then
  echo "Checking out existing ${INTEGRATION_BRANCH}..."
  git checkout "$INTEGRATION_BRANCH"
  git pull origin "$INTEGRATION_BRANCH"
else
  echo "Creating ${INTEGRATION_BRANCH} from main..."
  git checkout -b "$INTEGRATION_BRANCH" origin/main
fi

# Merge each session branch
MERGED=0
FAILED=0
for branch in $SESSION_BRANCHES; do
  echo ""
  echo "--- Merging ${branch} ---"
  if git merge "origin/${branch}" --no-edit -m "chore(integration): merge ${branch}"; then
    MERGED=$((MERGED + 1))
    echo "OK: ${branch} merged"
  else
    echo "CONFLICT: ${branch} has merge conflicts. Aborting this merge."
    git merge --abort
    FAILED=$((FAILED + 1))
  fi
done

echo ""
echo "=== Summary ==="
echo "  Merged: ${MERGED}"
echo "  Failed: ${FAILED}"

# Push if requested
if [ "$PUSH" = true ] && [ "$MERGED" -gt 0 ]; then
  echo ""
  echo "Pushing ${INTEGRATION_BRANCH} to remote..."
  git push -u origin "$INTEGRATION_BRANCH"
fi

# Return to original branch
git checkout "$ORIGINAL_BRANCH"

echo ""
echo "Done. Integration branch: ${INTEGRATION_BRANCH}"
if [ "$PUSH" = false ] && [ "$MERGED" -gt 0 ]; then
  echo "Run with --push to push to remote."
fi
