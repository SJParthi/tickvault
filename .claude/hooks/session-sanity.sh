#!/bin/bash
# session-sanity.sh — Lightweight sanity check on every session start
# MUST be fast (<5s). Heavy checks belong in pre-commit/pre-push gates.
# No blocking network ops. No cargo check (commit gate covers it).

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$CWD" || exit 0

echo "SESSION START SANITY CHECK" >&2
echo "=========================" >&2

# Check 1: Current branch
BRANCH=$(git branch --show-current 2>/dev/null || echo "detached")
echo "Branch: $BRANCH" >&2

# Check 2: Uncommitted changes?
DIRTY=$(git status --porcelain 2>/dev/null | head -5)
if [ -n "$DIRTY" ]; then
  echo "WARNING: Uncommitted changes:" >&2
  echo "$DIRTY" >&2
  echo "" >&2
else
  echo "Working tree: clean" >&2
fi

# Check 3: Test count baseline (fast file read, no compilation)
HOOKS_DIR="$CWD/.claude/hooks"
if [ -f "$HOOKS_DIR/.test-count-baseline" ]; then
  BASELINE=$(cat "$HOOKS_DIR/.test-count-baseline" | tr -d ' \n')
  echo "Test baseline: $BASELINE tests" >&2
fi

# Check 4: Network ops in background (non-blocking, cached)
# Push branch to remote + collision detection — runs async, does NOT block session
# CRITICAL: redirect stdout+stderr to log file and disown, otherwise the hook
# framework waits for stderr to close and the entire SessionStart hangs.
# OPTIMIZATION: Skip remote ref check if last check was <1 hour ago.
NETWORK_LOG="$CWD/.claude/hooks/.session-network.log"
SESSION_CHECK_CACHE="$CWD/.claude/hooks/.last-session-check"
SKIP_NETWORK=false
if [ -f "$SESSION_CHECK_CACHE" ]; then
  CACHE_TIME=$(cat "$SESSION_CHECK_CACHE" 2>/dev/null | tr -d ' \n')
  NOW=$(date +%s)
  CACHE_AGE=$(( NOW - CACHE_TIME ))
  if [ "$CACHE_AGE" -lt 3600 ]; then
    SKIP_NETWORK=true
  fi
fi

if [ "$SKIP_NETWORK" = "false" ]; then
  (
    if [ "$BRANCH" != "detached" ]; then
      PUSH_LOCK="$CWD/.claude/hooks/.push-in-progress"
      if [ -f "$PUSH_LOCK" ]; then
        echo "[$(date '+%H:%M:%S')] Push lock found, skipping session push"
      else
        REMOTE_REF=$(git -C "$CWD" ls-remote --heads origin "$BRANCH" 2>/dev/null | head -1)
        if [ -z "$REMOTE_REF" ]; then
          timeout 15 git -C "$CWD" push -u origin "$BRANCH" 2>/dev/null || true
        fi
      fi
    fi

    # Lightweight collision warning (ls-remote only, no fetch)
    REMOTE_REFS=$(timeout 10 git -C "$CWD" ls-remote origin 'refs/auto-save/*' 2>/dev/null)
    if [ -n "$REMOTE_REFS" ]; then
      echo "[$(date '+%H:%M:%S')] Found auto-save snapshots from previous sessions."
    fi

    # Update cache timestamp
    date +%s > "$SESSION_CHECK_CACHE"
  ) >> "$NETWORK_LOG" 2>&1 &
  disown
else
  echo "Network check: cached (last check <1hr ago)" >&2
fi

# Check 5: Quick cargo check (catches broken state before Claude starts coding)
# Only runs on fresh sessions (startup), not resume/compact (already checked)
INPUT=$(echo "$@" 2>/dev/null || echo "")
if echo "$INPUT" | grep -q "startup" 2>/dev/null || [ -z "$INPUT" ]; then
  echo "Running cargo check..." >&2
  CARGO_CHECK=$(cd "$CWD" && cargo check --workspace 2>&1)
  CARGO_EXIT=$?
  if [ $CARGO_EXIT -ne 0 ]; then
    echo "WARNING: cargo check FAILED — workspace has compile errors" >&2
    echo "$CARGO_CHECK" | grep '^error' | head -5 >&2
    echo "" >&2
  else
    echo "cargo check: PASS" >&2
  fi
fi

# Remind about principles and phase
echo "" >&2
echo "Three principles: (1) Zero alloc hot path (2) O(1) or compile fail (3) All versions pinned" >&2
echo "Current phase: docs/phases/phase-1-live-trading.md" >&2

# AUTOMATION BRIEF — every session sees the zero-touch entry points
echo "" >&2
echo "ZERO-TOUCH AUTOMATION (use these BEFORE grepping or tailing logs):" >&2
echo "  make doctor              # 7-section system health (docker + endpoints + logs)" >&2
echo "  make validate-automation # 30 guard-suite checks (proves the chain is intact)" >&2
echo "  make tail-errors         # live JSONL tail of structured ERROR events" >&2
echo "  make errors-summary      # human-readable snapshot (refreshed every 60s by the app)" >&2
echo "  make triage-dry-run      # what would auto-fix do? (inspect before execute)" >&2
echo "  make triage-execute      # run the auto-fix against today's novel signatures" >&2
echo "MCP live tools (auto-loaded via .mcp.json — 12 tools, prefix mcp__tickvault-logs__):" >&2
echo "  tail_errors, list_novel_signatures, summary_snapshot, triage_log_tail," >&2
echo "  signature_history, prometheus_query, find_runbook_for_code, list_active_alerts," >&2
echo "  questdb_sql, grep_codebase, run_doctor, git_recent_log" >&2
echo "Runbooks (for operator + any AI session): docs/runbooks/README.md (54-code index)" >&2
echo "Architecture + guarantees: docs/architecture/{zero-touch-stack,guarantees,claude-cowork}.md" >&2

# ERROR SUMMARY GLANCE — one-line answer to "anything broken?"
SUMMARY_FILE="$CWD/data/logs/errors.summary.md"
if [ -f "$SUMMARY_FILE" ]; then
  TOTAL=$(grep -oE 'Total events: [0-9]+' "$SUMMARY_FILE" 2>/dev/null | head -1 | awk '{print $3}')
  NOVEL=$(grep -oE 'Novel signatures: [0-9]+' "$SUMMARY_FILE" 2>/dev/null | head -1 | awk '{print $3}')
  if [ -n "$TOTAL" ] || [ -n "$NOVEL" ]; then
    echo "Error snapshot: total=${TOTAL:-?}  novel=${NOVEL:-?}  (open data/logs/errors.summary.md)" >&2
  fi
fi
