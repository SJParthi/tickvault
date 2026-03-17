#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Self-Test (macOS)
# =============================================================================
# Validates the entire auto-start installation. Run after install or anytime.
# Tests every component without starting the trader.
#
# Usage:
#   ~/.dlt/dlt-selftest.sh           # full test
#   ~/.dlt/dlt-selftest.sh --quick   # skip cargo check (faster)
#
# Exit codes:
#   0 = all tests passed
#   1 = one or more tests failed
# =============================================================================

set -euo pipefail

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

# ---- Configuration (replaced by install-autostart.sh) ----
PROJECT_DIR="__PROJECT_DIR__"

QUICK_MODE="${1:-}"
PASS=0
FAIL=0
WARN=0

check() {
    local description="$1"
    local result="$2"  # 0=pass, 1=fail, 2=warn

    if [ "${result}" -eq 0 ]; then
        echo -e "  ${GREEN}PASS${NC}  ${description}"
        PASS=$((PASS + 1))
    elif [ "${result}" -eq 2 ]; then
        echo -e "  ${YELLOW}WARN${NC}  ${description}"
        WARN=$((WARN + 1))
    else
        echo -e "  ${RED}FAIL${NC}  ${description}"
        FAIL=$((FAIL + 1))
    fi
}

echo ""
echo -e "${CYAN}DLT Auto-Start Self-Test${NC}"
echo -e "${CYAN}========================${NC}"
echo ""

# ---- 1. Platform ----
echo -e "${CYAN}Platform:${NC}"

if [ "$(uname)" = "Darwin" ]; then
    check "Running on macOS" 0
else
    check "Running on macOS (got: $(uname))" 1
fi

TZ_NAME=$(readlink /etc/localtime 2>/dev/null | sed 's|.*/zoneinfo/||' || echo "unknown")
if [ "${TZ_NAME}" = "Asia/Kolkata" ]; then
    check "Timezone is Asia/Kolkata (IST)" 0
else
    check "Timezone is Asia/Kolkata (got: ${TZ_NAME})" 2
fi

echo ""

# ---- 2. Tools ----
echo -e "${CYAN}Required tools:${NC}"

for tool in cargo rustc docker aws git curl; do
    if command -v "${tool}" >/dev/null 2>&1; then
        VERSION=$("${tool}" --version 2>/dev/null | head -1 | cut -c1-60)
        check "${tool}: ${VERSION}" 0
    else
        check "${tool}: not found" 1
    fi
done

echo ""

# ---- 3. launchd Agents ----
echo -e "${CYAN}launchd agents:${NC}"

if launchctl list "com.dlt.trader" >/dev/null 2>&1; then
    check "com.dlt.trader agent loaded (daily 8:15 AM)" 0
else
    check "com.dlt.trader agent loaded" 1
fi

if launchctl list "com.dlt.watchdog" >/dev/null 2>&1; then
    check "com.dlt.watchdog agent loaded (every 5 min)" 0
else
    check "com.dlt.watchdog agent loaded" 1
fi

if [ -f "${HOME}/Library/LaunchAgents/com.dlt.trader.plist" ]; then
    if plutil -lint "${HOME}/Library/LaunchAgents/com.dlt.trader.plist" >/dev/null 2>&1; then
        check "Trader plist syntax valid" 0
    else
        check "Trader plist syntax valid" 1
    fi
else
    check "Trader plist exists" 1
fi

if [ -f "${HOME}/Library/LaunchAgents/com.dlt.watchdog.plist" ]; then
    if plutil -lint "${HOME}/Library/LaunchAgents/com.dlt.watchdog.plist" >/dev/null 2>&1; then
        check "Watchdog plist syntax valid" 0
    else
        check "Watchdog plist syntax valid" 1
    fi
else
    check "Watchdog plist exists" 1
fi

echo ""

# ---- 4. Wake Schedule ----
echo -e "${CYAN}Wake schedule:${NC}"

WAKE=$(pmset -g sched 2>/dev/null | grep -i "wake" | head -1)
if [ -n "${WAKE}" ]; then
    check "pmset wake scheduled: ${WAKE}" 0
else
    check "pmset wake schedule configured" 1
fi

echo ""

# ---- 5. Installation Files ----
echo -e "${CYAN}Installed files (~/.dlt/):${NC}"

for file in dlt-daily-start.sh dlt-watchdog.sh dlt-stop.sh dlt-status.sh dlt-selftest.sh; do
    local_path="${HOME}/.dlt/${file}"
    if [ -f "${local_path}" ]; then
        if [ -x "${local_path}" ]; then
            check "${file} exists and executable" 0
        else
            check "${file} exists but NOT executable" 1
        fi
    else
        check "${file} exists" 1
    fi
done

if [ -d "${HOME}/.dlt/logs" ]; then
    check "Log directory exists" 0
else
    check "Log directory exists" 1
fi

echo ""

# ---- 6. Project Directory ----
echo -e "${CYAN}Project:${NC}"

