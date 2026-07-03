#!/bin/bash
# groww-scale-test.sh — §34 Groww multi-connection auto-scale test harness
# (Mac-first, operator authorization 2026-07-03; plan Item 10 + addenda).
#
# Everything runs on THIS machine. AWS prod (the r8g.large box) is NOT
# touched: no SSH, no terraform, no deploy — the ONLY AWS interaction is
# the READ-ONLY SSM GetParameter for the Groww token (token-minter lock).
#
# Modes (first argument, default "ladder"):
#   ladder  — full auto-scale ladder (rungs from config, default [1,2,5,10])
#   probe   — 2 x 600 cap-probe: per-conn-vs-per-account verdict, hold at 2
#   smoke   — weekend SMOKE: market closed, machinery-validated verdict,
#             tick gates honestly SKIPPED (never a live validation)
#   clean   — remove the harness overlay block from config/local.toml
#
# Config mechanism: the app merges config/base.toml -> config/local.toml.
# This script manages ONE marker-delimited block in config/local.toml
# (idempotent add/replace; `clean` removes it). Operator content outside
# the markers is never touched.

set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-ladder}"
LOCAL_TOML="config/local.toml"
BEGIN_MARK="# >>> groww-scale-test (managed by scripts/groww-scale-test.sh) >>>"
END_MARK="# <<< groww-scale-test <<<"

banner() {
  echo "============================================================"
  echo " GROWW AUTO-SCALE TEST — mode: ${MODE}"
  echo " PROD IS UNTOUCHED: local QuestDB + local app only."
  echo " Only AWS call = read-only SSM token read (minter lock)."
  echo "============================================================"
}

strip_block() {
  # Remove any existing managed block (idempotent).
  if [ -f "$LOCAL_TOML" ] && grep -qF "$BEGIN_MARK" "$LOCAL_TOML"; then
    awk -v b="$BEGIN_MARK" -v e="$END_MARK" '
      $0 == b {skip=1; next}
      $0 == e {skip=0; next}
      !skip {print}
    ' "$LOCAL_TOML" > "$LOCAL_TOML.tmp"
    mv "$LOCAL_TOML.tmp" "$LOCAL_TOML"
  fi
}

if [ "$MODE" = "clean" ]; then
  strip_block
  echo "harness overlay removed from $LOCAL_TOML (operator content untouched)"
  exit 0
fi

case "$MODE" in
  ladder) PROBE=false; SMOKE=false ;;
  probe)  PROBE=true;  SMOKE=false ;;
  smoke)  PROBE=false; SMOKE=true ;;
  *) echo "usage: $0 [ladder|probe|smoke|clean]" >&2; exit 2 ;;
esac

banner

# ── 1. Write the managed overlay block (append after stripping any old one).
strip_block
touch "$LOCAL_TOML"
{
  echo "$BEGIN_MARK"
  echo "# Auto-generated $(date '+%Y-%m-%d %H:%M:%S %Z') — run 'make scale-test-clean' to remove."
  echo "[feeds]"
  echo "dhan_enabled = false"
  echo "groww_enabled = true"
  echo "[feeds.groww.scale]"
  echo "enabled = true"
  echo "probe_mode = ${PROBE}"
  echo "weekend_smoke = ${SMOKE}"
  echo "$END_MARK"
} >> "$LOCAL_TOML"
echo "overlay written to $LOCAL_TOML (mode=${MODE})"

# ── 2. Local QuestDB up (standard compose file; app boot re-checks readiness
#      and the in-app preflight gates the fleet on ILP/HTTP reachability).
echo "starting local QuestDB container..."
docker compose -f deploy/docker/docker-compose.yml up -d tv-questdb
bash scripts/questdb-init.sh || true

# ── 3. Run the app. The in-app preflight prints the check results and the
#      PROD-IS-UNTOUCHED banner; on any FAIL the fleet does NOT spawn (the
#      single-connection Groww path runs instead — capture continues).
echo "launching tickvault (scale mode=${MODE})..."
echo "watch:  GET http://127.0.0.1:3001/api/feeds/health  -> groww_scale.connections[]"
echo "stop:   Ctrl-C, then 'make scale-test-clean' to remove the overlay"
exec cargo run --release
