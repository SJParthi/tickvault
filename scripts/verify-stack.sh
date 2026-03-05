#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Verify Observability Stack
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
echo -e "${CYAN}║   dhan-live-trader — Stack Verification        ║${NC}"
echo -e "${CYAN}╚════════════════════════════════════════════════╝${NC}"
echo ""

# --- Container Status -------------------------------------------------------
echo -e "${CYAN}[1/5]${NC} Container Status"
check_container "dlt-questdb"
check_container "dlt-valkey"
check_container "dlt-prometheus"
check_container "dlt-loki"
check_container "dlt-alloy"
check_container "dlt-jaeger"
check_container "dlt-grafana"
check_container "dlt-traefik"
echo ""

# --- HTTP Health Checks ------------------------------------------------------
echo -e "${CYAN}[2/5]${NC} HTTP Health Checks"
check_http "QuestDB" "http://localhost:9000"
check_http "Prometheus" "http://localhost:9090/-/healthy"
check_http "Grafana" "http://localhost:3000/api/health"
check_http "Jaeger UI" "http://localhost:16686"
check_http "Traefik Dashboard" "http://localhost:8080/api/rawdata"
check_http "Traefik Metrics" "http://localhost:8082/metrics"
check_http "Alloy UI" "http://localhost:12345" "false"
check_http "Loki Ready" "http://localhost:3100/ready" "false"
echo ""

# --- TCP Port Checks ---------------------------------------------------------
echo -e "${CYAN}[3/5]${NC} TCP Port Checks"
check_tcp "Valkey (6379)" "localhost" "6379"
check_tcp "QuestDB PG (8812)" "localhost" "8812"
check_tcp "QuestDB ILP (9009)" "localhost" "9009"
check_tcp "OTLP gRPC (4317)" "localhost" "4317"
check_tcp "OTLP HTTP (4318)" "localhost" "4318"
echo ""

# --- Prometheus Targets -------------------------------------------------------
echo -e "${CYAN}[4/5]${NC} Prometheus Scrape Targets"
TARGETS_JSON=$(curl -sf --max-time 3 "http://localhost:9090/api/v1/targets" 2>/dev/null || echo "")
if [ -n "$TARGETS_JSON" ]; then
    # Count active vs down targets
    ACTIVE=$(echo "$TARGETS_JSON" | grep -o '"health":"up"' | wc -l)
    DOWN=$(echo "$TARGETS_JSON" | grep -o '"health":"down"' | wc -l)
    UNKNOWN=$(echo "$TARGETS_JSON" | grep -o '"health":"unknown"' | wc -l)
    echo -e "  Active targets:  ${GREEN}${ACTIVE}${NC}"
    if [ "$DOWN" -gt 0 ]; then
        echo -e "  Down targets:    ${RED}${DOWN}${NC}"
        # Show which jobs are down
        echo "$TARGETS_JSON" | grep -o '"job":"[^"]*".*"health":"down"' | grep -o '"job":"[^"]*"' | sort -u | while read -r line; do
            echo -e "    ${RED}DOWN:${NC} $line"
        done
    fi
    if [ "$UNKNOWN" -gt 0 ]; then
        echo -e "  Unknown targets: ${YELLOW}${UNKNOWN}${NC} (still initializing)"
    fi
else
    echo -e "  ${YELLOW}Could not query Prometheus targets API${NC}"
fi
echo ""

# --- Grafana Provisioning -----------------------------------------------------
echo -e "${CYAN}[5/5]${NC} Grafana Provisioning"

# Check datasources (anonymous access enabled — no auth header needed)
DS_JSON=$(curl -sf --max-time 3 "http://localhost:3000/api/datasources" 2>/dev/null || echo "")
if [ -n "$DS_JSON" ] && [ "$DS_JSON" != "[]" ]; then
    DS_COUNT=$(echo "$DS_JSON" | grep -o '"name"' | wc -l)
    echo -e "  Datasources:     ${GREEN}${DS_COUNT} configured${NC}"
    # Check QuestDB datasource specifically
    if echo "$DS_JSON" | grep -q '"name":"QuestDB"'; then
        echo -e "  QuestDB DS:      ${GREEN}provisioned (query via Grafana Explore)${NC}"
    else
        echo -e "  QuestDB DS:      ${YELLOW}not found — restart Grafana to provision${NC}"
    fi
else
    echo -e "  Datasources:     ${YELLOW}could not verify (Grafana may still be starting)${NC}"
fi

# Check dashboards (anonymous access enabled — no auth header needed)
DASH_SEARCH=$(curl -sf --max-time 3 "http://localhost:3000/api/search?type=dash-db" 2>/dev/null || echo "")
if [ -n "$DASH_SEARCH" ] && [ "$DASH_SEARCH" != "[]" ]; then
    DASH_COUNT=$(echo "$DASH_SEARCH" | grep -o '"uid"' | wc -l)
    echo -e "  Dashboards:      ${GREEN}${DASH_COUNT} provisioned${NC}"
else
    echo -e "  Dashboards:      ${YELLOW}could not verify (may need first scrape)${NC}"
fi

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
echo -e "  ${CYAN}Grafana:${NC}    http://localhost:3000"
echo -e "  ${CYAN}Prometheus:${NC} http://localhost:9090"
echo -e "  ${CYAN}Jaeger:${NC}     http://localhost:16686"
echo -e "  ${CYAN}Traefik:${NC}    http://localhost:8080"
echo -e "  ${CYAN}QuestDB:${NC}    http://localhost:9000"
echo -e "  ${CYAN}Loki:${NC}       http://localhost:3100"
echo -e "  ${CYAN}Alloy:${NC}      http://localhost:12345"
echo ""
exit 0
