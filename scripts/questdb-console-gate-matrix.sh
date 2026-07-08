#!/usr/bin/env bash
# questdb-console-gate-matrix.sh — behavioral scenario matrix for the
# terraform-apply.yml step "Deploy gate: console shell GET / must return 200
# within 5s" (the console smoke gate, simplified to a conservative decision
# tree in the 2026-07-08 closure pass).
#
# The gate's contract is deliberately small; this harness pins exactly its
# decision tree, one scenario per arm:
#   A  console disabled (TF var off)        -> loud SKIP, exit 0
#   B  terraform output unresolvable        -> FATAL, exit 1
#   C  SSM token read fails                 -> FATAL, exit 1
#   D  200 on the first probe               -> PASS, exit 0
#   E  200 after failed probes              -> PASS, exit 0
#   F  probes exhausted + box running       -> FATAL, exit 1 (codes listed)
#   G  probes exhausted + box stopped       -> loud SKIP, exit 0
#   H  probes exhausted + box stopping      -> loud SKIP, exit 0
#   I  gate step missing from the workflow  -> extraction ABORT, non-zero
#
# The gate step is EXTRACTED from .github/workflows/terraform-apply.yml at
# run time (yaml.safe_load; the `${{ env.* }}` expressions substituted; the
# extraction ABORTS with one clear error if the step is missing/renamed or
# the extracted script is empty). Stubs terraform/aws/curl/sleep on PATH and
# drives each scenario end-to-end through the REAL gate script.
#
# Run from anywhere:  bash scripts/questdb-console-gate-matrix.sh
# CI wiring: the ci.yml Repo Guards step "QuestDB console lambda unit tests
# (back + front)" runs this harness on every PR (the job feeds All Green).
set -u

SELF_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SELF_DIR/.." && pwd)
WORKFLOW="$REPO_ROOT/.github/workflows/terraform-apply.yml"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
GATE_SRC="$WORK/gate-step.sh"

# extract_gate <workflow-yaml> <out-file>: writes the gate step's `run`
# script (env expressions substituted) to <out-file>; non-zero on any
# failure. The main flow ABORTS on failure instead of running scenarios
# against an empty script.
extract_gate() {
  local wf="$1" out="$2"
  if ! python3 - "$wf" > "$out" <<'PY'
import sys
import yaml

doc = yaml.safe_load(open(sys.argv[1]))
for job in (doc.get("jobs") or {}).values():
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
raise SystemExit("gate step not found in the workflow")
PY
  then
    return 1
  fi
  [ -s "$out" ] || return 1
  return 0
}

if ! extract_gate "$WORKFLOW" "$GATE_SRC"; then
  echo "gate-matrix: ABORT — gate-step extraction failed (see the extractor error above); not running scenarios against an empty script" >&2
  exit 1
fi

FAILS=0
TOTAL=0

# run_case <name> <console-enabled true/false> <tf-mode ok/url-fail>
#          <ssm-mode ok/fail> <curl-code-sequence> <ec2-state>
#          <expect-rc> <expect-grep>
run_case() {
  local name="$1" enabled="$2" tf_mode="$3" ssm_mode="$4" codes="$5" state="$6" expect_rc="$7" expect_grep="$8"
  local dir="$WORK/$name"
  mkdir -p "$dir/bin"
  echo "$codes" | tr ' ' '\n' > "$dir/codes.txt"

  cat > "$dir/bin/terraform" <<EOF
#!/usr/bin/env bash
for a in "\$@"; do
  case "\$a" in
    questdb_console_url)
      if [ "$tf_mode" = "url-fail" ]; then echo "Error: output not readable" >&2; exit 1; fi
      echo "https://console.example.test"; exit 0 ;;
    instance_id) echo "i-0abc"; exit 0 ;;
  esac
done
exit 1
EOF
  cat > "$dir/bin/aws" <<EOF
#!/usr/bin/env bash
if [ "\$1" = "ssm" ]; then
  if [ "$ssm_mode" = "fail" ]; then echo "AccessDeniedException" >&2; exit 254; fi
  echo "FAKETOKEN"; exit 0
fi
if [ "\$1" = "ec2" ]; then echo "$state"; exit 0; fi
exit 1
EOF
  cat > "$dir/bin/curl" <<EOF
