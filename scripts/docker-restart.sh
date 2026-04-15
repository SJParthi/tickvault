#!/usr/bin/env bash
# =============================================================================
# tickvault — Docker Restart (Force)
# =============================================================================
# Full restart: down → build → up. Use from IntelliJ run configuration.
# Preserves volumes (data survives restart). Use docker-down.sh -v for full wipe.
#
# Fetches credentials from AWS SSM on every run — zero secrets on disk.
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
COMPOSE_FILE="${PROJECT_ROOT}/deploy/docker/docker-compose.yml"

# ---------------------------------------------------------------------------
# Fetch credentials from AWS SSM (zero secrets on disk)
# ---------------------------------------------------------------------------
REGION="ap-south-1"
SSM_ENV="${ENVIRONMENT:-dev}"

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

if ! command -v aws > /dev/null 2>&1; then
    echo -e "${RED}AWS CLI not found! Install: pip3 install awscli${NC}"
    exit 1
fi

if ! aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    echo -e "${RED}AWS credentials not configured! Run: aws configure --region ${REGION}${NC}"
    exit 1
fi

echo -e "${CYAN}Fetching credentials from AWS SSM...${NC}"
export TV_QUESTDB_PG_USER=$(fetch_ssm_secret "/dlt/${SSM_ENV}/questdb/pg-user")
export TV_QUESTDB_PG_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/questdb/pg-password")
export TV_GRAFANA_ADMIN_USER=$(fetch_ssm_secret "/dlt/${SSM_ENV}/grafana/admin-user")
export TV_GRAFANA_ADMIN_PASSWORD=$(fetch_ssm_secret "/dlt/${SSM_ENV}/grafana/admin-password")
export TV_TELEGRAM_BOT_TOKEN=$(fetch_ssm_secret "/dlt/${SSM_ENV}/telegram/bot-token")
export TV_TELEGRAM_CHAT_ID=$(fetch_ssm_secret "/dlt/${SSM_ENV}/telegram/chat-id")
echo -e "  ${GREEN}Done${NC}"

echo ""
echo -e "${CYAN}[1/3] Stopping all tv-* containers...${NC}"
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
    "tv-questdb"
    "tv-valkey"
    "tv-prometheus"
    "tv-grafana"
    "tv-loki"
    "tv-alloy"
    "tv-jaeger"
    "tv-traefik"
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
    docker ps --filter "name=tv-" --format "  {{.Names}}\t{{.Status}}" | sort
    exit 1
fi
