#!/bin/bash
# dedup-latency-scanner.sh — Enforces O(1) latency and deduplication guarantees
# Called by pre-commit-gate.sh. Exit 2 = block commit.
# Layer 1 enforcement for data integrity and latency requirements.

set -euo pipefail

PROJECT_DIR="${1:-.}"
STAGED_FILES="${2:-}"

if [ -z "$STAGED_FILES" ]; then
  STAGED_FILES=$(cd "$PROJECT_DIR" && git diff --cached --name-only --diff-filter=ACMR 2>/dev/null | grep '\.rs$' || true)
fi

if [ -z "$STAGED_FILES" ]; then
  exit 0
fi

VIOLATIONS=0
WARNINGS=0
REPORT=""

scan_pattern() {
  local pattern="$1"
  local description="$2"
  local files="$3"
  local is_hot_path="${4:-false}"

  while IFS= read -r file; do
    [ -z "$file" ] && continue
    local full_path="$PROJECT_DIR/$file"
    [ ! -f "$full_path" ] && continue

    # Skip test files
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/test_|_tests\.rs|/benches/)'; then
      continue
    fi

    # If hot-path only, filter to actual hot-path code:
    # Full crates: trading/, websocket/, oms/
    # Core submodules: core/src/websocket/, core/src/ticker/
    # NOT: core/src/auth/, core/src/instrument/, core/src/notification/ (cold path)
    if [ "$is_hot_path" = "true" ]; then
      if ! echo "$file" | grep -qE '^crates/(trading|websocket|oms)/|^crates/core/src/(websocket|ticker)/'; then
        continue
      fi
    fi

    # Extract only production code (skip #[cfg(test)] blocks)
    local matches
    matches=$(awk '
      BEGIN { skip=0; depth=0; exempt=0; skip_next=0; buf="" }
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
      /O\(1\) EXEMPT: begin/ { exempt=1; next }
      /O\(1\) EXEMPT: end/ { exempt=0; next }
      exempt==1 { next }
      /^[[:space:]]*\/\/.*APPROVED:/ || /^[[:space:]]*\/\/.*O\(1\) EXEMPT:/ {
        if (buf != "" && buf ~ /#\[allow\(/) { buf = ""; next }
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
    ' "$full_path" \
      | grep "$pattern" \
      | grep -v '// test' \
      | grep -v '/// ' \
      | grep -v '// SAFETY:' \
      | grep -v '// O(1):' \
      | grep -v '// O(1) EXEMPT:' \
      || true)

    if [ -n "$matches" ]; then
      REPORT="${REPORT}\n  [LATENCY] $description in $file:"
      while IFS= read -r match; do
        REPORT="${REPORT}\n    $match"
        VIOLATIONS=$((VIOLATIONS + 1))
      done <<< "$matches"
    fi
  done <<< "$files"
}

echo "=== O(1) Latency & Dedup Scanner ===" >&2

# ─────────────────────────────────────────────
# O(1) LATENCY VIOLATIONS (hot-path crates only)
# ─────────────────────────────────────────────

# O(n) collection operations on hot path
scan_pattern '\.iter()\.find(' 'O(n) linear search — use HashMap/index lookup' "$STAGED_FILES" true
scan_pattern '\.iter()\.position(' 'O(n) linear search — use HashMap/index lookup' "$STAGED_FILES" true
scan_pattern '\.iter()\.filter(' 'O(n) filter — use pre-indexed structure' "$STAGED_FILES" true
scan_pattern '\.contains(&' 'O(n) contains on Vec — use HashSet or sorted+binary_search' "$STAGED_FILES" true
scan_pattern '\.sort(' 'O(n log n) sort on hot path — pre-sort or use BTreeMap' "$STAGED_FILES" true
scan_pattern '\.sort_by(' 'O(n log n) sort on hot path — pre-sort or use BTreeMap' "$STAGED_FILES" true
scan_pattern '\.sort_unstable(' 'O(n log n) sort on hot path — pre-sort or use BTreeMap' "$STAGED_FILES" true

# NOTE: Recursion detection removed — structurally impossible with grep.
# Covered by hot-path-reviewer agent (AST-level review).

# Blocking I/O on hot path
scan_pattern 'std::fs::' 'Blocking filesystem I/O on hot path — use async' "$STAGED_FILES" true
scan_pattern 'std::net::' 'Blocking network I/O on hot path — use tokio' "$STAGED_FILES" true
scan_pattern '\.read_to_string(' 'Blocking read on hot path' "$STAGED_FILES" true
scan_pattern 'std::thread::sleep' 'thread::sleep on hot path — never block' "$STAGED_FILES" true

# Dynamic dispatch on hot path (vtable lookup = unpredictable latency)
scan_pattern 'Box<dyn' 'Box<dyn> on hot path — use enum_dispatch' "$STAGED_FILES" true
scan_pattern '&dyn ' '&dyn on hot path — use enum_dispatch' "$STAGED_FILES" true

# ─────────────────────────────────────────────
# DEDUPLICATION & INTEGRITY CHECKS (warnings only — heuristic, not enforceable by grep)
# These are reminders, not blockers. Idempotency/dedup logic may live in a separate module.
# ─────────────────────────────────────────────

HOT_FILES=$(echo "$STAGED_FILES" | grep -E '^crates/(trading|oms|core)/' || true)
if [ -n "$HOT_FILES" ]; then
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    local_path="$PROJECT_DIR/$file"
    [ ! -f "$local_path" ] && continue

    # Skip test files
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/test_|_tests\.rs|/benches/)'; then
      continue
    fi

    # Order submission without idempotency — WARNING only
    if grep -q 'submit_order\|place_order\|send_order' "$local_path" 2>/dev/null; then
      if ! grep -q 'idempotency\|idempotent\|dedup' "$local_path" 2>/dev/null; then
        echo "  WARNING: Order submission without idempotency reference in $file" >&2
        echo "    Ensure idempotency key is checked in Valkey BEFORE submission." >&2
        WARNINGS=$((WARNINGS + 1))
      fi
    fi

    # Tick processing without dedup — WARNING only
    if grep -q 'process_tick\|handle_tick\|on_tick' "$local_path" 2>/dev/null; then
      if ! grep -q 'dedup\|duplicate\|seen_tick\|sequence_number\|exchange_timestamp' "$local_path" 2>/dev/null; then
        echo "  WARNING: Tick processing without dedup reference in $file" >&2
        echo "    Deduplicate by (security_id, exchange_timestamp, sequence_number)." >&2
        WARNINGS=$((WARNINGS + 1))
      fi
    fi
  done <<< "$HOT_FILES"
