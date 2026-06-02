#!/usr/bin/env bash
# =============================================================================
# aws-autopilot.sh — self-check + auto-heal the tickvault AWS deployment.
# =============================================================================
# Runs from GitHub Actions (aws-autopilot.yml) with AWS creds already
# configured. Checks every layer of the running deployment and AUTO-FIXES what
# it safely can, alerting (Telegram via SNS) only when human eyes are needed.
#
# Design rules:
#   - Idempotent: safe to run every 15 min. Each check is independent.
#   - Conservative healing: only actions that cannot lose data.
#   - Market-hours aware: a STOPPED box outside the trading window is EXPECTED
#     (EventBridge stop schedule) — not an error. We do NOT fight that schedule.
#   - Never silent: every run posts a one-line Telegram summary.
#
# Exit code is always 0 (a failed check is reported, not a workflow failure) —
# EXCEPT --strict mode (manual dispatch) where unhealed issues exit 1.
# =============================================================================
set -uo pipefail

REGION="${AWS_REGION:-ap-south-1}"
INSTANCE_ID="${EC2_INSTANCE_ID:-}"
ENVIRONMENT="${TV_ENVIRONMENT:-staging}"
STRICT="${STRICT:-no}"

if [ -z "$INSTANCE_ID" ]; then
  echo "::error::EC2_INSTANCE_ID not set — cannot run autopilot."
  exit 1
fi

ISSUES=()       # human-action-needed
HEALED=()       # auto-fixed this run
OK=()           # already healthy

note_ok()     { OK+=("$1");     echo "OK    : $1"; }
note_heal()   { HEALED+=("$1"); echo "HEAL  : $1"; }
note_issue()  { ISSUES+=("$1"); echo "ISSUE : $1"; }

# --- helper: run a shell command ON the box via SSM, capture stdout ----------
ssm_run() {
  local cmd="$1" cid out status
  cid=$(aws ssm send-command \
    --region "$REGION" --instance-ids "$INSTANCE_ID" \
    --document-name "AWS-RunShellScript" \
    --parameters "commands=[\"$cmd\"]" \
    --query 'Command.CommandId' --output text 2>/dev/null) || return 1
  # wait (bounded)
  for _ in $(seq 1 20); do
    status=$(aws ssm get-command-invocation --region "$REGION" \
      --command-id "$cid" --instance-id "$INSTANCE_ID" \
      --query 'Status' --output text 2>/dev/null || echo Pending)
    [ "$status" = "InProgress" ] || [ "$status" = "Pending" ] || break
    sleep 3
  done
  aws ssm get-command-invocation --region "$REGION" \
    --command-id "$cid" --instance-id "$INSTANCE_ID" \
    --query 'StandardOutputContent' --output text 2>/dev/null
}

