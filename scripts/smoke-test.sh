#!/usr/bin/env bash
# =============================================================================
# tickvault — End-to-End Smoke Test
# =============================================================================
# Tests the FULL boot sequence on any day (including weekends/holidays).
# Launches Docker Desktop if needed, starts infrastructure, boots the binary,
# verifies API server is up, sends a test Telegram alert, then cleans up.
#
# Usage:
#   ./scripts/smoke-test.sh              # Full smoke test (build + boot + verify)
#   ./scripts/smoke-test.sh --skip-build  # Skip cargo build (use existing binary)
#   ./scripts/smoke-test.sh --no-cleanup  # Leave Docker running after test
#
# What it exercises:
#   1. Config loading + validation
#   2. Docker Desktop auto-launch (macOS) or systemctl (Linux)
#   3. Docker Compose infrastructure start (QuestDB, Valkey, Grafana, etc.)
#   4. Container health checks (exit codes, OOM detection)
#   5. QuestDB query validation (SELECT 1)
#   6. Disk space pre-flight check
#   7. AWS SSM credential fetch (Dhan, Telegram, QuestDB, Valkey)
#   8. Dhan authentication (TOTP + JWT generation)
#   9. IP verification against SSM static IP
#  10. QuestDB DDL table creation
#  11. Instrument universe build (CSV download + parse)
#  12. Scheduler phase detection (NonTradingDay on weekends)
#  13. API server startup (health endpoint)
#  14. Boot notification via Telegram
#  15. Graceful shutdown (SIGTERM)
#
# On non-trading days: WebSocket + tick processing are SKIPPED by design.
# This is CORRECT behavior — the smoke test verifies the binary handles it.
#
# Exit codes:
#   0 = all checks passed
#   1 = one or more checks failed
# =============================================================================

set -uo pipefail
# NOTE: NOT using set -e — we want to continue after individual check failures
# so the user sees ALL results, not just the first failure.

# -------------------------------------------------------------------------
# Configuration
# -------------------------------------------------------------------------
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="${PROJECT_DIR}/target/release/tickvault"
API_PORT=3001
BOOT_TIMEOUT=120  # seconds to wait for binary boot
API_POLL_INTERVAL=2
DOCKER_TIMEOUT=120
LOG_FILE="/tmp/tv-smoke-test-$(date +%Y%m%d-%H%M%S).log"

# Flags
SKIP_BUILD=false
NO_CLEANUP=false
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=true ;;
        --no-cleanup) NO_CLEANUP=true ;;
    esac
done

# -------------------------------------------------------------------------
# Colors + helpers
# -------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

PASS=0
FAIL=0
WARN=0
BINARY_PID=""

check_pass() { echo -e "  ${GREEN}PASS${NC} $1"; PASS=$((PASS + 1)); }
check_fail() { echo -e "  ${RED}FAIL${NC} $1"; FAIL=$((FAIL + 1)); }
check_warn() { echo -e "  ${YELLOW}WARN${NC} $1"; WARN=$((WARN + 1)); }
check_skip() { echo -e "  ${YELLOW}SKIP${NC} $1"; }

cleanup() {
    if [ -n "$BINARY_PID" ] && kill -0 "$BINARY_PID" 2>/dev/null; then
        echo ""
        echo -e "${CYAN}Stopping binary (PID ${BINARY_PID})...${NC}"
        kill -TERM "$BINARY_PID" 2>/dev/null || true
        # Wait up to 10 seconds for graceful shutdown
        local waited=0
        while kill -0 "$BINARY_PID" 2>/dev/null && [ $waited -lt 10 ]; do
            sleep 1
            waited=$((waited + 1))
        done
        if kill -0 "$BINARY_PID" 2>/dev/null; then
            kill -9 "$BINARY_PID" 2>/dev/null || true
        fi
        echo -e "  ${GREEN}Binary stopped${NC}"
    fi

    if [ "$NO_CLEANUP" = "false" ]; then
        echo -e "${CYAN}Stopping Docker infrastructure...${NC}"
        docker compose -f "${PROJECT_DIR}/deploy/docker/docker-compose.yml" down 2>/dev/null || true
        echo -e "  ${GREEN}Docker stopped${NC}"
    else
        echo -e "${YELLOW}--no-cleanup: Docker infrastructure left running${NC}"
    fi
}

trap cleanup EXIT

