#!/usr/bin/env bash
# aws-upgrade-instance.sh — one-command in-place EC2 upgrade tool.
#
# Implements §7 Mechanical Rule 7 of
# `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`:
# the existing prod EC2 instance is upgraded IN PLACE (same instance-id,
# same Elastic IP, same EBS volume) — NOT replaced. The Dhan static-IP
# whitelist (EIP, 7-day modify cooldown) and the QuestDB data volume are
# preserved across the upgrade.
#
# This is READY-TO-FIRE TOOLING, not an actual upgrade. The instance-type
# LOCK is t4g.medium everywhere (Terraform validation +
# instance_type_lock_guard.rs) per operator Quote 8 (2026-07-15 downsize;
# supersedes the 2026-06-30 r8g.large Quote 7 lock). Flipping the lock to a
# different type requires a dated operator quote + the 4-file lock update
# FIRST (see daily-universe-scope-expansion-2026-05-27.md §7 Mechanical
# Rule 1). This script is the MANUAL fallback for the r8g.large → t4g.medium
# flip (the primary path is .github/workflows/downsize-instance.yml) — the
# safe defaults below are the current locked types.
#
# What it does (each step idempotent / guarded):
#   1. Discover the instance by Name tag `tv-${ENV}-app` (default env=prod).
#   2. Validate --to is in the allowlist {t4g.medium,t4g.large,m8g.large,r8g.large,m8g.xlarge,r8g.xlarge}.
#   3. Refuse unless current type == FROM_TYPE (idempotent: if already TO_TYPE,
#      report and skip the type change but still run optional EBS/QuestDB steps).
#   4. Market-hours guard — refuse 09:00–15:30 IST Mon–Fri unless --force
#      (the instance-type change requires a stop; never stop a live session).
#   5. [optional] Online EBS resize: `aws ec2 modify-volume` on the root volume
#      (size / IOPS / throughput), polled to optimizing|completed. NO stop.
#   6. Instance-type flip: stop → modify-instance-attribute → start → WAIT.
#      ROLLBACK: if `wait instance-running` fails/times out, revert to FROM_TYPE,
#      start, verify, and exit non-zero.
#   7. Verify the new type + that the EIP is still associated.
#   8. [optional] QuestDB mem_limit retune: emit (or --apply-ssm send) the exact
#      command to set QDB_MEM_LIMIT + restart the questdb container.
#   9. Print the in-guest growpart + resize2fs/xfs_growfs commands for the
#      operator to run on the box (this script never SSHes).
#
# Downtime ≈ 3 minutes (only the instance-type flip stops the box; EBS resize
# and QuestDB retune are online). Run from the operator's Mac (uses ~/.aws creds).
# Scheduled window per rule: Sunday/off-market (08:30–16:30 IST Mon–Fri blocked).
#
# Usage:
#   ./scripts/aws-upgrade-instance.sh                       # type flip only, guarded
#                                                           # (t4g.medium target auto-sets QDB_MEM_LIMIT=1g)
#   ./scripts/aws-upgrade-instance.sh --dry-run             # print actions, change nothing
#   ./scripts/aws-upgrade-instance.sh --force               # bypass market-hours guard
#   ./scripts/aws-upgrade-instance.sh --from r8g.large --to t4g.medium \
#        --qdb-mem 1g
#   ./scripts/aws-upgrade-instance.sh --ebs-size 60         # ONLY grow the disk (no type change)
#   ./scripts/aws-upgrade-instance.sh --qdb-mem 1g --apply-ssm  # retune QuestDB, send via SSM
#   ./scripts/aws-upgrade-instance.sh --from t4g.medium --to r8g.large \
#        --qdb-mem 4g                                       # emergency roll-UP direction
#
# NOTE: a t4g.medium target auto-defaults QDB_MEM_LIMIT=1g and an r8g.large
#   target auto-defaults QDB_MEM_LIMIT=4g (persisted in repo/deploy/docker/.env)
#   so the QuestDB ceiling always lands COUPLED to the host RAM change.
#   Pass an explicit --qdb-mem to override.
#
# EBS NOTE: this script GROWS disks only — gp3 can NEVER shrink, and the
#   2026-07-15 20 GB target is a FRESH-VOLUME replacement plan (terraform
#   terminate-and-recreate in the operator's erase window), not a resize.
#
# Env overrides: TV_ENV, AWS_REGION, FROM_TYPE, TO_TYPE.
# Idempotent: re-running after success is a no-op for steps already applied.

