#!/usr/bin/env bash
# =============================================================================
# tickvault — Observability Stack: FULL AUTO Setup
# =============================================================================
# ONE COMMAND. ZERO HUMAN INTERVENTION.
#
# What this does (end-to-end):
#   1.  Detect OS (macOS / Linux) and set platform-specific commands
#   2.  Check prerequisites (Docker, docker compose, curl, jq)
#   3.  Auto-install missing prerequisites where possible
#   4.  Validate AWS credentials exist (for Grafana/QuestDB secrets)
#   5.  Fetch/provision all SSM secrets (idempotent)
#   6.  Pull all Docker images (7 services, pinned SHA256)
#   7.  Start the full stack (docker compose up -d)
#   8.  Health-wait every service with exponential backoff
#   9.  Validate Prometheus scrape targets are UP
#   10. Validate Grafana datasources are provisioned
#   11. Validate Grafana dashboards are loaded
#   12. Validate Grafana alert rules are provisioned
#   13. Validate Prometheus recording + alert rules loaded
#   14. Validate Traefik routes are configured
#   15. Validate Jaeger OTLP endpoints are accepting traces
#   16. Validate Loki is accepting logs via Alloy
#   17. Print full status report
#   18. Auto-open all dashboards in browser
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
TOTAL_STEPS=18
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
echo -e "${CYAN}║   tickvault — Observability Stack (Auto)      ║${NC}"
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
TV_GRAFANA_ADMIN_USER=$(fetch_ssm_secret "/tickvault/${SSM_ENV}/grafana/admin-user")
TV_GRAFANA_ADMIN_PASSWORD=$(fetch_ssm_secret "/tickvault/${SSM_ENV}/grafana/admin-password")
export TV_QUESTDB_PG_USER TV_QUESTDB_PG_PASSWORD TV_GRAFANA_ADMIN_USER TV_GRAFANA_ADMIN_PASSWORD

# Telegram credentials — used by Grafana alerting contact point (alerts.yml).
# Same SSM path as the Rust app and notify-telegram.sh — ONE source, everywhere.
TV_TELEGRAM_BOT_TOKEN=$(fetch_ssm_secret "/tickvault/${SSM_ENV}/telegram/bot-token" 2>/dev/null || echo "")
TV_TELEGRAM_CHAT_ID=$(fetch_ssm_secret "/tickvault/${SSM_ENV}/telegram/chat-id" 2>/dev/null || echo "")
export TV_TELEGRAM_BOT_TOKEN TV_TELEGRAM_CHAT_ID

if [ -n "$TV_TELEGRAM_BOT_TOKEN" ] && [ -n "$TV_TELEGRAM_CHAT_ID" ]; then
    ok "All 6 secrets ready (infra + Telegram)"
else
    ok "4 infrastructure secrets ready"
    warn "Telegram secrets not in SSM — Grafana alerts will not reach Telegram (Rust-side alerts unaffected)"
fi

if [ "$VERIFY_ONLY" = true ]; then
    CURRENT_STEP=8
    TOTAL_STEPS=18
