#!/usr/bin/env bash
# =============================================================================
# tickvault — Ensure Ready (Idempotent)
# =============================================================================
# Called by IntelliJ "Before launch" on every Run App / Test All.
#
# Fast path: if all 8 tv-* containers are healthy → exits in <1 second.
# Slow path: runs full setup (Docker + SSM + tables + tools) → 3-5 min first time.
#
# This replaces the need for a separate "Bootstrap" step. Clone → Run → Done.
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# ---- Telegram alert helper (best-effort) ----
# Fires a CRITICAL Telegram alert when automation cannot proceed.
# Silent no-op if notify-telegram.sh isn't installed or SSM isn't reachable —
# never let alerting failure block the main flow.
send_telegram_alert() {
    local message="$1"
    local notify_script="${SCRIPT_DIR}/notify-telegram.sh"
    if [ -x "${notify_script}" ]; then
        bash "${notify_script}" "${message}" >/dev/null 2>&1 || true
    fi
}

# ---- Docker daemon auto-start ----
# IntelliJ "Run tickvault" and the AWS EventBridge cron (8 AM IST) both call
# this script. Neither of them can tolerate a "Docker daemon is not running"
# hard failure — the whole point of automation is that it starts Docker for
# you. This function:
#   1. Checks if docker daemon is up (fast path)
#   2. If not, actively starts it (Darwin: open -a Docker, Linux: systemctl)
#   3. Polls for up to DOCKER_START_TIMEOUT seconds
#   4. Fires a Telegram alert if it still fails, then exits non-zero
DOCKER_START_TIMEOUT=120

ensure_docker_daemon_running() {
    if docker info > /dev/null 2>&1; then
        return 0
    fi

    echo -e "${CYAN}Docker daemon not running — auto-starting...${NC}"

    if [ "$(uname)" = "Darwin" ]; then
        # macOS dev: launch Docker Desktop
        if [ -d "/Applications/Docker.app" ]; then
            open -a Docker --background 2>/dev/null || true
            echo -e "  ${CYAN}Launched Docker Desktop (macOS)${NC}"
        else
            echo -e "${RED}Docker Desktop not installed at /Applications/Docker.app${NC}"
            send_telegram_alert "CRITICAL: ensure-ready.sh could not auto-start Docker — Docker Desktop is not installed at /Applications/Docker.app. Install Docker Desktop and rerun."
            return 1
        fi
    else
        # Linux / AWS EC2: systemctl (requires passwordless sudo for the user,
        # which the AWS user-data.sh.tftpl already configures via `systemctl
        # enable --now docker`. Local systemd users may need to run
        # `sudo systemctl enable docker` once.)
        if command -v systemctl > /dev/null 2>&1; then
            sudo systemctl start docker 2>/dev/null \
                || systemctl --user start docker 2>/dev/null \
                || true
            echo -e "  ${CYAN}Started docker.service (systemd)${NC}"
        else
            echo -e "${RED}No systemctl found — cannot auto-start docker daemon${NC}"
            send_telegram_alert "CRITICAL: ensure-ready.sh could not auto-start Docker — no systemctl available on $(uname -a)."
            return 1
        fi
    fi

    # Poll until ready or timeout
    local elapsed=0
    while ! docker info > /dev/null 2>&1 && [ $elapsed -lt $DOCKER_START_TIMEOUT ]; do
        sleep 2
        elapsed=$((elapsed + 2))
        printf "\r  ${CYAN}Waiting for Docker daemon... %ds / %ds${NC}" "$elapsed" "$DOCKER_START_TIMEOUT"
    done
    echo ""

    if docker info > /dev/null 2>&1; then
        echo -e "  ${GREEN}Docker daemon ready (took ${elapsed}s)${NC}"
        return 0
    else
        echo -e "${RED}Docker daemon failed to start after ${DOCKER_START_TIMEOUT}s${NC}"
        send_telegram_alert "CRITICAL: ensure-ready.sh auto-started Docker but the daemon did not become ready within ${DOCKER_START_TIMEOUT}s. Manual investigation needed. Host: $(hostname), user: ${USER:-unknown}."
        return 1
    fi
}
open_url() {
    local url="$1"
    if command -v xdg-open > /dev/null 2>&1; then
        xdg-open "$url" 2>/dev/null &
    elif command -v open > /dev/null 2>&1; then
        open "$url" 2>/dev/null &
    else
        echo -e "  ${YELLOW}Open manually:${NC} $url"
    fi
}

