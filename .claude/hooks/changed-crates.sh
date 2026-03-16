#!/bin/bash
# =============================================================================
# changed-crates.sh — Detect which crates were modified since last push
# =============================================================================
# Returns space-separated list of crate names that have changes.
# Used by pre-push-gate to scope testing to affected crates only.
#
# Usage:
#   bash .claude/hooks/changed-crates.sh [project-dir]
#
# Output (stdout): space-separated crate names, e.g. "core storage"
# If detection fails or no crates dir: outputs "ALL" (fallback to full workspace)
# =============================================================================

set -uo pipefail

PROJECT_DIR="${1:-.}"
cd "$PROJECT_DIR" || { echo "ALL"; exit 0; }

# Determine the remote tracking branch to diff against
REMOTE_REF=""

# Try: upstream tracking branch
UPSTREAM=$(git rev-parse --abbrev-ref '@{upstream}' 2>/dev/null || echo "")
if [ -n "$UPSTREAM" ]; then
  REMOTE_REF="$UPSTREAM"
else
  # Try: origin/<current-branch>
  CURRENT_BRANCH=$(git branch --show-current 2>/dev/null || echo "")
  if [ -n "$CURRENT_BRANCH" ] && git rev-parse "origin/$CURRENT_BRANCH" >/dev/null 2>&1; then
    REMOTE_REF="origin/$CURRENT_BRANCH"
  else
    # Try: origin/main
    if git rev-parse "origin/main" >/dev/null 2>&1; then
      REMOTE_REF="origin/main"
    fi
  fi
fi

# If no remote ref found, fall back to full workspace
if [ -z "$REMOTE_REF" ]; then
  echo "ALL"
  exit 0
fi

# Get changed files between remote and HEAD
CHANGED_FILES=$(git diff --name-only "$REMOTE_REF"..HEAD 2>/dev/null)

# Also include staged but not yet committed changes
STAGED_FILES=$(git diff --name-only --cached 2>/dev/null)

# Also include unstaged changes (working tree)
UNSTAGED_FILES=$(git diff --name-only 2>/dev/null)

# Combine all
ALL_CHANGED=$(printf '%s\n%s\n%s' "$CHANGED_FILES" "$STAGED_FILES" "$UNSTAGED_FILES" | sort -u)

if [ -z "$ALL_CHANGED" ]; then
  # No changes detected — nothing to test
  echo "NONE"
  exit 0
fi

# Extract unique crate names from paths like "crates/<name>/..."
CRATES=$(echo "$ALL_CHANGED" | grep '^crates/' | sed 's|^crates/\([^/]*\)/.*|\1|' | sort -u | tr '\n' ' ' | sed 's/ $//')

# Check if non-crate files changed (root Cargo.toml, .claude/hooks, scripts, etc.)
NON_CRATE=$(echo "$ALL_CHANGED" | grep -v '^crates/' | head -1)

if [ -n "$NON_CRATE" ] && [ -z "$CRATES" ]; then
  # Only non-crate files changed (hooks, scripts, docs) — no Rust testing needed
  echo "NONE"
  exit 0
fi

if [ -n "$NON_CRATE" ] && [ -n "$CRATES" ]; then
  # Mixed: crate + non-crate files. If root Cargo.toml changed, test all.
  if echo "$ALL_CHANGED" | grep -q '^Cargo.toml$'; then
    echo "ALL"
    exit 0
  fi
  # Otherwise just test the changed crates
  echo "$CRATES"
  exit 0
fi

if [ -z "$CRATES" ]; then
  echo "NONE"
  exit 0
fi

echo "$CRATES"
exit 0
