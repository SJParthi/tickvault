# Runbook — EventBridge Rule Missing or Disabled (Case A)

> **Severity:** Critical. App will not auto-start today.
> **Detection latency:** ≤7 min after scheduled fire-time.
> **Companion:** `docs/architecture/aws-daily-lifecycle.md` §3 Case A.

## Symptom

- 08:30 IST has passed (or 08:00 IST per schedule lock).
- Telegram silence — no `BootReadyConfirmation` message.
- AWS-side CloudWatch alarm `tv-prod-weekday-start-missing-invocation` is firing.
- SNS fan-out delivered the alert via SMS + Email + Telegram-webhook.

## Likely causes

| # | Cause | How to check |
|---|---|---|
| 1 | Rule manually disabled | `aws events describe-rule --name tv-prod-weekday-start --query 'State'` returns `DISABLED` |
| 2 | Rule deleted by Terraform drift | rule not found |
| 3 | IAM role `eventbridge_ec2_scheduler` trust policy broken | `aws iam get-role-policy ...` errors |
| 4 | EventBridge service outage in ap-south-1 | AWS Health Dashboard |

## Immediate actions (operator on phone hotspot, ~3 min)

```bash
# 1. Confirm rule state
aws events describe-rule --region ap-south-1 \
  --name tv-prod-weekday-start

# 2. If disabled — enable
aws events enable-rule --region ap-south-1 \
  --name tv-prod-weekday-start

# 3. Manual one-shot start (do not wait for next day's cron)
INSTANCE_ID=$(aws ec2 describe-instances --region ap-south-1 \
  --filters "Name=tag:Name,Values=tv-prod-app" \
  --query 'Reservations[0].Instances[0].InstanceId' --output text)
aws ec2 start-instances --region ap-south-1 --instance-ids $INSTANCE_ID

# 4. Monitor boot via SSM (after instance reaches running)
aws ssm start-session --region ap-south-1 --target $INSTANCE_ID
# inside the session:
journalctl -u tickvault -f
```

## Acceptance criteria

- Instance reaches `running` within 60s of `start-instances` call.
- App emits `BootReadyConfirmation` Telegram within 180s of `running`.
- If app does not boot → escalate to `docs/runbooks/aws-disaster-recovery.md`.

## Post-incident

1. File a Terraform-drift issue.
2. Run `terraform plan` to verify all 4 rules are in expected state.
3. If the rule was disabled by a human, add a Terraform `lifecycle { ignore_changes = [] }` audit comment.
