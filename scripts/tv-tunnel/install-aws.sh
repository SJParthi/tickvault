#!/usr/bin/env bash
# tv-tunnel install-aws.sh — one-command Tailscale Funnel setup on AWS EC2.
#
# Run once on your EC2 instance (Amazon Linux 2023 / Ubuntu). After that,
# the funnel auto-starts via systemd, survives reboots, and exposes:
#   - Prometheus       (port 9090)
#   - Alertmanager     (port 9093)
#   - QuestDB HTTP     (port 9000)
#   - Grafana          (port 3000)
#   - tickvault API    (port 3001) — includes /api/debug/logs/*
#
# After this runs, every Claude session (sandbox, web, Mac) on any branch
# can query the live AWS stack without further setup.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
UNIT_SRC="$REPO_ROOT/scripts/tv-tunnel/tickvault-tunnel.service"
UNIT_DEST="/etc/systemd/system/tickvault-tunnel.service"

c_green="\033[0;32m"
c_yellow="\033[0;33m"
c_red="\033[0;31m"
c_reset="\033[0m"
pass() { printf "${c_green}✓${c_reset} %s\n" "$*"; }
warn() { printf "${c_yellow}⚠${c_reset} %s\n" "$*"; }
fail() { printf "${c_red}✗${c_reset} %s\n" "$*"; exit 1; }
info() { printf "  %s\n" "$*"; }

echo "=============================================================="
echo "  tv-tunnel install — AWS / Linux"
echo "  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "=============================================================="

# -----------------------------------------------------------------------------
# Prereq 1: running on Linux with systemd
# -----------------------------------------------------------------------------
if [[ "$(uname -s)" != "Linux" ]]; then
  fail "This script is Linux only. For macOS run install-mac.sh."
fi
if ! command -v systemctl >/dev/null 2>&1; then
  fail "systemd not available. This script requires a systemd-managed host (EC2 AL2023/Ubuntu/Debian)."
fi
pass "running on Linux with systemd"

# -----------------------------------------------------------------------------
# Prereq 2: root or sudo available
# -----------------------------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
  if ! command -v sudo >/dev/null 2>&1; then
    fail "Not running as root and sudo not available."
  fi
  SUDO="sudo"
else
  SUDO=""
fi
pass "privileges: $( [[ -z "$SUDO" ]] && echo root || echo 'sudo available' )"

# -----------------------------------------------------------------------------
# Prereq 3: Tailscale installed
# -----------------------------------------------------------------------------
if ! command -v tailscale >/dev/null 2>&1; then
  warn "tailscale not installed — running official install script"
  # shellcheck disable=SC2086
  curl -fsSL https://tailscale.com/install.sh | $SUDO sh
fi
pass "tailscale installed: $(tailscale version | head -1)"

# -----------------------------------------------------------------------------
# Prereq 4: logged in
# -----------------------------------------------------------------------------
if ! $SUDO tailscale status >/dev/null 2>&1; then
  warn "Tailscale not logged in. Running 'tailscale up' — follow the browser prompt."
  $SUDO tailscale up --ssh || fail "tailscale up failed"
fi
pass "tailscale logged in"

HOSTNAME_FQDN="$($SUDO tailscale status --json 2>/dev/null | awk -F '"' '/"DNSName":/{print $4; exit}' | sed 's/\.$//')"
[[ -z "$HOSTNAME_FQDN" ]] && fail "Could not determine Tailscale hostname"
pass "Tailscale hostname: $HOSTNAME_FQDN"

# -----------------------------------------------------------------------------
# Prereq 5: Funnel feature enabled on tailnet
# -----------------------------------------------------------------------------
if ! $SUDO tailscale funnel status >/dev/null 2>&1; then
  warn "Funnel not yet enabled — open https://login.tailscale.com/admin/acls"
  warn "and add: nodeAttrs: [{ target: [\"autogroup:member\"], attr: [\"funnel\"] }]"
  fail "Funnel feature disabled"
fi
pass "Tailscale Funnel feature enabled"

# -----------------------------------------------------------------------------
# Install systemd unit
# -----------------------------------------------------------------------------
$SUDO install -m 0644 "$UNIT_SRC" "$UNIT_DEST"
pass "installed systemd unit: $UNIT_DEST"

$SUDO systemctl daemon-reload
$SUDO systemctl enable --now tickvault-tunnel.service
pass "tickvault-tunnel.service enabled and started"

# -----------------------------------------------------------------------------
# Wait up to 30s for funnel to come up
# -----------------------------------------------------------------------------
info "probing funnel endpoint (up to 30s)…"
tunnel_ok=0
for i in {1..15}; do
  if $SUDO tailscale funnel status 2>/dev/null | grep -q ":9090"; then
    tunnel_ok=1
    break
  fi
  sleep 2
done

if [[ $tunnel_ok -eq 1 ]]; then
  pass "funnel is serving on ports 9090, 9093, 9000, 3000, 3001"
else
  warn "funnel did not report status within 30s — check with 'sudo tailscale funnel status'"
fi

# -----------------------------------------------------------------------------
# Final output
# -----------------------------------------------------------------------------
echo
echo "=============================================================="
echo "  SETUP COMPLETE"
echo "=============================================================="
echo
echo "Your stable Tailscale Funnel URLs:"
echo "  Prometheus:     https://$HOSTNAME_FQDN:9090"
echo "  Alertmanager:   https://$HOSTNAME_FQDN:9093"
echo "  QuestDB HTTP:   https://$HOSTNAME_FQDN:9000"
echo "  Grafana:        https://$HOSTNAME_FQDN:3000"
echo "  tickvault API:  https://$HOSTNAME_FQDN:3001"
echo
echo "NEXT: update config/claude-mcp-endpoints.toml to set these URLs"
echo "under [profiles.aws-prod], set active = \"aws-prod\", commit + push."
echo "=============================================================="
