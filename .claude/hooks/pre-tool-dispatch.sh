#!/bin/bash
# pre-tool-dispatch.sh — Single dispatcher for all PreToolUse Bash hooks
# Reads stdin ONCE and routes to the correct gate script.
# This replaces 4 separate hooks (pre-commit-gate, pre-push-gate, pre-pr-gate, pre-merge-gate)
# that each did INPUT=$(cat), which could hang if stdin wasn't piped correctly to all four.

set -uo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

if [ -z "$COMMAND" ]; then
  exit 0
fi

HOOKS_DIR="$(dirname "$0")"

# SAFETY: Block compound commands that chain gated operations.
# e.g., "git commit -m msg && git push" would only gate the first operation.
# Each gated command must be a separate tool call.
if echo "$COMMAND" | grep -qE '(&&|\|\||;)\s*(git (commit|push)|gh pr (create|merge))'; then
  echo "BLOCKED: Compound command detected with gated operations." >&2
  echo "Each git commit, git push, gh pr create, gh pr merge must be a separate command." >&2
  exit 2
fi

# SAFETY: Block bypass tactics (--no-verify, git stash, force-push to main,
# --dangerously-skip-permissions, --no-gpg-sign, gh pr merge --admin).
# Patterns the settings.json deny-list cannot express via prefix matching.
# See .claude/hooks/block-bypass.sh for full pattern list + rationale.
#
# NOTE: This is a best-effort string-matching guard. It can be defeated by
# shell variable substitution (`X=--no-verify; git commit $X`), subshells
# (`bash -c 'git commit --no-verify'`), or aliases. Authoritative protection
# lives in the upstream gate scripts; this layer catches the easy paths
# Claude Code itself would otherwise emit.
if ! "$HOOKS_DIR/block-bypass.sh" "$COMMAND"; then
  exit 2
fi

# Route to the correct gate based on command content.
# Only ONE gate runs per command — no wasted work.
if echo "$COMMAND" | grep -qE '^\s*gh\s+pr\s+merge'; then
  echo "$INPUT" | "$HOOKS_DIR/pre-merge-gate.sh"
  exit $?
elif echo "$COMMAND" | grep -qE '^\s*gh\s+pr\s+create'; then
  echo "$INPUT" | "$HOOKS_DIR/pre-pr-gate.sh"
  exit $?
elif echo "$COMMAND" | grep -q 'git push'; then
  echo "$INPUT" | "$HOOKS_DIR/pre-push-gate.sh"
  exit $?
elif echo "$COMMAND" | grep -q 'git commit'; then
  echo "$INPUT" | "$HOOKS_DIR/pre-commit-gate.sh"
  exit $?
fi

# No gate matched — allow the command
exit 0
