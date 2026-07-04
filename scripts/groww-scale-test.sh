#!/bin/bash
# groww-scale-test.sh — §34 Groww multi-connection auto-scale test harness
# (Mac-first, operator authorization 2026-07-03; plan Item 10 + addenda).
#
# Everything runs on THIS machine. AWS prod (the r8g.large box) is NOT
# touched: no SSH, no terraform, no deploy — the ONLY AWS interaction is
# the READ-ONLY SSM GetParameter for the Groww token (token-minter lock).
#
# Modes (first argument, default "ladder"):
#   ladder    — full auto-scale ladder (rungs from config, default [1,2,5,10])
#   probe     — 2 x 600 cap-probe: per-conn-vs-per-account verdict, hold at 2
#   smoke     — weekend SMOKE: market closed, machinery-validated verdict,
#               tick gates honestly SKIPPED (never a live validation)
#   max       — 100K MAX-SCALE LAB (local-runtime branch, operator 2026-07-04):
#               ladder [1,2,5,10,20,40,80,100] x 1000/conn over the FULL Groww
#               master; purely Groww (Dhan stays OFF). Tuned for the operator's
#               M4 Pro (14 cores / 48 GB): memory watermark 75% (~36 GB).
#   max-smoke — the max config with weekend SMOKE (machinery-only dry run)
#   clean     — remove the harness overlay block from config/local.toml
#
# DRY_RUN=1 — write/remove the overlay and print the plan WITHOUT starting
#             Docker or the app (sandbox/harness validation).
#
# Config mechanism: the app merges config/base.toml -> config/local.toml.
# This script manages ONE marker-delimited block in config/local.toml
# (idempotent add/replace; `clean` removes it). Operator content outside
# the markers is never touched.

# ── Pure helpers (testable: `SCALE_TEST_LIB=1 source scripts/groww-scale-test.sh`) ──

# valid_gate_hold_min <value> — rc 0 when <value> is a plain integer in
# [1, 1440] minutes (1 min floor: the ladder evaluates every 30s; 1440 =
# 24h ceiling — anything longer is a config mistake, not a hold).
valid_gate_hold_min() {
  case "${1:-}" in '' | *[!0-9]*) return 1 ;; esac
  [ "$1" -ge 1 ] && [ "$1" -le 1440 ]
}

if [ "${SCALE_TEST_LIB:-0}" = "1" ]; then
  return 0 2>/dev/null || exit 0
fi

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

# The local-runtime branch overlay pins its own `[feeds]` table in
# config/local.toml (Dhan + Groww ON for the normal local window). TOML
# forbids a duplicate `[feeds]` table, so the harness cannot append its own —
# instead it overrides individual keys IN PLACE with a restorable marker
# (`# scale-override(was: <old>)`); `clean` restores the original values.

restore_overrides() {
  if [ -f "$LOCAL_TOML" ] && grep -qF "# scale-override(was: " "$LOCAL_TOML"; then
    sed -E 's|^([A-Za-z_]+) = .* # scale-override\(was: (.*)\)$|\1 = \2|' \
      "$LOCAL_TOML" > "$LOCAL_TOML.tmp"
    mv "$LOCAL_TOML.tmp" "$LOCAL_TOML"
  fi
}

# apply_override <key> <value> — rewrite `key = old` → `key = value
# # scale-override(was: old)` (first match). Returns 1 if the key is absent
# (caller then emits it inside the managed block instead).
apply_override() {
  local key="$1" want="$2"
  grep -qE "^${key} = " "$LOCAL_TOML" || return 1
  awk -v k="$key" -v w="$want" '
    !done && $1 == k && $2 == "=" {
      old = $3
      if (old == w) { print; done = 1; next }
      print k " = " w " # scale-override(was: " old ")"
      done = 1
      next
    }
    { print }
  ' "$LOCAL_TOML" > "$LOCAL_TOML.tmp"
  mv "$LOCAL_TOML.tmp" "$LOCAL_TOML"
  return 0
}

if [ "$MODE" = "clean" ]; then
  strip_block
  restore_overrides
  echo "harness overlay removed from $LOCAL_TOML (operator content untouched)"
  exit 0
fi

case "$MODE" in
  ladder)    PROBE=false; SMOKE=false; MAX=false ;;
  probe)     PROBE=true;  SMOKE=false; MAX=false ;;
  smoke)     PROBE=false; SMOKE=true;  MAX=false ;;
  max)       PROBE=false; SMOKE=false; MAX=true ;;
  max-smoke) PROBE=false; SMOKE=true;  MAX=true ;;
  *) echo "usage: $0 [ladder|probe|smoke|max|max-smoke|clean]" >&2; exit 2 ;;
esac

banner
if [ "$MAX" = true ]; then
  echo " MAX-SCALE LAB: full Groww master (~100K), ladder to 100 conns."
  echo " PURELY GROWW (operator 2026-07-04) — Dhan stays OFF."
  echo " Auto-abort gates: mem>75% of host RAM, CPU>70%, disk<20% free."
  echo "============================================================"
