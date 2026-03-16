#!/usr/bin/env bash
# =============================================================================
# test-coverage-guard.sh — Canonical 22 Test Type Enforcement
# =============================================================================
# Single source of truth enforcement for docs/standards/22-test-types.md.
# Replaces BOTH testing-standards-guard.sh (19 cats) AND test-type-guard.sh (20 cats).
#
# Usage:
#   bash scripts/test-coverage-guard.sh [project-dir] [scope]
#
# Scope:
#   "all" or "ALL"    — Full workspace (all crates)
#   "core trading"    — Only check these specific crates (space-separated)
#   "NONE"            — No crates changed, skip
#   (default)         — Full workspace
#
# Exit 0 = all required types present. Exit 1 = missing types.
# =============================================================================

set -uo pipefail

PROJECT_DIR="${1:-.}"
SCOPE="${2:-all}"
cd "$PROJECT_DIR" || exit 1

if [ "$SCOPE" = "NONE" ]; then
  echo "  No crate changes — skipping test coverage guard." >&2
  exit 0
fi

FAILED=0
TOTAL_CHECKED=0
TOTAL_PASSED=0
CRATE_RESULTS=""

# ---------------------------------------------------------------------------
# Per-crate required types (from 22-test-types.md matrix)
# Each crate has a space-separated list of required type numbers.
# ---------------------------------------------------------------------------
REQUIRED_core="1 2 3 4 5 6 7 8 9 10 11 12 13 16 17 18 19 20 22"
REQUIRED_trading="1 2 3 4 5 6 11 13 14 15 16 18 22"
REQUIRED_common="1 2 3 4 5 6 13 16 17 18 21 22"
REQUIRED_storage="1 2 3 4 5 13 18 22"
REQUIRED_api="1 2 3 13 22"
REQUIRED_app="1 22"

# ---------------------------------------------------------------------------
# Type check functions — each returns 0 (found) or 1 (not found)
# All patterns search ONLY within the given crate directory.
# ---------------------------------------------------------------------------

check_type() {
  local type_num="$1"
  local crate_dir="$2"
  local crate_name="$3"

  case "$type_num" in
    1)  # Smoke Tests
      grep -rE '(smoke_|test_.*(_new|_default|_create|_init|_construct|_exists|_is_reasonable|_is_not_empty))' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    2)  # Happy-Path Functionality
      grep -rE '(test_.*(_success|_valid|_happy|_correct|_parse))' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    3)  # Error Scenario Tests
      grep -rE '(test_.*(_error|_fail|_invalid|_reject)|is_err\(\))' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    4)  # Edge Case Tests
      grep -rE '(test_.*(_empty|_single|_edge|_boundary|_exact))' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    5)  # Stress/Boundary Tests
      grep -rE '(stress_|boundary_|test_.*(_max|_overflow|_zero_|_capacity))' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    6)  # Property-Based Tests (proptest)
      grep -r 'proptest!' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    7)  # DHAT Allocation Tests
      grep -r 'dhat::Profiler' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    8)  # Snapshot/Golden Tests
      grep -rE '(fn snapshot_|fn golden_)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    9)  # Deterministic Replay Tests
      grep -rE '(fn .*replay|fn .*deterministic|test_.*_idempotent)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    10) # Backpressure Tests
      grep -rE '(backpressure_|test_.*_backpressure)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    11) # Timeout/Backoff Tests
      grep -rE '(test_.*(_timeout|_backoff|_retry|_delay))' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    12) # Graceful Degradation Tests
      grep -rE '(degradation_|test_.*_degradation)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    13) # Panic Safety Tests
      grep -rE '(should_panic|no_panic|does_not_panic|doesnt_panic|panic_safety|catch_unwind|_panic)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    14) # Financial Overflow/Boundary Tests
      grep -rE '(overflow_|boundary_|financial_)' "$crate_dir/tests/" --include='*.rs' -q 2>/dev/null
      ;;
    15) # Loom Concurrency Tests
      grep -rE '(cfg\(loom\)|loom::model)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    16) # Serialization/Deserialization Tests
      grep -rE '(test_.*(_json|_serial|_deserial|_toml|_csv)|parse_strategy_config|serde_json::from|toml::from)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    17) # Round-Trip Tests
      grep -rE '(test_.*(_round_trip|_roundtrip|_flow|_pipeline))' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    18) # Display/Debug Format Tests
      grep -rE '(test_.*(_display|_debug|_format|_to_string)|_display_|_debug_|Display.*panic|no_panic.*display)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    19) # Security Tests (Secret Handling)
      grep -rE '(test_.*(_secret|_redact)|Secret<)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    20) # Deduplication Tests
      grep -rE '(test_.*(_dedup|_duplicate)|test_no_duplicate)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    21) # Schema Validation Tests
      grep -rE '(fn schema_|test_.*_schema)' "$crate_dir" --include='*.rs' -q 2>/dev/null
      ;;
    22) # Integration Tests
      local threshold=1
      case "$crate_name" in
        core) threshold=5 ;;
        trading) threshold=3 ;;
        storage) threshold=2 ;;
        *) threshold=1 ;;
      esac
      local count
      count=$(find "$crate_dir" -path "*/tests/*.rs" -not -name "mod.rs" 2>/dev/null | wc -l | tr -d ' ')
      if [ "$count" -ge "$threshold" ]; then
        return 0
      fi
      # Fallback for binary crates (app): count inline #[test] functions
      if [ "$crate_name" = "app" ]; then
        local inline_count
        inline_count=$(grep -r '#\[test\]' "$crate_dir/src/" --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')
        [ "$inline_count" -ge 1 ]
      else
        [ "$count" -ge "$threshold" ]
      fi
      ;;
    *)
      echo "  WARNING: Unknown test type $type_num" >&2
      return 1
      ;;
  esac
}

