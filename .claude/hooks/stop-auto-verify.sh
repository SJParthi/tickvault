#!/usr/bin/env bash
# stop-auto-verify.sh — Auto-verify changed Rust files when Claude stops
# Fires on Stop event. Runs cargo fmt --check and cargo clippy on changed files.
# Only runs if .rs files were modified (skip for config-only changes).
#
# This is advisory — exit 0 always. The pre-push gate handles hard enforcement.
# Purpose: catch issues early before /grill or push.

set -uo pipefail

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"

# Fast exit if not a git repo
if ! git -C "$CWD" rev-parse --git-dir >/dev/null 2>&1; then
  exit 0
fi

# Check if any .rs files have been modified (staged + unstaged + untracked)
RS_CHANGED=$(git -C "$CWD" diff --name-only 2>/dev/null | grep '\.rs$' | head -1)
RS_STAGED=$(git -C "$CWD" diff --cached --name-only 2>/dev/null | grep '\.rs$' | head -1)
RS_UNTRACKED=$(git -C "$CWD" ls-files --others --exclude-standard 2>/dev/null | grep '\.rs$' | head -1)

if [ -z "$RS_CHANGED" ] && [ -z "$RS_STAGED" ] && [ -z "$RS_UNTRACKED" ]; then
  # No Rust changes — skip verification
  exit 0
fi

# Run fmt check (fast, <2s)
FMT_OUTPUT=$(cd "$CWD" && cargo fmt --all -- --check 2>&1)
FMT_EXIT=$?

if [ $FMT_EXIT -ne 0 ]; then
  echo "" >&2
  echo "AUTO-VERIFY: cargo fmt --check FAILED" >&2
  echo "Run 'cargo fmt --all' to fix formatting." >&2
  echo "$FMT_OUTPUT" | head -5 >&2
  exit 0
fi

# Run clippy (heavier, but catches real bugs — timeout protects us)
CLIPPY_OUTPUT=$(cd "$CWD" && cargo clippy --workspace --all-targets -- -D warnings -W clippy::perf 2>&1)
CLIPPY_EXIT=$?

if [ $CLIPPY_EXIT -ne 0 ]; then
  echo "" >&2
  echo "AUTO-VERIFY: cargo clippy FAILED" >&2
  echo "$CLIPPY_OUTPUT" | grep '^error' | head -5 >&2
  exit 0
fi

# Both passed silently — no noise when everything is clean
exit 0
