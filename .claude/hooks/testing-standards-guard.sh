#!/bin/bash
# =============================================================================
# testing-standards-guard.sh — Mechanical enforcement of testing-standards.md
# =============================================================================
# Enforces ALL 19 test categories from docs/standards/testing-standards.md.
# This is a MECHANICAL check — not human review. If this file and the testing
# standards doc diverge, the guard blocks until they are reconciled.
#
# Usage:
#   bash .claude/hooks/testing-standards-guard.sh [project-dir] [scope]
#
# Scope:
#   "all" or "ALL"    — Full workspace check (all crates)
#   "core trading"    — Only check these specific crates (space-separated)
#   (default)         — Full workspace
#
# Exit 0 = all categories covered. Exit 1 = missing categories.
# =============================================================================

set -uo pipefail

PROJECT_DIR="${1:-.}"
SCOPE="${2:-all}"
cd "$PROJECT_DIR" || exit 1

FAILED=0
MISSING=""
FOUND=0
TOTAL=0

# Determine scan directory based on scope
if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  SCAN_SCOPE="crates/"
  SCOPE_LABEL="workspace-wide"
  ENFORCED_CRATES="core trading common storage"
else
  # Build scan paths from crate list
  SCAN_SCOPE=""
  for crate in $SCOPE; do
    if [ -d "crates/$crate" ]; then
      SCAN_SCOPE="$SCAN_SCOPE crates/$crate/"
    fi
  done
  SCAN_SCOPE=$(echo "$SCAN_SCOPE" | sed 's/^ //')
  SCOPE_LABEL="scoped [$SCOPE]"
  # Only enforce per-crate checks for changed crates that are in the enforced list
  ENFORCED_CRATES="$SCOPE"
fi

if [ -z "$SCAN_SCOPE" ]; then
  echo "  No valid crate directories found for scope: $SCOPE" >&2
  exit 0
fi

check_category() {
  local num="$1"
  local name="$2"
  local pattern="$3"
  local scope="$4"
  TOTAL=$((TOTAL + 1))

  if grep -r "$pattern" $scope --include='*.rs' -q 2>/dev/null; then
    FOUND=$((FOUND + 1))
  else
    FAILED=1
    MISSING="$MISSING  [$num] $name\n"
  fi
}

echo "╔═══════════════════════════════════════════════════════╗" >&2
echo "║  Testing Standards Guard (19 categories — $SCOPE_LABEL) ║" >&2
echo "╚═══════════════════════════════════════════════════════╝" >&2

# ---- Category 1: Smoke Tests ----
check_category 1 "Smoke Tests (basic construction/defaults)" \
  "smoke_\|test_.*default\|test_.*_new\|test_.*_create\|test_.*_init\|test_.*_construct" \
  "$SCAN_SCOPE"

# ---- Category 2: Functionality Tests — Happy Path ----
check_category 2 "Functionality Tests (happy path)" \
  "test_.*success\|test_.*valid\|test_.*happy\|test_.*correct\|test_.*parse" \
  "$SCAN_SCOPE"

# ---- Category 3: Error Scenario Tests ----
check_category 3 "Error Scenario Tests (Result::Err / Option::None)" \
  "test_.*error\|test_.*fail\|test_.*invalid\|test_.*reject\|is_err()" \
  "$SCAN_SCOPE"

# ---- Category 4: Stress / Boundary Tests ----
check_category 4 "Stress / Boundary Tests (MAX, zero, overflow)" \
  "stress_\|boundary_\|test_.*max\|test_.*overflow\|test_.*zero_\|test_.*capacity" \
  "$SCAN_SCOPE"

# ---- Category 5: Property-Based Tests (proptest) ----
check_category 5 "Property-Based Tests (proptest)" \
  "proptest!" \
  "$SCAN_SCOPE"

# ---- Category 6: Performance / O(1) Verification ----
check_category 6 "Performance / O(1) Verification (DHAT)" \
  "dhat::Profiler\|#\[dhat\]" \
  "$SCAN_SCOPE"

# ---- Category 7: Security Tests ----
check_category 7 "Security Tests (no secrets in logs)" \
  "Secret\|secrecy\|zeroize\|test_.*secret\|test_.*redact" \
  "$SCAN_SCOPE"

# ---- Category 8: Deduplication Tests ----
check_category 8 "Deduplication Tests (4-layer dedup chain)" \
  "test_.*dedup\|test_.*duplicate\|test_no_duplicate" \
  "$SCAN_SCOPE"

# ---- Category 9: Concurrency Tests ----
check_category 9 "Concurrency Tests (Mutex/Atomic/ArcSwap)" \
  "loom\|cfg(loom)\|test_.*concurrent\|test_.*atomic\|test_.*mutex" \
  "$SCAN_SCOPE"

# ---- Category 10: Configuration Validation Tests ----
check_category 10 "Configuration Validation Tests" \
  "test_.*config\|test_.*validation\|test_.*constraint\|Config.*test" \
  "$SCAN_SCOPE"

# ---- Category 11: Serialization / Deserialization Tests ----
check_category 11 "Serialization / Deserialization Tests" \
  "test_.*serial\|test_.*deserial\|test_.*json\|test_.*toml\|test_.*csv\|serde" \
  "$SCAN_SCOPE"

# ---- Category 12: Round-Trip Tests ----
check_category 12 "Round-Trip Tests (data flow end-to-end)" \
  "test_.*round_trip\|test_.*roundtrip\|test_.*flow\|test_.*pipeline" \
  "$SCAN_SCOPE"

# ---- Category 13: Determinism Tests ----
check_category 13 "Determinism Tests (same input → same output)" \
  "test_.*deterministic\|test_.*determinism\|replay_\|test_.*idempotent" \
  "$SCAN_SCOPE"