# Type names for human-readable output
type_name() {
  case "$1" in
    1)  echo "Smoke Tests" ;;
    2)  echo "Happy-Path Functionality" ;;
    3)  echo "Error Scenario Tests" ;;
    4)  echo "Edge Case Tests" ;;
    5)  echo "Stress/Boundary Tests" ;;
    6)  echo "Property-Based (proptest)" ;;
    7)  echo "DHAT Allocation Tests" ;;
    8)  echo "Snapshot/Golden Tests" ;;
    9)  echo "Deterministic Replay" ;;
    10) echo "Backpressure Tests" ;;
    11) echo "Timeout/Backoff Tests" ;;
    12) echo "Graceful Degradation" ;;
    13) echo "Panic Safety Tests" ;;
    14) echo "Financial Overflow/Boundary" ;;
    15) echo "Loom Concurrency" ;;
    16) echo "Serde Tests" ;;
    17) echo "Round-Trip Tests" ;;
    18) echo "Display/Debug Format" ;;
    19) echo "Security (secrets)" ;;
    20) echo "Deduplication Tests" ;;
    21) echo "Schema Validation" ;;
    22) echo "Integration Tests" ;;
    *)  echo "Unknown ($1)" ;;
  esac
}

# ---------------------------------------------------------------------------
# Determine which crates to check
# ---------------------------------------------------------------------------
if [ "$SCOPE" = "all" ] || [ "$SCOPE" = "ALL" ]; then
  CRATES_TO_CHECK="core trading common storage api app"
  SCOPE_LABEL="full workspace"
else
  CRATES_TO_CHECK=""
  for crate in $SCOPE; do
    if [ -d "crates/$crate" ]; then
      CRATES_TO_CHECK="$CRATES_TO_CHECK $crate"
    fi
  done
  CRATES_TO_CHECK=$(echo "$CRATES_TO_CHECK" | sed 's/^ //')
  SCOPE_LABEL="scoped [$SCOPE]"
fi

if [ -z "$CRATES_TO_CHECK" ]; then
  echo "  No valid crate directories found for scope: $SCOPE" >&2
  exit 0
fi

echo "╔══════════════════════════════════════════════════════════╗" >&2
echo "║  Test Coverage Guard (22 types — $SCOPE_LABEL)  ║" >&2
echo "╚══════════════════════════════════════════════════════════╝" >&2
echo "" >&2

# ---------------------------------------------------------------------------
# Check each crate against its required types
# ---------------------------------------------------------------------------
for crate in $CRATES_TO_CHECK; do
  CRATE_DIR="crates/$crate"
  [ ! -d "$CRATE_DIR" ] && continue

  # Get required types for this crate
  REQUIRED_VAR="REQUIRED_${crate}"
  REQUIRED_TYPES="${!REQUIRED_VAR:-}"

  if [ -z "$REQUIRED_TYPES" ]; then
    echo "  SKIP: $crate (no requirements defined)" >&2
    continue
  fi

  CRATE_PASSED=0
  CRATE_TOTAL=0
  CRATE_MISSING=""

  for type_num in $REQUIRED_TYPES; do
    CRATE_TOTAL=$((CRATE_TOTAL + 1))
    TOTAL_CHECKED=$((TOTAL_CHECKED + 1))

    if check_type "$type_num" "$CRATE_DIR" "$crate"; then
      CRATE_PASSED=$((CRATE_PASSED + 1))
      TOTAL_PASSED=$((TOTAL_PASSED + 1))
    else
      CRATE_MISSING="$CRATE_MISSING [$type_num]$(type_name "$type_num")"
    fi
  done

  if [ "$CRATE_PASSED" -eq "$CRATE_TOTAL" ]; then
    echo "  PASS: $crate ($CRATE_PASSED/$CRATE_TOTAL types)" >&2
  else
    FAILED=1
    echo "  FAIL: $crate ($CRATE_PASSED/$CRATE_TOTAL types)" >&2
    echo "    Missing:$CRATE_MISSING" >&2
  fi
done

# ---------------------------------------------------------------------------
# Final verdict
# ---------------------------------------------------------------------------
echo "" >&2
echo "  Total: $TOTAL_PASSED/$TOTAL_CHECKED checks passed ($SCOPE_LABEL)" >&2

if [ "$FAILED" -ne 0 ]; then
  echo "" >&2
  echo "  Test Coverage Guard FAILED." >&2
  echo "  Reference: docs/standards/22-test-types.md" >&2
  exit 1
fi

echo "  All required test types present." >&2
exit 0
