#!/usr/bin/env bash
# =============================================================================
# tickvault — Per-Crate Coverage Gate
# =============================================================================
# Parses cargo-llvm-cov JSON output and enforces per-crate line coverage
# thresholds. Reads thresholds from quality/crate-coverage-thresholds.toml.
#
# Usage: bash scripts/coverage-gate.sh <coverage.json>
#        bash scripts/coverage-gate.sh --self-test
#
# Exit 0 = all crates pass. Exit 1 = one or more crates below threshold,
# OR a threshold-listed crate is MISSING from the report (fail-closed
# crate-presence assert, 2026-07-10 — see below), OR zero threshold
# entries parsed (fail-closed).
# =============================================================================

set -euo pipefail

# 2026-07-10 (audit follow-up row 9): the thresholds path is overridable ONLY
# for the --self-test mode below, and ONLY when the self-test flag is also
# set — a lone env var can never silently repoint the real gate at a lax
# TOML (adversarial-review MEDIUM, 2026-07-10). CI and `make coverage` set
# neither var — the default is unchanged.
if [ "${TV_COVERAGE_SELF_TEST:-0}" = "1" ] && [ -n "${TV_COVERAGE_THRESHOLDS_FILE:-}" ]; then
  THRESHOLDS_FILE="$TV_COVERAGE_THRESHOLDS_FILE"
else
  THRESHOLDS_FILE="quality/crate-coverage-thresholds.toml"
fi

# -----------------------------------------------------------------------------
# --self-test (2026-07-10): synthetic-report proof that the crate-presence
# assert is NOT vacuous. Runs the gate recursively against synthetic
# reports: (1) all threshold crates present → must PASS; (2) one threshold
# crate missing from the report → must FAIL naming it; (3) an extra crate in
# the report with no explicit floor → must PASS with a WARN naming it;
# (4) an empty [crates] section → must FAIL (zero-thresholds fail-closed).
# -----------------------------------------------------------------------------
if [ "${1:-}" = "--self-test" ]; then
  SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT

  cat > "$TMP/thresholds.toml" <<'EOF'
default = 50.0
[crates]
# a comment with an = sign must be ignored by the parser
alpha = 50.0
beta  = 50.0
EOF

  cat > "$TMP/thresholds-empty.toml" <<'EOF'
default = 50.0
[crates]
EOF

  file_entry() { # file_entry <crate> <count> <covered>
    printf '{"filename":"/w/crates/%s/src/lib.rs","summary":{"lines":{"count":%s,"covered":%s}}}' "$1" "$2" "$3"
  }
  report() { # report <file_entry>[,<file_entry>...]
    printf '{"data":[{"files":[%s]}]}' "$1"
  }

  run_gate() { # run_gate <report.json> <thresholds.toml> — sets RC and OUT
    RC=0
    OUT="$(TV_COVERAGE_SELF_TEST=1 TV_COVERAGE_THRESHOLDS_FILE="$2" bash "$SELF" "$1" 2>&1)" || RC=$?
  }

  FAILURES=0

  # Case 1: every threshold crate present → PASS (exit 0)
  report "$(file_entry alpha 10 10),$(file_entry beta 10 10)" > "$TMP/case1.json"
  run_gate "$TMP/case1.json" "$TMP/thresholds.toml"
  if [ "$RC" -eq 0 ]; then
    echo "self-test case 1 (all crates present → pass): OK"
  else
    echo "self-test case 1 FAILED: expected exit 0, got $RC"; echo "$OUT"; FAILURES=1
  fi

  # Case 2: 'beta' listed in thresholds but ABSENT from the report → HARD FAIL
  # naming it (the crate-presence assert — the whole point of this self-test)
  report "$(file_entry alpha 10 10)" > "$TMP/case2.json"
  run_gate "$TMP/case2.json" "$TMP/thresholds.toml"
  if [ "$RC" -ne 0 ] && echo "$OUT" | grep -q "MISSING: beta"; then
    echo "self-test case 2 (missing crate → hard fail naming it): OK"
  else
    echo "self-test case 2 FAILED: expected non-zero exit + 'MISSING: beta' (got exit $RC)"; echo "$OUT"; FAILURES=1
  fi

  # Case 3: 'gamma' in the report with no explicit floor → PASS + WARN naming it
  report "$(file_entry alpha 10 10),$(file_entry beta 10 10),$(file_entry gamma 10 10)" > "$TMP/case3.json"
  run_gate "$TMP/case3.json" "$TMP/thresholds.toml"
  if [ "$RC" -eq 0 ] && echo "$OUT" | grep -q "WARN: gamma"; then
    echo "self-test case 3 (unlisted crate → pass + warn naming it): OK"
  else
    echo "self-test case 3 FAILED: expected exit 0 + 'WARN: gamma' (got exit $RC)"; echo "$OUT"; FAILURES=1
  fi

  # Case 4: empty [crates] section → HARD FAIL (zero-thresholds fail-closed —
  # otherwise the presence assert is vacuous and every crate silently rides
  # the default floor; adversarial-review HIGH, 2026-07-10)
  report "$(file_entry alpha 10 10),$(file_entry beta 10 10)" > "$TMP/case4.json"
  run_gate "$TMP/case4.json" "$TMP/thresholds-empty.toml"
  if [ "$RC" -ne 0 ] && echo "$OUT" | grep -q "parsed 0 entries"; then
    echo "self-test case 4 (empty [crates] → hard fail): OK"
  else
    echo "self-test case 4 FAILED: expected non-zero exit + 'parsed 0 entries' (got exit $RC)"; echo "$OUT"; FAILURES=1
  fi

  if [ "$FAILURES" -ne 0 ]; then
    echo "coverage-gate self-test: FAILED"
    exit 1
  fi
  echo "coverage-gate self-test: all 4 cases passed"
  exit 0
