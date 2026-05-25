"""Budget kill-switch Lambda — Z+ L7 COOLDOWN layer.

Subscribed to the dedicated `tv-${env}-budget-kill` SNS topic. AWS
Budgets publishes here when monthly spend crosses 100% of the
operator-locked limit (aws-budget.md — t4g.medium ~₹1,022/mo).

On invocation this Lambda:
  1. Stops the tv-app EC2 instance (prevents further EC2-hour spend)
  2. Publishes a Severity::Critical message to the operator's regular
     `tv_alerts` topic so the existing Telegram webhook from PR #781
     wakes the operator immediately

The L7 COOLDOWN demand from operator-charter-forever.md §C: the
recovery action itself must not cause unbounded damage. Stopping
EC2 leaves the EIP attached (₹306/mo continues to accrue) but caps
the EC2-hour bill (~₹514/mo) at the moment of breach. EBS continues
(~₹78/mo) because deleting it would lose all unflushed QuestDB
partitions — that decision belongs to the operator, not to
automation.

Net: stopped-instance burn ≈ ₹0.51/hour vs running ≈ ₹1.90/hour
(per aws-budget.md). Operator has time to investigate without the
clock ticking at full rate.

Environment variables (set by Terraform):
  EC2_INSTANCE_ID    — instance to stop on budget breach
  ALERTS_TOPIC_ARN   — operator's tv_alerts SNS topic for Telegram
  LOG_LEVEL          — INFO (default) / DEBUG / WARNING

No pip deps — boto3 is pre-installed in the AWS Lambda Python 3.12
runtime. Keeps the zip artifact tiny (< 5 KB).
"""

from __future__ import annotations

import json
import logging
import os
from typing import Any

LOG_LEVEL = os.environ.get("LOG_LEVEL", "INFO")
logger = logging.getLogger()
logger.setLevel(LOG_LEVEL)

EC2_INSTANCE_ID = os.environ.get("EC2_INSTANCE_ID", "")
ALERTS_TOPIC_ARN = os.environ.get("ALERTS_TOPIC_ARN", "")


def _extract_budget_message(event: dict[str, Any]) -> str:
    """Pull a human-readable summary out of the SNS budget envelope."""
    records = event.get("Records") or []
    if not records:
        return "<no SNS Records>"
    sns = records[0].get("Sns") or {}
    subject = sns.get("Subject") or "<no subject>"
    message = sns.get("Message") or "<no message>"
    # AWS Budgets sometimes ships a JSON message, sometimes a plain
    # paragraph. We don't need to parse — just include both so the
    # operator's Telegram has full context.
    if len(message) > 1000:
        message = message[:1000] + " …(truncated)"
    return f"Subject: {subject}\n{message}"


def _stop_instance(client: Any, instance_id: str) -> dict[str, Any]:
    """Call ec2:StopInstances. Idempotent — Stopping an already-stopped
    instance returns the current state, not an error."""
    return client.stop_instances(InstanceIds=[instance_id])


def _format_alert_payload(instance_id: str, stop_result: dict[str, Any], budget_context: str) -> dict[str, str]:
    """Build the SNS publish dict for the operator alert."""
    state_changes = stop_result.get("StoppingInstances") or []
    transitions = ", ".join(
        f"{sc.get('InstanceId', '?')}: "
        f"{(sc.get('PreviousState') or {}).get('Name', '?')} -> "
        f"{(sc.get('CurrentState') or {}).get('Name', '?')}"
        for sc in state_changes
    ) or "<no state change reported>"
    body = (
        f"BUDGET KILL-SWITCH ACTIVATED\n"
        f"Instance {instance_id} stop requested.\n"
        f"Transition: {transitions}\n"
        f"\n"
        f"Trigger:\n{budget_context}\n"
        f"\n"
        f"Next steps:\n"
        f"  1. Inspect AWS Cost Explorer for the runaway cost source.\n"
        f"  2. Fix the underlying issue (rogue stress test, unintended\n"
        f"     resource, exchange-rate spike, etc).\n"
        f"  3. Restart via AWS Console or `aws ec2 start-instances`.\n"
        f"  4. Charter aws-budget.md §6: EIP + EBS continue to accrue at\n"
        f"     ~Rs 0.51/hour while stopped (vs Rs 1.90/hour running)."
    )
    return {
        "Subject": "BUDGET KILL-SWITCH ACTIVATED",
        "Message": body,
    }


def _publish_alert(client: Any, topic_arn: str, payload: dict[str, str]) -> dict[str, Any]:
    """Publish the operator alert to the tv_alerts topic."""
    return client.publish(
        TopicArn=topic_arn,
        Subject=payload["Subject"][:99],  # SNS hard limit is 100 chars
        Message=payload["Message"],
    )


def lambda_handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    """Entry point — invoked by SNS publish from AWS Budgets."""
    if not EC2_INSTANCE_ID:
        logger.error("EC2_INSTANCE_ID env var is empty — cannot stop instance")
        return {"ok": False, "reason": "missing EC2_INSTANCE_ID"}
    if not ALERTS_TOPIC_ARN:
        logger.error("ALERTS_TOPIC_ARN env var is empty — cannot send operator alert")
        return {"ok": False, "reason": "missing ALERTS_TOPIC_ARN"}

    # Lazy-import boto3 so pure-function tests run without it.
    import boto3  # noqa: PLC0415

    ec2 = boto3.client("ec2")
    sns = boto3.client("sns")

    budget_context = _extract_budget_message(event)
    logger.info("budget kill triggered: %s", budget_context[:200])

    try:
        stop_result = _stop_instance(ec2, EC2_INSTANCE_ID)
        logger.info("ec2 stop result: %s", json.dumps(stop_result, default=str)[:500])
    except Exception:  # noqa: BLE001
        logger.exception("ec2:StopInstances failed — instance may still be running")
        # Re-raise so SNS retries per default policy AND the Lambda's
        # own Errors metric increments (alarm in TF wakes operator
        # via tv_alerts).
        raise

    try:
        publish_result = _publish_alert(
            sns,
            ALERTS_TOPIC_ARN,
            _format_alert_payload(EC2_INSTANCE_ID, stop_result, budget_context),
        )
        logger.info("operator alert published: %s", publish_result.get("MessageId"))
    except Exception:  # noqa: BLE001
        # If the alert publish fails, the EC2 is already stopped (good!),
        # but the operator may not know. The Lambda self-error alarm in
        # TF catches this — it ALSO publishes to tv_alerts, so the
        # operator gets paged via the same pipe.
        logger.exception("sns:Publish to operator alerts topic failed")
        raise

    return {
        "ok": True,
        "instance_id": EC2_INSTANCE_ID,
        "stop_state_changes": stop_result.get("StoppingInstances", []),
    }
