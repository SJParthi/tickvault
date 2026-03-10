#!/bin/bash
# pub-fn-test-guard.sh — Ensures every new pub fn has a matching test
# Called by pre-push-gate.sh. Exit 2 = block push.
#
# TWO MODES:
#   "staged" (default) — Scans staged files. NEW pub fn MUST have tests. Hard block.
#   "all"              — Scans full workspace. Uses RATCHETING baseline. Count of
#                        untested pub fns can only go DOWN. Blocks if it goes UP.
#
# Exemptions:
#   - Functions in test files (_test.rs, /tests/, /benches/)
#   - Functions with "// TEST-EXEMPT: <reason>" on the preceding line
#   - Structural: main/new/default/from/into/try_from/try_into/fmt/drop/clone/eq/hash/partial_cmp/cmp

set -uo pipefail

PROJECT_DIR="${1:-.}"
cd "$PROJECT_DIR" || exit 0

MODE="${2:-staged}"
HOOKS_DIR="$(cd "$(dirname "$0")" && pwd)"
BASELINE_FILE="$HOOKS_DIR/.untested-pubfn-baseline"

VIOLATIONS=0
REPORT=""

# Get files to scan
if [ "$MODE" = "all" ]; then
  FILES=$(find crates -name '*.rs' -not -path '*/target/*' 2>/dev/null)
else
  FILES=$(git diff --cached --name-only --diff-filter=ACMR 2>/dev/null | grep '\.rs$' || true)
fi

if [ -z "$FILES" ]; then
  exit 0
fi

# Count untested pub fns
count_untested() {
  local scan_files="$1"
  local scan_mode="$2"
  local count=0

  while IFS= read -r file; do
    [ -z "$file" ] && continue
    local full_path="$PROJECT_DIR/$file"
    [ ! -f "$full_path" ] && continue

    # Skip test files
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/benches/)'; then
      continue
    fi

    # Determine crate
    local crate_dir
    crate_dir=$(echo "$file" | sed -n 's|\(crates/[^/]*\)/.*|\1|p')
    [ -z "$crate_dir" ] && continue

    # Get pub fn declarations
    local pub_fns
    if [ "$scan_mode" = "all" ]; then
      pub_fns=$(grep -n 'pub fn \|pub async fn \|pub(crate) fn \|pub(crate) async fn ' "$full_path" 2>/dev/null || true)
    else
      pub_fns=$(git diff --cached -U0 "$file" 2>/dev/null | grep '^\+.*pub fn \|^\+.*pub async fn \|^\+.*pub(crate) fn \|^\+.*pub(crate) async fn ' | sed 's/^\+//' || true)
    fi

    [ -z "$pub_fns" ] && continue

    while IFS= read -r line; do
      [ -z "$line" ] && continue

      local fn_name
      fn_name=$(echo "$line" | sed -n 's/.*fn \([a-z_][a-z0-9_]*\).*/\1/p')
      [ -z "$fn_name" ] && continue

      # Skip structural functions
      case "$fn_name" in
        main|new|default|from|into|try_from|try_into|fmt|drop|clone|eq|hash|partial_cmp|cmp)
          continue
          ;;
      esac

      # Check for TEST-EXEMPT
      if [ "$scan_mode" = "all" ]; then
        local line_num
        line_num=$(echo "$line" | cut -d: -f1)
        if [ -n "$line_num" ] && [ "$line_num" -gt 1 ]; then
          local prev_line=$((line_num - 1))
          if sed -n "${prev_line}p" "$full_path" 2>/dev/null | grep -q 'TEST-EXEMPT:'; then
            continue
          fi
        fi
      fi

      # Search for matching test
      local test_found
      test_found=$(grep -r "#\[test\]" "$crate_dir" --include='*.rs' -l 2>/dev/null | \
        xargs grep -l "fn test_.*${fn_name}\|fn ${fn_name}_\|test.*${fn_name}" 2>/dev/null || true)

      if [ -z "$test_found" ]; then
        count=$((count + 1))
        if [ "$scan_mode" = "staged" ]; then
          REPORT="${REPORT}\n  [UNTESTED] pub fn ${fn_name} in ${file} — no matching #[test] in ${crate_dir}/"
        fi
      fi
    done <<< "$pub_fns"
  done <<< "$scan_files"

  echo "$count"
}

if [ "$MODE" = "all" ]; then
  # ── RATCHETING MODE ──
  # Count current untested pub fns across full workspace
  CURRENT_COUNT=$(count_untested "$FILES" "all")

  if [ ! -f "$BASELINE_FILE" ]; then
    # First run: establish baseline
    echo "$CURRENT_COUNT" > "$BASELINE_FILE"
    echo "  PASS: Untested pub fn baseline established: $CURRENT_COUNT (ratchet — can only go DOWN)" >&2
    exit 0
  fi

  BASELINE=$(cat "$BASELINE_FILE" | tr -d ' \n')

  if [ "$CURRENT_COUNT" -gt "$BASELINE" ]; then
    ADDED=$((CURRENT_COUNT - BASELINE))
    echo "  FAIL: Untested pub fn count INCREASED by $ADDED (was $BASELINE, now $CURRENT_COUNT)" >&2
    echo "  New pub fns MUST have matching #[test] functions." >&2
    echo "  Options: Add test, or add '// TEST-EXEMPT: <reason>' on preceding line." >&2
    echo "  Override baseline: echo $CURRENT_COUNT > $BASELINE_FILE" >&2
    exit 2
  fi

  if [ "$CURRENT_COUNT" -lt "$BASELINE" ]; then
    echo "$CURRENT_COUNT" > "$BASELINE_FILE"
    echo "  PASS: Untested pub fn count decreased ($BASELINE -> $CURRENT_COUNT) — baseline updated" >&2
  else
    echo "  PASS: Untested pub fn count stable ($CURRENT_COUNT)" >&2
  fi
  exit 0

else
  # ── STAGED MODE ──
  # Hard block: any NEW pub fn in staged files must have a test
  VIOLATIONS=$(count_untested "$FILES" "staged")

  if [ "$VIOLATIONS" -gt 0 ]; then
    echo "" >&2
    echo "BLOCKED: $VIOLATIONS new public function(s) have no matching tests:" >&2
    echo -e "$REPORT" >&2
    echo "" >&2
    echo "Every new pub fn must have at least one #[test]. Options:" >&2
    echo "  1. Add test: fn test_<module>_<function>_<scenario>_<expected>()" >&2
    echo "  2. Exempt: Add '// TEST-EXEMPT: <reason>' on the line before pub fn" >&2
    exit 2
  fi

  echo "  PASS: All new public functions have matching tests" >&2
  exit 0
fi
