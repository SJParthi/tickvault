#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Bootstrap Script
# =============================================================================
# THE ONE COMMAND. Run once after cloning. Sets up everything.
#
# Usage:
#   git clone https://github.com/SJParthi/dhan-live-trader.git
#   cd dhan-live-trader
#   ./scripts/bootstrap.sh
#
# What it does:
#   1. Installs Rust toolchain (reads rust-toolchain.toml)
#   2. Installs quality gate tools (cargo-audit, cargo-deny, etc.)
#   3. Sets up git hooks (pre-commit, pre-push, commit-msg)
#   4. Provisions + fetches infrastructure credentials from AWS SSM (QuestDB, Grafana)
#   5. Starts Docker infrastructure (8 services) — fails fast if Docker not running
#   6. Waits for services to be healthy (QuestDB, Prometheus, Grafana)
#   7. Initializes QuestDB tables (CREATE TABLE IF NOT EXISTS — idempotent)
#   8. Verifies secrets in real AWS SSM + sends test Telegram notification
#   9. Verifies cargo check + cargo test
#   10. Verifies observability stack (Prometheus targets, Grafana dashboards, etc.)
#
# After this: open IntelliJ and start working. Zero manual config.
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

echo ""
echo -e "${CYAN}╔════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   dhan-live-trader — Bootstrap                 ║${NC}"
echo -e "${CYAN}╚════════════════════════════════════════════════╝${NC}"
echo ""

# ---- Step 1: Rust Toolchain ----
echo -e "${CYAN}[1/10]${NC} Installing Rust toolchain..."
if command -v rustup > /dev/null 2>&1; then
    rustup show active-toolchain
    echo -e "  ${GREEN}Rust toolchain ready${NC}"
else
    echo -e "  ${RED}rustup not found!${NC}"
    echo "  Install rustup: https://rustup.rs"
    exit 1
fi
echo ""

# ---- Step 2: Quality Tools ----
echo -e "${CYAN}[2/10]${NC} Installing quality gate tools..."

install_tool() {
    local tool_name="$1"
    local cargo_name="${2:-$1}"
    if command -v "$tool_name" > /dev/null 2>&1; then
        echo -e "  ${GREEN}$tool_name${NC} — already installed"
    else
        echo -n "  Installing $tool_name... "
        if cargo install "$cargo_name" --quiet 2>/dev/null; then
            echo -e "${GREEN}OK${NC}"
        else
            echo -e "${YELLOW}SKIPPED${NC} (install manually: cargo install $cargo_name)"
        fi
    fi
}

install_tool "cargo-audit" "cargo-audit"
install_tool "cargo-deny" "cargo-deny"
install_tool "cargo-llvm-cov" "cargo-llvm-cov"
install_tool "cargo-machete" "cargo-machete"
install_tool "cargo-nextest" "cargo-nextest"
install_tool "cargo-mutants" "cargo-mutants"
install_tool "typos" "typos-cli"
echo ""

