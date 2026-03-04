#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Ensure Ready (Idempotent)
# =============================================================================
# Called by IntelliJ "Before launch" on every Run App / Test All.
#
# Fast path: if all 8 dlt-* containers are healthy → exits in <1 second.
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

# ---- Auto-configure ~/.pgpass for IntelliJ QuestDB database tool ----
# Reads credentials from the running dlt-questdb container (set via AWS SSM).
# pgpass format: hostname:port:database:username:password
ensure_pgpass() {
    local qdb_user qdb_pass pgpass_entry
    qdb_user=$(docker exec dlt-questdb printenv QDB_PG_USER 2>/dev/null) || return 0
    qdb_pass=$(docker exec dlt-questdb printenv QDB_PG_PASSWORD 2>/dev/null) || return 0
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
    "dlt-questdb"
    "dlt-valkey"
    "dlt-prometheus"
    "dlt-grafana"
    "dlt-loki"
    "dlt-alloy"
    "dlt-jaeger"
    "dlt-traefik"
)

all_running() {
    for name in "${REQUIRED_CONTAINERS[@]}"; do
        if ! docker ps --format '{{.Names}}' 2>/dev/null | grep -q "^${name}$"; then
            return 1
        fi
    done
    return 0
}

# Quick check — if everything is already running, exit immediately
if all_running; then
    ensure_pgpass
    echo -e "${GREEN}All 8 infrastructure containers running. Ready.${NC}"
    exit 0
fi

# ---- Slow path: full setup needed ----
echo -e "${CYAN}Infrastructure not ready — running full auto-setup...${NC}"
echo ""

# 1. Check Docker is running
if ! command -v docker > /dev/null 2>&1; then
    echo -e "${RED}Docker CLI not found!${NC}"
    echo -e "${RED}Install Docker Desktop: https://docker.com/products/docker-desktop${NC}"
    exit 1
fi

if ! docker info > /dev/null 2>&1; then
    echo -e "${RED}Docker daemon is not running!${NC}"
    echo -e "${RED}Start Docker Desktop first, then re-run.${NC}"
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

# 4. Create QuestDB tables (idempotent)
echo ""
echo -e "${CYAN}Ensuring QuestDB tables exist...${NC}"
QUESTDB_EXEC_URL="http://localhost:9000/exec"

init_table() {
    local description="$1"
    local ddl="$2"
    echo -n "  $description... "
    if curl -sf --max-time 5 -G "${QUESTDB_EXEC_URL}" --data-urlencode "query=${ddl}" > /dev/null 2>&1; then
        echo -e "${GREEN}OK${NC}"
    else
        echo -e "${YELLOW}SKIPPED${NC} (app creates tables at runtime)"
    fi
}

init_table "ticks" "CREATE TABLE IF NOT EXISTS ticks (segment SYMBOL, security_id LONG, ltp DOUBLE, open DOUBLE, high DOUBLE, low DOUBLE, close DOUBLE, volume LONG, oi LONG, avg_price DOUBLE, last_trade_qty LONG, total_buy_qty LONG, total_sell_qty LONG, received_at TIMESTAMP, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY HOUR WAL"

init_table "instrument_build_metadata" "CREATE TABLE IF NOT EXISTS instrument_build_metadata (csv_source SYMBOL, csv_row_count LONG, parsed_row_count LONG, index_count LONG, equity_count LONG, underlying_count LONG, derivative_count LONG, option_chain_count LONG, build_duration_ms LONG, build_timestamp TIMESTAMP, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"

init_table "fno_underlyings" "CREATE TABLE IF NOT EXISTS fno_underlyings (underlying_symbol SYMBOL, price_feed_segment SYMBOL, derivative_segment SYMBOL, kind SYMBOL, underlying_security_id LONG, price_feed_security_id LONG, lot_size LONG, contract_count LONG, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"

init_table "derivative_contracts" "CREATE TABLE IF NOT EXISTS derivative_contracts (underlying_symbol SYMBOL, instrument_kind SYMBOL, exchange_segment SYMBOL, option_type SYMBOL, symbol_name SYMBOL, security_id LONG, expiry_date STRING, strike_price DOUBLE, lot_size LONG, tick_size DOUBLE, display_name STRING, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"

init_table "subscribed_indices" "CREATE TABLE IF NOT EXISTS subscribed_indices (symbol SYMBOL, exchange SYMBOL, category SYMBOL, subcategory SYMBOL, security_id LONG, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"

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
    exit 0
else
    echo -e "${RED}Some containers failed to start. Check Docker Desktop.${NC}"
    docker ps --filter "name=dlt-" --format "  {{.Names}}\t{{.Status}}" | sort
    exit 1
fi