# ---- Auto-open all monitoring dashboards ----
open_dashboards() {
    echo -e "${CYAN}Opening monitoring dashboards...${NC}"
    # QuestDB web console — primary SQL tool
    open_url "http://localhost:9000"
    sleep 0.3
    # Grafana dashboards (anonymous access enabled — no login required)
    open_url "http://localhost:3000/d/tv-system-overview/tv-system-overview?orgId=1&refresh=5s"
    sleep 0.3
    open_url "http://localhost:3000/d/tv-market-data/market-data-explorer?orgId=1&from=now-3d&to=now&timezone=Asia%2FKolkata&refresh=5s"
    sleep 0.3
    open_url "http://localhost:3000/d/tv-trading-pipeline/trading-pipeline?orgId=1&refresh=5s"
    sleep 0.3
    # Jaeger tracing UI
    open_url "http://localhost:16686"
    echo -e "${GREEN}Opened: QuestDB, Grafana (3 dashboards), Jaeger${NC}"
}

# ---- Auto-configure ~/.pgpass for IntelliJ QuestDB database tool ----
# Reads credentials from the running tv-questdb container (set via AWS SSM).
# pgpass format: hostname:port:database:username:password
ensure_pgpass() {
    local qdb_user qdb_pass pgpass_entry
    qdb_user=$(docker exec tv-questdb printenv QDB_PG_USER 2>/dev/null) || return 0
    qdb_pass=$(docker exec tv-questdb printenv QDB_PG_PASSWORD 2>/dev/null) || return 0
    [ -z "${qdb_pass}" ] && return 0

    pgpass_entry="localhost:8812:qdb:${qdb_user}:${qdb_pass}"

    if [ -f ~/.pgpass ]; then
        # Preserve other entries, replace QuestDB line
        grep -v "^localhost:8812:qdb:" ~/.pgpass > ~/.pgpass.tmp 2>/dev/null || true
        echo "${pgpass_entry}" >> ~/.pgpass.tmp
        mv ~/.pgpass.tmp ~/.pgpass
    else
        echo "${pgpass_entry}" > ~/.pgpass
    fi
    chmod 600 ~/.pgpass
}

# ---- Fast path: check if all 8 containers are running ----
REQUIRED_CONTAINERS=(
    "tv-questdb"
    "tv-valkey"
    "tv-prometheus"
    "tv-grafana"
    "tv-loki"
    "tv-alloy"
    "tv-jaeger"
    "tv-traefik"
)

all_running() {
    for name in "${REQUIRED_CONTAINERS[@]}"; do
        if ! docker ps --format '{{.Names}}' 2>/dev/null | grep -q "^${name}$"; then
            return 1
        fi
    done
    return 0
}

# Before the fast-path check we must ensure Docker itself is reachable —
# otherwise `docker ps` (called by all_running) silently returns empty and
# we fall into the slow path. Auto-start the daemon here so both paths
# benefit from the no-human-intervention guarantee.
if ! ensure_docker_daemon_running; then
    exit 1
fi

# Quick check — if everything is already running, still ensure schema is up-to-date
if all_running; then
    ensure_pgpass
    echo -e "${GREEN}All 8 infrastructure containers running.${NC}"
    # Always run schema init (idempotent) — ensures new tables are created
    # even when containers were already running before a code update.
    echo -e "${CYAN}Ensuring QuestDB schema is up-to-date...${NC}"
    if bash "${SCRIPT_DIR}/questdb-init.sh" localhost 9000; then
        echo -e "  ${GREEN}QuestDB schema ready${NC}"
    else
        echo -e "  ${YELLOW}QuestDB schema init had warnings — app will retry at boot${NC}"
    fi
    open_dashboards
    exit 0
fi

# ---- Slow path: full setup needed ----
echo -e "${CYAN}Infrastructure not ready — running full auto-setup...${NC}"
echo ""

# 1. Ensure Docker daemon is running — auto-start if needed
if ! command -v docker > /dev/null 2>&1; then
    echo -e "${RED}Docker CLI not found!${NC}"
    echo -e "${RED}Install Docker Desktop: https://docker.com/products/docker-desktop${NC}"
    send_telegram_alert "CRITICAL: ensure-ready.sh — Docker CLI not installed on $(hostname). Automation cannot proceed."
    exit 1
fi

# Replaces the old "tell the user to start Docker" hard-fail. The automation
# plan (8 AM IST EventBridge on AWS, IntelliJ Run tickvault locally) cannot
# tolerate a human-in-the-loop Docker start step.
if ! ensure_docker_daemon_running; then
    exit 1
