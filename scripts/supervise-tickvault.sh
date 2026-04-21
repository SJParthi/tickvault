#!/usr/bin/env bash
# =============================================================================
# tickvault — supervisor (crash-restart loop)
# =============================================================================
# Wraps `cargo run --release` (or a pre-built binary) so that if the
# tickvault process exits unexpectedly, the supervisor logs the exit
# code, waits a short back-off, and restarts it.
#
# This does NOT fix bugs inside tickvault. It only ensures that a
# process-level crash does not leave the trading system silently down.
# For systemd or AWS environments, prefer a proper unit file / ECS
# health check. This script is intended for local + bare-metal
# installs where you want "survive overnight" semantics without a
# full init-system setup.
#
# Usage:
#   scripts/supervise-tickvault.sh [--release|--debug] [--max-restarts N]
#
# Flags:
#   --release        (default) Run from target/release/tickvault
#   --debug          Run `cargo run` (debug build, auto-recompiles)
#   --max-restarts N Give up after N restarts within the same session
#                    (default: 50 — a truly broken build should not
#                    crash-loop the host forever).
#
# Signals:
#   SIGTERM / SIGINT are forwarded to the child; the supervisor exits
#   after the child exits, without restarting.
#
# Back-off:
#   2s, 4s, 8s, 16s, 30s, 30s, ... (cap 30s). This mirrors the main-
#   feed reconnect back-off in the app itself.
# =============================================================================

set -u  # Unset vars are errors. We do NOT use -e — we want to handle
         # child exits explicitly rather than have the shell abort on them.

MODE="release"
MAX_RESTARTS=50

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release)       MODE="release";       shift ;;
        --debug)         MODE="debug";         shift ;;
        --max-restarts)  MAX_RESTARTS="$2";    shift 2 ;;
        -h|--help)
            sed -n '/^# Usage:/,/^# Back-off:/p' "$0"
            exit 0
            ;;
        *)
            echo "supervise-tickvault: unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

# Validate MAX_RESTARTS is a positive integer.
if ! [[ "$MAX_RESTARTS" =~ ^[0-9]+$ ]] || [[ "$MAX_RESTARTS" -lt 1 ]]; then
    echo "supervise-tickvault: --max-restarts must be a positive integer (got: $MAX_RESTARTS)" >&2
    exit 2
fi

case "$MODE" in
    release)
        BIN="target/release/tickvault"
        if [[ ! -x "$BIN" ]]; then
            echo "supervise-tickvault: $BIN not found — build first with 'cargo build --release'" >&2
            exit 2
        fi
        CMD=("$BIN")
        ;;
    debug)
        CMD=(cargo run --)
        ;;
esac

CHILD_PID=0
SHUTTING_DOWN=0

forward_signal() {
    local sig="$1"
    SHUTTING_DOWN=1
    if [[ "$CHILD_PID" -ne 0 ]]; then
        echo "[supervise] forwarding $sig to child pid=$CHILD_PID" >&2
        kill "-$sig" "$CHILD_PID" 2>/dev/null || true
    fi
}

trap 'forward_signal TERM' TERM
trap 'forward_signal INT'  INT

restart_count=0
backoff_secs=2

while :; do
    if [[ "$SHUTTING_DOWN" -eq 1 ]]; then
        echo "[supervise] shutdown requested — exiting supervisor" >&2
        exit 0
    fi

    if [[ "$restart_count" -ge "$MAX_RESTARTS" ]]; then
        echo "[supervise] exceeded --max-restarts=$MAX_RESTARTS — giving up" >&2
        exit 1
    fi

    ts="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    echo "[supervise] $ts starting tickvault (attempt $((restart_count + 1))/$MAX_RESTARTS, mode=$MODE)" >&2

    # Start the child. We run it in the foreground of this subshell so
    # we capture its exit code directly. `&` + `wait $!` is used so
    # that signals can interrupt the wait and we can forward them.
    "${CMD[@]}" &
    CHILD_PID=$!

    wait "$CHILD_PID"
    exit_code=$?
    CHILD_PID=0

    if [[ "$SHUTTING_DOWN" -eq 1 ]]; then
        echo "[supervise] child exited after shutdown signal (code=$exit_code) — not restarting" >&2
        exit 0
    fi

    if [[ "$exit_code" -eq 0 ]]; then
        echo "[supervise] child exited cleanly (code=0) — not restarting" >&2
        exit 0
    fi

    echo "[supervise] child exited with code=$exit_code — restarting in ${backoff_secs}s" >&2
    sleep "$backoff_secs"

    # Exponential back-off capped at 30s.
    backoff_secs=$((backoff_secs * 2))
    if [[ "$backoff_secs" -gt 30 ]]; then
        backoff_secs=30
    fi

    restart_count=$((restart_count + 1))
done
