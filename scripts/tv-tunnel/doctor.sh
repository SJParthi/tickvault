#!/usr/bin/env bash
# tv-tunnel doctor.sh — verify the Tailscale Funnel exposes the tickvault
# API port (3001) and that the service behind it responds.
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
# migration (#O1/#O2/#O3). Security trim 2026-07-04: QuestDB (9000) is
# intentionally NO LONGER funnelled — auth-less raw SQL stays local-only.
# The tunnel fronts ONLY the tickvault API (3001, bearer-auth).
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
  echo "# QuestDB (9000) is no longer funnelled (auth-less raw SQL — local only);"
  echo "# questdb_url stays a localhost URL usable only on the host itself."
  echo "[profiles.${profile_name}]"
  echo "questdb_url       = \"http://127.0.0.1:9000\""
  echo "tickvault_api_url = \"https://${HOSTNAME_FQDN}:3001\""
  echo "# log tools read the local filesystem — no HTTP log fetch is implemented"
  echo "logs_source       = \"local\""
  echo "logs_dir_local    = \"./data/logs\""
  exit 0
fi

echo "==========================================================="
echo "  tv-tunnel doctor — ${HOSTNAME_FQDN}"
echo "==========================================================="

# 1. Check funnel status
FUNNEL_STATUS="$($TS_BIN funnel status 2>/dev/null || true)"
if grep -q "https" <<<"$FUNNEL_STATUS"; then
  pass "tailscale funnel active"
else
  fail_row "tailscale funnel inactive — run install-mac.sh or install-aws.sh"
fi

# 1b. Negative assertion (adversarial re-review HIGH, 2026-07-04): funnel
# state persists inside tailscaled across installs, so an upgraded host can
# still be publicly serving the RETIRED ports from a pre-trim install. The
# trim is only real if none of them appear in `funnel status`.
for stale_port in 9000 9090 9093 3000; do
  if grep -Eq ":${stale_port}([^0-9]|$)" <<<"$FUNNEL_STATUS"; then
    fail_row "retired port ${stale_port} is STILL publicly funnelled — stale pre-trim state; run 'tailscale funnel reset' then re-run the install script"
  else
    pass "retired port ${stale_port} not funnelled"
  fi
done

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

# 3. Hit the new debug endpoints specifically.
# Security trim 2026-07-04: /api/debug/* is gated behind bearer auth, and every
# real boot fetches the SSM token (auth always ENABLED) — so a tokenless probe
# returning 401 is the CORRECT, healthy answer on a secured host. Auth is NEVER
# legitimately disabled on a real boot (the disabled passthrough is reachable
# only in unit-test constructions), so a tokenless 200/404 on a funnelled host
# means the bearer wall was removed/bypassed — a regression, never a healthy
# state (audit Rule 11: no false-OK).
for dbg in "/api/debug/logs/summary" "/api/debug/logs/jsonl/latest"; do
  url="https://${HOSTNAME_FQDN}:3001${dbg}"
  code=$(curl -s -o /dev/null -m 5 -w "%{http_code}" "$url" || echo "000")
  if [[ "$code" == "401" ]]; then
    pass "tickvault debug endpoint ${dbg} reachable + gated (HTTP 401 — bearer auth enforced, expected)"
  elif [[ "$code" =~ ^(200|404)$ ]]; then
    fail_row "tickvault debug endpoint ${dbg} answered WITHOUT auth (HTTP ${code}) — bearer gate missing/bypassed; auth must be enabled on every real boot"
  else
    fail_row "tickvault debug endpoint ${dbg} — HTTP ${code}"
  fi
done

# 3b. Authenticated positive probe (optional — consolidated from PR #1402).
# When TV_API_TOKEN is exported, ALSO prove the gate OPENS for the real
# token: one debug route must answer non-401 (200 = artefact present,
# 404 = authed but the file hasn't been written yet). The tokenless
# 401-as-PASS probes above are unchanged — they never send the token.
# The token goes to curl via a mode-0600 header FILE (-H @file, umask 077
# + mktemp; printf is a shell builtin), NEVER argv, so it cannot leak
# through `ps` / /proc/<pid>/cmdline; the trap removes the file on exit.
if [[ -n "${TV_API_TOKEN:-}" ]]; then
  AUTH_HDR_FILE="$(umask 077 && mktemp "${TMPDIR:-/tmp}/tv-doctor-auth.XXXXXX")"
  if [[ -n "$AUTH_HDR_FILE" ]]; then
    trap 'rm -f "$AUTH_HDR_FILE"' EXIT
    printf 'Authorization: Bearer %s\n' "${TV_API_TOKEN}" >"$AUTH_HDR_FILE"
    dbg="/api/debug/logs/summary"
    url="https://${HOSTNAME_FQDN}:3001${dbg}"
    code=$(curl -s -o /dev/null -m 5 -w "%{http_code}" -H "@${AUTH_HDR_FILE}" "$url" || echo "000")
    if [[ "$code" =~ ^(200|404)$ ]]; then
      pass "authed probe ${dbg} — gate OPENS for TV_API_TOKEN (HTTP ${code})"
    elif [[ "$code" == "401" ]]; then
      fail_row "authed probe ${dbg} — TV_API_TOKEN REJECTED (HTTP 401); wrong or rotated token"
    else
      fail_row "authed probe ${dbg} — HTTP ${code}"
    fi
  else
    fail_row "authed probe skipped — mktemp failed for the 0600 header file"
  fi
else
  info "TV_API_TOKEN not set — skipping authenticated gate-open probe (export TV_API_TOKEN=... to enable)"
fi

echo "==========================================================="
if [[ $FAIL_COUNT -eq 0 ]]; then
  printf "${c_green}ALL CHECKS PASSED${c_reset} — tunnel is healthy.\n"
  exit 0
else
  printf "${c_red}%d CHECK(S) FAILED${c_reset} — see above.\n" "$FAIL_COUNT"
  exit 1
fi
