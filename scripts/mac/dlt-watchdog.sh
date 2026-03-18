#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Watchdog Health Monitor (macOS)
# =============================================================================
# Runs every 5 minutes via launchd. Checks system health and auto-recovers.
# This is the SECOND layer of reliability (dlt-daily-start.sh is the first).
#
# Checks:
#   1. Is the trader process alive?
#   2. Is Docker Desktop running?
#   3. Are all 8 infra containers healthy?
#   4. Is QuestDB responding?
#   5. During market hours: are ticks flowing? (QuestDB query)
#
# Actions:
#   - Trader dead during market hours → restart via dlt-daily-start.sh
#   - Docker dead → start Docker → restart infra
#   - Containers unhealthy → docker compose up -d
#   - No ticks during market hours → Telegram alert (WebSocket issue)
#   - All actions logged + Telegram notification
#
# Install: ./scripts/mac/install-autostart.sh (installs both trader + watchdog)
# =============================================================================

set -euo pipefail

# ---- Configuration (replaced by install-autostart.sh) ----
PROJECT_DIR="__PROJECT_DIR__"

# ---- Paths ----
LOG_DIR="${HOME}/.dlt/logs"
PID_FILE="${HOME}/.dlt/trader.pid"
WATCHDOG_STATE="${HOME}/.dlt/watchdog-state"
mkdir -p "${LOG_DIR}"
LOG_FILE="${LOG_DIR}/watchdog-$(date +%Y-%m-%d).log"

# ---- PATH setup (same as daily start — launchd has minimal env) ----
export PATH="${HOME}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
for rc in "${HOME}/.zprofile" "${HOME}/.zshrc" "${HOME}/.bash_profile" "${HOME}/.profile"; do
    if [ -f "${rc}" ]; then
        set +euo pipefail
        source "${rc}" 2>/dev/null || true
        set -euo pipefail
        break
    fi
done

# ---- Logging ----
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S IST')] [WATCHDOG] $*" >> "${LOG_FILE}"
}

# ---- Notifications ----
notify_telegram() {
    if [ -f "${PROJECT_DIR}/scripts/notify-telegram.sh" ]; then
        bash "${PROJECT_DIR}/scripts/notify-telegram.sh" "[Watchdog] $1" >> "${LOG_FILE}" 2>&1 || true
    fi
}

notify_all() {
    log "$1"
    notify_telegram "$1"
    osascript -e "display notification \"$1\" with title \"DLT Watchdog\"" 2>/dev/null || true
}

