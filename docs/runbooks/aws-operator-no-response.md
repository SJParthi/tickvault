# Runbook — Operator AWOL / No Response in 10 min (Case L)

> **Severity:** Critical. Trade-day at risk.
> **Detection latency:** 10 min after primary SNS publish (Lambda gate).
> **Companion:** `docs/architecture/aws-daily-lifecycle.md` §3 Case L.

## Symptom

- A primary alert (any of Cases A-K) was published to SNS.
- 10 minutes have passed with no operator acknowledgement.
- Escalation Lambda has fired, dispatching to secondary channels.

## What the system does (Item AWS-10, to be implemented)

```
Primary alert at T+0    → SMS + Email + Telegram-webhook (parallel)
                        │
                        ▼  (operator has 10 min to ack via any mechanism)
At T+10min, Lambda checks SSM Parameter `/tickvault/alerts/last-acked-ts`
                        │
       ┌────────────────┴────────────────┐
       ▼                                 ▼
   Acked recently              Not acked
   → no escalation             → escalation fan-out:
                                  - Phone CALL via Amazon Connect
                                  - Secondary phone number SMS
                                  - Slack #incidents
```

## How operator acknowledges

Any ONE of these:

1. Reply `/ack` to the Telegram bot (bot updates SSM parameter)
2. SMS reply `ACK` to a designated number
3. Click an "ack" URL in the email (calls a tiny Lambda)
4. Run `aws ssm put-parameter --name /tickvault/alerts/last-acked-ts --value $(date +%s) --overwrite` from any phone/laptop

## What the operator should do AT THE SAME TIME

1. Identify which primary alert fired (subject line of the message).
2. Follow the corresponding runbook from `docs/runbooks/aws-*.md`.
3. If app is still alive and partly working, decide go/no-go for market open:
   - If pre-09:14:30 IST: can still meet `Phase2ReadinessPassed` window.
   - If post-09:14:30 IST: kill switch should auto-engage (Phase 1 design); confirm via `dhan killswitch GET` API.

## Acceptance criteria

- Operator acknowledges within 10 min OR escalation tier fires.
- No trading day proceeds past 09:14:30 IST without operator confirmation.

## Post-incident

- Update SSM parameter audit table `alert_ack_audit`.
- Review whether the primary 3-leg fan-out reached the operator at all (carrier delays?).
- File issue if any leg's p99 latency exceeded 60s.

## Honest envelope (per operator-charter §F)

> "Operator-AWOL detection has a 10-min deadline by design. Beyond that, escalation tier fires; cumulative envelope to first secondary-channel delivery is ≤12 min. NOT promised: human response time. The system protects the trade day by auto-engaging kill switch at 09:14:30 IST if no ack was received."
