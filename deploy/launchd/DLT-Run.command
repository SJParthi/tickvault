#!/bin/bash
# =============================================================================
# DLT Run — Manual trigger, identical to 8:15 AM autostart
# =============================================================================
# Double-click this file anytime. It does EVERYTHING the autostart does:
#
#   1. Stops any running binary (graceful shutdown)
#   2. Restores PATH (Terminal.app may have minimal env)
#   3. Starts Docker Desktop if not running
#   4. Pulls latest code from git
#   5. Fetches SSM secrets for infrastructure
#   6. Starts all 8 Docker infra containers
#   7. Builds release binary (only recompiles what changed)
#   8. Opens IntelliJ IDEA + monitoring dashboards
#   9. Runs binary with crash recovery (auto-restart on crash)
#
# Click it again — stops old binary, pulls fresh, rebuilds, runs fresh.
# Press Ctrl+C to stop the binary.
# =============================================================================

set -uo pipefail

# ---- Crash Recovery Settings ----
MAX_RESTARTS=5
INITIAL_BACKOFF=10  # seconds

# ---- Paths ----
LOG_DIR="${HOME}/.dlt/logs"
PID_FILE="${HOME}/.dlt/trader.pid"
RESTART_COUNT_FILE="${HOME}/.dlt/restart-count"
mkdir -p "${LOG_DIR}"
LOG_FILE="${LOG_DIR}/manual-$(date +%Y-%m-%d).log"

# ---- Logging (to file + terminal) ----
log() {
    local msg="[$(date '+%Y-%m-%d %H:%M:%S IST')] $*"
    echo "${msg}" >> "${LOG_FILE}"
    echo "         ${msg}"
}

# ---- Notifications ----
notify_mac() {
    osascript -e "display notification \"$1\" with title \"DLT\" subtitle \"${2:-}\"" 2>/dev/null || true
}

notify_telegram() {
    if [ -f "${PROJECT_DIR:-}/scripts/notify-telegram.sh" ]; then
        bash "${PROJECT_DIR}/scripts/notify-telegram.sh" "$1" >> "${LOG_FILE}" 2>&1 || true
    fi
}

notify_all() {
    local message="$1"
    local subtitle="${2:-}"
    log "${message}"
    notify_mac "${message}" "${subtitle}"
    notify_telegram "[Manual Start] ${message}"
}

# -------------------------------------------------------------------------
# Step 0: PATH setup + find project
# -------------------------------------------------------------------------
# Terminal.app or launchd may have minimal PATH — restore it.
export PATH="${HOME}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH}"

# Source shell profile for AWS credentials, ENVIRONMENT, and any custom PATH entries.
for rc in "${HOME}/.zprofile" "${HOME}/.zshrc" "${HOME}/.bash_profile" "${HOME}/.profile"; do
    if [ -f "${rc}" ]; then
        set +uo pipefail
        source "${rc}" 2>/dev/null || true
        set -uo pipefail
        break
    fi
done

# Find project directory
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
echo "║   dhan-live-trader — Manual Start (mirrors autostart)   ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "  Project:  $PROJECT_DIR"
echo "  Branch:   $(git branch --show-current 2>/dev/null || echo 'unknown')"
echo "  Time:     $(date '+%Y-%m-%d %H:%M:%S %Z')"
echo "  Day:      $(date '+%A')"
echo "  Log:      ${LOG_FILE}"
echo ""

log "========== DLT Manual Start =========="

# Verify critical tools are available
for tool in cargo docker aws; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "  ERROR: '${tool}' not found in PATH."
        notify_all "CRITICAL: '${tool}' not found in PATH" "Tool missing"
        read -rp "Press Enter to close..."
        exit 1
    fi
done

# -------------------------------------------------------------------------
# Step 1: Stop any running binary
# -------------------------------------------------------------------------
echo "  [1/8] Stopping running binary..."

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

# Also clean up stale PID file
rm -f "${PID_FILE}"
echo "0" > "${RESTART_COUNT_FILE}"

echo ""

# -------------------------------------------------------------------------
# Step 2: Start Docker Desktop if not running
# -------------------------------------------------------------------------
echo "  [2/8] Checking Docker Desktop..."

