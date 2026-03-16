#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Test Type Guard
# =============================================================================
# Enforces that ALL required test types exist in scope.
# A live trading system handling real money MUST have comprehensive coverage
# across every test category — not just unit tests.
#
# Usage:
#   bash scripts/test-type-guard.sh [project-dir] [scope]
#
# Scope:
#   "all" or omitted  — Full workspace (all crates)
#   "core trading"    — Only check types relevant to these crates
#
# Exit 0 = all required test types present. Exit 1 = missing test types.
# =============================================================================

set -euo pipefail

PROJECT_DIR="${1:-.}"
SCOPE="${2:-all}"
cd "$PROJECT_DIR" || exit 1

FAILED=0
MISSING=""
FOUND=0
TOTAL=0

# Check if a crate is in scope
in_scope() {
  local crate="$1"
  if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
    return 0
  fi
  if echo " $SCOPE " | grep -q " $crate "; then
    return 0
  fi
  return 1
}

check_test_type() {
  local name="$1"
  local pattern="$2"
  local scope="$3"
  TOTAL=$((TOTAL + 1))

  if grep -r "$pattern" "$scope" --include='*.rs' -q 2>/dev/null; then
    FOUND=$((FOUND + 1))
    echo "  PASS: $name"
  else
    FAILED=1
    MISSING="$MISSING  - $name\n"
    echo "  FAIL: $name — no tests found matching pattern '$pattern' in $scope"
  fi
}

skip_test_type() {
  local name="$1"
  echo "  SKIP: $name (crate not in scope)"
}

# Workspace-wide categories: in scoped mode, SKIP (not FAIL) if not found.
# These categories may legitimately live in other crates.
check_workspace_category_scoped() {
  local name="$1"
  local pattern="$2"

  local WC_FOUND=0
  for crate in $SCOPE; do
    if [ -d "crates/$crate" ] && grep -r "$pattern" "crates/$crate/" --include='*.rs' -q 2>/dev/null; then
      WC_FOUND=1
      break
    fi
  done
  if [ "$WC_FOUND" -eq 1 ]; then
    TOTAL=$((TOTAL + 1))
    FOUND=$((FOUND + 1))
    echo "  PASS: $name"
  else
    echo "  SKIP: $name (not in changed crates — enforced in full mode)"
  fi
}

if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  SCOPE_LABEL="full workspace"
else
  SCOPE_LABEL="scoped [$SCOPE]"
fi

echo "╔══════════════════════════════════════════════╗"
echo "║   Test Type Guard (20 categories — $SCOPE_LABEL)  ║"
echo "╚══════════════════════════════════════════════╝"
echo ""

# --- Category 1: Unit Tests ---
if in_scope "core"; then
  check_test_type "Unit tests (core)" "#\[test\]" "crates/core/src"
else
  skip_test_type "Unit tests (core)"
fi
if in_scope "trading"; then
  check_test_type "Unit tests (trading)" "#\[test\]" "crates/trading/src"
else
  skip_test_type "Unit tests (trading)"
fi
if in_scope "common"; then
  check_test_type "Unit tests (common)" "#\[test\]" "crates/common/src"
else
  skip_test_type "Unit tests (common)"
fi

# --- Category 2: Property-Based / Fuzz (proptest) ---
if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  check_test_type "Property-based tests (proptest)" "proptest!" "crates/"
else
  check_workspace_category_scoped "Property-based tests (proptest)" "proptest!"
fi

# --- Category 3: DHAT Allocation Tests ---
if in_scope "core"; then
  check_test_type "DHAT allocation tests" "dhat::Profiler" "crates/core/tests"
else
  skip_test_type "DHAT allocation tests"
fi

# --- Category 4: Snapshot / Golden Tests ---
if in_scope "core"; then
  check_test_type "Snapshot/golden tests" "snapshot_\|golden_" "crates/core/tests"
else
  skip_test_type "Snapshot/golden tests"
fi

# --- Category 5: Deterministic Replay ---
if in_scope "core"; then
  check_test_type "Deterministic replay tests" "replay_\|deterministic_" "crates/core/tests"
else
  skip_test_type "Deterministic replay tests"
fi

# --- Category 6: Backpressure Tests ---
if in_scope "core"; then
  check_test_type "Backpressure tests" "backpressure_" "crates/core/tests"
else
  skip_test_type "Backpressure tests"
fi

# --- Category 7: Timeout Enforcement ---
if in_scope "core"; then
  check_test_type "Timeout enforcement tests" "timeout_" "crates/core/tests"
