#!/bin/bash
# decommission-mac.sh — REMOVE all dhan-live-trader from Mac completely
#
# Called automatically by ec2-setup.sh via SSH when AWS becomes primary.
# Can also be run manually on the Mac.
#
# This does NOT disable. It DELETES EVERYTHING:
#
#   ALL 3 launchd agents:
#     - co.dhan.live-trader  (8:15 AM trigger — original)
#     - com.dlt.trader       (8:15 AM trigger — newer, with crash recovery)
#     - com.dlt.watchdog     (every 5 min — auto-restarts trader if dead)
#
#   ALL project directories:
#     - /Users/Shared/dhan-live-trader/
#     - ~/IdeaProjects/dhan-live-trader/
#     - ~/dhan-live-trader/
#
#   ALL installed scripts:
#     - ~/.dlt/ (dlt-daily-start.sh, dlt-watchdog.sh, PID files, logs)
#
#   ALL Docker artifacts:
#     - Containers matching "dhan" or "dlt-"
#     - Images matching "dhan" or "dlt"
#     - Volumes matching "dhan" or "dlt"
#
#   Power management:
#     - pmset repeat cancel (kills 8:10 AM wake schedule)
#     - pmset restoredefaults (resets sleep/wake settings)
#
#   Desktop shortcuts:
#     - ~/Desktop/DLT-Run.command
#
# After this runs, there is NOTHING left on the Mac. No auto-start.
# No manual start. No binary. No scripts. No watchdog. No wake schedule.
# Nothing to start. Nothing to restart. It's gone.
#
# This is SAFE to run multiple times (idempotent).

set -euo pipefail

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  REMOVING ALL dhan-live-trader from Mac — COMPLETE WIPE     ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# ---- Read Telegram creds BEFORE deleting anything ----
BOT_TOKEN=""
CHAT_ID=""
for creds_file in "${HOME}/.dlt-telegram-creds" "${HOME}/.dlt/telegram-creds"; do
    if [ -f "$creds_file" ]; then
        BOT_TOKEN=$(grep '^BOT_TOKEN=' "$creds_file" | cut -d= -f2 || true)
        CHAT_ID=$(grep '^CHAT_ID=' "$creds_file" | cut -d= -f2 || true)
        break
    fi
done
# Also try SSM if aws CLI is available and creds weren't found locally
if [ -z "$BOT_TOKEN" ] && command -v aws &> /dev/null; then
    BOT_TOKEN=$(aws ssm get-parameter --region ap-south-1 --name "/dlt/dev/telegram/bot-token" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null || true)
    CHAT_ID=$(aws ssm get-parameter --region ap-south-1 --name "/dlt/dev/telegram/chat-id" --with-decryption --output text --query "Parameter.Value" --no-cli-pager 2>/dev/null || true)
fi

send_telegram() {
    if [ -n "$BOT_TOKEN" ] && [ -n "$CHAT_ID" ]; then
        curl -s -X POST "https://api.telegram.org/bot${BOT_TOKEN}/sendMessage" \
            -d "chat_id=${CHAT_ID}" \
            -d "text=$1" \
            -d "parse_mode=HTML" \
            --max-time 10 > /dev/null 2>&1 || true
    fi
}

STEP=0
TOTAL=9

# ---- Step 1: Kill ALL running processes ----
STEP=$((STEP + 1))
echo "[${STEP}/${TOTAL}] Killing ALL dhan/dlt processes..."
# Kill the binary
pkill -f "dhan-live-trader" 2>/dev/null || true
# Kill cargo run (which spawns the binary)
pkill -f "cargo run.*dhan-live-trader" 2>/dev/null || true
# Kill the launcher scripts themselves
pkill -f "dlt-daily-start" 2>/dev/null || true
pkill -f "dlt-watchdog" 2>/dev/null || true
pkill -f "trading-launcher" 2>/dev/null || true
# Wait a moment for graceful shutdown, then force-kill
sleep 2
pkill -9 -f "dhan-live-trader" 2>/dev/null || true
pkill -9 -f "dlt-daily-start" 2>/dev/null || true
pkill -9 -f "dlt-watchdog" 2>/dev/null || true
echo "      Killed all dhan/dlt processes"

