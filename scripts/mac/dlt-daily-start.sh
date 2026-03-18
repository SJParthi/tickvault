#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Daily Auto-Start with Crash Recovery (macOS)
# =============================================================================
# Triggered by launchd at 8:15 AM IST daily.
# Mirrors AWS systemd: start infra -> start binary -> auto-restart on crash.
#
# Self-healing behavior:
#   - Crash → auto-restart with exponential backoff (10s, 20s, 40s, 80s, 160s)
#   - Max 5 restarts per session → CRITICAL alert, stop retrying
#   - Telegram notification on every lifecycle event
#   - macOS notification for visual feedback
#   - Docker auto-recovery if daemon stops
#
# Flow:
#   8:10 AM — Mac wakes (pmset)
#   8:15 AM — This script fires (launchd)
#     1. Restore PATH (launchd has minimal env)
#     2. Start Docker Desktop if sleeping
#     3. Fetch SSM secrets, start infra containers
#     4. Open IntelliJ IDEA with project
#     5. cargo run with crash recovery loop
#
# Install: ./scripts/mac/install-autostart.sh
# Stop:    ~/.dlt/dlt-stop.sh
# Status:  ~/.dlt/dlt-status.sh
# Logs:    ~/.dlt/logs/autostart-YYYY-MM-DD.log
# =============================================================================

set -euo pipefail

# ---- Configuration (replaced by install-autostart.sh) ----
PROJECT_DIR="__PROJECT_DIR__"

# ---- Crash Recovery Settings ----
MAX_RESTARTS=5
INITIAL_BACKOFF=10  # seconds

# ---- Paths ----
LOG_DIR="${HOME}/.dlt/logs"
PID_FILE="${HOME}/.dlt/trader.pid"
RESTART_COUNT_FILE="${HOME}/.dlt/restart-count"
mkdir -p "${LOG_DIR}"
LOG_FILE="${LOG_DIR}/autostart-$(date +%Y-%m-%d).log"

# ---- Logging ----
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S IST')] $*" >> "${LOG_FILE}"
}

# ---- Notifications ----
notify_mac() {
    osascript -e "display notification \"$1\" with title \"DLT\" subtitle \"${2:-}\"" 2>/dev/null || true
}

notify_telegram() {
    if [ -f "${PROJECT_DIR}/scripts/notify-telegram.sh" ]; then
        bash "${PROJECT_DIR}/scripts/notify-telegram.sh" "$1" >> "${LOG_FILE}" 2>&1 || true
    fi
}

notify_all() {
    local message="$1"
    local subtitle="${2:-}"
    log "${message}"
    notify_mac "${message}" "${subtitle}"
    notify_telegram "[Mac Auto-Start] ${message}"
}

log "========== DLT Daily Auto-Start =========="
log "Project: ${PROJECT_DIR}"

# ---- 0. PATH setup ----
# launchd provides only /usr/bin:/bin:/usr/sbin:/sbin
# We need: cargo, docker, aws, git
export PATH="${HOME}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

# Source shell profile for AWS credentials, ENVIRONMENT, and any custom PATH entries.
for rc in "${HOME}/.zprofile" "${HOME}/.zshrc" "${HOME}/.bash_profile" "${HOME}/.profile"; do
    if [ -f "${rc}" ]; then
        set +euo pipefail
        source "${rc}" 2>/dev/null || true
        set -euo pipefail
        log "Sourced ${rc}"
        break
    fi
done

# Verify critical tools are available
for tool in cargo docker aws; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        notify_all "CRITICAL: '${tool}' not found in PATH" "Tool missing"
        exit 1
    fi
done
log "Tools OK (cargo, docker, aws)"

# ---- 1. Check if already running ----
if [ -f "${PID_FILE}" ]; then
    OLD_PID=$(cat "${PID_FILE}" 2>/dev/null || echo "")
    if [ -n "${OLD_PID}" ] && kill -0 "${OLD_PID}" 2>/dev/null; then
        log "Trader already running (PID ${OLD_PID}). Skipping."
        exit 0
    fi
    rm -f "${PID_FILE}"
fi

# Reset daily restart counter
echo "0" > "${RESTART_COUNT_FILE}"