#!/usr/bin/env bash
n=\$(cat "$dir/curl_idx" 2>/dev/null || echo 1)
c=\$(sed -n "\${n}p" "$dir/codes.txt")
[ -z "\$c" ] && c=\$(tail -1 "$dir/codes.txt")
echo \$((n+1)) > "$dir/curl_idx"
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
      TF_VAR_enable_questdb_console="$enabled" \
      bash "$GATE_SRC" > "$dir/out" 2>&1 )
  local rc=$?
  TOTAL=$((TOTAL + 1))
  local verdict="PASS"
  [ "$rc" -ne "$expect_rc" ] && verdict="FAIL(rc=$rc want=$expect_rc)"
  if [ -n "$expect_grep" ] && ! grep -qF "$expect_grep" "$dir/out"; then
    if [ "$verdict" = PASS ]; then
      verdict="FAIL(missing: $expect_grep)"
    else
      verdict="$verdict FAIL(missing: $expect_grep)"
    fi
  fi
  [ "$verdict" != PASS ] && FAILS=$((FAILS + 1))
  printf '%-32s rc=%d  %s\n' "$name" "$rc" "$verdict"
  if [[ "$verdict" != PASS ]]; then sed 's/^/    | /' "$dir/out" | tail -8; fi
}

# A. console disabled -> loud SKIP
run_case A_disabled_skip false ok ok "200" running 0 "console disabled"
# B. terraform output failure -> FATAL (fail-closed)
run_case B_tf_output_fail_fatal true url-fail ok "200" running 1 "could not be resolved"
# C. SSM token read failure -> FATAL (fail-closed)
run_case C_ssm_fail_fatal true ok fail "200" running 1 "could not read the console auth token"
# D. healthy 200 on the first probe -> PASS
run_case D_200_first_try true ok ok "200" running 0 "GET / == 200 within 5s through the proxy (attempt 1/12)"
# E. 200 after failed probes -> PASS
run_case E_200_after_retries true ok ok "503 000 200" running 0 "(attempt 3/12)"
# F. probes exhausted + box running -> FATAL, codes listed
run_case F_exhausted_running_fatal true ok ok "503" running 1 "while the box reported running"
# G. probes exhausted + box stopped -> loud SKIP naming the state + codes
run_case G_exhausted_stopped_skip true ok ok "000" stopped 0 "box state 'stopped' after the probe window"
# H. probes exhausted + box stopping -> loud SKIP (no per-code diagnosis)
run_case H_exhausted_stopping_skip true ok ok "504" stopping 0 "box state 'stopping' after the probe window"

# I. gate step missing from the workflow -> extraction ABORT (non-zero, one
#    clear error; never scenarios against an empty script)
TOTAL=$((TOTAL + 1))
STRIPPED="$WORK/stripped.yml"
python3 - "$WORKFLOW" "$STRIPPED" <<'PY'
import sys
import yaml

doc = yaml.safe_load(open(sys.argv[1]))
for job in (doc.get("jobs") or {}).values():
    steps = job.get("steps") or []
    job["steps"] = [s for s in steps if not (s.get("name") or "").startswith("Deploy gate: console shell")]
yaml.safe_dump(doc, open(sys.argv[2], "w"))
PY
if extract_gate "$STRIPPED" "$WORK/should-not-exist.sh" 2> "$WORK/extract-err"; then
  FAILS=$((FAILS + 1))
  printf '%-32s %s\n' "I_missing_step_abort" "FAIL(extraction unexpectedly succeeded)"
elif ! grep -qF "gate step not found" "$WORK/extract-err"; then
  FAILS=$((FAILS + 1))
  printf '%-32s %s\n' "I_missing_step_abort" "FAIL(missing: gate step not found)"
  sed 's/^/    | /' "$WORK/extract-err"
else
  printf '%-32s %s\n' "I_missing_step_abort" "PASS"
fi

echo "---"
if [ "$FAILS" -ne 0 ]; then
  echo "gate-matrix: $FAILS of $TOTAL scenario(s) FAILED"
  exit 1
fi
echo "gate-matrix: all $TOTAL scenarios green"
