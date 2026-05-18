# Runbook — StartInstances API Failed (Case B)

> **Severity:** Critical. App will not auto-start today.
> **Detection latency:** ≤2 min after rule fire.
> **Companion:** `docs/architecture/aws-daily-lifecycle.md` §3 Case B.

## Symptom

- EventBridge rule fired (CW metric `Invocations` incremented).
- But `Scheduler.TargetErrorCount` is non-zero.
- SQS DLQ depth alarm firing.
- Instance is still `stopped`.

## Likely causes

| # | Cause | How to check |
|---|---|---|
| 1 | IAM role `eventbridge_ec2_scheduler` lost `ec2:StartInstances` permission | `aws iam get-role-policy --role-name tv-prod-eventbridge-ec2-scheduler --policy-name tv-prod-eventbridge-ec2-scheduler` |
| 2 | SSM Automation document `AWS-StartEC2Instance` access denied | CloudTrail event `AssumeRole` denial |
| 3 | AWS-side API throttle | CloudTrail `StartInstances` `ThrottlingException` |
| 4 | Instance is being terminated (state-conflict) | `aws ec2 describe-instances ...` |

## Immediate actions

```bash
# 1. Look at DLQ message
aws sqs receive-message --region ap-south-1 \
  --queue-url $(aws sqs get-queue-url --queue-name tv-prod-eventbridge-dlq --output text)

# 2. Look at CloudTrail for last hour
aws cloudtrail lookup-events --region ap-south-1 \
  --lookup-attributes AttributeKey=EventName,AttributeValue=StartInstances \
  --max-results 5

# 3. If IAM issue — verify role policy
aws iam get-role-policy --role-name tv-prod-eventbridge-ec2-scheduler \
  --policy-name tv-prod-eventbridge-ec2-scheduler

# 4. Manual fallback
aws ec2 start-instances --region ap-south-1 --instance-ids $INSTANCE_ID
```

## Acceptance criteria

Same as `aws-eventbridge-rule-missing.md`.

## Post-incident

- If IAM regression — file PR to restore policy with ratchet test.
- If throttle — confirm `RetryPolicy` on EventBridge rule has `MaximumRetryAttempts >= 3`.