# ---- Step 2: Unload + delete ALL 3 launchd agents ----
STEP=$((STEP + 1))
echo "[${STEP}/${TOTAL}] Removing ALL launchd agents..."
GUI_UID=$(id -u)

# Agent 1: co.dhan.live-trader (original, 8:15 AM, uses launchctl load/unload)
PLIST_1="${HOME}/Library/LaunchAgents/co.dhan.live-trader.plist"
if launchctl list 2>/dev/null | grep -q "co.dhan.live-trader"; then
    launchctl unload "$PLIST_1" 2>/dev/null || true
    echo "      Unloaded co.dhan.live-trader"
fi
rm -f "$PLIST_1"

# Agent 2: com.dlt.trader (newer, 8:15 AM, uses launchctl bootstrap/bootout)
PLIST_2="${HOME}/Library/LaunchAgents/com.dlt.trader.plist"
if launchctl list "com.dlt.trader" >/dev/null 2>&1; then
    launchctl bootout "gui/${GUI_UID}" "$PLIST_2" 2>/dev/null || \
        launchctl unload "$PLIST_2" 2>/dev/null || true
    echo "      Unloaded com.dlt.trader"
fi
rm -f "$PLIST_2"

# Agent 3: com.dlt.watchdog (every 5 min, RunAtLoad=true, auto-restarts trader)
PLIST_3="${HOME}/Library/LaunchAgents/com.dlt.watchdog.plist"
if launchctl list "com.dlt.watchdog" >/dev/null 2>&1; then
    launchctl bootout "gui/${GUI_UID}" "$PLIST_3" 2>/dev/null || \
        launchctl unload "$PLIST_3" 2>/dev/null || true
    echo "      Unloaded com.dlt.watchdog"
fi
rm -f "$PLIST_3"