# ---- Category 14: Edge Case Tests ----
check_category 14 "Edge Case Tests (empty, single, exact boundaries)" \
  "test_.*empty\|test_.*single\|test_.*edge\|test_.*boundary\|test_.*exact" \
  "$SCAN_SCOPE"

# ---- Category 15: Regression Tests ----
check_category 15 "Regression Tests (bug fix tests)" \
  "test_.*regression\|test_.*fix\|test_.*bug\|test_.*issue\|test_.*prevent" \
  "$SCAN_SCOPE"

# ---- Category 16: Display / Debug Tests ----
check_category 16 "Display / Debug Tests (format verification)" \
  "test_.*display\|test_.*debug\|test_.*format\|test_.*to_string" \
  "$SCAN_SCOPE"

# ---- Category 17: Timeout / Backoff Tests ----
check_category 17 "Timeout / Backoff Tests (formula verification)" \
  "test_.*timeout\|test_.*backoff\|test_.*retry\|test_.*delay" \
  "$SCAN_SCOPE"

# ---- Category 18: Monitoring Tests ----
check_category 18 "Monitoring Tests (health snapshots, reconnection)" \
  "test_.*health\|test_.*monitor\|test_.*reconnect\|test_.*snapshot" \
  "$SCAN_SCOPE"

# ---- Category 19: Integration Smoke Tests ----
TOTAL=$((TOTAL + 1))
INTEG_COUNT=$(find $SCAN_SCOPE -path "*/tests/*.rs" -not -name "mod.rs" 2>/dev/null | wc -l | tr -d ' ')
if [ "$INTEG_COUNT" -gt 0 ]; then
  FOUND=$((FOUND + 1))
else
  FAILED=1
  MISSING="$MISSING  [19] Integration Smoke Tests (only $INTEG_COUNT files)\n"
fi

echo "" >&2
echo "  $SCOPE_LABEL: $FOUND/$TOTAL testing standard categories covered" >&2

if [ "$FAILED" -ne 0 ]; then
  echo "" >&2
  echo "  MISSING categories (from docs/standards/testing-standards.md):" >&2
  echo -e "$MISSING" >&2
fi

# =============================================================================
# PER-CRATE ENFORCEMENT — Universal categories every crate must satisfy
# =============================================================================
# Only checks crates that are in scope (changed crates in scoped mode).
# Prevents one crate from carrying the entire workspace.
# =============================================================================

PER_CRATE_FAILED=0
PER_CRATE_DETAIL=""

check_crate_category() {
  local crate_dir="$1"
  local cat_name="$2"
  local pattern="$3"

  if grep -r "$pattern" "$crate_dir" --include='*.rs' -q 2>/dev/null; then
    return 0
  else
    return 1
  fi
}

echo "" >&2
echo "  Per-crate enforcement (4 universal categories):" >&2

ALL_ENFORCEABLE="core trading common storage"
for crate in $ENFORCED_CRATES; do
  CRATE_DIR="crates/$crate"
  [ ! -d "$CRATE_DIR" ] && continue

  # In scoped mode, only enforce for crates that are actually enforceable
  if ! echo " $ALL_ENFORCEABLE " | grep -q " $crate "; then
    continue
  fi

  CRATE_GAPS=""

  if ! check_crate_category "$CRATE_DIR" "smoke" \
    "smoke_\|test_.*default\|test_.*_new\|test_.*_create\|test_.*_init\|test_.*_construct"; then
    CRATE_GAPS="$CRATE_GAPS smoke"
  fi

  if ! check_crate_category "$CRATE_DIR" "happy-path" \
    "test_.*success\|test_.*valid\|test_.*happy\|test_.*correct\|test_.*parse"; then
    CRATE_GAPS="$CRATE_GAPS happy-path"
  fi

  if ! check_crate_category "$CRATE_DIR" "error" \
    "test_.*error\|test_.*fail\|test_.*invalid\|test_.*reject\|is_err()"; then
    CRATE_GAPS="$CRATE_GAPS error"
  fi

  if ! check_crate_category "$CRATE_DIR" "edge-case" \
    "test_.*empty\|test_.*single\|test_.*edge\|test_.*boundary\|test_.*exact"; then
    CRATE_GAPS="$CRATE_GAPS edge-case"
  fi

  if [ -n "$CRATE_GAPS" ]; then
    PER_CRATE_FAILED=1
    PER_CRATE_DETAIL="$PER_CRATE_DETAIL    FAIL: crates/$crate — missing:$CRATE_GAPS\n"
    echo "    FAIL: crates/$crate — missing:$CRATE_GAPS" >&2
  else
    echo "    PASS: crates/$crate (smoke, happy-path, error, edge-case)" >&2
  fi
done

if [ "$PER_CRATE_FAILED" -ne 0 ]; then
  FAILED=1
  echo "" >&2
  echo "  Per-crate enforcement FAILED." >&2
  echo "  Every enforced crate must independently have:" >&2
  echo "    - Smoke tests (construction/defaults)" >&2
  echo "    - Happy-path tests (core functionality)" >&2
  echo "    - Error scenario tests (Err/None paths)" >&2
  echo "    - Edge case tests (empty/single/boundary)" >&2
else
  echo "    All enforced crates satisfy universal categories." >&2
fi

# =============================================================================
# FINAL VERDICT
# =============================================================================
echo "" >&2
if [ "$FAILED" -ne 0 ]; then
  echo "  Testing Standards Guard FAILED ($SCOPE_LABEL)." >&2
  echo "  Reference: docs/standards/testing-standards.md" >&2
  exit 1
fi

echo "  All testing standard categories covered ($SCOPE_LABEL)." >&2
exit 0
