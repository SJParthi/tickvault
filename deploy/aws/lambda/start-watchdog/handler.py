"""Instance start watchdog Lambda — answers "who monitors the 08:00 start?".

Two EventBridge schedules invoke this Lambda with a `mode` input:

  * mode="ping"  @ 08:00 IST (02:30 UTC Mon-Fri) — fires WITH the daily_start
    EventBridge rule. Publishes a positive "start triggered" Telegram so the
    operator sees the morning kick-off even before the app boots (~3 min later).

  * mode="check" @ 08:15 IST (02:45 UTC Mon-Fri) — 15 min after the start was
    triggered. Calls ec2:DescribeInstances; if the box is NOT "running" it
    publishes a Severity::Critical Telegram ("08:00 auto-start FAILED"). This is
    the AWS-native answer to the 2026-06-02 incident, where the EventBridge ->
    SSM-Automation start silently failed and NOTHING alerted the operator until
    he noticed by hand. Because this Lambda runs IN AWS (not on a GitHub Actions
    runner like aws-autopilot), it alerts even if the box — or GitHub — is dead.

On a healthy "check" (box running) it stays SILENT — no spam.

Publishes to the operator's `tv_alerts` SNS topic with Subject + Message, the
same shape the telegram-webhook Lambda (PR #781) already renders.

Environment variables (set by Terraform):
  EC2_INSTANCE_ID    — the tv-app instance to watch
  ALERTS_TOPIC_ARN   — operator's tv_alerts SNS topic for Telegram
  LOG_LEVEL          — INFO (default) / DEBUG / WARNING

No pip deps — boto3 is pre-installed in the AWS Lambda Python 3.12 runtime.
"""

from __future__ import annotations

import logging
import os
from typing import Any

import boto3

LOG_LEVEL = os.environ.get("LOG_LEVEL", "INFO")
logger = logging.getLogger()
logger.setLevel(LOG_LEVEL)

EC2_INSTANCE_ID = os.environ.get("EC2_INSTANCE_ID", "")
ALERTS_TOPIC_ARN = os.environ.get("ALERTS_TOPIC_ARN", "")


def _instance_state(ec2_client: Any, instance_id: str) -> str:
    """Return the EC2 instance lifecycle state, or 'unknown' on any error.

    Never raises — a watchdog that crashes is worse than one that reports
    'unknown' (which itself is not 'running' and therefore alerts).
    """
    try:
        resp = ec2_client.describe_instances(InstanceIds=[instance_id])
        reservations = resp.get("Reservations", [])
        if not reservations:
            return "not-found"
        instances = reservations[0].get("Instances", [])
        if not instances:
            return "not-found"
        return instances[0].get("State", {}).get("Name", "unknown")
    except Exception as exc:  # noqa: BLE001 — watchdog must never crash
        logger.error("describe_instances failed: %s", exc)
        return "unknown"


def _publish(sns_client: Any, subject: str, message: str) -> None:
    if not ALERTS_TOPIC_ARN:
        logger.warning("ALERTS_TOPIC_ARN unset — cannot publish: %s", subject)
        return
    sns_client.publish(TopicArn=ALERTS_TOPIC_ARN, Subject=subject, Message=message)


def lambda_handler(event: dict[str, Any], _context: Any = None) -> dict[str, Any]:
    """Entry point. `event["mode"]` selects ping vs check behaviour."""
    mode = (event or {}).get("mode", "check")
    logger.info("start-watchdog invoked mode=%s instance=%s", mode, EC2_INSTANCE_ID)

    sns_client = boto3.client("sns")

    if mode == "ping":
        _publish(
            sns_client,
            "Instance start triggered",
            "🟢 08:00 IST instance start triggered (Mon-Fri). "
            "The app should report live in ~3 min. "
            "If you do NOT also get a 'tickvault started' message by 08:10, check the box.",
        )
        return {"mode": "ping", "published": True}

    # mode == "check" (default): verify the box actually started.
    ec2_client = boto3.client("ec2")
    state = _instance_state(ec2_client, EC2_INSTANCE_ID)
    if state == "running":
        logger.info("check OK — instance is running; staying silent")
        return {"mode": "check", "state": state, "alerted": False}

    _publish(
        sns_client,
        "08:00 auto-start FAILED",
        f"🆘 The 08:00 IST auto-start did NOT bring the box up — it is '{state}' at "
        f"08:15 IST. Market opens 09:15. Start it NOW: portal 'Start instance' or "
        f"`aws ec2 start-instances --instance-ids {EC2_INSTANCE_ID} --region ap-south-1`.",
    )
    logger.error("check FAILED — instance state=%s — operator paged", state)
    return {"mode": "check", "state": state, "alerted": True}