set -euo pipefail

# ---------------------------------------------------------------------------
# Config + defaults
# ---------------------------------------------------------------------------
ENV="${TV_ENV:-prod}"
REGION="${AWS_REGION:-ap-south-1}"          # ap-south-1 Mumbai per aws-budget.md
FROM_TYPE="${FROM_TYPE:-t4g.medium}"         # the prior locked type to flip FROM (overridable)
TO_TYPE="${TO_TYPE:-m8g.large}"            # operator lock 2026-07-15 §7 Quote 8 (downsize)
NAME_TAG="tv-${ENV}-app"

# Allowlist of flip targets. Adding a new type here is NOT the same as
# flipping the lock — the lock lives in Terraform + the ratchet. This list
# only bounds what this tool will accept once a flip is authorized.
# r8g.large is KEPT so the emergency roll-UP direction stays executable.
ALLOWED_TO_TYPES="t4g.medium t4g.large m8g.large r8g.large m8g.xlarge r8g.xlarge"

DRY_RUN=0
FORCE=0
EBS_SIZE=""
EBS_IOPS=""
EBS_THROUGHPUT=""
QDB_MEM=""
APPLY_SSM=0

usage() {
  sed -n '2,46p' "$0" | sed 's/^# \{0,1\}//'
}

# ---------------------------------------------------------------------------
# Arg parsing (supports `--flag value` and `--flag=value`)
# ---------------------------------------------------------------------------
while [ "$#" -gt 0 ]; do
  arg="$1"
  val=""
  had_eq=0
  case "$arg" in
    *=*) val="${arg#*=}"; arg="${arg%%=*}"; had_eq=1 ;;
  esac
  # For a value-taking flag, resolve $val from either `--flag=value` (already
  # split above) or the next positional `--flag value`. Consume the extra
  # positional in THIS (parent) loop via an extra `shift`, never inside a fn.
  take_val() {
    if [ "$had_eq" -eq 1 ]; then return 0; fi
    if [ "$#" -lt 2 ]; then echo "missing value for $arg" >&2; exit 2; fi
    val="$2"
    shift_extra=1
  }
  shift_extra=0
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --force)   FORCE=1 ;;
    --apply-ssm) APPLY_SSM=1 ;;
    --from) take_val "$@"; FROM_TYPE="$val" ;;
    --to) take_val "$@"; TO_TYPE="$val" ;;
    --ebs-size) take_val "$@"; EBS_SIZE="$val" ;;
    --ebs-iops) take_val "$@"; EBS_IOPS="$val" ;;
    --ebs-throughput) take_val "$@"; EBS_THROUGHPUT="$val" ;;
    --qdb-mem) take_val "$@"; QDB_MEM="$val" ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $arg" >&2; echo "run with --help for usage." >&2; exit 2 ;;
  esac
  shift
  [ "$shift_extra" -eq 1 ] && shift
done

log() { printf '\n\033[1;36m▶ %s\033[0m\n' "$*"; }
ok()  { printf '\033[1;32m  ✓ %s\033[0m\n' "$*"; }
warn(){ printf '\033[1;33m  ⚠ %s\033[0m\n' "$*"; }
die() { printf '\033[1;31m  ✗ %s\033[0m\n' "$*" >&2; exit 1; }
run() { if [ "$DRY_RUN" -eq 1 ]; then echo "    [dry-run] $*"; else eval "$*"; fi; }

command -v aws >/dev/null || die "aws CLI not found — install + 'aws configure' first."

# ---------------------------------------------------------------------------
# Validate args
# ---------------------------------------------------------------------------
log "Validating arguments"

# --to must be in the allowlist (exact-match — no metacharacters possible).
case " $ALLOWED_TO_TYPES " in
  *" $TO_TYPE "*) ok "Target type ${TO_TYPE} is allowed." ;;
  *) die "Refusing --to '${TO_TYPE}': not in allowlist {${ALLOWED_TO_TYPES}}. To add a type, update this allowlist AND complete the 4-file lock flip (dated operator quote) first." ;;
esac

