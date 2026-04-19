#!/usr/bin/env bash
# claude-session-bootstrap.sh — auto-resolve tickvault runtime endpoints for
# every Claude Code session (Mac local, AWS remote, or claude.ai sandbox).
#
# Outputs:
#   .claude/.session-env      Shell-sourceable export block for the MCP server
#                             and any tool that wants TICKVAULT_*_URL env vars.
#
# Contract:
#   - Reads the active profile from config/claude-mcp-endpoints.toml.
#   - Probes each endpoint with a 2s request and records REACHABLE/OFFLINE/UNSET.
#   - PR #288 (#10): profile auto-switch. If the configured profile has fewer
#     than AUTO_SWITCH_MIN_REACHABLE endpoints reachable AND another profile
#     has more reachable endpoints, transparently use that profile. The
#     TOML's `active = ...` is respected first — we only auto-switch when it
#     is demonstrably not reachable. An operator override
#     (`TICKVAULT_MCP_PROFILE=<name>`) always wins and disables auto-switch.
#   - Never fails the session — a dead profile just means MCP tools return
#     "not reachable" instead of breaking the shell.
#   - Idempotent. Safe to run on every SessionStart and PreCompact.

set -uo pipefail

CWD="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$CWD" || exit 0

CONFIG="$CWD/config/claude-mcp-endpoints.toml"
OUT="$CWD/.claude/.session-env"

# PR #288 (#10) — auto-switch threshold. A profile is considered "workable"
# if at least this many of the 5 probed endpoints are REACHABLE. If the
# configured profile falls below this AND a different profile meets it, we
# auto-switch. 3-of-5 is chosen because QuestDB + Prometheus + app-api is the
# minimum useful set for the MCP log/metric tools.
AUTO_SWITCH_MIN_REACHABLE=3

if [ ! -f "$CONFIG" ]; then
    echo "bootstrap: $CONFIG missing — nothing to export" >&2
    exit 0
fi

# Operator override disables auto-switch entirely — treat it as authoritative.
OVERRIDE_PROFILE="${TICKVAULT_MCP_PROFILE:-}"
# Reject the literal `${...}` shell placeholder that Claude Code's MCP
# launcher sometimes passes through (same bug class as the server.py fix).
case "$OVERRIDE_PROFILE" in
    '${'*'}') OVERRIDE_PROFILE="" ;;
esac

CONFIGURED_PROFILE=$(awk -F'"' '/^active[[:space:]]*=/ { print $2; exit }' "$CONFIG")
if [ -z "$CONFIGURED_PROFILE" ]; then
    CONFIGURED_PROFILE="local"
fi

if [ -n "$OVERRIDE_PROFILE" ]; then
    PROFILE="$OVERRIDE_PROFILE"
    AUTO_SWITCH_ENABLED=0
else
    PROFILE="$CONFIGURED_PROFILE"
    AUTO_SWITCH_ENABLED=1
fi

