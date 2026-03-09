#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Docker Restart (Force)
# =============================================================================
# Full restart: down → build → up. Use from IntelliJ run configuration.
# Preserves volumes (data survives restart). Use docker-down.sh -v for full wipe.
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
COMPOSE_FILE="${PROJECT_ROOT}/deploy/docker/docker-compose.yml"

# Source SSM env vars (bootstrap must have run at least once)
if [ -f "${PROJECT_ROOT}/data/.env.ssm" ]; then
    set -a
    # shellcheck disable=SC1091
    source "${PROJECT_ROOT}/data/.env.ssm"
    set +a
else
    # Fallback: try bootstrap.sh if env file doesn't exist
    echo -e "${RED}data/.env.ssm not found — run scripts/bootstrap.sh first${NC}"
    exit 1
fi

echo -e "${CYAN}[1/3] Stopping all dlt-* containers...${NC}"
docker compose -f "${COMPOSE_FILE}" down --remove-orphans
echo -e "${GREEN}  Done${NC}"

echo ""
echo -e "${CYAN}[2/3] Rebuilding images (if any custom builds)...${NC}"
docker compose -f "${COMPOSE_FILE}" build 2>/dev/null || echo -e "  ${GREEN}No custom builds — using pinned images${NC}"

echo ""
echo -e "${CYAN}[3/3] Starting all services...${NC}"
docker compose -f "${COMPOSE_FILE}" up -d

# Wait for health checks
echo ""
echo -e "${CYAN}Waiting for containers to become healthy...${NC}"
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

MAX_WAIT=90
ELAPSED=0
while [ $ELAPSED -lt $MAX_WAIT ]; do
    ALL_UP=true
    for name in "${REQUIRED_CONTAINERS[@]}"; do
        if ! docker ps --format '{{.Names}}' 2>/dev/null | grep -q "^${name}$"; then
            ALL_UP=false
            break
        fi
    done
    if $ALL_UP; then
        break
    fi
    sleep 2
    ELAPSED=$((ELAPSED + 2))
done

echo ""
if $ALL_UP; then
    echo -e "${GREEN}╔════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║   Docker restart complete. All 8 services UP.  ║${NC}"
    echo -e "${GREEN}╚════════════════════════════════════════════════╝${NC}"
else
    echo -e "${RED}Some containers not ready after ${MAX_WAIT}s:${NC}"
    docker ps --filter "name=dlt-" --format "  {{.Names}}\t{{.Status}}" | sort
    exit 1
fi
