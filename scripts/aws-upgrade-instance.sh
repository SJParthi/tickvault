#!/usr/bin/env bash
# aws-upgrade-instance.sh — in-place t4g.medium → m8g.large flip.
#
# Implements §7 Mechanical Rule 7 of
# `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`:
# the existing prod EC2 instance is upgraded IN PLACE (same instance-id,
# same Elastic IP, same EBS volumes) — NOT replaced. The Dhan static-IP
# whitelist (EIP, 7-day modify cooldown) and the QuestDB data volume are
# preserved across the upgrade.
#
# Flow (per the rule):
#   1. Discover the instance by Name tag `tv-${ENV}-app` (default env=prod).
#   2. Refuse unless it is currently t4g.medium (idempotent: if already
#      m8g.large, report and exit 0).
#   3. Market-hours guard — refuse 09:00–15:30 IST Mon–Fri unless --force
#      (upgrade requires a stop; never stop during a live session).
#   4. `aws ec2 stop-instances` → wait `instance-stopped`.
#   5. `aws ec2 modify-instance-attribute --instance-type m8g.large`.
#   6. `aws ec2 start-instances` → wait `instance-running`.
#   7. Verify the new type + that the EIP is still associated.
#
# Downtime ≈ 3 minutes. Run from the operator's Mac (uses ~/.aws creds).
# Scheduled window per rule: Sunday 22:00 IST (off-market).
#
# Usage:
#   ./scripts/aws-upgrade-instance.sh            # safe, guarded
#   ./scripts/aws-upgrade-instance.sh --dry-run  # print actions, change nothing
#   ./scripts/aws-upgrade-instance.sh --force    # bypass market-hours guard
#
# Idempotent: re-running after success is a no-op (already m8g.large).

set -euo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
ENV="${TV_ENV:-prod}"
REGION="${AWS_REGION:-ap-south-1}"          # ap-south-1 Mumbai per aws-budget.md
FROM_TYPE="t4g.medium"
TO_TYPE="m8g.large"                          # operator lock 2026-05-29 §7 Quote 5
NAME_TAG="tv-${ENV}-app"

DRY_RUN=0
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --force)   FORCE=1 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

log() { printf '\n\033[1;36m▶ %s\033[0m\n' "$*"; }
ok()  { printf '\033[1;32m  ✓ %s\033[0m\n' "$*"; }
die() { printf '\033[1;31m  ✗ %s\033[0m\n' "$*" >&2; exit 1; }
run() { if [ "$DRY_RUN" -eq 1 ]; then echo "    [dry-run] $*"; else eval "$*"; fi; }

command -v aws >/dev/null || die "aws CLI not found — install + 'aws configure' first."

