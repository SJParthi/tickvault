#!/usr/bin/env bash
# Auto-deploy on green CI for the active branch.
#
# Polls the GitHub combined-status API every N seconds. When a NEW commit
# arrives on the tracked branch AND every required check has succeeded,
# pulls + rebuilds + restarts the app.
#
# Designed for the Mac dev box BEFORE the AWS instance exists. On AWS this
# is replaced by a proper GitHub Actions deploy job.
#
# Install (one-time):
#   cp scripts/launchd/com.tickvault.autodeploy.plist ~/Library/LaunchAgents/
#   launchctl load ~/Library/LaunchAgents/com.tickvault.autodeploy.plist
#
# Uninstall:
#   launchctl unload ~/Library/LaunchAgents/com.tickvault.autodeploy.plist
#   rm ~/Library/LaunchAgents/com.tickvault.autodeploy.plist
#
# Logs: data/logs/auto-deploy.log
#
# Safety guards (intentional):
#   - Only deploys on the branch listed in DEPLOY_BRANCH below (default: main)
#   - Skips deploy during market hours (09:15-15:30 IST) so a build never
#     interrupts live trading
#   - Records last deployed SHA so the same commit isn't redeployed twice
#   - Exits non-zero if the working tree is dirty — never overwrites local work

set -euo pipefail

# ---- Config -----------------------------------------------------------------

DEPLOY_BRANCH="${TV_DEPLOY_BRANCH:-main}"
POLL_INTERVAL_SECS="${TV_DEPLOY_POLL_SECS:-60}"
REPO_DIR="${TV_REPO_DIR:-$HOME/IdeaProjects/tickvault}"
STATE_FILE="$REPO_DIR/data/cache/auto-deploy-last-sha"
LOG_FILE="$REPO_DIR/data/logs/auto-deploy.log"

# Market hours guard (IST). Auto-deploy SKIPS during this window so a
# rebuild never interrupts trading. SEC-1: Listed in `market-hours.md`.
MARKET_OPEN_HHMM="0915"
MARKET_CLOSE_HHMM="1530"

# ---- Helpers ----------------------------------------------------------------

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >> "$LOG_FILE"; }

is_market_hours_ist() {
    # Returns 0 (true) if current IST time is within market hours
    local hhmm
    hhmm=$(TZ='Asia/Kolkata' date +%H%M)
    [ "$hhmm" -ge "$MARKET_OPEN_HHMM" ] && [ "$hhmm" -lt "$MARKET_CLOSE_HHMM" ]
}

is_trading_day_ist() {
    # Returns 0 (true) if today is a weekday IST (Mon-Fri).
    # Holiday calendar is not consulted here — the app's own startup check
    # is the authoritative guard. This is just to avoid weekend churn.
    local dow
    dow=$(TZ='Asia/Kolkata' date +%u)  # 1=Mon, 7=Sun
    [ "$dow" -le 5 ]
}

git_remote_sha() {
    # Latest SHA on origin/$DEPLOY_BRANCH without fetching everything.
    git -C "$REPO_DIR" ls-remote --heads origin "$DEPLOY_BRANCH" \
        | awk '{print $1}'
}

ci_status() {
    # Returns "success" / "pending" / "failure" / "unknown" for the given SHA
    # via gh api (no PAT needed if `gh auth login` was done).
    local sha="$1"
    gh api "repos/SJParthi/tickvault/commits/$sha/status" \
        --jq '.state' 2>/dev/null || echo unknown
}

last_deployed_sha() {
    [ -f "$STATE_FILE" ] && cat "$STATE_FILE" || echo ""
}

mark_deployed() {
    mkdir -p "$(dirname "$STATE_FILE")"
    echo "$1" > "$STATE_FILE"
}

deploy() {
    local sha="$1"
    log "deploy starting: sha=$sha branch=$DEPLOY_BRANCH"

    cd "$REPO_DIR"

    # SAFETY: refuse if local changes exist
    if [ -n "$(git status --porcelain)" ]; then
        log "ABORT: working tree dirty — refusing to overwrite local changes"
        return 1
    fi

    # SAFETY: refuse if checked out on a different branch
    local current
    current=$(git rev-parse --abbrev-ref HEAD)
    if [ "$current" != "$DEPLOY_BRANCH" ]; then
        log "ABORT: HEAD is on '$current', not '$DEPLOY_BRANCH' — skip"
        return 1
    fi

    git pull --ff-only origin "$DEPLOY_BRANCH" >> "$LOG_FILE" 2>&1
    log "git pull OK"

    # Build before stopping the running app so a build failure leaves
    # the old binary in place and trading continues.
    cargo build --release >> "$LOG_FILE" 2>&1
    log "cargo build OK"

    make stop >> "$LOG_FILE" 2>&1 || true
    log "stop OK"

    # Background restart so launchd doesn't hold the script open.
    nohup make run >> "$LOG_FILE" 2>&1 &
    log "restart launched (PID $!)"

    mark_deployed "$sha"
    log "deploy complete: sha=$sha"
}

# ---- Main loop --------------------------------------------------------------

mkdir -p "$(dirname "$LOG_FILE")"
log "auto-deploy started: branch=$DEPLOY_BRANCH poll=${POLL_INTERVAL_SECS}s"

while true; do
    if is_trading_day_ist && is_market_hours_ist; then
        log "skip: inside market hours IST"
        sleep "$POLL_INTERVAL_SECS"
        continue
    fi

    remote_sha=$(git_remote_sha)
    if [ -z "$remote_sha" ]; then
        log "skip: could not read remote SHA"
        sleep "$POLL_INTERVAL_SECS"
        continue
    fi

    local_sha=$(last_deployed_sha)
    if [ "$remote_sha" = "$local_sha" ]; then
        sleep "$POLL_INTERVAL_SECS"
        continue
    fi

    status=$(ci_status "$remote_sha")
    if [ "$status" != "success" ]; then
        log "skip: CI status=$status for $remote_sha (waiting)"
        sleep "$POLL_INTERVAL_SECS"
        continue
    fi

    deploy "$remote_sha" || log "deploy failed — will retry on next change"
    sleep "$POLL_INTERVAL_SECS"
done
