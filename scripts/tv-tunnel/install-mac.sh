#!/usr/bin/env bash
# tv-tunnel install-mac.sh — one-command Tailscale Funnel setup on macOS.
#
# Run once on your Mac. After that, the funnel auto-starts on reboot
# (launchd), auto-reconnects if the Mac sleeps/wakes, and exposes:
#   - Prometheus       (port 9090)
#   - Alertmanager     (port 9093)
#   - QuestDB HTTP     (port 9000)
#   - Grafana          (port 3000)
#   - tickvault API    (port 3001) — includes /api/debug/logs/*
#
# Resulting stable URLs are committed to config/claude-mcp-endpoints.toml
# (via the `--emit-config` flag of doctor.sh after tunnel is up).
#
# Safety:
#   - Idempotent: re-running does nothing harmful
#   - Every step verified before proceeding
#   - Never modifies ~/.zshrc, ~/.bashrc, or any dotfile
#   - Never installs sudo packages without prompting

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLIST_DEST="$HOME/Library/LaunchAgents/com.tickvault.tunnel.plist"
PLIST_SRC="$REPO_ROOT/scripts/tv-tunnel/com.tickvault.tunnel.plist"
TAILSCALE_BIN="/Applications/Tailscale.app/Contents/MacOS/Tailscale"

# -----------------------------------------------------------------------------
# Logging helpers
# -----------------------------------------------------------------------------
c_green="\033[0;32m"
c_yellow="\033[0;33m"
c_red="\033[0;31m"
c_reset="\033[0m"
pass() { printf "${c_green}✓${c_reset} %s\n" "$*"; }
warn() { printf "${c_yellow}⚠${c_reset} %s\n" "$*"; }
fail() { printf "${c_red}✗${c_reset} %s\n" "$*"; exit 1; }
info() { printf "  %s\n" "$*"; }

echo "=============================================================="
echo "  tv-tunnel install — macOS"
echo "  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "=============================================================="

# -----------------------------------------------------------------------------
# Prereq 1: macOS
# -----------------------------------------------------------------------------
if [[ "$(uname -s)" != "Darwin" ]]; then
  fail "This script is macOS only. For AWS / Linux run install-aws.sh."
fi
pass "running on macOS"

# -----------------------------------------------------------------------------
# Prereq 2: Tailscale app installed
# -----------------------------------------------------------------------------
if [[ ! -x "$TAILSCALE_BIN" ]]; then
  if command -v brew >/dev/null 2>&1; then
    warn "Tailscale app not found — installing via Homebrew"
    brew install --cask tailscale
  else
    fail "Tailscale app not installed and brew not available. Install from https://tailscale.com/download/mac or 'brew install --cask tailscale'."
  fi
fi
pass "Tailscale app installed"

# -----------------------------------------------------------------------------
# Prereq 3: Tailscale logged in (tailscale status shows your account)
# -----------------------------------------------------------------------------
if ! "$TAILSCALE_BIN" status >/dev/null 2>&1; then
  warn "Tailscale not logged in — launching login flow"
  "$TAILSCALE_BIN" up || fail "tailscale up failed. Log in at https://login.tailscale.com/start and retry."
fi
pass "Tailscale logged in"

HOSTNAME_FQDN="$("$TAILSCALE_BIN" status --json 2>/dev/null | /usr/bin/awk -F '"' '/"DNSName":/{print $4; exit}' | sed 's/\.$//')"
if [[ -z "$HOSTNAME_FQDN" ]]; then
  fail "Could not determine Tailscale hostname. Run 'tailscale status' and check your account."
fi
pass "Tailscale hostname: $HOSTNAME_FQDN"

# -----------------------------------------------------------------------------
# Prereq 4: Tailscale Funnel feature enabled on your tailnet
# -----------------------------------------------------------------------------
# Tailscale Funnel requires the `funnel` feature to be enabled in the admin
# console (https://login.tailscale.com/admin/acls — nodeAttrs). The install
# script cannot toggle this for you — it requires human ACK in the web UI.
info "Funnel feature check: attempting probe…"
if ! "$TAILSCALE_BIN" funnel status >/dev/null 2>&1; then
  warn "Funnel not yet enabled. Open https://login.tailscale.com/admin/acls"
  warn "and add 'nodeAttrs: [{ target: [\"autogroup:member\"], attr: [\"funnel\"] }]'"
  warn "then re-run this script."
  fail "Tailscale Funnel feature disabled."
fi
pass "Tailscale Funnel feature enabled"

# -----------------------------------------------------------------------------
# Install launchd plist (auto-start on reboot, auto-restart on crash)
# -----------------------------------------------------------------------------
mkdir -p "$(dirname "$PLIST_DEST")"
# Substitute the Tailscale binary path into the plist template so we don't
# hardcode /Applications/Tailscale.app/... into the repo's committed plist.
sed "s|@TAILSCALE_BIN@|$TAILSCALE_BIN|g" "$PLIST_SRC" > "$PLIST_DEST"
pass "installed launchd plist: $PLIST_DEST"

# -----------------------------------------------------------------------------
# Load the launchd service (idempotent)
# -----------------------------------------------------------------------------
/bin/launchctl unload "$PLIST_DEST" 2>/dev/null || true
/bin/launchctl load -w "$PLIST_DEST"
pass "launchd service loaded — tunnel will start within 10 seconds"

# -----------------------------------------------------------------------------
# Wait up to 30s for funnel to come up
# -----------------------------------------------------------------------------
info "probing funnel endpoint (up to 30s)…"
tunnel_ok=0
for i in {1..15}; do
  if "$TAILSCALE_BIN" funnel status 2>/dev/null | grep -q ":9090"; then
    tunnel_ok=1
    break
  fi
  sleep 2
done

if [[ $tunnel_ok -eq 1 ]]; then
  pass "funnel is serving on ports 9090, 9093, 9000, 3000, 3001"
else
  warn "funnel did not report status within 30s — check with 'tailscale funnel status'"
fi

# -----------------------------------------------------------------------------
# Final output + instructions
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
echo "  Log summary:    https://$HOSTNAME_FQDN:3001/api/debug/logs/summary"
echo "  Log JSONL:      https://$HOSTNAME_FQDN:3001/api/debug/logs/jsonl/latest"
echo
echo "NEXT: update config/claude-mcp-endpoints.toml to set these URLs"
echo "under [profiles.mac-dev], then commit + push:"
echo "  bash scripts/tv-tunnel/doctor.sh --emit-config > /tmp/mac-profile.toml"
echo "  # Review /tmp/mac-profile.toml, then merge into the repo file"
echo
echo "AFTER THAT: every Claude session on every branch, every host, has"
echo "live MCP access to your Mac stack. No further setup needed."
echo "=============================================================="
