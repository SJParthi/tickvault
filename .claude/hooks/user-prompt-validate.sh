#!/bin/bash
# user-prompt-validate.sh — UserPromptSubmit hook
# Fires before every user prompt is processed.
# Validates prompt context and logs for audit trail.
#
# Exit 0 = allow (always — we never block user input, just warn)
# Output on stdout = feedback injected into Claude's context

INPUT=$(cat)
PROMPT=$(echo "$INPUT" | jq -r '.prompt // empty' 2>/dev/null)
CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"

# Skip empty prompts
if [ -z "$PROMPT" ]; then
  exit 0
fi

# Audit log: append every prompt timestamp to session log
LOG_DIR="$CWD/data/logs"
mkdir -p "$LOG_DIR" 2>/dev/null
TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
echo "$TIMESTAMP | prompt_length=$(echo -n "$PROMPT" | wc -c | tr -d ' ')" >> "$LOG_DIR/session-audit.log" 2>/dev/null

# Warning: detect if user mentions destructive operations
DESTRUCTIVE_PATTERNS="drop table|delete all|rm -rf|force push|reset --hard|cargo update|nuke|wipe|destroy"
if echo "$PROMPT" | grep -qiE "$DESTRUCTIVE_PATTERNS"; then
  echo "WARNING: Prompt contains potentially destructive intent. Proceeding with extra caution."
fi

# Warning: detect if user asks to skip tests or hooks
SKIP_PATTERNS="skip test|skip hook|no-verify|skip ci|skip check|bypass"
if echo "$PROMPT" | grep -qiE "$SKIP_PATTERNS"; then
  echo "WARNING: Prompt requests skipping safety gates. All gates remain enforced per CLAUDE.md."
fi

exit 0