# ---- 2. Start Docker Desktop if not running ----
start_docker() {
    if docker info >/dev/null 2>&1; then
        log "Docker Desktop already running"
        return 0
    fi

    log "Starting Docker Desktop..."
    notify_mac "Starting Docker Desktop..." "Please wait"
    open -a Docker

    local wait=0
    while [ "${wait}" -lt 120 ]; do
        if docker info >/dev/null 2>&1; then
            log "Docker Desktop ready (waited ${wait}s)"
            return 0
        fi
        sleep 2
        wait=$((wait + 2))
    done

    notify_all "CRITICAL: Docker failed to start after 120s" "Docker timeout"
    return 1
}

if ! start_docker; then
    exit 1
fi

# ---- 3. Pull latest code from main ----
cd "${PROJECT_DIR}"
log "Pulling latest code..."
if git fetch origin main >> "${LOG_FILE}" 2>&1 && git merge origin/main >> "${LOG_FILE}" 2>&1; then
    COMMIT=$(git rev-parse --short HEAD)
    log "Code updated to ${COMMIT}"
    notify_all "Code updated to ${COMMIT}" "Git pull"
else
    COMMIT=$(git rev-parse --short HEAD)
    log "WARNING: git pull failed — running existing code (${COMMIT})"
    notify_all "WARNING: git pull failed — running ${COMMIT}" "Git failed"
fi

# ---- 4. Fetch SSM secrets and start infra ----
log "Fetching SSM secrets..."

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
    notify_all "CRITICAL: AWS credentials expired or not configured" "Run: aws configure"
    exit 1
fi

export DLT_QUESTDB_PG_USER=$(fetch_ssm_secret "/dlt/${SSM_ENV}/questdb/pg-user")
export DLT_QUESTDB_PG_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/questdb/pg-password")
export DLT_GRAFANA_ADMIN_USER=$(fetch_ssm_secret "/dlt/${SSM_ENV}/grafana/admin-user")
export DLT_GRAFANA_ADMIN_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/grafana/admin-password")
export DLT_TELEGRAM_BOT_TOKEN=$(fetch_ssm_secret "/dlt/${SSM_ENV}/telegram/bot-token")
export DLT_TELEGRAM_CHAT_ID=$(fetch_ssm_secret "/dlt/${SSM_ENV}/telegram/chat-id")
export DLT_VALKEY_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/valkey/password")
log "SSM secrets loaded"

# ---- 5. Start infrastructure containers ----
start_infra() {
    log "Starting infrastructure containers..."
    docker compose -f deploy/docker/docker-compose.yml up -d >> "${LOG_FILE}" 2>&1

    # Wait for QuestDB (critical dependency)
    local wait=0
    while [ "${wait}" -lt 60 ]; do
        if curl -sf --max-time 2 "http://localhost:9000" >/dev/null 2>&1; then
            log "Infrastructure ready (QuestDB waited ${wait}s)"
            return 0
        fi
        sleep 2
        wait=$((wait + 2))
    done
    log "WARNING: QuestDB not responding after 60s (continuing anyway — app retries)"
    return 0
}

start_infra

# ---- 6. Open IntelliJ IDEA ----
log "Opening IntelliJ IDEA..."
if [ -d "/Applications/IntelliJ IDEA.app" ]; then
    open -a "IntelliJ IDEA" "${PROJECT_DIR}" 2>/dev/null || true
elif [ -d "/Applications/IntelliJ IDEA CE.app" ]; then
    open -a "IntelliJ IDEA CE" "${PROJECT_DIR}" 2>/dev/null || true
else
    log "WARNING: IntelliJ IDEA not found in /Applications"
fi

# ---- 7. Open monitoring dashboards ----
log "Opening monitoring dashboards..."
open "http://localhost:3000" 2>/dev/null || true   # Grafana
open "http://localhost:9000" 2>/dev/null || true   # QuestDB
open "http://localhost:16686" 2>/dev/null || true  # Jaeger
open "http://localhost:9090" 2>/dev/null || true   # Prometheus

# ---- 8. Start trading system with crash recovery ----
notify_all "Starting dhan-live-trader..." "Building"

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

    # Run the trader
    log "Starting cargo run (attempt $((RESTART_COUNT + 1))/${MAX_RESTARTS})..."
    cargo run -p dhan-live-trader-app >> "${LOG_FILE}" 2>&1 &
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

    # Clean exit (code 0) = intentional shutdown, don't restart
    if [ "${EXIT_CODE}" -eq 0 ]; then
        notify_all "Trader exited cleanly (code 0). Session complete." "Stopped"
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
log "========== DLT Daily Auto-Start Ended =========="
