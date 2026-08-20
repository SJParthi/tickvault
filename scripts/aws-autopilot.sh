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
# Exit code: 0 when everything is healthy or was auto-healed; 1 when any issue
# is left UNHEALED — on every trigger, scheduled runs included.
#
# (Until 2026-08-07 the non-zero exit applied only to --strict manual runs, so
# the scheduled lane always exited 0 and a green tick meant nothing more than
# "the script ran". `strict` is now retained only for backward compatibility
# with existing dispatch inputs; it no longer changes behaviour.)
# =============================================================================
set -uo pipefail

REGION="${AWS_REGION:-ap-south-1}"
# Single real env (operator 2026-06-30): default prod. NO real orders —
# production.toml locks dry_run=true.
ENVIRONMENT="${TV_ENVIRONMENT:-prod}"
STRICT="${STRICT:-no}"

# --- instance identity: the TAG is the authority, the secret is a hint -------
# 2026-08-10. This script used to take EC2_INSTANCE_ID as gospel and abort when
# it was empty. That made a stale-or-unset repo secret able to disable the one
# lane whose entire job is noticing that the repo secret is stale — a watchdog
# switched off by the fault it watches for. It also meant a human had to rotate
# a secret before ANY self-healing could resume.
#
# Now the live box is discovered from its Name tag on every run and used
# directly, so autopilot keeps working through a secret rotation that never
# happened. The secret is compared, and a mismatch is reported as an issue
# (below, once note_issue exists) instead of being fatal.
SECRET_INSTANCE_ID="${EC2_INSTANCE_ID:-}"
INSTANCE_ID=""
SECRET_DRIFT_MSG=""

SECRET_DRIFT_SEVERITY=""
RESOLVED_IDS=$(aws ec2 describe-instances --region "$REGION" \
  --filters "Name=tag:Name,Values=tv-${ENVIRONMENT}-app" \
            "Name=instance-state-name,Values=pending,running,stopping,stopped" \
  --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null)
RESOLVED_COUNT=$(printf '%s\n' $RESOLVED_IDS | grep -c . || true)

if [ "$RESOLVED_COUNT" -gt 1 ]; then
  # Refuse to guess which box to heal. Two instances carrying the prod Name tag
  # is itself the emergency; picking one and running SSM against it could
  # restart the wrong machine.
  echo "::error::$RESOLVED_COUNT instances carry tag Name=tv-${ENVIRONMENT}-app — refusing to guess which one to operate on. Found: $RESOLVED_IDS"
  exit 1
elif [ "$RESOLVED_COUNT" -eq 1 ]; then
  INSTANCE_ID=$(printf '%s\n' $RESOLVED_IDS | head -1)
  if [ -n "$SECRET_INSTANCE_ID" ] && [ "$SECRET_INSTANCE_ID" != "$INSTANCE_ID" ]; then
    SECRET_DRIFT_SEVERITY="cosmetic"
    SECRET_DRIFT_MSG="EC2_INSTANCE_ID secret is STALE — it names ${SECRET_INSTANCE_ID}, but the live \
tv-${ENVIRONMENT}-app instance is ${INSTANCE_ID}. Autopilot healed itself by using the live id, so this \
run is valid. Rotating the secret to ${INSTANCE_ID} (Settings > Secrets and \
variables > Actions) is OPTIONAL TIDY-UP only: as of 2026-08-20 every lane (autopilot, deploy, resize, aws-control) tag-resolves the live box, so a stale secret can no longer misdirect anything."
  fi
else
  # Tag lookup found nothing. Fall back to the secret rather than giving up —
  # a missing tag is a weaker signal than a configured id.
  INSTANCE_ID="$SECRET_INSTANCE_ID"
  if [ -z "$INSTANCE_ID" ]; then
    echo "::error::no instance carries tag Name=tv-${ENVIRONMENT}-app in $REGION and EC2_INSTANCE_ID is unset — nothing to check."
    exit 1
  fi
  SECRET_DRIFT_SEVERITY="issue"
  SECRET_DRIFT_MSG="no instance carries tag Name=tv-${ENVIRONMENT}-app in ${REGION} — falling back to the \
EC2_INSTANCE_ID secret (${INSTANCE_ID}). Either the box was terminated without being recreated, or its \
Name tag is wrong. Every tag-discovery lane (deploy, resize) is blind until this is corrected."
fi

ISSUES=()       # human-action-needed
HEALED=()       # auto-fixed this run
OK=()           # already healthy

note_ok()     { OK+=("$1");     echo "OK    : $1"; }
note_heal()   { HEALED+=("$1"); echo "HEAL  : $1"; }
note_issue()  { ISSUES+=("$1"); echo "ISSUE : $1"; }

