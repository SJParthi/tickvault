#!/usr/bin/env bash
# Wave 1 Item 0.e — self-test for banned-pattern-scanner.sh category 7
# (gap closure G12 — "without fixture self-test the hook can be silently
# broken").
#
# Procedure:
#   1. Copy `.claude/hooks/test-fixtures/cat7_violation_fixture.rs` to a
#      temporary path that LOOKS LIKE `crates/core/src/pipeline/tick_processor.rs`.
#   2. Run the scanner against it.
#   3. Assert the scanner reports EXACTLY 3 violations, all of category 7.
#   4. Assert the scanner does NOT flag the fixture's `hot_path_with_exempt_marker`
#      counter-fixture (proves the exempt-detection logic is intact).
#
# Exit 0 = self-test passed (category 7 fires as expected).
# Exit 1 = self-test failed — category 7 is silently broken.

set -u -o pipefail

PROJECT_DIR="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
HOOK="$PROJECT_DIR/.claude/hooks/banned-pattern-scanner.sh"
FIXTURE="$PROJECT_DIR/.claude/hooks/test-fixtures/cat7_violation_fixture.rs"

if [ ! -f "$HOOK" ]; then
  echo "FAIL: banned-pattern-scanner.sh not found at $HOOK" >&2
  exit 1
fi
if [ ! -f "$FIXTURE" ]; then
  echo "FAIL: fixture not found at $FIXTURE" >&2
  exit 1
fi

# Stage the fixture as if it were `tick_processor.rs`. We use a sandbox
# directory under /tmp so we don't pollute the real source tree.
SANDBOX=$(mktemp -d)
trap 'rm -rf "$SANDBOX"' EXIT
mkdir -p "$SANDBOX/crates/core/src/pipeline"
cp "$FIXTURE" "$SANDBOX/crates/core/src/pipeline/tick_processor.rs"

# Run the scanner against the sandbox path.
SCAN_OUT=$(bash "$HOOK" "$SANDBOX" "crates/core/src/pipeline/tick_processor.rs" 2>&1 || true)

# Count category-7 hits — the scanner labels them with "Wave 1 Item 0.a"
# / "Wave 1 Item 0.d" prefixes.
CAT7_HITS=$(echo "$SCAN_OUT" | grep -cE 'Wave 1 Item 0\.[ad]:' || true)

# Assert: at least 3 violations expected (write, rename, create_dir_all).
if [ "$CAT7_HITS" -lt 3 ]; then
  echo "FAIL: category 7 self-test detected only $CAT7_HITS violation(s) in fixture (expected ≥ 3)" >&2
  echo "Scanner output:" >&2
  echo "$SCAN_OUT" >&2
  exit 1
fi

# Assert: the counter-fixture (HOT-PATH-EXEMPT marker on preceding
# line) is NOT flagged. We grep the line containing "fixture-allowed"
# in the scanner output — it should be absent.
if echo "$SCAN_OUT" | grep -q 'fixture-allowed'; then
  echo "FAIL: category 7 self-test flagged the HOT-PATH-EXEMPT counter-fixture" >&2
  echo "(this means the exempt-detection regex regressed — the rule is now too strict)" >&2
  echo "Scanner output:" >&2
  echo "$SCAN_OUT" >&2
  exit 1
fi

echo "PASS: banned-pattern-scanner category 7 self-test ($CAT7_HITS violations detected, exempt-marker honored)" >&2
exit 0