fi

COVERAGE_JSON="${1:-target/llvm-cov/coverage.json}"

if [ ! -f "$COVERAGE_JSON" ]; then
  echo "ERROR: Coverage JSON not found: $COVERAGE_JSON" >&2
  echo "  Run: cargo llvm-cov --workspace --json --output-path $COVERAGE_JSON" >&2
  exit 1
fi

if [ ! -f "$THRESHOLDS_FILE" ]; then
  echo "ERROR: Thresholds file not found: $THRESHOLDS_FILE" >&2
  exit 1
fi

FAILED=0

echo "╔══════════════════════════════════════════════╗"
echo "║   Per-Crate Coverage Gate                     ║"
echo "╚══════════════════════════════════════════════╝"
echo ""

# Parse default threshold from TOML (simple grep — no external deps)
DEFAULT_THRESHOLD=$(grep -E '^default\s*=' "$THRESHOLDS_FILE" | head -1 | sed 's/.*=\s*//' | tr -d ' "')
if [ -z "$DEFAULT_THRESHOLD" ]; then
  DEFAULT_THRESHOLD="99.0"
fi

# Extract per-crate coverage from llvm-cov JSON using jq, then enforce the
# thresholds in awk with EXACT scaled-integer arithmetic (rust-only purge
# Phase 2a-2, 2026-07-18: replaces the inline python3/Fraction block — same
# semantics, same output, same exit codes).
# llvm-cov JSON format: { "data": [ { "files": [ { "filename": "...", "summary": { "lines": { "count": N, "covered": M } } } ] } ] }
#
# EXACTNESS CONTRACT (unchanged from the Fraction implementation this
# replaces): thresholds are EXACT decimals, never binary floats. A computed
# pct like covered/count*100.0 for a ratio of exactly 0.995 evaluates to
# 99.49999999999999 in binary float and would FAIL a >= 99.5 gate even
# though the true value EQUALS the threshold. A ratchet floor means 'at
# least the threshold' — exact-equal MUST pass. The comparison below is
# therefore integer CROSS-MULTIPLICATION: with threshold = T/10^d
# (T, d integers parsed from the decimal string),
#     covered*100/count >= T/10^d   <=>   covered*100*10^d >= T*count
# All operands are integers far below 2^53 (d capped at 6; line counts are
# ~1e6-class), so awk's IEEE doubles represent every term EXACTLY and the
# comparison is exact rational arithmetic — no rounding anywhere. The
# 2-decimal pct shown in the report is display-only, never compared.
#
# File paths + the default threshold are passed via awk -v / argv, never
# interpolated into the program text (the 2026-07-10 injection posture kept).

TSV_FILE="$(mktemp)"
trap 'rm -f "$TSV_FILE"' EXIT

