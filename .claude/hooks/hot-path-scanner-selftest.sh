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
# 7. BRACE-LESS `#[cfg(test)]` terminator — bite-proof, BOTH scanners, BOTH
#    directions.
#
#    Found 2026-09-16. `extract_prod_code` sets skip=1 on `#[cfg(test)]` and,
#    for an item carrying no brace (`const X = ...;`, `mod tests;`,
#    `use foo::bar;`), the catch-all skip arm kept consuming until the NEXT
#    line containing `{` — the opening brace of the following PRODUCTION item,
#    which was then swallowed whole and never scanned. Measured on the live
#    tree: `crates/trading/src/strategy/mod.rs` lost three `pub mod` lines.
#
#    An adversarial review of the FIRST patch then found the naive
#    "any line ending in `;`" terminator was wrong three more ways, and two of
#    them were FALSE POSITIVES — a guard that blocks a legitimate commit is a
#    guard that gets disabled. All five shapes are pinned below.
#
#    Scoped to BOTH scanners deliberately: the arm is duplicated in
#    banned-pattern-scanner.sh and data-integrity-guard.sh, and until now only
#    the first copy was covered — the one-file-guarded-two-copies shape this
#    repo records repeatedly. Deleting the arm from EITHER file now fails here.
# ---------------------------------------------------------------------------
SELFTEST_TMP="$(mktemp -d)"
trap 'rm -rf "$SELFTEST_TMP"' EXIT

cat > "$SELFTEST_TMP/c1_trailing_comment.rs" <<'FIXTURE'
#[cfg(test)]
const PLANT_FIXTURE: u8 = 1; // fixture, tests only

pub fn selftest_production_after_comment(x: u8) -> u8 {
    Some(x).unwrap()
}
FIXTURE

cat > "$SELFTEST_TMP/c2_doc_comment.rs" <<'FIXTURE'
#[cfg(test)]
/// Asserts the production shape: let n = parse(s)?;
fn selftest_only_a_test_helper() -> u32 {
    Some(1u32).unwrap()
}
FIXTURE

cat > "$SELFTEST_TMP/c3_raw_string.rs" <<'FIXTURE'
#[cfg(test)]
const SELFTEST_FIXTURE_SQL: &str = r#"
SELECT 1;
"127.0.0.1"
"#;
pub fn selftest_after_raw_string() -> u8 { 1 }
FIXTURE

cat > "$SELFTEST_TMP/c4_braceless_const.rs" <<'FIXTURE'
#[cfg(test)]
const TEST_ONLY_SENTINEL: &str = "x";

fn selftest_production_after(v: Option<u32>) -> u32 {
    v.unwrap()
}
FIXTURE

cat > "$SELFTEST_TMP/c5_test_module.rs" <<'FIXTURE'
#[cfg(test)]
mod tests {
    fn selftest_inside_test_module() -> u32 {
        Some(1u32).unwrap()
    }
}
FIXTURE


for scanner in "$BANNED_SCANNER" ".claude/hooks/data-integrity-guard.sh"; do
  scanner_name="$(basename "$scanner")"
  if [ ! -f "$scanner" ]; then
    fail "$scanner not found — cannot bite-proof its extract_prod_code copy"
    continue
  fi

  sed -n '/^extract_prod_code()/,/^}/p' "$scanner" > "$SELFTEST_TMP/extract.sh"
  # Guard against a vacuous lift: an empty or truncated extraction would make
  # every assertion below pass for the wrong reason.
  if ! grep -q 'awk' "$SELFTEST_TMP/extract.sh"; then
    fail "could not lift extract_prod_code out of $scanner_name — the self-test would pass vacuously"
    continue
  fi
  if ! grep -q 'seen_item' "$SELFTEST_TMP/extract.sh"; then
    fail "$scanner_name: extract_prod_code has no \`seen_item\` state — the brace-less terminator arm is missing or was reverted"
    continue
  fi
  # shellcheck source=/dev/null
  . "$SELFTEST_TMP/extract.sh"

  # (1) production AFTER a brace-less item whose `;` carries a trailing comment
  #     must be scanned. The first patch anchored on `;$` and missed this, so a
  #     real banned pattern went unreported on a one-comment difference.
  out="$(extract_prod_code "$SELFTEST_TMP/c1_trailing_comment.rs")"
  if ! printf '%s' "$out" | grep -q 'selftest_production_after_comment'; then
    fail "$scanner_name: production after \`const X = 1; // comment\` is swallowed — the terminator regex must tolerate a trailing //-comment"
  fi

  # (2) FALSE-POSITIVE guard: a comment BETWEEN the attribute and the item must
  #     not end the skip, however it happens to end.
  out="$(extract_prod_code "$SELFTEST_TMP/c2_doc_comment.rs")"
  if printf '%s' "$out" | grep -q 'selftest_only_a_test_helper'; then
    fail "$scanner_name: a doc comment ending in \`;\` between #[cfg(test)] and its item ends the skip early — TEST-ONLY code is being scanned as production"
  fi

  # (3) FALSE-POSITIVE guard: a raw-string fixture must not be scanned because a
  #     line INSIDE it happens to end in \`;\`.
  out="$(extract_prod_code "$SELFTEST_TMP/c3_raw_string.rs")"
  if printf '%s' "$out" | grep -q '127\.0\.0\.1'; then
    fail "$scanner_name: a raw-string test fixture leaks into the production scan — only the FIRST code line of a brace-less item may terminate the skip"
  fi

  # (4) the original defect: production after a plain brace-less item survives,
  #     and the item itself is still stripped.
  out="$(extract_prod_code "$SELFTEST_TMP/c4_braceless_const.rs")"
  if ! printf '%s' "$out" | grep -q 'selftest_production_after'; then
    fail "$scanner_name: extract_prod_code swallows production code after a brace-less #[cfg(test)] item"
  fi
  if ! printf '%s' "$out" | grep -q 'v.unwrap()'; then
    fail "$scanner_name: extract_prod_code drops the body of the production fn following a brace-less #[cfg(test)] item"
  fi
  if printf '%s' "$out" | grep -q 'TEST_ONLY_SENTINEL'; then
    fail "$scanner_name: extract_prod_code no longer strips the brace-less #[cfg(test)] item itself"
  fi

  # (5) the half that stops the "fix" from being a scanner that strips nothing.
  out="$(extract_prod_code "$SELFTEST_TMP/c5_test_module.rs")"
  if printf '%s' "$out" | grep -q 'selftest_inside_test_module'; then
    fail "$scanner_name: extract_prod_code no longer strips \`#[cfg(test)] mod tests { .. }\` — the terminator arm is too greedy"
  fi

  echo "  ok: $scanner_name extract_prod_code — 5 brace-less shapes pinned" >&2
done

# ---------------------------------------------------------------------------
# RESULT
# ---------------------------------------------------------------------------
if [ "$FAILED" -ne 0 ]; then
  echo "" >&2
  echo "hot-path scanner self-test: FAILED — fix the scanner path filters." >&2
  exit 2
fi

echo "  hot-path scanner self-test: PASS ($ALT_COUNT alternatives, all match real files; brace-less #[cfg(test)] terminator bite-proven in BOTH scanners across 5 shapes)" >&2
exit 0
