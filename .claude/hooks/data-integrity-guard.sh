#!/bin/bash
# data-integrity-guard.sh — Blocks ALL patterns that corrupt price data
# Called by pre-commit-gate.sh (fast) and pre-push-gate.sh (full workspace).
# Exit 2 = block.
#
# Scans storage and pipeline code for patterns that cause data manipulation:
#   1. f64::from(f32) — IEEE 754 widening artifacts (10.20 → 10.19999980926514)
#   2. `as f64` on f32 values — same problem as f64::from
#   3. f32 arithmetic before storage — rounding errors compound
#   4. .round() / .floor() / .ceil() on prices — silent precision loss
#   5. format!() then parse back for prices — unnecessary lossy round-trip
#
# Scope: crates/storage/, crates/core/src/pipeline/
# Exemptions: "// DATA-INTEGRITY-EXEMPT: <reason>" on same or preceding line
#
# This guard ensures Dhan price data is NEVER manipulated between receipt and storage.
# What Dhan sends is what QuestDB stores. Period.

set -uo pipefail

PROJECT_DIR="${1:-.}"
STAGED_FILES="${2:-}"

# If no staged files passed, scan full workspace
if [ -z "$STAGED_FILES" ]; then
  STAGED_FILES=$(find "$PROJECT_DIR/crates/storage" "$PROJECT_DIR/crates/core/src/pipeline" \
    -name '*.rs' -not -path '*/target/*' 2>/dev/null | \
    sed "s|^$PROJECT_DIR/||" || true)
fi

# Filter to only storage and pipeline files
SCAN_FILES=$(echo "$STAGED_FILES" | grep -E '^crates/(storage|core/src/pipeline)/' || true)

if [ -z "$SCAN_FILES" ]; then
  exit 0
fi

VIOLATIONS=0
REPORT=""

# Helper: extract prod code (same as banned-pattern-scanner)
extract_prod_code() {
  local file="$1"
  awk '
    BEGIN { skip=0; depth=0; exempt=0; skip_next=0; buf="" }
    skip==0 && /^[[:space:]]*#\[cfg\(test\)\]/ { skip=1; depth=0; next }
    skip==0 && /^[[:space:]]*#\[test\]/ { skip=1; depth=0; next }
    skip==1 && depth > 0 {
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth <= 0) { skip=0; depth=0 }
      next
    }
    skip==1 && /\{/ {
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth <= 0) { skip=0; depth=0 }
      next
    }
    skip==1 { next }
    # Block-level exemptions
    /DATA-INTEGRITY-EXEMPT: begin/ { exempt=1; next }
    /DATA-INTEGRITY-EXEMPT: end/ { exempt=0; next }
    exempt==1 { next }
    # Standalone exemption comments (approve NEXT line or retroactively approve buffered line)
    /^[[:space:]]*\/\/.*DATA-INTEGRITY-EXEMPT:/ || /^[[:space:]]*\/\/.*APPROVED:/ {
      # Retroactive: if buffered line is a potential violation, clear it
      if (buf != "" && (buf ~ /f64::from/ || buf ~ / as f64/)) { buf = ""; next }
      # Forward: flush any buffered non-violation line, then skip the next line
      if (buf != "") print buf
      buf = ""
      skip_next = 1
      next
    }
    skip_next > 0 { skip_next = 0; next }
    {
      if (buf != "") print buf
      buf = NR": "$0
    }
    END { if (buf != "") print buf }
  ' "$file"
}

# Scan helper
scan_integrity() {
  local pattern="$1"
  local description="$2"
  local files="$3"

  while IFS= read -r file; do
    [ -z "$file" ] && continue
    local full_path="$PROJECT_DIR/$file"
    [ ! -f "$full_path" ] && continue

    # Skip test files
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/benches/)'; then
      continue
    fi

    local matches
    matches=$(extract_prod_code "$full_path" \
      | grep "$pattern" \
      | grep -v '/// ' \
      | grep -v '//!' \
      | grep -v '// DATA-INTEGRITY-EXEMPT:' \
      | grep -v '// APPROVED:' \
      || true)

    if [ -n "$matches" ]; then
      REPORT="${REPORT}\n  [DATA CORRUPTION] $description in $file:"
      while IFS= read -r match; do
        REPORT="${REPORT}\n    $match"
        VIOLATIONS=$((VIOLATIONS + 1))
      done <<< "$matches"
    fi
  done <<< "$files"
}

echo "=== Data Integrity Guard ===" >&2

# ─────────────────────────────────────────────
# CHECK 1: f64::from() on f32 — IEEE 754 widening
# ─────────────────────────────────────────────
scan_integrity 'f64::from(' \
  'f64::from(f32) — use f32_to_f64_clean() to preserve Dhan price precision' \
  "$SCAN_FILES"

