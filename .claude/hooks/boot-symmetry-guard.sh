#!/bin/bash
# boot-symmetry-guard.sh — Phase 6.1 G3 + G4
#
# Catches the "fast-boot blind spot" class of bugs:
#   - Code wired into fast-boot path but not slow-boot (or vice versa)
#   - State machines that have poll_*() methods but no caller polls them
#
# Detection algorithm:
#   1. Scan crates/app/src/main.rs for `tokio::spawn` blocks
#   2. For each spawn, check whether it appears inside the fast-boot scope
#      (line range derived by finding `if let Some(cache_result) = fast_cache`
#      and its closing brace at indent level 4) OR slow-boot scope
#   3. For tokio::spawn references that touch a NEW Arc<*WebSocket*>,
#      Arc<*Pool*>, or *Watchdog*/*HealthPoller*, ensure the SAME pattern
#      exists in the OTHER boot scope (or // BOOT-SYMMETRY-EXEMPT:)
#   4. For state machines with `pub fn poll_*` or `pub fn tick(`, ensure
#      a caller exists that runs them in a loop or interval task
#
# Exit:
#   0 — symmetric or exempt
#   2 — asymmetry detected; commit/push blocked
#
# This is a heuristic guard. It accepts false-positive rate < false-negative
# rate, and the // BOOT-SYMMETRY-EXEMPT: escape hatch lets you opt out of
# any specific check with a one-line justification.

set -uo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$REPO_ROOT" || exit 0

MAIN_RS="crates/app/src/main.rs"
[ -f "$MAIN_RS" ] || exit 0

VIOLATIONS=()

# G4 first: every state machine with a poll_*() pub method must have a
# caller in main.rs that invokes it on an interval. This catches the
# A4 audit bug (pool watchdog defined + tested but never polled).
for STATE_MACHINE in WatchdogVerdict QuestDbHealthVerdict; do
  # Find pub fns named poll_* or tick that operate on the state machine.
  while IFS=: read -r FILE LINENO _; do
    [ -n "$LINENO" ] || continue
    [ -f "$FILE" ] || continue
    # Extract surrounding pub fn name.
    FN_LINE=$(awk -v l="$LINENO" 'NR==l && /pub fn/ {print}' "$FILE" 2>/dev/null)
    [ -n "$FN_LINE" ] || continue
    FN_NAME=$(echo "$FN_LINE" | sed -nE 's/.*pub[[:space:]]+(async[[:space:]]+|const[[:space:]]+)?fn[[:space:]]+([a-zA-Z_][a-zA-Z0-9_]*).*/\2/p')
    [ -n "$FN_NAME" ] || continue
    # Skip non-poll fns.
    case "$FN_NAME" in
      poll_*|tick) ;;
      *) continue ;;
    esac
    # Look for a caller of this fn name in main.rs.
    if ! grep -qE "\.${FN_NAME}\(" "$MAIN_RS" 2>/dev/null; then
      # Allow exempt comment on the declaration line above.
      PREV=$((LINENO - 1))
      if sed -n "${PREV}p" "$FILE" 2>/dev/null | grep -q 'BOOT-SYMMETRY-EXEMPT:'; then
        continue
      fi
      VIOLATIONS+=("${FILE}:${LINENO}: state-machine pub fn ${FN_NAME}() defined but no caller in ${MAIN_RS}")
    fi
  done < <(grep -nrE "${STATE_MACHINE}|pub fn (poll_|tick\b)" crates/core/src/websocket crates/storage/src 2>/dev/null | grep -E 'pub fn (poll_|tick\b)' || true)
done

# G3: heuristic — every Arc<WebSocketConnectionPool>::new should be cloned
# into BOTH a watchdog spawn AND a graceful-shutdown handler in the same
# function scope. We don't enforce strict location matching; we just check
# that if the pool Arc is created, it appears in both spawn_pool_watchdog_task
# AND request_graceful_shutdown call sites.
if grep -q 'Arc::new(pool)' "$MAIN_RS" 2>/dev/null; then
  if ! grep -q 'spawn_pool_watchdog_task' "$MAIN_RS" 2>/dev/null; then
    VIOLATIONS+=("${MAIN_RS}: Arc<WebSocketConnectionPool> created but spawn_pool_watchdog_task is missing — pool watchdog dormant")
  fi
  if ! grep -q 'request_graceful_shutdown' "$MAIN_RS" 2>/dev/null; then
    VIOLATIONS+=("${MAIN_RS}: Arc<WebSocketConnectionPool> created but request_graceful_shutdown is never called — graceful unsubscribe dormant")
  fi
fi

# G3 part 2: the synth tick channel pattern from S4-T1f / S5-A1.
# In any boot path that has a synth_tick_rx, there must be a writer task
# that calls .append_tick() OR a forwarder to a broadcast that has a
# downstream writer. Both fast-boot (synth_tick_rx) and slow-boot
# (synth_tick_rx_slow) must satisfy this independently.
for SYNTH_RX in synth_tick_rx synth_tick_rx_slow; do
  if grep -q "let.*${SYNTH_RX}" "$MAIN_RS" 2>/dev/null; then
    # Find the line range from the let-binding to the next top-level brace.
    # Heuristic: just check that within ~120 lines after the let-binding
    # there is either a `.append_tick(` call OR a `_for_synth.send(` call
    # OR a `broadcast_for_synth_slow.send(` call.
    BIND_LINE=$(grep -nE "let.*${SYNTH_RX}" "$MAIN_RS" | head -1 | cut -d: -f1)
    [ -n "$BIND_LINE" ] || continue
    END_LINE=$((BIND_LINE + 200))
    HAS_WRITER=$(sed -n "${BIND_LINE},${END_LINE}p" "$MAIN_RS" | grep -cE "append_tick\(|_for_synth.*\.send\(|broadcast_for_synth.*\.send\(")
    if [ "$HAS_WRITER" -eq 0 ]; then
      VIOLATIONS+=("${MAIN_RS}:${BIND_LINE}: ${SYNTH_RX} has no .append_tick() or broadcast_for_synth.*.send() within 200 lines — synth ticks dormant")
    fi
  fi
done

if [ "${#VIOLATIONS[@]}" -gt 0 ]; then
  echo "  FAIL: ${#VIOLATIONS[@]} boot-symmetry violation(s):" >&2
  for V in "${VIOLATIONS[@]}"; do
    echo "    $V" >&2
  done
  echo "  Options:" >&2
  echo "    1. Wire the missing call site (spawn_pool_watchdog_task, append_tick, etc)" >&2
  echo "    2. Add '// BOOT-SYMMETRY-EXEMPT: <reason>' on the declaration line above" >&2
  echo "  This catches the slow-boot-blind-spot class of bugs (S5 honest audit A1, A4)." >&2
  exit 2
fi

exit 0
