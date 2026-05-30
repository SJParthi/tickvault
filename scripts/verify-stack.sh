#!/usr/bin/env bash
# =============================================================================
# tickvault — Verify Observability Stack
# =============================================================================
# Validates that all infrastructure services are running and healthy.
# Run after bootstrap.sh or docker compose up.
#
# Usage:  ./scripts/verify-stack.sh
# Exit:   0 if all checks pass, 1 if any critical service is down
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

PASS=0
WARN=0
FAIL=0

check_http() {
    local name="$1"
    local url="$2"
    local critical="${3:-true}"

    printf "  %-20s " "$name"
    if curl -sf --max-time 3 "$url" > /dev/null 2>&1; then
        echo -e "${GREEN}OK${NC}"
        PASS=$((PASS + 1))
    elif [ "$critical" = "true" ]; then
        echo -e "${RED}FAIL${NC}"
        FAIL=$((FAIL + 1))
    else
        echo -e "${YELLOW}WARN${NC} (non-critical)"
        WARN=$((WARN + 1))
    fi
}

check_tcp() {
    local name="$1"
    local host="$2"
    local port="$3"

    printf "  %-20s " "$name"
    if timeout 3 bash -c "cat < /dev/null > /dev/tcp/$host/$port" 2>/dev/null; then
        echo -e "${GREEN}OK${NC}"
        PASS=$((PASS + 1))
    else
        echo -e "${RED}FAIL${NC}"
        FAIL=$((FAIL + 1))
    fi
}

check_container() {
    local name="$1"

    printf "  %-20s " "$name"
    local status
    status=$(docker inspect --format='{{.State.Status}}' "$name" 2>/dev/null || echo "missing")
    if [ "$status" = "running" ]; then
        echo -e "${GREEN}running${NC}"
        PASS=$((PASS + 1))
    else
        echo -e "${RED}$status${NC}"
        FAIL=$((FAIL + 1))
    fi
}

echo ""
echo -e "${CYAN}╔════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   tickvault — Stack Verification        ║${NC}"
echo -e "${CYAN}╚════════════════════════════════════════════════╝${NC}"
echo ""

# --- Container Status -------------------------------------------------------
echo -e "${CYAN}[1/3]${NC} Container Status"
check_container "tv-questdb"
# Wave 7-A removed: tv-traefik / tv-loki / tv-alloy / tv-jaeger / tv-valkey-exporter.
# CloudWatch-only migration removed: tv-grafana (#O1), tv-alertmanager (#O2),
# tv-prometheus (#O3), tv-valkey (#O4 — fully removed; runtime is QuestDB + app
# only). See .claude/rules/project/aws-budget.md. The Prometheus/Grafana/
# Alertmanager HTTP probes + scrape-target + datasource checks were removed
# in #O5 (2026-05-30); CloudWatch is the sole observability layer in prod.
echo ""

# --- HTTP Health Checks ------------------------------------------------------
echo -e "${CYAN}[2/3]${NC} HTTP Health Checks"
check_http "QuestDB" "http://localhost:9000"
check_http "Alloy UI" "http://localhost:12345" "false"
check_http "Loki Ready" "http://localhost:3100/ready" "false"
echo ""

# --- TCP Port Checks ---------------------------------------------------------
echo -e "${CYAN}[3/3]${NC} TCP Port Checks"
check_tcp "QuestDB PG (8812)" "localhost" "8812"
check_tcp "QuestDB ILP (9009)" "localhost" "9009"
check_tcp "OTLP gRPC (4317)" "localhost" "4317"
check_tcp "OTLP HTTP (4318)" "localhost" "4318"
echo ""

# --- Summary ------------------------------------------------------------------
echo -e "${CYAN}════════════════════════════════════════════════${NC}"
echo -e "  Results: ${GREEN}${PASS} passed${NC}  ${YELLOW}${WARN} warnings${NC}  ${RED}${FAIL} failed${NC}"
echo -e "${CYAN}════════════════════════════════════════════════${NC}"
echo ""

if [ "$FAIL" -gt 0 ]; then
    echo -e "${RED}Some critical checks failed. Run 'docker compose -f deploy/docker/docker-compose.yml logs' to debug.${NC}"
    echo ""
    exit 1
fi

echo -e "${GREEN}All critical checks passed.${NC}"
echo ""
# Grafana/Prometheus URL hints removed in #O5 (2026-05-30) — those
# containers were retired (#O1/#O3); operator dashboards live in AWS
# CloudWatch in prod.
echo -e "  ${CYAN}QuestDB:${NC}    http://localhost:9000"
echo -e "  ${CYAN}Loki:${NC}       http://localhost:3100"
echo -e "  ${CYAN}Alloy:${NC}      http://localhost:12345"
echo ""
exit 0