fi

# 2. Install Rust quality tools + git hooks (first-run only, idempotent)
echo -e "${CYAN}Checking dev tools...${NC}"

# Git hooks
if [ "$(git -C "${PROJECT_ROOT}" config core.hooksPath 2>/dev/null)" != "scripts/git-hooks" ]; then
    git -C "${PROJECT_ROOT}" config core.hooksPath scripts/git-hooks
    chmod +x "${PROJECT_ROOT}"/scripts/git-hooks/* 2>/dev/null || true
    echo -e "  ${GREEN}Git hooks configured${NC}"
fi

# Cargo tools (skip if already installed — fast check)
install_tool_if_missing() {
    local tool_name="$1"
    local cargo_name="${2:-$1}"
    if ! command -v "$tool_name" > /dev/null 2>&1; then
        echo -n "  Installing $tool_name... "
        if cargo install "$cargo_name" --quiet 2>/dev/null; then
            echo -e "${GREEN}OK${NC}"
        else
            echo -e "${YELLOW}SKIPPED${NC}"
        fi
    fi
}

install_tool_if_missing "cargo-audit" "cargo-audit"
install_tool_if_missing "cargo-deny" "cargo-deny"
install_tool_if_missing "cargo-nextest" "cargo-nextest"
install_tool_if_missing "typos" "typos-cli"

# 3. Run the full observability setup (SSM + Docker + health checks)
echo ""
echo -e "${CYAN}Starting infrastructure (Docker + AWS SSM + health checks)...${NC}"
if ! bash "${SCRIPT_DIR}/setup-observability.sh" --no-open; then
    echo ""
    echo -e "${RED}╔════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${RED}║  Observability stack setup failed. See errors above.       ║${NC}"
    echo -e "${RED}║                                                            ║${NC}"
    echo -e "${RED}║  Quick checks:                                             ║${NC}"
    echo -e "${RED}║  1. Docker running?  docker info                           ║${NC}"
    echo -e "${RED}║  2. Port conflict?   lsof -i :9000 :8812 :3000 :80        ║${NC}"
    echo -e "${RED}║  3. AWS creds?       aws sts get-caller-identity           ║${NC}"
    echo -e "${RED}║  4. Clean restart?   docker compose \\                      ║${NC}"
    echo -e "${RED}║       -f deploy/docker/docker-compose.yml down -v          ║${NC}"
    echo -e "${RED}╚════════════════════════════════════════════════════════════╝${NC}"
    exit 1
fi

# 4. Create ALL QuestDB tables + materialized views (idempotent)
#    Single source of truth: scripts/questdb-init.sh
#    Creates 15 tables + 11 DEDUP keys + 18 materialized views
echo ""
echo -e "${CYAN}Ensuring QuestDB schema (15 tables + 18 views)...${NC}"
if bash "${SCRIPT_DIR}/questdb-init.sh" localhost 9000; then
    echo -e "  ${GREEN}QuestDB schema ready${NC}"
else
    echo -e "  ${YELLOW}QuestDB schema init had errors — app will retry at boot${NC}"
fi

# 5. Verify QuestDB PG wire protocol (port 8812) is accepting connections
#    IntelliJ database tool uses this port. HTTP (9000) can be ready before PG wire (8812).
echo ""
echo -e "${CYAN}Waiting for QuestDB PG wire (port 8812)...${NC}"
PG_READY=0
for i in $(seq 1 30); do
    if bash -c 'cat < /dev/null > /dev/tcp/localhost/8812' 2>/dev/null; then
        PG_READY=1
        break
    fi
    sleep 1
done
if [ "${PG_READY}" -eq 1 ]; then
    echo -e "  ${GREEN}QuestDB PG wire ready${NC}"
    ensure_pgpass
    echo -e "  ${GREEN}QuestDB credentials auto-configured for IntelliJ${NC}"
else
    echo -e "  ${YELLOW}QuestDB PG wire not ready yet — IntelliJ may need a manual refresh${NC}"
fi

# 6. Final check
echo ""
if all_running; then
    echo -e "${GREEN}╔════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║   Infrastructure ready. All 8 services UP.     ║${NC}"
    echo -e "${GREEN}╚════════════════════════════════════════════════╝${NC}"
    open_dashboards
    exit 0
else
    echo -e "${RED}Some containers failed to start. Check Docker Desktop.${NC}"
    docker ps --filter "name=tv-" --format "  {{.Names}}\t{{.Status}}" | sort
    exit 1
fi
