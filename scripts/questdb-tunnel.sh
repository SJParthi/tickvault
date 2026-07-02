#!/usr/bin/env bash
# =============================================================================
# questdb-tunnel.sh — one-command secure access to the PROD QuestDB console
# =============================================================================
# Opens an AWS SSM port-forwarding session from this machine to port 9000 on
# the tickvault prod box (QuestDB web console — localhost-only on the box by
# design, since the console is unauthenticated), waits until the local port is
# live, auto-opens the browser, and keeps the tunnel in the foreground until
# Ctrl-C.
#
# Usage:
#   scripts/questdb-tunnel.sh [--port N] [--no-open] [--wait-for-box MINUTES]
#   scripts/questdb-tunnel.sh --install-autostart    # macOS: auto-open weekdays 08:35 IST
#   scripts/questdb-tunnel.sh --uninstall-autostart
#   make questdb-prod
#
# No secrets, no hardcoded instance ids — the instance is resolved live from
# the tv-prod-app Name tag (see deploy/aws/terraform/main.tf).
# =============================================================================
set -euo pipefail

REGION="${AWS_DEFAULT_REGION:-ap-south-1}"
INSTANCE_NAME_TAG="tv-prod-app"
REMOTE_PORT=9000
DEFAULT_LOCAL_PORT=9000
FALLBACK_LOCAL_PORT=19000
TUNNEL_TIMEOUT_SECS=20
BOX_POLL_INTERVAL_SECS=20
OPEN_BROWSER=1
EXPLICIT_PORT=""
WAIT_FOR_BOX_MINS=0
DO_INSTALL_AUTOSTART=0
DO_UNINSTALL_AUTOSTART=0

LAUNCHD_LABEL="com.tickvault.questdb-tunnel"
LAUNCHD_PLIST="${HOME}/Library/LaunchAgents/${LAUNCHD_LABEL}.plist"
LAUNCHD_LOG="${HOME}/Library/Logs/tickvault-questdb-tunnel.log"

usage() {
    cat <<'EOF'
Usage: scripts/questdb-tunnel.sh [OPTIONS]

  --port N                Use local port N (default 9000; auto-falls back to
                          19000 only when the default is busy and no explicit
                          port was given)
  --no-open               Do not auto-open the browser
  --wait-for-box MINUTES  If the prod box is not running yet, poll every 20s
                          for up to MINUTES until it is, then proceed
  --install-autostart     macOS only: install a launchd agent that runs this
                          script weekdays at 08:35 (box auto-starts 08:30 IST)
                          with --wait-for-box 30, so the console opens itself
  --uninstall-autostart   Remove the launchd agent installed above
  -h, --help              Show this help
EOF
}

fail() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --port)
            [ "$#" -ge 2 ] || fail "--port requires a value"
            EXPLICIT_PORT="$2"
            case "$EXPLICIT_PORT" in
                ''|*[!0-9]*) fail "--port expects a number, got '${EXPLICIT_PORT}'" ;;
            esac
            shift 2
            ;;
        --no-open)
            OPEN_BROWSER=0
            shift
            ;;
        --wait-for-box)
            [ "$#" -ge 2 ] || fail "--wait-for-box requires a value (minutes)"
            WAIT_FOR_BOX_MINS="$2"
            case "$WAIT_FOR_BOX_MINS" in
                ''|*[!0-9]*) fail "--wait-for-box expects a number of minutes, got '${WAIT_FOR_BOX_MINS}'" ;;
            esac
            shift 2
            ;;
        --install-autostart)
            DO_INSTALL_AUTOSTART=1
            shift
            ;;
        --uninstall-autostart)
            DO_UNINSTALL_AUTOSTART=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            fail "unknown argument: $1"
            ;;
    esac
done

# ---- Autostart install / uninstall (macOS launchd) ---------------------------
require_darwin() {
    [ "$(uname -s)" = "Darwin" ] \
        || fail "$1 is macOS-only (it manages a launchd agent). On Linux, wire the script into cron or a systemd user timer instead."
}

