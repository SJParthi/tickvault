#!/usr/bin/env bash
# =============================================================================
# tickvault — Benchmark Budget Gate
# =============================================================================
# Parses Criterion JSON output and enforces absolute latency budgets.
# Reads budgets from quality/benchmark-budgets.toml.
#
# Usage: bash scripts/bench-gate.sh [criterion-dir]
#
# Default criterion-dir: target/criterion
#
# Env:
#   BENCH_BUDGET_MULTIPLIER (float, default 1.0) — scales the ABSOLUTE ns
#   budgets from quality/benchmark-budgets.toml for slower runner classes
#   (the "runner-class multiplier" remediation documented in bench.yml's
#   header). The TOML values stay the dev-hardware truth and are NEVER
#   edited; the multiplier is the explicit, logged hardware adjustment.
#   It does NOT touch the regression arm (max_regression_pct is unchanged).
#
# Exit codes (consumed by .github/workflows/bench.yml baseline-ratchet logic):
#   0 = clean — all benchmarks within budget, no confident regression
#   1 = absolute-budget breach ONLY (per-element ns budget exceeded)
#   2 = regression breach (>max_regression_pct, CI-lower-bound confident) —
#       wins over 1 when both breach, so a regressed run can never be
#       mistaken for a mere hardware-calibration misfit.
#   3 = NOTHING WAS MEASURED (2026-08-24, fail-closed) — either no Criterion
#       estimates were found at all, or none of the benches found matched a
#       budget key, so ZERO absolute latency budgets were enforced. Distinct
#       from 1 and 2 so the bench.yml baseline-ratchet condition
#       (gate_rc != 2) is unchanged, while the run still fails loudly.
#       Both shapes previously exited 0 and printed a success line — the
#       false-OK class audit-findings Rule 11 forbids. Bite-proven in both
#       directions by scripts/bench-gate.selftest.sh cases H and I.
#
# Regression semantics: a benchmark FAILS the regression arm only when the
# LOWER bound of Criterion's 95% confidence interval for the mean change
# exceeds max_regression_pct — i.e. even the optimistic bound of the measured
# change is a >5% slowdown. Point-estimate-only deltas (cross-runner noise,
# statistically insignificant jitter) do NOT fail the gate.
#
# Hardware-drift suppression (added 2026-08-07 — the false-RED fix):
#   The CI-lower-bound rule above filters NOISE but not a systematic hardware
#   SHIFT: when GitHub rotates the shared runner pool, every benchmark gets
#   confidently slower against a baseline recorded on the previous hardware.
#   Observed live on main @993a144c: 31 of 31 benches "regressed" +24..30%
#   while EVERY absolute ns budget passed with 5-10x margin, and the identical
#   tree (da796888) had passed two days earlier.
#   The discriminator is SHAPE, not size: a code regression hits a few RELATED
#   benchmarks; a hardware shift hits nearly ALL of them by nearly the SAME
#   amount. When count >= HARDWARE_DRIFT_MIN_COUNT AND the regressing share
#   >= HARDWARE_DRIFT_MIN_SHARE AND the spread <= HARDWARE_DRIFT_SPREAD_PCT,
#   the relative arm is suppressed and every line is reported as DRIFT with an
#   explicit verdict. Absolute budgets are NEVER suppressed.
#   This is strictly MORE discriminating than widening max_regression_pct
#   (which would blind the gate to real 30% regressions) and than resetting the
#   baseline (which re-breaks on the next rotation).
#
# Spread re-calibration (2026-08-07, SAME DAY — the drift fix did not fire):
#   The suppressor above shipped with spread = max - min, and then FAILED to
#   suppress the very next rotation. Live on main @026c7b63 (bench run
#   31182757999 attempt 2): 31 of 31 benches regressed, share 100%, every
#   absolute budget passed — yet drift was NOT declared, because ONE outlier
#   (ws_reader_dispatch_ticker, a 13ns bench, +111.6%) dragged max-min to
#   88.0 pts against the 15 pt bar. The other 30 sat between +23.6% and
#   +42.9%. max-min is a WORST-CASE statistic: a single noisy sub-30ns
#   benchmark destroys it, which is exactly the population this gate measures.
#   Same commit, minutes earlier (attempt 1), only 1 of 31 "regressed" — the
#   two attempts differed by RUNNER, not by code (the merge touched zero files
#   under crates/core, where the attempt-1 bench lives).
#   Fix: spread is now the INTERQUARTILE RANGE (Q3-Q1) of the regressions —
#   a robust statistic that ignores the tails by construction. On the live
#   failing data IQR = 2.6 pts (vs max-min 88.0). The threshold, the count and
#   share rules, and the absolute arm are all UNCHANGED; only the estimator
#   changed. Case C of the self-test (8 benches genuinely fanned +6%..+80%,
#   IQR 24 pts) still FAILS, so the discriminating power is preserved, and the
#   full min..max range is still PRINTED for the operator.
# =============================================================================

