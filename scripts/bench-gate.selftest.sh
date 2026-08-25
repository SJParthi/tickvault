#!/usr/bin/env bash
# =============================================================================
# tickvault — bench-gate.sh self-test (hardware-drift classification)
# =============================================================================
# Builds throwaway Criterion directory trees and asserts the gate's exit code
# for each. Proves BOTH directions:
#   * the real 2026-08-07 drift shape is suppressed (was a false RED), and
#   * a genuine regression still FAILS (the guarantee is not weakened).
#
# Usage: bash scripts/bench-gate.selftest.sh
# Exit: 0 = all cases behaved as specified, 1 = at least one case regressed.
# =============================================================================
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0
FAIL=0

# Match the real CI config: .github/workflows/bench.yml runs the gate with
# BENCH_BUDGET_MULTIPLIER=2.5 (the documented shared-runner hardware scaling).
# The case-A medians below are the REAL ns values observed on main @993a144c,
# so they must be judged against the REAL multiplier or the fixture would not
# reproduce the incident it exists to pin.
export BENCH_BUDGET_MULTIPLIER=2.5

# mk_bench <root> <name> <median_ns> <point_frac> <lower_frac>
#   Writes the two Criterion files the gate reads: the absolute-budget source
#   (<name>/new/estimates.json) and the relative-change source
#   (<name>/change/estimates.json).
mk_bench() {
  local root="$1" name="$2" median_ns="$3" point="$4" lower="$5"
  mkdir -p "$root/$name/new" "$root/$name/change"
  # new/estimates.json: the gate reads .median.point_estimate (absolute arm)
  printf '{"median":{"point_estimate":%s}}\n' \
    "$median_ns" > "$root/$name/new/estimates.json"
  printf '{"mean":{"point_estimate":%s,"confidence_interval":{"lower_bound":%s,"upper_bound":%s}}}\n' \
    "$point" "$lower" "$point" > "$root/$name/change/estimates.json"
}