else
    # ---- Step 6: Restart if requested ----
    if [ "$RESTART" = true ]; then
        step "Tearing down existing stack"
        docker compose -f "${COMPOSE_FILE}" down --remove-orphans 2>&1 || true
        # Remove Grafana data volume to purge stale provisioning state.
        # Old Telegram contact point (with empty bot token) persists in Grafana's
        # SQLite DB even after removing it from alerts.yml — causes crash-loop.
        # All dashboards/datasources/alerts are defined in provisioning files, so
        # nothing is lost by clearing the volume.
        docker volume rm tv-grafana-data 2>/dev/null || true
        ok "Stack torn down (Grafana volume reset)"
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
    step "Pulling Docker images (7 services, SHA256-pinned)"
    info "This may take a few minutes on first run (~2GB)..."
    if ! docker compose -f "${COMPOSE_FILE}" pull --quiet 2>&1; then
        fail "Docker image pull failed. Check network connectivity."
        docker compose -f "${COMPOSE_FILE}" pull 2>&1 | tail -20
        exit 1
    fi
    ok "All images pulled"

    # ---- Step 8: Start stack ----
    step "Starting observability stack"
    COMPOSE_OUTPUT=$(docker compose -f "${COMPOSE_FILE}" up -d --remove-orphans 2>&1) || {
        fail "docker compose up failed:"
        echo "$COMPOSE_OUTPUT"
        echo ""
        info "Container status:"
        docker compose -f "${COMPOSE_FILE}" ps --all 2>&1 || true
        echo ""
        info "Recent container logs (last 30 lines per failed service):"
        # Loki, Alloy, Jaeger removed per aws-budget.md (saves 2.5GB RAM).
        # Keep in sync with docker-compose.yml services.
        for svc in tv-questdb tv-valkey tv-valkey-exporter tv-prometheus tv-alertmanager tv-grafana tv-traefik; do
            STATUS=$(docker inspect --format='{{.State.Status}}' "$svc" 2>/dev/null || echo "not_found")
            if [ "$STATUS" != "running" ]; then
                echo -e "  ${RED}--- ${svc} (${STATUS}) ---${NC}"
                docker logs "$svc" --tail 30 2>&1 || true
                echo ""
            fi
        done
        info "Common fixes:"
        info "  Port conflict: lsof -i :9000 :8812 :3000 :6379 :80 :443"
        info "  Clean restart: docker compose -f deploy/docker/docker-compose.yml down -v && re-run"
        info "  Docker resources: Check Docker Desktop → Settings → Resources"
        exit 1
    }
    ok "docker compose up -d complete"
fi

# ---- Step 9: Wait for all services ----
step "Waiting for services to be healthy"
HEALTH_FAIL=0
# Jaeger, Alloy, Loki removed per aws-budget.md (2.5GB RAM saving).
# QuestDB readiness: use /exec?query=SELECT%201 instead of the root URL.
# The root URL returns the full web console HTML (hundreds of KB) which can
# time out curl's --max-time 3 on first boot while QuestDB is WAL-initializing.
# The /exec endpoint returns ~30 bytes of JSON and is a true "SQL engine
# ready" signal, not just "web server accepting connections".
wait_for_http "QuestDB"         "http://localhost:9000/exec?query=SELECT%201" 120 || HEALTH_FAIL=$((HEALTH_FAIL + 1))
wait_for_http "Prometheus"      "http://localhost:9090/-/healthy"    30 || HEALTH_FAIL=$((HEALTH_FAIL + 1))
wait_for_http "Grafana"         "http://localhost:3000/api/health"  45 || HEALTH_FAIL=$((HEALTH_FAIL + 1))
wait_for_http "Alertmanager"    "http://localhost:9093/-/healthy"    30 || HEALTH_FAIL=$((HEALTH_FAIL + 1))
wait_for_http "Traefik"         "http://localhost:8080/api/rawdata" 20 || HEALTH_FAIL=$((HEALTH_FAIL + 1))
wait_for_http "Traefik Metrics" "http://localhost:8082/metrics"     20 || HEALTH_FAIL=$((HEALTH_FAIL + 1))

if [ "$HEALTH_FAIL" -gt 0 ]; then
    warn "$HEALTH_FAIL services slow to start (may still be initializing)"
else
    ok "All services healthy"
fi

# ---- Step 10: Prometheus targets ----
step "Validating Prometheus scrape targets"
PROM_TARGETS=$(curl -sf --max-time 5 "http://localhost:9090/api/v1/targets" 2>/dev/null || echo "")
if [ -n "$PROM_TARGETS" ]; then
    UP_COUNT=$(echo "$PROM_TARGETS" | { grep -o '"health":"up"' || true; } | wc -l | tr -d ' ')
    DOWN_COUNT=$(echo "$PROM_TARGETS" | { grep -o '"health":"down"' || true; } | wc -l | tr -d ' ')
    TOTAL_TARGETS=$((UP_COUNT + DOWN_COUNT))
    info "Targets: ${UP_COUNT}/${TOTAL_TARGETS} UP"
    if [ "$DOWN_COUNT" -gt 0 ]; then
        # Extract down job names
        echo "$PROM_TARGETS" | grep -o '"job":"[^"]*"[^}]*"health":"down"' | grep -o '"job":"[^"]*"' | sort -u | while read -r line; do
            warn "Target DOWN: $line"
        done
    fi
    if [ "$UP_COUNT" -ge 3 ]; then
        ok "Core targets reachable (${UP_COUNT} up)"
    else
        warn "Only ${UP_COUNT} targets up — some services still starting"
    fi
