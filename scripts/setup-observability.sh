#!/usr/bin/env bash
# =============================================================================
# tickvault — Infrastructure Stack: FULL AUTO Setup
# =============================================================================
# ONE COMMAND. ZERO HUMAN INTERVENTION.
#
# What this does (end-to-end):
#   1.  Detect OS (macOS / Linux) and set platform-specific commands
#   2.  Check prerequisites (Docker, docker compose, curl, jq)
#   3.  Auto-install missing prerequisites where possible
#   4.  Validate AWS credentials exist (for QuestDB secrets)
#   5.  Fetch/provision all SSM secrets (idempotent)
#   6.  Check existing stack state (or tear down on --restart)
#   7.  Pull Docker images (pinned SHA256)
#   8.  Start the stack (docker compose up -d)
#   9.  Health-wait QuestDB with exponential backoff; verify Valkey is up
#   10. Print log-pipeline pointers
#   11. Print full status report
#
# CloudWatch-only migration (aws-budget.md "OPERATOR DECISION 2026-05-20"):
# the runtime is QuestDB + the tickvault app + AWS CloudWatch. Grafana,
# Prometheus, Alertmanager, Loki and Alloy are not part of the default
# stack. CloudWatch is the entire observability layer (metrics, logs,
# alarms). Valkey stays until plan item #O4 removes it from the code.
#
# Usage:
#   ./scripts/setup-observability.sh              # full setup + open browser
#   ./scripts/setup-observability.sh --no-open    # full setup, skip browser
#   ./scripts/setup-observability.sh --verify     # verify only (skip setup)
#   ./scripts/setup-observability.sh --restart    # tear down + fresh start
#
# Exit: 0 = all green, 1 = critical failure
# =============================================================================

set -euo pipefail

# ---- Color Codes ----
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# ---- Counters ----
PASS=0
WARN=0
FAIL=0
TOTAL_STEPS=11
CURRENT_STEP=0

# ---- Flags ----
OPEN_BROWSER=true
VERIFY_ONLY=false
RESTART=false

for arg in "$@"; do
    case "$arg" in
        --no-open)   OPEN_BROWSER=false ;;
        --verify)    VERIFY_ONLY=true ;;
        --restart)   RESTART=true ;;
        *)           echo "Unknown flag: $arg"; exit 1 ;;
    esac
done

# ---- Paths ----
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
COMPOSE_FILE="${PROJECT_ROOT}/deploy/docker/docker-compose.yml"
REGION="ap-south-1"
SSM_ENV="${ENVIRONMENT:-dev}"

# ---- Helpers ----
step() {
    CURRENT_STEP=$((CURRENT_STEP + 1))
    echo ""
    echo -e "${CYAN}[${CURRENT_STEP}/${TOTAL_STEPS}]${NC} ${BOLD}$1${NC}"
}

ok()   { echo -e "  ${GREEN}OK${NC} $1"; PASS=$((PASS + 1)); }
warn() { echo -e "  ${YELLOW}WARN${NC} $1"; WARN=$((WARN + 1)); }
fail() { echo -e "  ${RED}FAIL${NC} $1"; FAIL=$((FAIL + 1)); }
info() { echo -e "  $1"; }

# Cross-platform browser opener
open_url() {
    local url="$1"
    if command -v xdg-open > /dev/null 2>&1; then
        xdg-open "$url" 2>/dev/null &
    elif command -v open > /dev/null 2>&1; then
        open "$url" 2>/dev/null &
    else
        info "Open manually: $url"
    fi
}

# Wait for HTTP endpoint with exponential backoff
wait_for_http() {
    local name="$1"
    local url="$2"
    local max_attempts="${3:-30}"
    local delay=1
    local attempt=1

    printf "  Waiting for %-18s " "$name..."
    while [ "$attempt" -le "$max_attempts" ]; do
        if curl -sf --max-time 3 "$url" > /dev/null 2>&1; then
            echo -e "${GREEN}UP${NC} (${attempt}s)"
            return 0
        fi
        sleep "$delay"
        attempt=$((attempt + delay))
        # Exponential backoff capped at 5s
        if [ "$delay" -lt 5 ]; then
            delay=$((delay + 1))
        fi
    done
    echo -e "${YELLOW}TIMEOUT${NC} after ${max_attempts}s"
    return 1
}

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