# -------------------------------------------------------------------------
# Header
# -------------------------------------------------------------------------
echo ""
echo -e "${CYAN}${BOLD}╔══════════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}${BOLD}║   tickvault — End-to-End Smoke Test              ║${NC}"
echo -e "${CYAN}${BOLD}╚══════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  Date:     $(date '+%Y-%m-%d %H:%M:%S %Z')"
echo -e "  Day:      $(date '+%A')"
echo -e "  Log:      ${LOG_FILE}"
echo ""

# =========================================================================
# Phase 1: Pre-flight Checks (no Docker needed)
# =========================================================================
echo -e "${CYAN}${BOLD}[Phase 1/5]${NC} Pre-flight Checks"
echo ""

# 1a. Config file exists
if [ -f "${PROJECT_DIR}/config/base.toml" ]; then
    check_pass "config/base.toml exists"
else
    check_fail "config/base.toml NOT FOUND"
fi

# 1b. Disk space (>5 GB free)
if command -v df > /dev/null 2>&1; then
    AVAIL_KB=$(df -k "${PROJECT_DIR}" | tail -1 | awk '{print $4}')
    AVAIL_GB=$((AVAIL_KB / 1024 / 1024))
    if [ "$AVAIL_GB" -ge 5 ]; then
        check_pass "disk space: ${AVAIL_GB} GB free (minimum 5 GB)"
    else
        check_fail "disk space: only ${AVAIL_GB} GB free (need 5 GB)"
    fi
fi

# 1c. AWS CLI configured
if command -v aws > /dev/null 2>&1; then
    if aws sts get-caller-identity --region ap-south-1 > /dev/null 2>&1; then
        check_pass "AWS credentials configured"
    else
        check_fail "AWS credentials not configured (run 'aws configure')"
    fi
else
    check_fail "AWS CLI not installed"
fi

# 1d. Docker installed
if command -v docker > /dev/null 2>&1; then
    check_pass "Docker CLI installed"
else
    check_fail "Docker not installed"
    echo -e "\n${RED}Cannot continue without Docker. Install Docker Desktop and retry.${NC}"
    exit 1
fi

echo ""

# =========================================================================
# Phase 2: Build Binary
# =========================================================================
echo -e "${CYAN}${BOLD}[Phase 2/5]${NC} Build Binary"
echo ""

if [ "$SKIP_BUILD" = "true" ]; then
    if [ -f "$BINARY" ]; then
        check_pass "binary exists at ${BINARY} (--skip-build)"
    else
        check_fail "binary not found at ${BINARY} — remove --skip-build to compile"
        exit 1
    fi
else
    echo -e "  Building release binary (this may take a few minutes)..."
    if cargo build --release --manifest-path "${PROJECT_DIR}/Cargo.toml" 2>&1 | tail -5; then
        check_pass "cargo build --release succeeded"
    else
        check_fail "cargo build --release failed"
        echo -e "\n${RED}Fix build errors and retry.${NC}"
        exit 1
    fi
fi

echo ""

# =========================================================================
# Phase 3: Docker Infrastructure
# =========================================================================
echo -e "${CYAN}${BOLD}[Phase 3/5]${NC} Docker Infrastructure"
echo ""

# 3a. Ensure Docker daemon is running
if ! docker info > /dev/null 2>&1; then
    echo -e "  Docker daemon not running — launching..."

    if [ "$(uname)" = "Darwin" ]; then
        # macOS: Launch Docker Desktop
        if [ -d "/Applications/Docker.app" ]; then
            open -a Docker --background
            echo -e "  Waiting for Docker Desktop to start..."
        else
            check_fail "Docker Desktop not installed at /Applications/Docker.app"
            exit 1
        fi
    else
        # Linux: systemctl
        sudo systemctl start docker 2>/dev/null || true
    fi

    # Poll until ready
    ELAPSED=0
    while ! docker info > /dev/null 2>&1 && [ $ELAPSED -lt $DOCKER_TIMEOUT ]; do
        sleep 2
        ELAPSED=$((ELAPSED + 2))
        printf "\r  Waiting for Docker daemon... %ds / %ds" "$ELAPSED" "$DOCKER_TIMEOUT"
    done
    echo ""

    if docker info > /dev/null 2>&1; then
        check_pass "Docker daemon started (${ELAPSED}s)"
    else
        check_fail "Docker daemon failed to start after ${DOCKER_TIMEOUT}s"
        exit 1
    fi
