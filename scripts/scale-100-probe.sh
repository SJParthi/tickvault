#!/bin/bash
# =============================================================================
# scale-100-probe.sh — ONE-CLICK Saturday 100-connection Groww probe (SMOKE).
#
# What one click does, in order:
#   1. STOP any running normal app cleanly (make local-stop semantics).
#      The scale binary needs the app ports AND the scale OVERLAY config —
#      adopting an already-running normal-config app would be a FALSE probe.
#      local-stop also writes the manual-stop marker, so the autopilot will
#      not relaunch a normal app underneath the probe.
#   2. Clamp QuestDB container memory to the Docker VM (FIX-5 clamp reuse):
#      max-smoke defaults QDB_MEM_LIMIT=8g; on a 7 GB Docker VM that would
#      fail — we pre-export (vm_mem − 1 GB) instead and say so.
#   3. Run `make scale-100-probe` in the FOREGROUND — the ladder output
#      streams live in this window (max-smoke overlay: Dhan OFF, Groww ON,
#      weekend_smoke=true, ladder to 100 conns, 3-minute holds).
#   4. On ANY exit (success / failure / Ctrl-C): remove the config overlay
#      (scale-test-clean) so the next "Run tickvault" click is normal again,
#      and print a plain-English verdict of where the ladder stopped plus a
#      pointer to the per-rung evidence TSV.
#
# Testing: `SCALE_PROBE_LIB=1 source scripts/scale-100-probe.sh` loads ONLY
# the pure helpers below (no stop, no docker, no app launch).
# =============================================================================

# ── Pure helpers (testable) ─────────────────────────────────────────────────

# scale_probe_verdict <hit_ceiling 0|1> <max_conns> <target> — one
# plain-English line naming where the ladder stopped. rc 1 on garbage args.
scale_probe_verdict() {
  local hit="${1:-}" max="${2:-}" target="${3:-}"
  case "$hit" in 0 | 1) ;; *) return 1 ;; esac
  case "$max" in '' | *[!0-9]*) return 1 ;; esac
  case "$target" in '' | *[!0-9]*) return 1 ;; esac
  if [ "$hit" = "1" ] && [ "$max" -ge "$target" ]; then
    echo "Reached ${target} connections ✅ — Groww accepted the full fleet (SMOKE: connect + auth + subscribe only, no ticks on a closed market)."
  elif [ "$max" -gt 0 ]; then
    echo "Groww (or this Mac) capped us at ${max} of ${target} connections — that IS the probe answer. Record rung ${max}."
  else
    echo "No ladder stage was recorded — the probe never climbed. Read the app output above for the failure reason."
  fi
}

if [ "${SCALE_PROBE_LIB:-0}" = "1" ]; then
  return 0 2>/dev/null || exit 0
fi

set -uo pipefail
cd "$(dirname "$0")/.."

TARGET_CONNS=100
TSV="data/groww-scale/summary-$(date +%F).tsv"

verdict_from_tsv() {
  local hit=0 max=0 ts outcome conns _rest
  if [ -f "$TSV" ]; then
    while IFS=$'\t' read -r ts outcome conns _rest; do
      [ "$ts" = "ts_ist" ] && continue
      case "$outcome" in *halted_at_ceiling*) hit=1 ;; esac
      case "$conns" in
        '' | *[!0-9]*) : ;;
        *) [ "$conns" -gt "$max" ] && max="$conns" ;;
      esac
    done <"$TSV"
  fi
  scale_probe_verdict "$hit" "$max" "$TARGET_CONNS"
}

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  echo
  echo "── probe finished — cleaning up ─────────────────────────────────────"
  bash scripts/groww-scale-test.sh clean || true
  echo "Config overlay removed — the next 'Run tickvault' click is normal again."
  echo
  echo "VERDICT: $(verdict_from_tsv)"
  echo "Evidence (one row per ladder stage): ${TSV}"
  exit "$rc"
}
trap cleanup EXIT INT TERM

echo "============================================================"
echo " GROWW 100-CONNECTION PROBE (SMOKE — Saturday, no ticks)"
echo " Ladder 1→2→5→10→20→40→80→100, 3-minute holds (~25 min to 100)."
echo " Prod is untouched: local QuestDB + local app only."
echo "============================================================"

# 1. Stop any running normal app (make local-stop semantics — never adopt).
echo "stopping any running normal app first (scale run needs the overlay config)..."
make local-stop || true

# 2. FIX-5 memory clamp reuse: respect the Docker VM size before max-smoke
#    pins QDB_MEM_LIMIT=8g. groww-scale-test.sh honors a pre-set value.
if [ -z "${QDB_MEM_LIMIT:-}" ]; then
  # shellcheck source=scripts/local-autopilot.sh
  AUTOPILOT_LIB=1 source scripts/local-autopilot.sh
  vm_bytes=$(docker info --format '{{.MemTotal}}' 2>/dev/null || true)
  if clamped=$(clamp_qdb_mem_g "${vm_bytes:-}"); then
    clamped_g="${clamped%g}"
    if [ "$clamped_g" -lt 8 ]; then
      echo "Docker VM memory is small — clamping QuestDB to ${clamped} for this run"
      echo "(raise Docker Desktop memory to 10 GB for the full 8g QuestDB limit)."
      export QDB_MEM_LIMIT="$clamped"
    fi
  fi
fi

# 3. The probe itself — foreground, output streams here. Ctrl-C to stop
#    holding at the ceiling; cleanup then runs automatically.
make scale-100-probe
