#!/bin/bash
# plan-gate.selftest.sh — proves the design-first wall actually BLOCKS.
# Run: bash .claude/hooks/plan-gate.selftest.sh
# Exit 0 = all scenarios behaved as designed.
#
# Covers the original 6 scenarios PLUS the adversarial-review hardening
# (2026-06-05): untracked new .rs (C1), digit-crate regex (H2), stale plan that
# doesn't reference the change (H3), empty-body sections (M4), multi-file
# PLAN-EXEMPT cap (M5) — PLUS the stale-plan-pile cap (V7, 2026-07-10): more
# than PLAN_GATE_MAX_ACTIVE (default 5) active plans → BLOCK even when one of
# them would individually satisfy the change (accumulation = vacuous wall).
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
# A complete, APPROVED plan whose Design body references the `core` crate.
full_plan() { cat <<'EOF'
# Plan
**Status:** APPROVED
## Design
This change touches the core crate; here is the approach.
## Edge Cases
empty input, boundary values
## Failure Modes
network down, parse error
## Test Plan
unit + property tests
## Rollback
revert the commit
## Observability
counter + tracing span
EOF
}

run() { # <desc> <expected> <setup-fn>
  local desc="$1" exp="$2" setup="$3" d
  d=$(new_repo)
  ( cd "$d"; "$setup" ) >/dev/null 2>&1
  bash "$GATE" "$d" >/dev/null 2>&1; check "$desc" "$exp" $?
  rm -rf "$d"
}

s_no_plan(){ echo "fn b(){}" >> crates/core/src/lib.rs; git add -A; git commit -qm "feat: add b"; }
s_exempt(){ echo "fn b(){}" >> crates/core/src/lib.rs; git add -A; git commit -qm "fix: tiny

PLAN-EXEMPT: one-line typo, no logic change"; }
s_exempt_multi(){ echo "fn b(){}" >> crates/core/src/lib.rs
  mkdir -p crates/trading/src; echo "fn c(){}" > crates/trading/src/lib.rs
  git add -A; git commit -qm "feat: two files

PLAN-EXEMPT: trying to sneak a feature through"; }
s_full(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  full_plan > .claude/plans/active-plan.md
  git add -A; git commit -qm "feat: add b with plan"; }
s_partial(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  { echo "# Plan"; echo "**Status:** APPROVED"; echo "## Design"; echo "touches core"; echo "## Test Plan"; echo "x"; } > .claude/plans/active-plan.md
  git add -A; git commit -qm "feat: add b, partial plan"; }
s_draft(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  full_plan | sed 's/APPROVED/DRAFT/' > .claude/plans/active-plan.md
  git add -A; git commit -qm "feat: add b, draft plan"; }
s_docs(){ echo "# readme" > README.md; git add -A; git commit -qm "docs: readme"; }
s_untracked(){ echo "fn evil(){}" > crates/core/src/evil.rs; }   # NEW file, never git-added (C1)
s_emptybody(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  { echo "# Plan"; echo "**Status:** APPROVED"
    echo "## Design"; echo "## Edge Cases"; echo "## Failure Modes"
    echo "## Test Plan"; echo "## Rollback"; echo "## Observability"; } > .claude/plans/active-plan.md
  git add -A; git commit -qm "feat: add b, hollow plan"; }
s_digitcrate(){ mkdir -p crates/core2/src; echo "fn z(){}" > crates/core2/src/a.rs
  git add -A; git commit -qm "feat: core2 module, no plan"; }
s_wrongcrate(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  full_plan | sed 's/the core crate/the trading crate/; s/touches the core/touches the trading/' > .claude/plans/active-plan.md
  git add -A; git commit -qm "feat: core change, trading plan"; }
# V7: 6 active plans (all individually valid + referencing core) — the PILE
# itself must BLOCK, proving accumulation can no longer buy a vacuous pass.
s_pile(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  for i in 1 2 3 4 5 6; do full_plan > ".claude/plans/active-plan-pile-$i.md"; done
  git add -A; git commit -qm "feat: add b, six stale plans"; }
# V7 boundary: exactly 5 plans (the cap), one satisfying -> still PASS.
s_at_cap(){ echo "fn b(){}" >> crates/core/src/lib.rs; mkdir -p .claude/plans
  for i in 1 2 3 4 5; do full_plan > ".claude/plans/active-plan-pile-$i.md"; done
  git add -A; git commit -qm "feat: add b, five plans at cap"; }
# V7 knob: same 6-plan pile but PLAN_GATE_MAX_ACTIVE raised -> PASS (selftest
# knob only; CI never sets it, so the server-side wall enforces the default).
s_pile_override(){ s_pile; }

run "impl change + no plan -> BLOCK"                  2 s_no_plan
run "impl change + PLAN-EXEMPT (1 file) -> PASS"      0 s_exempt
run "PLAN-EXEMPT covering 2 impl files -> BLOCK"      2 s_exempt_multi
run "impl change + complete APPROVED plan -> PASS"    0 s_full
run "impl change + incomplete plan -> BLOCK"          2 s_partial
run "complete plan but Status=DRAFT -> BLOCK"         2 s_draft
run "docs-only change -> PASS"                        0 s_docs
run "untracked NEW .rs file, no plan -> BLOCK (C1)"   2 s_untracked
run "hollow plan (empty sections) -> BLOCK (M4)"      2 s_emptybody
run "digit-crate (core2) src, no plan -> BLOCK (H2)"  2 s_digitcrate
run "plan references wrong crate -> BLOCK (H3)"       2 s_wrongcrate
run "6 active plans (> cap 5) -> BLOCK (V7)"          2 s_pile
run "exactly 5 plans (at cap), one valid -> PASS (V7)" 0 s_at_cap
# V7 knob cases — need the env var on the gate invocation itself. Guard the
# setup (an impl change MUST exist, else the docs-only arm would pass vacuously).
d=$(new_repo)
( cd "$d"; s_pile_override ) >/dev/null 2>&1
if [ "$(ls "$d"/.claude/plans/active-plan-pile-*.md 2>/dev/null | wc -l)" -ne 6 ]; then
  echo "  FAIL : V7 knob setup broken (expected 6 pile plans)"; FAIL=$((FAIL+1))
else
  PLAN_GATE_MAX_ACTIVE=10 bash "$GATE" "$d" >/dev/null 2>&1
  check "6 plans + PLAN_GATE_MAX_ACTIVE=10 -> PASS (V7 knob)" 0 $?
  PLAN_GATE_MAX_ACTIVE=garbage bash "$GATE" "$d" >/dev/null 2>&1
  check "6 plans + non-integer knob -> BLOCK (V7 fail-closed)" 2 $?
fi
rm -rf "$d"

echo "  plan-gate self-test: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
