#!/bin/bash
# hot-path-scanner-selftest.sh — zero-match guard for the hot-path scanner paths.
#
# WHY THIS EXISTS (2026-07-05 audit finding, HIGH): the hot-path filters in
# banned-pattern-scanner.sh + dedup-latency-scanner.sh referenced paths that
# DID NOT EXIST in the tree (`crates/websocket/`, `crates/oms/`,
# `crates/core/src/ticker/`), so the REAL per-tick chain was never scanned —
# silently, for months. This self-test fails (exit 2) if ANY configured
# hot-path alternative matches zero existing .rs files, so a future rename or
# module move can never blind the scanners again.
#
# It also pins LOCKSTEP: the dedup-latency-scanner must carry the exact same
# alternatives as the canonical HOT_PATH_INCLUDE_REGEX in
# banned-pattern-scanner.sh.
#
# Usage: bash .claude/hooks/hot-path-scanner-selftest.sh [PROJECT_DIR]
# Exit codes: 0 = PASS, 2 = FAIL.

set -euo pipefail

PROJECT_DIR="${1:-.}"
cd "$PROJECT_DIR"

BANNED_SCANNER=".claude/hooks/banned-pattern-scanner.sh"
DEDUP_SCANNER=".claude/hooks/dedup-latency-scanner.sh"

FAILED=0

fail() {
  echo "  FAIL: $1" >&2
  FAILED=1
}

echo "=== Hot-path scanner self-test (zero-match guard) ===" >&2

for f in "$BANNED_SCANNER" "$DEDUP_SCANNER"; do
  if [ ! -f "$f" ]; then
    fail "$f not found"
  fi
done
if [ "$FAILED" -ne 0 ]; then
  exit 2
fi

# ---------------------------------------------------------------------------
# 1. Extract the canonical include regex from banned-pattern-scanner.sh
# ---------------------------------------------------------------------------
# NOTE: `|| true` keeps set -e from silently aborting on a no-match grep —
# the explicit empty-checks below report the failure loudly instead.
INCLUDE_REGEX=$(grep -E "^HOT_PATH_INCLUDE_REGEX=" "$BANNED_SCANNER" | head -1 | sed -E "s/^HOT_PATH_INCLUDE_REGEX='//; s/'$//" || true)
EXCLUDE_REGEX=$(grep -E "^HOT_PATH_EXCLUDE_REGEX=" "$BANNED_SCANNER" | head -1 | sed -E "s/^HOT_PATH_EXCLUDE_REGEX='//; s/'$//" || true)

if [ -z "$INCLUDE_REGEX" ]; then
  fail "could not extract HOT_PATH_INCLUDE_REGEX from $BANNED_SCANNER"
  exit 2
fi
if [ -z "$EXCLUDE_REGEX" ]; then
  fail "could not extract HOT_PATH_EXCLUDE_REGEX from $BANNED_SCANNER"
fi

# ---------------------------------------------------------------------------
# 2. Build the real file list (tracked + on-disk, target/ excluded)
# ---------------------------------------------------------------------------
ALL_RS=$(find crates -name '*.rs' -not -path '*/target/*' 2>/dev/null || true)
if [ -z "$ALL_RS" ]; then
  fail "no .rs files found under crates/ — run from the repo root"
  exit 2
fi

# ---------------------------------------------------------------------------
# 3. Every '|'-separated alternative of the include regex must match at least
#    one EXISTING .rs file. A zero-match alternative = the scanner is blind
#    for that path (the exact bug class this guard prevents).
# ---------------------------------------------------------------------------
ALT_COUNT=0
IFS='|' read -r -a ALTERNATIVES <<< "$INCLUDE_REGEX"
for alt in "${ALTERNATIVES[@]}"; do
  [ -z "$alt" ] && continue
  ALT_COUNT=$((ALT_COUNT + 1))
  matches=$(echo "$ALL_RS" | grep -cE "$alt" || true)
  if [ "${matches:-0}" -eq 0 ]; then
    fail "hot-path alternative matches ZERO existing files: $alt"
  else
    echo "  ok: $alt  (${matches} files)" >&2
  fi
done

if [ "$ALT_COUNT" -lt 2 ]; then
  fail "suspiciously few hot-path alternatives ($ALT_COUNT) — regex extraction broke?"
fi

# ---------------------------------------------------------------------------
# 4. The exclude regex must also match at least one existing file — an
#    exclusion that matches nothing is stale and should be removed.
# ---------------------------------------------------------------------------
if [ -n "$EXCLUDE_REGEX" ]; then
  ex_matches=$(echo "$ALL_RS" | grep -cE "$EXCLUDE_REGEX" || true)
  if [ "${ex_matches:-0}" -eq 0 ]; then
    fail "hot-path EXCLUDE regex matches zero existing files (stale): $EXCLUDE_REGEX"
  else
    echo "  ok: exclusion $EXCLUDE_REGEX  (${ex_matches} files)" >&2
  fi
