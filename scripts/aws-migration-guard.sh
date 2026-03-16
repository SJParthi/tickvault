#!/bin/bash
# aws-migration-guard.sh — Mechanical enforcement for Mac cleanup on AWS deploy
#
# Scans for ALL Mac local infrastructure that must be removed before AWS deploy.
# No sentinel file needed — checks git diff to detect deploy/aws/ changes,
# or can be run with --full to always scan.
#
# Usage:
#   bash scripts/aws-migration-guard.sh [project-dir] [--full]
#
# Exit 0 = clean (no conflict or AWS not being modified)
# Exit 1 = BLOCKED (Mac artifacts found while working on AWS deploy)

set -uo pipefail

CWD="${1:-.}"
MODE="${2:-auto}"

FAILED=0

echo "=== AWS Migration Guard ==="

# ---- Determine if we need to check ----
if [ "$MODE" != "--full" ]; then
  # Auto mode: only trigger if deploy/aws/ files are in the current changeset
  cd "$CWD" || exit 0
  AWS_CHANGED=""
  # Check committed changes vs remote
  UPSTREAM=$(git rev-parse --abbrev-ref '@{upstream}' 2>/dev/null || echo "")
  if [ -n "$UPSTREAM" ]; then
    AWS_CHANGED=$(git diff --name-only "$UPSTREAM"..HEAD -- 'deploy/aws/' 2>/dev/null | head -1)
  fi
  # Check staged
  if [ -z "$AWS_CHANGED" ]; then
    AWS_CHANGED=$(git diff --name-only --cached -- 'deploy/aws/' 2>/dev/null | head -1)
  fi
  # Check unstaged
  if [ -z "$AWS_CHANGED" ]; then
    AWS_CHANGED=$(git diff --name-only -- 'deploy/aws/' 2>/dev/null | head -1)
  fi

  if [ -z "$AWS_CHANGED" ]; then
    echo "INFO: No deploy/aws/ changes detected. Mac infra expected during dev."
    echo "PASS: No migration conflict."
    exit 0
  fi
  echo "Detected deploy/aws/ changes — scanning for Mac artifacts..."
  cd - > /dev/null || true
else
  echo "Full scan mode — checking ALL Mac artifacts..."
fi

# ---- Check 1: deploy/launchd must not exist ----
if [ -d "$CWD/deploy/launchd" ]; then
  echo "FAIL: deploy/launchd/ must be deleted before AWS deployment."
  FAILED=1
else
  echo "PASS: deploy/launchd/ does not exist"
fi

# ---- Check 2: Mac-specific scripts ----
MAC_FILES=""

if [ -f "$CWD/scripts/smoke-test.sh" ]; then
  MAC_FILES="$MAC_FILES scripts/smoke-test.sh"
fi

if [ -d "$CWD/deploy/launchd" ]; then
  for f in "$CWD"/deploy/launchd/*; do
    if [ -f "$f" ]; then
      MAC_FILES="$MAC_FILES deploy/launchd/$(basename "$f")"
    fi
  done
fi

if [ -f "$CWD/crates/app/src/infra.rs" ]; then
  if grep -q 'target_os.*macos\|df -k \.' "$CWD/crates/app/src/infra.rs" 2>/dev/null; then
    MAC_FILES="$MAC_FILES crates/app/src/infra.rs(mac-specific-code)"
  fi
fi

if [ -n "$MAC_FILES" ]; then
  echo "FAIL: Mac-specific files still present:"
  for f in $MAC_FILES; do
    echo "      - $f"
  done
  FAILED=1
else
  echo "PASS: No Mac-specific files found"
fi

echo ""
if [ "$FAILED" -ne 0 ]; then
  echo "╔══════════════════════════════════════════════════════════════╗"
  echo "║  BLOCKED: Clean up Mac infra before AWS deployment         ║"
  echo "║  See: .claude/rules/project/aws-migration.md               ║"
  echo "╚══════════════════════════════════════════════════════════════╝"
  exit 1
fi

echo "PASS: No Mac+AWS coexistence conflict."
exit 0
