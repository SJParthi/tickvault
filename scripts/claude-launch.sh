#!/bin/bash
# =============================================================================
# claude-launch.sh — Full-featured Claude Code launcher for tickvault
# =============================================================================
# Launches Claude Code with ALL features enabled:
#   - Chrome integration (browser automation)
#   - High effort level
#   - Extended bash timeouts
#   - All project settings auto-loaded from .claude/settings.json
#
# Usage:
#   ./scripts/claude-launch.sh                    # Interactive session
#   ./scripts/claude-launch.sh "your prompt"      # Start with prompt
#   ./scripts/claude-launch.sh --resume           # Resume last session
#
# Features auto-enabled via .claude/settings.json (no flags needed):
#   - Effort: high
#   - Auto-compact: 85%
#   - Agent teams: enabled
#   - All 14 hooks (SessionStart, PreToolUse, PostToolUse, Stop, etc.)
#   - 5 custom agents (verify-build, hot-path-reviewer, etc.)
#   - GitHub MCP server
#
# Per-session toggles (use keyboard shortcuts during session):
#   - Alt+T  → Extended thinking (complex problems)
#   - Alt+O  → Fast mode (quick tasks)
#   - Alt+P  → Switch model
#   - /voice → Push-to-talk
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_DIR"

# Build launch args
ARGS=(
  "--chrome"             # Browser automation enabled
)

# Pass through any user arguments
if [ $# -gt 0 ]; then
  if [ "$1" = "--resume" ] || [ "$1" = "-c" ]; then
    ARGS+=("--continue")
  else
    ARGS+=("$@")
  fi
fi

echo "╔══════════════════════════════════════════════════════╗"
echo "║  tickvault — Claude Code (Full Features)     ║"
echo "╠══════════════════════════════════════════════════════╣"
echo "║  Chrome: ON | Effort: HIGH | Agent Teams: ON        ║"
echo "║  Hooks: 14 active | Agents: 5 custom                ║"
echo "║                                                      ║"
echo "║  Shortcuts: Alt+T=Thinking  Alt+O=Fast  Alt+P=Model ║"
echo "╚══════════════════════════════════════════════════════╝"
echo ""

exec claude "${ARGS[@]}"
