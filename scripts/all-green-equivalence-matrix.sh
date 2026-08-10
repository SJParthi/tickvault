#!/usr/bin/env bash
# =============================================================================
# all-green-equivalence-matrix.sh — frozen fixture pin for the All Green
# verdict evaluator (ci.yml `all-green` job, "Verify every needed job
# succeeded" step).
#
# WHY (rust-only purge Phase 2a-1, 2026-07-18 — merge-gate-lock-2026-07-04.md
# §5.1): the verdict step's inline interpreter heredoc was ported to
# a jq+shell program with BYTE-EQUIVALENT semantics. The porting PR proved
# equivalence by running an 88-case fixture matrix through BOTH the old evaluator
# heredoc and the new jq program (byte-identical stdout + stderr + exit codes;
# evidence pasted in the PR body). This harness FREEZES that matrix: the
# expected exit code + stdout of every fixture (generated from the OLD evaluator
# evaluator, byte-verified against the jq program before merge) are pinned in
# the table below, and the harness re-runs the LIVE jq block — self-extracted
# from ci.yml between the ALL-GREEN-VERDICT-BEGIN/END markers — against every
# fixture. Any future drift in the jq program's sets, skip semantics, message
# bytes, or exit codes fails this harness (and it runs in CI inside the
# existing Repo Guards console-gate step — a step in an existing needed job,
# NOT a new job, per merge-gate-lock §5).
#
# NO INTERPRETER survives here by design: the reference side is the frozen table.
#
# Fixture matrix shape (88 cases):
#   A) 4 target jobs {commit-lint, design-first-wall, local-runtime-block,
#      test} x 5 results {success, failure, cancelled, skipped, missing}
#      x 4 events {pull_request, push, workflow_dispatch, schedule}  = 80
#   B) 8 extras: empty needs (x2 events), all three PR-only jobs skipped
#      (x3 events), failure+tolerated-skip, two-skips+cancel on
#      pull_request, missing-result+failure ordering.
#
# ABORTS loudly if the marker block is deleted, renamed, or extracts empty.
# Honest residual (the questdb-console-gate-matrix precedent): the harness
# executes the step's SCRIPT, not its YAML attributes — a `continue-on-error:`
# added to the ci.yml step would not be caught here.
# (2026-07-18 review-round-1 fix: fixture-row DELETION is no longer a residual
# — the count floor below is ratcheted at the full 88-row matrix.)
# =============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CI_YML="${REPO_ROOT}/.github/workflows/ci.yml"

command -v jq >/dev/null 2>&1 || { echo "FATAL: jq not found — required by the all-green verdict and this harness" >&2; exit 2; }
[ -f "$CI_YML" ] || { echo "FATAL: ${CI_YML} not found" >&2; exit 2; }

# --- self-extract the live verdict block from ci.yml ------------------------
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT
VERDICT_SH="${WORK_DIR}/all-green-verdict.sh"
{
  echo 'set -euo pipefail'
  sed -n '/ALL-GREEN-VERDICT-BEGIN/,/ALL-GREEN-VERDICT-END/p' "$CI_YML"
} > "$VERDICT_SH"
NONBLANK="$(grep -cv '^[[:space:]]*$' "$VERDICT_SH" || true)"
if [ "$NONBLANK" -lt 10 ]; then
  echo "FATAL: could not extract the All Green verdict block from ci.yml" >&2
  echo "       (ALL-GREEN-VERDICT-BEGIN/END markers missing or renamed)" >&2
  exit 2
fi

