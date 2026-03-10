#!/bin/bash
# financial-test-guard.sh — Ensures price/money/order functions have proper test types
# Called by pre-push-gate.sh. Exit 2 = block push.
#
# Financial functions (identified by name patterns) MUST have:
#   1. At least one standard #[test]
#   2. Property-based test (proptest/quickcheck) OR boundary test
#      (test name contains boundary/edge/limit/overflow/min/max/zero/negative)
#
# RATCHETING: Counts violations across full workspace. Count can only go DOWN.
# This prevents shipping NEW financial logic without boundary tests while
# allowing gradual cleanup of existing code.
#
# Scope: crates/trading/, crates/storage/, crates/core/src/pipeline/
# Exemptions: "// FINANCIAL-TEST-EXEMPT: <reason>" on the preceding line

set -uo pipefail

PROJECT_DIR="${1:-.}"
cd "$PROJECT_DIR" || exit 0

HOOKS_DIR="$(cd "$(dirname "$0")" && pwd)"
BASELINE_FILE="$HOOKS_DIR/.financial-test-baseline"

# Financial crate paths
FINANCIAL_PATHS="crates/trading crates/storage crates/core/src/pipeline"

# Function name patterns that identify financial functions
FINANCIAL_NAME_PATTERNS="price\|pnl\|profit\|loss\|margin\|order\|position\|strike\|premium\|lot\|quantity\|fill\|trade\|candle\|ohlc\|tick.*persist\|calculate.*fee\|calculate.*charge\|slippage\|exposure"

VIOLATIONS=0

for scan_path in $FINANCIAL_PATHS; do
  [ ! -d "$PROJECT_DIR/$scan_path" ] && continue

  rs_files=$(find "$PROJECT_DIR/$scan_path" -name '*.rs' -not -path '*/target/*' -not -name '*_test.rs' -not -path '*/tests/*' -not -path '*/benches/*' 2>/dev/null)

  while IFS= read -r rs_file; do
    [ -z "$rs_file" ] && continue
    [ ! -f "$rs_file" ] && continue

    rel_path="${rs_file#$PROJECT_DIR/}"
    crate_dir=$(echo "$rel_path" | sed -n 's|\(crates/[^/]*\)/.*|\1|p')
    [ -z "$crate_dir" ] && continue

    # Find financial pub fn declarations (with exemption check)
    financial_fns=$(awk '
      /^[[:space:]]*\/\/ FINANCIAL-TEST-EXEMPT:/ { exempt=1; next }
      /^[[:space:]]*(pub|pub\(crate\)) (async )?fn / {
        if (exempt) { exempt=0; next }
        print NR": "$0
        exempt=0
      }
      { exempt=0 }
    ' "$rs_file" | grep -iE "$FINANCIAL_NAME_PATTERNS" || true)

    while IFS= read -r fn_line; do
      [ -z "$fn_line" ] && continue

      fn_name=$(echo "$fn_line" | sed -n 's/.*fn \([a-z_][a-z0-9_]*\).*/\1/p')
      [ -z "$fn_name" ] && continue

      # Skip structural
      case "$fn_name" in
        new|default|from|into|try_from|try_into|fmt|drop|clone)
          continue
          ;;
      esac

      # Check 1: Has at least one standard test
      has_test=$(grep -r "#\[test\]" "$PROJECT_DIR/$crate_dir" --include='*.rs' -l 2>/dev/null | \
        xargs grep -l "fn test_.*${fn_name}\|test.*${fn_name}" 2>/dev/null || true)

      if [ -z "$has_test" ]; then
        VIOLATIONS=$((VIOLATIONS + 1))
        continue
      fi

      # Check 2: Has property-based OR boundary test
      has_proptest=$(grep -r "proptest!\|#\[proptest\]\|quickcheck\|#\[quickcheck\]" "$PROJECT_DIR/$crate_dir" --include='*.rs' 2>/dev/null | \
        grep -i "$fn_name" || true)

      has_boundary=$(grep -r "#\[test\]" "$PROJECT_DIR/$crate_dir" --include='*.rs' -A1 2>/dev/null | \
        grep -iE "fn test_.*${fn_name}.*(boundary|edge|limit|overflow|underflow|min_val|max_val|zero|negative|saturat|extreme|f32_max|f64_max|i32_max|u32_max|huge|tiny)" || true)

      if [ -z "$has_proptest" ] && [ -z "$has_boundary" ]; then
        VIOLATIONS=$((VIOLATIONS + 1))
      fi
    done <<< "$financial_fns"
  done <<< "$rs_files"
done

# ── RATCHETING ──
if [ ! -f "$BASELINE_FILE" ]; then
  echo "$VIOLATIONS" > "$BASELINE_FILE"
  echo "  PASS: Financial test baseline established: $VIOLATIONS gaps (ratchet — can only go DOWN)" >&2
  exit 0
fi

BASELINE=$(cat "$BASELINE_FILE" | tr -d ' \n')

if [ "$VIOLATIONS" -gt "$BASELINE" ]; then
  ADDED=$((VIOLATIONS - BASELINE))
  echo "  FAIL: Financial test gaps INCREASED by $ADDED (was $BASELINE, now $VIOLATIONS)" >&2
  echo "  New financial functions MUST have: unit test + boundary/property test." >&2
  echo "  Exempt: '// FINANCIAL-TEST-EXEMPT: <reason>' on preceding line." >&2
  echo "  Override baseline: echo $VIOLATIONS > $BASELINE_FILE" >&2
  exit 2
fi

if [ "$VIOLATIONS" -lt "$BASELINE" ]; then
  echo "$VIOLATIONS" > "$BASELINE_FILE"
  echo "  PASS: Financial test gaps decreased ($BASELINE -> $VIOLATIONS) — baseline updated" >&2
else
  echo "  PASS: Financial test gaps stable ($VIOLATIONS)" >&2
fi
exit 0