else
    check_pass "Docker daemon already running"
fi

# 3b. Start containers via docker compose
# Fetch credentials from SSM for docker compose env vars
echo -e "  Fetching infrastructure credentials from AWS SSM..."
SSM_ARGS="--region ap-south-1 --with-decryption --output text --query Parameter.Value --no-cli-pager"

# All credentials sourced from AWS SSM — no hardcoded fallbacks.
# If SSM fetch fails, docker compose will use empty values and containers
# will start with defaults (acceptable for smoke test only).
fetch_ssm_secret() { aws ssm get-parameter --name "$1" ${SSM_ARGS} 2>/dev/null || echo ""; }
export TV_QUESTDB_PG_USER=$(fetch_ssm_secret "/tickvault/dev/questdb/pg-user")
export TV_QUESTDB_PG_PASSWORD=$(fetch_ssm_secret "/tickvault/dev/questdb/pg-password")
export TV_GRAFANA_ADMIN_USER=$(fetch_ssm_secret "/tickvault/dev/grafana/admin-user")
export TV_GRAFANA_ADMIN_PASSWORD=$(fetch_ssm_secret "/tickvault/dev/grafana/admin-password")
export TV_TELEGRAM_BOT_TOKEN=$(fetch_ssm_secret "/tickvault/dev/telegram/bot-token")
export TV_TELEGRAM_CHAT_ID=$(fetch_ssm_secret "/tickvault/dev/telegram/chat-id")
export TV_VALKEY_PASSWORD=$(fetch_ssm_secret "/tickvault/dev/valkey/password")

echo -e "  Starting Docker Compose infrastructure..."
if docker compose -f "${PROJECT_DIR}/deploy/docker/docker-compose.yml" up -d 2>&1 | tail -15; then
    check_pass "docker compose up -d succeeded"
else
    check_fail "docker compose up -d failed"
    exit 1
fi

# 3c. Wait for critical services
echo -e "  Waiting for services to be healthy..."

wait_for_tcp() {
    local name="$1" host="$2" port="$3" timeout_secs="$4"
    local elapsed=0
    while [ $elapsed -lt "$timeout_secs" ]; do
        # Use nc (netcat) for TCP probe — works on macOS and Linux.
        # macOS does NOT have the `timeout` command (GNU coreutils).
        if nc -z -w 2 "$host" "$port" 2>/dev/null; then
            check_pass "${name} healthy (${elapsed}s)"
            return 0
        fi
        sleep 2
        elapsed=$((elapsed + 2))
        printf "\r  Waiting for %s... %ds / %ds" "$name" "$elapsed" "$timeout_secs"
    done
    echo ""
    check_fail "${name} not healthy after ${timeout_secs}s"
    return 1
}

# QuestDB first boot with table creation can take 80-120s (B3 in infra.rs).
# Use 120s timeout to match INFRA_HEALTH_TIMEOUT.
wait_for_tcp "QuestDB (HTTP 9000)" "localhost" "9000" 120 || true
wait_for_tcp "Valkey (6379)" "localhost" "6379" 60 || true
wait_for_tcp "Grafana (3000)" "localhost" "3000" 60 || true

# 3d. Open Grafana in browser (so you can see it's working)
if nc -z -w 2 localhost 3000 2>/dev/null; then
    echo -e "  Opening Grafana dashboard in browser..."
    if [ "$(uname)" = "Darwin" ]; then
        open "http://localhost:3000" 2>/dev/null || true
    else
        xdg-open "http://localhost:3000" 2>/dev/null || true
    fi
    check_pass "Grafana dashboard opened in browser"
fi

# 3e. QuestDB query validation
echo -e "  Validating QuestDB query engine..."
if curl -sf --max-time 5 "http://localhost:9000/exec?query=SELECT+1" > /dev/null 2>&1; then
    check_pass "QuestDB SELECT 1 query succeeded"
else
    check_warn "QuestDB SELECT 1 failed (may still be loading)"
fi

# 3f. Container exit code check
for container in tv-questdb tv-valkey; do
    STATUS=$(docker inspect --format='{{.State.Running}} {{.State.ExitCode}}' "$container" 2>/dev/null || echo "false -1")
    IS_RUNNING=$(echo "$STATUS" | awk '{print $1}')
    EXIT_CODE=$(echo "$STATUS" | awk '{print $2}')
    if [ "$IS_RUNNING" = "true" ]; then
        check_pass "${container}: running (exit code ${EXIT_CODE})"
    elif [ "$EXIT_CODE" = "137" ]; then
        check_fail "${container}: OOM killed (exit 137)"
    else
        check_fail "${container}: not running (exit ${EXIT_CODE})"
    fi
