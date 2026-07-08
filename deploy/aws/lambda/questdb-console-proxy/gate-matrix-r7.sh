#!/usr/bin/env bash
# gate-matrix-r7.sh — behavioral scenario matrix for the terraform-apply.yml
# step "Deploy gate: console shell GET / must return 200 within 5s".
#
# Provenance (fixer rounds 7-8, 2026-07-06/07): produced in the 2026-07-06
# fixer-round-7 session scratchpad and COMMITTED here in round 8 because the
# plan cites it as the gate's ONLY behavioral proof — a session-ephemeral
# scratchpad path was a dangling reference for every future reader of HEAD
# (zero-loss-guarantee-charter §4 evidence discipline; the identical fix
# pattern as repro-evidence.md alongside). Round-8 changes vs the scratchpad
# original, so the harness is re-runnable from HEAD instead of frozen:
#   (a) the gate script is EXTRACTED from .github/workflows/terraform-apply.yml
#       at run time (yaml.safe_load, GitHub `${{ env.* }}` expressions
#       substituted, FAILS if any expression survives) — no manual
#       /tmp/gate-step.sh prep step;
#   (b) scenario L added: pending-at-both-samples skip must name the START
#       transition (box booting), not the auto-stop window (round-8 message
#       fix), and scenario E now pins the retained auto-stop wording;
#   (c) per-scenario PASS/FAIL is counted and the harness exits non-zero on
#       any failure.
# It lives alongside handler.py because the gate it exercises is this lambda
# pair's deploy gate; it is EXCLUDED from the lambda zip (questdb-console.tf
# archive_file excludes — literal filenames; round-10 correction: the
# provider DOES support glob excludes, see the questdb-console.tf comment).
#
# Round-10 changes (2026-07-07): (a) $WORK is cleaned up via an EXIT trap
# (previously leaked one temp tree per run); (b) a grep-miss on a scenario
# whose rc matched now REPLACES the PASS verdict instead of appending to it
# (rows previously printed the contradictory "PASS FAIL(missing: ...)" —
# exit code was already correct); (c) scenario M pins the 000-pre-grace
# mid-grace-auto-stop skip wording (the hang-class cause enumeration added
# to terraform-apply.yml in round 10 — scenario D pins only the 503-pre-grace
# variant); (d) the scenario count in the green line is computed, not
# hardcoded.
#
# Round-11 changes (2026-07-08): (a) an extraction FAILURE now ABORTS with
# the single loud message instead of running all scenarios against an empty
# $GATE_SRC — under `set -u` alone a python non-zero exit did NOT stop the
# harness, so a deleted/renamed gate step produced one stderr line buried
# under 14 misleading "FAIL(rc=0 want=1)" rows (exit code was already
# non-zero, so the merge ratchet held; triage honesty only — this makes the
# ci.yml "fails loudly ('gate step not found')" claim actually true);
# (b) scenario N pins the INITIAL-probe 200-wrong-body FATAL (proven
# zero-coverage in round 11: deleting that body check left all 14 scenarios
# green — scenario J pins only the GRACE-arm twin); (c) scenario O pins the
# STATE_PRE=unknown loud-skip policy (proven zero-coverage: flipping the
# documented unknown-pre skip into a FATAL left all 14 green); (d) scenarios
# P + Q pin the round-11 gate change: a fast 503/504 with the box stopped at
# both samples is a console-lambda-layer fault -> FATAL (previously exit-0
# skip; scenario E remains the stopped+000 skip pin).
#
# Run from anywhere:  bash deploy/aws/lambda/questdb-console-proxy/gate-matrix-r7.sh
# Stubs terraform/aws/curl/sleep on PATH and drives (curl-code sequence,
# ec2-state sequence) scenarios end-to-end through the REAL gate script.
set -u

SELF_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SELF_DIR/../../../.." && pwd)
WORKFLOW="$REPO_ROOT/.github/workflows/terraform-apply.yml"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
GATE_SRC="$WORK/gate-step.sh"

# Round-11 (2026-07-08): abort on extraction failure. Under `set -u` alone,
# a python SystemExit("gate step not found ...") did NOT stop the harness —
# $GATE_SRC stayed EMPTY, every scenario ran `bash` on the empty file (rc=0)
# and printed misleading FAIL rows against a script that was never there.
if ! python3 - "$WORKFLOW" > "$GATE_SRC" <<'PY'
import sys
import yaml

doc = yaml.safe_load(open(sys.argv[1]))
for job in doc["jobs"].values():
    for step in job.get("steps", []) or []:
        name = step.get("name") or ""
        if name.startswith("Deploy gate: console shell"):
            run = step["run"]
            run = run.replace("${{ env.TF_DIR }}", "deploy/aws/terraform")
            run = run.replace("${{ env.AWS_REGION }}", "ap-south-1")
            if "${{" in run:
                raise SystemExit("unsubstituted GitHub expression remains in the gate script")
            sys.stdout.write(run)
            raise SystemExit(0)
