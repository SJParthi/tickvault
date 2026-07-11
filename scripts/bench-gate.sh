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
#
# Regression semantics: a benchmark FAILS the regression arm only when the
# LOWER bound of Criterion's 95% confidence interval for the mean change
# exceeds max_regression_pct — i.e. even the optimistic bound of the measured
# change is a >5% slowdown. Point-estimate-only deltas (cross-runner noise,
# statistically insignificant jitter) do NOT fail the gate.
# =============================================================================

set -euo pipefail

CRITERION_DIR="${1:-target/criterion}"
BUDGETS_FILE="quality/benchmark-budgets.toml"
BENCH_BUDGET_MULTIPLIER="${BENCH_BUDGET_MULTIPLIER:-1.0}"

# Validate the multiplier early and loudly: digits and at most one dot only
# (rejects empty, negative, and non-numeric garbage before it reaches Python).
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
  echo "INFO: No benchmark estimates found — skipping."
  exit 0
fi

python3 -c "
import json, sys, os, re

# Parse budgets TOML (simple parser)
budgets = {}
elements = {}
max_regression = 5.0
in_budgets = False
in_elements = False
with open('$BUDGETS_FILE') as f:
    for line in f:
        line = line.strip()
        if line.startswith('max_regression_pct'):
            max_regression = float(line.split('=', 1)[1].strip())
            continue
        if line == '[budgets]':
            in_budgets = True
            in_elements = False
            continue
        if line == '[elements]':
            in_elements = True
            in_budgets = False
            continue
        if line.startswith('[') and line not in ('[budgets]', '[elements]'):
            in_budgets = False
            in_elements = False
            continue
        if in_budgets and '=' in line and not line.startswith('#'):
            key, val = line.split('=', 1)
            # Strip inline comments
            val = val.split('#')[0].strip()
            budgets[key.strip()] = float(val)
        if in_elements and '=' in line and not line.startswith('#'):
            key, val = line.split('=', 1)
            val = val.split('#')[0].strip()
            elements[key.strip()] = int(val)

abs_failed = False   # absolute per-element ns budget breach -> exit 1
reg_failed = False   # confident (CI lower bound) regression breach -> exit 2
checked = 0

# Runner-class multiplier for ABSOLUTE budgets only (never the regression
# arm). Default 1.0 = dev-hardware budgets exactly as written in the TOML.
multiplier = float('$BENCH_BUDGET_MULTIPLIER')
if multiplier <= 0:
    print(f'ERROR: BENCH_BUDGET_MULTIPLIER must be > 0, got {multiplier}', file=sys.stderr)
    sys.exit(1)
if multiplier != 1.0:
    print(f'  NOTE: absolute budgets scaled x{multiplier:g} for shared-runner hardware (BENCH_BUDGET_MULTIPLIER={multiplier:g})')
    print('')

# Walk criterion results
criterion_dir = '$CRITERION_DIR'
for root, dirs, files in os.walk(criterion_dir):
    if 'estimates.json' in files and '/new/' in root + '/':
        est_path = os.path.join(root, 'estimates.json')
        with open(est_path) as f:
            data = json.load(f)

        # Extract benchmark name from path
        # Pattern: target/criterion/<bench_name>/new/estimates.json
        rel = os.path.relpath(root, criterion_dir)
        bench_name = rel.replace('/new', '').replace('/', '_').lower()

        # Get median estimate in nanoseconds
        median_ns = data.get('median', {}).get('point_estimate', 0)

        # Per-element normalization: batch benchmarks process N elements per
        # Criterion iteration (e.g. pipeline/batch_100_mixed runs 100 ticks per
        # measured iter). The budgets are PER-ELEMENT (e.g. pipeline < 100ns/
        # tick), so divide the batch median by its element count before
        # comparing. Benches not listed in [elements] default to 1 (no change).
        # Without this, the gate compared a 100-tick batch median against a
        # 1-tick budget — a unit bug that made the pipeline gate fail by
        # construction even though per-tick latency was ~20x under budget.
        n_elements = 1
        for key in elements:
            if key in bench_name or bench_name in key:
                n_elements = max(1, elements[key])
                break
        per_elem_ns = median_ns / n_elements

        # Normalize bench name for budget lookup
        # Try exact match, then underscore-normalized
        budget_ns = None
        for key in budgets:
            if key in bench_name or bench_name in key:
                budget_ns = budgets[key]
                break

        if budget_ns is not None:
            checked += 1
            unit = f' ({median_ns:.0f}ns / {n_elements})' if n_elements > 1 else ''
            effective_ns = budget_ns * multiplier
            budget_label = (f'{effective_ns:.0f}ns = {budget_ns:.0f}ns x{multiplier:g}'
                            if multiplier != 1.0 else f'{budget_ns:.0f}ns')
            if per_elem_ns > effective_ns:
                print(f'  FAIL: {bench_name}: {per_elem_ns:.0f}ns{unit} (budget: {budget_label})')
                abs_failed = True
            else:
                print(f'  PASS: {bench_name}: {per_elem_ns:.0f}ns{unit} (budget: {budget_label})')
        else:
            print(f'  INFO: {bench_name}: {median_ns:.0f}ns (no budget defined — skipping)')

    # Check for regression from change/estimates.json
    if 'estimates.json' in files and '/change/' in root + '/':
        est_path = os.path.join(root, 'estimates.json')
        with open(est_path) as f:
            data = json.load(f)

        rel = os.path.relpath(root, criterion_dir)
        bench_name = rel.replace('/change', '').replace('/', '_').lower()

        # Change values are fractions (0.05 = 5%). Criterion writes change/
        # even for statistically insignificant noise, and heterogeneous CI
        # runners produce 10-30% cross-machine point-estimate deltas — so the
        # gate uses the 95% confidence-interval LOWER bound: fail only when
        # even the optimistic bound of the measured change exceeds the budget
        # (a statistically confident regression, not noise). Fall back to the
        # point estimate only if the CI structure is absent (schema drift).
        mean_data = data.get('mean', {})
        point_pct = mean_data.get('point_estimate', 0) * 100
        ci = mean_data.get('confidence_interval', {})
        lower_bound = ci.get('lower_bound') if isinstance(ci, dict) else None
        if lower_bound is not None:
            lower_pct = lower_bound * 100
            if lower_pct > max_regression:
                print(f'  FAIL: {bench_name}: {point_pct:+.1f}% regression (CI lower bound {lower_pct:+.1f}% > {max_regression}%)')
                reg_failed = True
        else:
            print(f'  NOTE: {bench_name}: change/estimates.json has no confidence_interval — falling back to point estimate')
            if point_pct > max_regression:
                print(f'  FAIL: {bench_name}: {point_pct:+.1f}% regression (point estimate, no CI available; max: {max_regression}%)')
                reg_failed = True

if checked == 0:
    print('  INFO: No benchmarks matched budget entries — gate passed (no violations).')

print('')
if reg_failed:
    print('  Benchmark budget gate FAILED (confident regression).')
    sys.exit(2)
elif abs_failed:
    print('  Benchmark budget gate FAILED (absolute budget breach).')
    sys.exit(1)
else:
    print('  All benchmarks within budget.')
    sys.exit(0)
"

exit $?