fi

# ---------------------------------------------------------------------------
# 5. Lockstep: every include alternative must appear VERBATIM in
#    dedup-latency-scanner.sh (its hot-path filter is inline).
# ---------------------------------------------------------------------------
for alt in "${ALTERNATIVES[@]}"; do
  [ -z "$alt" ] && continue
  if ! grep -qF "$alt" "$DEDUP_SCANNER"; then
    fail "dedup-latency-scanner.sh is missing hot-path alternative (lockstep broken): $alt"
  fi
done

# ---------------------------------------------------------------------------
# 6. Phantom-path regression pins: the exact stale paths from the 2026-07-05
#    audit must never reappear in either scanner's hot-path filter.
# ---------------------------------------------------------------------------
for phantom in 'crates/(trading|websocket|oms)/' 'core/src/(websocket|ticker)/'; do
  for f in "$BANNED_SCANNER" "$DEDUP_SCANNER"; do
    if grep -F "$phantom" "$f" | grep -v '^[[:space:]]*#' | grep -q .; then
      fail "phantom hot-path filter reappeared in $f: $phantom"
    fi
  done
done

# ---------------------------------------------------------------------------
# 7. BRACE-LESS `#[cfg(test)]` terminator — bite-proof, both directions.
#
#    Found 2026-09-16. `extract_prod_code` sets skip=1 on `#[cfg(test)]` and,
#    for an item carrying no brace (`const X = ...;`, `mod tests;`,
#    `use foo::bar;`), the catch-all skip arm kept consuming until the NEXT
#    line containing `{` — the opening brace of the following PRODUCTION item,
#    which was then swallowed whole and never scanned. Measured on the live
#    tree: `crates/trading/src/strategy/mod.rs` lost three `pub mod` lines, and
#    a minimal fixture lost an entire production fn carrying `.unwrap()`.
#
#    This pins BOTH halves, because only the second half stops the "fix" from
#    being a scanner that strips nothing:
#      (a) production code AFTER a brace-less #[cfg(test)] item must SURVIVE;
#      (b) a real `mod tests { ... }` body must still be STRIPPED.
# ---------------------------------------------------------------------------
SELFTEST_TMP="$(mktemp -d)"
trap 'rm -rf "$SELFTEST_TMP"' EXIT

sed -n '/^extract_prod_code()/,/^}/p' "$BANNED_SCANNER" > "$SELFTEST_TMP/extract.sh"
if ! grep -q 'awk' "$SELFTEST_TMP/extract.sh"; then
  fail "could not lift extract_prod_code out of $BANNED_SCANNER — the self-test would pass vacuously"
fi
# shellcheck source=/dev/null
. "$SELFTEST_TMP/extract.sh"

cat > "$SELFTEST_TMP/braceless.rs" <<'FIXTURE'
#[cfg(test)]
const TEST_ONLY_SENTINEL: &str = "x";

fn selftest_production_after(v: Option<u32>) -> u32 {
    v.unwrap()
}

#[cfg(test)]
mod tests {
    fn selftest_inside_test_module() -> u32 {
        Some(1u32).unwrap()
    }
}
FIXTURE

SELFTEST_EXTRACTED="$(extract_prod_code "$SELFTEST_TMP/braceless.rs")"

# (a) production after a brace-less #[cfg(test)] item must be scanned
if ! printf '%s' "$SELFTEST_EXTRACTED" | grep -q 'selftest_production_after'; then
  fail "extract_prod_code swallows production code after a brace-less #[cfg(test)] item — restore the \`skip==1 && depth==0 && /;[[:space:]]*\$/\` terminator arm"
fi
if ! printf '%s' "$SELFTEST_EXTRACTED" | grep -q 'v.unwrap()'; then
  fail "extract_prod_code drops the body of the production fn following a brace-less #[cfg(test)] item"
fi

# (b) a real test module body must STILL be stripped
if printf '%s' "$SELFTEST_EXTRACTED" | grep -q 'selftest_inside_test_module'; then
  fail "extract_prod_code no longer strips \`#[cfg(test)] mod tests { .. }\` — the terminator arm is too greedy"
fi
if printf '%s' "$SELFTEST_EXTRACTED" | grep -q 'TEST_ONLY_SENTINEL'; then
  fail "extract_prod_code no longer strips the brace-less #[cfg(test)] item itself"
fi

# ---------------------------------------------------------------------------
# RESULT
# ---------------------------------------------------------------------------
if [ "$FAILED" -ne 0 ]; then
  echo "" >&2
  echo "hot-path scanner self-test: FAILED — fix the scanner path filters." >&2
  exit 2
fi

echo "  hot-path scanner self-test: PASS ($ALT_COUNT alternatives, all match real files; brace-less #[cfg(test)] terminator bite-proven both ways)" >&2
exit 0