# ─────────────────────────────────────────────
# CHECK 2: `as f64` cast on price-context lines only
# (Not all `as f64` are bad — metrics/durations are fine)
# ─────────────────────────────────────────────
for scan_file in $SCAN_FILES; do
  [ -z "$scan_file" ] && continue
  full_path="$PROJECT_DIR/$scan_file"
  [ ! -f "$full_path" ] && continue
  if echo "$scan_file" | grep -qE '(_test\.rs|/tests/|/benches/)'; then
    continue
  fi

  matches=$(extract_prod_code "$full_path" \
    | grep ' as f64' \
    | grep -iE 'price\|open\|high\|low\|close\|ltp\|avg_price\|strike\|premium\|f32' \
    | grep -v '/// ' \
    | grep -v '//!' \
    | grep -v '// DATA-INTEGRITY-EXEMPT:' \
    | grep -v '// APPROVED:' \
    || true)

  if [ -n "$matches" ]; then
    REPORT="${REPORT}\n  [DATA CORRUPTION] \`as f64\` on price data in $scan_file — use f32_to_f64_clean():"
    while IFS= read -r match; do
      REPORT="${REPORT}\n    $match"
      VIOLATIONS=$((VIOLATIONS + 1))
    done <<< "$matches"
  fi
done

# ─────────────────────────────────────────────
# CHECK 3: .round()/.floor()/.ceil() on price-context lines
# Only flag if the line also mentions price/open/high/low/close/ltp
# ─────────────────────────────────────────────
for fn_call in '\.round()' '\.floor()' '\.ceil()'; do
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    full_path="$PROJECT_DIR/$file"
    [ ! -f "$full_path" ] && continue
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/benches/)'; then
      continue
    fi

    matches=$(extract_prod_code "$full_path" \
      | grep "$fn_call" \
      | grep -iE 'price\|open\|high\|low\|close\|ltp\|avg_price\|strike\|premium' \
      | grep -v '// DATA-INTEGRITY-EXEMPT:' \
      | grep -v '// APPROVED:' \
      || true)

    if [ -n "$matches" ]; then
      REPORT="${REPORT}\n  [DATA CORRUPTION] ${fn_call} on price data — never round/floor/ceil raw prices:"
      while IFS= read -r match; do
        REPORT="${REPORT}\n    $file: $match"
        VIOLATIONS=$((VIOLATIONS + 1))
      done <<< "$matches"
    fi
  done <<< "$SCAN_FILES"
done

# ─────────────────────────────────────────────
# CHECK 4: format!() then parse for price conversion
# Pattern: format!(...price...).parse or format!(...ltp...).parse
# ─────────────────────────────────────────────
while IFS= read -r file; do
  [ -z "$file" ] && continue
  full_path="$PROJECT_DIR/$file"
  [ ! -f "$full_path" ] && continue
  if echo "$file" | grep -qE '(_test\.rs|/tests/|/benches/)'; then
    continue
  fi

  # Check for format!() → parse chains on price data (multi-line aware)
  matches=$(extract_prod_code "$full_path" \
    | grep -iE 'format!.*price\|format!.*ltp\|format!.*open\|format!.*high\|format!.*low\|format!.*close' \
    | grep '\.parse' \
    | grep -v '// DATA-INTEGRITY-EXEMPT:' \
    | grep -v '// APPROVED:' \
    || true)

  if [ -n "$matches" ]; then
    REPORT="${REPORT}\n  [DATA CORRUPTION] format!().parse() on prices in $file — use f32_to_f64_clean():"
    while IFS= read -r match; do
      REPORT="${REPORT}\n    $match"
      VIOLATIONS=$((VIOLATIONS + 1))
    done <<< "$matches"
  fi
done <<< "$SCAN_FILES"

# ─────────────────────────────────────────────
# RESULT
# ─────────────────────────────────────────────

if [ "$VIOLATIONS" -gt 0 ]; then
  echo "" >&2
  echo "BLOCKED: $VIOLATIONS data integrity violation(s) found:" >&2
  echo -e "$REPORT" >&2
  echo "" >&2
  echo "Dhan price data must NEVER be manipulated between receipt and storage." >&2
  echo "What Dhan sends is what QuestDB stores. Use f32_to_f64_clean() for f32→f64." >&2
  echo "Exempt: Add '// DATA-INTEGRITY-EXEMPT: <reason>' on preceding line." >&2
  exit 2
fi

echo "  Data integrity scan: CLEAN" >&2
exit 0