else
  skip_test_type "Timeout enforcement tests"
fi

# --- Category 8: Graceful Degradation ---
if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  check_test_type "Graceful degradation tests" "degradation_" "crates/"
else
  check_workspace_category_scoped "Graceful degradation tests" "degradation_"
fi

# --- Category 9: Load / Stress Tests ---
if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  check_test_type "Load/stress tests" "stress_" "crates/"
else
  check_workspace_category_scoped "Load/stress tests" "stress_"
fi

# --- Category 10: Panic Safety Tests ---
if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  check_test_type "Panic safety tests" "panic_safety\|no_panic\|should_panic\|doesnt_panic\|does_not_panic" "crates/"
else
  check_workspace_category_scoped "Panic safety tests" "panic_safety\|no_panic\|should_panic\|doesnt_panic\|does_not_panic"
fi

# --- Category 11: Financial Overflow / Boundary ---
if in_scope "trading"; then
  check_test_type "Financial overflow/boundary tests" "overflow_\|boundary_\|financial_" "crates/trading/tests"
else
  skip_test_type "Financial overflow/boundary tests"
fi

# --- Category 12: Loom Concurrency Tests ---
if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  check_test_type "Loom concurrency tests" "loom\|cfg(loom)" "crates/"
else
  check_workspace_category_scoped "Loom concurrency tests" "loom\|cfg(loom)"
fi

# --- Category 13: API Contract Tests ---
if in_scope "api"; then
  check_test_type "API contract tests" "contract_" "crates/api/tests"
else
  skip_test_type "API contract tests"
fi

# --- Category 14: Schema Validation ---
if in_scope "common"; then
  check_test_type "Schema validation tests" "schema_" "crates/common/tests"
else
  skip_test_type "Schema validation tests"
fi

# --- Category 15: Benchmark Tests (Criterion) ---
if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  check_test_type "Benchmark tests (Criterion)" "criterion_group\|criterion_main" "crates/"
else
  check_workspace_category_scoped "Benchmark tests (Criterion)" "criterion_group\|criterion_main"
fi

# --- Category 16: Integration Tests ---
TOTAL=$((TOTAL + 1))
if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  INTEG_COUNT=$(find crates -path "*/tests/*.rs" -not -name "mod.rs" 2>/dev/null | wc -l | tr -d ' ')
  INTEG_THRESHOLD=5
else
  INTEG_COUNT=0
  for crate in $SCOPE; do
    C=$(find "crates/$crate" -path "*/tests/*.rs" -not -name "mod.rs" 2>/dev/null | wc -l | tr -d ' ')
    INTEG_COUNT=$((INTEG_COUNT + C))
  done
  INTEG_THRESHOLD=1
fi
if [ "$INTEG_COUNT" -ge "$INTEG_THRESHOLD" ]; then
  FOUND=$((FOUND + 1))
  echo "  PASS: Integration tests ($INTEG_COUNT files)"
else
  FAILED=1
  MISSING="$MISSING  - Integration tests (only $INTEG_COUNT files, need >=$INTEG_THRESHOLD)\n"
  echo "  FAIL: Integration tests — only $INTEG_COUNT files (need >=$INTEG_THRESHOLD)"
fi

# --- Category 17: Error Path Tests ---
if in_scope "core"; then
  check_test_type "Error path tests" "is_err()\|Err(" "crates/core/tests"
else
  skip_test_type "Error path tests"
fi

# --- Category 18: Configuration Validation ---
if in_scope "common"; then
  check_test_type "Config validation tests" "config\|Config" "crates/common/src"
else
  skip_test_type "Config validation tests"
fi

# --- Category 19: Data Integrity ---
if in_scope "storage"; then
  check_test_type "Data integrity tests (price precision)" "f32_to_f64\|price.*precision\|ieee_754\|data_integrity" "crates/storage"
else
  skip_test_type "Data integrity tests"
fi

# --- Category 20: Security (secret handling) ---
if in_scope "core"; then
  check_test_type "Security tests (secret handling)" "Secret\|secrecy\|zeroize\|secret_" "crates/core/src"
else
  skip_test_type "Security tests"
fi

echo ""
echo "  Result: $FOUND/$TOTAL test types checked ($SCOPE_LABEL)"

if [ "$FAILED" -ne 0 ]; then
  echo ""
  echo "  MISSING test types:"
  echo -e "$MISSING"
  echo "  Test type guard FAILED."
  exit 1
fi

echo "  All required test types present."
exit 0