# --from is user-supplied and reaches modify-instance-attribute via run()'s
# eval. It is also gated to == CUR_TYPE before the flip, but validate its FORMAT
# strictly here too (defence-in-depth): an EC2 instance type is letters/digits
# with one dot, e.g. m8g.large / t4g.medium. Reject anything with a shell
# metacharacter so it can never inject through the eval.
if [[ ! "$FROM_TYPE" =~ ^[a-z0-9]+\.[a-z0-9]+$ ]]; then
  die "Refusing --from '${FROM_TYPE}': not a valid EC2 instance type (expected e.g. m8g.large)."
fi

is_uint() { case "$1" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac; }

if [ -n "$EBS_SIZE" ]; then
  is_uint "$EBS_SIZE" || die "--ebs-size must be a positive integer (GB)."
  { [ "$EBS_SIZE" -ge 10 ] && [ "$EBS_SIZE" -le 200 ]; } || die "--ebs-size must be 10–200 GB (matches Terraform ebs_gp3_size_gb range)."
fi
if [ -n "$EBS_IOPS" ]; then
  is_uint "$EBS_IOPS" || die "--ebs-iops must be a positive integer."
  { [ "$EBS_IOPS" -ge 3000 ] && [ "$EBS_IOPS" -le 16000 ]; } || die "--ebs-iops must be 3000–16000 (gp3 range; matches Terraform ebs_gp3_iops)."
fi
if [ -n "$EBS_THROUGHPUT" ]; then
  is_uint "$EBS_THROUGHPUT" || die "--ebs-throughput must be a positive integer (MiB/s)."
  { [ "$EBS_THROUGHPUT" -ge 125 ] && [ "$EBS_THROUGHPUT" -le 1000 ]; } || die "--ebs-throughput must be 125–1000 MiB/s (gp3 range; matches Terraform ebs_gp3_throughput)."
fi
if [ -n "$QDB_MEM" ]; then
  # STRICT regex anchor — a shell glob ([1-9]*[gGmM]) would let metacharacters
  # through (e.g. "4g;reboot" / "4g'; curl x|sh; echo '4" end in a letter and
  # match), and that value flows into run()'s eval AND the remote SSM heredoc =
  # remote code exec. Anchored [[ =~ ]] permits ONLY digits + a single g/m/G/M.
  if [[ "$QDB_MEM" =~ ^[1-9][0-9]*[gGmM]$ ]]; then
    ok "QuestDB mem target ${QDB_MEM}."
  else
    die "--qdb-mem must be digits followed by g/m (e.g. '4g' or '4096m', docker mem_limit syntax). Got: '${QDB_MEM}'."
  fi
fi
# --apply-ssm needs something to send. Allow it without an explicit --qdb-mem
# ONLY when the target has an auto-defaulted QDB_MEM below (t4g.medium → 1g,
# r8g.large → 4g, coupled after instance discovery) — so there WILL be a value
# to send. For any other target, --apply-ssm with no --qdb-mem sends nothing.
[ "$APPLY_SSM" -eq 1 ] && [ -z "$QDB_MEM" ] \
  && [ "$TO_TYPE" != "r8g.large" ] && [ "$TO_TYPE" != "t4g.medium" ] \
  && die "--apply-ssm requires --qdb-mem (nothing to send otherwise)."

WANT_EBS=0
{ [ -n "$EBS_SIZE" ] || [ -n "$EBS_IOPS" ] || [ -n "$EBS_THROUGHPUT" ]; } && WANT_EBS=1