install_autostart() {
    require_darwin "--install-autostart"
    local script_path uid
    script_path="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
    uid="$(id -u)"
    mkdir -p "${HOME}/Library/LaunchAgents" "${HOME}/Library/Logs"

    cat > "$LAUNCHD_PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/bash</string>
        <string>${script_path}</string>
        <string>--wait-for-box</string>
        <string>30</string>
    </array>
    <key>RunAtLoad</key>
    <false/>
    <key>StartCalendarInterval</key>
    <array>
        <dict><key>Weekday</key><integer>1</integer><key>Hour</key><integer>8</integer><key>Minute</key><integer>35</integer></dict>
        <dict><key>Weekday</key><integer>2</integer><key>Hour</key><integer>8</integer><key>Minute</key><integer>35</integer></dict>
        <dict><key>Weekday</key><integer>3</integer><key>Hour</key><integer>8</integer><key>Minute</key><integer>35</integer></dict>
        <dict><key>Weekday</key><integer>4</integer><key>Hour</key><integer>8</integer><key>Minute</key><integer>35</integer></dict>
        <dict><key>Weekday</key><integer>5</integer><key>Hour</key><integer>8</integer><key>Minute</key><integer>35</integer></dict>
    </array>
    <key>StandardOutPath</key>
    <string>${LAUNCHD_LOG}</string>
    <key>StandardErrorPath</key>
    <string>${LAUNCHD_LOG}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
</dict>
</plist>
EOF

    # Re-register cleanly if it was already loaded.
    launchctl bootout "gui/${uid}/${LAUNCHD_LABEL}" 2>/dev/null || true
    if launchctl bootstrap "gui/${uid}" "$LAUNCHD_PLIST" 2>/dev/null; then
        :
    else
        # Older macOS fallback.
        launchctl load "$LAUNCHD_PLIST" \
            || fail "could not register the launchd agent. Inspect ${LAUNCHD_PLIST} and try 'launchctl bootstrap gui/${uid} ${LAUNCHD_PLIST}' manually."
    fi

    printf 'Installed: %s\n' "$LAUNCHD_PLIST"
    printf 'The prod QuestDB console will auto-open weekdays at 08:35 (Mac local time = IST),\n'
    printf 'waiting up to 30 min for the box (it auto-starts 08:30 IST Mon-Fri).\n'
    printf 'Logs: %s\n' "$LAUNCHD_LOG"
    printf 'Remove with: scripts/questdb-tunnel.sh --uninstall-autostart\n'
}

uninstall_autostart() {
    require_darwin "--uninstall-autostart"
    local uid
    uid="$(id -u)"
    launchctl bootout "gui/${uid}/${LAUNCHD_LABEL}" 2>/dev/null \
        || launchctl unload "$LAUNCHD_PLIST" 2>/dev/null \
        || true
    rm -f "$LAUNCHD_PLIST"
    printf 'Removed the %s launchd agent (if it was installed).\n' "$LAUNCHD_LABEL"
}

if [ "$DO_INSTALL_AUTOSTART" -eq 1 ] && [ "$DO_UNINSTALL_AUTOSTART" -eq 1 ]; then
    fail "--install-autostart and --uninstall-autostart are mutually exclusive"
fi
if [ "$DO_INSTALL_AUTOSTART" -eq 1 ]; then
    install_autostart
    exit 0
fi
if [ "$DO_UNINSTALL_AUTOSTART" -eq 1 ]; then
    uninstall_autostart
    exit 0
fi

# ---- Preflight checks (actionable errors) -----------------------------------
command -v aws >/dev/null 2>&1 \
    || fail "'aws' CLI not found. Install it: https://docs.aws.amazon.com/cli/ (macOS: brew install awscli)"

aws sts get-caller-identity >/dev/null 2>&1 \
    || fail "AWS credentials are missing or expired. Run 'aws configure' (or refresh your SSO session: 'aws sso login') and retry."

command -v session-manager-plugin >/dev/null 2>&1 \
    || fail "'session-manager-plugin' not found — the SSM tunnel needs it. macOS: brew install --cask session-manager-plugin"

# ---- Resolve the prod instance dynamically (no hardcoded ids) ---------------
resolve_running_instance() {
    aws ec2 describe-instances \
        --region "$REGION" \
        --filters "Name=tag:Name,Values=${INSTANCE_NAME_TAG}" "Name=instance-state-name,Values=running" \
        --query 'Reservations[0].Instances[0].InstanceId' \
        --output text 2>/dev/null || true
}

printf 'Resolving %s instance in %s...\n' "$INSTANCE_NAME_TAG" "$REGION"
INSTANCE_ID="$(resolve_running_instance)"