# --- fixture needs builder (deterministic; mirrors the real needs list) -----
#
# 2026-08-10 (gate-integrity audit): this array is no longer allowed to be a
# hand-maintained copy. It had DRIFTED - it listed 11 jobs while the live
# all-green `needs:` block had 12, missing `loom-concurrency`. The header two
# screens up claims the builder "mirrors the real needs list"; it had stopped
# doing so, silently, and nothing checked. The consequence was narrow but
# real: the frozen proof that the merge gate's verdict evaluator is correct
# never exercised the newest gating job, and the blind spot would have widened
# with every job added.
#
# Derived from ci.yml now, then asserted against the recorded list so that a
# NEW job is a loud, deliberate update of this file rather than a silent
# omission. Adding a job that defaults to success does not change any frozen
# expected stdout (only NON-success jobs are named in the output), so the 88
# fixtures stand unchanged - verified by running them.
JOBS_RECORDED=(local-runtime-block commit-lint design-first-wall deploy-lint build-and-verify test test-trading-groww-orders security-and-audit coverage-and-perf repo-guards dhat-zero-alloc loom-concurrency)

CI_YML="${CI_YML:-.github/workflows/ci.yml}"
mapfile -t JOBS_LIVE < <(
  awk '
    /^  all-green:/        { injob = 1 }
    injob && /^    needs:/ { inneeds = 1; next }
    inneeds && /^      - / { sub(/^      - /, ""); print; next }
    inneeds && !/^      - / { exit }
  ' "$CI_YML"
)

if [ "${#JOBS_LIVE[@]}" -lt 5 ]; then
  echo "FATAL: could not extract the all-green needs list from ${CI_YML} (found ${#JOBS_LIVE[@]})" >&2
  echo "       Refusing to run the matrix against a list this file invented." >&2
  exit 2
fi

if [ "$(printf '%s\n' "${JOBS_RECORDED[@]}" | sort)" != "$(printf '%s\n' "${JOBS_LIVE[@]}" | sort)" ]; then
  echo "FATAL: the all-green needs list in ${CI_YML} no longer matches JOBS_RECORDED here." >&2
  echo "       live:     $(printf '%s ' "${JOBS_LIVE[@]}")" >&2
  echo "       recorded: $(printf '%s ' "${JOBS_RECORDED[@]}")" >&2
  echo "       A job added to the merge gate but not to this proof means the" >&2
  echo "       equivalence matrix never exercises it. Update JOBS_RECORDED and" >&2
  echo "       re-run the fixtures." >&2
  exit 2
fi

JOBS=("${JOBS_LIVE[@]}")

# mk_needs "job1=result job2=missing ..." -> compact needs JSON on stdout.
# Every job defaults to result=success; `missing` emits an empty meta object
# (no result key — the old evaluator's meta.get("result") -> None path).
# The special spec "EMPTY" yields an empty needs map.
mk_needs() {
  local overrides="$1"
  if [ "$overrides" = "EMPTY" ]; then
    printf '{}'
    return
  fi
  local args=("-n")
  local prog='{}'
  local i=0
  local j res missing ov k v
  for j in "${JOBS[@]}"; do
    res="success"; missing=0
    for ov in $overrides; do
      k="${ov%%=*}"; v="${ov#*=}"
      if [ "$k" = "$j" ]; then
        if [ "$v" = "missing" ]; then missing=1; else res="$v"; fi
      fi
    done
    if [ "$missing" = 1 ]; then
      args+=("--arg" "j$i" "$j")
      prog+=" + {(\$j$i): {}}"
    else
      args+=("--arg" "j$i" "$j" "--arg" "r$i" "$res")
      prog+=" + {(\$j$i): {result: \$r$i}}"
    fi
    i=$((i+1))
  done
  jq -c "${args[@]}" "$prog"
}

# --- drive every frozen fixture through the live block ----------------------
total=0; pass=0; fail=0
while IFS=$'\t' read -r name event overrides expect_exit expect_out; do
  [ -n "$name" ] || continue
  total=$((total+1))
  needs="$(mk_needs "$overrides")"
  set +e
  actual_out="$(NEEDS_JSON="$needs" EVENT_NAME="$event" bash "$VERDICT_SH" 2>"${WORK_DIR}/stderr")"
  actual_exit=$?
  set -e
  actual_err="$(cat "${WORK_DIR}/stderr")"
  if [ "$actual_out" = "$expect_out" ] && [ "$actual_exit" = "$expect_exit" ] && [ -z "$actual_err" ]; then
    pass=$((pass+1))
  else
    fail=$((fail+1))
    echo "MISMATCH: ${name} (event=${event}, overrides=[${overrides}])" >&2
    echo "  expected: exit=${expect_exit} stdout=[${expect_out}]" >&2
    echo "  actual:   exit=${actual_exit} stdout=[${actual_out}] stderr=[${actual_err}]" >&2
  fi