# ---------------------------------------------------------------------------
# Market-hours guard (IST). The instance-type flip needs a stop — never
# during a session. (EBS resize + QuestDB retune are online; the guard fires
# only because the type flip is requested when FROM != TO. If only --ebs-size
# / --qdb-mem are given with no type change, the box is never stopped, but we
# keep the guard conservative and only relax it when no stop will occur.)
# ---------------------------------------------------------------------------
log "Market-hours guard"
utc_h=$(date -u +%H); utc_m=$(date -u +%M)
ist_total=$(( (10#$utc_h * 60 + 10#$utc_m + 330) % 1440 ))
dow=$(date -u +%u)   # 1=Mon .. 7=Sun (UTC dow ~ IST dow at these hours)
IN_MARKET=0
# Market window 09:00–15:30 IST = 540–930 minutes, Mon–Fri.
if [ "$dow" -le 5 ] && [ "$ist_total" -ge 540 ] && [ "$ist_total" -le 930 ]; then
  IN_MARKET=1
fi

# ---------------------------------------------------------------------------
# Discover the instance
# ---------------------------------------------------------------------------
log "Discovering instance Name=${NAME_TAG} in ${REGION}"
IID=$(aws ec2 describe-instances --region "$REGION" \
  --filters "Name=tag:Name,Values=${NAME_TAG}" \
            "Name=instance-state-name,Values=pending,running,stopping,stopped" \
  --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null || true)
# shellcheck disable=SC2015  # die always exits, so this is a guard (not if-then-else)
[ -n "$IID" ] && [ "$IID" != "None" ] || die "No instance found with Name=${NAME_TAG}. Is the stack deployed? Check region."
# Guard against more than one match (split on whitespace).
if [ "$(printf '%s' "$IID" | wc -w | tr -d ' ')" != "1" ]; then
  die "Multiple instances matched Name=${NAME_TAG}: ${IID}. Refusing to act ambiguously."
fi
ok "Instance: ${IID}"

CUR_TYPE=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$IID" \
  --query 'Reservations[0].Instances[0].InstanceType' --output text)
ok "Current type: ${CUR_TYPE}"

# Decide whether a type flip is needed.
DO_TYPE_FLIP=1
if [ "$CUR_TYPE" = "$TO_TYPE" ]; then
  ok "Already ${TO_TYPE} — instance-type flip is a no-op (idempotent)."
  DO_TYPE_FLIP=0
elif [ "$CUR_TYPE" != "$FROM_TYPE" ]; then
  die "Expected ${FROM_TYPE} but found ${CUR_TYPE}. Refusing — investigate, or pass --from ${CUR_TYPE}."
fi

# ---------------------------------------------------------------------------
# Couple QuestDB mem_limit to the host RAM change (2026-07-15 Quote 8).
# When the operator did NOT pass an explicit --qdb-mem, auto-default it per
# target so Step 8 persists the matching QDB_MEM_LIMIT in repo/deploy/docker/.env
# EXACTLY when the box changes size — t4g.medium (4 GiB) → 1g (the downsize),
# r8g.large (16 GiB) → 4g (the emergency roll-UP direction). Applies whether
# the flip happens now (DO_TYPE_FLIP=1) or the box is already TO_TYPE
# (idempotent re-run / retune-only, DO_TYPE_FLIP=0). Any explicit --qdb-mem
# the operator passed wins (already validated above).
if [ -z "$QDB_MEM" ]; then
  case "$TO_TYPE" in
    t4g.medium)
      QDB_MEM="1g"
      ok "Target t4g.medium (4 GiB): defaulting QuestDB mem_limit to ${QDB_MEM} (set in repo/deploy/docker/.env at Step 8, coupled to the downsize)." ;;
    r8g.large)
      QDB_MEM="4g"
      ok "Target r8g.large (16 GiB): defaulting QuestDB mem_limit to ${QDB_MEM} (set in repo/deploy/docker/.env at Step 8, coupled to the resize)." ;;
    *) : ;; # no auto-default for other targets — --qdb-mem is explicit-only
  esac
fi

# Now apply the market-hours guard ONLY if a stop will actually happen.
if [ "$DO_TYPE_FLIP" -eq 1 ] && [ "$IN_MARKET" -eq 1 ]; then
  if [ "$FORCE" -eq 1 ]; then
    warn "within market hours but --force given; proceeding with the stop."
  else
    die "Refusing the instance-type flip: within 09:00–15:30 IST Mon–Fri (a stop would interrupt the live session). Re-run on a Sunday/off-market window, or pass --force. (Online --ebs-size/--qdb-mem steps need no stop and would be safe.)"
  fi
else
  ok "No live-session stop will occur (type-flip=${DO_TYPE_FLIP}, in-market=${IN_MARKET})."
fi

# Record the associated EIP so we can verify it survives.
EIP_BEFORE=$(aws ec2 describe-addresses --region "$REGION" \
  --filters "Name=instance-id,Values=${IID}" \
  --query 'Addresses[0].PublicIp' --output text 2>/dev/null || echo "None")
