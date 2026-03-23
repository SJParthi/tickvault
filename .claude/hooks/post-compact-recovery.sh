#!/usr/bin/env bash
# post-compact-recovery.sh — Re-inject critical context after session compaction
# Fires on PostCompact event. Prevents Claude from "forgetting" rules after
# context window compresses at 85%.
#
# Output goes to stdout → Claude sees it as context in the next turn.

set -uo pipefail

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"

# ─── Critical Context Re-injection ───────────────────────────────────────────

cat <<'CONTEXT'
POST-COMPACTION CONTEXT RECOVERY
================================

THREE PRINCIPLES (non-negotiable):
1. Zero allocation on hot path
2. O(1) or fail at compile time
3. Every version pinned

CURRENT PHASE: docs/phases/phase-1-live-trading.md

WORKFLOW: Present plan → wait for approval → execute → show proof.
NEVER execute without approval.

CODEBASE: 6 crates (common ← core ← trading ← storage ← api ← app)

SESSION PROTOCOL: Read CLAUDE.md → read phase doc → cargo check → cargo test

BANNED: .env | bincode/Promtail/Jaeger-v1 | ^/~/*/>=/:latest | brew |
        localhost | hardcoded values | .clone()/DashMap/dyn on hot |
        unbounded channels | println!/unwrap | cargo update
CONTEXT

# ─── Modified Files (if git available) ───────────────────────────────────────

if git -C "$CWD" rev-parse --git-dir >/dev/null 2>&1; then
  BRANCH=$(git -C "$CWD" branch --show-current 2>/dev/null || echo "detached")
  echo ""
  echo "BRANCH: $BRANCH"

  # Show uncommitted changes
  DIRTY=$(git -C "$CWD" status --porcelain 2>/dev/null | head -10)
  if [ -n "$DIRTY" ]; then
    echo ""
    echo "UNCOMMITTED CHANGES:"
    echo "$DIRTY"
  fi

  # Show recent commits (what we've been working on)
  RECENT=$(git -C "$CWD" log --oneline -5 2>/dev/null)
  if [ -n "$RECENT" ]; then
    echo ""
    echo "RECENT COMMITS:"
    echo "$RECENT"
  fi
fi

# ─── Active Plan (if exists) ─────────────────────────────────────────────────

PLAN_FILE="$CWD/.claude/plans/active-plan.md"
if [ -f "$PLAN_FILE" ]; then
  PLAN_STATUS=$(grep -m1 '^[*]*Status:' "$PLAN_FILE" 2>/dev/null | sed 's/.*: //' | tr -d '*' || echo "unknown")
  PLAN_TITLE=$(head -1 "$PLAN_FILE" 2>/dev/null | sed 's/^# //')
  UNCHECKED=$(grep -c '^\- \[ \]' "$PLAN_FILE" 2>/dev/null || echo "0")
  CHECKED=$(grep -c '^\- \[x\]' "$PLAN_FILE" 2>/dev/null || echo "0")
  echo ""
  echo "ACTIVE PLAN: $PLAN_TITLE"
  echo "STATUS: $PLAN_STATUS | Done: $CHECKED | Remaining: $UNCHECKED"
fi

echo ""
echo "Re-read CLAUDE.md and phase doc if you need full context."