# ---------------------------------------------------------------------------
# Market-hours guard (IST). Upgrade requires a stop — never during a session.
# ---------------------------------------------------------------------------
log "Market-hours guard"
NOW_UTC_MIN=$(date -u +%H%M)
# IST = UTC + 5:30. Compute IST minutes-of-day.
utc_h=$(date -u +%H); utc_m=$(date -u +%M)
ist_total=$(( (10#$utc_h * 60 + 10#$utc_m + 330) % 1440 ))
dow=$(date -u +%u)   # 1=Mon .. 7=Sun (UTC dow ~ IST dow at these hours)
# Market window 09:00–15:30 IST = 540–930 minutes, Mon–Fri.
if [ "$dow" -le 5 ] && [ "$ist_total" -ge 540 ] && [ "$ist_total" -le 930 ]; then
  if [ "$FORCE" -eq 1 ]; then
    printf '  ⚠ within market hours but --force given; proceeding.\n'
  else
    die "Refusing: within 09:00–15:30 IST Mon–Fri (a stop would interrupt the live session). Re-run on a Sunday/off-market window, or pass --force."
  fi
else
  ok "Outside market hours (IST minute-of-day=${ist_total}, dow=${dow}) — safe to stop."
fi

# ---------------------------------------------------------------------------
# Discover the instance
# ---------------------------------------------------------------------------
log "Discovering instance Name=${NAME_TAG} in ${REGION}"
IID=$(aws ec2 describe-instances --region "$REGION" \
  --filters "Name=tag:Name,Values=${NAME_TAG}" \
            "Name=instance-state-name,Values=pending,running,stopping,stopped" \
  --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null || true)
[ -n "$IID" ] && [ "$IID" != "None" ] || die "No instance found with Name=${NAME_TAG}. Is the stack deployed? Check region."
# Guard against more than one match (split on whitespace).
if [ "$(printf '%s' "$IID" | wc -w | tr -d ' ')" != "1" ]; then
  die "Multiple instances matched Name=${NAME_TAG}: ${IID}. Refusing to act ambiguously."
fi
ok "Instance: ${IID}"

CUR_TYPE=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$IID" \
  --query 'Reservations[0].Instances[0].InstanceType' --output text)
ok "Current type: ${CUR_TYPE}"

if [ "$CUR_TYPE" = "$TO_TYPE" ]; then
  ok "Already ${TO_TYPE} — nothing to do (idempotent)."
  exit 0
fi
[ "$CUR_TYPE" = "$FROM_TYPE" ] || die "Expected ${FROM_TYPE} but found ${CUR_TYPE}. Refusing — investigate before forcing an upgrade."

# Record the associated EIP so we can verify it survives.
EIP_BEFORE=$(aws ec2 describe-addresses --region "$REGION" \
  --filters "Name=instance-id,Values=${IID}" \
  --query 'Addresses[0].PublicIp' --output text 2>/dev/null || echo "None")
ok "Elastic IP before: ${EIP_BEFORE}"
[ "$EIP_BEFORE" != "None" ] || printf '  ⚠ no EIP currently associated — proceeding, but verify Dhan static-IP after.\n'

# ---------------------------------------------------------------------------
# Stop → modify → start
# ---------------------------------------------------------------------------
# Clear stop-protection FIRST (idempotent). The live box may still have
# disable_api_stop=true if the Terraform that flips it to false (PR #867)
# hasn't applied yet — without this, the stop below fails with
# OperationNotPermitted *after* the market-hours guard already committed,
# leaving a half-run. --no-disable-api-stop is a no-op when already cleared.
log "Clearing stop-protection (disable_api_stop=false) — idempotent"
run "aws ec2 modify-instance-attribute --region '$REGION' --instance-id '$IID' --no-disable-api-stop"

log "Stopping ${IID} (downtime begins)"
run "aws ec2 stop-instances --region '$REGION' --instance-ids '$IID' >/dev/null"
run "aws ec2 wait instance-stopped --region '$REGION' --instance-ids '$IID'"
ok "Stopped."

log "Modifying instance type → ${TO_TYPE}"
run "aws ec2 modify-instance-attribute --region '$REGION' --instance-id '$IID' --instance-type '{\"Value\":\"${TO_TYPE}\"}'"
ok "Attribute set."

log "Starting ${IID}"
run "aws ec2 start-instances --region '$REGION' --instance-ids '$IID' >/dev/null"
run "aws ec2 wait instance-running --region '$REGION' --instance-ids '$IID'"
ok "Running."

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------
if [ "$DRY_RUN" -eq 1 ]; then
  log "Dry-run complete — no changes made."
  exit 0
fi

log "Verifying"
NEW_TYPE=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$IID" \
  --query 'Reservations[0].Instances[0].InstanceType' --output text)
[ "$NEW_TYPE" = "$TO_TYPE" ] || die "Post-upgrade type is ${NEW_TYPE}, expected ${TO_TYPE}."
ok "Instance type: ${NEW_TYPE}"

EIP_AFTER=$(aws ec2 describe-addresses --region "$REGION" \
  --filters "Name=instance-id,Values=${IID}" \
  --query 'Addresses[0].PublicIp' --output text 2>/dev/null || echo "None")
ok "Elastic IP after: ${EIP_AFTER}"
if [ "$EIP_BEFORE" != "None" ] && [ "$EIP_AFTER" != "$EIP_BEFORE" ]; then
  die "Elastic IP changed (${EIP_BEFORE} → ${EIP_AFTER})! Dhan static-IP whitelist is now broken. Re-associate ${EIP_BEFORE} immediately."
fi

log "Upgrade complete: ${FROM_TYPE} → ${TO_TYPE}, EIP ${EIP_AFTER} preserved."
printf '  Next: confirm the app booted (make doctor / CloudWatch tv_boot_completed) and that ticks are flowing.\n'