# Report instance-identity drift now that the note_* helpers exist.
#
# 2026-08-20 — SEVERITY SPLIT. This used to be an unconditional `note_issue`,
# which fails the run AND pages Telegram. After the 2026-08-12 instance
# recreate left `EC2_INSTANCE_ID` naming the old box, that meant a page and a
# red run EVERY 15 MINUTES of every trading day — 30 of 30 recent runs failed
# on this one cosmetic line. Alert fatigue on a self-healed condition is the
# audit Rule 10 "edge-triggered only" violation, and it buried the REAL
# 2026-08-20 outage (app down all morning) in identical-looking noise.
#
# The two drift cases are NOT the same severity:
#   cosmetic — tag resolved fine, the secret merely disagrees. Every lane
#              tag-resolves the live box now, so nothing can be misdirected.
#              Informational: no page, no red run.
#   issue    — the Name tag resolved NOTHING and we fell back to the secret.
#              Tag discovery is blind, so deploy/resize really are at risk.
#              Still a hard issue: page and fail.
if [ -n "$SECRET_DRIFT_MSG" ]; then
  if [ "$SECRET_DRIFT_SEVERITY" = "issue" ]; then
    note_issue "$SECRET_DRIFT_MSG"
  else
    note_ok "$SECRET_DRIFT_MSG"
  fi
fi
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
  [ "$dow" -le 5 ] && [ "$t" -ge 830 ] && [ "$t" -le 1630 ]
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
    # NSE-holiday intentional stop (2026-07-07 round-3 review fix):
    # deploy/aws/holiday-gate.sh self-stops the box at boot on a weekday NSE
    # holiday and stamps today's IST date into this SSM param. Before this
    # check, autopilot's holiday-blind up-window self-start re-booted the
    # stopped box every 15 min all holiday — a boot/stop war whose 1-3 min
    # up-bursts can bracket the 09:20 IST market-hours alarm-gate sample and
    # restore the holiday false page. Marker == today => the stop is
    # intentional; leave the box alone. FAIL-OPEN: a missing/stale/unreadable
    # marker keeps the self-start (a real trading day never loses the heal).
    HOLIDAY_MARKER=$(aws ssm get-parameter --region "$REGION" \
      --name "/tickvault/${ENVIRONMENT}/holiday-stop-date" \
      --query 'Parameter.Value' --output text 2>/dev/null || echo "")
    if [ -n "$HOLIDAY_MARKER" ] && [ "$HOLIDAY_MARKER" = "$(TZ='Asia/Kolkata' date +%F)" ]; then
      note_ok "EC2 instance stopped (expected — NSE-holiday self-stop marker for today)"
    else
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
    fi
  else
    note_ok "EC2 instance stopped (expected — outside the 08:30-16:30 IST up-window)"
  fi
  # Nothing else to check on a stopped box.
  STATE="stopped"