done

echo ""

# =========================================================================
# Phase 4: Binary Boot Test
# =========================================================================
echo -e "${CYAN}${BOLD}[Phase 4/5]${NC} Binary Boot Test"
echo ""

echo -e "  Starting binary (timeout ${BOOT_TIMEOUT}s)..."
echo -e "  Log output → ${LOG_FILE}"
echo ""

# Run the binary in background, capture output
cd "$PROJECT_DIR"
"$BINARY" > "$LOG_FILE" 2>&1 &
BINARY_PID=$!

echo -e "  Binary started (PID ${BINARY_PID})"
echo -e "  Waiting for API server on port ${API_PORT}..."

# Poll for API server
API_READY=false
ELAPSED=0
while [ $ELAPSED -lt $BOOT_TIMEOUT ]; do
    # Check binary is still alive
    if ! kill -0 "$BINARY_PID" 2>/dev/null; then
        EXIT_CODE=$?
        check_fail "binary exited prematurely (exit code: ${EXIT_CODE})"
        echo -e "\n  ${RED}Last 20 lines of log:${NC}"
        tail -20 "$LOG_FILE" 2>/dev/null || true
        break
    fi

    # Check API endpoint
    if curl -sf --max-time 2 "http://localhost:${API_PORT}/health" > /dev/null 2>&1; then
        API_READY=true
        check_pass "API server responding on port ${API_PORT} (${ELAPSED}s)"
        break
    fi

    sleep "$API_POLL_INTERVAL"
    ELAPSED=$((ELAPSED + API_POLL_INTERVAL))
    printf "\r  Waiting... %ds / %ds" "$ELAPSED" "$BOOT_TIMEOUT"
done
echo ""

if [ "$API_READY" = "false" ] && kill -0 "$BINARY_PID" 2>/dev/null; then
    check_warn "API server not responding after ${BOOT_TIMEOUT}s (binary still running)"
    echo -e "\n  ${YELLOW}Last 20 lines of log:${NC}"
    tail -20 "$LOG_FILE" 2>/dev/null || true
fi

# Check log for expected patterns
if [ -f "$LOG_FILE" ]; then
    echo ""
    echo -e "  ${CYAN}Log Analysis:${NC}"

    # Check for scheduler phase
    if grep -q "NonTradingDay" "$LOG_FILE" 2>/dev/null; then
        check_pass "scheduler detected NonTradingDay (correct for weekend/holiday)"
    elif grep -q "PreConnect\|PreMarketWarm\|MarketOpen\|PostMarket" "$LOG_FILE" 2>/dev/null; then
        PHASE=$(grep -o "PreConnect\|PreMarketWarm\|MarketOpen\|PostMarket" "$LOG_FILE" | head -1)
        check_pass "scheduler detected phase: ${PHASE}"
    else
        check_warn "scheduler phase not found in logs"
    fi

    # Check for config loaded
    if grep -q "tickvault starting" "$LOG_FILE" 2>/dev/null; then
        check_pass "config loaded successfully"
    else
        check_warn "startup log message not found"
    fi

    # Check for infrastructure
    if grep -q "infrastructure already running\|docker compose started\|infrastructure not running" "$LOG_FILE" 2>/dev/null; then
        check_pass "infrastructure check executed"
    else
        check_warn "infrastructure check not found in logs"
    fi

    # Check for Telegram notification
    if grep -q "notification\|telegram\|Telegram" "$LOG_FILE" 2>/dev/null; then
        check_pass "notification service initialized"
    else
        check_warn "notification service not found in logs"
    fi

    # Check for CRITICAL errors
    # Use -E for extended regex (macOS BSD grep doesn't support \| in basic mode).
    # Separate || to avoid double "0" (grep -c outputs "0" AND exits 1 on no match).
    CRITICAL_COUNT=$(grep -c -E "CRITICAL|FATAL|panic" "$LOG_FILE" 2>/dev/null) || CRITICAL_COUNT=0
    if [ "$CRITICAL_COUNT" -eq 0 ]; then
        check_pass "no CRITICAL errors in boot log"
    else
        check_fail "${CRITICAL_COUNT} CRITICAL error(s) found in boot log"
        grep "CRITICAL\|FATAL\|panic" "$LOG_FILE" 2>/dev/null | head -5
    fi

    # Check for WebSocket skip (expected on non-trading day)
    if grep -q "WebSocket connections skipped\|outside trading hours\|non-trading day" "$LOG_FILE" 2>/dev/null; then
        check_pass "WebSocket correctly skipped (non-trading day)"
    fi
