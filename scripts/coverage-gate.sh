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

with open('$COVERAGE_JSON') as f:
    data = json.load(f)

# Parse thresholds TOML (simple key=value parser — no toml library needed)
thresholds = {}
default_threshold = float('$DEFAULT_THRESHOLD')
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
            thresholds[key.strip()] = float(val.strip())

# Aggregate per-crate: group files by crate name (crates/<name>/...)
crate_lines = {}
for entry in data.get('data', []):
    for file_info in entry.get('files', []):
        filename = file_info.get('filename', '')
        summary = file_info.get('summary', {}).get('lines', {})
        count = summary.get('count', 0)
        covered = summary.get('covered', 0)

        # Extract crate name from path: crates/<crate_name>/src/...
        match = re.match(r'crates/([^/]+)/', filename)
        if match:
            crate_name = match.group(1)
            if crate_name not in crate_lines:
                crate_lines[crate_name] = {'count': 0, 'covered': 0}
            crate_lines[crate_name]['count'] += count
            crate_lines[crate_name]['covered'] += covered

failed = False
for crate_name in sorted(crate_lines.keys()):
    info = crate_lines[crate_name]
    if info['count'] == 0:
        pct = 100.0
    else:
        pct = (info['covered'] / info['count']) * 100.0
    threshold = thresholds.get(crate_name, default_threshold)
    status = 'PASS' if pct >= threshold else 'FAIL'
    if status == 'FAIL':
        failed = True
    print(f'  {status}: {crate_name:15s} {pct:6.1f}% (threshold: {threshold}%)')

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