fi

# ─────────────────────────────────────────────
# POSITION RECONCILIATION (warning only)
# ─────────────────────────────────────────────

if [ -n "$HOT_FILES" ]; then
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    local_path="$PROJECT_DIR/$file"
    [ ! -f "$local_path" ] && continue

    # Skip test files
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/test_|_tests\.rs|/benches/)'; then
      continue
    fi

    # Fill handler without reconciliation — WARNING only
    if grep -q 'on_fill\|handle_fill\|order_filled\|execution_report' "$local_path" 2>/dev/null; then
      if ! grep -q 'reconcil\|position_check\|mismatch\|halt' "$local_path" 2>/dev/null; then
        echo "  WARNING: Fill handler without position reconciliation in $file" >&2
        echo "    Reconcile after every fill — mismatch = halt trading." >&2
        WARNINGS=$((WARNINGS + 1))
      fi
    fi
  done <<< "$HOT_FILES"
fi

# ─────────────────────────────────────────────
# RESULT
# ─────────────────────────────────────────────

if [ "$WARNINGS" -gt 0 ]; then
  echo "" >&2
  echo "  $WARNINGS data integrity warning(s) above — review before committing." >&2
fi

if [ "$VIOLATIONS" -gt 0 ]; then
  echo "" >&2
  echo "BLOCKED: $VIOLATIONS O(1) latency violation(s) found:" >&2
  echo -e "$REPORT" >&2
  echo "" >&2
  echo "All hot-path code must be O(1). Use '// O(1) EXEMPT: <reason>' to justify exceptions." >&2
  exit 2
fi

echo "  O(1) latency & dedup scan: CLEAN" >&2
exit 0
