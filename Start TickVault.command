#!/bin/bash
# Double-clickable MANUAL START (operator 2026-07-04: "let me autostart or
# autostop manually also"). Starts Docker + QuestDB + the app + keep-awake,
# clears the manual-stop marker so the autopilot resumes normal duty, and
# then tails the live app log. Closing this window does NOT stop the app.
cd "$(dirname "$0")" || exit 1
echo "================ TickVault — MANUAL START ================"
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