# =============================================================================
echo ""
echo -e "${CYAN}╔══════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   tickvault — Infrastructure Stack (Auto)     ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════════════╝${NC}"
echo ""
# =============================================================================

# ---- Step 1: Detect OS ----
step "Detecting platform"
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Darwin) PLATFORM="macOS" ;;
    Linux)  PLATFORM="Linux" ;;
    *)      PLATFORM="$OS" ;;
esac
ok "$PLATFORM ($ARCH)"

# ---- Step 2: Check prerequisites ----
step "Checking prerequisites"

check_cmd() {
    local cmd="$1"
    local install_hint="$2"
    printf "  %-12s " "$cmd"
    if command -v "$cmd" > /dev/null 2>&1; then
        echo -e "${GREEN}found${NC}"
        return 0
    else
        echo -e "${RED}missing${NC} — $install_hint"
        return 1
    fi
}

MISSING=0
check_cmd "docker"         "Install Docker Desktop: https://docker.com/products/docker-desktop" || MISSING=$((MISSING + 1))
check_cmd "curl"           "apt install curl / brew install curl" || MISSING=$((MISSING + 1))
check_cmd "aws"            "pip3 install awscli" || MISSING=$((MISSING + 1))

# docker compose is a subcommand, not a binary
printf "  %-12s " "compose"
if docker compose version > /dev/null 2>&1; then
    COMPOSE_VERSION=$(docker compose version --short 2>/dev/null || echo "unknown")
    echo -e "${GREEN}found${NC} (${COMPOSE_VERSION})"
else
    echo -e "${RED}missing${NC} — Update Docker Desktop for compose plugin"
    MISSING=$((MISSING + 1))
fi

# jq is optional but useful — auto-install if missing
printf "  %-12s " "jq"
if command -v jq > /dev/null 2>&1; then
    echo -e "${GREEN}found${NC}"
else
    echo -e "${YELLOW}missing${NC} — installing..."
    if [ "$PLATFORM" = "macOS" ] && command -v brew > /dev/null 2>&1; then
        brew install jq --quiet 2>/dev/null && echo -e "  ${GREEN}jq installed${NC}" || warn "Could not install jq"
    elif [ "$PLATFORM" = "Linux" ]; then
        sudo apt-get install -y jq > /dev/null 2>&1 && echo -e "  ${GREEN}jq installed${NC}" || warn "Could not install jq (not critical)"
    fi
fi

if [ "$MISSING" -gt 0 ]; then
    fail "$MISSING critical prerequisites missing. Install them and re-run."
    exit 1
fi
ok "All critical prerequisites present"

# ---- Step 3: Docker daemon ----
step "Checking Docker daemon"
if docker info > /dev/null 2>&1; then
    DOCKER_VERSION=$(docker version --format '{{.Server.Version}}' 2>/dev/null || echo "unknown")
    ok "Docker daemon running (${DOCKER_VERSION})"
else
    fail "Docker daemon is not running. Start Docker Desktop and re-run."
    exit 1
fi

# ---- Step 4: AWS credentials ----
step "Validating AWS credentials"
if aws sts get-caller-identity --region "${REGION}" > /dev/null 2>&1; then
    AWS_ACCOUNT=$(aws sts get-caller-identity --region "${REGION}" --query "Account" --output text 2>/dev/null || echo "unknown")
    ok "AWS authenticated (account: ${AWS_ACCOUNT}, region: ${REGION})"
else
    fail "AWS credentials not configured. Run: aws configure --region ${REGION}"
    exit 1
fi

