#!/bin/bash
# ============================================================
# ONE-CLICK: Run all 200-level depth tests for Dhan Google Meet
# ============================================================
#
# BEFORE THE CALL:
#   pip install websockets requests
#
# USAGE:
#   # Option 1: Set env vars first
#   export DHAN_ACCESS_TOKEN="<YOUR_TOKEN>"
#   export DHAN_CLIENT_ID="<YOUR_CLIENT_ID>"
#   bash scripts/depth200-run-all.sh
#
#   # Option 2: Inline
#   DHAN_ACCESS_TOKEN="<YOUR_TOKEN>" DHAN_CLIENT_ID="<YOUR_CLIENT_ID>" bash scripts/depth200-run-all.sh
#
# WHAT IT RUNS:
#   1. depth200-quick-test.py  — Tests both URLs, shows PASS/FAIL
#   2. (if test passes) depth200-live-viewer.py — Shows live order book
#
# ============================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "============================================================"
echo "  Dhan 200-Level Depth — Test Suite"
echo "  $(date '+%Y-%m-%d %H:%M:%S IST')"
echo "============================================================"
echo ""

# Check deps
python3 -c "import websockets" 2>/dev/null || { echo "ERROR: pip install websockets requests"; exit 1; }
python3 -c "import requests" 2>/dev/null || { echo "ERROR: pip install websockets requests"; exit 1; }

# Check token
if [ -z "${DHAN_ACCESS_TOKEN:-}" ]; then
    if [ -f "data/cache/tv-token-cache" ]; then
        echo "[OK] Will read token from data/cache/tv-token-cache"
    else
        echo "ERROR: Set DHAN_ACCESS_TOKEN env var or run the app to create token cache"
        exit 1
    fi
else
    echo "[OK] Token from environment"
fi

echo ""
echo "========================================="
echo "  STEP 1: Quick diagnostic test"
echo "========================================="
echo ""

python3 "$SCRIPT_DIR/depth200-quick-test.py"

echo ""
echo "========================================="
echo "  STEP 2: Live depth viewer"
echo "========================================="
echo ""
echo "Starting live depth viewer..."
echo "(Press Ctrl+C to stop)"
echo ""

python3 "$SCRIPT_DIR/depth200-live-viewer.py"
