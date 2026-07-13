#!/bin/bash
# pre-push-gate.selftest.sh — proves the pre-push gate's cwd handling is safe.
# Run: bash .claude/hooks/pre-push-gate.selftest.sh
# Exit 0 = all scenarios behaved as designed.
#
# Covers the 2026-07-13 hardening: before it, a `git push` issued from any
# directory lacking crates/ (temp dir, subdirectory, other project) made
# `find crates` come up empty and the gate exited 0 SILENTLY — a full-gate
# bypass. The gate must now (a) anchor to `git rev-parse --show-toplevel`,
# (b) fail LOUDLY (exit 2) outside a git repo, (c) fail LOUDLY when the repo
# root has no crates/*.rs, (d) keep the non-push early exit 0 intact, and
# (e) resolve its sibling sub-guards via an ABSOLUTE HOOKS_DIR computed
# BEFORE any cd — a relative $0 must NOT degrade every sub-guard to SKIP.
#
# HERMETIC by construction: the gate under test is a COPY placed in a temp
# hooks dir alongside STUB sub-guards (the gate resolves siblings via its own
# dirname), and `cargo` is stubbed on PATH — so the real repo's
# .claude/hooks/.{test-count,untested-pubfn,...}-baseline ratchet files are
# never read or written by this selftest.
set -uo pipefail

# Dependency guard: the selftest needs jq (payload build) and the gate copy
# itself shells out to `timeout`. Skip cleanly if either is missing.
command -v jq >/dev/null 2>&1 && command -v timeout >/dev/null 2>&1 || {
  echo "SKIP: pre-push-gate selftest needs jq + timeout" >&2
  exit 0
}

REAL_GATE="$(cd "$(dirname "$0")" && pwd)/pre-push-gate.sh"
PASS=0; FAIL=0

# Single temp root for EVERYTHING this selftest creates; trap guarantees
# cleanup on every exit path (success, failure, interrupt).
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Environment sanity: every scenario dir lives under $WORK. If the temp area
# is unexpectedly INSIDE a git repo, the non-git scenario would be invalid —
# skip (not fail) to avoid env flakiness.
if git -C "$WORK" rev-parse --show-toplevel >/dev/null 2>&1; then
  echo "SKIP: TMPDIR is inside a git repo — non-git scenario would be invalid" >&2
  exit 0
fi

check() { # <desc> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then echo "  ok   : $1 (exit $3)"; PASS=$((PASS+1));
  else echo "  FAIL : $1 (expected $2, got $3)"; FAIL=$((FAIL+1)); fi
}

check_stderr_has() { # <desc> <needle> <stderr_file>
  if grep -q "$2" "$3" 2>/dev/null; then echo "  ok   : $1"; PASS=$((PASS+1));
  else echo "  FAIL : $1 (stderr missing '$2')"; FAIL=$((FAIL+1)); fi
}

# ── Hermetic harness: copy of the gate + stub sub-guards + stub cargo ──
STUB_HOOKS="$WORK/hooks"
STUB_BIN="$WORK/bin"
MARKER="$WORK/plan-gate-ran.marker"
mkdir -p "$STUB_HOOKS" "$STUB_BIN"
cp "$REAL_GATE" "$STUB_HOOKS/pre-push-gate.sh"
chmod +x "$STUB_HOOKS/pre-push-gate.sh"
GATE="$STUB_HOOKS/pre-push-gate.sh"

# plan-gate stub writes a marker so scenarios can PROVE the sub-guards
# actually executed (pins the absolute-HOOKS_DIR-before-cd contract).
printf '#!/bin/bash\ntouch "%s"\nexit 0\n' "$MARKER" > "$STUB_HOOKS/plan-gate.sh"
chmod +x "$STUB_HOOKS/plan-gate.sh"
for g in banned-pattern-scanner.sh secret-scanner.sh \
         test-count-guard.sh data-integrity-guard.sh pub-fn-test-guard.sh \
         boot-symmetry-guard.sh pub-fn-wiring-guard.sh financial-test-guard.sh; do
  printf '#!/bin/bash\nexit 0\n' > "$STUB_HOOKS/$g"
  chmod +x "$STUB_HOOKS/$g"
