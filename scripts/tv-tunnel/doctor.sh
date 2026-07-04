#!/usr/bin/env bash
# tv-tunnel doctor.sh — verify the Tailscale Funnel exposes the tickvault
# API (port 3001 ONLY, 2026-07-04 security hardening) and that it responds.
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

# Prometheus(9090)/Alertmanager(9093)/Grafana(3000) removed in the CloudWatch-only
# migration (#O1/#O2/#O3). QuestDB (9000) removed from the funnel in the
# 2026-07-04 security hardening — the raw /exec?query= SQL port must never be
# on the public internet. The tunnel fronts ONLY the tickvault API (3001);
# QuestDB stays reachable on-box at 127.0.0.1:9000.
declare -a PORTS=(3001)
declare -A NAMES=(
  [3001]="tickvault API"
)
declare -A PATHS=(
  [3001]="/health"
)

if [[ $EMIT_CONFIG -eq 1 ]]; then
  # Emit a TOML snippet and exit — no probing, just URL rendering
  host_dashed="${HOSTNAME_FQDN//./-}"
  profile_name=$([[ "$(uname -s)" == "Darwin" ]] && echo "mac-dev" || echo "aws-prod")
  echo "# Paste under [profiles.${profile_name}] in config/claude-mcp-endpoints.toml"
  echo "[profiles.${profile_name}]"
  echo "# 2026-07-04 hardening: the funnel no longer fronts QuestDB (9000)."
  echo "# questdb_sql works ON the box only — remote sessions use the API."
  echo "questdb_url       = \"http://127.0.0.1:9000\""
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

# 3. Hit the debug endpoints specifically.
# 2026-07-04 hardening: these routes are BEARER-GATED (crates/api). An
# unauthenticated probe returning 401 IS the win — the route exists and is
# protected. 200 means a bearer token was supplied via TV_API_TOKEN; 404
# means authed but the artefact file hasn't been written yet.
# Token via a mode-0600 header FILE (-H @file), never argv — keeps it out of
# `ps` output (security review 2026-07-04). Also avoids the empty-array +
# `set -u` expansion trap on macOS's bash 3.2.
AUTH_HDR_FILE=""
if [[ -n "${TV_API_TOKEN:-}" ]]; then
  AUTH_HDR_FILE="$(umask 077 && mktemp "${TMPDIR:-/tmp}/tv-doctor-auth.XXXXXX")"
  printf 'Authorization: Bearer %s\n' "${TV_API_TOKEN}" >"$AUTH_HDR_FILE"
  trap 'rm -f "$AUTH_HDR_FILE"' EXIT
fi
for dbg in "/api/debug/logs/summary" "/api/debug/logs/jsonl/latest"; do
  url="https://${HOSTNAME_FQDN}:3001${dbg}"
  if [[ -n "$AUTH_HDR_FILE" ]]; then
    code=$(curl -s -o /dev/null -m 5 -w "%{http_code}" -H "@${AUTH_HDR_FILE}" "$url" || echo "000")
  else
    code=$(curl -s -o /dev/null -m 5 -w "%{http_code}" "$url" || echo "000")
  fi
  if [[ "$code" =~ ^(200|401|404)$ ]]; then
    pass "tickvault debug endpoint ${dbg} responding (HTTP ${code} — 401 = auth gate working)"
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