ok "Elastic IP before: ${EIP_BEFORE}"
[ "$EIP_BEFORE" != "None" ] || warn "no EIP currently associated — proceeding, but verify Dhan static-IP after."

# Discover the root volume id (needed for EBS resize + the in-guest grow note).
ROOT_DEV=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$IID" \
  --query 'Reservations[0].Instances[0].RootDeviceName' --output text 2>/dev/null || echo "/dev/xvda")
VOL_ID=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$IID" \
  --query "Reservations[0].Instances[0].BlockDeviceMappings[?DeviceName=='${ROOT_DEV}'].Ebs.VolumeId | [0]" \
  --output text 2>/dev/null || echo "None")
ok "Root device ${ROOT_DEV} → volume ${VOL_ID}"

# ---------------------------------------------------------------------------
# Step 5 — online EBS resize (optional, NO stop). Do this BEFORE the type
# flip so the larger disk is already in place when the box comes back up.
# ---------------------------------------------------------------------------
if [ "$WANT_EBS" -eq 1 ]; then
  [ "$VOL_ID" != "None" ] || die "Could not resolve the root EBS volume id — cannot resize."

  # gp3 can GROW online but can NEVER shrink. Guard --ebs-size against the
  # CURRENT volume size: refuse a shrink (would be rejected by AWS / risk data
  # loss), and skip a no-op when the requested size already matches.
  if [ -n "$EBS_SIZE" ]; then
    CUR_VOL_SIZE=$(aws ec2 describe-volumes --region "$REGION" --volume-ids "$VOL_ID" \
      --query 'Volumes[0].Size' --output text 2>/dev/null || echo "None")
    is_uint "$CUR_VOL_SIZE" || die "Could not read the current size of ${VOL_ID} (got '${CUR_VOL_SIZE}') — refusing to resize blind."
    if [ "$EBS_SIZE" -lt "$CUR_VOL_SIZE" ]; then
      die "--ebs-size ${EBS_SIZE} GB is SMALLER than the current ${CUR_VOL_SIZE} GB. gp3 cannot shrink — refusing. Pass a size >= ${CUR_VOL_SIZE}."
    fi
    if [ "$EBS_SIZE" -eq "$CUR_VOL_SIZE" ]; then
      warn "--ebs-size ${EBS_SIZE} GB equals the current size — skipping the size change (no-op)."
      EBS_SIZE=""
      # If size was the ONLY EBS arg, there is nothing left to resize.
      { [ -n "$EBS_IOPS" ] || [ -n "$EBS_THROUGHPUT" ]; } || WANT_EBS=0
    fi
  fi
fi

if [ "$WANT_EBS" -eq 1 ]; then
  log "Online EBS resize on ${VOL_ID} (size=${EBS_SIZE:-keep} iops=${EBS_IOPS:-keep} throughput=${EBS_THROUGHPUT:-keep})"
  MOD_ARGS="--region '$REGION' --volume-id '$VOL_ID'"
  [ -n "$EBS_SIZE" ]       && MOD_ARGS="$MOD_ARGS --size $EBS_SIZE"
  [ -n "$EBS_IOPS" ]       && MOD_ARGS="$MOD_ARGS --iops $EBS_IOPS"
  [ -n "$EBS_THROUGHPUT" ] && MOD_ARGS="$MOD_ARGS --throughput $EBS_THROUGHPUT"
  run "aws ec2 modify-volume $MOD_ARGS >/dev/null"

  if [ "$DRY_RUN" -eq 0 ]; then
    log "Polling volume-modification state (optimizing|completed)…"
    EBS_DONE=0
    for _ in $(seq 1 60); do
      STATE=$(aws ec2 describe-volumes-modifications --region "$REGION" --volume-ids "$VOL_ID" \
        --query 'VolumesModifications[0].ModificationState' --output text 2>/dev/null || echo "None")
      case "$STATE" in
        optimizing|completed) ok "Modification state: ${STATE} — disk change is live."; EBS_DONE=1; break ;;
        failed) die "EBS modify-volume FAILED. Inspect: aws ec2 describe-volumes-modifications --volume-ids ${VOL_ID}" ;;
        *) printf '    …state=%s\n' "$STATE"; sleep 5 ;;
      esac
    done
    # 60 × 5s = 5 min. If we never reached optimizing|completed, do NOT proceed
    # silently — the modify may still be queued/stuck; the operator must check.
    if [ "$EBS_DONE" -ne 1 ]; then
      die "EBS modify-volume did NOT reach optimizing|completed within ~5 minutes (last state='${STATE}'). NOT proceeding to the instance-type flip. Check: aws ec2 describe-volumes-modifications --region ${REGION} --volume-ids ${VOL_ID}"
    fi
  fi
  ok "EBS resize requested. The in-guest grow commands are printed at the end."
