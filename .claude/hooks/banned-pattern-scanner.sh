#!/bin/bash
# banned-pattern-scanner.sh — Scans staged .rs files for ALL banned patterns
# Called by pre-commit-gate.sh. Exit 2 = block commit.
# This is Layer 1 enforcement: mechanical, zero LLM involvement.

set -euo pipefail

PROJECT_DIR="${1:-.}"
STAGED_FILES="${2:-}"

# If no staged files passed, detect them
if [ -z "$STAGED_FILES" ]; then
  STAGED_FILES=$(cd "$PROJECT_DIR" && git diff --cached --name-only --diff-filter=ACMR 2>/dev/null | grep '\.rs$' || true)
fi

if [ -z "$STAGED_FILES" ]; then
  exit 0
fi

VIOLATIONS=0
REPORT=""

# Helper: extract ONLY production code from a Rust file (strips #[cfg(test)] modules and test functions)
# Uses awk to track brace depth inside #[cfg(test)] and #[test] blocks
extract_prod_code() {
  local file="$1"
  awk '
    BEGIN { skip=0; depth=0 }
    /^[[:space:]]*#\[cfg\(test\)\]/ { skip=1; next }
    /^[[:space:]]*#\[test\]/ { skip=1; next }
    skip==1 && /\{/ {
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth <= 0) { skip=0; depth=0 }
      next
    }
    skip==1 {
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth <= 0) { skip=0; depth=0 }
      next
    }
    { print NR": "$0 }
  ' "$file"
}

# Helper: scan staged files for a pattern, excluding test files and test modules
scan_prod_code() {
  local pattern="$1"
  local description="$2"
  local files="$3"

  while IFS= read -r file; do
    [ -z "$file" ] && continue
    local full_path="$PROJECT_DIR/$file"
    [ ! -f "$full_path" ] && continue

    # Skip test files entirely
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/test_|_tests\.rs|/benches/)'; then
      continue
    fi

    # Extract prod code (strips #[cfg(test)] blocks), then scan
    local matches
    matches=$(extract_prod_code "$full_path" \
      | grep "$pattern" \
      | grep -v '// test' \
      | grep -v '// TODO' \
      | grep -v '// SAFETY:' \
      | grep -v '// APPROVED:' \
      | grep -v '/// ' \
      | grep -v '//!' \
      || true)

    if [ -n "$matches" ]; then
      REPORT="${REPORT}\n  [BANNED] $description in $file:"
      while IFS= read -r match; do
        REPORT="${REPORT}\n    $match"
        VIOLATIONS=$((VIOLATIONS + 1))
      done <<< "$matches"
    fi
  done <<< "$files"
}

# Helper: scan ONLY hot-path code
# Hot path = crates/trading/, crates/websocket/, crates/oms/ (full crates)
#          + crates/core/src/websocket/, crates/core/src/ticker/ (specific modules within core)
# Cold path within core (auth/, instrument/, notification/, config/) is NOT hot path.
scan_hot_path() {
  local pattern="$1"
  local description="$2"
  local files="$3"
  local hot_path_files

  hot_path_files=$(echo "$files" | grep -E '^crates/(trading|websocket|oms)/|^crates/core/src/(websocket|ticker)/' || true)
  if [ -z "$hot_path_files" ]; then
    return
  fi

  scan_prod_code "$pattern" "$description" "$hot_path_files"
}

echo "=== Banned Pattern Scanner ===" >&2

# ─────────────────────────────────────────────
# CATEGORY 1: Universal bans (all prod code)
# ─────────────────────────────────────────────

# .unwrap() in production code
scan_prod_code '\.unwrap()' '.unwrap() — use ? with anyhow/thiserror' "$STAGED_FILES"

# .expect() in production code (same as .unwrap with a message)
scan_prod_code '\.expect(' '.expect() — use ? with anyhow/thiserror' "$STAGED_FILES"

# println! / dbg! / eprintln! in production code
scan_prod_code 'println!' 'println! — use tracing macros' "$STAGED_FILES"
scan_prod_code 'dbg!' 'dbg! — use tracing macros' "$STAGED_FILES"
scan_prod_code 'eprintln!' 'eprintln! — use tracing macros' "$STAGED_FILES"

# localhost / 127.0.0.1 in application code (not config/test)
scan_prod_code '"localhost' 'localhost — use Docker DNS hostnames' "$STAGED_FILES"
scan_prod_code '"127\.0\.0\.1' '127.0.0.1 — use Docker DNS hostnames' "$STAGED_FILES"

# DashMap anywhere
scan_prod_code 'DashMap' 'DashMap — use papaya for concurrent maps' "$STAGED_FILES"

