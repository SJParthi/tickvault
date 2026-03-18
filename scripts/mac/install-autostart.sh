#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Install Auto-Start + Watchdog (macOS)
# =============================================================================
# One-time setup. Run from the repo root:
#   ./scripts/mac/install-autostart.sh
#
# What it does:
#   1. Copies all scripts to ~/.dlt/ (keeps repo clean)
#   2. Installs TWO launchd agents:
#      - com.dlt.trader   — daily 8:15 AM auto-start with crash recovery
#      - com.dlt.watchdog  — every 5 min health monitor + auto-recovery
#   3. Schedules Mac wake at 8:10 AM via pmset (requires sudo)
#   4. Runs self-test to validate everything works
#
# To uninstall (clean removal for AWS migration):
#   ./scripts/mac/install-autostart.sh --uninstall
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

INSTALL_DIR="${HOME}/.dlt"
LAUNCHAGENTS_DIR="${HOME}/Library/LaunchAgents"

TRADER_LABEL="com.dlt.trader"
WATCHDOG_LABEL="com.dlt.watchdog"
TRADER_PLIST="${LAUNCHAGENTS_DIR}/${TRADER_LABEL}.plist"
WATCHDOG_PLIST="${LAUNCHAGENTS_DIR}/${WATCHDOG_LABEL}.plist"

# ---- Uninstall mode ----
if [ "${1:-}" = "--uninstall" ]; then
    echo ""
    echo -e "${CYAN}DLT Auto-Start — Full Uninstall${NC}"
    echo -e "${CYAN}================================${NC}"
    echo ""

    # Stop trader if running
    if [ -f "${INSTALL_DIR}/trader.pid" ]; then
        PID=$(cat "${INSTALL_DIR}/trader.pid" 2>/dev/null || echo "")
        if [ -n "${PID}" ] && kill -0 "${PID}" 2>/dev/null; then
            echo -e "  Stopping trader (PID ${PID})..."
            kill "${PID}" 2>/dev/null || true
            sleep 2
        fi
        rm -f "${INSTALL_DIR}/trader.pid"
    fi

    # Unload launchd agents
    for label in "${TRADER_LABEL}" "${WATCHDOG_LABEL}"; do
        plist_path="${LAUNCHAGENTS_DIR}/${label}.plist"
        if launchctl list "${label}" >/dev/null 2>&1; then
            launchctl bootout "gui/$(id -u)" "${plist_path}" 2>/dev/null || true
            echo -e "  ${GREEN}Unloaded ${label}${NC}"
        fi
        rm -f "${plist_path}"
    done

    # Remove installed scripts (keep logs for post-mortem)
    for file in dlt-daily-start.sh dlt-watchdog.sh dlt-stop.sh dlt-status.sh dlt-selftest.sh; do
        rm -f "${INSTALL_DIR}/${file}"
    done
    rm -f "${INSTALL_DIR}/restart-count"
    rm -f "${INSTALL_DIR}/watchdog-state"
    echo -e "  ${GREEN}Removed installed scripts${NC}"

    # Cancel wake schedule
    echo ""
    echo -e "  ${YELLOW}To also cancel the wake schedule:${NC}"
    echo -e "  ${YELLOW}  sudo pmset repeat cancel${NC}"
    echo ""
    echo -e "${GREEN}Uninstalled.${NC} Logs preserved in ~/.dlt/logs/ for review."
    echo -e "Safe to delete entire ~/.dlt/ directory if no longer needed."
    exit 0
fi

# ---- Prerequisite checks ----
echo ""
echo -e "${CYAN}╔══════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   DLT Auto-Start + Watchdog Installer (macOS)    ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════════╝${NC}"
echo ""

# Verify macOS
if [ "$(uname)" != "Darwin" ]; then
    echo -e "${RED}This script is macOS-only.${NC}"
    exit 1
fi

# Verify project structure
if [ ! -f "${PROJECT_DIR}/Cargo.toml" ]; then
    echo -e "${RED}Cannot find Cargo.toml at ${PROJECT_DIR}${NC}"
    exit 1
fi

# Verify critical tools
for tool in cargo docker aws; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo -e "${RED}Required tool '${tool}' not found.${NC}"
        exit 1
    fi
done
echo -e "  ${GREEN}Prerequisites OK${NC} (cargo, docker, aws)"

# Timezone check
TZ_NAME=$(readlink /etc/localtime 2>/dev/null | sed 's|.*/zoneinfo/||' || echo "unknown")
if [ "${TZ_NAME}" != "Asia/Kolkata" ]; then
    echo -e "  ${YELLOW}WARNING: Timezone is '${TZ_NAME}', not 'Asia/Kolkata'${NC}"
    echo -e "  ${YELLOW}Fix: System Settings > General > Date & Time > Kolkata${NC}"
    echo ""
fi

# ---- 1. Install scripts to ~/.dlt/ ----
echo -e "${CYAN}[1/4] Installing scripts to ${INSTALL_DIR}/${NC}"
mkdir -p "${INSTALL_DIR}/logs"

# Scripts that need PROJECT_DIR placeholder replaced
for file in dlt-daily-start.sh dlt-watchdog.sh dlt-selftest.sh; do
    sed "s|__PROJECT_DIR__|${PROJECT_DIR}|g" \
        "${SCRIPT_DIR}/${file}" > "${INSTALL_DIR}/${file}"
    chmod +x "${INSTALL_DIR}/${file}"
    echo -e "  ${GREEN}${file}${NC}"
done

# Scripts with no placeholders
for file in dlt-stop.sh dlt-status.sh; do
    cp "${SCRIPT_DIR}/${file}" "${INSTALL_DIR}/${file}"
    chmod +x "${INSTALL_DIR}/${file}"
    echo -e "  ${GREEN}${file}${NC}"