fi

# ---------------------------------------------------------------------------
# Step 6 — instance-type flip with rollback (the only step that stops the box)
# ---------------------------------------------------------------------------
rollback_type() {
  warn "Start FAILED on ${TO_TYPE}. Rolling back to ${FROM_TYPE}…"
  run "aws ec2 modify-instance-attribute --region '$REGION' --instance-id '$IID' --instance-type '{\"Value\":\"${FROM_TYPE}\"}'" || true
  run "aws ec2 start-instances --region '$REGION' --instance-ids '$IID' >/dev/null" || true
  if [ "$DRY_RUN" -eq 0 ]; then
    if aws ec2 wait instance-running --region "$REGION" --instance-ids "$IID" 2>/dev/null; then
      RB_TYPE=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$IID" \
        --query 'Reservations[0].Instances[0].InstanceType' --output text 2>/dev/null || echo "?")
      die "Rolled back to ${RB_TYPE} and the box is running again. The ${TO_TYPE} upgrade FAILED — likely an InsufficientInstanceCapacity for ${TO_TYPE} in ${REGION}. Try a different AZ or retry later."
    fi
    die "ROLLBACK ALSO FAILED to bring the box up on ${FROM_TYPE}. MANUAL INTERVENTION REQUIRED: aws ec2 describe-instances --instance-ids ${IID}; the EBS volume + data are intact (delete_on_termination=false)."
  fi
  die "Rollback path exercised (dry-run)."
}

if [ "$DO_TYPE_FLIP" -eq 1 ]; then
  # Clear stop-protection FIRST (idempotent). The live box may still have
  # disable_api_stop=true; --no-disable-api-stop is a no-op when already cleared.
  log "Clearing stop-protection (disable_api_stop=false) — idempotent"
  run "aws ec2 modify-instance-attribute --region '$REGION' --instance-id '$IID' --no-disable-api-stop"

  # Pre-start steps (stop → modify). If any FAIL, the box is still on
  # FROM_TYPE — no rollback needed, but message clearly so the operator knows
  # the box is stopped on the ORIGINAL type and a re-run is safe + idempotent.
  log "Stopping ${IID} (downtime begins)"
  run "aws ec2 stop-instances --region '$REGION' --instance-ids '$IID' >/dev/null" \
    || die "stop-instances failed. The box is still ${FROM_TYPE} (not modified). Investigate, then re-run — this script is idempotent."
  run "aws ec2 wait instance-stopped --region '$REGION' --instance-ids '$IID'" \
    || die "Timed out waiting for ${IID} to stop. The box is still ${FROM_TYPE} (not modified). Check 'aws ec2 describe-instances --instance-ids ${IID}', then re-run — safe + idempotent."
  ok "Stopped."

  log "Modifying instance type ${FROM_TYPE} → ${TO_TYPE}"
  run "aws ec2 modify-instance-attribute --region '$REGION' --instance-id '$IID' --instance-type '{\"Value\":\"${TO_TYPE}\"}'" \
    || die "modify-instance-attribute failed. The box is STOPPED, still ${FROM_TYPE}. Restart it with 'aws ec2 start-instances --instance-ids ${IID}' (it comes back on ${FROM_TYPE}), or re-run this script."
  ok "Attribute set."

  # Start on TO_TYPE. This is the capacity-risk step: a SYNCHRONOUS
  # InsufficientInstanceCapacity (most likely when flipping to r8g) makes
  # start-instances itself fail BEFORE the wait below. Wrap BOTH the start AND
  # the wait so a failure of EITHER triggers rollback to FROM_TYPE. We must NOT
  # let `set -e` abort before rollback — hence `if ! run ...; then ...`.
  log "Starting ${IID} on ${TO_TYPE}"
  if ! run "aws ec2 start-instances --region '$REGION' --instance-ids '$IID' >/dev/null"; then
    rollback_type
  fi
  if [ "$DRY_RUN" -eq 0 ]; then
    if ! aws ec2 wait instance-running --region "$REGION" --instance-ids "$IID" 2>/dev/null; then
      rollback_type
    fi
  fi
  ok "Running."
