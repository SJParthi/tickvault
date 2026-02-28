#!/bin/bash
# pre-commit-gate.sh — Blocks git commit if quality gates fail
# Called by PreToolUse hook on Bash commands matching "git commit"

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Only gate git commit commands
if ! echo "$COMMAND" | grep -q 'git commit'; then
  exit 0
fi

CWD=$(echo "$INPUT" | jq -r '.cwd // empty')
if [ -z "$CWD" ]; then
  CWD="."
fi

cd "$CWD" || exit 0

# Check if any .rs files are staged
RS_STAGED=$(git diff --cached --name-only --diff-filter=ACMR 2>/dev/null | grep '\.rs$' || true)

# If no Rust files staged, allow commit
if [ -z "$RS_STAGED" ]; then
  exit 0
fi

# Gate 1: cargo fmt check (don't modify, just check)
if ! cargo fmt --all -- --check > /dev/null 2>&1; then
  echo "BLOCKED: cargo fmt check failed. Run 'cargo fmt --all' first." >&2
  exit 2
fi

# Gate 2: cargo clippy
if ! cargo clippy --workspace --all-targets -- -D warnings > /dev/null 2>&1; then
  echo "BLOCKED: cargo clippy has warnings. Fix them before committing." >&2
  exit 2
fi

# Gate 3: cargo test
if ! cargo test --workspace > /dev/null 2>&1; then
  echo "BLOCKED: cargo test failed. Fix failing tests before committing." >&2
  exit 2
fi

exit 0