done < <(sed -n '/^# FIXTURES-BEGIN/,/^# FIXTURES-END/p' "${BASH_SOURCE[0]}" | grep -v '^#')

echo "all-green-equivalence-matrix: ${pass}/${total} fixtures match the frozen expected outputs (${fail} mismatches)"
# 2026-07-18 ratchet (review round 1): the floor is pinned at the CURRENT
# fixture count (88) and only moves UP as fixtures are added — deleting any
# fixture row below 88 fails the guard (the 40-row floor left rows 41..88
# mechanically deletable, a weakening vector on the merge choke point).
if [ "$total" -lt 88 ]; then
  echo "FATAL: fixture table shrank below the 88-case ratchet floor (found ${total})" >&2
  exit 2
fi
[ "$fail" -eq 0 ] || exit 1
exit 0

# =============================================================================
# Frozen fixture table (TAB-separated):
#   name <TAB> event <TAB> overrides <TAB> expected_exit <TAB> expected_stdout
# Generated 2026-07-18 from the OLD evaluator; every row byte-verified
# identical (stdout + stderr + exit) against the new jq program before merge.
# Do NOT hand-edit expected outputs — a semantics change needs its own dated
# merge-gate-lock quote first (§5.1).
# =============================================================================
# FIXTURES-BEGIN
A:commit-lint=success	pull_request	commit-lint=success	0	All Green: every needed job succeeded (event=pull_request).
A:commit-lint=success	push	commit-lint=success	0	All Green: every needed job succeeded (event=push).
A:commit-lint=success	workflow_dispatch	commit-lint=success	0	All Green: every needed job succeeded (event=workflow_dispatch).
A:commit-lint=success	schedule	commit-lint=success	0	All Green: every needed job succeeded (event=schedule).
A:commit-lint=failure	pull_request	commit-lint=failure	1	::error::All Green FAILED — non-success needed jobs: commit-lint=failure
A:commit-lint=failure	push	commit-lint=failure	1	::error::All Green FAILED — non-success needed jobs: commit-lint=failure
A:commit-lint=failure	workflow_dispatch	commit-lint=failure	1	::error::All Green FAILED — non-success needed jobs: commit-lint=failure
A:commit-lint=failure	schedule	commit-lint=failure	1	::error::All Green FAILED — non-success needed jobs: commit-lint=failure
A:commit-lint=cancelled	pull_request	commit-lint=cancelled	1	::error::All Green FAILED — non-success needed jobs: commit-lint=cancelled
A:commit-lint=cancelled	push	commit-lint=cancelled	1	::error::All Green FAILED — non-success needed jobs: commit-lint=cancelled
A:commit-lint=cancelled	workflow_dispatch	commit-lint=cancelled	1	::error::All Green FAILED — non-success needed jobs: commit-lint=cancelled
A:commit-lint=cancelled	schedule	commit-lint=cancelled	1	::error::All Green FAILED — non-success needed jobs: commit-lint=cancelled
A:commit-lint=skipped	pull_request	commit-lint=skipped	1	::error::All Green FAILED — non-success needed jobs: commit-lint=skipped
A:commit-lint=skipped	push	commit-lint=skipped	0	All Green: every needed job succeeded (event=push).
A:commit-lint=skipped	workflow_dispatch	commit-lint=skipped	0	All Green: every needed job succeeded (event=workflow_dispatch).
A:commit-lint=skipped	schedule	commit-lint=skipped	1	::error::All Green FAILED — non-success needed jobs: commit-lint=skipped
A:commit-lint=missing	pull_request	commit-lint=missing	1	::error::All Green FAILED — non-success needed jobs: commit-lint=None
A:commit-lint=missing	push	commit-lint=missing	1	::error::All Green FAILED — non-success needed jobs: commit-lint=None
A:commit-lint=missing	workflow_dispatch	commit-lint=missing	1	::error::All Green FAILED — non-success needed jobs: commit-lint=None
A:commit-lint=missing	schedule	commit-lint=missing	1	::error::All Green FAILED — non-success needed jobs: commit-lint=None
A:design-first-wall=success	pull_request	design-first-wall=success	0	All Green: every needed job succeeded (event=pull_request).
A:design-first-wall=success	push	design-first-wall=success	0	All Green: every needed job succeeded (event=push).
A:design-first-wall=success	workflow_dispatch	design-first-wall=success	0	All Green: every needed job succeeded (event=workflow_dispatch).
A:design-first-wall=success	schedule	design-first-wall=success	0	All Green: every needed job succeeded (event=schedule).
A:design-first-wall=failure	pull_request	design-first-wall=failure	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=failure
A:design-first-wall=failure	push	design-first-wall=failure	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=failure
A:design-first-wall=failure	workflow_dispatch	design-first-wall=failure	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=failure
A:design-first-wall=failure	schedule	design-first-wall=failure	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=failure
A:design-first-wall=cancelled	pull_request	design-first-wall=cancelled	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=cancelled
A:design-first-wall=cancelled	push	design-first-wall=cancelled	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=cancelled
A:design-first-wall=cancelled	workflow_dispatch	design-first-wall=cancelled	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=cancelled
A:design-first-wall=cancelled	schedule	design-first-wall=cancelled	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=cancelled
A:design-first-wall=skipped	pull_request	design-first-wall=skipped	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=skipped
A:design-first-wall=skipped	push	design-first-wall=skipped	0	All Green: every needed job succeeded (event=push).
A:design-first-wall=skipped	workflow_dispatch	design-first-wall=skipped	0	All Green: every needed job succeeded (event=workflow_dispatch).
A:design-first-wall=skipped	schedule	design-first-wall=skipped	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=skipped
A:design-first-wall=missing	pull_request	design-first-wall=missing	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=None
A:design-first-wall=missing	push	design-first-wall=missing	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=None
A:design-first-wall=missing	workflow_dispatch	design-first-wall=missing	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=None
A:design-first-wall=missing	schedule	design-first-wall=missing	1	::error::All Green FAILED — non-success needed jobs: design-first-wall=None
A:local-runtime-block=success	pull_request	local-runtime-block=success	0	All Green: every needed job succeeded (event=pull_request).
A:local-runtime-block=success	push	local-runtime-block=success	0	All Green: every needed job succeeded (event=push).
A:local-runtime-block=success	workflow_dispatch	local-runtime-block=success	0	All Green: every needed job succeeded (event=workflow_dispatch).
A:local-runtime-block=success	schedule	local-runtime-block=success	0	All Green: every needed job succeeded (event=schedule).
A:local-runtime-block=failure	pull_request	local-runtime-block=failure	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=failure
A:local-runtime-block=failure	push	local-runtime-block=failure	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=failure
A:local-runtime-block=failure	workflow_dispatch	local-runtime-block=failure	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=failure
A:local-runtime-block=failure	schedule	local-runtime-block=failure	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=failure
A:local-runtime-block=cancelled	pull_request	local-runtime-block=cancelled	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=cancelled
A:local-runtime-block=cancelled	push	local-runtime-block=cancelled	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=cancelled
A:local-runtime-block=cancelled	workflow_dispatch	local-runtime-block=cancelled	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=cancelled
A:local-runtime-block=cancelled	schedule	local-runtime-block=cancelled	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=cancelled
A:local-runtime-block=skipped	pull_request	local-runtime-block=skipped	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=skipped
A:local-runtime-block=skipped	push	local-runtime-block=skipped	0	All Green: every needed job succeeded (event=push).
A:local-runtime-block=skipped	workflow_dispatch	local-runtime-block=skipped	0	All Green: every needed job succeeded (event=workflow_dispatch).
A:local-runtime-block=skipped	schedule	local-runtime-block=skipped	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=skipped
A:local-runtime-block=missing	pull_request	local-runtime-block=missing	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=None
A:local-runtime-block=missing	push	local-runtime-block=missing	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=None
A:local-runtime-block=missing	workflow_dispatch	local-runtime-block=missing	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=None
A:local-runtime-block=missing	schedule	local-runtime-block=missing	1	::error::All Green FAILED — non-success needed jobs: local-runtime-block=None
A:test=success	pull_request	test=success	0	All Green: every needed job succeeded (event=pull_request).
A:test=success	push	test=success	0	All Green: every needed job succeeded (event=push).
A:test=success	workflow_dispatch	test=success	0	All Green: every needed job succeeded (event=workflow_dispatch).
A:test=success	schedule	test=success	0	All Green: every needed job succeeded (event=schedule).
A:test=failure	pull_request	test=failure	1	::error::All Green FAILED — non-success needed jobs: test=failure
A:test=failure	push	test=failure	1	::error::All Green FAILED — non-success needed jobs: test=failure
A:test=failure	workflow_dispatch	test=failure	1	::error::All Green FAILED — non-success needed jobs: test=failure
A:test=failure	schedule	test=failure	1	::error::All Green FAILED — non-success needed jobs: test=failure
A:test=cancelled	pull_request	test=cancelled	1	::error::All Green FAILED — non-success needed jobs: test=cancelled
A:test=cancelled	push	test=cancelled	1	::error::All Green FAILED — non-success needed jobs: test=cancelled
A:test=cancelled	workflow_dispatch	test=cancelled	1	::error::All Green FAILED — non-success needed jobs: test=cancelled
A:test=cancelled	schedule	test=cancelled	1	::error::All Green FAILED — non-success needed jobs: test=cancelled
A:test=skipped	pull_request	test=skipped	1	::error::All Green FAILED — non-success needed jobs: test=skipped
A:test=skipped	push	test=skipped	1	::error::All Green FAILED — non-success needed jobs: test=skipped
A:test=skipped	workflow_dispatch	test=skipped	1	::error::All Green FAILED — non-success needed jobs: test=skipped
A:test=skipped	schedule	test=skipped	1	::error::All Green FAILED — non-success needed jobs: test=skipped
A:test=missing	pull_request	test=missing	1	::error::All Green FAILED — non-success needed jobs: test=None
A:test=missing	push	test=missing	1	::error::All Green FAILED — non-success needed jobs: test=None
A:test=missing	workflow_dispatch	test=missing	1	::error::All Green FAILED — non-success needed jobs: test=None
A:test=missing	schedule	test=missing	1	::error::All Green FAILED — non-success needed jobs: test=None
B:empty-needs	pull_request	EMPTY	0	All Green: every needed job succeeded (event=pull_request).
B:empty-needs	push	EMPTY	0	All Green: every needed job succeeded (event=push).
B:all-pr-only-skipped	push	commit-lint=skipped design-first-wall=skipped local-runtime-block=skipped	0	All Green: every needed job succeeded (event=push).
B:all-pr-only-skipped	workflow_dispatch	commit-lint=skipped design-first-wall=skipped local-runtime-block=skipped	0	All Green: every needed job succeeded (event=workflow_dispatch).
B:all-pr-only-skipped	pull_request	commit-lint=skipped design-first-wall=skipped local-runtime-block=skipped	1	::error::All Green FAILED — non-success needed jobs: commit-lint=skipped, design-first-wall=skipped, local-runtime-block=skipped
B:fail+tolerated-skip	push	test=failure commit-lint=skipped	1	::error::All Green FAILED — non-success needed jobs: test=failure
B:two-skips+cancel	pull_request	commit-lint=skipped design-first-wall=skipped test=cancelled	1	::error::All Green FAILED — non-success needed jobs: commit-lint=skipped, design-first-wall=skipped, test=cancelled
B:missing+failure	push	test=missing build-and-verify=failure	1	::error::All Green FAILED — non-success needed jobs: build-and-verify=failure, test=None
# FIXTURES-END
