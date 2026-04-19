#!/usr/bin/env bash
# tv-tunnel doctor.sh — verify the Tailscale Funnel exposes all 5 ports
# and that the tickvault services behind them respond.
#
# Run on either Mac or AWS. Also usable as `--emit-config` to produce a
# TOML snippet ready to paste into config/claude-mcp-endpoints.toml.

set -u

c_green="\033[0;32m"
c_yellow="\033[0;33m"
c_red="\033[0;31m"
c_reset="\033[0m"
pass() { printf "${c_green}[PASS]${c_reset} %s\n" "$*"; }
fail_row() { printf "${c_red}[FAIL]${c_reset} %s\n" "$*"; FAIL_COUNT=$((FAIL_COUNT + 1)); }
warn() { printf "${c_yellow}[WARN]${c_reset} %s\n" "$*"; }
info() { printf "  %s\n" "$*"; }

FAIL_COUNT=0
EMIT_CONFIG=0
if [[ "${1:-}" == "--emit-config" ]]; then
  EMIT_CONFIG=1
fi

# Resolve tailscale binary for macOS vs Linux
if [[ -x /Applications/Tailscale.app/Contents/MacOS/Tailscale ]]; then
  TS_BIN=/Applications/Tailscale.app/Contents/MacOS/Tailscale
elif command -v tailscale >/dev/null 2>&1; then
  TS_BIN=$(command -v tailscale)
else
  fail_row "tailscale binary not found"
  exit 1
fi

HOSTNAME_FQDN="$($TS_BIN status --json 2>/dev/null | awk -F '"' '/"DNSName":/{print $4; exit}' | sed 's/\.$//')"
[[ -z "$HOSTNAME_FQDN" ]] && { fail_row "tailscale not logged in"; exit 1; }

declare -a PORTS=(9090 9093 9000 3000 3001)
declare -A NAMES=(
  [9090]="Prometheus"
  [9093]="Alertmanager"
  [9000]="QuestDB HTTP"
  [3000]="Grafana"
  [3001]="tickvault API"
)
declare -A PATHS=(
  [9090]="/-/ready"
  [9093]="/-/ready"
  [9000]="/"
  [3000]="/api/health"
  [3001]="/health"
)

if [[ $EMIT_CONFIG -eq 1 ]]; then
  # Emit a TOML snippet and exit — no probing, just URL rendering
  host_dashed="${HOSTNAME_FQDN//./-}"
  profile_name=$([[ "$(uname -s)" == "Darwin" ]] && echo "mac-dev" || echo "aws-prod")
  echo "# Paste under [profiles.${profile_name}] in config/claude-mcp-endpoints.toml"
  echo "[profiles.${profile_name}]"
  echo "prometheus_url    = \"https://${HOSTNAME_FQDN}:9090\""
  echo "alertmanager_url  = \"https://${HOSTNAME_FQDN}:9093\""
  echo "questdb_url       = \"https://${HOSTNAME_FQDN}:9000\""
  echo "grafana_url       = \"https://${HOSTNAME_FQDN}:3000\""
  echo "tickvault_api_url = \"https://${HOSTNAME_FQDN}:3001\""
  echo "logs_source       = \"http\""
  echo "logs_dir_local    = \"./data/logs\""
  exit 0
fi

echo "==========================================================="
echo "  tv-tunnel doctor — ${HOSTNAME_FQDN}"
echo "==========================================================="

# 1. Check funnel status
if $TS_BIN funnel status 2>/dev/null | grep -q "https"; then
  pass "tailscale funnel active"
else
  fail_row "tailscale funnel inactive — run install-mac.sh or install-aws.sh"
fi

# 2. Probe each port
for port in "${PORTS[@]}"; do
  name=${NAMES[$port]}
  path=${PATHS[$port]}
  url="https://${HOSTNAME_FQDN}:${port}${path}"
  code=$(curl -s -o /dev/null -m 5 -w "%{http_code}" "$url" || echo "000")
  if [[ "$code" =~ ^(200|204|301|302|401|403)$ ]]; then
    pass "${name} via funnel (HTTP ${code})"
  else
    fail_row "${name} via funnel — ${url} returned HTTP ${code}"
  fi
done

# 3. Hit the new debug endpoints specifically
for dbg in "/api/debug/logs/summary" "/api/debug/logs/jsonl/latest"; do
  url="https://${HOSTNAME_FQDN}:3001${dbg}"
  code=$(curl -s -o /dev/null -m 5 -w "%{http_code}" "$url" || echo "000")
  if [[ "$code" =~ ^(200|404)$ ]]; then
    # 404 is acceptable if the app hasn't written errors.summary.md yet
    pass "tickvault debug endpoint ${dbg} reachable (HTTP ${code})"
  else
    fail_row "tickvault debug endpoint ${dbg} — HTTP ${code}"
  fi
done

echo "==========================================================="
if [[ $FAIL_COUNT -eq 0 ]]; then
  printf "${c_green}ALL CHECKS PASSED${c_reset} — tunnel is healthy.\n"
  exit 0
else
  printf "${c_red}%d CHECK(S) FAILED${c_reset} — see above.\n" "$FAIL_COUNT"
  exit 1
fi