else
    warn "Could not query Prometheus targets API"
fi

# ---- Step 11: Grafana datasources ----
step "Validating Grafana datasources"
GRAFANA_AUTH="$(echo -n "${TV_GRAFANA_ADMIN_USER}:${TV_GRAFANA_ADMIN_PASSWORD}" | base64)"
DS_JSON=$(curl -sf --max-time 5 "http://localhost:3000/api/datasources" \
    -H "Authorization: Basic ${GRAFANA_AUTH}" 2>/dev/null || echo "")
if [ -n "$DS_JSON" ] && [ "$DS_JSON" != "[]" ]; then
    DS_COUNT=$(echo "$DS_JSON" | { grep -o '"name"' || true; } | wc -l | tr -d ' ')
    # Check each expected datasource
    for ds in Prometheus Loki Jaeger QuestDB; do
        if echo "$DS_JSON" | grep -q "\"name\":\"${ds}\""; then
            info "  ${ds}: provisioned"
        else
            warn "  ${ds}: NOT found"
        fi
    done
    ok "${DS_COUNT} datasources configured"
else
    warn "Could not verify datasources (Grafana may still be starting)"
fi

# ---- Step 12: Grafana dashboards ----
step "Validating Grafana dashboards"
DASH_JSON=$(curl -sf --max-time 5 "http://localhost:3000/api/search?type=dash-db" \
    -H "Authorization: Basic ${GRAFANA_AUTH}" 2>/dev/null || echo "")
if [ -n "$DASH_JSON" ] && [ "$DASH_JSON" != "[]" ]; then
    DASH_COUNT=$(echo "$DASH_JSON" | { grep -o '"uid"' || true; } | wc -l | tr -d ' ')
    # Check for expected dashboards by UID
    for uid in tv-system-overview tv-traefik tv-logs; do
        DASH_TITLE=$(echo "$DASH_JSON" | grep -o "\"uid\":\"${uid}\"[^}]*\"title\":\"[^\"]*\"" | grep -o '"title":"[^"]*"' | head -1 || echo "")
        if [ -n "$DASH_TITLE" ]; then
            info "  ${uid}: loaded"
        else
            warn "  ${uid}: not found (may need Grafana restart)"
        fi
    done
    ok "${DASH_COUNT} dashboards provisioned"
else
    warn "Could not verify dashboards (provisioning may need a moment)"
fi

# ---- Step 13: Grafana alert rules + Telegram contact point ----
step "Validating Grafana alert rules + Telegram contact point"
ALERT_RULES=$(curl -sf --max-time 5 "http://localhost:3000/api/v1/provisioning/alert-rules" \
    -H "Authorization: Basic ${GRAFANA_AUTH}" 2>/dev/null || echo "")
if [ -n "$ALERT_RULES" ] && [ "$ALERT_RULES" != "[]" ]; then
    ALERT_COUNT=$(echo "$ALERT_RULES" | { grep -o '"uid"' || true; } | wc -l | tr -d ' ')
    ok "${ALERT_COUNT} alert rules provisioned"
else
    warn "Alert rules not yet loaded (Grafana unified alerting may need time)"
fi

# Configure Telegram contact point via API (not file provisioning — empty tokens crash Grafana)
CP_JSON=$(curl -sf --max-time 5 "http://localhost:3000/api/v1/provisioning/contact-points" \
    -H "Authorization: Basic ${GRAFANA_AUTH}" 2>/dev/null || echo "")
if echo "$CP_JSON" | grep -q "tv-telegram"; then
    ok "Telegram contact point already configured"
