#!/usr/bin/env bash
# =============================================================================
# tickvault — Per-Crate Coverage Gate
# =============================================================================
# Parses cargo-llvm-cov JSON output and enforces per-crate line coverage
# thresholds. Reads thresholds from quality/crate-coverage-thresholds.toml.
#
# Usage: bash scripts/coverage-gate.sh <coverage.json>
#        bash scripts/coverage-gate.sh --self-test
#
# Exit 0 = all crates pass. Exit 1 = one or more crates below threshold,
# OR a threshold-listed crate is MISSING from the report (fail-closed
# crate-presence assert, 2026-07-10 — see below), OR zero threshold
# entries parsed (fail-closed).
# =============================================================================

set -euo pipefail

# 2026-07-10 (audit follow-up row 9): the thresholds path is overridable ONLY
# for the --self-test mode below, and ONLY when the self-test flag is also
# set — a lone env var can never silently repoint the real gate at a lax
# TOML (adversarial-review MEDIUM, 2026-07-10). CI and `make coverage` set
# neither var — the default is unchanged.
if [ "${TV_COVERAGE_SELF_TEST:-0}" = "1" ] && [ -n "${TV_COVERAGE_THRESHOLDS_FILE:-}" ]; then
  THRESHOLDS_FILE="$TV_COVERAGE_THRESHOLDS_FILE"
else
  THRESHOLDS_FILE="quality/crate-coverage-thresholds.toml"
fi

# -----------------------------------------------------------------------------
# --self-test (2026-07-10): synthetic-report proof that the crate-presence
# assert is NOT vacuous. Runs the gate recursively against synthetic
# reports: (1) all threshold crates present → must PASS; (2) one threshold
# crate missing from the report → must FAIL naming it; (3) an extra crate in
# the report with no explicit floor → must PASS with a WARN naming it;
# (4) an empty [crates] section → must FAIL (zero-thresholds fail-closed).
# -----------------------------------------------------------------------------
if [ "${1:-}" = "--self-test" ]; then
  SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT

  cat > "$TMP/thresholds.toml" <<'EOF'
default = 50.0
[crates]
# a comment with an = sign must be ignored by the parser
alpha = 50.0
beta  = 50.0
EOF

  cat > "$TMP/thresholds-empty.toml" <<'EOF'
default = 50.0
[crates]
EOF

  file_entry() { # file_entry <crate> <count> <covered>
    printf '{"filename":"/w/crates/%s/src/lib.rs","summary":{"lines":{"count":%s,"covered":%s}}}' "$1" "$2" "$3"
  }
  report() { # report <file_entry>[,<file_entry>...]
    printf '{"data":[{"files":[%s]}]}' "$1"
  }

  run_gate() { # run_gate <report.json> <thresholds.toml> — sets RC and OUT
    RC=0
    OUT="$(TV_COVERAGE_SELF_TEST=1 TV_COVERAGE_THRESHOLDS_FILE="$2" bash "$SELF" "$1" 2>&1)" || RC=$?
  }

  FAILURES=0

  # Case 1: every threshold crate present → PASS (exit 0)
  report "$(file_entry alpha 10 10),$(file_entry beta 10 10)" > "$TMP/case1.json"
  run_gate "$TMP/case1.json" "$TMP/thresholds.toml"
  if [ "$RC" -eq 0 ]; then
    echo "self-test case 1 (all crates present → pass): OK"
  else
    echo "self-test case 1 FAILED: expected exit 0, got $RC"; echo "$OUT"; FAILURES=1
  fi

  # Case 2: 'beta' listed in thresholds but ABSENT from the report → HARD FAIL
  # naming it (the crate-presence assert — the whole point of this self-test)
  report "$(file_entry alpha 10 10)" > "$TMP/case2.json"
  run_gate "$TMP/case2.json" "$TMP/thresholds.toml"
  if [ "$RC" -ne 0 ] && echo "$OUT" | grep -q "MISSING: beta"; then
    echo "self-test case 2 (missing crate → hard fail naming it): OK"
  else
    echo "self-test case 2 FAILED: expected non-zero exit + 'MISSING: beta' (got exit $RC)"; echo "$OUT"; FAILURES=1
  fi

  # Case 3: 'gamma' in the report with no explicit floor → PASS + WARN naming it
  report "$(file_entry alpha 10 10),$(file_entry beta 10 10),$(file_entry gamma 10 10)" > "$TMP/case3.json"
  run_gate "$TMP/case3.json" "$TMP/thresholds.toml"
  if [ "$RC" -eq 0 ] && echo "$OUT" | grep -q "WARN: gamma"; then
    echo "self-test case 3 (unlisted crate → pass + warn naming it): OK"
  else
    echo "self-test case 3 FAILED: expected exit 0 + 'WARN: gamma' (got exit $RC)"; echo "$OUT"; FAILURES=1
  fi

  # Case 4: empty [crates] section → HARD FAIL (zero-thresholds fail-closed —
  # otherwise the presence assert is vacuous and every crate silently rides
  # the default floor; adversarial-review HIGH, 2026-07-10)
  report "$(file_entry alpha 10 10),$(file_entry beta 10 10)" > "$TMP/case4.json"
  run_gate "$TMP/case4.json" "$TMP/thresholds-empty.toml"
  if [ "$RC" -ne 0 ] && echo "$OUT" | grep -q "parsed 0 entries"; then
    echo "self-test case 4 (empty [crates] → hard fail): OK"
  else
    echo "self-test case 4 FAILED: expected non-zero exit + 'parsed 0 entries' (got exit $RC)"; echo "$OUT"; FAILURES=1
  fi

  if [ "$FAILURES" -ne 0 ]; then
    echo "coverage-gate self-test: FAILED"
    exit 1
  fi
  echo "coverage-gate self-test: all 4 cases passed"
  exit 0