set -euo pipefail

CRITERION_DIR="${1:-target/criterion}"
BUDGETS_FILE="quality/benchmark-budgets.toml"
BENCH_BUDGET_MULTIPLIER="${BENCH_BUDGET_MULTIPLIER:-1.0}"

# ---- hardware-drift thresholds (2026-08-07) ---------------------------------
# All three must hold before the relative regression arm may be suppressed.
# Deliberately NOT env-overridable: they are the gate's semantics, and an env
# knob would be a one-line way to silence a real regression in CI.
HARDWARE_DRIFT_MIN_COUNT=5      # never classify drift from a handful of benches
HARDWARE_DRIFT_MIN_SHARE=0.70   # >= 70% of compared benches must have regressed
HARDWARE_DRIFT_SPREAD_PCT=15    # INTERQUARTILE RANGE (Q3-Q1) of the regressions,
                                # in percentage points. Was max-min until
                                # 2026-08-07: one outlier sub-30ns bench could
                                # single-handedly veto a textbook drift shape
                                # (live: IQR 2.6 pts vs max-min 88.0 pts).

# Validate the multiplier early and loudly: digits and at most one dot only
# (rejects empty, negative, and non-numeric garbage before it reaches the evaluator).
case "$BENCH_BUDGET_MULTIPLIER" in
  ''|*[!0-9.]*|.|*.*.*)
    echo "ERROR: BENCH_BUDGET_MULTIPLIER must be a positive number, got '$BENCH_BUDGET_MULTIPLIER'" >&2
    exit 1
    ;;
esac

if [ ! -d "$CRITERION_DIR" ]; then
  echo "INFO: No criterion results found at $CRITERION_DIR — skipping bench gate."
  echo "  Run 'cargo bench --workspace' first to generate benchmark data."
  exit 0
fi

if [ ! -f "$BUDGETS_FILE" ]; then
  echo "ERROR: Budgets file not found: $BUDGETS_FILE" >&2
  exit 1
fi

echo "╔══════════════════════════════════════════════╗"
echo "║   Benchmark Budget Gate                       ║"
echo "╚══════════════════════════════════════════════╝"
echo ""

# Find all new/estimates.json files from Criterion
ESTIMATE_FILES=$(find "$CRITERION_DIR" -name "estimates.json" -path "*/new/*" 2>/dev/null || true)

if [ -z "$ESTIMATE_FILES" ]; then
  # FAIL-CLOSED (2026-08-24, audit): an empty Criterion tree means the benches
  # never ran or never produced output, so NO latency budget was enforced.
  # Exiting 0 here reported success from a measurement that did not happen —
  # the false-OK class audit-findings Rule 11 forbids. The sibling gate
  # scripts/coverage-gate.sh was fixed this way on 2026-07-10 (adversarial
  # review, HIGH) with self-test case 4; the fix was never propagated here.
  echo "ERROR: No benchmark estimates found under $CRITERION_DIR."
  echo "       ZERO latency budgets were enforced — this run proves nothing."
  echo "       Likely causes: cargo bench did not run, failed to build, or its"
  echo "       Criterion output was not restored into the expected directory."
  exit 3
