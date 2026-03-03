#!/bin/bash
# =============================================================================
# dhan-live-trader — Claude Code Status Line
# =============================================================================
# Shows: model | branch | context usage bar | cost | duration | dirty state
#
# Setup (user-level, not committed):
#   cp .claude/statusline.sh ~/.claude/statusline.sh
#   chmod +x ~/.claude/statusline.sh
#
# Then add to ~/.claude/settings.json:
#   "statusLine": { "type": "command", "command": "~/.claude/statusline.sh" }
# =============================================================================

input=$(cat)

MODEL=$(echo "$input" | jq -r '.model.display_name // "unknown"')
DIR=$(echo "$input" | jq -r '.workspace.current_dir // "."')
COST=$(echo "$input" | jq -r '.cost.total_cost_usd // 0')
PCT=$(echo "$input" | jq -r '.context_window.used_percentage // 0' | cut -d. -f1)
DURATION_MS=$(echo "$input" | jq -r '.cost.total_duration_ms // 0')
LINES_ADDED=$(echo "$input" | jq -r '.cost.total_lines_added // 0')
LINES_REMOVED=$(echo "$input" | jq -r '.cost.total_lines_removed // 0')

# Colors
CYAN='\033[36m'
GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
DIM='\033[2m'
RESET='\033[0m'

# Context bar — color coded by usage
if [ "$PCT" -ge 85 ]; then
  BAR_COLOR="$RED"
elif [ "$PCT" -ge 60 ]; then
  BAR_COLOR="$YELLOW"
else
  BAR_COLOR="$GREEN"
fi

FILLED=$((PCT / 10))
EMPTY=$((10 - FILLED))
BAR=$(printf "%${FILLED}s" | tr ' ' '#')$(printf "%${EMPTY}s" | tr ' ' '-')

# Duration
MINS=$((DURATION_MS / 60000))
SECS=$(((DURATION_MS % 60000) / 1000))

# Git info
BRANCH=""
DIRTY=""
if git rev-parse --git-dir > /dev/null 2>&1; then
  BRANCH=$(git branch --show-current 2>/dev/null || echo "detached")
  if ! git diff --quiet 2>/dev/null || ! git diff --cached --quiet 2>/dev/null; then
    DIRTY=" *"
  fi
fi

# Format cost
COST_FMT=$(printf '$%.2f' "$COST")

# Line 1: model | branch | dirty state
echo -e "${CYAN}${MODEL}${RESET} ${DIM}|${RESET} ${GREEN}${BRANCH}${DIRTY}${RESET} ${DIM}|${RESET} ${DIR##*/}"

# Line 2: context bar | cost | duration | lines changed
echo -e "${BAR_COLOR}[${BAR}]${RESET} ${PCT}% ${DIM}|${RESET} ${YELLOW}${COST_FMT}${RESET} ${DIM}|${RESET} ${MINS}m${SECS}s ${DIM}|${RESET} +${LINES_ADDED}/-${LINES_REMOVED}"
