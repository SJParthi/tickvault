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
#
# 2026-08-10 SECURITY FIX (audit finding, docs/audits/2026-08-10-rust-only-o1-audit.md
# section 4b). Before this, a command carrying BOTH a commit and a push routed to
# the push gate ONLY: the chain below is first-substring-match-wins, the push arm
# is tested before the commit arm, and each arm ends in `exit $?`. The compound
# block above does not catch the two-line form either, because its separator
# alternation lists && / || / semicolon and grep is line-oriented, so a NEWLINE
# between them matches nothing.
#
# Net effect of the old behaviour: one ordinary-looking two-line command skipped
# the ENTIRE pre-commit battery — fmt, banned patterns, data integrity, dedup,
# the SECRET SCANNER, version pinning, commit-message validation and the
# commit-time invariant test — while both operations executed for real. The push
# gate could not compensate: it diffs against HEAD, which at that moment does not
# yet contain the pending commit.
#
# The fix deliberately targets the SECURITY property (every applicable gate runs)
# rather than the cosmetic rule (one verb per call). Both gates now run, commit
# first, matching execution order.
#
# Accepted cost, stated rather than hidden: this also fires when a verb appears
# only inside heredoc TEXT (e.g. committing documentation that quotes these
# commands), so such a commit pays one extra gate run. That is wasted time, never
# a wrong answer, and it fails in the safe direction — a redundant check rather
# than a skipped one.
if echo "$COMMAND" | grep -q 'git commit' && echo "$COMMAND" | grep -q 'git push'; then
  echo "$INPUT" | "$HOOKS_DIR/pre-commit-gate.sh" || exit 2
  echo "$INPUT" | "$HOOKS_DIR/pre-push-gate.sh" || exit 2
  exit 0
fi

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
