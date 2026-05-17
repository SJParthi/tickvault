#!/usr/bin/env bash
# block-bypass.sh — PreToolUse Bash hook.
#
# Blocks bypass tactics that defeat the project's gate hierarchy:
#   - `git commit --no-verify`     — skips pre-commit-gate.sh (9 gates)
#   - `git push --no-verify`       — skips pre-push-gate.sh (8 gates)
#   - `git stash` (general)        — sidesteps hook checks on dirty tree
#   - `git push --force` to main/master — destructive, never allowed
#   - `--dangerously-skip-permissions` — Claude CLI escape hatch
#   - `--no-gpg-sign`              — bypasses signing if enabled
#   - `git rebase -i`              — interactive (not supported in CLI hooks)
#   - `git add -i` / `git add -p`  — interactive (not supported)
#
# The settings.json `permissions.deny` list catches the most common
# offenders, but it operates on prefix matching only. This hook scans
# the full command string for patterns the deny list cannot express.
#
# LIMITATIONS (be honest with future operators):
#   - String-matching only. CANNOT detect variable substitution like
#     `X=--no-verify; git commit $X`, subshells `bash -c 'git commit
#     --no-verify'`, or shell aliases. These bypasses succeed silently.
#     Authoritative protection lives in upstream gate scripts.
#   - Best-effort layer for the easy paths Claude Code itself would emit.
#
# Exit codes:
#   0 — pass (no offense found)
#   2 — block (offense found; stderr explains)
#
# Spec: .claude/rules/project/operator-charter-forever.md §H/§I
# Rule: enforcement.md "Never skip hooks via --no-verify"
#
# Routing: invoked by pre-tool-dispatch.sh AFTER the compound-command
# guard and BEFORE the per-command gate routing. Receives the raw
# command string as $1 (already extracted from JSON by dispatcher).

set -uo pipefail

INPUT="${1:-}"
if [ -z "$INPUT" ] && [ ! -t 0 ]; then
    INPUT="$(cat)"
fi

# Extract the command string. Tolerate both raw command and JSON
# stdin formats so this hook is callable from multiple integration points.
COMMAND=""
if echo "$INPUT" | grep -q '^{'; then
    COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null || echo "")
else
    COMMAND="$INPUT"
fi

if [ -z "$COMMAND" ]; then
    exit 0
fi

# --no-verify on git commit / push
if echo "$COMMAND" | grep -qE 'git\s+(commit|push|merge|rebase|cherry-pick).*--no-verify'; then
    echo "BLOCKED: --no-verify is forbidden." >&2
    echo "The pre-commit-gate.sh and pre-push-gate.sh enforce 9 + 8 checks" >&2
    echo "respectively that exist to protect SEBI-regulated production code." >&2
    echo "If a gate fails, FIX THE UNDERLYING ISSUE — do not skip the gate." >&2
    echo "See: .claude/rules/project/enforcement.md" >&2
    exit 2
fi

# --dangerously-skip-permissions on the claude CLI
if echo "$COMMAND" | grep -q -- '--dangerously-skip-permissions'; then
    echo "BLOCKED: --dangerously-skip-permissions is forbidden." >&2
    echo "Permission prompts exist to protect against accidental destructive ops." >&2
    echo "If you need a permission moved to the allowlist, edit settings.json." >&2
    exit 2
fi

# --no-gpg-sign on git commit / rebase
if echo "$COMMAND" | grep -qE 'git\s+(commit|rebase|cherry-pick|merge).*--no-gpg-sign'; then
    echo "BLOCKED: --no-gpg-sign is forbidden by operator-charter §A bullet 'pre-commit fast gate'." >&2
    echo "If signing is broken, fix the underlying GPG config — do not skip." >&2
    exit 2
fi

# General `git stash` push/pop/apply/save (allows `git stash list/show/branch`
# which are read-only). Already partially blocked at deny-list level, but
# only for `drop` and `clear` (see settings.json permissions.deny).
if echo "$COMMAND" | grep -qE 'git\s+stash(\s+(push|pop|apply|save))?(\s|$)' \
   && ! echo "$COMMAND" | grep -qE 'git\s+stash\s+(list|show|branch)(\s|$)'; then
    echo "BLOCKED: git stash push/pop/apply/save is forbidden." >&2
    echo "Stashing sidesteps the pre-commit gate on the staged tree." >&2
    echo "If you need to switch branches with WIP, commit to a WIP branch instead." >&2
    echo "See: operator-charter-forever.md 'Mechanical enforcement chain'." >&2
    exit 2
fi

# `git push --force` to main/master — destructive, never authorized
if echo "$COMMAND" | grep -qE 'git\s+push.*(--force|-f)\b' \
   && echo "$COMMAND" | grep -qE '\b(main|master)\b'; then
    echo "BLOCKED: force-push to main/master is forbidden — operator never authorized this." >&2
    echo "If you genuinely need a force-push to a feature branch, use --force-with-lease." >&2
    exit 2
fi

# Interactive git flags that hang in non-TTY CLI hook context.
# Match `-i` ONLY when it's a flag (whitespace before, word-boundary
# after) so we don't false-positive on `-m "fix -i bug"` etc.
# Also accept long-form `--interactive`.
if echo "$COMMAND" | grep -qE 'git\s+(rebase|add)(\s+\S+)*\s+(-i\b|--interactive\b)' \
   && ! echo "$COMMAND" | grep -qE '\-\-no-edit'; then
    echo "BLOCKED: interactive git flag (-i / --interactive) is unsupported in this environment." >&2
    echo "Use non-interactive: 'git rebase' without -i, 'git add <files>' instead of '-i'." >&2
    exit 2
fi

# `gh pr merge --admin` — bypasses branch protection
if echo "$COMMAND" | grep -qE 'gh\s+pr\s+merge.*--admin'; then
    echo "BLOCKED: 'gh pr merge --admin' bypasses branch protection." >&2
    echo "The PR-completion 10-step loop requires CI gating; --admin skips it." >&2
    echo "See: .claude/rules/project/pr-completion-protocol.md" >&2
    exit 2
fi

exit 0
