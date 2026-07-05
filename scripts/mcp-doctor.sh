#!/usr/bin/env bash
# mcp-doctor.sh — probes every endpoint the tickvault-logs MCP server
# reads from and prints pass/fail. Purpose: verify that Claude Code
# (running wherever this script runs) can reach the tickvault
# observability chain.
#
# Prints a summary the user can screenshot if anything fails.
#
# Env vars (override defaults):
#   TICKVAULT_LOGS_DIR          default: <repo>/data/logs
#   TICKVAULT_QUESTDB_URL       default: http://127.0.0.1:9000
#   TICKVAULT_API_URL           default: http://127.0.0.1:3001
# Prometheus/Alertmanager probes removed in #O5 (2026-05-30) — those
# containers were removed in #O3/#O2 (CloudWatch-only observability).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOGS_DIR="${TICKVAULT_LOGS_DIR:-$REPO_ROOT/data/logs}"
QDB_URL="${TICKVAULT_QUESTDB_URL:-http://127.0.0.1:9000}"
API_URL="${TICKVAULT_API_URL:-http://127.0.0.1:3001}"

PASS=0
FAIL=0
FAILED_CHECKS=()

check_pass() {
    echo "  [PASS] $1"
    PASS=$((PASS + 1))
}

check_fail() {
    echo "  [FAIL] $1 — $2"
    FAIL=$((FAIL + 1))
    FAILED_CHECKS+=("$1")
}

probe_url() {
    local name="$1"
    local url="$2"
    local path="$3"
    local timeout_s=3

    if curl --silent --show-error --max-time "$timeout_s" \
            --output /dev/null --fail \
            "$url$path" 2>/dev/null; then
        check_pass "$name ($url)"
    else
        check_fail "$name ($url)" "unreachable or returned non-2xx"
    fi
}

echo "============================================================"
echo "  MCP reachability doctor"
echo "============================================================"
echo

echo "--- logs directory ---"
if [[ -d "$LOGS_DIR" ]]; then
    jsonl_count=$(find "$LOGS_DIR/machine" "$LOGS_DIR" -maxdepth 1 -name 'errors.jsonl.*' 2>/dev/null | wc -l | tr -d ' ')
    summary_exists="no"
    { [[ -f "$LOGS_DIR/machine/errors.summary.md" ]] || [[ -f "$LOGS_DIR/errors.summary.md" ]]; } && summary_exists="yes"
    check_pass "logs dir exists: $LOGS_DIR (jsonl_files=$jsonl_count summary=$summary_exists)"
else
    check_fail "logs dir: $LOGS_DIR" "directory does not exist"
fi
echo

echo "--- HTTP endpoints ---"
probe_url "QuestDB"      "$QDB_URL"  "/exec?query=SELECT%201"
probe_url "tickvault API" "$API_URL" "/health"
echo

echo "--- Docker CLI presence ---"
if command -v docker >/dev/null 2>&1; then
    check_pass "docker CLI on PATH"
else
    check_fail "docker CLI" "not installed (docker_status MCP tool will fail)"
fi
echo

echo "============================================================"
echo "  PASS: $PASS    FAIL: $FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
    echo
    echo "Failed checks:"
    for check in "${FAILED_CHECKS[@]}"; do
        echo "  - $check"
    done
    echo
    echo "Fix: see docs/runbooks/claude-mcp-access.md"
    exit 1
fi

echo "All MCP endpoints reachable — zero-touch chain is live."