if [ -d "${PROJECT_DIR}" ]; then
    check "Project directory: ${PROJECT_DIR}" 0
else
    check "Project directory exists: ${PROJECT_DIR}" 1
fi

if [ -f "${PROJECT_DIR}/Cargo.toml" ]; then
    check "Cargo.toml found" 0
else
    check "Cargo.toml found" 1
fi

if [ -f "${PROJECT_DIR}/deploy/docker/docker-compose.yml" ]; then
    check "docker-compose.yml found" 0
else
    check "docker-compose.yml found" 1
fi

if [ -f "${PROJECT_DIR}/scripts/notify-telegram.sh" ]; then
    check "notify-telegram.sh found" 0
else
    check "notify-telegram.sh found" 2
fi

echo ""

# ---- 7. AWS Credentials ----
echo -e "${CYAN}AWS:${NC}"

if aws sts get-caller-identity --region ap-south-1 >/dev/null 2>&1; then
    IDENTITY=$(aws sts get-caller-identity --region ap-south-1 --output text --query "Account" 2>/dev/null)
    check "AWS credentials valid (account: ${IDENTITY})" 0
else
    check "AWS credentials valid" 1
fi

# Test one SSM secret (non-sensitive: just check it exists)
SSM_ENV="${ENVIRONMENT:-dev}"
if aws ssm get-parameter --region ap-south-1 --name "/dlt/${SSM_ENV}/questdb/pg-user" --with-decryption --output text --query "Parameter.Value" --no-cli-pager >/dev/null 2>&1; then
    check "SSM secrets accessible (/dlt/${SSM_ENV}/)" 0
else
    check "SSM secrets accessible" 1
fi

echo ""

# ---- 8. Docker ----
echo -e "${CYAN}Docker:${NC}"

if docker info >/dev/null 2>&1; then
    check "Docker Desktop running" 0

    RUNNING=$(docker ps --filter "name=dlt-" --format '{{.Names}}' 2>/dev/null | wc -l | xargs)
    if [ "${RUNNING}" -ge 8 ]; then
        check "Infra containers: ${RUNNING}/8 running" 0
    elif [ "${RUNNING}" -gt 0 ]; then
        check "Infra containers: ${RUNNING}/8 running (some missing)" 2
    else
        check "Infra containers running (0/8)" 2
    fi

    if curl -sf --max-time 3 "http://localhost:9000" >/dev/null 2>&1; then
        check "QuestDB responding (port 9000)" 0
    else
        check "QuestDB responding" 2
    fi
else
    check "Docker Desktop running" 2
fi

echo ""

# ---- 9. IntelliJ ----
echo -e "${CYAN}IntelliJ:${NC}"

if [ -d "/Applications/IntelliJ IDEA.app" ]; then
    check "IntelliJ IDEA Ultimate found" 0
elif [ -d "/Applications/IntelliJ IDEA CE.app" ]; then
    check "IntelliJ IDEA Community found" 0
else
    check "IntelliJ IDEA found in /Applications" 1
fi

echo ""

# ---- 10. Cargo Check (skip with --quick) ----
if [ "${QUICK_MODE}" != "--quick" ]; then
    echo -e "${CYAN}Build verification:${NC}"

    if [ -d "${PROJECT_DIR}" ] && [ -f "${PROJECT_DIR}/Cargo.toml" ]; then
        echo -n "  Checking cargo build (this may take a moment)... "
        if (cd "${PROJECT_DIR}" && cargo check --workspace 2>/dev/null); then
            check "cargo check passes" 0
        else
            check "cargo check passes" 1
        fi
    else
        check "cargo check (project not found)" 1
    fi
    echo ""
fi

# ---- 11. Telegram ----
echo -e "${CYAN}Telegram:${NC}"

if [ -f "${PROJECT_DIR}/scripts/notify-telegram.sh" ]; then
    echo -n "  Sending test notification... "
    if bash "${PROJECT_DIR}/scripts/notify-telegram.sh" "[Self-Test] DLT auto-start validation passed at $(date '+%Y-%m-%d %H:%M IST')" 2>/dev/null; then
        check "Telegram notification sent" 0
    else
        check "Telegram notification sent" 2
    fi
else
    check "Telegram notification (script missing)" 2
fi

echo ""

# ---- Summary ----
TOTAL=$((PASS + FAIL + WARN))
echo -e "${CYAN}════════════════════════════════════════${NC}"
echo -e "  ${GREEN}Passed: ${PASS}${NC}  ${RED}Failed: ${FAIL}${NC}  ${YELLOW}Warnings: ${WARN}${NC}  Total: ${TOTAL}"
echo -e "${CYAN}════════════════════════════════════════${NC}"
echo ""

if [ "${FAIL}" -eq 0 ]; then
    echo -e "${GREEN}All critical checks passed. Auto-start is ready.${NC}"
    exit 0
else
    echo -e "${RED}${FAIL} critical check(s) failed. Fix before relying on auto-start.${NC}"
    exit 1
fi
