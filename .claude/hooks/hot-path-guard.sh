#!/bin/bash
# hot-path-guard.sh — PostToolUse hook that warns when hot-path crates are edited
# Triggers on Edit|Write to crates/core, trading, websocket, oms .rs files
# This is a WARNING hook (exit 0) — it injects a reminder, doesn't block.
# The pre-commit-gate with banned-pattern-scanner provides the hard block.

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')

# Only check .rs files in hot-path crates
if [ -z "$FILE_PATH" ]; then
  exit 0
fi

if ! echo "$FILE_PATH" | grep -qE '\.(rs)$'; then
  exit 0
fi

if ! echo "$FILE_PATH" | grep -qE 'crates/(core|trading|websocket|oms)/'; then
  exit 0
fi

echo "HOT PATH WARNING: You just modified a performance-critical file: $(basename "$FILE_PATH")"
echo "BEFORE COMMITTING, verify:"
echo "  1. Zero heap allocations (no Box/Vec/String/format!/clone)"
echo "  2. O(1) complexity only (no .iter().find, .contains, .sort)"
echo "  3. No blocking I/O (no std::fs, std::net, thread::sleep)"
echo "  4. No dyn Trait (use enum_dispatch)"
echo "  5. No DashMap (use papaya)"
echo "  6. Tick dedup by (security_id, exchange_timestamp, sequence_number)"
echo "  7. Order idempotency key checked in Valkey BEFORE submission"
echo "The pre-commit scanner will block violations mechanically."

exit 0