fi

# ---------------------------------------------------------------------------
# Step 7 — verify (skip in dry-run)
# ---------------------------------------------------------------------------
if [ "$DRY_RUN" -eq 1 ]; then
  log "Dry-run complete — no changes made."
else
  log "Verifying"
  NEW_TYPE=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$IID" \
    --query 'Reservations[0].Instances[0].InstanceType' --output text)
  if [ "$DO_TYPE_FLIP" -eq 1 ]; then
    [ "$NEW_TYPE" = "$TO_TYPE" ] || die "Post-upgrade type is ${NEW_TYPE}, expected ${TO_TYPE}."
  fi
  ok "Instance type: ${NEW_TYPE}"

  EIP_AFTER=$(aws ec2 describe-addresses --region "$REGION" \
    --filters "Name=instance-id,Values=${IID}" \
    --query 'Addresses[0].PublicIp' --output text 2>/dev/null || echo "None")
  ok "Elastic IP after: ${EIP_AFTER}"
  if [ "$EIP_BEFORE" != "None" ] && [ "$EIP_AFTER" != "$EIP_BEFORE" ]; then
    die "Elastic IP changed (${EIP_BEFORE} → ${EIP_AFTER})! Dhan static-IP whitelist is now broken. Re-associate ${EIP_BEFORE} immediately."
  fi
fi

# ---------------------------------------------------------------------------
# Step 8 — QuestDB mem_limit retune. The compose PROJECT on the box is
# /opt/tickvault/repo/deploy/docker (user-data.sh.tftpl Step 6 cd's there;
# deploy-aws.yml operates there too) — NOT /opt/tickvault/deploy/docker, which
# does not exist on the box (review round 4 HIGH: the old path aborted the
# whole && chain at `touch`, so the documented manual/emergency retune leg
# never executed). The compose file reads ${QDB_MEM_LIMIT:-1g}, so we persist
# that env in the PROJECT .env (the one every compose caller reads) and
# recreate ONLY the questdb container. docker-compose.yml also HARD-REQUIRES
# TV_QUESTDB_PG_USER / TV_QUESTDB_PG_PASSWORD ("${VAR:?}" interpolations) on
# EVERY compose invocation and they are never persisted to .env — so the
# on-box command fetches them from SSM at invocation time (the deploy-aws.yml
# ~L634 pattern, same admin/quest fallbacks; the instance role has
# ssm:GetParameter). QDB_MEM is either an explicit --qdb-mem or the per-target
# auto-default (t4g.medium→1g, r8g.large→4g), coupling the QuestDB ceiling to
# the host RAM change. We EMIT the exact command; with --apply-ssm we send it
# via SSM RunShellScript (only if the operator opts in).
# ---------------------------------------------------------------------------
if [ -n "$QDB_MEM" ]; then
  log "QuestDB mem_limit retune → ${QDB_MEM}"
  # The on-box command: persist QDB_MEM_LIMIT in the compose PROJECT env file,
  # export the SSM-held QuestDB PG creds (compose hard-requires them), and
  # recreate ONLY the questdb container so the new mem_limit takes effect.
  # NOTE: \$(...) below is deliberately escaped — the SSM parameter fetches run
  # ON THE BOX at invocation time, never on the operator's machine.
  read -r -d '' QDB_CMD <<EOF || true
cd /opt/tickvault/repo/deploy/docker \
  && touch .env \
  && (grep -q '^QDB_MEM_LIMIT=' .env \
        && sed -i 's/^QDB_MEM_LIMIT=.*/QDB_MEM_LIMIT=${QDB_MEM}/' .env \
        || echo 'QDB_MEM_LIMIT=${QDB_MEM}' >> .env) \
  && export TV_QUESTDB_PG_USER=\$(aws ssm get-parameter --region ${REGION} --name /tickvault/prod/questdb/pg-user --with-decryption --output text --query Parameter.Value 2>/dev/null || echo admin) \
  && export TV_QUESTDB_PG_PASSWORD=\$(aws ssm get-parameter --region ${REGION} --name /tickvault/prod/questdb/pg-password --with-decryption --output text --query Parameter.Value 2>/dev/null || echo quest) \
  && docker compose config -q \
  && docker compose up -d --no-deps tv-questdb \
  && echo "questdb restarted with mem_limit=${QDB_MEM}"