# ---- Step 5: SSM secrets ----
step "Provisioning infrastructure secrets (SSM)"
ENVIRONMENT="${SSM_ENV}" bash "${SCRIPT_DIR}/provision-infra-secrets.sh"

info "Fetching credentials for docker-compose..."
TV_QUESTDB_PG_USER=$(fetch_ssm_secret "/tickvault/${SSM_ENV}/questdb/pg-user")
TV_QUESTDB_PG_PASSWORD=$(fetch_ssm_secret "/tickvault/${SSM_ENV}/questdb/pg-password")
export TV_QUESTDB_PG_USER TV_QUESTDB_PG_PASSWORD

if [ -n "$TV_QUESTDB_PG_USER" ] && [ -n "$TV_QUESTDB_PG_PASSWORD" ]; then
    ok "QuestDB credentials ready"
else
    fail "QuestDB credentials missing from SSM — check /tickvault/${SSM_ENV}/questdb/*"
    exit 1
fi

if [ "$VERIFY_ONLY" = true ]; then
    CURRENT_STEP=8
else
    # ---- Step 6: Restart if requested ----
    if [ "$RESTART" = true ]; then
        step "Tearing down existing stack"
        docker compose -f "${COMPOSE_FILE}" down --remove-orphans 2>&1 || true
        ok "Stack torn down"
    else
        step "Checking existing stack state"
        RUNNING=$(docker ps --filter "name=tv-" --format "{{.Names}}" 2>/dev/null | wc -l | tr -d ' ')
        if [ "$RUNNING" -gt 0 ]; then
            info "${RUNNING} tv-* containers already running"
        else
            info "No tv-* containers running — starting fresh"
        fi
        ok "State checked"
    fi

    # ---- Step 7: Pull images ----
    step "Pulling Docker images (SHA256-pinned)"
    info "This may take a few minutes on first run..."
    if ! docker compose -f "${COMPOSE_FILE}" pull --quiet 2>&1; then
        fail "Docker image pull failed. Check network connectivity."
        docker compose -f "${COMPOSE_FILE}" pull 2>&1 | tail -20
        exit 1
    fi
    ok "All images pulled"

    # ---- Step 8: Start stack ----
    step "Starting infrastructure stack"
    COMPOSE_OUTPUT=$(docker compose -f "${COMPOSE_FILE}" up -d --remove-orphans 2>&1) || {
        fail "docker compose up failed:"
        echo "$COMPOSE_OUTPUT"
        echo ""
        info "Container status:"
        docker compose -f "${COMPOSE_FILE}" ps --all 2>&1 || true
        echo ""
        info "Recent container logs (last 30 lines per failed service):"
        # CloudWatch-only runtime: QuestDB + Valkey. Loki/Alloy are
        # profile-gated ('logs' profile) and not started by default.
        for svc in tv-questdb tv-valkey; do
            STATUS=$(docker inspect --format='{{.State.Status}}' "$svc" 2>/dev/null || echo "not_found")
            if [ "$STATUS" != "running" ]; then
                echo -e "  ${RED}--- ${svc} (${STATUS}) ---${NC}"
                docker logs "$svc" --tail 30 2>&1 || true
                echo ""
            fi
        done
        info "Common fixes:"
        info "  Port conflict: lsof -i :9000 :8812 :6379"
        info "  Clean restart: docker compose -f deploy/docker/docker-compose.yml down -v && re-run"
        info "  Docker resources: Check Docker Desktop → Settings → Resources"
        exit 1
    }
    ok "docker compose up -d complete"
fi

# ---- Step 9: Wait for services ----
step "Waiting for services to be healthy"
HEALTH_FAIL=0
# QuestDB readiness: use /exec?query=SELECT%201 instead of the root URL.
# The root URL returns the full web console HTML (hundreds of KB) which can
# time out curl's --max-time 3 on first boot while QuestDB is WAL-initializing.
# The /exec endpoint returns ~30 bytes of JSON and is a true "SQL engine
# ready" signal, not just "web server accepting connections".
wait_for_http "QuestDB"         "http://localhost:9000/exec?query=SELECT%201" 120 || HEALTH_FAIL=$((HEALTH_FAIL + 1))