fi

# ── 1. Write the managed overlay block (append after stripping any old one).
#      Idempotent: strip the old block + restore any prior key overrides
#      first, so re-runs always start from the pristine operator content.
strip_block
restore_overrides
touch "$LOCAL_TOML"

# Feeds toggles: PURELY GROWW during scale runs (operator 2026-07-04 — Dhan
# stays OFF). If the file already has an unmanaged `[feeds]` table (the
# local-runtime branch overlay pins both feeds ON), override its keys in
# place; otherwise emit a `[feeds]` table inside the managed block.
NEED_FEEDS_TABLE=false
if grep -qE '^\[feeds\]$' "$LOCAL_TOML"; then
  apply_override "dhan_enabled" "false" || true
  apply_override "groww_enabled" "true" || true
  echo "feeds toggles overridden in place (dhan OFF, groww ON) — restored by 'clean'"
else
  NEED_FEEDS_TABLE=true
fi

{
  echo "$BEGIN_MARK"
  echo "# Auto-generated $(date '+%Y-%m-%d %H:%M:%S %Z') — run 'make scale-test-clean' to remove."
  if [ "$NEED_FEEDS_TABLE" = true ]; then
    echo "[feeds]"
    echo "dhan_enabled = false"
    echo "groww_enabled = true"
  fi
  echo "[feeds.groww.scale]"
  echo "enabled = true"
  echo "probe_mode = ${PROBE}"
  echo "weekend_smoke = ${SMOKE}"
  if [ "$MAX" = true ]; then
    # 100K max-scale lab (local-runtime branch ONLY; operator 2026-07-04).
    # Rungs climb toward 100 conns x 1000/conn = 100K; the ladder's own
    # effective_ceiling clamps to what the master actually provides
    # (subscribe what exists, record actual). Tuned for the operator's
    # MacBook Pro M4 Pro — 14 cores / 48 GB: mem watermark 75% (~36 GB
    # used) leaves macOS headroom; CPU gate 70% of 14 cores; disk stays
    # %-based (20% free floor).
    echo "full_master_universe = true"
    echo "target_connections = 100"
    echo "instruments_per_conn = 1000"
    echo "ladder = [1, 2, 5, 10, 20, 40, 80, 100]"
    echo "gate_max_mem_used_pct = 75.0"
    # Optional hold override for fast weekend probes (minutes). Unset ⇒
    # byte-identical overlay (config default gate_hold_minutes applies).
    # `gate_hold_minutes` is a real GrowwScaleConfig key with a serde
    # default, so emitting it here is legal TOML.
    if [ -n "${GATE_HOLD_MIN:-}" ]; then
      if valid_gate_hold_min "${GATE_HOLD_MIN}"; then
        echo "gate_hold_minutes = ${GATE_HOLD_MIN}"
      else
        echo "GATE_HOLD_MIN='${GATE_HOLD_MIN}' invalid (integer minutes 1..1440) — keeping the config default hold" >&2
      fi
    fi
  fi
  echo "$END_MARK"
} >> "$LOCAL_TOML"
echo "overlay written to $LOCAL_TOML (mode=${MODE})"

if [ "${DRY_RUN:-0}" = "1" ]; then
  echo "DRY_RUN=1 — overlay written; skipping Docker + app launch."
  echo "----- key overrides (restored by clean) -----"
  grep -F "# scale-override(was: " "$LOCAL_TOML" || echo "(none — feeds table emitted in managed block)"
  echo "----- managed overlay block -----"
  awk -v b="$BEGIN_MARK" -v e="$END_MARK" '$0==b{show=1} show{print} $0==e{show=0}' "$LOCAL_TOML"
  echo "---------------------------------"
  exit 0
fi

# ── 2. Local QuestDB up (standard compose file; app boot re-checks readiness
#      and the in-app preflight gates the fleet on ILP/HTTP reachability).
#      Max modes raise the QuestDB container memory limit to 8g — the 48 GB
#      M4 Pro host absorbs the ~100K-SID write pressure comfortably.
if [ "$MAX" = true ]; then
  export QDB_MEM_LIMIT="${QDB_MEM_LIMIT:-8g}"
  echo "QuestDB container memory limit: ${QDB_MEM_LIMIT} (max-scale lab)"
fi
echo "starting local QuestDB container..."
docker compose -f deploy/docker/docker-compose.yml up -d tv-questdb
bash scripts/questdb-init.sh || true

# ── 3. Run the app. The in-app preflight prints the check results and the
#      PROD-IS-UNTOUCHED banner; on any FAIL the fleet does NOT spawn (the
#      single-connection Groww path runs instead — capture continues).
echo "launching tickvault (scale mode=${MODE})..."
echo "watch:  GET http://127.0.0.1:3001/api/feeds/health  -> groww_scale.connections[]"
echo "stop:   Ctrl-C, then 'make scale-test-clean' to remove the overlay"
exec cargo run --release --bin tickvault
