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

# scale_probe_verdict <hit_ceiling 0|1> <max_conns> <target> <real_subscribed>
# — one plain-English line naming where the ladder stopped. rc 1 on garbage
# args. FIX 10 A2: in weekend smoke the gates hardcode the tick checks, so
# "held the rung" alone can be hollow — the verdict demands REAL per-conn
# subscribe proof (the summary TSV subscribe_proof column) and never prints
# ✅ unless real_subscribed >= target.
scale_probe_verdict() {
  local hit="${1:-}" max="${2:-}" target="${3:-}" proof="${4:-}"
  case "$hit" in 0 | 1) ;; *) return 1 ;; esac
  local v
  for v in "$max" "$target" "$proof"; do
    case "$v" in '' | *[!0-9]*) return 1 ;; esac
  done
  if [ "$hit" = "1" ] && [ "$proof" -ge "$target" ]; then
    echo "Reached ${target} connections ✅ — ${proof} of ${target} REALLY subscribed (SMOKE: connect + auth + subscribe only, no ticks on a closed market)."
  elif [ "$hit" = "1" ] && [ "$max" -ge "$target" ]; then
    echo "Held rung ${target}, but only ${proof} of ${target} connections REALLY subscribed — NOT a full pass. Record the real number."
  elif [ "$max" -gt 0 ]; then
    echo "Groww (or this Mac) capped us at ${max} of ${target} connections (${proof} REALLY subscribed) — that IS the probe answer. Record rung ${max}."
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
# A1: the app writes the summary TSV under the IST date (its rows are IST
# timestamps) — the wrapper must use the SAME calendar, not host-local.
TSV="data/groww-scale/summary-$(TZ=Asia/Kolkata date +%F).tsv"

# A1: run-start sentinel — the TSV is APPEND-per-day, so rows from any
# EARLIER scale run today must never feed this run's watcher/verdict (a
# stale halted_at_ceiling row would kill the probe ~3 min in and print a
# FALSE "Reached 100"). Only rows appended after this line count exist.
# shellcheck source=scripts/local-autopilot.sh
AUTOPILOT_LIB=1 source scripts/local-autopilot.sh
TSV_SENTINEL=$(tsv_sentinel "$(wc -l <"$TSV" 2>/dev/null || echo 0)")

probe_new_rows() { # rows appended AFTER this probe started (may be empty)
  tail -n +"$((TSV_SENTINEL + 1))" "$TSV" 2>/dev/null || true
}

verdict_from_tsv() {
  local hit=0 max=0 proof=0 ts outcome conns target ceiling sp _rest
  while IFS=$'\t' read -r ts outcome conns target ceiling sp _rest; do
    [ -z "$ts" ] && continue
    [ "$ts" = "ts_ist" ] && continue
    case "$outcome" in *halted_at_ceiling*) hit=1 ;; esac
    case "$conns" in
      '' | *[!0-9]*) : ;;
      *) [ "$conns" -gt "$max" ] && max="$conns" ;;
    esac
    case "$sp" in
      '' | *[!0-9]*) : ;;
      *) [ "$sp" -gt "$proof" ] && proof="$sp" ;;
    esac
  done < <(probe_new_rows)
  scale_probe_verdict "$hit" "$max" "$TARGET_CONNS" "$proof"
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
# A5: local-stop is pidfile-based — ALSO stop any tickvault release binary
# still alive without a pidfile (stale/foreign session), or the probe app
# port-clashes into a false probe.
if pgrep -f 'target/release/tickvault' >/dev/null 2>&1; then
  echo "a tickvault app is still running without a pidfile — stopping it before the probe"
  pkill -TERM -f 'target/release/tickvault' 2>/dev/null || true
  for _ in $(seq 1 15); do
    pgrep -f 'target/release/tickvault' >/dev/null 2>&1 || break
    sleep 2
  done
  if pgrep -f 'target/release/tickvault' >/dev/null 2>&1; then
    pkill -KILL -f 'target/release/tickvault' 2>/dev/null || true
  fi
fi

# 2. FIX-5 memory clamp reuse: respect the Docker VM size before max-smoke
#    pins QDB_MEM_LIMIT=8g. groww-scale-test.sh honors a pre-set value.
if [ -z "${QDB_MEM_LIMIT:-}" ]; then
  # (pure helpers already sourced above — clamp_qdb_mem_g is available)
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

# 3. The probe itself — foreground, output streams here.
#    Interactive (default): Ctrl-C to stop holding at the ceiling; cleanup
#    then runs automatically.
#    PROBE_AUTO_STOP_MIN=<minutes> (FIX 9 — the one-shot Run-click path):
#    the probe ENDS BY ITSELF — ~3 minutes after the ladder records
#    halted_at_ceiling in the summary TSV, or at the hard time cap — so the
#    launcher can continue into the normal live start with zero extra clicks.
if [ -n "${PROBE_AUTO_STOP_MIN:-}" ]; then
  case "$PROBE_AUTO_STOP_MIN" in '' | *[!0-9]*) PROBE_AUTO_STOP_MIN=45 ;; esac
  echo "auto-stop armed: probe ends ~3 min after reaching the ceiling (hard cap ${PROBE_AUTO_STOP_MIN} min)"
  make scale-100-probe &
  probe_pid=$!
  deadline=$((SECONDS + PROBE_AUTO_STOP_MIN * 60))
  ceiling_hold_until=0
  while kill -0 "$probe_pid" 2>/dev/null; do
    if ((SECONDS >= deadline)); then
      echo "hard time cap reached — stopping the probe"
      break
    fi
    if ((ceiling_hold_until == 0)) && probe_new_rows | grep -q "halted_at_ceiling"; then
      echo "ceiling reached — holding 3 more minutes, then stopping automatically"
      ceiling_hold_until=$((SECONDS + 180))
    fi
    if ((ceiling_hold_until > 0)) && ((SECONDS >= ceiling_hold_until)); then break; fi
    sleep 10
  done
  if kill -0 "$probe_pid" 2>/dev/null; then
    echo "stopping the probe app (graceful)..."
    pkill -TERM -f 'target/release/tickvault' 2>/dev/null || true
    for _ in $(seq 1 30); do
      kill -0 "$probe_pid" 2>/dev/null || break
      sleep 2
    done
    if kill -0 "$probe_pid" 2>/dev/null; then
      pkill -KILL -f 'target/release/tickvault' 2>/dev/null || true
    fi
  fi
  wait "$probe_pid" 2>/dev/null || true
else
  make scale-100-probe
fi