start_docker() {
    if docker info >/dev/null 2>&1; then
        echo "         Docker Desktop already running."
        return 0
    fi

    echo "         Starting Docker Desktop..."
    notify_mac "Starting Docker Desktop..." "Please wait"
    open -a Docker

    local wait=0
    while [ "${wait}" -lt 120 ]; do
        if docker info >/dev/null 2>&1; then
            echo "         Docker Desktop ready (waited ${wait}s)."
            return 0
        fi
        sleep 2
        wait=$((wait + 2))
        printf "\r         Waiting for Docker daemon... %ds" "${wait}"
    done
    echo ""

    notify_all "CRITICAL: Docker failed to start after 120s" "Docker timeout"
    return 1
}

if ! start_docker; then
    read -rp "Press Enter to close..."
    exit 1
fi

echo ""

# -------------------------------------------------------------------------
# Step 3: Pull latest code
# -------------------------------------------------------------------------
echo "  [3/8] Pulling latest code..."

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
    COMMIT=$(git rev-parse --short HEAD)
    echo "         Pull complete (${COMMIT})."
    notify_all "Code updated to ${COMMIT}" "Git pull"
else
    COMMIT=$(git rev-parse --short HEAD)
    echo ""
    echo "         WARNING: git pull failed — building with current local code (${COMMIT})."
    notify_all "WARNING: git pull failed — running ${COMMIT}" "Git failed"
fi

echo ""

# -------------------------------------------------------------------------
# Step 4: Fetch SSM secrets
# -------------------------------------------------------------------------
echo "  [4/8] Fetching SSM secrets..."

REGION="ap-south-1"
SSM_ENV="${ENVIRONMENT:-dev}"

fetch_ssm_secret() {
    aws ssm get-parameter \
        --region "${REGION}" \
        --name "$1" \
        --with-decryption \
        --output text \
        --query "Parameter.Value" \
        --no-cli-pager 2>/dev/null
}

# Verify AWS credentials are valid
if ! aws sts get-caller-identity --region "${REGION}" >/dev/null 2>&1; then
    echo "         ERROR: AWS credentials expired or not configured."
    notify_all "CRITICAL: AWS credentials expired or not configured" "Run: aws configure"
    read -rp "Press Enter to close..."
    exit 1
fi

export DLT_QUESTDB_PG_USER=$(fetch_ssm_secret "/dlt/${SSM_ENV}/questdb/pg-user")
export DLT_QUESTDB_PG_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/questdb/pg-password")
export DLT_GRAFANA_ADMIN_USER=$(fetch_ssm_secret "/dlt/${SSM_ENV}/grafana/admin-user")
export DLT_GRAFANA_ADMIN_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/grafana/admin-password")
export DLT_TELEGRAM_BOT_TOKEN=$(fetch_ssm_secret "/dlt/${SSM_ENV}/telegram/bot-token")
export DLT_TELEGRAM_CHAT_ID=$(fetch_ssm_secret "/dlt/${SSM_ENV}/telegram/chat-id")
export DLT_VALKEY_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/valkey/password")

echo "         SSM secrets loaded."
log "SSM secrets loaded"

echo ""

# -------------------------------------------------------------------------
# Step 5: Start infrastructure containers
# -------------------------------------------------------------------------
echo "  [5/8] Starting infrastructure containers..."

start_infra() {
    docker compose -f deploy/docker/docker-compose.yml up -d >> "${LOG_FILE}" 2>&1

    # Wait for QuestDB (critical dependency)
    local wait=0
    while [ "${wait}" -lt 60 ]; do
        if curl -sf --max-time 2 "http://localhost:9000" >/dev/null 2>&1; then
            echo "         Infrastructure ready (QuestDB waited ${wait}s)."
            return 0
        fi
        sleep 2
        wait=$((wait + 2))
        printf "\r         Waiting for QuestDB... %ds" "${wait}"
    done
    echo ""
    echo "         WARNING: QuestDB not responding after 60s (continuing — app retries)."
    return 0
}

start_infra

# Show container status
RUNNING=$(docker ps --filter "name=dlt-" --format '{{.Names}}' 2>/dev/null | wc -l | xargs)
echo "         ${RUNNING}/8 containers running."

echo ""

# -------------------------------------------------------------------------
# Step 6: Build release binary
# -------------------------------------------------------------------------
echo "  [6/8] Building release binary..."
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
    notify_all "CRITICAL: Build failed" "Fix and retry"
    read -rp "Press Enter to close..."
    exit 1
fi

echo ""

# -------------------------------------------------------------------------
# Step 7: Open IDE + monitoring dashboards
# -------------------------------------------------------------------------
echo "  [7/8] Opening IDE + dashboards..."

if [ -d "/Applications/IntelliJ IDEA.app" ]; then
    open -a "IntelliJ IDEA" "${PROJECT_DIR}" 2>/dev/null || true
    echo "         IntelliJ IDEA opened."