done
# Stub cargo so gates 1/9/10 (fmt, dhan locked facts, audit/deny) are inert.
printf '#!/bin/bash\nexit 0\n' > "$STUB_BIN/cargo"
chmod +x "$STUB_BIN/cargo"

run_gate() { # <cwd> <command> <stderr_file>; echoes exit code
  local payload
  payload=$(jq -cn --arg cmd "$2" --arg cwd "$1" '{tool_input:{command:$cmd},cwd:$cwd}')
  echo "$payload" | PATH="$STUB_BIN:$PATH" bash "$GATE" >/dev/null 2>"$3"
  echo $?
}

new_repo() { # throwaway git repo under $WORK, print its path
  local d; d=$(mktemp -d "$WORK/repo.XXXXXX")
  ( cd "$d"
    git init -q
    git config user.email t@t; git config user.name t
    git config commit.gpgsign false
    echo "x" > README.md
    git add -A; git commit -qm init >/dev/null 2>&1
  )
  echo "$d"
}

with_crates() { # add a crate + a subdir to repo $1
  ( cd "$1"
    mkdir -p crates/x/src sub/dir
    echo "pub fn f() {}" > crates/x/src/lib.rs
    git add -A; git commit -qm "add crate" >/dev/null 2>&1
  )
}

ERR="$WORK/stderr.txt"

# (a) non-push command → early exit 0 (the intended filter, untouched)
D_A=$(mktemp -d "$WORK/plain.XXXXXX")
RC=$(run_gate "$D_A" "ls -la" "$ERR")
check "non-push command -> PASS-through" 0 "$RC"

# (b) push from a NON-GIT dir → exit 2 + loud "refusing"
D_B=$(mktemp -d "$WORK/nongit.XXXXXX")
RC=$(run_gate "$D_B" "git push origin main" "$ERR")
check "push from non-git dir -> BLOCK" 2 "$RC"
check_stderr_has "push from non-git dir -> loud stderr" "refusing" "$ERR"

# (c) push from a git repo WITHOUT crates/ → exit 2 + loud
D_C=$(new_repo)
RC=$(run_gate "$D_C" "git push origin main" "$ERR")
check "push from crates-less git repo -> BLOCK" 2 "$RC"
check_stderr_has "crates-less repo -> loud stderr" "refusing" "$ERR"

# (d) push from a SUBDIRECTORY of a repo WITH crates/ → gate anchors to the
# repo root, finds crates/*.rs, and (with stubbed sub-guards) passes.
D_D=$(new_repo)
with_crates "$D_D"
rm -f "$MARKER"
RC=$(run_gate "$D_D/sub/dir" "git push origin main" "$ERR")
check "push from repo SUBDIR (crates at root) -> anchors + PASS" 0 "$RC"
[ -f "$MARKER" ] && { echo "  ok   : SUBDIR run executed the stub sub-guards"; PASS=$((PASS+1)); } \
  || { echo "  FAIL : SUBDIR run never reached the stub sub-guards"; FAIL=$((FAIL+1)); }

# (e) RELATIVE $0: invoke the gate copy via a relative path (bash hooks/...)
# from $WORK, cwd payload = a repo subdir. Pre-fix, `dirname "$0"` resolved
# against the post-anchor cwd and every sub-guard [ -x ] check failed ->
# silent SKIP. With HOOKS_DIR computed absolute BEFORE any cd, the stubs
# must run (marker file) and the gate must pass.
D_E=$(new_repo)
with_crates "$D_E"
rm -f "$MARKER"
payload=$(jq -cn --arg cmd "git push origin main" --arg cwd "$D_E/sub/dir" '{tool_input:{command:$cmd},cwd:$cwd}')
( cd "$WORK" && echo "$payload" | PATH="$STUB_BIN:$PATH" bash "hooks/pre-push-gate.sh" >/dev/null 2>"$ERR" )
RC=$?
check "relative \$0 from repo subdir -> anchors + PASS" 0 "$RC"
[ -f "$MARKER" ] && { echo "  ok   : relative \$0 still executed the stub sub-guards (absolute HOOKS_DIR)"; PASS=$((PASS+1)); } \
  || { echo "  FAIL : relative \$0 silently skipped the sub-guards (HOOKS_DIR mis-resolved)"; FAIL=$((FAIL+1)); }

echo "  pre-push-gate self-test: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
