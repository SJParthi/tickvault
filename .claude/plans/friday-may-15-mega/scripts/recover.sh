#!/usr/bin/env bash
# Layer 3 recovery script — tail the latest Claude Code native transcript JSONL.
# Use ONLY when Layer 1 (INDEX.md + .last-cursor.md) and Layer 2 (00-decisions-log.md)
# are both missing/corrupted.
#
# Usage: bash scripts/recover.sh [LINES_TO_TAIL]   (default 100)
#
# Claude Code stores per-session transcripts at:
#   ~/.claude/projects/<encoded-project-path>/<session-id>.jsonl
# where <encoded-project-path> = absolute path with '/' replaced by '-'.
# For tickvault: /home/user/tickvault → -home-user-tickvault

set -euo pipefail

LINES="${1:-100}"
PROJ_DIR_PRIMARY="${HOME}/.claude/projects/-home-user-tickvault"
PROJ_DIR_FALLBACK_GLOB="${HOME}/.claude/projects/*tickvault*"

# Resolve the actual transcript directory
PROJ_DIR=""
if [ -d "$PROJ_DIR_PRIMARY" ]; then
  PROJ_DIR="$PROJ_DIR_PRIMARY"
else
  # Try fuzzy match for non-standard paths
  for d in $PROJ_DIR_FALLBACK_GLOB; do
    [ -d "$d" ] && { PROJ_DIR="$d"; break; }
  done
fi

if [ -z "$PROJ_DIR" ] || [ ! -d "$PROJ_DIR" ]; then
  echo "ERROR: No Claude Code transcript directory found."
  echo "  Looked at: $PROJ_DIR_PRIMARY"
  echo "  Also tried glob: $PROJ_DIR_FALLBACK_GLOB"
  echo ""
  echo "If you are a NEW Claude session opening this cold:"
  echo "  1. ask the operator to paste the most recent decision they remember"
  echo "  2. start from there"
  exit 1
fi

LATEST=$(ls -t "$PROJ_DIR"/*.jsonl 2>/dev/null | head -1)
if [ -z "$LATEST" ]; then
  echo "ERROR: $PROJ_DIR exists but no *.jsonl files inside."
  exit 1
fi

echo "=========================================="
echo "Layer 3 recovery from: $LATEST"
echo "Tailing last $LINES lines."
echo "=========================================="
echo ""

# Try jq for structured extraction; fall back to raw tail if jq absent.
if command -v jq >/dev/null 2>&1; then
  tail -n "$LINES" "$LATEST" | jq -r '
    select(.type == "user" or .type == "assistant" or .type == "summary") |
    "[\(.timestamp // "?")] \(.type | ascii_upcase):\n\(
      (.message.content // .summary // "(no content)") |
      if type == "array" then
        map(if type == "object" then (.text // .content // (. | tostring)) else (. | tostring) end) | join("\n")
      else
        (. | tostring)
      end |
      .[0:600]
    )\n---"
  ' 2>/dev/null || tail -n "$LINES" "$LATEST"
else
  echo "(jq not installed — raw JSONL tail follows)"
  tail -n "$LINES" "$LATEST"
fi

echo ""
echo "=========================================="
echo "End of recovery output."
echo "Next step: read INDEX.md + 00-decisions-log.md and reconcile."
echo "=========================================="