fi


# =============================================================================
# Rust-only purge Phase 2a-2 (2026-07-18): the inline interpreter walker below
# was replaced by jq (per-file JSON extraction) + awk (TOML parse, budget
# matching, float comparisons, verdicts). Numeric semantics preserved:
# every comparison and every printed number is computed on IEEE-754 doubles
# exactly as the evaluator was (json floats, per-element division, budget x
# multiplier, pct x 100, printf %.0f / %+.1f / %g are the same C-double
# operations in both). Per-line output content and exit codes are
# unchanged; LINE ORDER is now deterministic (sorted paths, all absolute-
# budget lines before regression lines) where the legacy walker's order was
# filesystem-dependent.
# =============================================================================

RECORDS_FILE="$(mktemp)"
trap 'rm -f "$RECORDS_FILE"' EXIT

# N <tab> <rel-root> <tab> <median_ns>            (from */new/estimates.json)
# C <tab> <rel-root> <tab> <point> <tab> <lb|NA>  (from */change/estimates.json)
while IFS= read -r f; do
  [ -n "$f" ] || continue
  rel="${f#"$CRITERION_DIR"/}"
  rel="${rel%/estimates.json}"
  # FAIL CLOSED on a null/missing/non-numeric median (review round 1 fix,
  # 2026-07-18): the previous `.median.point_estimate // 0` turned an
  # explicit JSON null into 0ns — a corrupt estimates.json silently PASSED
  # every budget. The old evaluator raised TypeError there (exit 1); restore
  # that fail-closed contract with a named error instead of a stack trace.
  median=$(jq -r '(try .median.point_estimate catch null)
                  | if type == "number" then . else "CORRUPT" end' "$f")
  if [ "$median" = "CORRUPT" ] || [ -z "$median" ]; then
    echo "ERROR: $f: median.point_estimate is null/missing/non-numeric — corrupt Criterion output; refusing to treat it as 0ns (fail closed)" >&2
    exit 1
  fi
  printf 'N\t%s\t%s\n' "$rel" "$median" >> "$RECORDS_FILE"
done < <(printf '%s\n' "$ESTIMATE_FILES" | sort)

while IFS= read -r f; do
  [ -n "$f" ] || continue
  rel="${f#"$CRITERION_DIR"/}"
  rel="${rel%/estimates.json}"
  # mean.point_estimate defaults to 0 when absent; the CI lower bound is NA
  # when confidence_interval is absent, not an object, or lower_bound is
  # null (the evaluator's isinstance(ci, dict) + .get() semantics).
  # FAIL CLOSED on a PRESENT null/non-numeric mean.point_estimate or a
  # present NON-NULL non-numeric lower_bound (review round 2 fix,
  # 2026-07-18): the previous `// 0` / awk `+0` coerced them to 0, so a
  # real +30% regression with a corrupt lower_bound silently PASSED. The
  # old evaluator crashed there (TypeError, exit 1). Parity kept byte-
  # identical everywhere else: an ABSENT mean/point_estimate is still 0
  # (documented legacy-parity), and a null/absent lower_bound (or a
  # non-object confidence_interval) still takes the NA fallback.
  vals=$(jq -r '[ ((.mean // {}) as $m
                   | if ($m | type) == "object" and ($m | has("point_estimate"))
                     then ($m.point_estimate
                           | if type == "number" then . else "CORRUPT" end)
                     else 0 end),
                  ((.mean.confidence_interval // {}) as $ci
                   | if ($ci | type) == "object"
                     then ($ci.lower_bound as $lb
                           | if $lb == null then "NA"
                             elif ($lb | type) == "number" then ($lb | tostring)
                             else "CORRUPT" end)
                     else "NA" end) ] | @tsv' "$f")
  case "$vals" in
    *CORRUPT*)
      echo "ERROR: $f: mean.point_estimate or confidence_interval.lower_bound is present but non-numeric — corrupt Criterion output; refusing to coerce it to 0 (fail closed)" >&2
      exit 1
      ;;
  esac
  printf 'C\t%s\t%s\n' "$rel" "$vals" >> "$RECORDS_FILE"