elif [ "$STATE" = "None" ] || [ -z "$STATE" ]; then
  # 2026-08-10: `describe-instances --instance-ids <id>` on an id that does not
  # exist returns an EMPTY `Reservations[]`, and `--query ...State.Name` renders
  # that as the literal string "None". The old message ("state=None (not
  # running/stopped)") described the SYMPTOM and left the operator to guess the
  # cause, which is almost always one specific thing: the EC2_INSTANCE_ID
  # GitHub secret still names an instance that has been replaced.
  #
  # This is not hypothetical — it is the live state as of 2026-08-10. The box
  # was recreated as part of escaping the ap-south-1a capacity outage, and
  # daily-universe-scope-expansion-2026-05-27.md §7 Mechanical Rule 3 warns
  # explicitly: "the GitHub secret EC2_INSTANCE_ID must be rotated to the new
  # id at recreate time". It was not.
  #
  # Why this matters beyond autopilot: deploy-aws.yml reads the same secret in
  # 11 places to target SSM, so while it is stale NO DEPLOY CAN REACH THE BOX
  # and prod silently keeps running whatever binary it launched with. Autopilot
  # is the only lane that reports it, so it must report it in full.
  #
  # Resolve the live instance by its Name tag so the message can hand the
  # operator the exact value to paste. Best-effort: a lookup failure must not
  # replace the diagnosis with a shell error.
  LIVE_ID=$(aws ec2 describe-instances --region "$REGION" \
    --filters "Name=tag:Name,Values=tv-${ENVIRONMENT}-app" \
              "Name=instance-state-name,Values=running,stopped,stopping,starting" \
    --query 'Reservations[0].Instances[0].InstanceId' --output text 2>/dev/null || echo "")
  if [ -n "$LIVE_ID" ] && [ "$LIVE_ID" != "None" ]; then
    # ACTUALLY ADOPT THE LIVE ID (2026-08-11).
    #
    # Until today this branch resolved `LIVE_ID`, reported it, and then threw
    # it away — `INSTANCE_ID` kept the dead value and `STATE` stayed "None", so
    # the `if [ "$STATE" = "running" ]` gate below skipped EVERY remaining
    # check: EIP, SSM ping, app health, QuestDB, disk, feed. Autopilot became
    # blind on precisely the day it was most needed, while its Telegram text
    # told the operator it "healed itself by using the live id, so this run is
    # valid". That sentence was false — nothing downstream used the id. A
    # monitor that reports one problem while silently skipping eight checks is
    # worse than one that fails loudly (audit Rule 11, no false-OK).
    #
    # Adopting the id makes the claim true: the rest of this script now runs
    # against the real box. The secret is still stale and that is still
    # reported — but as a DEPLOY-lane problem, which is what it is.
    INSTANCE_ID="$LIVE_ID"
    STATE=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$INSTANCE_ID" \
      --query 'Reservations[0].Instances[0].State.Name' --output text 2>/dev/null || echo None)
    note_ok "adopted the live instance id ${LIVE_ID} by tag — this is now the NORMAL resolution path, not a heal; every check below \
ran against the real box, not the dead id in the secret"
    note_ok "EC2_INSTANCE_ID secret names an instance that no longer exists. \
The live tv-${ENVIRONMENT}-app instance is ${LIVE_ID}, adopted for this run. Rotating the secret \
to ${LIVE_ID} (Settings > Secrets and variables > Actions) is OPTIONAL: as of 2026-08-20 deploy-aws, \
resize, aws-control and autopilot all tag-resolve the live box, so a stale secret misdirects nothing."
    # Fall through to the running-box checks below with the healed id. If the
    # live box is stopped, `STATE` now says so honestly instead of "None".
  else
    note_issue "EC2 instance state=$STATE — the id in EC2_INSTANCE_ID does not exist, and no \
tv-${ENVIRONMENT}-app instance was found to suggest as its replacement. Check the region and \
whether the box was terminated without being recreated."
  fi
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
        # BP-09: a bare `systemctl restart` is a NO-OP on a StartLimit-`failed`
        # unit. tickvault.service sets StartLimitBurst=8 / StartLimitIntervalSec=600,
        # so after > 8 restarts in 10 min systemd flips the unit to `failed` and
        # `restart`/`start` returns "Start request repeated too quickly" and does
        # nothing until `reset-failed` clears the start-limit counter. A
        # Dhan-RST-driven crash-loop (see the 2026-06-30 feed-reset incident) trips
        # exactly this. So: ALWAYS `reset-failed` first (a harmless no-op on a
        # non-failed unit — it just clears any latched failed/limit state), THEN
        # `start`. This recovers BOTH a plain enabled-but-inactive unit AND a stuck
        # `failed` crash-loop with one path, without a human running reset-failed.
        #
        # HONEST WORDING (audit fix #2, 2026-07-03): autopilot only OBSERVES
        # "unit inactive/failed" — it CANNOT tell a crash-loop from an external
        # stop or an operator action (the 2026-07-02 mid-market wipe was
        # misread as a crash by the deploy monitor). The heal message states
        # the observation + what was done, never an assumed cause.
        echo "  app inactive ($APP), unit enabled — reset-failed + start via SSM"
        ssm_run "systemctl reset-failed tickvault 2>/dev/null; systemctl start tickvault 2>/dev/null; sleep 5" >/dev/null
        APP2=$(ssm_run "systemctl is-active tickvault 2>/dev/null || echo inactive" | tr -d '[:space:]')
        if [ "$APP2" = "active" ]; then
          note_heal "restarted app (tickvault) — unit was $APP (cause unknown to autopilot: could be a crash, an external stop, or an operator action); cleared any reset-failed state and started it"
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

  # 6. Required SSM secrets present? (operator populates /tickvault/prod/* manually)
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
    note_issue "missing SSM secrets: ${MISSING[*]} (populate /tickvault/${ENVIRONMENT}/* manually via aws ssm put-parameter)"
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

# Unhealed issues ALWAYS exit non-zero — scheduled runs included.
#
# This used to be gated on $STRICT (manual dispatch only), so the scheduled
# lane always exited 0 and every run showed a green checkmark in the Actions
# tab regardless of what it found. 30 of 30 recent runs reported success —
# including 2026-08-05, when the box never booted at all, and 2026-08-07,
# when it failed to start on InsufficientInstanceCapacity. The operator was
# reading green ticks over a dead box for a week.
#
# A green run must mean "the box is healthy", not "the script finished".
# This is audit Rule 11: no false-OK signals. The SNS page above still fires
# either way; this makes the RUN STATUS agree with it.
if [ "${#ISSUES[@]}" -gt 0 ]; then
  echo "aws-autopilot: ${#ISSUES[@]} unhealed issue(s) — failing the run so it is visible: ${ISSUES[*]}" >&2
  exit 1
fi
exit 0
