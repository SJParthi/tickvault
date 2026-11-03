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
# 2026-07-13 hardening: a bad project dir used to `exit 0` silently — a guard
# that cannot evaluate must refuse loudly, never pass.
cd "$PROJECT_DIR" || {
  echo "  FAIL: pub-fn-test-guard: cannot cd to project dir '$PROJECT_DIR' — refusing to pass silently" >&2
  exit 2
}

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
  if [ "$MODE" = "all" ]; then
    # 2026-07-13 hardening: in "all" mode an empty `find crates` means the
    # guard is running somewhere without the workspace — refuse loudly.
    # (In "staged" mode an empty staged-file list is a legitimate clean pass.)
    echo "  FAIL: pub-fn-test-guard (all mode): no crates/*.rs found under '$PROJECT_DIR' — guard cannot evaluate; refusing to pass silently" >&2
    exit 2
  fi
  exit 0
fi

# A private, size-limited text index is optional: unsupported encodings, binary
# input, unusual filenames or any index failure retain the legacy search path.
TEST_INDEX=""
TEST_CHARSET=$(locale charmap 2>/dev/null || true)
case "$TEST_CHARSET" in
  UTF-8|UTF8|ANSI_X3.4-1968|US-ASCII)
    if command -v iconv >/dev/null 2>&1 && [ -z "${GREP_OPTIONS:-}" ]; then
      TEST_INDEX=$(mktemp "${TMPDIR:-/tmp}/tickvault-pubfn.XXXXXXXX" 2>/dev/null || true)
    fi
    ;;
esac
trap '[ -z "$TEST_INDEX" ] || rm -f -- "$TEST_INDEX"' EXIT
trap 'exit 2' HUP INT TERM