done < <(find "$CRITERION_DIR" -name "estimates.json" -path "*/change/*" 2>/dev/null | sort)

LC_ALL=C awk -F'\t' \
  -v budgets_file="$BUDGETS_FILE" \
  -v multiplier_raw="$BENCH_BUDGET_MULTIPLIER" \
  -v drift_min_count="$HARDWARE_DRIFT_MIN_COUNT" \
  -v drift_min_share="$HARDWARE_DRIFT_MIN_SHARE" \
  -v drift_spread_pct="$HARDWARE_DRIFT_SPREAD_PCT" \
'
function trim(s) { sub(/^[ \t\r]+/, "", s); sub(/[ \t\r]+$/, "", s); return s }
# frepr: render a decimal string the way the legacy float repr did for the
# TOML-magnitude values in use ("5.0" -> "5.0", "5" -> "5.0") — display only.
function frepr(v,   s) {
  s = v
  if (s !~ /\./) return s ".0"
  sub(/0+$/, "", s)
  if (s ~ /\.$/) s = s "0"
  return s
}
function max(a, b) { return a > b ? a : b }
BEGIN {
  multiplier = multiplier_raw + 0
  if (multiplier <= 0) {
    printf "ERROR: BENCH_BUDGET_MULTIPLIER must be > 0, got %s\n", frepr(multiplier_raw) > "/dev/stderr"
    died = 1; exit 1
  }
  if (multiplier != 1.0) {
    printf "  NOTE: absolute budgets scaled x%g for shared-runner hardware (BENCH_BUDGET_MULTIPLIER=%g)\n", multiplier, multiplier
    print ""
  }
  max_regression = 5.0
  max_regression_raw = "5.0"
  nb = 0; ne = 0
}
# ---- budgets TOML (simple parser — [budgets] floats with inline comments
# ---- stripped, [elements] ints, max_regression_pct anywhere) ----
FILENAME == budgets_file {
  line = trim($0)
  if (line ~ /^max_regression_pct/) {
    eq = index(line, "=")
    if (eq == 0) { print "ERROR: malformed max_regression_pct line in " budgets_file > "/dev/stderr"; died = 1; exit 1 }
    max_regression_raw = trim(substr(line, eq + 1))
    if (max_regression_raw !~ /^[0-9]+(\.[0-9]+)?$/) {
      print "ERROR: max_regression_pct is not a plain decimal: " max_regression_raw > "/dev/stderr"; died = 1; exit 1
    }
    max_regression = max_regression_raw + 0
    next
  }
  if (line == "[budgets]")  { in_budgets = 1; in_elements = 0; next }
  if (line == "[elements]") { in_elements = 1; in_budgets = 0; next }
  if (line ~ /^\[/) { in_budgets = 0; in_elements = 0; next }
  if (in_budgets && index(line, "=") > 0 && line !~ /^#/) {
    eq = index(line, "=")
    key = trim(substr(line, 1, eq - 1))
    val = substr(line, eq + 1)
    sub(/#.*$/, "", val)          # strip inline comments
    val = trim(val)
    if (val !~ /^-?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?$/) {
      print "ERROR: [budgets] value is not a number: " key " = " val > "/dev/stderr"; died = 1; exit 1
    }
    if (key in bidx) { bval[bidx[key]] = val + 0 }
    else { bkey[++nb] = key; bval[nb] = val + 0; bidx[key] = nb }
    next
  }
  if (in_elements && index(line, "=") > 0 && line !~ /^#/) {
    eq = index(line, "=")
    key = trim(substr(line, 1, eq - 1))
    val = substr(line, eq + 1)
    sub(/#.*$/, "", val)
    val = trim(val)
    if (val !~ /^-?[0-9]+$/) {
      print "ERROR: [elements] value is not an integer: " key " = " val > "/dev/stderr"; died = 1; exit 1
    }
    if (key in eidx) { eval_[eidx[key]] = val + 0 }
    else { ekey[++ne] = key; eval_[ne] = val + 0; eidx[key] = ne }
    next
  }
  next
}
# ---- absolute-budget arm: N <rel-root> <median_ns> ----
$1 == "N" {
  bench = $2
  gsub(/\/new/, "", bench)        # legacy .replace("/new", "") — all occurrences
  gsub(/\//, "_", bench)
  bench = tolower(bench)
  median_ns = $3 + 0

  # Per-element normalization: budgets are PER-ELEMENT; batch benches divide
  # the batch median by their [elements] count (default 1). First TOML-order
  # key matching by bidirectional substring wins (same as before).
  n_elements = 1
  for (j = 1; j <= ne; j++) {
    if (index(bench, ekey[j]) > 0 || index(ekey[j], bench) > 0) {
      n_elements = max(1, eval_[j])
      break
    }
  }
  per_elem_ns = median_ns / n_elements

  budget_found = 0
  for (j = 1; j <= nb; j++) {
    if (index(bench, bkey[j]) > 0 || index(bkey[j], bench) > 0) {
      budget_ns = bval[j]; budget_found = 1
      break
    }
  }

  if (budget_found) {
    checked++
    unit = (n_elements > 1) ? sprintf(" (%.0fns / %d)", median_ns, n_elements) : ""
    effective_ns = budget_ns * multiplier
    if (multiplier != 1.0)
      budget_label = sprintf("%.0fns = %.0fns x%g", effective_ns, budget_ns, multiplier)
    else
      budget_label = sprintf("%.0fns", budget_ns)
    if (per_elem_ns > effective_ns) {
      printf "  FAIL: %s: %.0fns%s (budget: %s)\n", bench, per_elem_ns, unit, budget_label
      abs_failed = 1
    } else {
      printf "  PASS: %s: %.0fns%s (budget: %s)\n", bench, per_elem_ns, unit, budget_label
    }
  } else {
    printf "  INFO: %s: %.0fns (no budget defined — skipping)\n", bench, median_ns
  }
  next
}
# ---- regression arm: C <rel-root> <point> <lb|NA> — fail only when the 95%
# ---- CI LOWER bound exceeds max_regression_pct (confident regression) ----
$1 == "C" {
  bench = $2
  gsub(/\/change/, "", bench)
  gsub(/\//, "_", bench)
  bench = tolower(bench)
  point_pct = ($3 + 0) * 100
  lb = $4
  cmp_total++
  if (lb != "NA") {
    lower_pct = (lb + 0) * 100
    if (lower_pct > max_regression) {
      reg_n++
      reg_bench[reg_n] = bench; reg_point[reg_n] = point_pct
      reg_lower[reg_n] = lower_pct; reg_kind[reg_n] = "ci"
    }
  } else {
    printf "  NOTE: %s: change/estimates.json has no confidence_interval — falling back to point estimate\n", bench
    if (point_pct > max_regression) {
      reg_n++
      reg_bench[reg_n] = bench; reg_point[reg_n] = point_pct
      reg_lower[reg_n] = point_pct; reg_kind[reg_n] = "point"
    }
  }
  next
}
END {
  if (died) exit 1
  # FAIL-CLOSED (2026-08-24, audit): checked counts benches that matched a
  # budget key. At zero, NO absolute latency budget was enforced, so the run
  # proves nothing. The pre-fix behaviour printed an INFO line and fell
  # through to exit 0 — a green verdict from an empty comparison. Exit 3 is
  # deliberately distinct from 1 (absolute breach) and 2 (regression), so the
  # bench.yml baseline-ratchet condition (gate_rc != 2) is unchanged while the
  # run still fails loudly.
  if (checked == 0) {
    print "  ERROR: No benchmark matched any budget key in quality/benchmark-budgets.toml."
    print "         ZERO absolute latency budgets were enforced — this run proves nothing."
    print "         Likely cause: a Criterion bench id drifted away from its budget key."
    exit 3
  }

  # ---- hardware-drift classification (2026-08-07) --------------------------
  # A CODE regression hits a FEW RELATED benchmarks. A runner/toolchain change
  # hits ALL of them by roughly the SAME amount. Declare drift only when all
  # three hold: enough samples to be meaningful, nearly every compared bench
  # regressed, and the regressions cluster tightly. Fail-safe in every other
  # case -> the pre-2026-08-07 behaviour is preserved exactly.
  drift = 0; share = 0; spread = 0; full_range = 0; lo = 0; hi = 0
  if (reg_n > 0) {
    if (cmp_total > 0) share = reg_n / cmp_total
    # Robust spread = interquartile range. max-min was vetoed by a single
    # noisy sub-30ns bench on 2026-08-07 (see the header note); the IQR
    # ignores both tails by construction. Insertion sort — reg_n is small
    # (tens), and asort() is a gawk extension the CI runner awk may lack.
    # NOTE: no apostrophes inside this awk program — it is single-quoted in
    # the surrounding shell, so one would terminate the program early.
    for (i = 1; i <= reg_n; i++) sorted[i] = reg_point[i]
    for (i = 2; i <= reg_n; i++) {
      v = sorted[i]; j = i - 1
      while (j >= 1 && sorted[j] > v) { sorted[j + 1] = sorted[j]; j-- }
      sorted[j + 1] = v
    }
    lo = sorted[1]; hi = sorted[reg_n]
    full_range = hi - lo
    q = int(reg_n / 4)
    spread = sorted[reg_n - q] - sorted[q + 1]
    if (reg_n >= drift_min_count && share >= drift_min_share && spread <= drift_spread_pct)
      drift = 1
  }
  for (i = 1; i <= reg_n; i++) {
    if (drift) {
      printf "  DRIFT: %s: %+.1f%% (uniform shift — suppressed, see verdict)\n", reg_bench[i], reg_point[i]
    } else if (reg_kind[i] == "ci") {
      printf "  FAIL: %s: %+.1f%% regression (CI lower bound %+.1f%% > %s%%)\n", reg_bench[i], reg_point[i], reg_lower[i], frepr(max_regression_raw)
      reg_failed = 1
    } else {
      printf "  FAIL: %s: %+.1f%% regression (point estimate, no CI available; max: %s%%)\n", reg_bench[i], reg_point[i], frepr(max_regression_raw)
      reg_failed = 1
    }
  }
  if (drift) {
    printf "\n  HARDWARE DRIFT DETECTED: %d of %d compared benchmarks regressed (%.0f%% >= %.0f%%), IQR %.1f pts <= %.1f (full range %+.1f%%..%+.1f%% = %.1f pts).\n", \
      reg_n, cmp_total, share * 100, drift_min_share * 100, spread, drift_spread_pct, lo, hi, full_range
    print "  A code regression hits a few RELATED benchmarks; a uniform shift across nearly ALL of them is the runner, not the code."
    print "  The RELATIVE regression arm is SUPPRESSED for this run."
    print "  NOT suppressed: the absolute ns budgets above, which still gate and still fail this run if breached."
    print "  Honest limit: a genuine broad regression coinciding with a runner rotation is indistinguishable from relative data alone."
  }
  print ""
  if (reg_failed) {
    print "  Benchmark budget gate FAILED (confident regression)."
    exit 2
  } else if (abs_failed) {
    print "  Benchmark budget gate FAILED (absolute budget breach)."
    exit 1
  } else {
    print "  All benchmarks within budget."
    exit 0
  }
}
' "$BUDGETS_FILE" "$RECORDS_FILE"

exit $?
