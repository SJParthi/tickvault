#!/bin/bash
# Double-clickable MANUAL START (operator 2026-07-04: "let me autostart or
# autostop manually also"). Starts Docker + QuestDB + the app + keep-awake,
# clears the manual-stop marker so the autopilot resumes normal duty, and
# then tails the live app log. Closing this window does NOT stop the app.
cd "$(dirname "$0")" || exit 1
echo "================ TickVault — MANUAL START ================"

# Step 1 — SELF-UPDATE (2026-07-04, operator: the two buttons are the ENTIRE
# interface — no terminal ever). Fast-forward to the latest local-runtime
# BEFORE anything else runs, so the Start button always launches the newest
# code. Failure is NEVER fatal (mirrors the autopilot's preflight_git): a
# plain-English line + a best-effort Telegram, then continue with the code
# already on disk.
tg_note() { # best-effort Telegram; never blocks the start
  bash scripts/notify-telegram.sh "💻 LOCAL autopilot — $*" >/dev/null 2>&1 || true
}
current_branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "?")
if [ "$current_branch" = "local-runtime" ]; then
  echo "Checking for updates..."
  if git fetch origin local-runtime >/dev/null 2>&1; then
    if git merge --ff-only origin/local-runtime >/dev/null 2>&1; then
      echo "Code is up to date."
    else
      echo "Could not apply the latest update (local changes are in the way) — starting with the code already on this Mac."
      tg_note "⚠️ Start button could not apply the latest update — started with the code already on the Mac. Everything still runs."
    fi
  else
    echo "No internet or GitHub unreachable — starting with the code already on this Mac."
  fi
else
  echo "Not on the local-runtime branch ($current_branch) — skipping self-update."
fi

# Step 2 — Zero-manual-step install (2026-07-04): if the daily auto-start launchd
# agent is missing or not loaded, install + load it right here — same logic
# as `make local-autopilot-install`, so double-clicking this file is the
# ONLY setup step the operator ever needs. Idempotent: an already-loaded
# agent is left untouched. macOS only; failures are non-fatal (the manual
# start below still runs) but are printed so the operator can see them.
AUTOPILOT_LABEL="com.tickvault.local-autopilot"
AUTOPILOT_PLIST="$HOME/Library/LaunchAgents/$AUTOPILOT_LABEL.plist"
AUTOPILOT_TEMPLATE="deploy/local/$AUTOPILOT_LABEL.plist.template"
if [ "$(uname -s)" = "Darwin" ] && [ -f "$AUTOPILOT_TEMPLATE" ]; then
  agent_loaded=0
  launchctl print "gui/$(id -u)/$AUTOPILOT_LABEL" >/dev/null 2>&1 && agent_loaded=1
  if [ ! -f "$AUTOPILOT_PLIST" ] || [ "$agent_loaded" = "0" ]; then
    echo "Setting up the daily auto-start (one-time, automatic)..."
    mkdir -p data/local-autopilot "$HOME/Library/LaunchAgents"
    if sed -e "s|__REPO__|$PWD|g" -e "s|__HOME__|$HOME|g" \
      "$AUTOPILOT_TEMPLATE" >"$AUTOPILOT_PLIST"; then
      launchctl bootout "gui/$(id -u)/$AUTOPILOT_LABEL" 2>/dev/null || true
      launchctl bootstrap "gui/$(id -u)" "$AUTOPILOT_PLIST" 2>/dev/null ||
        launchctl load "$AUTOPILOT_PLIST" 2>/dev/null || true
      if launchctl print "gui/$(id -u)/$AUTOPILOT_LABEL" >/dev/null 2>&1; then
        echo "Daily auto-start installed — fires weekdays at 8:55 AM."
      else
        echo "WARNING: could not load the daily auto-start agent (manual start below still works)."
        echo "         Run 'make local-autopilot-install' in Terminal to see the error."
      fi
    else
      echo "WARNING: could not write $AUTOPILOT_PLIST (manual start below still works)."
    fi
  fi
fi

# Step 3 — the actual start (docker up → database up + self-heal → app →
# keep-awake → live log). Runs the possibly-just-updated script fresh.
bash scripts/local-autopilot.sh start
rc=$?
echo "==========================================================="
if [ $rc -ne 0 ]; then
  echo "START FAILED (see messages above / Telegram)."
  read -r -p "Press Enter to close..." _
  exit $rc
fi
echo "App is running. Live log below — closing this window is safe."
echo "-----------------------------------------------------------"
# Tail the app's own canonical log (the human one); fall back to the ONE
# common autopilot log if the app has not created its file yet.
APP_LOG_FILE=$(ls -t data/logs/app.*.log 2>/dev/null | head -1)
if [ -z "$APP_LOG_FILE" ]; then APP_LOG_FILE="data/logs/autopilot/autopilot.log"; fi
exec tail -f "$APP_LOG_FILE"