raise SystemExit("gate step not found in terraform-apply.yml")
PY
then
  echo "gate-matrix: ABORT — gate-step extraction failed (see the extractor error above); not running scenarios against an empty script" >&2
  exit 1
fi
if [ ! -s "$GATE_SRC" ]; then
  echo "gate-matrix: ABORT — extracted gate script is empty; not running scenarios against it" >&2
  exit 1
fi

FAILS=0
TOTAL=0

run_case() {
  local name="$1" codes="$2" states="$3" body="$4" expect_rc="$5" expect_grep="$6"
  local dir="$WORK/$name"
  mkdir -p "$dir/bin"
  echo "$codes" | tr ' ' '\n' > "$dir/codes.txt"
  echo "$states" | tr ' ' '\n' > "$dir/states.txt"
  printf '%s' "$body" > "$dir/body.txt"

  cat > "$dir/bin/terraform" <<EOF
#!/usr/bin/env bash
for a in "\$@"; do
  case "\$a" in
    questdb_console_url) echo "https://console.example.test"; exit 0 ;;
    instance_id) echo "i-0abc"; exit 0 ;;
  esac
done
exit 1
EOF
  cat > "$dir/bin/aws" <<EOF
#!/usr/bin/env bash
if [ "\$1" = "ssm" ]; then echo "FAKETOKEN"; exit 0; fi
if [ "\$1" = "ec2" ]; then
  n=\$(cat "$dir/state_idx" 2>/dev/null || echo 1)
  s=\$(sed -n "\${n}p" "$dir/states.txt")
  [ -z "\$s" ] && s=\$(tail -1 "$dir/states.txt")
  echo \$((n+1)) > "$dir/state_idx"
  echo "\$s"; exit 0
fi
exit 1
EOF
  cat > "$dir/bin/curl" <<EOF
#!/usr/bin/env bash
n=\$(cat "$dir/curl_idx" 2>/dev/null || echo 1)
c=\$(sed -n "\${n}p" "$dir/codes.txt")
[ -z "\$c" ] && c=\$(tail -1 "$dir/codes.txt")
echo \$((n+1)) > "$dir/curl_idx"
# find -o target
out=""
prev=""
for a in "\$@"; do
  [ "\$prev" = "-o" ] && out="\$a"
  prev="\$a"
done
if [ "\$c" = "200" ]; then cat "$dir/body.txt" > "\$out"; else : > "\$out"; fi
if [ "\$c" = "000" ]; then printf '000'; exit 28; fi
printf '%s' "\$c"
EOF
  cat > "$dir/bin/sleep" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "$dir/bin/"*
  : > "$dir/summary"
  ( cd "$dir" && PATH="$dir/bin:$PATH" GITHUB_STEP_SUMMARY="$dir/summary" \
      bash "$GATE_SRC" > "$dir/out" 2>&1 )
  local rc=$?
  TOTAL=$((TOTAL + 1))
  local verdict="PASS"
  [ "$rc" -ne "$expect_rc" ] && verdict="FAIL(rc=$rc want=$expect_rc)"
  if [ -n "$expect_grep" ] && ! grep -qF "$expect_grep" "$dir/out"; then
    # Round-10 fix: REPLACE a prior PASS instead of appending to it — rows
    # used to print the contradictory "PASS FAIL(missing: ...)" (exit code
    # was already correct; output honesty only).
    if [ "$verdict" = PASS ]; then
      verdict="FAIL(missing: $expect_grep)"
    else
      verdict="$verdict FAIL(missing: $expect_grep)"
    fi
  fi
  [ "$verdict" != PASS ] && FAILS=$((FAILS + 1))
  printf '%-42s rc=%d  %s\n' "$name" "$rc" "$verdict"
  if [[ "$verdict" != PASS ]]; then sed 's/^/    | /' "$dir/out" | tail -8; fi
}

HTML='<!DOCTYPE html><html>console</html>'

