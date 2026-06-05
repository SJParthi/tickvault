#!/bin/bash
# plan-gate.selftest.sh — proves the design-first wall actually BLOCKS.
# Run: bash .claude/hooks/plan-gate.selftest.sh
# Exit 0 = all scenarios behaved as designed.
set -uo pipefail
GATE="$(cd "$(dirname "$0")" && pwd)/plan-gate.sh"
PASS=0; FAIL=0
check() { # <desc> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then echo "  ok   : $1 (exit $3)"; PASS=$((PASS+1));
  else echo "  FAIL : $1 (expected $2, got $3)"; FAIL=$((FAIL+1)); fi
}
new_repo() { # create temp repo, print its path (NO cd — caller cd's in its own shell)
  local d; d=$(mktemp -d)
  ( cd "$d"
    git init -q
    git config user.email t@t; git config user.name t
    git config commit.gpgsign false; git config tag.gpgsign false; git config gpg.program /bin/true
    mkdir -p crates/core/src; echo "fn a(){}" > crates/core/src/lib.rs
    git add -A; git commit -qm init >/dev/null 2>&1; git branch -q -M main
    git update-ref refs/remotes/origin/main HEAD
  )
  echo "$d"
}
SECTIONS='## Design
x
## Edge Cases
x
## Failure Modes
x
## Test Plan
x
## Rollback
x
## Observability
x'

run() { # <desc> <expected> -- builds a scenario via callback fn name $3 then runs gate
  local desc="$1" exp="$2" setup="$3" d
  d=$(new_repo)
  ( cd "$d"; "$setup" ) >/dev/null 2>&1
  bash "$GATE" "$d" >/dev/null 2>&1; check "$desc" "$exp" $?
  rm -rf "$d"
}

s_no_plan(){ echo "fn b(){}" >> crates/core/src/lib.rs; git add -A; git commit -qm "feat: add b"; }
s_exempt(){ echo "fn b(){}" >> crates/core/src/lib.rs; git add -A; git commit -qm "fix: tiny

PLAN-EXEMPT: one-line typo, no logic change"; }
s_full(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  { echo "# Plan"; echo "**Status:** APPROVED"; echo "$SECTIONS"; } > .claude/plans/active-plan.md
  git add -A; git commit -qm "feat: add b with plan"; }
s_partial(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  { echo "# Plan"; echo "**Status:** APPROVED"; echo "## Design"; echo x; echo "## Test Plan"; echo x; } > .claude/plans/active-plan.md
  git add -A; git commit -qm "feat: add b, partial plan"; }
s_draft(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  { echo "# Plan"; echo "**Status:** DRAFT"; echo "$SECTIONS"; } > .claude/plans/active-plan.md
  git add -A; git commit -qm "feat: add b, draft plan"; }
s_docs(){ echo "# readme" > README.md; git add -A; git commit -qm "docs: readme"; }

run "impl change + no plan -> BLOCK"                 2 s_no_plan
run "impl change + PLAN-EXEMPT -> PASS"              0 s_exempt
run "impl change + complete APPROVED plan -> PASS"   0 s_full
run "impl change + incomplete plan -> BLOCK"         2 s_partial
run "complete plan but Status=DRAFT -> BLOCK"        2 s_draft
run "docs-only change -> PASS"                       0 s_docs

echo "  plan-gate self-test: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