# expect <label> <expected_exit> <criterion_dir>
expect() {
  local label="$1" want="$2" dir="$3"
  local out rc=0
  out="$(cd "$REPO_ROOT" && bash scripts/bench-gate.sh "$dir" 2>&1)" || rc=$?
  if [ "$rc" -eq "$want" ]; then
    printf '  ok   : %s (exit %d)\n' "$label" "$rc"
    PASS=$((PASS + 1))
  else
    printf '  FAIL : %s -> got exit %d, want %d\n' "$label" "$rc" "$want"
    printf '%s\n' "$out" | sed 's/^/         | /' | tail -20
    FAIL=$((FAIL + 1))
  fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- Case A: the REAL 2026-08-07 shape -------------------------------------
# 31 benches, all regressing +24%..+30% (spread 6 pts), every absolute value
# far inside budget. This exact shape produced the live false RED.
# Budget keys chosen so each maps to a real quality/benchmark-budgets.toml
# entry; medians are the real observed ns values.
A="$TMP/a"
i=0
for spec in \
  "registry/get_hit:14" "registry/get_miss:14" "oms/state_transition:38" \
  "calendar/is_trading_day:9" "config/toml_load:40324" "obi_compute_20_levels:371" \
  "obi_compute_empty_book:14" "obi_compute_with_wall:369" "dispatch_frame/ticker:16" \
  "dispatch_frame/quote:16" "token_handle_load:6" "token_handle_load_none:6" \
  "token_handle_clone_arc:5" "moneyness_classify:2" "moneyness_snapshot_read:7" \
  "pipeline/burst_100_ticker:641" "pipeline/batch_100_mixed:771" \
  "ws_reader_frame_handle_ns:153" "order_gate_mark_forward_disarmed:4" \
  "order_gate_mark_forward_armed_full:4" "order_gate_mark_forward_armed_accept:31" \
  "calendar/is_trading_day_holiday:10" "calendar/is_trading_day_weekend:1" \
  "extra_a:10" "extra_b:10" "extra_c:10" "extra_d:10" "extra_e:10" \
  "extra_f:10" "extra_g:10" "extra_h:10" ; do
  name="${spec%%:*}"; median="${spec##*:}"
  # point 0.240..0.300, lower bound just under it — all confidently regressed
  frac="0.$((240 + i * 2))"
  lower="0.$((235 + i * 2))"
  mk_bench "$A" "$name" "$median" "$frac" "$lower"
  i=$((i + 1))
done
expect "31/31 uniform +24..30% drift, all budgets OK -> PASS (was false RED)" 0 "$A"

# --- Case B: a genuine regression (few benches) -----------------------------
# Same 31 benches, but only 3 regress. Share 3/31 = 10% < 70% -> NOT drift.
B="$TMP/b"
i=0
for spec in \
  "registry/get_hit:14" "registry/get_miss:14" "oms/state_transition:38" \
  "calendar/is_trading_day:9" "config/toml_load:40324" "obi_compute_20_levels:371" \
  "obi_compute_empty_book:14" "obi_compute_with_wall:369" "dispatch_frame/ticker:16" \
  "dispatch_frame/quote:16" "token_handle_load:6" "token_handle_load_none:6" \
  "token_handle_clone_arc:5" "moneyness_classify:2" "moneyness_snapshot_read:7" \
  "pipeline/burst_100_ticker:641" "pipeline/batch_100_mixed:771" \
  "ws_reader_frame_handle_ns:153" "order_gate_mark_forward_disarmed:4" \
  "order_gate_mark_forward_armed_full:4" "order_gate_mark_forward_armed_accept:31" \
  "calendar/is_trading_day_holiday:10" "calendar/is_trading_day_weekend:1" \
  "extra_a:10" "extra_b:10" "extra_c:10" "extra_d:10" "extra_e:10" \
  "extra_f:10" "extra_g:10" "extra_h:10" ; do
  name="${spec%%:*}"; median="${spec##*:}"
  if [ "$i" -lt 3 ]; then frac="0.300"; lower="0.290"; else frac="0.001"; lower="-0.010"; fi
  mk_bench "$B" "$name" "$median" "$frac" "$lower"
  i=$((i + 1))
done
expect "3/31 regressing +30% -> still FAILS (guarantee intact)" 2 "$B"

# --- Case C: high share but WIDE spread -------------------------------------
# All 8 regress, but from +6% to +80% (spread 74 pts) -> not a uniform shift.
C="$TMP/c"
i=0
for name in registry/get_hit registry/get_miss oms/state_transition \
            calendar/is_trading_day obi_compute_20_levels obi_compute_empty_book \
            token_handle_load moneyness_classify ; do
  frac="0.$((60 + i * 100))"
  lower="0.$((55 + i * 100))"
  mk_bench "$C" "$name" 10 "$frac" "$lower"
  i=$((i + 1))
done
expect "8/8 regressing but spread ~74 pts -> still FAILS (not uniform)" 2 "$C"

# --- Case D: uniform but too FEW benches ------------------------------------
# 4 benches all +29% (below HARDWARE_DRIFT_MIN_COUNT=5) -> not enough evidence.
D="$TMP/d"
for name in registry/get_hit registry/get_miss oms/state_transition calendar/is_trading_day ; do
  mk_bench "$D" "$name" 10 "0.290" "0.285"
done
expect "4/4 uniform +29% but below min-count 5 -> still FAILS (fail-safe)" 2 "$D"

# --- Case E: drift shape WITH an absolute budget breach ---------------------
# Drift suppresses the RELATIVE arm only. registry/get_hit at 9999ns blows its
# 50ns budget, so the run must still fail (exit 1 = absolute breach).
E="$TMP/e"
i=0
for spec in \
  "registry/get_hit:9999" "registry/get_miss:14" "oms/state_transition:38" \
  "calendar/is_trading_day:9" "config/toml_load:40324" "obi_compute_20_levels:371" \
  "obi_compute_empty_book:14" "obi_compute_with_wall:369" "dispatch_frame/ticker:16" \
  "dispatch_frame/quote:16" ; do
  name="${spec%%:*}"; median="${spec##*:}"
  frac="0.$((250 + i * 2))"
  lower="0.$((245 + i * 2))"
  mk_bench "$E" "$name" "$median" "$frac" "$lower"
  i=$((i + 1))
done
expect "drift shape + absolute budget breach -> FAILS on absolute (not suppressed)" 1 "$E"

# --- Case F: clean run ------------------------------------------------------
F="$TMP/f"
for name in registry/get_hit registry/get_miss oms/state_transition ; do
  mk_bench "$F" "$name" 10 "0.001" "-0.020"
done
expect "no regressions, budgets OK -> PASS" 0 "$F"

# --- Case G: drift shape WITH one outlier (the 2026-08-07 second incident) ---
# The REAL shape of bench run 31182757999 attempt 2 on main @026c7b63:
# 31/31 regressed, 30 of them clustered, and ONE sub-30ns bench
# (ws_reader_dispatch_ticker) at +111.6%. Under the original max-min spread
# that single outlier produced 88.0 pts vs the 15 pt bar, so drift was NOT
# declared and the run went RED with every absolute budget passing. Under the
# IQR the tails are ignored and the shape is correctly suppressed.
# This case FAILS (exit 2) against the pre-IQR gate and PASSES against it.
G="$TMP/g"
i=0
for spec in \
  "ws_reader_dispatch_ticker:27" \
  "registry/get_hit:14" "registry/get_miss:14" "oms/state_transition:38" \
  "calendar/is_trading_day:9" "config/toml_load:40324" "obi_compute_20_levels:371" \
  "obi_compute_empty_book:14" "obi_compute_with_wall:369" "dispatch_frame/ticker:16" \
  "dispatch_frame/quote:16" "token_handle_load:6" "token_handle_load_none:6" \
  "token_handle_clone_arc:5" "moneyness_classify:2" "moneyness_snapshot_read:7" \
  "pipeline/burst_100_ticker:641" "pipeline/batch_100_mixed:771" \
  "ws_reader_frame_handle_ns:153" "order_gate_mark_forward_disarmed:4" \
  "order_gate_mark_forward_armed_full:4" "order_gate_mark_forward_armed_accept:31" \
  "calendar/is_trading_day_holiday:10" "calendar/is_trading_day_weekend:1" \
  "extra_a:10" "extra_b:10" "extra_c:10" "extra_d:10" "extra_e:10" \
  "extra_f:10" "extra_g:10" ; do
  name="${spec%%:*}"; median="${spec##*:}"
  if [ "$i" -eq 0 ]; then
    # the lone outlier: +111.6%, exactly as observed live
    frac="1.116"; lower="1.000"
  else
    # the clustered majority: +23.6% .. +43.9%
    frac="0.$((236 + (i - 1) * 7))"
    lower="0.$((230 + (i - 1) * 7))"
  fi
  mk_bench "$G" "$name" "$median" "$frac" "$lower"
  i=$((i + 1))
done
expect "31/31 drift + one +111.6% outlier -> PASS (IQR ignores tails; was false RED)" 0 "$G"

# --- Case H: benches present but NONE matches a budget key -> FAIL-CLOSED ----
# The 2026-08-24 audit finding. Criterion produced output, but every bench id
# drifted away from its budget key, so ZERO absolute budgets were enforced.
# Pre-fix this printed an INFO line and fell through to exit 0 — a green
# verdict from an empty comparison. Mirrors coverage-gate self-test case 4.
H="$TMP/h"
for name in zzz_unbudgeted_alpha zzz_unbudgeted_beta ; do
  mk_bench "$H" "$name" 10 "0.001" "-0.020"
done
expect "benches present, zero budget matches -> FAIL-CLOSED" 3 "$H"

# --- Case I: empty Criterion tree -> FAIL-CLOSED ----------------------------
# The benches-did-not-run / artifact-not-restored shape. Pre-fix this printed
# "No benchmark estimates found — skipping." and exited 0.
I_DIR="$TMP/i"
mkdir -p "$I_DIR"
expect "empty criterion dir -> FAIL-CLOSED" 3 "$I_DIR"

printf '  bench-gate self-test: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