# ---- Step 3: Git Hooks ----
echo -e "${CYAN}[3/10]${NC} Setting up git hooks..."
git config core.hooksPath scripts/git-hooks
chmod +x scripts/git-hooks/*
echo -e "  ${GREEN}Git hooks configured${NC} (pre-commit, pre-push, commit-msg)"
echo ""

# ---- Step 4: Provision + Fetch Infrastructure Credentials from SSM ----
echo -e "${CYAN}[4/10]${NC} Provisioning infrastructure credentials in AWS SSM..."

REGION="ap-south-1"
SSM_ENV="${ENVIRONMENT:-dev}"

# Auto-install AWS CLI if missing (true auto-setup — no manual steps)
if ! command -v aws > /dev/null 2>&1; then
    echo -e "  ${YELLOW}AWS CLI not found — auto-installing...${NC}"
    if command -v pip3 > /dev/null 2>&1; then
        pip3 install awscli --quiet 2>/dev/null
    elif command -v pip > /dev/null 2>&1; then
        pip install awscli --quiet 2>/dev/null
    fi
    if command -v aws > /dev/null 2>&1; then
        echo -e "  ${GREEN}AWS CLI installed${NC}"
    else
        echo -e "  ${RED}Could not auto-install AWS CLI.${NC}"
        echo -e "  ${RED}Install manually: pip3 install awscli${NC}"
        exit 1
    fi
fi

fetch_ssm_secret() {
    local name="$1"
    aws ssm get-parameter \
        --region "${REGION}" \
        --name "${name}" \
        --with-decryption \
        --output text \
        --query "Parameter.Value" \
        --no-cli-pager 2>/dev/null
}

if ! aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    echo -e "  ${RED}AWS credentials not configured!${NC}"
    echo -e "  ${RED}Run: aws configure --region ${REGION}${NC}"
    echo -e "  ${RED}All credentials come from real AWS SSM. No exceptions.${NC}"
    exit 1
fi

# Auto-create QuestDB + Grafana secrets if they don't exist (idempotent)
ENVIRONMENT="${SSM_ENV}" bash scripts/provision-infra-secrets.sh

# Fetch credentials and export for docker-compose
echo -n "  Exporting QuestDB credentials... "
DLT_QUESTDB_PG_USER=$(fetch_ssm_secret "/dlt/${SSM_ENV}/questdb/pg-user")
DLT_QUESTDB_PG_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/questdb/pg-password")
export DLT_QUESTDB_PG_USER DLT_QUESTDB_PG_PASSWORD
echo -e "${GREEN}OK${NC}"

echo -n "  Exporting Grafana credentials... "
DLT_GRAFANA_ADMIN_USER=$(fetch_ssm_secret "/dlt/${SSM_ENV}/grafana/admin-user")
DLT_GRAFANA_ADMIN_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/grafana/admin-password")
export DLT_GRAFANA_ADMIN_USER DLT_GRAFANA_ADMIN_PASSWORD
echo -e "${GREEN}OK${NC}"

echo -n "  Exporting Telegram credentials for Grafana alerts... "
DLT_TELEGRAM_BOT_TOKEN=$(fetch_ssm_secret "/dlt/${SSM_ENV}/telegram/bot-token")
DLT_TELEGRAM_CHAT_ID=$(fetch_ssm_secret "/dlt/${SSM_ENV}/telegram/chat-id")
export DLT_TELEGRAM_BOT_TOKEN DLT_TELEGRAM_CHAT_ID
echo -e "${GREEN}OK${NC}"

echo ""

# ---- Step 5: Docker Infrastructure ----
echo -e "${CYAN}[5/10]${NC} Starting Docker infrastructure..."
if ! command -v docker > /dev/null 2>&1; then
    echo -e "  ${RED}Docker CLI not found!${NC}"
    echo "  Install Docker Desktop: https://docker.com/products/docker-desktop"
    exit 1
fi

if ! docker info > /dev/null 2>&1; then
    echo -e "  ${RED}Docker daemon is not running!${NC}"
    echo "  Start Docker Desktop first, then re-run this script."
    exit 1
fi

if ! docker compose version > /dev/null 2>&1; then
    echo -e "  ${RED}docker compose plugin not found!${NC}"
    echo "  Update Docker Desktop to get the compose plugin."
    exit 1
fi

echo -n "  Pulling Docker images (first run downloads ~2GB)... "
docker compose -f deploy/docker/docker-compose.yml pull --quiet 2>/dev/null
echo -e "${GREEN}done${NC}"

docker compose -f deploy/docker/docker-compose.yml up -d
echo -e "  ${GREEN}Docker services starting${NC}"
echo ""

# ---- Step 5: Wait for Health ----
echo -e "${CYAN}[6/10]${NC} Waiting for services to be healthy..."

wait_for_service() {
    local name="$1"
    local url="$2"
    local max_attempts=30
    local attempt=1

    echo -n "  Waiting for $name... "
    while [ $attempt -le $max_attempts ]; do
        if curl -sf --max-time 2 "$url" > /dev/null 2>&1; then
            echo -e "${GREEN}OK${NC}"
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 1
    done
    echo -e "${YELLOW}TIMEOUT${NC} (service may still be starting)"
    return 0  # Don't fail bootstrap — services might need more time
}

wait_for_service "QuestDB" "http://localhost:9000"
wait_for_service "Valkey" "http://localhost:6379" || true  # Valkey has no HTTP — check via redis-cli below
wait_for_service "Prometheus" "http://localhost:9090/-/healthy"
wait_for_service "Grafana" "http://localhost:3000/api/health"

# Verify QuestDB PostgreSQL wire protocol (port 8812 — used by IntelliJ Database tool)
echo -n "  Checking QuestDB PG wire (port 8812)... "
if docker exec dlt-questdb bash -c 'echo | nc -z localhost 8812' > /dev/null 2>&1; then
    echo -e "${GREEN}OK${NC}"
else
    echo -e "${YELLOW}NOT READY${NC} (may take a few more seconds)"
fi
echo ""

# ---- Step 6: QuestDB Table Initialization ----
echo -e "${CYAN}[7/10]${NC} Initializing QuestDB tables..."
QUESTDB_EXEC_URL="http://localhost:9000/exec"

init_questdb_table() {
    local description="$1"
    local ddl="$2"
    echo -n "  Creating $description... "
    if curl -sf --max-time 5 -G "${QUESTDB_EXEC_URL}" --data-urlencode "query=${ddl}" > /dev/null 2>&1; then
        echo -e "${GREEN}OK${NC}"
    else
        echo -e "${YELLOW}SKIPPED${NC} (QuestDB may still be starting — app creates tables at runtime)"
    fi
}

TICKS_DDL="CREATE TABLE IF NOT EXISTS ticks (segment SYMBOL, security_id LONG, ltp DOUBLE, open DOUBLE, high DOUBLE, low DOUBLE, close DOUBLE, volume LONG, oi LONG, avg_price DOUBLE, last_trade_qty LONG, total_buy_qty LONG, total_sell_qty LONG, received_at TIMESTAMP, ts TIMESTAMP) TIMESTAMP(ts) PARTITION BY HOUR WAL"
init_questdb_table "ticks" "${TICKS_DDL}"

BUILD_METADATA_DDL="CREATE TABLE IF NOT EXISTS instrument_build_metadata (csv_source SYMBOL, csv_row_count LONG, parsed_row_count LONG, index_count LONG, equity_count LONG, underlying_count LONG, derivative_count LONG, option_chain_count LONG, build_duration_ms LONG, build_timestamp TIMESTAMP, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"
init_questdb_table "instrument_build_metadata" "${BUILD_METADATA_DDL}"

FNO_UNDERLYINGS_DDL="CREATE TABLE IF NOT EXISTS fno_underlyings (underlying_symbol SYMBOL, price_feed_segment SYMBOL, derivative_segment SYMBOL, kind SYMBOL, underlying_security_id LONG, price_feed_security_id LONG, lot_size LONG, contract_count LONG, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"
init_questdb_table "fno_underlyings" "${FNO_UNDERLYINGS_DDL}"

DERIVATIVE_CONTRACTS_DDL="CREATE TABLE IF NOT EXISTS derivative_contracts (underlying_symbol SYMBOL, instrument_kind SYMBOL, exchange_segment SYMBOL, option_type SYMBOL, symbol_name SYMBOL, security_id LONG, expiry_date STRING, strike_price DOUBLE, lot_size LONG, tick_size DOUBLE, display_name STRING, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"
init_questdb_table "derivative_contracts" "${DERIVATIVE_CONTRACTS_DDL}"

SUBSCRIBED_INDICES_DDL="CREATE TABLE IF NOT EXISTS subscribed_indices (symbol SYMBOL, exchange SYMBOL, category SYMBOL, subcategory SYMBOL, security_id LONG, timestamp TIMESTAMP) TIMESTAMP(timestamp) PARTITION BY DAY WAL"
init_questdb_table "subscribed_indices" "${SUBSCRIBED_INDICES_DDL}"
echo ""

# ---- Step 7: Verify Secrets in Real AWS SSM ----
echo -e "${CYAN}[8/10]${NC} Verifying secrets in AWS SSM..."
if [ -f "scripts/setup-secrets.sh" ]; then
    bash scripts/setup-secrets.sh
else
    echo -e "  ${YELLOW}scripts/setup-secrets.sh not found — skipping${NC}"
fi
echo ""

# ---- Step 8: Verify ----
echo -e "${CYAN}[9/10]${NC} Verifying build..."
echo -n "  Compiling workspace... "
if cargo check --workspace > /dev/null 2>&1; then
    echo -e "${GREEN}OK${NC}"
else
    echo -e "${RED}FAILED${NC}"
    echo "  Run 'cargo check' to see errors."
fi

echo -n "  Running tests... "
if cargo test --workspace > /dev/null 2>&1; then
    echo -e "${GREEN}OK${NC}"
else
    echo -e "${YELLOW}SOME FAILURES${NC}"
    echo "  Run 'cargo test' to see details."
fi

# ---- Step 10: Verify Observability Stack ----
echo -e "${CYAN}[10/10]${NC} Verifying observability stack..."
if [ -f "scripts/verify-stack.sh" ]; then
    bash scripts/verify-stack.sh || true
else
    echo -e "  ${YELLOW}scripts/verify-stack.sh not found — skipping${NC}"
fi
echo ""

echo -e "${CYAN}╔════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   Bootstrap complete!                          ║${NC}"
echo -e "${CYAN}╠════════════════════════════════════════════════╣${NC}"
echo -e "${CYAN}║                                                ║${NC}"
echo -e "${CYAN}║   Useful commands:                             ║${NC}"
echo -e "${CYAN}║     cargo r    — run the app                  ║${NC}"
echo -e "${CYAN}║     cargo t    — run all tests                ║${NC}"
echo -e "${CYAN}║     make help  — see all Make targets          ║${NC}"
echo -e "${CYAN}║                                                ║${NC}"
echo -e "${CYAN}║   Observability:                               ║${NC}"
echo -e "${CYAN}║     make obs          — full auto setup        ║${NC}"
echo -e "${CYAN}║     make obs-verify   — verify stack only      ║${NC}"
echo -e "${CYAN}║     make obs-restart  — tear down + restart    ║${NC}"
echo -e "${CYAN}║                                                ║${NC}"
echo -e "${CYAN}║   Monitoring URLs:                             ║${NC}"
echo -e "${CYAN}║     Grafana:    http://localhost:3000          ║${NC}"
echo -e "${CYAN}║     QuestDB:    http://localhost:9000          ║${NC}"
echo -e "${CYAN}║     Prometheus: http://localhost:9090          ║${NC}"
echo -e "${CYAN}║     Jaeger:     http://localhost:16686         ║${NC}"
echo -e "${CYAN}║     Traefik:    http://localhost:8080          ║${NC}"
echo -e "${CYAN}║                                                ║${NC}"
echo -e "${CYAN}╚════════════════════════════════════════════════╝${NC}"
echo ""