# --- market-hours (IST) helper: is now within 09:00-15:35 IST Mon-Fri? -------
is_market_window() {
  local h m dow t
  h=$(TZ='Asia/Kolkata' date +%H); m=$(TZ='Asia/Kolkata' date +%M)
  dow=$(TZ='Asia/Kolkata' date +%u)   # 1=Mon..7=Sun
  t=$((10#$h * 100 + 10#$m))
  [ "$dow" -le 5 ] && [ "$t" -ge 900 ] && [ "$t" -le 1535 ]
}

# --- up-window (IST) helper: is now within 08:00-17:00 IST Mon-Fri? ----------
# This is the EventBridge START/STOP schedule window — the box is SUPPOSED to
# be running for the WHOLE of it, not just market hours (09:15-15:30). The
# pre-market gap (08:30-09:00) is when the daily universe is fetched + the WS
# subscribes, so a stopped box there is a real problem, not "expected". Using
# is_market_window() for the self-start left the 08:30-09:00 window uncovered:
# if the EventBridge 08:30 start silently failed, autopilot waited until 09:00
# to rescue it (2026-06-02 incident). is_box_up_window() closes that gap.
is_box_up_window() {
  local h m dow t
  h=$(TZ='Asia/Kolkata' date +%H); m=$(TZ='Asia/Kolkata' date +%M)
  dow=$(TZ='Asia/Kolkata' date +%u)   # 1=Mon..7=Sun
  t=$((10#$h * 100 + 10#$m))
  [ "$dow" -le 5 ] && [ "$t" -ge 800 ] && [ "$t" -le 1700 ]
}

# --- diagnostic: is the EventBridge daily-start rule healthy? ----------------
# Called when the box is found stopped DURING the up-window — i.e. the 08:30
# EventBridge start should have run but didn't. Surfaces WHICH layer failed so
# the operator (or a future Claude session) doesn't have to guess. Infra
# resources are named with the terraform var.environment = "prod" (the SNS
# topic above is tv-prod-alerts), so the rule is tv-prod-daily-start.
diagnose_eventbridge_start() {
  local rule="tv-prod-daily-start" state today execs
  state=$(aws events describe-rule --region "$REGION" --name "$rule" \
    --query 'State' --output text 2>/dev/null || echo MISSING)
  if [ "$state" = "MISSING" ]; then
    note_issue "EventBridge rule '$rule' is MISSING — run terraform-apply (the 08:30 auto-start does not exist)"
    return
  fi
  if [ "$state" != "ENABLED" ]; then
    note_issue "EventBridge rule '$rule' state=$state (not ENABLED) — re-enable it; the box will not auto-start until then"
    return
  fi
  # Rule exists + enabled, but the box is still stopped in the up-window =>
  # the rule fired but the SSM AWS-StartEC2Instance automation failed (IAM /
  # PassRole / capacity). Surface the most recent start-automation outcome.
  today=$(TZ='Asia/Kolkata' date +%F)
  execs=$(aws ssm describe-automation-executions --region "$REGION" \
    --filters "Key=DocumentNamePrefix,Values=AWS-StartEC2Instance" \
    --query 'AutomationExecutionMetadataList[0].[AutomationExecutionStatus,ExecutionStartTime]' \
    --output text 2>/dev/null || echo "none")
  note_issue "EventBridge rule '$rule' ENABLED but box stopped in up-window — start-automation likely failed (last AWS-StartEC2Instance: ${execs}). Autopilot is starting the box now; check the rule target IAM (PassRole) if this repeats daily."
}

echo "===== tickvault AWS autopilot — $(date -u +%FT%TZ) ====="
echo "instance=$INSTANCE_ID region=$REGION env=$ENVIRONMENT strict=$STRICT"

# ---------------------------------------------------------------------------
# 1. Instance state
# ---------------------------------------------------------------------------
STATE=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$INSTANCE_ID" \
  --query 'Reservations[0].Instances[0].State.Name' --output text 2>/dev/null || echo unknown)
if [ "$STATE" = "running" ]; then
  note_ok "EC2 instance running"
elif [ "$STATE" = "stopped" ]; then
  if is_box_up_window; then
    # The box should be RUNNING for the whole 08:30-16:30 IST window. Stopped
    # here = the 08:30 EventBridge start failed. Diagnose WHICH layer broke,
    # then self-start (the diagnostic only reports; it never starts the box).
    echo "  instance stopped DURING up-window (08:30-16:30 IST) — diagnosing + starting it"
    diagnose_eventbridge_start
    if aws ec2 start-instances --region "$REGION" --instance-ids "$INSTANCE_ID" >/dev/null 2>&1; then
      note_heal "started EC2 instance (was stopped during the 08:30-16:30 IST up-window)"
    else
      note_issue "EC2 instance stopped during up-window and start-instances failed"
    fi
  else
    note_ok "EC2 instance stopped (expected — outside the 08:30-16:30 IST up-window)"
  fi
  # Nothing else to check on a stopped box.
  STATE="stopped"
else
  note_issue "EC2 instance state=$STATE (not running/stopped)"
fi

# ---------------------------------------------------------------------------
# The remaining checks only make sense on a RUNNING box.
# ---------------------------------------------------------------------------
if [ "$STATE" = "running" ]; then

  # 2. Elastic IP associated?
  EIP=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$INSTANCE_ID" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' --output text 2>/dev/null || echo None)
  if [ -n "$EIP" ] && [ "$EIP" != "None" ]; then
    note_ok "public IP present ($EIP)"
  else
    # Try to find an allocated-but-unassociated tv EIP and associate it.
    ALLOC=$(aws ec2 describe-addresses --region "$REGION" \
      --filters "Name=tag:Name,Values=tv-${ENVIRONMENT}-eip,tv-prod-eip" \
      --query 'Addresses[?AssociationId==null]|[0].AllocationId' --output text 2>/dev/null || echo None)
    if [ -n "$ALLOC" ] && [ "$ALLOC" != "None" ]; then
      if aws ec2 associate-address --region "$REGION" \
        --instance-id "$INSTANCE_ID" --allocation-id "$ALLOC" >/dev/null 2>&1; then
        note_heal "re-associated Elastic IP ($ALLOC)"
      else
        note_issue "no public IP and EIP re-association failed"
      fi
    else
      note_issue "no public IP and no free tv EIP to associate (run terraform-apply)"
    fi
  fi

  # 3. SSM managed node online?
  PING=$(aws ssm describe-instance-information --region "$REGION" \
    --filters "Key=InstanceIds,Values=$INSTANCE_ID" \
    --query 'InstanceInformationList[0].PingStatus' --output text 2>/dev/null || echo None)
  if [ "$PING" = "Online" ]; then
    note_ok "SSM managed node Online"
  else
    note_issue "SSM managed node not Online (ping=$PING) — check instance role + internet path"
  fi

  # Below here we need SSM to reach the box. Skip if not Online.
  if [ "$PING" = "Online" ]; then

    # 4. App systemd unit active?
    APP=$(ssm_run "systemctl is-active tickvault 2>/dev/null || echo inactive" | tr -d '[:space:]')
    if [ "$APP" = "active" ]; then
      note_ok "app (tickvault) active"
    else
      # Respect the operator kill-switch. `stop-app` (AWS Control) runs
      # `systemctl disable tickvault`, so a DISABLED unit means the app was
      # stopped ON PURPOSE — e.g. to let a Dhan feed rate-limit (HTTP 429 /
      # DATA-805) cool down. Auto-restarting it would re-open the feed
      # connection and keep the 429 alive forever (every restart = a fresh
      # connect attempt). So: NEVER auto-restart a disabled unit — surface it
      # as an intentional stop. Only an ENABLED-but-inactive unit (a genuine
      # crash) gets the auto-restart.
      ENABLED=$(ssm_run "systemctl is-enabled tickvault 2>/dev/null || echo disabled" | tr -d '[:space:]')
      if [ "$ENABLED" = "disabled" ]; then
        note_ok "app (tickvault) intentionally stopped (kill-switch — unit disabled); not auto-restarting (re-enable via AWS Control deploy/start when ready)"
      else
        echo "  app inactive ($APP), unit enabled — restarting via SSM"
        ssm_run "systemctl restart tickvault 2>/dev/null; sleep 5" >/dev/null
        APP2=$(ssm_run "systemctl is-active tickvault 2>/dev/null || echo inactive" | tr -d '[:space:]')
        if [ "$APP2" = "active" ]; then
          note_heal "restarted app (tickvault) — was $APP"
        else
          note_issue "app (tickvault) not active after restart ($APP2) — check journalctl"
        fi
      fi
    fi

    # 5. QuestDB container up?
    QDB=$(ssm_run "docker ps --filter name=tv-questdb --filter status=running -q 2>/dev/null | head -c1" | tr -d '[:space:]')
    if [ -n "$QDB" ]; then
      note_ok "QuestDB container running"
    else
      echo "  QuestDB not running — docker compose up via SSM"
      ssm_run "cd /opt/tickvault && docker compose -f deploy/docker/docker-compose.yml up -d tv-questdb 2>/dev/null; sleep 5" >/dev/null
      QDB2=$(ssm_run "docker ps --filter name=tv-questdb --filter status=running -q 2>/dev/null | head -c1" | tr -d '[:space:]')
      if [ -n "$QDB2" ]; then
        note_heal "started QuestDB container"
      else
        note_issue "QuestDB container not running after docker compose up"
      fi
    fi
  fi

  # 6. Required SSM secrets present? (re-seed from dev if any staging key missing)
  NEED=("dhan/client-id" "dhan/totp-secret" "questdb/pg-password" "telegram/bot-token" "api/bearer-token")
  MISSING=()
  for k in "${NEED[@]}"; do
    if ! aws ssm get-parameter --region "$REGION" \
      --name "/tickvault/${ENVIRONMENT}/${k}" >/dev/null 2>&1; then
      MISSING+=("$k")
    fi
  done
  if [ "${#MISSING[@]}" -eq 0 ]; then
    note_ok "all required SSM secrets present (/tickvault/${ENVIRONMENT}/*)"
  else
    note_issue "missing SSM secrets: ${MISSING[*]} (run seed-staging-ssm workflow)"
  fi
fi

# ---------------------------------------------------------------------------
# Summary + Telegram (via SNS) — always.
# ---------------------------------------------------------------------------
echo ""
echo "===== SUMMARY ====="
printf 'OK     : %s\n' "${#OK[@]}"
printf 'HEALED : %s\n' "${#HEALED[@]}"
printf 'ISSUES : %s\n' "${#ISSUES[@]}"

{
  echo "## AWS autopilot — $(TZ='Asia/Kolkata' date '+%d %b %Y %I:%M %p IST')"
  echo ""
  echo "- ✅ healthy: ${#OK[@]}"
  echo "- 🔧 auto-fixed: ${#HEALED[@]}"
  echo "- 🆘 needs attention: ${#ISSUES[@]}"
  [ "${#HEALED[@]}" -gt 0 ] && { echo ""; echo "**Auto-fixed:**"; printf '  - %s\n' "${HEALED[@]}"; }
  [ "${#ISSUES[@]}" -gt 0 ] && { echo ""; echo "**Needs attention:**"; printf '  - %s\n' "${ISSUES[@]}"; }
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"

# Telegram via SNS — only fire if something was healed or is broken (avoid spam
# on all-green runs). Account id derived at runtime (no AWS_ACCOUNT_ID secret).
if [ "${#HEALED[@]}" -gt 0 ] || [ "${#ISSUES[@]}" -gt 0 ]; then
  ACCT=$(aws sts get-caller-identity --query Account --output text 2>/dev/null || echo "")
  if [ -n "$ACCT" ]; then
    EMOJI="🔧"; [ "${#ISSUES[@]}" -gt 0 ] && EMOJI="🆘"
    MSG="$EMOJI AWS autopilot: ${#HEALED[@]} auto-fixed, ${#ISSUES[@]} need attention."
    [ "${#HEALED[@]}" -gt 0 ] && MSG="$MSG Fixed: ${HEALED[*]}."
    [ "${#ISSUES[@]}" -gt 0 ] && MSG="$MSG ACTION: ${ISSUES[*]}."
    aws sns publish --region "$REGION" \
      --topic-arn "arn:aws:sns:${REGION}:${ACCT}:tv-prod-alerts" \
      --subject "AWS autopilot" --message "$MSG" >/dev/null 2>&1 || true
  fi
fi

# Strict mode (manual dispatch): non-zero exit if unhealed issues remain.
if [ "$STRICT" = "yes" ] && [ "${#ISSUES[@]}" -gt 0 ]; then
  exit 1
fi
exit 0
