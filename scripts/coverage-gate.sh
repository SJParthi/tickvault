#!/usr/bin/env bash
# =============================================================================
# tickvault — Per-Crate Coverage Gate
# =============================================================================
# Parses cargo-llvm-cov JSON output and enforces per-crate line coverage
# thresholds. Reads thresholds from quality/crate-coverage-thresholds.toml.
#
# Usage: bash scripts/coverage-gate.sh <coverage.json>
#
# Exit 0 = all crates pass. Exit 1 = one or more crates below threshold.
# =============================================================================

set -euo pipefail

COVERAGE_JSON="${1:-target/llvm-cov/coverage.json}"
THRESHOLDS_FILE="quality/crate-coverage-thresholds.toml"

if [ ! -f "$COVERAGE_JSON" ]; then
  echo "ERROR: Coverage JSON not found: $COVERAGE_JSON" >&2
  echo "  Run: cargo llvm-cov --workspace --json --output-path $COVERAGE_JSON" >&2
  exit 1
fi

if [ ! -f "$THRESHOLDS_FILE" ]; then
  echo "ERROR: Thresholds file not found: $THRESHOLDS_FILE" >&2
  exit 1
fi

FAILED=0

echo "╔══════════════════════════════════════════════╗"
echo "║   Per-Crate Coverage Gate                     ║"
echo "╚══════════════════════════════════════════════╝"
echo ""

# Parse default threshold from TOML (simple grep — no external deps)
DEFAULT_THRESHOLD=$(grep -E '^default\s*=' "$THRESHOLDS_FILE" | head -1 | sed 's/.*=\s*//' | tr -d ' "')
if [ -z "$DEFAULT_THRESHOLD" ]; then
  DEFAULT_THRESHOLD="99.0"
fi

# Extract per-crate coverage from llvm-cov JSON using python3 (available on GHA runners)
# llvm-cov JSON format: { "data": [ { "files": [ { "filename": "...", "summary": { "lines": { "percent": N } } } ] } ] }
python3 -c "
import json, sys, re
from fractions import Fraction

with open('$COVERAGE_JSON') as f:
    data = json.load(f)

# Parse thresholds TOML (simple key=value parser — no toml library needed).
# Thresholds are kept as EXACT decimals (Fraction), never binary floats:
# float('99.5') is exact, but a computed pct like covered/count*100.0 for a
# ratio of exactly 0.995 evaluates to 99.49999999999999 in binary float and
# would FAIL a >= 99.5 gate even though the true value EQUALS the threshold.
# A ratchet floor means 'at least the threshold' — exact-equal MUST pass, so
# the comparison below is done in exact rational arithmetic.
thresholds = {}
default_threshold = Fraction('$DEFAULT_THRESHOLD')
in_crates = False
with open('$THRESHOLDS_FILE') as f:
    for line in f:
        line = line.strip()
        if line == '[crates]':
            in_crates = True
            continue
        if line.startswith('[') and line != '[crates]':
            in_crates = False
            continue
        if in_crates and '=' in line:
            key, val = line.split('=', 1)
            thresholds[key.strip()] = Fraction(val.strip())

# Aggregate per-crate: group files by crate name (crates/<name>/...)
crate_lines = {}
for entry in data.get('data', []):
    for file_info in entry.get('files', []):
        filename = file_info.get('filename', '')
        summary = file_info.get('summary', {}).get('lines', {})
        count = summary.get('count', 0)
        covered = summary.get('covered', 0)

        # Extract crate name from path: .../crates/<crate_name>/src/...
        # cargo-llvm-cov emits ABSOLUTE paths (/home/runner/work/.../crates/...),
        # so this must be re.search, NOT re.match (start-anchored re.match
        # matched zero files and made the gate pass vacuously — see
        # .claude/plans/research/coverage-gaps.md §2, GAP #0).
        match = re.search(r'crates/([^/]+)/', filename)
        if match:
            crate_name = match.group(1)
            if crate_name not in crate_lines:
                crate_lines[crate_name] = {'count': 0, 'covered': 0}
            crate_lines[crate_name]['count'] += count
            crate_lines[crate_name]['covered'] += covered

# FAIL-CLOSED: an empty aggregation map means the report's paths matched no
# crate at all — checking nothing must never count as passing (audit-findings
# Rule 11: no false-OK signals).
if not crate_lines:
    print('  ERROR: gate matched 0 files under crates/ — refusing vacuous pass.')
    print('         (coverage JSON paths did not contain \"crates/<name>/\")')
    sys.exit(1)

failed = False
for crate_name in sorted(crate_lines.keys()):
    info = crate_lines[crate_name]
    if info['count'] == 0:
        pct = Fraction(100)
    else:
        # EXACT rational percentage — no binary-float rounding error.
        pct = Fraction(info['covered'] * 100, info['count'])
    threshold = thresholds.get(crate_name, default_threshold)
    # Exact comparison: pct == threshold PASSES (ratchet = 'at least').
    status = 'PASS' if pct >= threshold else 'FAIL'
    if status == 'FAIL':
        failed = True
    # Display at 2 decimals so a value just below the threshold can no
    # longer be printed as visually EQUAL to it (the old 1-decimal display
    # showed 'FAIL: 99.5% (threshold: 99.5%)' for a true 99.46%).
    print(f'  {status}: {crate_name:15s} {float(pct):7.2f}% (threshold: {float(threshold)}%)')

if failed:
    print('')
    print('  Per-crate coverage gate FAILED.')
    sys.exit(1)
else:
    print('')
    print('  All crates meet coverage thresholds.')
    sys.exit(0)
"

exit $?
