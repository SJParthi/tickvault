#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Full Status Dashboard (macOS)
# =============================================================================
# Shows current state of the entire auto-start + watchdog system.
# =============================================================================

set -euo pipefail

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

PID_FILE="${HOME}/.dlt/trader.pid"
RESTART_COUNT_FILE="${HOME}/.dlt/restart-count"
WATCHDOG_STATE="${HOME}/.dlt/watchdog-state"
AUTOSTART_LOG="${HOME}/.dlt/logs/autostart-$(date +%Y-%m-%d).log"
WATCHDOG_LOG="${HOME}/.dlt/logs/watchdog-$(date +%Y-%m-%d).log"

echo ""
echo -e "${CYAN}╔══════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   DLT Auto-Start Status Dashboard        ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════╝${NC}"
echo ""

# ---- Trader Process ----
echo -e "${CYAN}Trader:${NC}"
echo -n "  Process:          "
if [ -f "${PID_FILE}" ]; then
    PID=$(cat "${PID_FILE}" 2>/dev/null || echo "")
    if [ -n "${PID}" ] && kill -0 "${PID}" 2>/dev/null; then
        UPTIME=$(ps -o etime= -p "${PID}" 2>/dev/null | xargs)
        echo -e "${GREEN}RUNNING${NC} (PID ${PID}, uptime: ${UPTIME})"
    else
        echo -e "${RED}DEAD${NC} (stale PID ${PID})"
    fi
else
    echo -e "${YELLOW}NOT STARTED${NC}"
fi

echo -n "  Restarts today:   "
if [ -f "${RESTART_COUNT_FILE}" ]; then
    COUNT=$(cat "${RESTART_COUNT_FILE}" 2>/dev/null || echo "0")
    if [ "${COUNT}" -eq 0 ]; then
        echo -e "${GREEN}0${NC} (no crashes)"
    elif [ "${COUNT}" -lt 5 ]; then
        echo -e "${YELLOW}${COUNT}${NC} (recovered)"
    else
        echo -e "${RED}${COUNT}${NC} (MAX REACHED — halted)"
    fi
else
    echo -e "${GREEN}0${NC}"
fi
echo ""

# ---- Docker ----
echo -e "${CYAN}Infrastructure:${NC}"
echo -n "  Docker Desktop:   "
if docker info >/dev/null 2>&1; then
    echo -e "${GREEN}RUNNING${NC}"

    echo -n "  Containers:       "
    RUNNING=$(docker ps --filter "name=dlt-" --format '{{.Names}}' 2>/dev/null | wc -l | xargs)
    if [ "${RUNNING}" -ge 8 ]; then
        echo -e "${GREEN}${RUNNING}/8 UP${NC}"
    elif [ "${RUNNING}" -gt 0 ]; then
        echo -e "${YELLOW}${RUNNING}/8 UP${NC}"
        docker ps --filter "name=dlt-" --format "    {{.Names}}: {{.Status}}" 2>/dev/null
    else
        echo -e "${RED}0/8 (not started)${NC}"
    fi

    echo -n "  QuestDB:          "
    if curl -sf --max-time 2 "http://localhost:9000" >/dev/null 2>&1; then
        echo -e "${GREEN}RESPONDING${NC}"
    else
        echo -e "${RED}NOT RESPONDING${NC}"
    fi
else
    echo -e "${RED}NOT RUNNING${NC}"
fi
echo ""

# ---- launchd Agents ----
echo -e "${CYAN}Automation:${NC}"

echo -n "  Auto-start agent: "
if launchctl list "com.dlt.trader" >/dev/null 2>&1; then
    echo -e "${GREEN}LOADED${NC} (daily at 8:15 AM)"
else
    echo -e "${RED}NOT LOADED${NC}"
fi

echo -n "  Watchdog agent:   "
if launchctl list "com.dlt.watchdog" >/dev/null 2>&1; then
    echo -e "${GREEN}LOADED${NC} (every 5 min)"
else
    echo -e "${RED}NOT LOADED${NC}"
fi

echo -n "  Wake schedule:    "
WAKE_INFO=$(pmset -g sched 2>/dev/null | grep -i "wake" | head -1 | xargs)
if [ -n "${WAKE_INFO}" ]; then
    echo -e "${GREEN}${WAKE_INFO}${NC}"
else
    echo -e "${YELLOW}Not configured${NC}"
fi
echo ""

# ---- Watchdog State ----
if [ -f "${WATCHDOG_STATE}" ]; then
    ACTIVE_ALERTS=$(grep -v "^DATE:" "${WATCHDOG_STATE}" 2>/dev/null | wc -l | xargs)
    if [ "${ACTIVE_ALERTS}" -gt 0 ]; then
        echo -e "${CYAN}Active watchdog alerts:${NC}"
        grep -v "^DATE:" "${WATCHDOG_STATE}" 2>/dev/null | sed 's/^/  ⚠ /'
        echo ""
    fi
fi

# ---- Market Status ----
echo -e "${CYAN}Market:${NC}"
DAY=$(TZ="Asia/Kolkata" date +%u 2>/dev/null || date +%u)
HOUR=$(TZ="Asia/Kolkata" date +%H 2>/dev/null || date +%H)
MINUTE=$(TZ="Asia/Kolkata" date +%M 2>/dev/null || date +%M)
TIME_MINS=$((10#${HOUR} * 60 + 10#${MINUTE}))

echo -n "  Current time IST: "
echo -e "${CYAN}$(TZ='Asia/Kolkata' date '+%H:%M:%S %A' 2>/dev/null || date '+%H:%M:%S')${NC}"

echo -n "  Market status:    "
if [ "${DAY}" -gt 5 ]; then
    echo -e "${YELLOW}WEEKEND${NC}"
elif [ "${TIME_MINS}" -lt 540 ]; then
    echo -e "${YELLOW}PRE-MARKET${NC} (opens 09:15)"
elif [ "${TIME_MINS}" -le 930 ]; then
    echo -e "${GREEN}OPEN${NC} (closes 15:30)"
elif [ "${TIME_MINS}" -le 960 ]; then
    echo -e "${YELLOW}POST-MARKET${NC}"
else
    echo -e "${YELLOW}CLOSED${NC}"
fi
echo ""

# ---- Today's Logs ----
echo -e "${CYAN}Today's auto-start log (last 8 lines):${NC}"
if [ -f "${AUTOSTART_LOG}" ]; then
    tail -8 "${AUTOSTART_LOG}" | sed 's/^/  /'
else
    echo -e "  ${YELLOW}No auto-start log for today.${NC}"
fi
echo ""

echo -e "${CYAN}Today's watchdog log (last 5 lines):${NC}"
if [ -f "${WATCHDOG_LOG}" ]; then
    tail -5 "${WATCHDOG_LOG}" | sed 's/^/  /'
else
    echo -e "  ${YELLOW}No watchdog log for today.${NC}"
fi
echo ""
