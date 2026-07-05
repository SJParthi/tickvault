#!/bin/bash
# session-context-brief.sh — Surfaces session continuity context at every
# SessionStart so the new session resumes correctly without manual
# context-loading. Runs in <2s, output to stderr.
#
# What it surfaces:
#   1. Active plan(s) with Status: APPROVED or IN_PROGRESS (NOT VERIFIED)
#   2. Open PR for current branch (via gh CLI, cached 1h)
#   3. Error count in last hour from data/logs/errors.jsonl.*
#   4. Active Prometheus alerts (if endpoint reachable, 2s timeout)
#   5. Pinned per-session protocol from .claude/rules/project/wave-4-shared-preamble.md
#
# Why: operator demand 2026-05-01 — "always in existing or new session
# we need to make it extremely effective and efficient to use all
# these dude". This hook makes session continuity automatic.

set -uo pipefail  # no -e: best-effort, never block session start

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$CWD" || exit 0

echo "" >&2
echo "SESSION CONTEXT BRIEF" >&2
echo "=====================" >&2

# 1. Active plans (Status: APPROVED or IN_PROGRESS, NOT VERIFIED)
PLANS_DIR="$CWD/.claude/plans"
if [ -d "$PLANS_DIR" ]; then
  ACTIVE_PLANS=()
  while IFS= read -r f; do
    if grep -lE '^\*\*Status:\*\*\s+(APPROVED|IN_PROGRESS|DRAFT)\b' "$f" >/dev/null 2>&1; then
      STATUS=$(grep -E '^\*\*Status:\*\*' "$f" | head -1 | sed -E 's/.*Status:\*\* +//' | awk '{print $1}')
      BASENAME=$(basename "$f")
      ACTIVE_PLANS+=("$STATUS  $BASENAME")
    fi
  done < <(find "$PLANS_DIR" -maxdepth 1 -name 'active-plan*.md' 2>/dev/null)

  if [ ${#ACTIVE_PLANS[@]} -gt 0 ]; then
    echo "Active plans (read these first):" >&2
    for p in "${ACTIVE_PLANS[@]}"; do
      echo "  $p" >&2
    done
  else
    echo "Active plans: none in flight" >&2
  fi
fi

# 2. Open PR for current branch (cached 1h to keep <2s budget)
BRANCH=$(git branch --show-current 2>/dev/null || echo "")
if [ -n "$BRANCH" ] && command -v gh >/dev/null 2>&1; then
  PR_CACHE="$CWD/.claude/hooks/.pr-cache.$BRANCH"
  PR_LINE=""
  if [ -f "$PR_CACHE" ]; then
    AGE=$(($(date +%s) - $(stat -c %Y "$PR_CACHE" 2>/dev/null || echo 0)))
    if [ "$AGE" -lt 3600 ]; then
      PR_LINE=$(cat "$PR_CACHE" 2>/dev/null)
    fi
  fi
  if [ -z "$PR_LINE" ]; then
    PR_LINE=$(timeout 3 gh pr list --head "$BRANCH" --state open --json number,title,isDraft,url \
      --jq '.[0] | "PR #\(.number) [\(if .isDraft then "DRAFT" else "READY" end)]: \(.title)\n  \(.url)"' 2>/dev/null || echo "")
    if [ -n "$PR_LINE" ]; then
      echo "$PR_LINE" > "$PR_CACHE"
    fi
  fi
  if [ -n "$PR_LINE" ]; then
    echo "Open PR for $BRANCH:" >&2
    echo "  $PR_LINE" >&2
  else
    echo "Open PR for $BRANCH: (none)" >&2
  fi
fi

# 3. Error count in last hour from errors.jsonl rotation files
LOGS_DIR="$CWD/data/logs/machine"
# 2026-07-05 grace window: pre-move rotations at the legacy top level.
if [ ! -d "$LOGS_DIR" ]; then
  LOGS_DIR="$CWD/data/logs"
fi
if [ -d "$LOGS_DIR" ]; then
  CURRENT_HOUR=$(date -u +%Y-%m-%d-%H)
  CURRENT_FILE="$LOGS_DIR/errors.jsonl.$CURRENT_HOUR"
  if [ -f "$CURRENT_FILE" ]; then
    ERR_COUNT=$(wc -l < "$CURRENT_FILE" 2>/dev/null | tr -d ' ')
    echo "Errors in current hour: $ERR_COUNT (file: errors.jsonl.$CURRENT_HOUR)" >&2
  else
    echo "Errors in current hour: 0 (no file yet)" >&2
  fi
fi

# 4. Active Prometheus alerts (best-effort, 2s timeout)
PROM_URL="${TICKVAULT_PROM_URL:-http://localhost:9090}"
if command -v curl >/dev/null 2>&1; then
  RAW=$(timeout 2 curl -sf "$PROM_URL/api/v1/alerts" 2>/dev/null || echo "")
  if [ -z "$RAW" ]; then
    echo "Active Prom alerts: (Prometheus unreachable at $PROM_URL)" >&2
  else
    ALERTS=$(echo "$RAW" | grep -oE '"state":"firing"' | wc -l | tr -d ' ')
    if [ "$ALERTS" -eq 0 ] 2>/dev/null; then
      echo "Active Prom alerts: 0 (clean)" >&2
    else
      echo "Active Prom alerts: $ALERTS firing — investigate" >&2
    fi
  fi
fi

# 5. Per-session protocol pointer
echo "" >&2
echo "Per-session protocol (mandatory):" >&2
echo "  1. mcp__tickvault-logs__run_doctor (if not already running)" >&2
echo "  2. mcp__tickvault-logs__summary_snapshot" >&2
echo "  3. Read active plan(s) listed above" >&2
echo "  4. Spawn 3 specialist agents in parallel for >3-crate decisions" >&2
echo "  5. File-first chat — bulk to .claude/plans/*.md, pointers in chat" >&2
echo "" >&2
exit 0