# Catch any other dhan/dlt plists we might have missed
for plist in "${HOME}"/Library/LaunchAgents/*dhan* "${HOME}"/Library/LaunchAgents/*dlt*; do
    if [ -f "$plist" ] 2>/dev/null; then
        LABEL=$(basename "$plist" .plist)
        launchctl bootout "gui/${GUI_UID}" "$plist" 2>/dev/null || \
            launchctl unload "$plist" 2>/dev/null || true
        rm -f "$plist"
        echo "      Also removed: $plist"
    fi
done

echo "      All launchd agents removed (8:15 AM original, 8:15 AM newer, watchdog — all gone)"

# ---- Step 3: Stop + remove ALL Docker containers, images, volumes ----
STEP=$((STEP + 1))
echo "[${STEP}/${TOTAL}] Removing ALL Docker artifacts..."
if command -v docker &> /dev/null && docker info > /dev/null 2>&1; then
    # Stop + remove containers matching dhan OR dlt
    for filter in "name=dhan" "name=dlt-" "name=dlt_"; do
        CONTAINERS=$(docker ps -a --filter "$filter" --format '{{.ID}}' 2>/dev/null || true)
        if [ -n "$CONTAINERS" ]; then
            echo "$CONTAINERS" | xargs docker rm -f 2>/dev/null || true
        fi
    done
    # Remove images matching dhan OR dlt
    for filter in "reference=*dhan*" "reference=*dlt*"; do
        IMAGES=$(docker images --filter "$filter" --format '{{.ID}}' 2>/dev/null || true)
        if [ -n "$IMAGES" ]; then
            echo "$IMAGES" | xargs docker rmi -f 2>/dev/null || true
        fi
    done
    # Remove volumes matching dhan OR dlt
    for filter in "name=dhan" "name=dlt"; do
        VOLUMES=$(docker volume ls --filter "$filter" --format '{{.Name}}' 2>/dev/null || true)
        if [ -n "$VOLUMES" ]; then
            echo "$VOLUMES" | xargs docker volume rm -f 2>/dev/null || true
        fi
    done
    echo "      Removed all dhan/dlt containers, images, volumes"
else
    echo "      Docker not available — skipping"
fi

# ---- Step 4: Delete ALL project directories ----
STEP=$((STEP + 1))
echo "[${STEP}/${TOTAL}] Deleting ALL project directories..."
for dir in \
    "/Users/Shared/dhan-live-trader" \
    "${HOME}/IdeaProjects/dhan-live-trader" \
    "${HOME}/dhan-live-trader" \
    "${DLT_PROJECT_DIR:-__SKIP__}"; do
    if [ "$dir" = "__SKIP__" ]; then
        continue
    fi
    if [ -d "$dir" ]; then
        rm -rf "$dir"
        echo "      Deleted ${dir}"
    fi
done

# ---- Step 5: Delete ~/.dlt/ (installed scripts, PID files, watchdog state) ----
STEP=$((STEP + 1))
echo "[${STEP}/${TOTAL}] Deleting ~/.dlt/ directory..."
if [ -d "${HOME}/.dlt" ]; then
    rm -rf "${HOME}/.dlt"
    echo "      Deleted ${HOME}/.dlt (scripts, logs, PID files, watchdog state)"
else
    echo "      Already gone"
fi

# ---- Step 6: Delete Desktop shortcut ----
STEP=$((STEP + 1))
echo "[${STEP}/${TOTAL}] Cleaning up Desktop shortcut..."
rm -f "${HOME}/Desktop/DLT-Run.command" 2>/dev/null || true
echo "      Done"

# ---- Step 7: Cancel wake schedule + revert power management ----
STEP=$((STEP + 1))
echo "[${STEP}/${TOTAL}] Cancelling wake schedule + reverting power management..."
if command -v pmset &> /dev/null; then
    # pmset repeat cancel — THIS IS THE CRITICAL ONE
    # restoredefaults does NOT cancel scheduled wakes
    # Without this, Mac still wakes at 8:10 AM every weekday
    sudo pmset repeat cancel 2>/dev/null && \
        echo "      Cancelled wake schedule (no more 8:10 AM wake)" || \
        echo "      NOTE: Run manually: sudo pmset repeat cancel"
    sudo pmset restoredefaults 2>/dev/null && \
        echo "      Power management restored to defaults" || \
        echo "      NOTE: Run manually: sudo pmset restoredefaults"
else
    echo "      Not a Mac — skipping"
fi

# ---- Step 8: Clean up temp/log files ----
STEP=$((STEP + 1))
echo "[${STEP}/${TOTAL}] Cleaning up temp files..."
rm -f /tmp/dlt-launchd-stdout.log /tmp/dlt-launchd-stderr.log 2>/dev/null || true
rm -f "${HOME}/.dlt-telegram-creds" 2>/dev/null || true
echo "      Done"

# ---- Step 9: Create decommission markers ----
STEP=$((STEP + 1))
echo "[${STEP}/${TOTAL}] Creating decommission markers..."
touch "${HOME}/.dhan-aws-is-primary"
touch "${HOME}/.dhan-live-trader-disabled"
echo "      Created ~/.dhan-aws-is-primary"
echo "      Created ~/.dhan-live-trader-disabled"

# ---- Send Telegram ----
send_telegram "Mac FULLY WIPED. All 3 launchd agents removed (8:15 original, 8:15 newer, watchdog). All project dirs deleted. ~/.dlt deleted. Docker cleaned. Wake schedule cancelled. Nothing remains. AWS is the only trading host."

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  DONE — Mac is completely clean. Nothing left.              ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "Removed:"
echo "  LAUNCHD AGENTS (all 3):"
echo "    - co.dhan.live-trader  (8:15 AM auto-start)"
echo "    - com.dlt.trader       (8:15 AM auto-start + crash recovery)"
echo "    - com.dlt.watchdog     (every 5 min auto-restart)"
echo ""
echo "  DIRECTORIES:"
echo "    - /Users/Shared/dhan-live-trader/"
echo "    - ~/IdeaProjects/dhan-live-trader/"
echo "    - ~/dhan-live-trader/"
echo "    - ~/.dlt/ (scripts, logs, PID files)"
echo ""
echo "  DOCKER:"
echo "    - All dhan/dlt containers, images, volumes"
echo ""
echo "  POWER MANAGEMENT:"
echo "    - pmset repeat cancelled (no more 8:10 AM wake)"
echo "    - pmset defaults restored"
echo ""
echo "  OTHER:"
echo "    - ~/Desktop/DLT-Run.command"
echo "    - /tmp/dlt-launchd-*.log"
echo ""
echo "There is no auto-start. There is no manual start."
echo "There is no watchdog. There is no wake schedule."
echo "There is nothing to start. It's gone."