# Count untested pub fns
count_untested() {
  local scan_files="$1"
  local scan_mode="$2"
  local count=0
  local test_files="" test_crate="" exemption_lines=""
  local matched_names=" " unmatched_names=" " fast_test_paths=0 indexed_tests=0

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

    # Read marker positions once, rather than rereading the file for every fn.
    # Literal marker matching and immediately preceding-line semantics stay intact.
    exemption_lines=""
    if [ "$scan_mode" = "all" ]; then
      exemption_lines=$'\n'$(sed -n '/TEST-EXEMPT:/=' "$full_path" 2>/dev/null || true)$'\n'
    fi

    while IFS= read -r line; do
      [ -z "$line" ] && continue

      [[ "$line" =~ .*fn\ ([a-z_][a-z0-9_]*).* ]] || continue
      local fn_name="${BASH_REMATCH[1]}"

      # Skip structural functions
      case "$fn_name" in
        main|new|default|from|into|try_from|try_into|fmt|drop|clone|eq|hash|partial_cmp|cmp)
          continue
          ;;
      esac

      # Check for TEST-EXEMPT
      if [ "$scan_mode" = "all" ]; then
        local line_num
        line_num="${line%%:*}"
        if [ -n "$line_num" ] && [ "$line_num" -gt 1 ]; then
          local prev_line=$((line_num - 1))
          if [[ "$exemption_lines" == *$'\n'"$prev_line"$'\n'* ]]; then
            continue
          fi
        fi
      fi

      # Search for matching test
      local test_found
      # Cache verdicts, not declarations: duplicate untested fns still count.
      # Space-delimited ASCII names work on Bash 3; no associative arrays needed.
      if [ "$test_crate" != "$crate_dir" ]; then
        test_files=$(grep -r "#\[test\]" "$crate_dir" --include='*.rs' -l 2>/dev/null || true)
        test_crate="$crate_dir"
        matched_names=" " unmatched_names=" "
        # Preserve legacy xargs quirks for unusual names, including tokens that
        # could be interpreted as grep options. Ordinary names can stop early.
        local flat_paths="${test_files//$'\n'/}"
        fast_test_paths=1
        if [[ "$flat_paths" =~ [[:space:]] ]] ||
           [[ "$test_files" == -* || "$test_files" == *$'\n-'* ||
              "$test_files" == *\'* || "$test_files" == *\"* || "$test_files" == *\\* ]]; then
          fast_test_paths=0
        fi
        indexed_tests=0
        if [ "$fast_test_paths" -eq 1 ] && [ -n "$TEST_INDEX" ] && [ -n "$test_files" ]; then
          # Capture all per-file counts: /dev/null enables grep early-exit,
          # and a text sibling can mask a binary file in aggregate exit status.
          # Any zero count or invalid encoding retains legacy binary semantics.
          local text_counts="" zero_status=0
          if text_counts=$(printf '%s\n' "$test_files" | xargs grep -I -c '^' 2>/dev/null); then
            # No producer pipe: grep -q may stop early. Only status 1 proves
            # there was no zero; a validation error also retains legacy search.
            grep -qE '(^|:)0$' <<< "$text_counts" 2>/dev/null || zero_status=$?
            if [ "$zero_status" -eq 1 ] &&
               printf '%s\n' "$test_files" | xargs iconv -f "$TEST_CHARSET" -t "$TEST_CHARSET" >/dev/null 2>&1 &&
               (ulimit -f 65536 && printf '%s\n' "$test_files" | xargs grep -h -e test -e 'fn ' > "$TEST_INDEX") 2>/dev/null; then
              indexed_tests=1
            fi
          fi
        fi
      fi
      if [[ "$matched_names" == *" $fn_name "* ]]; then
        test_found=match
      elif [[ "$unmatched_names" == *" $fn_name "* ]]; then
        test_found=""
      else
        if [ "$indexed_tests" -eq 1 ]; then
          # Every legacy match lies on a line containing test or "fn ". The
          # index preserves those entire lines and file-boundary newlines; the
          # exact legacy expression still decides each function's coverage.
          test_found=""
          local lookup_status=0
          grep -q "fn test_.*${fn_name}\|fn ${fn_name}_\|test.*${fn_name}" "$TEST_INDEX" 2>/dev/null || lookup_status=$?
          if [ "$lookup_status" -eq 0 ]; then
            test_found=match
          elif [ "$lookup_status" -ne 1 ]; then
            indexed_tests=0
          fi
        fi
        if [ "$indexed_tests" -eq 0 ]; then
          test_found=$(printf '%s\n' "$test_files" | \
            xargs grep -l "fn test_.*${fn_name}\|fn ${fn_name}_\|test.*${fn_name}" 2>/dev/null || true)
        fi
        if [ -n "$test_found" ]; then
          matched_names+="$fn_name "
        else
          unmatched_names+="$fn_name "
        fi
      fi

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
    # FAIL CLOSED in CI (2026-08-07) — see test-count-guard.sh for the full
    # rationale. A ratchet with no baseline compares nothing and passes
    # vacuously; the baseline is tracked in git now.
    if [ -n "${CI:-}" ]; then
      echo "  FAIL: untested-pubfn baseline missing in CI: $BASELINE_FILE" >&2
      echo "  This guard is a RATCHET; with no baseline it cannot compare and would pass vacuously." >&2
      echo "  The baseline is tracked in git — restore it rather than regenerating." >&2
      exit 1
    fi
    # First run (local dev only): establish baseline
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
    # Baseline is TRACKED IN GIT since 2026-08-07 so the ratchet is real in CI.
    # Auto-writing it here would dirty every working tree on every machine whose
    # count differs -- exactly the breakage that forced the 2026-07-13 untracking
    # (see .claude/rules/project/enforcement.md). Ratchet movement is now a
    # deliberate, PR-visible edit, same discipline as
    # quality/crate-coverage-thresholds.toml. Improvement still PASSES.
    echo "  PASS: Untested pub fn count decreased ($BASELINE -> $CURRENT_COUNT)." >&2
    echo "  Ratchet it in THIS PR:  echo $CURRENT_COUNT > $BASELINE_FILE" >&2
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