elif [ -d "/Applications/IntelliJ IDEA CE.app" ]; then
    open -a "IntelliJ IDEA CE" "${PROJECT_DIR}" 2>/dev/null || true
    echo "         IntelliJ IDEA CE opened."
else
    echo "         IntelliJ IDEA not found — skipping."
fi

open "http://localhost:3000" 2>/dev/null || true   # Grafana
open "http://localhost:9000" 2>/dev/null || true   # QuestDB
open "http://localhost:16686" 2>/dev/null || true  # Jaeger
open "http://localhost:9090" 2>/dev/null || true   # Prometheus
echo "         Dashboards opened (Grafana, QuestDB, Jaeger, Prometheus)."

echo ""

# -------------------------------------------------------------------------
# Step 8: Run binary with crash recovery
# -------------------------------------------------------------------------
BINARY="${PROJECT_DIR}/target/release/dhan-live-trader"

if ! [ -f "$BINARY" ]; then
    echo "  ERROR: Binary not found at ${BINARY}"
    notify_all "CRITICAL: Binary not found" "Build issue"
    read -rp "Press Enter to close..."
    exit 1
fi

echo "  [8/8] Starting trading system with crash recovery..."
echo ""
echo "  ─────────────────────────────────────────────────────────"
echo "  All systems go. Running release binary."
echo "  Crash recovery: max ${MAX_RESTARTS} restarts, exponential backoff."
echo ""
echo "  Press Ctrl+C to stop."
echo "  ─────────────────────────────────────────────────────────"
echo ""

notify_all "Starting dhan-live-trader..." "Running"

RESTART_COUNT=0
BACKOFF="${INITIAL_BACKOFF}"

while true; do
    cd "${PROJECT_DIR}"

    # Ensure Docker is still alive before each attempt
    if ! docker info >/dev/null 2>&1; then
        log "Docker died. Attempting recovery..."
        if ! start_docker; then
            notify_all "CRITICAL: Docker unrecoverable. Giving up." "Docker failed"
            break
        fi
        start_infra
    fi

    # Ensure infra containers are up
    RUNNING=$(docker ps --filter "name=dlt-" --format '{{.Names}}' 2>/dev/null | wc -l | xargs)
    if [ "${RUNNING}" -lt 8 ]; then
        log "Only ${RUNNING}/8 containers running. Restarting infra..."
        start_infra
    fi

    # Run the trader (foreground — Ctrl+C stops it)
    log "Starting binary (attempt $((RESTART_COUNT + 1))/${MAX_RESTARTS})..."
    "$BINARY" &
    TRADER_PID=$!
    echo "${TRADER_PID}" > "${PID_FILE}"

    if [ "${RESTART_COUNT}" -eq 0 ]; then
        notify_all "Trader started (PID ${TRADER_PID})" "Running"
    else
        notify_all "Trader restarted (PID ${TRADER_PID}, attempt ${RESTART_COUNT})" "Recovered"
    fi

    # Wait for process to exit
    wait "${TRADER_PID}" 2>/dev/null || true
    EXIT_CODE=$?
    rm -f "${PID_FILE}"

    # Clean exit (code 0) or Ctrl+C (code 130) = intentional shutdown
    if [ "${EXIT_CODE}" -eq 0 ] || [ "${EXIT_CODE}" -eq 130 ]; then
        notify_all "Trader exited cleanly (code ${EXIT_CODE}). Session complete." "Stopped"
        break
    fi

    # Crash detected
    RESTART_COUNT=$((RESTART_COUNT + 1))
    echo "${RESTART_COUNT}" > "${RESTART_COUNT_FILE}"

    # Check restart limit
    if [ "${RESTART_COUNT}" -ge "${MAX_RESTARTS}" ]; then
        notify_all "CRITICAL: Trader crashed ${RESTART_COUNT} times. Max restarts exceeded. HALTED." "HALTED"
        break
    fi

    # Notify and backoff
    notify_all "Trader crashed (exit ${EXIT_CODE}). Restarting in ${BACKOFF}s... (${RESTART_COUNT}/${MAX_RESTARTS})" "Crash #${RESTART_COUNT}"

    sleep "${BACKOFF}"
    BACKOFF=$((BACKOFF * 2))
done

# Cleanup
rm -f "${PID_FILE}"
echo "0" > "${RESTART_COUNT_FILE}"
log "========== DLT Manual Start Ended =========="

echo ""
echo "  Session ended. Close this window or press Enter."
read -rp ""