elif [ -n "$TV_TELEGRAM_BOT_TOKEN" ] && [ -n "$TV_TELEGRAM_CHAT_ID" ]; then
    # Create Telegram contact point via Grafana API
    TELEGRAM_PAYLOAD=$(cat <<EOFPAYLOAD
{
  "name": "tv-telegram",
  "type": "telegram",
  "settings": {
    "bottoken": "${TV_TELEGRAM_BOT_TOKEN}",
    "chatid": "${TV_TELEGRAM_CHAT_ID}",
    "parse_mode": "HTML",
    "message": "{{ if gt (len .Alerts.Firing) 0 }}🔴 FIRING{{ end }}{{ if gt (len .Alerts.Resolved) 0 }}🟢 RESOLVED{{ end }}\n{{ range .Alerts }}\n<b>{{ .Labels.alertname }}</b>\n{{ .Annotations.summary }}\n{{ .Annotations.description }}\nSeverity: {{ .Labels.severity }}\n{{ end }}"
  },
  "disableResolveMessage": false
}
EOFPAYLOAD
    )
    CP_RESULT=$(curl -sf --max-time 5 -X POST "http://localhost:3000/api/v1/provisioning/contact-points" \
        -H "Authorization: Basic ${GRAFANA_AUTH}" \
        -H "Content-Type: application/json" \
        -H "X-Disable-Provenance: true" \
        -d "${TELEGRAM_PAYLOAD}" 2>/dev/null || echo "")
    if echo "$CP_RESULT" | grep -q "uid"; then
        # Update notification policy to use Telegram
        POLICY_PAYLOAD='{"receiver":"tv-telegram","group_by":["grafana_folder","alertname"],"group_wait":"30s","group_interval":"5m","repeat_interval":"4h"}'
        curl -sf --max-time 5 -X PUT "http://localhost:3000/api/v1/provisioning/policies" \
            -H "Authorization: Basic ${GRAFANA_AUTH}" \
            -H "Content-Type: application/json" \
            -H "X-Disable-Provenance: true" \
            -d "${POLICY_PAYLOAD}" > /dev/null 2>&1 || true
        ok "Telegram contact point created — Grafana alerts → your phone"
    else
        warn "Could not create Telegram contact point (Grafana API error)"
    fi
else
    info "Telegram contact point skipped (no bot token in SSM — alerts go to default receiver)"
fi

# ---- Step 14: Prometheus rules ----
step "Validating Prometheus recording & alert rules"
PROM_RULES=$(curl -sf --max-time 5 "http://localhost:9090/api/v1/rules" 2>/dev/null || echo "")
if [ -n "$PROM_RULES" ]; then
    RULE_GROUPS=$(echo "$PROM_RULES" | { grep -o '"name"' || true; } | wc -l | tr -d ' ')
    RECORDING=$(echo "$PROM_RULES" | { grep -o '"type":"recording"' || true; } | wc -l | tr -d ' ')
    ALERTING=$(echo "$PROM_RULES" | { grep -o '"type":"alerting"' || true; } | wc -l | tr -d ' ')
    info "Rule groups: ${RULE_GROUPS}"
    info "Recording rules: ${RECORDING}"
    info "Alert rules: ${ALERTING}"
    if [ "$RULE_GROUPS" -gt 0 ]; then
        ok "Rules loaded successfully"
    else
        warn "No rules loaded — check /etc/prometheus/rules/*.yml"
    fi
else
    warn "Could not query Prometheus rules API"
fi

# ---- Step 15: Traefik routes ----
step "Validating Traefik dynamic routes"
TRAEFIK_ROUTERS=$(curl -sf --max-time 5 "http://localhost:8080/api/http/routers" 2>/dev/null || echo "")
if [ -n "$TRAEFIK_ROUTERS" ]; then
    ROUTER_COUNT=$(echo "$TRAEFIK_ROUTERS" | { grep -o '"name"' || true; } | wc -l | tr -d ' ')
    info "HTTP routers: ${ROUTER_COUNT}"
    # Check for expected routes
    for route in grafana prometheus questdb; do
        if echo "$TRAEFIK_ROUTERS" | grep -qi "$route"; then
            info "  /${route}: routed"
        else
            warn "  /${route}: not found"
        fi
    done
    ok "Dynamic routing configured"
else
    warn "Could not query Traefik API"
fi

# ---- Step 16: Jaeger OTLP ----
step "Validating Jaeger OTLP endpoints"
# Check OTLP HTTP endpoint
OTLP_HTTP_OK=false
if curl -sf --max-time 3 -X POST "http://localhost:4318/v1/traces" \
    -H "Content-Type: application/json" \
    -d '{"resourceSpans":[]}' > /dev/null 2>&1; then
    OTLP_HTTP_OK=true
    info "OTLP HTTP (:4318): accepting traces"
