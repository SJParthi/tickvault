#!/usr/bin/env bash
# .claude/hooks/session-auto-health.sh
#
# Fires on every SessionStart (startup + resume). Runs the three
# automation primitives the operator asked to happen "automatically
# every session":
#
#   1. `scripts/doctor.sh`                — infrastructure health snapshot
#   2. `scripts/validate-automation.sh`   — ratchet audit (30 checks)
#   3. `scripts/mcp-doctor.sh`            — MCP server connectivity
#
# Outputs go to `data/logs/session-auto-health.YYYY-MM-DD-HHMM.log`
# so we don't pollute the operator's terminal. The startup time of a
# Claude session is NOT gated on these runs — they are launched with
# `nohup` + `&` and detached.
#
# If any script exits non-zero the summary line is still emitted at
# session start so the operator sees a red/green indicator without
# having to open the log file.

set -euo pipefail

PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$(pwd)}"
LOG_DIR="$PROJECT_DIR/data/logs"
mkdir -p "$LOG_DIR"

TS="$(date +%Y-%m-%d-%H%M)"
LOG="$LOG_DIR/session-auto-health.$TS.log"
SUMMARY="$LOG_DIR/session-auto-health.latest.txt"

# Header (synchronous — operator sees this instantly on session start).
{
  echo "--- session-auto-health.sh ---"
  echo "started: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "project: $PROJECT_DIR"
  echo "log:     $LOG"
} >&2

# Wrap all three in a single background subshell so they run in series
# without blocking session start. Writes a one-line verdict to SUMMARY
# when done; `session-sanity.sh` (the other SessionStart hook) reads
# that file if present on the NEXT session start.
(
  exec > "$LOG" 2>&1
  echo "[auto-health] run $(date -u +%Y-%m-%dT%H:%M:%SZ)"

  STATUS="ok"

  # Toolchain self-heal: the pinned toolchain occasionally ships without the
  # `cargo` proxy binary (the rustup component is "installed" but the binary is
  # absent), which breaks every build until manually repaired. Detect + repair
  # automatically so no session needs the manual `rustup component add cargo`.
  ensure_cargo() {
    echo "--- stage: cargo-self-heal ---"
    if cargo --version >/dev/null 2>&1; then
      echo "[cargo-self-heal] cargo OK"
      return 0
    fi
    local channel
    channel="$(awk -F'"' '/^channel/{print $2}' "$PROJECT_DIR/rust-toolchain.toml" 2>/dev/null)"
    channel="${channel:-stable}"
    echo "[cargo-self-heal] cargo binary missing — repairing toolchain $channel"
    rustup component remove cargo --toolchain "$channel" >/dev/null 2>&1 || true
    rustup component add cargo --toolchain "$channel" >/dev/null 2>&1 \
      || rustup component add cargo >/dev/null 2>&1 || true
    if cargo --version >/dev/null 2>&1; then
      echo "[cargo-self-heal] repaired: $(cargo --version)"
    else
      echo "[cargo-self-heal] STILL BROKEN — manual rustup needed"
      STATUS="fail"
    fi
  }
  ensure_cargo

  run_stage() {
    local name="$1"
    local cmd="$2"
    echo "--- stage: $name ---"
    if bash -c "$cmd"; then
      echo "[$name] PASS"
    else
      echo "[$name] FAIL"
      STATUS="fail"
    fi
  }

  if [[ -x "$PROJECT_DIR/scripts/doctor.sh" ]]; then
    run_stage "doctor" "$PROJECT_DIR/scripts/doctor.sh"
  else
    echo "[doctor] SKIP (scripts/doctor.sh missing)"
  fi

  if [[ -x "$PROJECT_DIR/scripts/validate-automation.sh" ]]; then
    run_stage "validate-automation" "$PROJECT_DIR/scripts/validate-automation.sh"
  else
    echo "[validate-automation] SKIP (script missing)"
  fi

  if [[ -x "$PROJECT_DIR/scripts/mcp-doctor.sh" ]]; then
    run_stage "mcp-doctor" "$PROJECT_DIR/scripts/mcp-doctor.sh"
  else
    echo "[mcp-doctor] SKIP (script missing)"
  fi

  echo "[auto-health] verdict: $STATUS  at  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "$STATUS $TS" > "$SUMMARY"
) </dev/null >/dev/null 2>&1 &
disown || true

# Also print the last cycle's verdict (if any) so the operator sees
# state IMMEDIATELY instead of having to wait for the next cycle.
if [[ -f "$SUMMARY" ]]; then
  echo "prev-cycle verdict: $(cat "$SUMMARY")" >&2
else
  echo "prev-cycle verdict: (none yet — first run in progress)" >&2
fi

echo "--- session-auto-health.sh dispatched ---" >&2
exit 0
