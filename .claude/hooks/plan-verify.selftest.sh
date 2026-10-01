#!/bin/bash
# plan-verify.selftest.sh — proves plan-verify.sh checks real plan files.
# Run: bash .claude/hooks/plan-verify.selftest.sh
# Exit 0 = every scenario behaved as designed.
#
# Until 2026-10-01 plan-verify.sh read only `active-plan.md`, a name no plan
# in this repository uses, so it passed every real plan without reading it.
# The first scenario below is that defect: an `active-plan-<slug>.md` with a
# ticked item whose test does not exist must FAIL.
set -uo pipefail
VERIFY="$(cd "$(dirname "$0")" && pwd)/plan-verify.sh"
PASS=0; FAIL=0
check() { # <desc> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then echo "  ok   : $1 (exit $3)"; PASS=$((PASS+1));
  else echo "  FAIL : $1 (expected $2, got $3)"; FAIL=$((FAIL+1)); fi
}
new_tree() {
  local d; d=$(mktemp -d)
  mkdir -p "$d/crates/core/src" "$d/crates/core/tests" "$d/.claude/plans"
  cat > "$d/crates/core/src/lib.rs" <<'EOF'
fn real_test_name() {}
fn other_real_test() {}
EOF
  echo "fn guard_case() {}" > "$d/crates/core/tests/some_guard.rs"
  echo "$d"
}
plan() { # <dir> <file> <status> <body>
  printf '# Plan\n\n**Status:** %s\n\n%s\n' "$3" "$4" > "$1/.claude/plans/$2"
}
run() { # <desc> <expected> <dir> [mode] [plan files...]
  local desc="$1" exp="$2" d="$3"; shift 3
  bash "$VERIFY" "$d" "$@" >/dev/null 2>&1; check "$desc" "$exp" $?
}

d=$(new_tree)
plan "$d" active-plan-x.md APPROVED "- [x] item
  - Files: crates/core/src/lib.rs
  - Tests: does_not_exist_anywhere"
run "slug-named plan, ticked item with a missing test -> FAIL" 2 "$d"; rm -rf "$d"

d=$(new_tree)
run "no active plan -> PASS" 0 "$d"; rm -rf "$d"

d=$(new_tree)
plan "$d" active-plan-x.md IN_PROGRESS "- [x] done item
  - Files: \`crates/core/src/lib.rs\` (the fold)
  - Tests: \`real_test_name\` (bite-proven, both ways), \`core::module::other_real_test\`
- [ ] open item
  - Tests: not_written_yet"
run "in-progress plan, ticked items real, open items listed -> PASS" 0 "$d"; rm -rf "$d"

d=$(new_tree)
plan "$d" active-plan-x.md VERIFIED "- [x] done
  - Tests: real_test_name
- [ ] still open"
run "VERIFIED plan with an unticked item -> FAIL" 2 "$d"; rm -rf "$d"

d=$(new_tree)
plan "$d" active-plan-x.md APPROVED "- [x] done
  - Files: crates/core/src/gone.rs"
run "ticked item names a missing file -> FAIL" 2 "$d"; rm -rf "$d"

d=$(new_tree)
plan "$d" active-plan-x.md APPROVED "- [x] done
  - Tests: some_guard guard_case, crates/core/tests/some_guard.rs, N/A — docs only"
run "test file names, file+case pairs and N/A are understood -> PASS" 0 "$d"; rm -rf "$d"

d=$(new_tree)
plan "$d" active-plan-x.md APPROVED "- [x] done
  - Tests: real_test_name"
run "push mode on a plan that is not VERIFIED -> FAIL" 2 "$d" push; rm -rf "$d"

d=$(new_tree)
plan "$d" active-plan-a.md APPROVED "- [x] a
  - Tests: real_test_name"
plan "$d" active-plan-b.md APPROVED "- [x] b
  - Tests: missing_in_b"
run "named plan file checks only that plan -> PASS" 0 "$d" verify "$d/.claude/plans/active-plan-a.md"
run "every active-plan*.md is checked by default -> FAIL" 2 "$d"; rm -rf "$d"

d=$(new_tree)
printf '# Plan\n\n- [x] a\n' > "$d/.claude/plans/active-plan-x.md"
run "plan without a Status field -> FAIL" 2 "$d"; rm -rf "$d"

echo "  plan-verify self-test: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