# FAIL CLOSED on a PRESENT null/non-numeric lines.count / lines.covered
# (review round 2 fix, 2026-07-18): the previous `// 0` turned an explicit
# JSON null into 0 and let a string ride into awk (where `+0` coerces to 0),
# so a corrupt report read 100.00% PASS. The old python crashed with a
# TypeError there (exit 1); restore that fail-closed contract with a named
# jq error (set -e aborts the gate on jq's non-zero exit). A MISSING key
# stays 0 — the old python `.get(..., 0)` was equally open there, and the
# parity contract deliberately keeps missing-key behavior unchanged.
jq -r '
  def req_num($k):
    (.summary.lines // {}) as $l
    | if ($l | has($k))
      then ($l[$k]
            | if type == "number" then .
              else error("summary.lines.\($k) is present but \(type) — corrupt coverage JSON; refusing to treat it as 0 (fail closed)") end)
      else 0 end;
  (.data // [])[]
  | (.files // [])[]
  | [ (.filename // ""),
      req_num("count"),
      req_num("covered") ]
  | @tsv
' "$COVERAGE_JSON" > "$TSV_FILE"

LC_ALL=C awk -F'\t' \
  -v tf="$THRESHOLDS_FILE" \
  -v default_thr="$DEFAULT_THRESHOLD" \
'
function trim(s) { sub(/^[ \t\r]+/, "", s); sub(/[ \t\r]+$/, "", s); return s }
# parse_dec: parse a decimal string like "99.5" into G_scaled/G_pow so that
# value == G_scaled / G_pow, all-integer. Dies loudly on anything that is
# not a plain non-negative decimal (the old Fraction() call crashed there).
function parse_dec(v,   ip, fp, i) {
  if (v !~ /^[0-9]+(\.[0-9]+)?$/) {
    print "  ERROR: invalid threshold value (expected a plain decimal): " v
    died = 1; exit 1
  }
  if (v ~ /\./) {
    ip = v; sub(/\..*$/, "", ip)
    fp = v; sub(/^[^.]*\./, "", fp)
  } else { ip = v; fp = "" }
  if (length(fp) > 6) {
    # keeps every term of the cross-multiplication exactly below 2^53
    print "  ERROR: threshold has more than 6 decimal places: " v
    died = 1; exit 1
  }
  G_pow = 1
  for (i = 0; i < length(fp); i++) G_pow *= 10
  G_scaled = ip * G_pow + (fp == "" ? 0 : fp + 0)
}
# frepr: render a decimal threshold string the way python float repr did
# ("50" -> "50.0", "99.50" -> "99.5", "63.3" -> "63.3") — display only.
function frepr(v,   s) {
  s = v
  if (s !~ /\./) return s ".0"
  sub(/0+$/, "", s)
  if (s ~ /\.$/) s = s "0"
  return s
}
function sort_keys(arr, out,   k, n, i, j, t) {
  n = 0
  for (k in arr) out[++n] = k
  for (i = 2; i <= n; i++) {
    t = out[i]
    for (j = i - 1; j >= 1 && out[j] > t; j--) out[j + 1] = out[j]
    out[j + 1] = t
  }
  return n
}
# ---- thresholds TOML (simple key=value parser — comment/blank lines and
# ---- non-[crates] sections skipped, exactly as before) ----
FILENAME == tf {
  line = trim($0)
  if (line == "" || line ~ /^#/) next
  if (line == "[crates]") { in_crates = 1; next }
  if (line ~ /^\[/) { in_crates = 0; next }
  if (in_crates && index(line, "=") > 0) {
    eq = index(line, "=")
    key = trim(substr(line, 1, eq - 1))
    val = trim(substr(line, eq + 1))
    parse_dec(val)   # validate loudly at parse time
    thr_raw[key] = val
  }
  next
}
# ---- coverage TSV: aggregate per-crate (first "crates/<name>/" segment,
# ---- leftmost match — the re.search semantics; absolute paths supported) ----
{
  fn = $1; count = $2 + 0; covered = $3 + 0
  if (match(fn, /crates\/[^\/]+\//)) {
    name = substr(fn, RSTART + 7, RLENGTH - 8)
    cnt[name] += count
    cov[name] += covered
  }
}
END {
  if (died) exit 1

  # default threshold must itself be a valid decimal
  parse_dec(default_thr)

  nthr = 0; for (k in thr_raw) nthr++
  # FAIL-CLOSED (2026-07-10, adversarial-review HIGH): zero parsed [crates]
  # entries would make the crate-presence assert below vacuous and silently
  # collapse EVERY crate onto the default floor.
  if (nthr == 0) {
    print "  ERROR: parsed 0 entries from the [crates] section of the thresholds TOML —"
    print "         refusing vacuous crate-presence pass. Check " tf
    exit 1
  }

  nrep = 0; for (k in cnt) nrep++
  # FAIL-CLOSED: an empty aggregation map means the report paths matched no
  # crate at all — checking nothing must never count as passing (Rule 11).
  if (nrep == 0) {
    print "  ERROR: gate matched 0 files under crates/ — refusing vacuous pass."
    print "         (coverage JSON paths did not contain \"crates/<name>/\")"
    exit 1
  }

  tn = sort_keys(thr_raw, tkeys)
  rn = sort_keys(cnt, rkeys)

  # CRATE-PRESENCE ASSERT (2026-07-10, audit follow-up row 9 — Rule 11
  # false-OK class): every crate listed in the thresholds TOML MUST be
  # present in the parsed report; any missing crate hard-fails, named.
  miss = 0
  for (i = 1; i <= tn; i++) {
    if (!(tkeys[i] in cnt)) {
      printf "  MISSING: %-15s listed in thresholds TOML but ABSENT from the coverage report\n", tkeys[i]
      miss = 1
    }
  }
  if (miss) {
    print ""
    print "  ERROR: crate-presence assert FAILED — refusing vacuous pass for the crate(s) above."
    print "         Causes: crate renamed/removed (update quality/crate-coverage-thresholds.toml"
    print "         in the same PR), an llvm-cov flag/filter change, or a partial coverage run."
    exit 1
  }

  # Inverse direction (warn-only, 2026-07-10): a crate in the report with no
  # explicit floor still rides the default — but name it loudly.
  for (i = 1; i <= rn; i++) {
    if (!(rkeys[i] in thr_raw)) {
      printf "  WARN: %-15s in report but has NO explicit floor in thresholds TOML — enforced at default (%s%%). Add a ratcheted floor.\n", rkeys[i], frepr(default_thr)
    }
  }

  failed = 0
  # EXACT-INTEGER ENVELOPE GUARD (review round 1 fix, 2026-07-18): the
  # cross-multiplication below is exact ONLY while every product stays
  # below 2^53. Beyond it, a 1-ulp rounding of either product can FLIP the
  # verdict — the review round 1 case: floor=99.999999, count=99999999,
  # covered=99999998 has true verdict FAIL (LHS=9999999800000000 <
  # RHS=9999999800000001) but both products exceed 2^53 and round to the
  # same double, yielding a false PASS. Real per-crate line counts are
  # ~1e6-class (the review round 1 envelope math: ~2.3e7-class counts already
  # approach the bound at d<=6), so a breach means implausible input:
  # FAIL LOUDLY rather than risk a rounded verdict.
  TWO53 = 9007199254740992
  for (i = 1; i <= rn; i++) {
    name = rkeys[i]
    traw = (name in thr_raw) ? thr_raw[name] : default_thr
    parse_dec(traw)
    if ((cov[name] > TWO53 / (100 * G_pow)) || \
        (G_scaled > 0 && cnt[name] > TWO53 / G_scaled)) {
      printf "  ERROR: EXACT-INTEGER ENVELOPE EXCEEDED for crate %s (count=%d covered=%d threshold=%s):\n", name, cnt[name], cov[name], frepr(traw)
      print "         a scaled product of the exact comparison would exceed 2^53, where IEEE"
      print "         rounding can flip the verdict by 1 ulp — refusing a possibly-rounded"
      print "         verdict (fail-closed). Real line counts are ~1e6-class; inspect the"
      print "         coverage JSON for corruption."
      exit 1
    }
    if (cnt[name] == 0) {
      # zero countable lines == 100% by definition (same as before)
      ok = (100 * G_pow >= G_scaled)
      pct = 100
    } else {
      # EXACT comparison: pct == threshold PASSES (ratchet = at least).
      ok = (cov[name] * 100 * G_pow >= G_scaled * cnt[name])
      # display value only — one IEEE division of exact integers, which is
      # the correctly-rounded double of the true ratio (same double the old
      # float(Fraction(covered*100, count)) produced)
      pct = (cov[name] * 100) / cnt[name]
    }
    st = ok ? "PASS" : "FAIL"
    if (!ok) failed = 1
    # 2-decimal display so a value just below the threshold can no longer
    # print as visually EQUAL to it.
    printf "  %s: %-15s %7.2f%% (threshold: %s%%)\n", st, name, pct, frepr(traw)
  }

  print ""
  if (failed) {
    print "  Per-crate coverage gate FAILED."
    exit 1
  }
  print "  All crates meet coverage thresholds."
  print "  Crate-presence assert: all " tn " threshold-listed crates found in the report."
}
' "$THRESHOLDS_FILE" "$TSV_FILE"

exit $?
