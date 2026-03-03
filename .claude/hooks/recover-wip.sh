#!/bin/bash
# recover-wip.sh — Recover work from remote auto-save snapshots
#
# Usage:
#   recover-wip.sh                     List available snapshots (local + remote)
#   recover-wip.sh --diff <ref>        Show diff between HEAD and auto-save
#   recover-wip.sh --apply             Apply latest snapshot as uncommitted changes
#   recover-wip.sh --restore <ref>     Create recovery branch from specific snapshot
#   recover-wip.sh --clean             Delete all auto-save refs (local + remote)
#
# After a crash/hang/wipe, run this from any session to recover work.

set -uo pipefail

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$CWD" || { echo "ERROR: Cannot cd to $CWD" >&2; exit 1; }

ACTION="${1:-list}"

case "$ACTION" in
  list|--list)
    echo "Fetching remote auto-save refs..." >&2
    git -C "$CWD" fetch origin 'refs/auto-save/*:refs/auto-save/*' 2>/dev/null || true
    echo "" >&2

    REFS=$(git -C "$CWD" for-each-ref --sort=-committerdate \
      --format='%(refname)  %(committerdate:short) %(committerdate:relative)  %(subject)' \
      refs/auto-save/ 2>/dev/null)

    if [ -z "$REFS" ]; then
      echo "No auto-save snapshots found (local or remote)." >&2
      exit 0
    fi

    echo "Available auto-save snapshots:" >&2
    echo "==============================" >&2
    echo "$REFS" >&2
    echo "" >&2
    echo "Commands:" >&2
    echo "  recover-wip.sh --diff <ref>      Show what changed" >&2
    echo "  recover-wip.sh --apply           Apply latest snapshot" >&2
    echo "  recover-wip.sh --restore <ref>   Create recovery branch" >&2
    echo "  recover-wip.sh --clean           Delete all snapshots" >&2
    ;;

  --diff)
    REF="${2:?Usage: recover-wip.sh --diff <ref-name>}"
    git -C "$CWD" fetch origin 'refs/auto-save/*:refs/auto-save/*' 2>/dev/null || true
    echo "Diff HEAD vs ${REF}:" >&2
    echo "" >&2
    git -C "$CWD" diff HEAD "$REF" --stat
    echo "" >&2
    echo "For full diff: git diff HEAD $REF" >&2
    ;;

  --apply)
    # Apply latest auto-save snapshot as uncommitted changes
    git -C "$CWD" fetch origin 'refs/auto-save/*:refs/auto-save/*' 2>/dev/null || true

    # Find the latest /latest ref
    LATEST_REF=$(git -C "$CWD" for-each-ref --sort=-committerdate \
      --format='%(refname)' refs/auto-save/ 2>/dev/null | grep '/latest$' | head -1)

    if [ -z "$LATEST_REF" ]; then
      # Fall back to any ref
      LATEST_REF=$(git -C "$CWD" for-each-ref --sort=-committerdate \
        --format='%(refname)' refs/auto-save/ 2>/dev/null | head -1)
    fi

    if [ -z "$LATEST_REF" ]; then
      echo "ERROR: No auto-save snapshots found." >&2
      exit 1
    fi

    echo "Applying snapshot: ${LATEST_REF}" >&2
    echo "Changes:" >&2
    git -C "$CWD" diff HEAD "$LATEST_REF" --stat >&2
    echo "" >&2

    # Apply the diff as uncommitted changes (preserves current branch)
    if git -C "$CWD" diff HEAD "$LATEST_REF" | git -C "$CWD" apply --allow-empty 2>/dev/null; then
      echo "SUCCESS: Auto-save snapshot applied as uncommitted changes." >&2
      echo "Review with: git status / git diff" >&2
    else
      echo "WARN: Patch apply had conflicts. Trying checkout approach..." >&2
      # Fallback: checkout tree from the ref without changing branch
      git -C "$CWD" read-tree -u --reset "$LATEST_REF" 2>/dev/null || {
        echo "ERROR: Could not apply snapshot. Use --restore instead:" >&2
        echo "  recover-wip.sh --restore ${LATEST_REF}" >&2
        exit 1
      }
      echo "SUCCESS: Working tree restored from snapshot." >&2
    fi
    ;;

  --restore)
    REF="${2:?Usage: recover-wip.sh --restore <ref-name>}"
    git -C "$CWD" fetch origin 'refs/auto-save/*:refs/auto-save/*' 2>/dev/null || true

    RECOVERY_BRANCH="recovery/$(date +%Y%m%d-%H%M%S)"
    git -C "$CWD" checkout -b "$RECOVERY_BRANCH" "$REF" 2>/dev/null || {
      echo "ERROR: Could not create branch from $REF" >&2
      exit 1
    }
    echo "SUCCESS: Created recovery branch: ${RECOVERY_BRANCH}" >&2
    echo "You can now cherry-pick or merge changes as needed." >&2
    echo "" >&2
    echo "Useful commands:" >&2
    echo "  git log --oneline -5     # see recovery commits" >&2
    echo "  git diff main            # compare with main" >&2
    echo "  git checkout <original>  # switch back" >&2
    echo "  git merge ${RECOVERY_BRANCH}  # merge recovery in" >&2
    ;;

  --clean)
    echo "Cleaning all auto-save refs (local + remote)..." >&2
    CLEANED=0

    while IFS= read -r ref; do
      if [ -n "$ref" ]; then
        git -C "$CWD" push origin --delete "$ref" 2>/dev/null && echo "  Deleted remote: $ref" >&2 || true
        git -C "$CWD" update-ref -d "$ref" 2>/dev/null && echo "  Deleted local:  $ref" >&2 || true
        CLEANED=$((CLEANED + 1))
      fi
    done < <(git -C "$CWD" for-each-ref --format='%(refname)' refs/auto-save/ 2>/dev/null)

    if [ "$CLEANED" -eq 0 ]; then
      echo "No auto-save refs to clean." >&2
    else
      echo "Cleanup complete: ${CLEANED} refs removed." >&2
    fi
    ;;

  *)
    echo "Usage: recover-wip.sh [--list|--diff <ref>|--apply|--restore <ref>|--clean]" >&2
    exit 1
    ;;
esac
