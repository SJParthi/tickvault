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
# Exit 0 = all benchmarks within budget. Exit 1 = budget exceeded.
# =============================================================================

set -euo pipefail

CRITERION_DIR="${1:-target/criterion}"
BUDGETS_FILE="quality/benchmark-budgets.toml"

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
max_regression = 5.0
in_budgets = False
with open('$BUDGETS_FILE') as f:
    for line in f:
        line = line.strip()
        if line.startswith('max_regression_pct'):
            max_regression = float(line.split('=', 1)[1].strip())
            continue
        if line == '[budgets]':
            in_budgets = True
            continue
        if line.startswith('[') and line != '[budgets]':
            in_budgets = False
            continue
        if in_budgets and '=' in line and not line.startswith('#'):
            key, val = line.split('=', 1)
            # Strip inline comments
            val = val.split('#')[0].strip()
            budgets[key.strip()] = float(val)

failed = False
checked = 0

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

        # Normalize bench name for budget lookup
        # Try exact match, then underscore-normalized
        budget_ns = None
        for key in budgets:
            if key in bench_name or bench_name in key:
                budget_ns = budgets[key]
                break

        if budget_ns is not None:
            checked += 1
            if median_ns > budget_ns:
                print(f'  FAIL: {bench_name}: {median_ns:.0f}ns (budget: {budget_ns:.0f}ns)')
                failed = True
            else:
                print(f'  PASS: {bench_name}: {median_ns:.0f}ns (budget: {budget_ns:.0f}ns)')
        else:
            print(f'  INFO: {bench_name}: {median_ns:.0f}ns (no budget defined — skipping)')

    # Check for regression from change/estimates.json
    if 'estimates.json' in files and '/change/' in root + '/':
        est_path = os.path.join(root, 'estimates.json')
        with open(est_path) as f:
            data = json.load(f)

        rel = os.path.relpath(root, criterion_dir)
        bench_name = rel.replace('/change', '').replace('/', '_').lower()

        # Regression is stored as a fraction (0.05 = 5%)
        mean_change = data.get('mean', {}).get('point_estimate', 0)
        pct_change = mean_change * 100

        if pct_change > max_regression:
            print(f'  FAIL: {bench_name}: {pct_change:+.1f}% regression (max: {max_regression}%)')
            failed = True

if checked == 0:
    print('  INFO: No benchmarks matched budget entries — gate passed (no violations).')

print('')
if failed:
    print('  Benchmark budget gate FAILED.')
    sys.exit(1)
else:
    print('  All benchmarks within budget.')
    sys.exit(0)
"

exit $?