done

# ---- 2. Install launchd agents ----
echo ""
echo -e "${CYAN}[2/4] Installing launchd agents${NC}"
mkdir -p "${LAUNCHAGENTS_DIR}"

# Unload existing agents
for label in "${TRADER_LABEL}" "${WATCHDOG_LABEL}"; do
    if launchctl list "${label}" >/dev/null 2>&1; then
        echo -e "  ${YELLOW}Removing existing ${label}...${NC}"
        launchctl bootout "gui/$(id -u)" "${LAUNCHAGENTS_DIR}/${label}.plist" 2>/dev/null || true
    fi
done

# Install trader plist
sed -e "s|__INSTALL_DIR__|${INSTALL_DIR}|g" \
    -e "s|__HOME__|${HOME}|g" \
    "${SCRIPT_DIR}/com.dlt.trader.plist" > "${TRADER_PLIST}"

if ! plutil -lint "${TRADER_PLIST}" >/dev/null 2>&1; then
    echo -e "  ${RED}Trader plist validation failed!${NC}"
    plutil -lint "${TRADER_PLIST}"
    exit 1
fi
launchctl bootstrap "gui/$(id -u)" "${TRADER_PLIST}"
echo -e "  ${GREEN}com.dlt.trader${NC} — daily at 8:15 AM (auto-start + crash recovery)"

# Install watchdog plist
sed -e "s|__INSTALL_DIR__|${INSTALL_DIR}|g" \
    -e "s|__HOME__|${HOME}|g" \
    "${SCRIPT_DIR}/com.dlt.watchdog.plist" > "${WATCHDOG_PLIST}"

if ! plutil -lint "${WATCHDOG_PLIST}" >/dev/null 2>&1; then
    echo -e "  ${RED}Watchdog plist validation failed!${NC}"
    plutil -lint "${WATCHDOG_PLIST}"
    exit 1
fi
launchctl bootstrap "gui/$(id -u)" "${WATCHDOG_PLIST}"
echo -e "  ${GREEN}com.dlt.watchdog${NC} — every 5 minutes (health monitor + auto-recovery)"

# ---- 3. Schedule Mac wake (skip if already set) ----
echo ""
echo -e "${CYAN}[3/4] Scheduling Mac wake at 8:10 AM${NC}"

if pmset -g sched 2>/dev/null | grep -qi "wake.*08:10"; then
    echo -e "  ${GREEN}Already scheduled — skipping (no password needed)${NC}"
else
    echo -e "  ${YELLOW}This requires sudo (one-time only)${NC}"
    echo ""
    sudo pmset repeat wakeorpoweron MTWRFSU 08:10:00
    echo -e "  ${GREEN}Mac will wake at 8:10 AM every day${NC}"
fi

# ---- 4. Run self-test ----
echo ""
echo -e "${CYAN}[4/4] Running self-test...${NC}"
echo ""
bash "${INSTALL_DIR}/dlt-selftest.sh" --quick || true

# ---- Summary ----
echo ""
echo -e "${GREEN}╔══════════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║   Installation complete!                              ║${NC}"
echo -e "${GREEN}╠══════════════════════════════════════════════════════╣${NC}"
echo -e "${GREEN}║                                                        ║${NC}"
echo -e "${GREEN}║   Two layers of reliability:                           ║${NC}"
echo -e "${GREEN}║                                                        ║${NC}"
echo -e "${GREEN}║   Layer 1: Daily Auto-Start (com.dlt.trader)           ║${NC}"
echo -e "${GREEN}║     8:10 AM — Mac wakes from sleep                     ║${NC}"
echo -e "${GREEN}║     8:15 AM — Docker + infra + cargo run + IntelliJ    ║${NC}"
echo -e "${GREEN}║     Crash → auto-restart (5x max, exponential backoff) ║${NC}"
echo -e "${GREEN}║     Telegram alert on every lifecycle event             ║${NC}"
echo -e "${GREEN}║                                                        ║${NC}"
echo -e "${GREEN}║   Layer 2: Watchdog (com.dlt.watchdog)                 ║${NC}"
echo -e "${GREEN}║     Every 5 min: process + Docker + infra + ticks      ║${NC}"
echo -e "${GREEN}║     Auto-recovers: restarts trader, Docker, containers ║${NC}"
echo -e "${GREEN}║     Telegram alert on every issue detected              ║${NC}"
echo -e "${GREEN}║                                                        ║${NC}"
echo -e "${GREEN}║   Commands:                                             ║${NC}"
echo -e "${GREEN}║     ~/.dlt/dlt-status.sh     — full status dashboard    ║${NC}"
echo -e "${GREEN}║     ~/.dlt/dlt-stop.sh       — stop trader              ║${NC}"
echo -e "${GREEN}║     ~/.dlt/dlt-selftest.sh   — validate installation    ║${NC}"
echo -e "${GREEN}║                                                        ║${NC}"
echo -e "${GREEN}║   Run in IntelliJ instead:                              ║${NC}"
echo -e "${GREEN}║     1. ~/.dlt/dlt-stop.sh   (stop headless)            ║${NC}"
echo -e "${GREEN}║     2. Hit Run in IntelliJ  (starts with debugger)     ║${NC}"
echo -e "${GREEN}║                                                        ║${NC}"
echo -e "${GREEN}║   Uninstall (before AWS migration):                     ║${NC}"
echo -e "${GREEN}║     ./scripts/mac/install-autostart.sh --uninstall     ║${NC}"
echo -e "${GREEN}║     sudo pmset repeat cancel                            ║${NC}"
echo -e "${GREEN}║                                                        ║${NC}"
echo -e "${GREEN}╚══════════════════════════════════════════════════════╝${NC}"
echo ""
