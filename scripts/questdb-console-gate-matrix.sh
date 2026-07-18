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
# run time (constrained awk parse of the step's `run: |` block scalar; the
# `${{ env.* }}` expressions substituted; the extraction ABORTS with one
# clear error if the step is missing/renamed, the run block is not a plain
# `run: |` literal, or the extracted script is empty). Stubs
# terraform/aws/curl/sleep on PATH and drives each scenario end-to-end
# through the REAL gate script.
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
#
# Rust-only purge Phase 2a-2 (2026-07-18): the PyYAML extraction was
# replaced by a CONSTRAINED awk parse, total for the actual shape the
# workflow uses (verified against terraform-apply.yml):
#     - name: "Deploy gate: console shell ..."   <- step key, quoted string
#       # comment lines                           <- optional, deeper indent
#       run: |                                    <- literal block scalar
#         <script body>                           <- one deeper indent level
# The parse fail-closes loudly on ANY drift from that shape (step renamed/
# missing -> "gate step not found in the workflow"; run key absent or not a
# `run: |` literal block -> its own abort; unsubstituted `${{` -> abort) —
# the same abort-not-run-empty contract the PyYAML version had. Block-scalar
# semantics reproduced: body dedented to the first body line's indent,
# interior blank lines kept, trailing blank lines clipped (`|` chomping).
extract_gate() {
  local wf="$1" out="$2"
  if ! awk '
    function indent_of(s,   n) { n = match(s, /[^ ]/); return (n == 0) ? length(s) : n - 1 }
    function is_blank(s) { return s ~ /^[ \t]*$/ }
    function emit_err(msg) { print msg > "/dev/stderr"; bad = 1; exit 1 }
    function finish(   i) {
      # substitute the two env expressions the step uses (literal matches)
      gsub(/\$\{\{ env\.TF_DIR \}\}/, "deploy/aws/terraform", body)
      gsub(/\$\{\{ env\.AWS_REGION \}\}/, "ap-south-1", body)
      if (index(body, "${{") > 0)
        emit_err("unsubstituted GitHub expression remains in the gate script")
      printf "%s", body
      done_ok = 1
      exit 0
    }
    # state 0: searching for the gate step
    state == 0 {
      if ($0 ~ /^ *- name: *"?Deploy gate: console shell/) {
        step_indent = indent_of($0)
        state = 1
      }
      next
    }
    # state 1: inside the step, searching for its run: | key
    state == 1 {
      if (!is_blank($0) && indent_of($0) <= step_indent)
        emit_err("gate step found but it has no run: | block before the step ends")
      if ($0 ~ /^ *run:/) {
        if ($0 !~ /^ *run: *\| *$/)
          emit_err("gate step run block is not a plain literal block scalar (run: |) — extractor cannot parse it")
        state = 2
        body = ""
        body_indent = -1
        pending_blanks = 0
      }
      next
    }
    # state 2: inside the run block scalar body
    state == 2 {
      if (is_blank($0)) { pending_blanks++; next }
      ind = indent_of($0)
      if (body_indent < 0) {
        if (ind <= step_indent + 2)
          emit_err("gate step run block is empty or mis-indented")
        body_indent = ind
      }
      if (ind < body_indent) finish()   # block ended (trailing blanks clipped)
      for (i = 0; i < pending_blanks; i++) body = body "\n"
      pending_blanks = 0
      body = body substr($0, body_indent + 1) "\n"
      next
    }
    END {
      if (bad) exit 1
      if (done_ok) exit 0
      if (state == 2 && body != "") { finish(); exit 0 }
      print "gate step not found in the workflow" > "/dev/stderr"
      exit 1
    }
  ' "$wf" > "$out"
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
run_case F_exhausted_running_fatal true ok ok "503" running 1 "the box reported running when sampled after it"
# G. probes exhausted + box stopped -> loud SKIP naming the state + codes
run_case G_exhausted_stopped_skip true ok ok "000" stopped 0 "box state 'stopped' after the probe window"
# H. probes exhausted + box stopping -> loud SKIP (no per-code diagnosis)
run_case H_exhausted_stopping_skip true ok ok "504" stopping 0 "box state 'stopping' after the probe window"

# I. gate step missing from the workflow -> extraction ABORT (non-zero, one
#    clear error; never scenarios against an empty script)
TOTAL=$((TOTAL + 1))
STRIPPED="$WORK/stripped.yml"
# Strip the gate step from the RAW yaml (rust-only Phase 2a-2 — replaces the
# PyYAML load/dump round-trip): drop lines from the step's `- name:` line
# until the next non-blank line at the same-or-shallower indent (the next
# step / next job key). Only the extractor above consumes the stripped file,
# so a raw-text strip is sufficient — no YAML re-serialization needed.
awk '
  function indent_of(s,   n) { n = match(s, /[^ ]/); return (n == 0) ? length(s) : n - 1 }
  skip {
    if ($0 !~ /^[ \t]*$/ && indent_of($0) <= step_indent) skip = 0
    else next
  }
  /^ *- name: *"?Deploy gate: console shell/ {
    step_indent = indent_of($0)
    skip = 1
    next
  }
  { print }
' "$WORKFLOW" > "$STRIPPED"
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
