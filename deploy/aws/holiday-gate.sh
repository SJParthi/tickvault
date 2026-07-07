#!/bin/bash
# =============================================================================
# holiday-gate.sh — boot-time NSE-holiday self-stop gate
# =============================================================================
# Runs once per EC2 boot via the tickvault-holiday-gate.service oneshot unit,
# ORDERED BEFORE tickvault.service. The EventBridge cron starts the box every
# weekday (Mon-Fri), but NSE has weekday holidays (Republic Day, Diwali, etc.)
# on which a boot is a pure no-op that still bills ~8h. This gate asks the app's
# OWN binary (single source of truth — same config/base.toml NSE holiday list)
# whether today is a trading day, and self-stops the instance if not.
#
# FAIL-OPEN by design: the gate stops the box ONLY on a DEFINITIVE non-trading
# verdict (exit 75). Missing binary (first boot), config error (exit 70), IMDS
# failure, or any unexpected state → exit 0 → the app starts normally. A real
# trading day can NEVER be killed by a transient gate failure.
#
# Override: `touch /opt/tickvault/ALLOW_HOLIDAY_RUN` to run on a holiday/weekend
# without auto-stop (operator manual runs). See the may31 runbook §holiday-gate.
#
# Exit codes (consumed by the oneshot unit / journald):
#   0 = let the app start (trading day, override, or fail-open)
#   1 = holiday verdict, instance stop issued (app must NOT start)
# =============================================================================
set -uo pipefail

OVERRIDE_MARKER="/opt/tickvault/ALLOW_HOLIDAY_RUN"
BIN="/opt/tickvault/bin/tickvault"
WORKDIR="/opt/tickvault"
IMDS="http://169.254.169.254/latest"

log() { echo "holiday-gate: $*"; } # journald-captured (oneshot StandardOutput=journal)

# --- Override: operator wants to work on a holiday -> never stop. ---
if [ -f "$OVERRIDE_MARKER" ]; then
  log "override marker present ($OVERRIDE_MARKER) — skipping holiday check, app starts"
  exit 0
fi

# --- Fail-open: no binary yet (first boot before deploy drops it). ---
if [ ! -x "$BIN" ]; then
  log "binary not present yet ($BIN) — fail-open, app start proceeds (first boot)"
  exit 0
fi

# --- Ask the app's own calendar. Exit: 0=trading 75=holiday 70=load-error. ---
cd "$WORKDIR" || { log "cd $WORKDIR failed — fail-open"; exit 0; }
"$BIN" --check-trading-day
rc=$?

if [ "$rc" -eq 0 ]; then
  log "trading day — app start proceeds"
  exit 0
fi
if [ "$rc" -ne 75 ]; then
  # 70 (config/calendar load error) or any unexpected code -> FAIL-OPEN.
  log "check-trading-day returned $rc (not a definitive holiday) — fail-open, app starts"
  exit 0
fi

# --- Definitive non-trading day: self-stop the instance. ---
log "NON-TRADING day verdict (exit 75) — stopping this instance"

# IMDSv2 (token-required). Any IMDS failure -> fail-open (never risk a stop
# without a verified instance-id).
TOKEN=$(curl -sS -m 5 -X PUT "$IMDS/api/token" \
  -H "X-aws-ec2-metadata-token-ttl-seconds: 120" 2>/dev/null) || TOKEN=""
if [ -z "$TOKEN" ]; then
  log "IMDSv2 token fetch failed — fail-open (cannot resolve instance-id safely)"
  exit 0
fi
IID=$(curl -sS -m 5 -H "X-aws-ec2-metadata-token: $TOKEN" \
  "$IMDS/meta-data/instance-id" 2>/dev/null) || IID=""
REGION=$(curl -sS -m 5 -H "X-aws-ec2-metadata-token: $TOKEN" \
  "$IMDS/meta-data/placement/region" 2>/dev/null) || REGION=""
if [ -z "$IID" ] || [ -z "$REGION" ]; then
  log "instance-id/region resolve failed — fail-open"
  exit 0
fi

# --- Intentional-stop marker (2026-07-07 round-3 review fix) ----------------
# The stop below leaves NO trace, and three holiday-blind actors would
# otherwise fight it all day: the start-watchdog 08:45 IST check self-starts
# a not-running box, aws-autopilot start-instances a stopped box every 15 min
# inside 08:30-16:30 IST, and the market-hours alarm-gate Lambda samples the
# instance state ONCE at ~09:20 IST. The resulting boot/stop war keeps the
# box up in 1-3 min bursts all holiday — and a burst that brackets the 09:20
# sample re-arms the breaching-on-missing alarms against a box this gate is
# about to stop (the exact holiday false page). Stamping today's IST date
# into this SSM param BEFORE the stop lets every restarter + the gate Lambda
# recognise the stop as intentional. Stale markers are harmless (consumers
# compare against TODAY's IST date). FAIL-OPEN: a failed put is logged and
# the stop still proceeds (status quo ante — the war resumes, nothing worse).
TV_ENV="${TV_ENVIRONMENT:-prod}"
MARKER_PARAM="/tickvault/${TV_ENV}/holiday-stop-date"
TODAY_IST=$(TZ='Asia/Kolkata' date +%F)
if aws ssm put-parameter --region "$REGION" --name "$MARKER_PARAM" \
    --type String --value "$TODAY_IST" --overwrite >/dev/null 2>&1; then
  log "holiday-stop marker written: $MARKER_PARAM = $TODAY_IST"
else
  log "holiday-stop marker put FAILED ($MARKER_PARAM) — restarters may fight today's stop"
fi

log "stopping instance $IID in $REGION (NSE holiday — saves ~8h of billing)"
aws ec2 stop-instances --region "$REGION" --instance-ids "$IID" >/dev/null 2>&1 \
  || log "aws ec2 stop-instances call failed (will retry on next boot)"

# Abort the app start — the OS is going down.
exit 1
