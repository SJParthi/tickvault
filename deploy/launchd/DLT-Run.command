#!/bin/bash
# =============================================================================
# DLT Run — The ONE file for everything
# =============================================================================
# Double-click this file anytime. It does EVERYTHING:
#
#   1. Stops any running binary (graceful shutdown)
#   2. Pulls latest code from git
#   3. Builds release binary (only recompiles what changed)
#   4. Launches Docker Desktop if not running
#   5. Runs the full production flow (same as 8:15 AM auto-start)
#
# Click it again and again — each time it deploys fresh and runs fresh.
# Press Ctrl+C to stop the binary.
# =============================================================================

set -uo pipefail

# -------------------------------------------------------------------------
# Find project directory
# -------------------------------------------------------------------------
if [ -d "${DLT_PROJECT_DIR:-}" ]; then
    PROJECT_DIR="$DLT_PROJECT_DIR"
elif [ -d "${HOME}/IdeaProjects/dhan-live-trader" ]; then
    PROJECT_DIR="${HOME}/IdeaProjects/dhan-live-trader"
elif [ -d "/Users/Shared/dhan-live-trader" ]; then
    PROJECT_DIR="/Users/Shared/dhan-live-trader"
elif [ -d "${HOME}/dhan-live-trader" ]; then
    PROJECT_DIR="${HOME}/dhan-live-trader"
else
    echo ""
    echo "ERROR: Cannot find dhan-live-trader project directory."
    echo "Set DLT_PROJECT_DIR and retry."
    echo ""
    read -rp "Press Enter to close..."
    exit 1
fi

cd "$PROJECT_DIR" || exit 1

echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "║   dhan-live-trader — Deploy & Run                       ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "  Project:  $PROJECT_DIR"
echo "  Branch:   $(git branch --show-current 2>/dev/null || echo 'unknown')"
echo "  Time:     $(date '+%Y-%m-%d %H:%M:%S %Z')"
echo "  Day:      $(date '+%A')"
echo ""

# -------------------------------------------------------------------------
# Step 1: Stop any running binary
# -------------------------------------------------------------------------
echo "  [1/4] Stopping running binary..."

RUNNING_PID=$(pgrep -f "target/release/dhan-live-trader" 2>/dev/null || true)

if [ -n "$RUNNING_PID" ]; then
    echo "         Found PID $RUNNING_PID — sending SIGTERM..."
    kill "$RUNNING_PID" 2>/dev/null || true

    WAIT=0
    while [ $WAIT -lt 10 ] && kill -0 "$RUNNING_PID" 2>/dev/null; do
        sleep 1
        WAIT=$((WAIT + 1))
        printf "\r         Waiting for shutdown... %ds" "$WAIT"
    done
    echo ""

    if kill -0 "$RUNNING_PID" 2>/dev/null; then
        echo "         Still running — SIGKILL..."
        kill -9 "$RUNNING_PID" 2>/dev/null || true
        sleep 1
    fi
    echo "         Stopped."
else
    echo "         No running binary — fresh start."
fi

echo ""

# -------------------------------------------------------------------------
# Step 2: Pull latest code
# -------------------------------------------------------------------------
echo "  [2/4] Pulling latest code..."

BRANCH=$(git branch --show-current 2>/dev/null)
if [ -z "$BRANCH" ]; then
    echo "         ERROR: Not on any branch."
    read -rp "Press Enter to close..."
    exit 1
fi

# Warn about uncommitted changes but don't block
if ! git diff --quiet 2>/dev/null || ! git diff --cached --quiet 2>/dev/null; then
    echo "         NOTE: You have uncommitted local changes (they are safe)."
fi

if git pull origin "$BRANCH" 2>&1; then
    echo "         Pull complete."
else
    echo ""
    echo "         WARNING: git pull failed — building with current local code."
fi

echo ""

# -------------------------------------------------------------------------
# Step 3: Build release binary
# -------------------------------------------------------------------------
echo "  [3/4] Building release binary..."
echo ""

BUILD_START=$(date +%s)

if cargo build --release 2>&1; then
    BUILD_END=$(date +%s)
    BUILD_SECS=$((BUILD_END - BUILD_START))
    echo ""
    echo "         Build complete (${BUILD_SECS}s)."
else
    echo ""
    echo "         ERROR: Build failed. Fix errors and double-click again."
    read -rp "Press Enter to close..."
    exit 1
fi

echo ""

# -------------------------------------------------------------------------
# Step 4: Run the full production flow
# -------------------------------------------------------------------------
echo "  [4/4] Starting trading system..."
echo ""
echo "  ─────────────────────────────────────────────────────────"
echo "  Deploy complete. Running full production flow..."
echo "  (kill switch → DNS flush → Docker → network → binary)"
echo ""
echo "  Press Ctrl+C to stop."
echo "  ─────────────────────────────────────────────────────────"
echo ""

export DLT_PROJECT_DIR="$PROJECT_DIR"
exec bash "$PROJECT_DIR/deploy/launchd/trading-launcher.sh"