else
    # Some Jaeger versions return 400 for empty payload but that means it's listening
    HTTP_CODE=$(curl -sf --max-time 3 -o /dev/null -w "%{http_code}" -X POST "http://localhost:4318/v1/traces" \
        -H "Content-Type: application/json" \
        -d '{"resourceSpans":[]}' 2>/dev/null || echo "000")
    if [ "$HTTP_CODE" != "000" ]; then
        OTLP_HTTP_OK=true
        info "OTLP HTTP (:4318): listening (HTTP ${HTTP_CODE})"
    else
        warn "OTLP HTTP (:4318): not reachable"
    fi
fi

# Check OTLP gRPC port is open
if timeout 3 bash -c 'cat < /dev/null > /dev/tcp/localhost/4317' 2>/dev/null; then
    info "OTLP gRPC (:4317): port open"
    ok "Jaeger OTLP endpoints ready"
else
    if [ "$OTLP_HTTP_OK" = true ]; then
        ok "Jaeger OTLP HTTP ready (gRPC may need a moment)"
    else
        warn "Jaeger OTLP endpoints not yet reachable"
    fi
fi

# ---- Step 17: Log pipeline (Loki + Alloy removed per aws-budget.md) ----
# Loki and Alloy were removed to save 2.5GB RAM on the c7i.xlarge budget.
# Logs are now read directly via `docker logs tickvault --tail 100 -f`
# locally, and via CloudWatch journald collection on AWS (see
# user-data.sh.tftpl). This step is intentionally a no-op — kept so
# step numbering matches prior runbook references.
step "Log pipeline (Loki + Alloy removed — see aws-budget.md)"
info "Local: docker logs tickvault --tail 100 -f"
info "AWS:   CloudWatch Logs /tickvault/<env>/system via journald"

# ---- Step 18: Summary + auto-open ----
step "Final report"

echo ""
echo -e "${CYAN}══════════════════════════════════════════════════════${NC}"
echo -e "  Results: ${GREEN}${PASS} passed${NC}  ${YELLOW}${WARN} warnings${NC}  ${RED}${FAIL} failed${NC}"
echo -e "${CYAN}══════════════════════════════════════════════════════${NC}"
echo ""
echo -e "  ${BOLD}Service URLs:${NC}"
echo -e "    ${CYAN}Grafana${NC}         http://localhost:3000"
echo -e "    ${CYAN}Prometheus${NC}      http://localhost:9090"
echo -e "    ${CYAN}Alertmanager${NC}    http://localhost:9093"
echo -e "    ${CYAN}Traefik${NC}         http://localhost:8080"
echo -e "    ${CYAN}QuestDB${NC}         http://localhost:9000"
echo ""
echo -e "  ${BOLD}Traefik Unified Routes:${NC}"
echo -e "    ${CYAN}All Services${NC}    http://localhost/grafana"
echo -e "                    http://localhost/prometheus"
echo -e "                    http://localhost/questdb"
echo ""
echo -e "  ${BOLD}Grafana Credentials:${NC}"
echo -e "    User: ${TV_GRAFANA_ADMIN_USER}"
echo -e "    Pass: (from AWS SSM /tickvault/${SSM_ENV}/grafana/admin-password)"
echo ""

if [ "$OPEN_BROWSER" = true ] && [ "$FAIL" -eq 0 ]; then
    echo -e "  ${CYAN}Opening dashboards in browser...${NC}"
    sleep 1
    open_url "http://localhost:3000/d/tv-system-overview/tv-system-overview?orgId=1"
    sleep 0.5
    open_url "http://localhost:9090/targets"
    sleep 0.5
    open_url "http://localhost:8080"
    sleep 0.5
    open_url "http://localhost:9000"
    echo -e "  ${GREEN}Opened: Grafana, Prometheus, Traefik, QuestDB${NC}"
fi

echo ""
if [ "$FAIL" -gt 0 ]; then
    echo -e "${RED}${FAIL} critical checks failed. Debug with:${NC}"
    echo -e "  docker compose -f deploy/docker/docker-compose.yml logs"
    echo -e "  docker compose -f deploy/docker/docker-compose.yml ps"
    exit 1
fi

echo -e "${GREEN}Observability stack fully operational. Zero manual config needed.${NC}"
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
