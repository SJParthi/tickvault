#!/bin/bash
# Double-clickable MANUAL STOP. Gracefully stops the app, cleans the scale
# overlay, and writes the manual-stop marker — the autopilot will NOT
# auto-restart anything for the rest of the day (manual always wins).
# QuestDB stays up so the day's data remains queryable.
cd "$(dirname "$0")" || exit 1
echo "================ TickVault — MANUAL STOP ================="
bash scripts/local-autopilot.sh stop
echo "=========================================================="
echo "Stopped. The autopilot stands down until you double-click"
echo "'Start TickVault' (or until the next trading day begins)."
echo
bash scripts/local-autopilot.sh status
echo
read -r -p "Press Enter to close..." _
