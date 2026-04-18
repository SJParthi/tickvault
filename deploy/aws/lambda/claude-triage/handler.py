"""Claude-triage Lambda — Phase 8.2 of .claude/plans/active-plan.md.

Receives CloudWatch alarm events via SNS, extracts alarm context, and
invokes a Claude Code session on the tickvault EC2 instance via SSM
RunCommand. The Claude session reads the alarm details, applies the
triage rules YAML, and either auto-fixes, silences, or escalates.

No pip deps — boto3 is pre-installed in the AWS Lambda Python runtime.

Invocation: SNS -> Lambda (this) -> SSM RunCommand -> claude CLI on EC2.

Environment variables (set by Terraform):
  EC2_INSTANCE_ID           — target instance for claude triage session
  TRIAGE_COMMAND_TIMEOUT    — max seconds for the SSM command (default 180)
  CLAUDE_BINARY_PATH        — path to claude CLI on the instance (default /usr/local/bin/claude)
  TV_LOG_GROUP              — CloudWatch log group to tail for context
"""

from __future__ import annotations

import json
import logging
import os
from typing import Any

import boto3

logger = logging.getLogger()
logger.setLevel(logging.INFO)

EC2_INSTANCE_ID = os.environ.get("EC2_INSTANCE_ID", "")
TRIAGE_COMMAND_TIMEOUT = int(os.environ.get("TRIAGE_COMMAND_TIMEOUT", "180"))
CLAUDE_BINARY_PATH = os.environ.get("CLAUDE_BINARY_PATH", "/usr/local/bin/claude")
TV_LOG_GROUP = os.environ.get("TV_LOG_GROUP", "/tickvault/prod/app")


def _parse_sns_alarm(event: dict[str, Any]) -> dict[str, Any] | None:
    """Extract CloudWatch alarm fields from an SNS event envelope."""
    records = event.get("Records") or []
    if not records:
        return None
    sns = records[0].get("Sns") or {}
    message = sns.get("Message")
    if not message:
        return None
    try:
        alarm = json.loads(message)
    except (TypeError, json.JSONDecodeError):
        return None
    return {
        "alarm_name": alarm.get("AlarmName"),
        "new_state": alarm.get("NewStateValue"),
        "reason": alarm.get("NewStateReason"),
        "metric": (
            alarm.get("Trigger", {}).get("MetricName")
            if isinstance(alarm.get("Trigger"), dict)
            else None
        ),
        "threshold": (
            alarm.get("Trigger", {}).get("Threshold")
            if isinstance(alarm.get("Trigger"), dict)
            else None
        ),
        "region": alarm.get("Region") or alarm.get("AWSAccountId"),
        "timestamp": alarm.get("StateChangeTime"),
    }


def _build_claude_prompt(alarm: dict[str, Any]) -> str:
    """Construct the triage prompt for the Claude Code session."""
    return (
        f"CloudWatch alarm `{alarm.get('alarm_name')}` fired with state "
        f"`{alarm.get('new_state')}` at {alarm.get('timestamp')}. "
        f"Reason: {alarm.get('reason')}. "
        f"Metric: {alarm.get('metric')}, threshold: {alarm.get('threshold')}. "
        f"Read `.claude/triage/claude-loop-prompt.md` and apply the triage "
        f"rules to `data/logs/errors.summary.md`. Escalate novel signatures "
        f"by opening a draft GitHub issue; silence known recurring codes; "
        f"run auto-fix scripts only for rules with confidence >= 0.95. "
        f"Never auto-action Critical severity."
    )


def _invoke_claude_via_ssm(prompt: str) -> dict[str, Any]:
    """Run `claude triage ...` on the EC2 instance via SSM RunCommand."""
    if not EC2_INSTANCE_ID:
        raise RuntimeError(
            "EC2_INSTANCE_ID env var not set — Lambda cannot target an instance"
        )
    ssm = boto3.client("ssm")
    response = ssm.send_command(
        InstanceIds=[EC2_INSTANCE_ID],
        DocumentName="AWS-RunShellScript",
        TimeoutSeconds=TRIAGE_COMMAND_TIMEOUT,
        Parameters={
            "commands": [
                # The operator's EC2 must have a systemd-user tmux session
                # named `claude-triage` pre-created. The Lambda attaches
                # the prompt as a tmux send-keys — lets the claude CLI
                # stay long-running between invocations.
                f"tmux send-keys -t claude-triage {json.dumps(prompt)} Enter"
            ],
        },
        CloudWatchOutputConfig={
            "CloudWatchLogGroupName": TV_LOG_GROUP,
            "CloudWatchOutputEnabled": True,
        },
    )
    return {
        "command_id": response["Command"]["CommandId"],
        "status": response["Command"]["Status"],
    }


def lambda_handler(event: dict[str, Any], context: Any) -> dict[str, Any]:
    """Entry point. Returns JSON for CloudWatch Logs."""
    logger.info("claude-triage received event", extra={"event_size": len(str(event))})

    alarm = _parse_sns_alarm(event)
    if alarm is None:
        logger.warning("event did not contain an SNS alarm payload")
        return {"status": "skipped", "reason": "no SNS alarm payload"}

    logger.info("parsed alarm %s", alarm.get("alarm_name"))

    try:
        prompt = _build_claude_prompt(alarm)
        ssm_result = _invoke_claude_via_ssm(prompt)
        logger.info(
            "claude triage command dispatched command_id=%s status=%s",
            ssm_result.get("command_id"),
            ssm_result.get("status"),
        )
        return {
            "status": "dispatched",
            "alarm": alarm,
            "ssm": ssm_result,
        }
    except Exception as err:  # noqa: BLE001
        logger.exception("claude-triage dispatch failed")
        return {
            "status": "failed",
            "alarm": alarm,
            "error": str(err),
        }
