# Runbook — EC2 InsufficientInstanceCapacity (Case C)

> **Severity:** Critical. App will not auto-start today in the primary AZ.
> **Detection latency:** ≤2 min after SSM Automation execution.
> **Companion:** `docs/architecture/aws-daily-lifecycle.md` §3 Case C.

## Symptom

- EventBridge fired, IAM is fine.
- SSM Automation execution shows `Failed` state.
- CloudTrail `StartInstances` event has `Error: InsufficientInstanceCapacity`.

## Why this happens

AWS ap-south-1 has a finite hardware pool per AZ per instance type. On busy mornings, t4g.medium in `ap-south-1a` may be temporarily exhausted. AWS does not guarantee single-AZ capacity.

## Immediate actions

### Option 1 — Wait + retry (5-10 min)

```bash
# Wait 5 minutes, then manual retry
sleep 300
aws ec2 start-instances --region ap-south-1 --instance-ids $INSTANCE_ID
```

### Option 2 — Stop + change instance type + start

```bash
# Try c7i.large (1 size smaller) as fallback
aws ec2 modify-instance-attribute --region ap-south-1 \
  --instance-id $INSTANCE_ID \
  --instance-type Value=c7i.large

aws ec2 start-instances --region ap-south-1 --instance-ids $INSTANCE_ID
```

### Option 3 — Switch AZ (requires EBS volume snapshot+restore — DESTRUCTIVE)

NOT recommended live. Document in case of all-AZ exhaustion (extremely rare).

## Acceptance criteria

- Instance reaches `running` within 10 minutes of incident detection.
- If using c7i.large fallback: verify the host memory budget in `aws-budget.md` (rule 6) still fits within the smaller instance's 8GB (it does — c7i.large is also 8GB).

## Post-incident

- Operator decision: keep c7i.large running for the day, OR stop + revert + restart on t4g.medium tomorrow.
- File Item AWS-11 plan to implement multi-AZ fallback SSM Automation.
- Honest envelope: AWS does not guarantee against this; accept periodic occurrence.