# A. boot window, both samples running, 000s, grace 200 -> pass via grace (round-7 fix)
run_case A_boot_running_000_grace200 "000 000 000 200" "running running" "$HTML" 0 "200 on the 90s grace re-probe"
# B1. start transition pre=pending post=running, 000s, grace 200 -> pass via grace
run_case B1_pending_to_running_grace200 "000 000 000 200" "pending running" "$HTML" 0 "200 on the 90s grace re-probe"
# B2. same but grace still 000 -> FATAL past grace, pre-grace code shown
run_case B2_pending_to_running_grace000 "000 000 000 000" "pending running running" "" 1 "past the 90s grace re-probe (pre-grace: '000')"
# C. running + 503, grace 200 -> pass (round-5 behavior preserved)
run_case C_running_503_grace200 "503 503 503 200" "running running" "$HTML" 0 "200 on the 90s grace re-probe"
# D. running + 503, grace 000, mid-grace auto-stop -> loud SKIP whose cause
#    enumeration is the 503-specific one (round-10: causes are now per-code)
run_case D_midgrace_stop_skip "503 503 503 000" "running running stopped" "" 0 "consistent with QuestDB-not-listening / a booting OS OR a back-lambda invoke failure"
# E. stopped at both samples, 000 -> loud skip naming the auto-stop window (round-8 pin)
run_case E_stopped_both_skip "000 000 000" "stopped stopped" "" 0 "auto-stop window, 08:30–16:30 IST schedule"
# F. running->stopped TOCTOU (pre=running, post=stopped) -> fail closed (round-6, unchanged)
run_case F_run_to_stop_failclosed "000 000 000" "running stopped" "" 1 "transitioned running->stopped ACROSS the probe window"
# G. healthy 200 first attempt -> pass
run_case G_healthy_200 "200" "running" "$HTML" 0 "GET / == 200 within 5s"
# H. running + 504 persists past grace -> FATAL naming shell-hang class
run_case H_running_504_grace504 "504 504 504 504" "running running running" "" 1 "the unframed-response shell-hang class"
# I. weird code 401 -> generic FATAL (unchanged)
run_case I_401_generic_fatal "401 401 401" "running running" "" 1 "deploy gate FAILED"
# J. grace gives 200 but body is NOT shell html -> FATAL (fail-closed grace)
run_case J_grace200_wrong_body "000 000 000 200" "running running" "not the shell" 1 "200 but the body is not the console shell HTML"
# K. unknown state -> fail closed (unchanged)
run_case K_unknown_state "000 000 000" "unknown unknown" "" 1 "state could not be verified"
# L. pending at BOTH samples, 000 -> loud skip naming the START transition,
#    never the auto-stop window (round-8 message-accuracy fix)
run_case L_pending_both_start_wording "000 000 000" "pending pending" "" 0 "start transition — box booting"
# M. running + 000 pre-grace, grace 000, mid-grace auto-stop -> loud SKIP
#    whose cause enumeration is the 000/504-specific one: it must name the
#    hang class (which the terminal FATAL assigns to 000/504) and must NOT
#    name a back-lambda invoke failure (that yields a FAST 503, never 000) —
#    round-10 message-accuracy fix; scenario D pins only the 503 variant
run_case M_midgrace_stop_skip_000_hangclass "000 000 000 000" "running running stopped" "" 0 "a booting OS/ENI still dropping SYNs"
# N. INITIAL probe gives 200 but the body is NOT the shell HTML -> FATAL.
#    Round-11 pin: this PROBE fail-closed arm had ZERO coverage (proven by
#    mutation — deleting the check left all prior scenarios green; scenario J
#    pins only the grace-arm twin). The grep uses the initial arm's DISTINCT
#    prefix so the grace twin cannot satisfy it.
run_case N_initial_200_wrong_body "200" "running" "not the shell" 1 "GET / gave 200 but the body is not the console shell HTML"
# O. STATE_PRE=unknown (transient describe-instances failure) + post-sample
#    stopped + 000 -> LOUD SKIP, both samples printed (the documented round-6
#    policy). Round-11 pin: proven zero-coverage — mutating the skip's
#    STATE_PRE condition to also FATAL on 'unknown' left all prior scenarios
#    green.
run_case O_pre_unknown_stopped_000_skip "000 000 000" "unknown stopped" "" 0 "pre-probe sample: unknown, post-probe sample: stopped"
# P/Q. fast 503/504 with the box stopped at BOTH samples -> FATAL naming the
#    console-lambda-layer fault class (round-11 gate change: a not-running
#    box can only produce 000 through the 5s-capped curl, so a fast 503/504
#    there is a front/back wiring / lambda-permission / back-crash class the
#    old arm laundered into an exit-0 skip on every off-window apply).
run_case P_stopped_503_lambda_layer_fatal "503 503 503" "stopped stopped" "" 1 "console-lambda-layer fault"
run_case Q_stopped_504_lambda_layer_fatal "504 504 504" "stopped stopped" "" 1 "console-lambda-layer fault"

echo "---"
if [ "$FAILS" -ne 0 ]; then
  echo "gate-matrix: $FAILS of $TOTAL scenario(s) FAILED"
  exit 1
fi
echo "gate-matrix: all $TOTAL scenarios green"