if { [ -z "$INSTANCE_ID" ] || [ "$INSTANCE_ID" = "None" ]; } && [ "$WAIT_FOR_BOX_MINS" -gt 0 ]; then
    deadline=$(( $(date +%s) + WAIT_FOR_BOX_MINS * 60 ))
    printf 'Box is not running yet — polling every %ss for up to %s min...\n' \
        "$BOX_POLL_INTERVAL_SECS" "$WAIT_FOR_BOX_MINS"
    while [ "$(date +%s)" -lt "$deadline" ]; do
        sleep "$BOX_POLL_INTERVAL_SECS"
        INSTANCE_ID="$(resolve_running_instance)"
        if [ -n "$INSTANCE_ID" ] && [ "$INSTANCE_ID" != "None" ]; then
            break
        fi
        printf '  still waiting for %s to be running...\n' "$INSTANCE_NAME_TAG"
    done
fi

if [ -z "$INSTANCE_ID" ] || [ "$INSTANCE_ID" = "None" ]; then
    fail "no RUNNING instance tagged Name=${INSTANCE_NAME_TAG} in ${REGION}.
       The box is stopped (auto schedule: 08:30-16:30 IST Mon-Fri).
       Start it from the AWS console, or retry with --wait-for-box 30, or:
         aws ec2 start-instances --region ${REGION} --instance-ids \$(aws ec2 describe-instances --region ${REGION} --filters Name=tag:Name,Values=${INSTANCE_NAME_TAG} --query 'Reservations[0].Instances[0].InstanceId' --output text)"
fi
printf 'Instance: %s\n' "$INSTANCE_ID"

# ---- Pick the local port -----------------------------------------------------
port_busy() {
    # Connection accepted => something is already listening.
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null || return 1
    exec 3>&- 3<&-
    return 0
}

if [ -n "$EXPLICIT_PORT" ]; then
    LOCAL_PORT="$EXPLICIT_PORT"
    port_busy "$LOCAL_PORT" && fail "local port ${LOCAL_PORT} is already in use. Pick another with --port."
else
    LOCAL_PORT="$DEFAULT_LOCAL_PORT"
    if port_busy "$LOCAL_PORT"; then
        printf 'Local port %s is busy (local QuestDB running?) — falling back to %s.\n' \
            "$LOCAL_PORT" "$FALLBACK_LOCAL_PORT"
        LOCAL_PORT="$FALLBACK_LOCAL_PORT"
        port_busy "$LOCAL_PORT" && fail "fallback port ${FALLBACK_LOCAL_PORT} is also busy. Free a port or use --port N."
    fi
fi

# ---- Start the tunnel in the background, then wait for it -------------------
SSM_PID=""
cleanup() {
    trap - INT TERM EXIT
    if [ -n "$SSM_PID" ] && kill -0 "$SSM_PID" 2>/dev/null; then
        printf '\nClosing tunnel...\n'
        kill "$SSM_PID" 2>/dev/null || true
        wait "$SSM_PID" 2>/dev/null || true
    fi
}
trap cleanup INT TERM EXIT

printf 'Starting SSM port-forwarding session (remote %s -> localhost:%s)...\n' \
    "$REMOTE_PORT" "$LOCAL_PORT"
aws ssm start-session \
    --region "$REGION" \
    --target "$INSTANCE_ID" \
    --document-name AWS-StartPortForwardingSession \
    --parameters "{\"portNumber\":[\"${REMOTE_PORT}\"],\"localPortNumber\":[\"${LOCAL_PORT}\"]}" &
SSM_PID=$!

elapsed=0
until port_busy "$LOCAL_PORT"; do
    kill -0 "$SSM_PID" 2>/dev/null \
        || fail "the SSM session exited before the tunnel came up. Check the output above (SSM agent offline on the box? IAM ssm:StartSession denied?)."
    if [ "$elapsed" -ge "$TUNNEL_TIMEOUT_SECS" ]; then
        fail "tunnel did not become ready on localhost:${LOCAL_PORT} within ${TUNNEL_TIMEOUT_SECS}s. Check the SSM output above, then retry."
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done

URL="http://localhost:${LOCAL_PORT}"
printf '\nQuestDB prod console is live: %s\n' "$URL"

if [ "$OPEN_BROWSER" -eq 1 ]; then
    if command -v open >/dev/null 2>&1; then
        open "$URL"
    elif command -v xdg-open >/dev/null 2>&1; then
        xdg-open "$URL" >/dev/null 2>&1 || true
    else
        printf 'No browser opener found — open %s manually.\n' "$URL"
    fi
fi

printf 'Tunnel is running in the foreground. Ctrl-C to close the tunnel.\n'
wait "$SSM_PID"