EOF
  printf '  Run this ON THE BOX (or it is SSM-sent below):\n\n%s\n\n' "$QDB_CMD"

  if [ "$APPLY_SSM" -eq 1 ]; then
    command -v jq >/dev/null || die "jq not found — required to build the SSM parameters JSON safely. Install jq, or run the command above on the box manually."
    log "Sending the QuestDB retune via SSM RunShellScript"
    # Build the --parameters JSON with jq --arg so ANY character in the command
    # (quotes, backslashes, newlines) is JSON-escaped correctly — never via
    # printf/sed string-mashing. QDB_MEM is already strict-regex-validated, so
    # this is defence-in-depth. The single-line command is one array element.
    QDB_CMD_LINE=$(printf '%s' "$QDB_CMD" | tr '\n' ' ')
    PARAMS=$(jq -n --arg cmd "$QDB_CMD_LINE" '{commands: [$cmd]}')
    # Invoke aws DIRECTLY (NOT through run()/eval): pass every interpolation as a
    # separate, properly-quoted argv element so nothing reaches a shell eval.
    if [ "$DRY_RUN" -eq 1 ]; then
      echo "    [dry-run] aws ssm send-command --region $REGION --document-name AWS-RunShellScript --targets Key=InstanceIds,Values=${IID} --comment 'tickvault QuestDB mem_limit retune to ${QDB_MEM}' --parameters <json> --query Command.CommandId --output text"
    else
      aws ssm send-command --region "$REGION" \
        --document-name "AWS-RunShellScript" \
        --targets "Key=InstanceIds,Values=${IID}" \
        --comment "tickvault QuestDB mem_limit retune to ${QDB_MEM}" \
        --parameters "$PARAMS" \
        --query 'Command.CommandId' --output text \
        || die "SSM send-command failed. Run the command above on the box manually."
    fi
    ok "SSM command sent. Track it: aws ssm list-command-invocations --region ${REGION} --details --filters Key=InstanceId,Values=${IID}"
  else
    warn "Not sent automatically. Pass --apply-ssm to send via SSM, or run the command above on the box."
  fi
fi

# ---------------------------------------------------------------------------
# Step 9 — in-guest filesystem grow note (printed; this script never SSHes).
# AWS grows the EBS volume online, but the OS partition + filesystem must be
# grown inside the guest to actually use the new space.
# ---------------------------------------------------------------------------
if [ "$WANT_EBS" -eq 1 ] && [ -n "$EBS_SIZE" ]; then
  log "In-guest filesystem grow — RUN THESE ON THE BOX after the resize completes"
  cat <<'GUEST'
    # 1. Find the root partition (usually nvme0n1p1 on Nitro/Graviton):
    lsblk
    # 2. Grow the partition to fill the enlarged disk (note the SPACE before "1"):
    sudo growpart /dev/nvme0n1 1
    # 3. Grow the filesystem. Pick the one matching your root FS (check: df -hT /):
    #    xfs  (Amazon Linux 2023 default):
    sudo xfs_growfs -d /
    #    ext4:
    sudo resize2fs /dev/nvme0n1p1
    # 4. Confirm the new size:
    df -h /
GUEST
  warn "Until you grow the filesystem in-guest, the OS still sees the OLD size."
fi

log "Done."
if [ "$DO_TYPE_FLIP" -eq 1 ]; then
  printf '  Upgrade complete: %s → %s, EIP preserved.\n' "$FROM_TYPE" "$TO_TYPE"
fi
printf '  Next: confirm the app booted (make doctor / CloudWatch tv_boot_completed) and ticks are flowing.\n'
printf '  Lock note: a NEW production instance-type lock (e.g. r8g.large) requires a dated\n'
printf '  operator quote + the 4-file update (daily-universe-scope-expansion-2026-05-27.md §7\n'
printf '  Mechanical Rule 1) + the instance_type_lock_guard.rs ratchet BEFORE the flip is permanent.\n'