read_profile_key() {
    local profile="$1"
    local key="$2"
    awk -v section="[profiles.$profile]" -v key="$key" '
        $0 == section { in_section = 1; next }
        /^\[/ { in_section = 0 }
        in_section && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
            gsub(/^[^=]*=[[:space:]]*"/, "")
            gsub(/"[[:space:]]*$/, "")
            print
            exit
        }
    ' "$CONFIG"
}

probe() {
    local url="$1"
    [ -z "$url" ] && { echo UNSET; return; }
    if curl -fsS -m 2 -o /dev/null "$url" 2>/dev/null; then
        echo REACHABLE
    else
        echo OFFLINE
    fi
}

# Probe a profile and echo `<reachable_count> <prom_s> <alert_s> <qdb_s> <graf_s> <api_s>`.
# Side effects: sets global vars PROM, ALERT, QDB, GRAF, API, LOGS_SRC, LOGS_DIR
# to the profile's configured values (caller consumes them).
score_profile() {
    local profile="$1"
    PROM=$(read_profile_key "$profile" prometheus_url)
    ALERT=$(read_profile_key "$profile" alertmanager_url)
    QDB=$(read_profile_key "$profile" questdb_url)
    GRAF=$(read_profile_key "$profile" grafana_url)
    API=$(read_profile_key "$profile" tickvault_api_url)
    LOGS_SRC=$(read_profile_key "$profile" logs_source)
    LOGS_DIR=$(read_profile_key "$profile" logs_dir_local)
    PROM_S=$(probe "$PROM")
    ALERT_S=$(probe "$ALERT")
    QDB_S=$(probe "$QDB/status" 2>/dev/null || echo OFFLINE)
    GRAF_S=$(probe "$GRAF/api/health" 2>/dev/null || echo OFFLINE)
    API_S=$(probe "$API/health" 2>/dev/null || echo OFFLINE)
    REACHABLE_COUNT=0
    for s in "$PROM_S" "$ALERT_S" "$QDB_S" "$GRAF_S" "$API_S"; do
        [ "$s" = "REACHABLE" ] && REACHABLE_COUNT=$((REACHABLE_COUNT + 1))
    done
}

# Probe the configured (or override) profile first.
score_profile "$PROFILE"

AUTO_SWITCHED_FROM=""
# PR #288 (#10): auto-switch if configured profile is below threshold.
if [ "$AUTO_SWITCH_ENABLED" = "1" ] && [ "$REACHABLE_COUNT" -lt "$AUTO_SWITCH_MIN_REACHABLE" ]; then
    CANDIDATE_PROFILES=$(awk '/^\[profiles\.[^]]+\]/ { gsub(/\[profiles\.|\]/, ""); print }' "$CONFIG")
    BEST_PROFILE="$PROFILE"
    BEST_COUNT="$REACHABLE_COUNT"
    # Save current winning snapshot; only replace when a better candidate wins.
    BEST_PROM="$PROM" BEST_ALERT="$ALERT" BEST_QDB="$QDB" BEST_GRAF="$GRAF" BEST_API="$API"
    BEST_LOGS_SRC="$LOGS_SRC" BEST_LOGS_DIR="$LOGS_DIR"
    BEST_PROM_S="$PROM_S" BEST_ALERT_S="$ALERT_S" BEST_QDB_S="$QDB_S" BEST_GRAF_S="$GRAF_S" BEST_API_S="$API_S"
    for candidate in $CANDIDATE_PROFILES; do
        [ "$candidate" = "$PROFILE" ] && continue
        score_profile "$candidate"
        if [ "$REACHABLE_COUNT" -gt "$BEST_COUNT" ]; then
            BEST_PROFILE="$candidate"
            BEST_COUNT="$REACHABLE_COUNT"
            BEST_PROM="$PROM" BEST_ALERT="$ALERT" BEST_QDB="$QDB" BEST_GRAF="$GRAF" BEST_API="$API"
            BEST_LOGS_SRC="$LOGS_SRC" BEST_LOGS_DIR="$LOGS_DIR"
            BEST_PROM_S="$PROM_S" BEST_ALERT_S="$ALERT_S" BEST_QDB_S="$QDB_S" BEST_GRAF_S="$GRAF_S" BEST_API_S="$API_S"
        fi
    done
    if [ "$BEST_PROFILE" != "$PROFILE" ]; then
        AUTO_SWITCHED_FROM="$PROFILE"
        PROFILE="$BEST_PROFILE"
        PROM="$BEST_PROM" ALERT="$BEST_ALERT" QDB="$BEST_QDB" GRAF="$BEST_GRAF" API="$BEST_API"
        LOGS_SRC="$BEST_LOGS_SRC" LOGS_DIR="$BEST_LOGS_DIR"
        PROM_S="$BEST_PROM_S" ALERT_S="$BEST_ALERT_S" QDB_S="$BEST_QDB_S" GRAF_S="$BEST_GRAF_S" API_S="$BEST_API_S"
        REACHABLE_COUNT="$BEST_COUNT"
    else
        # No better candidate — restore the original profile's values so the
        # written .session-env still matches the configured profile.
        score_profile "$PROFILE"
    fi
fi

cat > "$OUT" <<EOF
# auto-generated by scripts/claude-session-bootstrap.sh
# profile: $PROFILE       generated: $(date -u +'%Y-%m-%dT%H:%M:%SZ')
# configured: $CONFIGURED_PROFILE    auto_switch: $AUTO_SWITCH_ENABLED
# switched_from: ${AUTO_SWITCHED_FROM:-none}    reachable: $REACHABLE_COUNT/5
export TICKVAULT_MCP_PROFILE="$PROFILE"
export TICKVAULT_PROMETHEUS_URL="$PROM"
export TICKVAULT_ALERTMANAGER_URL="$ALERT"
export TICKVAULT_QUESTDB_URL="$QDB"
export TICKVAULT_GRAFANA_URL="$GRAF"
export TICKVAULT_API_URL="$API"
export TICKVAULT_LOGS_DIR="$LOGS_DIR"
export TICKVAULT_LOGS_SOURCE="$LOGS_SRC"
# probe snapshot (REACHABLE|OFFLINE|UNSET) — consumed by session-sanity.sh
export TICKVAULT_PROM_STATUS="$PROM_S"
export TICKVAULT_ALERT_STATUS="$ALERT_S"
export TICKVAULT_QDB_STATUS="$QDB_S"
export TICKVAULT_GRAF_STATUS="$GRAF_S"
export TICKVAULT_API_STATUS="$API_S"
export TICKVAULT_AUTO_SWITCHED_FROM="${AUTO_SWITCHED_FROM:-}"
EOF

if [ -n "$AUTO_SWITCHED_FROM" ]; then
    echo "bootstrap: profile=$PROFILE (auto-switched from $AUTO_SWITCHED_FROM, reachable=$REACHABLE_COUNT/5)" >&2
else
    echo "bootstrap: profile=$PROFILE  prom=$PROM_S  qdb=$QDB_S  graf=$GRAF_S  api=$API_S" >&2
fi
exit 0