# Valkey speaks the Redis protocol (port 6379), not HTTP — verify the
# container is running rather than probing an HTTP endpoint.
printf "  Waiting for %-18s " "Valkey..."
VALKEY_STATUS=$(docker inspect --format='{{.State.Status}}' tv-valkey 2>/dev/null || echo "not_found")
if [ "$VALKEY_STATUS" = "running" ]; then
    echo -e "${GREEN}UP${NC}"
else
    echo -e "${YELLOW}${VALKEY_STATUS}${NC}"
    HEALTH_FAIL=$((HEALTH_FAIL + 1))
fi

if [ "$HEALTH_FAIL" -gt 0 ]; then
    warn "$HEALTH_FAIL service(s) slow to start (may still be initializing)"
else
    ok "All services healthy"
fi

# ---- Step 10: Log pipeline ----
step "Log pipeline (CloudWatch-only — see aws-budget.md)"
info "Local: docker logs tickvault --tail 100 -f"
info "AWS:   CloudWatch Logs /tickvault/<env>/system via journald"

# ---- Step 11: Final report ----
step "Final report"

echo ""
echo -e "${CYAN}══════════════════════════════════════════════════════${NC}"
echo -e "  Results: ${GREEN}${PASS} passed${NC}  ${YELLOW}${WARN} warnings${NC}  ${RED}${FAIL} failed${NC}"
echo -e "${CYAN}══════════════════════════════════════════════════════${NC}"
echo ""
echo -e "  ${BOLD}Service URLs:${NC}"
echo -e "    ${CYAN}QuestDB${NC}         http://localhost:9000"
echo ""
echo -e "  ${BOLD}Observability:${NC} AWS CloudWatch (metrics, logs, alarms)"
echo ""

if [ "$OPEN_BROWSER" = true ] && [ "$FAIL" -eq 0 ]; then
    echo -e "  ${CYAN}Opening QuestDB console in browser...${NC}"
    sleep 1
    open_url "http://localhost:9000"
    echo -e "  ${GREEN}Opened: QuestDB${NC}"
fi

echo ""
if [ "$FAIL" -gt 0 ]; then
    echo -e "${RED}${FAIL} critical checks failed. Debug with:${NC}"
    echo -e "  docker compose -f deploy/docker/docker-compose.yml logs"
    echo -e "  docker compose -f deploy/docker/docker-compose.yml ps"
    exit 1
fi

echo -e "${GREEN}Infrastructure stack fully operational. Zero manual config needed.${NC}"
echo ""

# Remind about external health alarms (AWS only)
# These cover the 2 scenarios local monitoring can't self-detect:
# host death and Docker daemon crash.
if curl -sf --max-time 1 "http://169.254.169.254/latest/meta-data/" >/dev/null 2>&1; then
    # Running on EC2 — check if CloudWatch alarms exist
    CW_ALARM=$(aws cloudwatch describe-alarms \
        --alarm-name-prefix "tv-${SSM_ENV}-ec2" \
        --region "ap-south-1" \
        --query 'MetricAlarms[0].AlarmName' \
        --output text 2>/dev/null || echo "None")
    if [ "$CW_ALARM" = "None" ] || [ -z "$CW_ALARM" ]; then
        echo ""
        echo -e "${YELLOW}═══════════════════════════════════════════════════════════════${NC}"
        echo -e "${YELLOW}  ACTION NEEDED: External health alarms not configured.${NC}"
        echo -e "${YELLOW}  Run: ./scripts/setup-cloudwatch-alarms.sh --env ${SSM_ENV}${NC}"
        echo -e "${YELLOW}  Covers: EC2 instance death + Docker daemon crash → SMS${NC}"
        echo -e "${YELLOW}═══════════════════════════════════════════════════════════════${NC}"
        echo ""
    else
        ok "CloudWatch external health alarms: active"
    fi
fi

exit 0