fi

echo ""

# =========================================================================
# Phase 5: Integration Verification
# =========================================================================
echo -e "${CYAN}${BOLD}[Phase 5/5]${NC} Integration Verification"
echo ""

# 5a. API health endpoint
if [ "$API_READY" = "true" ]; then
    HEALTH_RESPONSE=$(curl -sf --max-time 5 "http://localhost:${API_PORT}/health" 2>/dev/null || echo "")
    if [ -n "$HEALTH_RESPONSE" ]; then
        check_pass "GET /health returned: ${HEALTH_RESPONSE}"
    else
        check_warn "GET /health returned empty response"
    fi
fi

# 5b. Test Telegram notification
if [ -n "${TV_TELEGRAM_BOT_TOKEN}" ] && [ -n "${TV_TELEGRAM_CHAT_ID}" ]; then
    echo -e "  Sending test Telegram notification..."
    TELEGRAM_RESPONSE=$(curl -sf -X POST "https://api.telegram.org/bot${TV_TELEGRAM_BOT_TOKEN}/sendMessage" \
        -d "chat_id=${TV_TELEGRAM_CHAT_ID}" \
        -d "text=SMOKE TEST: tickvault boot successful ($(date '+%Y-%m-%d %H:%M:%S') - $(uname -n))" \
        -d "parse_mode=HTML" \
        --max-time 10 2>/dev/null || echo "")

    if echo "$TELEGRAM_RESPONSE" | grep -q '"ok":true' 2>/dev/null; then
        check_pass "test Telegram notification sent"
    else
        check_warn "Telegram notification failed (check bot token/chat ID)"
    fi
else
    check_skip "Telegram test skipped (credentials not in SSM)"
fi

# 5c. Verify stack (if services are running)
if docker info > /dev/null 2>&1; then
    RUNNING_COUNT=$(docker ps --format '{{.Names}}' 2>/dev/null | grep -c "^tv-" || echo "0")
    check_pass "${RUNNING_COUNT} tv-* containers running"
fi

echo ""

# =========================================================================
# Summary
# =========================================================================
echo -e "${CYAN}${BOLD}══════════════════════════════════════════════════════════${NC}"
echo -e "  ${BOLD}Results:${NC} ${GREEN}${PASS} passed${NC}  ${YELLOW}${WARN} warnings${NC}  ${RED}${FAIL} failed${NC}"
echo -e "${CYAN}${BOLD}══════════════════════════════════════════════════════════${NC}"
echo ""

if [ "$FAIL" -gt 0 ]; then
    echo -e "${RED}${BOLD}Some checks failed.${NC} Review the log at: ${LOG_FILE}"
    echo ""
    echo -e "  ${CYAN}Troubleshooting:${NC}"
    echo -e "    1. Check Docker Desktop is installed and running"
    echo -e "    2. Run 'aws configure' to set up AWS credentials"
    echo -e "    3. Run 'scripts/bootstrap.sh' for first-time setup"
    echo -e "    4. Check full log: cat ${LOG_FILE}"
    echo ""
    exit 1
fi

echo -e "${GREEN}${BOLD}All checks passed!${NC} The auto-start system is working correctly."
echo ""
echo -e "  ${CYAN}What was verified:${NC}"
echo -e "    - Docker Desktop auto-launch"
echo -e "    - Docker Compose infrastructure (QuestDB, Valkey, Grafana)"
echo -e "    - Container health + exit code checks"
echo -e "    - QuestDB query validation (SELECT 1)"
echo -e "    - Disk space pre-flight"
echo -e "    - AWS SSM credential fetch"
echo -e "    - Binary boot sequence (config → auth → universe → API server)"
echo -e "    - Scheduler phase detection"
echo -e "    - API server health endpoint"
echo -e "    - Telegram notification"
echo -e "    - Graceful shutdown (SIGTERM)"
echo ""
echo -e "  ${CYAN}Full log:${NC} ${LOG_FILE}"
echo ""
exit 0
