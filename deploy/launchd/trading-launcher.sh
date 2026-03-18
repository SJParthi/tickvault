#!/bin/bash
# C2: Mac trading launcher with Docker Desktop recovery
#
# Called by launchd at 08:00 AM IST. Handles:
#   1. Kill switch check
#   2. DNS cache flush (post-sleep recovery)
#   3. Docker Desktop launch + health verification
#   4. Network connectivity check
#   5. Binary execution
#
# Exit codes:
#   0 = success or intentionally disabled
#   1 = fatal error (Docker/network/binary failure)

set -euo pipefail

# -------------------------------------------------------------------------
# Configuration
# -------------------------------------------------------------------------
PROJECT_DIR="${DLT_PROJECT_DIR:-/Users/Shared/dhan-live-trader}"
BINARY="${PROJECT_DIR}/target/release/dhan-live-trader"
KILL_SWITCH_FILE="${HOME}/.dhan-live-trader-disabled"
DOCKER_POLL_INTERVAL=2
DOCKER_TIMEOUT=120
LOG_PREFIX="[dlt-launcher]"

# Telegram alert function (best-effort, does not block on failure)
send_telegram_alert() {
    local message="$1"
    # Read credentials from well-known location (written by SSM fetch)
    local creds_file="${HOME}/.dlt-telegram-creds"
    if [ -f "$creds_file" ]; then
        local bot_token chat_id
        bot_token=$(grep '^BOT_TOKEN=' "$creds_file" | cut -d= -f2)
        chat_id=$(grep '^CHAT_ID=' "$creds_file" | cut -d= -f2)
        if [ -n "$bot_token" ] && [ -n "$chat_id" ]; then
            curl -s -X POST "https://api.telegram.org/bot${bot_token}/sendMessage" \
                -d "chat_id=${chat_id}" \
                -d "text=${message}" \
                -d "parse_mode=HTML" \
                --max-time 10 > /dev/null 2>&1 || true
        fi
    fi
}

log_info() { echo "$(date '+%Y-%m-%d %H:%M:%S') ${LOG_PREFIX} INFO: $1"; }
log_error() { echo "$(date '+%Y-%m-%d %H:%M:%S') ${LOG_PREFIX} ERROR: $1" >&2; }

# -------------------------------------------------------------------------
# Step 0: Deployment mode check (AWS vs Mac — mechanical, auto-disabling)
# -------------------------------------------------------------------------
# When AWS becomes the primary trading host, this file is created by
# deploy/launchd/decommission-mac.sh. It permanently disables the Mac
# from auto-starting the trading system. No human memory required.
DECOMMISSION_FILE="${HOME}/.dhan-aws-is-primary"
if [ -f "$DECOMMISSION_FILE" ]; then
    log_info "AWS is primary trading host (${DECOMMISSION_FILE} exists)"
    log_info "Mac auto-start permanently disabled. Exiting cleanly."
    send_telegram_alert "INFO: Mac launchd triggered but AWS is primary. Mac auto-start blocked."
    exit 0
fi

# Also check SSM parameter if AWS CLI is available (belt + suspenders)
# This catches the case where decommission file wasn't created locally yet
# but AWS deployment is already active.
if command -v aws &> /dev/null; then
    AWS_PRIMARY=$(aws ssm get-parameter \
        --name "/dhan-live-trader/deployment/primary-host" \
        --query "Parameter.Value" --output text 2>/dev/null || echo "mac")
    if [ "$AWS_PRIMARY" = "aws" ]; then
        log_info "SSM says AWS is primary host — blocking Mac auto-start"
        # Auto-create the local decommission file so we don't hit SSM every morning
        touch "$DECOMMISSION_FILE"
        send_telegram_alert "WARNING: Mac launchd triggered but SSM says AWS is primary. Auto-created decommission file. Mac permanently disabled."
        exit 0
    fi
fi

# -------------------------------------------------------------------------
# Step 1: Kill switch check (manual emergency disable)
# -------------------------------------------------------------------------
if [ -f "$KILL_SWITCH_FILE" ]; then
    log_info "Kill switch active (${KILL_SWITCH_FILE} exists) — exiting cleanly"
    exit 0
fi

# -------------------------------------------------------------------------
# Step 2: DNS cache flush (post-sleep recovery)
# -------------------------------------------------------------------------
log_info "Flushing DNS cache (post-sleep recovery)"
# No sudo — dscacheutil works without root. mDNSResponder restart needs root
# but is skipped to avoid password prompts during unattended launchd auto-start.
# dscacheutil -flushcache alone is sufficient for clearing stale DNS entries.
dscacheutil -flushcache 2>/dev/null || true

# -------------------------------------------------------------------------
# Step 3: Docker Desktop launch + health verification
# -------------------------------------------------------------------------
if ! [ -d "/Applications/Docker.app" ]; then
    log_error "Docker Desktop not installed at /Applications/Docker.app"
    send_telegram_alert "CRITICAL: Docker Desktop not installed on Mac trading machine"
    exit 1
fi

wait_for_docker() {
    local elapsed=0
    while [ $elapsed -lt $DOCKER_TIMEOUT ]; do
        if docker info > /dev/null 2>&1; then
            log_info "Docker daemon ready (${elapsed}s)"
            return 0
        fi
        sleep $DOCKER_POLL_INTERVAL
        elapsed=$((elapsed + DOCKER_POLL_INTERVAL))
    done
    return 1
}

if ! docker info > /dev/null 2>&1; then
    # Check if Docker app is running but daemon is unresponsive (post-sleep hang)
    if pgrep -q "Docker Desktop"; then
        log_info "Docker Desktop running but daemon unresponsive — killing and restarting"
        killall "Docker Desktop" 2>/dev/null || true
        killall "Docker" 2>/dev/null || true
        sleep 3
    fi

    log_info "Launching Docker Desktop"
    open -a Docker --background

    if ! wait_for_docker; then
        log_error "Docker daemon not ready after ${DOCKER_TIMEOUT}s — attempting recovery"
        killall "Docker Desktop" 2>/dev/null || true
        killall "Docker" 2>/dev/null || true
        sleep 5

        log_info "Retry: launching Docker Desktop"
        open -a Docker --background

        if ! wait_for_docker; then
            log_error "Docker daemon failed after recovery attempt"
            send_telegram_alert "CRITICAL: Docker Desktop failed to start on Mac after recovery. Manual intervention required."
            exit 1
        fi
    fi
else
    log_info "Docker daemon already running"
fi

# -------------------------------------------------------------------------
# Step 4: Network connectivity check
# -------------------------------------------------------------------------
log_info "Checking network connectivity"
if ! host api.dhan.co > /dev/null 2>&1; then
    log_error "Cannot resolve api.dhan.co — network may be down"
    send_telegram_alert "WARNING: Cannot resolve api.dhan.co from Mac trading machine. Network issue."
    # Continue anyway — the app has its own retry logic
fi

# -------------------------------------------------------------------------
# Step 5: Run the trading binary
# -------------------------------------------------------------------------
if ! [ -f "$BINARY" ]; then
    log_error "Binary not found at ${BINARY}"
    send_telegram_alert "CRITICAL: Trading binary not found at ${BINARY}"
    exit 1
fi

log_info "Starting dhan-live-trader from ${PROJECT_DIR}"
cd "$PROJECT_DIR"
exec "$BINARY"

# Note: exec replaces this shell — code below never runs.
# If exec fails, the shell exits with error automatically.
# Sleep at the end (for crash detection throttle) is not needed because
# exec transfers control entirely to the binary.
