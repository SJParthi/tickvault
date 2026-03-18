#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Stop Auto-Started Trader (macOS)
# =============================================================================
# Gracefully stops the headless cargo run process started by dlt-daily-start.sh.
# Use this before running in IntelliJ (to avoid two instances).
# =============================================================================

set -euo pipefail

PID_FILE="${HOME}/.dlt/trader.pid"

if [ ! -f "${PID_FILE}" ]; then
    echo "No running trader found (no PID file at ${PID_FILE})"
    exit 0
fi

PID=$(cat "${PID_FILE}" 2>/dev/null || echo "")

if [ -z "${PID}" ]; then
    echo "Empty PID file. Cleaning up."
    rm -f "${PID_FILE}"
    exit 0
fi

if ! kill -0 "${PID}" 2>/dev/null; then
    echo "Trader not running (stale PID ${PID}). Cleaning up."
    rm -f "${PID_FILE}"
    exit 0
fi

echo "Stopping trader (PID ${PID})..."
kill "${PID}"

# Wait for graceful shutdown (up to 30s — matches SIGTERM handler in main.rs)
for i in $(seq 1 30); do
    if ! kill -0 "${PID}" 2>/dev/null; then
        echo "Trader stopped gracefully."
        rm -f "${PID_FILE}"
        exit 0
    fi
    sleep 1
done

echo "Graceful shutdown timed out. Force killing..."
kill -9 "${PID}" 2>/dev/null || true
rm -f "${PID_FILE}"
echo "Trader killed."