# ---- Market Hours Detection (IST) ----
# Market: 9:00-15:30 IST, Mon-Fri
# Pre-market prep: 8:15-9:00
# Post-market: 15:30-16:00
is_market_hours() {
    local day hour minute
    # Use IST (UTC+5:30)
    day=$(TZ="Asia/Kolkata" date +%u)    # 1=Mon, 7=Sun
    hour=$(TZ="Asia/Kolkata" date +%H)
    minute=$(TZ="Asia/Kolkata" date +%M)

    # Weekend: no market
    if [ "${day}" -gt 5 ]; then
        return 1
    fi

    local time_mins=$((10#${hour} * 60 + 10#${minute}))
    # Market hours: 9:00 (540) to 15:30 (930)
    if [ "${time_mins}" -ge 540 ] && [ "${time_mins}" -le 930 ]; then
        return 0
    fi
    return 1
}

# Should the trader be running? (8:15 AM to 16:00 PM on weekdays)
is_trading_session() {
    local day hour minute
    day=$(TZ="Asia/Kolkata" date +%u)
    hour=$(TZ="Asia/Kolkata" date +%H)
    minute=$(TZ="Asia/Kolkata" date +%M)

    if [ "${day}" -gt 5 ]; then
        return 1
    fi

    local time_mins=$((10#${hour} * 60 + 10#${minute}))
    # Trading session: 8:15 (495) to 16:00 (960)
    if [ "${time_mins}" -ge 495 ] && [ "${time_mins}" -le 960 ]; then
        return 0
    fi
    return 1
}

# ---- State tracking (prevent alert spam) ----
# Only alert once per issue, reset when resolved
was_alerted() {
    local key="$1"
    grep -q "^${key}$" "${WATCHDOG_STATE}" 2>/dev/null
}

set_alerted() {
    local key="$1"
    if ! was_alerted "${key}"; then
        echo "${key}" >> "${WATCHDOG_STATE}"
    fi
}

clear_alert() {
    local key="$1"
    if [ -f "${WATCHDOG_STATE}" ]; then
        grep -v "^${key}$" "${WATCHDOG_STATE}" > "${WATCHDOG_STATE}.tmp" 2>/dev/null || true
        mv "${WATCHDOG_STATE}.tmp" "${WATCHDOG_STATE}"
    fi
}

# Reset state file daily
TODAY=$(date +%Y-%m-%d)
STATE_DATE=$(head -1 "${WATCHDOG_STATE}" 2>/dev/null || echo "")
if [ "${STATE_DATE}" != "DATE:${TODAY}" ]; then
    echo "DATE:${TODAY}" > "${WATCHDOG_STATE}"
fi

# ---- Health Checks ----
ISSUES=0

# Check 1: Is the trader process alive?
check_trader_process() {
    if [ ! -f "${PID_FILE}" ]; then
        if is_trading_session; then
            log "Trader not running (no PID file) during trading session"
            if ! was_alerted "trader_dead"; then
                notify_all "Trader not running during trading session. Attempting restart..."
                set_alerted "trader_dead"
                # Trigger restart via the daily start script
                nohup bash "${HOME}/.dlt/dlt-daily-start.sh" >> "${LOG_FILE}" 2>&1 &
            fi
            ISSUES=$((ISSUES + 1))
        fi
        return
    fi

    local pid
    pid=$(cat "${PID_FILE}" 2>/dev/null || echo "")
    if [ -z "${pid}" ] || ! kill -0 "${pid}" 2>/dev/null; then
        log "Trader process dead (stale PID ${pid})"
        rm -f "${PID_FILE}"
        if is_trading_session; then
            if ! was_alerted "trader_dead"; then
                notify_all "Trader crashed (PID ${pid}). Attempting restart..."
                set_alerted "trader_dead"
                nohup bash "${HOME}/.dlt/dlt-daily-start.sh" >> "${LOG_FILE}" 2>&1 &
            fi
            ISSUES=$((ISSUES + 1))
        fi
    else
        clear_alert "trader_dead"
    fi
}

# Check 2: Is Docker running?
check_docker() {
    if ! docker info >/dev/null 2>&1; then
        log "Docker Desktop not running"
        if is_trading_session; then
            if ! was_alerted "docker_dead"; then
                notify_all "Docker Desktop stopped. Attempting recovery..."
                set_alerted "docker_dead"
                open -a Docker 2>/dev/null || true
            fi
            ISSUES=$((ISSUES + 1))
        fi
    else
        clear_alert "docker_dead"
    fi
}

# Check 3: Are all 8 containers healthy?
check_containers() {
    if ! docker info >/dev/null 2>&1; then
        return  # Docker not running, handled by check_docker
    fi

    local running
    running=$(docker ps --filter "name=dlt-" --format '{{.Names}}' 2>/dev/null | wc -l | xargs)

    if [ "${running}" -lt 8 ]; then
        log "Only ${running}/8 infra containers running"
        if is_trading_session; then
            if ! was_alerted "containers_unhealthy"; then
                notify_all "Infra degraded: ${running}/8 containers. Restarting..."
                set_alerted "containers_unhealthy"

                # Re-fetch SSM secrets for docker-compose env
                local ssm_env="${ENVIRONMENT:-dev}"
                export DLT_QUESTDB_PG_USER=$(aws ssm get-parameter --region ap-south-1 --name "/dlt/${ssm_env}/questdb/pg-user" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null || echo "")
                export DLT_QUESTDB_PG_PASSWORD=$(aws ssm get-parameter --region ap-south-1 --name "/dlt/${ssm_env}/questdb/pg-password" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null || echo "")
                export DLT_GRAFANA_ADMIN_USER=$(aws ssm get-parameter --region ap-south-1 --name "/dlt/${ssm_env}/grafana/admin-user" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null || echo "")
                export DLT_GRAFANA_ADMIN_PASSWORD=$(aws ssm get-parameter --region ap-south-1 --name "/dlt/${ssm_env}/grafana/admin-password" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null || echo "")
                export DLT_TELEGRAM_BOT_TOKEN=$(aws ssm get-parameter --region ap-south-1 --name "/dlt/${ssm_env}/telegram/bot-token" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null || echo "")
                export DLT_TELEGRAM_CHAT_ID=$(aws ssm get-parameter --region ap-south-1 --name "/dlt/${ssm_env}/telegram/chat-id" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null || echo "")
                export DLT_VALKEY_PASSWORD=$(aws ssm get-parameter --region ap-south-1 --name "/dlt/${ssm_env}/valkey/password" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null || echo "")

                docker compose -f "${PROJECT_DIR}/deploy/docker/docker-compose.yml" up -d >> "${LOG_FILE}" 2>&1 || true
            fi
            ISSUES=$((ISSUES + 1))
        fi
    else
        clear_alert "containers_unhealthy"
    fi
}

# Check 4: Is QuestDB responding?
check_questdb() {
    if ! docker info >/dev/null 2>&1; then
        return
    fi

    if ! curl -sf --max-time 3 "http://localhost:9000" >/dev/null 2>&1; then
        log "QuestDB not responding"
        if is_trading_session; then
            if ! was_alerted "questdb_dead"; then
                notify_all "QuestDB not responding. Restarting container..."
                set_alerted "questdb_dead"
                docker restart dlt-questdb >> "${LOG_FILE}" 2>&1 || true
            fi
            ISSUES=$((ISSUES + 1))
        fi
    else
        clear_alert "questdb_dead"
    fi
}

# Check 5: Are ticks flowing? (market hours only)
check_tick_flow() {
    if ! is_market_hours; then
        return  # Only check during market hours
    fi

    if ! curl -sf --max-time 3 "http://localhost:9000" >/dev/null 2>&1; then
        return  # QuestDB down, handled by check_questdb
    fi

    # Query QuestDB for ticks in last 5 minutes
    local tick_count
    tick_count=$(curl -sf --max-time 5 -G "http://localhost:9000/exec" \
        --data-urlencode "query=SELECT count() FROM ticks WHERE ts > dateadd('m', -5, now())" \
        2>/dev/null | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['dataset'][0][0] if d.get('dataset') else 0)" 2>/dev/null || echo "0")

    if [ "${tick_count}" -eq 0 ]; then
        log "No ticks in last 5 minutes during market hours (count: ${tick_count})"
        if ! was_alerted "no_ticks"; then
            notify_all "WARNING: No ticks flowing for 5 minutes during market hours. Possible WebSocket disconnect."
            set_alerted "no_ticks"
        fi
        ISSUES=$((ISSUES + 1))
    else
        if was_alerted "no_ticks"; then
            notify_all "Tick flow resumed (${tick_count} ticks in last 5 min)"
        fi
        clear_alert "no_ticks"
    fi
}

# ---- Run All Checks ----
log "--- Watchdog check start ---"

check_docker
check_trader_process
check_containers
check_questdb
check_tick_flow

if [ "${ISSUES}" -eq 0 ]; then
    log "All checks passed"
else
    log "Checks complete: ${ISSUES} issue(s) detected"
fi