# Unbounded channels
scan_prod_code 'unbounded_channel\|unbounded()' 'unbounded channel — use bounded capacity always' "$STAGED_FILES"
scan_prod_code 'mpsc::channel()' 'mpsc::channel() without capacity — use bounded' "$STAGED_FILES"

# bincode (banned, use bitcode)
scan_prod_code 'bincode::' 'bincode — use bitcode instead' "$STAGED_FILES"

# #[allow(...)] without approval
scan_prod_code '#\[allow(' '#[allow()] — requires // APPROVED: comment on same/preceding line' "$STAGED_FILES"

# unsafe blocks
scan_prod_code 'unsafe {' 'unsafe block — requires // SAFETY: justification' "$STAGED_FILES"
scan_prod_code 'unsafe fn ' 'unsafe fn — requires // SAFETY: justification' "$STAGED_FILES"

# Banned infrastructure (use alternatives)
scan_prod_code 'promtail' 'Promtail — use Grafana Alloy' "$STAGED_FILES"
scan_prod_code 'jaeger' 'Jaeger v1 — use Jaeger v2 or OTLP' "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 2: Hot-path only bans (core/trading/websocket/oms)
# ─────────────────────────────────────────────

# .clone() on hot path
scan_hot_path '\.clone()' '.clone() on hot path — use Copy types or references' "$STAGED_FILES"

# Box::new / Vec::new / String::new allocations on hot path
scan_hot_path 'Box::new(' 'Box::new() on hot path — zero allocation required' "$STAGED_FILES"
scan_hot_path 'Vec::new()' 'Vec::new() on hot path — pre-allocate or use ArrayVec' "$STAGED_FILES"
scan_hot_path 'vec!\[' 'vec![] on hot path — pre-allocate or use ArrayVec' "$STAGED_FILES"
scan_hot_path 'String::new()' 'String::new() on hot path — use &str or pre-allocated' "$STAGED_FILES"
scan_hot_path 'String::from(' 'String::from() on hot path — use &str or pre-allocated' "$STAGED_FILES"
scan_hot_path '\.to_string()' '.to_string() on hot path — use &str or pre-allocated' "$STAGED_FILES"
scan_hot_path '\.to_owned()' '.to_owned() on hot path — use references' "$STAGED_FILES"
scan_hot_path 'format!' 'format!() on hot path — zero allocation required' "$STAGED_FILES"

# .collect() on hot path (unbounded allocation)
scan_hot_path '\.collect()' '.collect() on hot path — unbounded allocation' "$STAGED_FILES"

# dyn Trait on hot path (use enum_dispatch)
scan_hot_path 'dyn ' 'dyn Trait on hot path — use enum_dispatch' "$STAGED_FILES"

# HashMap::new() without capacity on hot path
scan_hot_path 'HashMap::new()' 'HashMap::new() on hot path — use with_capacity()' "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 3: Hardcoded values (all prod code)
# ─────────────────────────────────────────────

# Hardcoded Duration::from_secs with literal number (digits-only inside parens)
scan_prod_code 'Duration::from_secs([0-9][0-9]*)' 'Hardcoded Duration — use named constant' "$STAGED_FILES"
scan_prod_code 'Duration::from_millis([0-9][0-9]*)' 'Hardcoded Duration — use named constant' "$STAGED_FILES"
scan_prod_code 'Duration::from_nanos([0-9][0-9]*)' 'Hardcoded Duration — use named constant' "$STAGED_FILES"

# Hardcoded port numbers
scan_prod_code '":[0-9][0-9][0-9][0-9]"' 'Hardcoded port — use config' "$STAGED_FILES"

# Hardcoded WebSocket/API URLs (exclude protocol validation like starts_with("https://"))
scan_prod_code '"wss://[a-zA-Z]' 'Hardcoded WebSocket URL — use config' "$STAGED_FILES"
scan_prod_code '"https://[a-zA-Z]' 'Hardcoded HTTPS URL — use config' "$STAGED_FILES"

# ─────────────────────────────────────────────
# RESULT
# ─────────────────────────────────────────────

if [ "$VIOLATIONS" -gt 0 ]; then
  echo "" >&2
  echo "BLOCKED: $VIOLATIONS banned pattern violation(s) found:" >&2
  echo -e "$REPORT" >&2
  echo "" >&2
  echo "Fix all violations before committing. See CLAUDE.md BANNED section." >&2
  exit 2
fi

echo "  Banned pattern scan: CLEAN ($( echo "$STAGED_FILES" | wc -l) files scanned)" >&2
exit 0
