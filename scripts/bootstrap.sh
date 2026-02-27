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
#   4. Starts Docker infrastructure (9 services)
#   5. Waits for services to be healthy
#   6. Seeds LocalStack with dev secrets
#   7. Verifies everything works
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
echo -e "${CYAN}[1/7]${NC} Installing Rust toolchain..."
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
echo -e "${CYAN}[2/7]${NC} Installing quality gate tools..."

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
install_tool "cargo-tarpaulin" "cargo-tarpaulin"
install_tool "cargo-machete" "cargo-machete"
install_tool "typos" "typos-cli"
echo ""

# ---- Step 3: Git Hooks ----
echo -e "${CYAN}[3/7]${NC} Setting up git hooks..."
git config core.hooksPath scripts/git-hooks
chmod +x scripts/git-hooks/*
echo -e "  ${GREEN}Git hooks configured${NC} (pre-commit, pre-push, commit-msg)"
echo ""

# ---- Step 4: Docker Infrastructure ----
echo -e "${CYAN}[4/7]${NC} Starting Docker infrastructure..."
if command -v docker > /dev/null 2>&1; then
    docker compose -f deploy/docker/docker-compose.yml up -d
    echo -e "  ${GREEN}Docker services starting${NC}"
else
    echo -e "  ${YELLOW}Docker not found — skipping infrastructure${NC}"
    echo "  Install Docker Desktop: https://docker.com/products/docker-desktop"
fi
echo ""

# ---- Step 5: Wait for Health ----
echo -e "${CYAN}[5/7]${NC} Waiting for services to be healthy..."

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

if command -v docker > /dev/null 2>&1; then
    wait_for_service "QuestDB" "http://localhost:9000"
    wait_for_service "Prometheus" "http://localhost:9090/-/healthy"
    wait_for_service "Grafana" "http://localhost:3000/api/health"
    wait_for_service "LocalStack" "http://localhost:4566/_localstack/health"
fi
echo ""

# ---- Step 6: Seed LocalStack Secrets ----
echo -e "${CYAN}[6/7]${NC} Seeding LocalStack with dev secrets..."
if [ -f "scripts/seed-localstack-secrets.sh" ] && command -v docker > /dev/null 2>&1; then
    if bash scripts/seed-localstack-secrets.sh 2>/dev/null; then
        echo -e "  ${GREEN}Dev secrets seeded${NC}"
    else
        echo -e "  ${YELLOW}Seeding skipped${NC} (LocalStack may not be ready yet)"
    fi
else
    echo -e "  ${YELLOW}Skipped${NC} (script not found or Docker not available)"
fi
echo ""

# ---- Step 7: Verify ----
echo -e "${CYAN}[7/7]${NC} Verifying build..."
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

echo ""
echo -e "${CYAN}╔════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   Bootstrap complete!                          ║${NC}"
echo -e "${CYAN}╠════════════════════════════════════════════════╣${NC}"
echo -e "${CYAN}║                                                ║${NC}"
echo -e "${CYAN}║   Open IntelliJ IDEA and start working.       ║${NC}"
echo -e "${CYAN}║                                                ║${NC}"
echo -e "${CYAN}║   Useful commands:                             ║${NC}"
echo -e "${CYAN}║     cargo r    — run the app                  ║${NC}"
echo -e "${CYAN}║     cargo t    — run all tests                ║${NC}"
echo -e "${CYAN}║     cargo c    — run clippy                   ║${NC}"
echo -e "${CYAN}║     cargo b    — build release                ║${NC}"
echo -e "${CYAN}║     make help  — see all Make targets          ║${NC}"
echo -e "${CYAN}║                                                ║${NC}"
echo -e "${CYAN}║   Monitoring URLs:                             ║${NC}"
echo -e "${CYAN}║     Grafana:    http://localhost:3000          ║${NC}"
echo -e "${CYAN}║     QuestDB:    http://localhost:9000          ║${NC}"
echo -e "${CYAN}║     Prometheus: http://localhost:9090          ║${NC}"
echo -e "${CYAN}║     Jaeger:     http://localhost:16686         ║${NC}"
echo -e "${CYAN}║                                                ║${NC}"
echo -e "${CYAN}╚════════════════════════════════════════════════╝${NC}"
echo ""