fi

COVERAGE_JSON="${1:-target/llvm-cov/coverage.json}"

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
#
# 2026-07-10: file paths + default threshold are passed via ENVIRONMENT, not
# interpolated into the python source — a path containing a quote can no
# longer break/inject the python program (adversarial-review MEDIUM).
TV_GATE_COVERAGE_JSON="$COVERAGE_JSON" \
TV_GATE_THRESHOLDS_FILE="$THRESHOLDS_FILE" \
TV_GATE_DEFAULT_THRESHOLD="$DEFAULT_THRESHOLD" \
python3 -c "
import json, sys, re, os
from fractions import Fraction

with open(os.environ['TV_GATE_COVERAGE_JSON']) as f:
    data = json.load(f)

# Parse thresholds TOML (simple key=value parser — no toml library needed).
# Thresholds are kept as EXACT decimals (Fraction), never binary floats:
# float('99.5') is exact, but a computed pct like covered/count*100.0 for a
# ratio of exactly 0.995 evaluates to 99.49999999999999 in binary float and
# would FAIL a >= 99.5 gate even though the true value EQUALS the threshold.
# A ratchet floor means 'at least the threshold' — exact-equal MUST pass, so
# the comparison below is done in exact rational arithmetic.
thresholds = {}
default_threshold = Fraction(os.environ['TV_GATE_DEFAULT_THRESHOLD'])
in_crates = False
with open(os.environ['TV_GATE_THRESHOLDS_FILE']) as f:
    for line in f:
        line = line.strip()
        # Skip comment/blank lines (2026-07-10): a future '# floor = x'
        # comment inside [crates] must not become a garbage 'missing crate'
        # or crash Fraction() with a ValueError.
        if not line or line.startswith('#'):
            continue
        if line == '[crates]':
            in_crates = True
            continue
        if line.startswith('[') and line != '[crates]':
            in_crates = False
            continue
        if in_crates and '=' in line:
            key, val = line.split('=', 1)
            thresholds[key.strip()] = Fraction(val.strip())

# FAIL-CLOSED (2026-07-10, adversarial-review HIGH): zero parsed [crates]
# entries would make the crate-presence assert below vacuous (nothing to
# require) and silently collapse EVERY crate onto the default floor. The
# thresholds file always lists the workspace crates (also pinned by
# crates/common/tests/coverage_threshold_lockdown.rs).
if not thresholds:
    print('  ERROR: parsed 0 entries from the [crates] section of the thresholds TOML —')
    print('         refusing vacuous crate-presence pass. Check ' + os.environ['TV_GATE_THRESHOLDS_FILE'])
    sys.exit(1)

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

# CRATE-PRESENCE ASSERT (2026-07-10, audit follow-up row 9 — Rule 11
# false-OK class): every crate listed in the thresholds TOML MUST be
# present in the parsed report. Before this assert, a crate that silently
# vanished from the report (renamed crate directory, an llvm-cov
# flag/--ignore-filename-regex change, or a partial run) was simply never
# iterated by the loop below — its floor passed VACUOUSLY while the
# all-crates-vanished check above stayed green. Fail-closed: any missing
# crate hard-fails the gate and is named loudly.
missing = sorted(c for c in thresholds if c not in crate_lines)
if missing:
    for crate_name in missing:
        print(f'  MISSING: {crate_name:15s} listed in thresholds TOML but ABSENT from the coverage report')
    print('')
    print('  ERROR: crate-presence assert FAILED — refusing vacuous pass for the crate(s) above.')
    print('         Causes: crate renamed/removed (update quality/crate-coverage-thresholds.toml')
    print('         in the same PR), an llvm-cov flag/filter change, or a partial coverage run.')
    sys.exit(1)

# Inverse direction (warn-only, 2026-07-10): a crate present in the report
# but NOT in the thresholds TOML is still enforced at the default floor by
# the loop below — but a new crate should get an explicit ratcheted floor,
# so name it loudly here instead of letting it ride the default silently.
for crate_name in sorted(c for c in crate_lines if c not in thresholds):
    print(f'  WARN: {crate_name:15s} in report but has NO explicit floor in thresholds TOML — enforced at default ({float(default_threshold)}%). Add a ratcheted floor.')

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
    print('  Crate-presence assert: all', len(thresholds), 'threshold-listed crates found in the report.')
    sys.exit(0)
"

exit $?
