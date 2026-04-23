#!/usr/bin/env bash
# analyze_results.sh — post-run analysis for depth_200_variants diagnostic.
#
# Reads /tmp/depth_200_variants_results.json (or $RESULTS_JSON_PATH) produced
# by `cargo run --example depth_200_variants`, prints a human-readable
# verdict, and decides whether to:
#   (a) Apply the fix (winner identified)
#   (b) Trigger the Dhan escalation email (no winner)
#
# Usage:
#   bash scripts/dhan-200-depth-repro/analyze_results.sh
#
# Outputs:
#   - Verdict to stdout
#   - Exit 0 = winner found, 1 = no winner (email Dhan), 2 = error

set -euo pipefail

JSON="${RESULTS_JSON_PATH:-/tmp/depth_200_variants_results.json}"

if [[ ! -f "$JSON" ]]; then
  echo "ERROR: $JSON not found. Run the diagnostic first:" >&2
  echo "  cargo run --release --example depth_200_variants" >&2
  exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq required (install: brew install jq / apt install jq)" >&2
  exit 2
fi

winner=$(jq -r '.winner // "null"' "$JSON")
n_variants=$(jq '.results | length' "$JSON")

echo "===== Depth 200 Variant Diagnostic Analysis ====="
echo "Source: $JSON"
echo "Variants tested: $n_variants"
echo ""

# Per-variant table — prefer `column` for nice alignment, fall back to raw tab-separated
_table_body() {
  jq -r '.results[] |
    "\(.label)\tframes=\(.frames)\tdisc=\(.disconnects)\tbytes=\(.bytes)\tlast=\(.last_disconnect_reason // "n/a")"' \
    "$JSON"
}

if command -v column >/dev/null 2>&1; then
  _table_body | column -t -s $'\t'
else
  _table_body
fi

echo ""

if [[ "$winner" == "null" || -z "$winner" ]]; then
  echo "=================================================="
  echo "VERDICT: NO CLEAR WINNER."
  echo ""
  echo "All variants either disconnected or received zero frames."
  echo "This means URL path + User-Agent + ALPN are NOT the root cause."
  echo ""
  echo "NEXT STEPS:"
  echo "  1. Go to Phase 2: test native-tls backend and fastwebsockets library."
  echo "     See .claude/plans/active-plan.md"
  echo "  2. In parallel, send the escalation email to Dhan support."
  echo "     Template: docs/dhan-support/2026-04-24-depth-200-variant-test-results.md"
  echo "     Populate placeholders, share GitHub URL in Gmail reply to apihelp@dhan.co."
  echo ""
  exit 1
fi

echo "=================================================="
echo "VERDICT: FIX IDENTIFIED."
echo ""
echo "Winning variant: $winner"
echo ""
jq -r --arg label "$winner" '.results[] | select(.label == $label) |
  "  URL path: \(.url_path)
  User-Agent: \(.ua)
  ALPN: \(.alpn)
  Frames received: \(.frames)
  Disconnects: \(.disconnects)"' "$JSON"
echo ""
echo "NEXT STEPS:"
echo "  1. Apply this combo to crates/core/src/websocket/depth_connection.rs"
echo "     and crates/common/src/constants.rs."
echo "  2. Update the 'LOCKED FACT' tests and banned-pattern hook (category 4)"
echo "     to reflect the new correct configuration."
echo "  3. Run cargo test --workspace to verify nothing regresses."
echo "  4. Open a follow-up PR with the minimal code + rule updates."
echo "  5. The 8-variant diagnostic can be deleted or kept as a regression test."
echo ""
exit 0
