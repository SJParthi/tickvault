"""Instance start watchdog Lambda — answers "who monitors the 08:30 start?".

Two EventBridge schedules invoke this Lambda with a `mode` input:

  * mode="ping"  @ 08:30 IST (03:00 UTC Mon-Fri) — fires WITH the daily_start
    EventBridge rule. Publishes a positive "start triggered" Telegram so the
    operator sees the morning kick-off even before the app boots (~3 min later).

  * mode="check" @ 08:45 IST (03:15 UTC Mon-Fri) — 15 min after the start was
    triggered. Calls ec2:DescribeInstances and:
      - box NOT running  -> SELF-HEALS: issues ec2:StartInstances itself, then
        publishes a Severity::Critical Telegram saying the 08:30 auto-start
        failed and the watchdog started the box. (2026-06-10 incident: the
        EventBridge -> SSM-Automation start silently failed AGAIN — repeat of
        2026-06-02 — and the operator had to start the box by hand at 08:43.
        Detection alone proved insufficient; the watchdog now fixes first,
        pages second.)
      - box running but its LaunchTime is AFTER the 08:30 trigger + grace ->
        the auto-start failed and something/someone started it late (exactly
        the 2026-06-10 masking case, where a manual 08:43 start beat the 08:45
        check and the failure went unflagged). Publishes a warning so the
        broken start path is investigated instead of silently rotting.
      - box running on time -> SILENT. No spam.

Because this Lambda runs IN AWS (not on a GitHub Actions runner like
aws-autopilot), it detects and heals even if the box — or GitHub — is dead.

Publishes to the operator's `tv_alerts` SNS topic with Subject + Message, the
same shape the telegram-webhook Lambda (PR #781) already renders.

Environment variables (set by Terraform):
  EC2_INSTANCE_ID    — the tv-app instance to watch
  ALERTS_TOPIC_ARN   — operator's tv_alerts SNS topic for Telegram
  DASHBOARD_PORT     — app port for the Feed Control page link (default 3001)
  LOG_LEVEL          — INFO (default) / DEBUG / WARNING

No pip deps — boto3 is pre-installed in the AWS Lambda Python 3.12 runtime.
"""

from __future__ import annotations

import logging
import os
from datetime import datetime, timedelta, timezone
from typing import Any

import boto3

LOG_LEVEL = os.environ.get("LOG_LEVEL", "INFO")
logger = logging.getLogger()
logger.setLevel(LOG_LEVEL)

EC2_INSTANCE_ID = os.environ.get("EC2_INSTANCE_ID", "")
ALERTS_TOPIC_ARN = os.environ.get("ALERTS_TOPIC_ARN", "")
# Port the app serves its Feed Control page on (config/base.toml [api] port).
DASHBOARD_PORT = os.environ.get("DASHBOARD_PORT", "3001")

# The daily_start EventBridge rule fires at 03:00 UTC (= 08:30 IST, Mon-Fri).
START_TRIGGER_UTC_HOUR = 3
START_TRIGGER_UTC_MINUTE = 0
# The daily_stop EventBridge rule fires at 11:00 UTC (= 16:30 IST, Mon-Fri).
STOP_TRIGGER_UTC_HOUR = 11
STOP_TRIGGER_UTC_MINUTE = 0
# EC2 start -> running takes <1 min; anything launched more than this many
# minutes after the trigger means the scheduled start did NOT do it.
LATE_START_GRACE_MINUTES = 5
IST_OFFSET = timedelta(hours=5, minutes=30)


def _now() -> datetime:
    """Wall clock, UTC. Separate fn so tests can monkeypatch it."""
    return datetime.now(timezone.utc)


def _instance_info(ec2_client: Any, instance_id: str) -> tuple[str, datetime | None]:
    """Return (lifecycle state, launch time) — ('unknown', None) on any error.

    Never raises — a watchdog that crashes is worse than one that reports
    'unknown' (which itself is not 'running' and therefore alerts).
    """
    try:
        resp = ec2_client.describe_instances(InstanceIds=[instance_id])
        reservations = resp.get("Reservations", [])
        if not reservations:
            return "not-found", None
        instances = reservations[0].get("Instances", [])
        if not instances:
            return "not-found", None
        state = instances[0].get("State", {}).get("Name", "unknown")
        return state, instances[0].get("LaunchTime")
    except Exception as exc:  # noqa: BLE001 — watchdog must never crash
        logger.error("describe_instances failed: %s", exc)
        return "unknown", None


def _instance_public_ip(ec2_client: Any, instance_id: str) -> str | None:
    """Return the instance's CURRENT public IP, or None if unavailable.

    The EIP is detached for the no-orders data-pull phase, so the public IP
    is auto-assigned and changes on every start — we read it live here.
    Never raises: a missing link must never stop the start-triggered message.
    """
    try:
        resp = ec2_client.describe_instances(InstanceIds=[instance_id])
        reservations = resp.get("Reservations", [])
        if not reservations:
            return None
        instances = reservations[0].get("Instances", [])
        if not instances:
            return None
        ip = instances[0].get("PublicIpAddress")
        return ip or None
    except Exception as exc:  # noqa: BLE001 — watchdog must never crash
        logger.error("describe_instances for public IP failed: %s", exc)
        return None


def _try_self_start(ec2_client: Any, instance_id: str) -> bool:
    """Issue ec2:StartInstances. Returns True if the call was accepted.

    Never raises — if the self-heal fails, the page tells the operator to
    start the box manually instead.
    """
    try:
        ec2_client.start_instances(InstanceIds=[instance_id])
        logger.info("self-heal start_instances issued for %s", instance_id)
        return True
    except Exception as exc:  # noqa: BLE001 — watchdog must never crash
        logger.error("self-heal start_instances FAILED: %s", exc)
        return False


def _try_self_stop(ec2_client: Any, instance_id: str) -> bool:
    """Issue ec2:StopInstances. Returns True if the call was accepted.

    Never raises — if the self-heal fails, the page tells the operator to
    stop the box manually instead.
    """
    try:
        ec2_client.stop_instances(InstanceIds=[instance_id])
        logger.info("self-heal stop_instances issued for %s", instance_id)
        return True
    except Exception as exc:  # noqa: BLE001 — watchdog must never crash
        logger.error("self-heal stop_instances FAILED: %s", exc)
        return False


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
        message = (
            "🟢 8:30 AM IST instance start triggered (Mon-Fri). "
            "The app should report live in ~3 minutes. "
            "If you do NOT get a 'tickvault started' message by 8:40 AM, the "
            "8:45 AM watchdog will check and start the box itself if needed."
        )
        # Append a tappable Feed Control link so the operator can open the
        # page from his phone. The IP changes each start (EIP detached), so
        # we read it live; if it isn't ready yet, degrade gracefully — the
        # start-triggered message must always send.
        public_ip = _instance_public_ip(boto3.client("ec2"), EC2_INSTANCE_ID)
        if public_ip:
            logger.info("ping — resolved public IP %s for dashboard link", public_ip)
            message += f"\n📊 Dashboard: http://{public_ip}:{DASHBOARD_PORT}/feeds"
        else:
            logger.info("ping — public IP not ready; sending without dashboard link")
            message += "\n📊 Dashboard link not ready yet — try again in a minute."
        _publish(sns_client, "Instance start triggered", message)
        return {"mode": "ping", "published": True}

    if mode == "stop_check":
        # @ 16:45 IST (11:15 UTC Mon-Fri): the daily_stop EventBridge rule
        # fired at 16:30 IST — verify the box actually stopped. 2026-06-10
        # incident: BOTH the 08:30 start AND the 16:30 stop silently failed
        # the same day (EventBridge -> SSM breakage is systemic, not
        # transient). Self-heal rule: stop ONLY a box whose LaunchTime is
        # BEFORE today's 16:30 trigger (i.e. left running by the failed
        # cron). A box launched AFTER 16:30 is an operator's deliberate
        # manual evening session ("whenever manually i need to run the
        # instance i will do it") and is NEVER touched.
        ec2_client = boto3.client("ec2")
        state, launch_time = _instance_info(ec2_client, EC2_INSTANCE_ID)
        if state != "running":
            logger.info("stop_check OK — instance is '%s'; staying silent", state)
            return {"mode": "stop_check", "state": state, "alerted": False}
        now = _now()
        stop_trigger = now.replace(
            hour=STOP_TRIGGER_UTC_HOUR,
            minute=STOP_TRIGGER_UTC_MINUTE,
            second=0,
            microsecond=0,
        )
        if launch_time is not None and launch_time >= stop_trigger:
            logger.info(
                "stop_check — running but launched %s (after 16:30 IST): manual "
                "evening session, leaving it alone",
                launch_time,
            )
            return {"mode": "stop_check", "state": state, "alerted": False}
        if launch_time is None:
            # Can't prove it isn't a manual session — page, don't stop.
            _publish(
                sns_client,
                "16:30 auto-stop FAILED — box still running",
                "🆘 The 4:30 PM IST auto-stop did NOT stop the box and I could "
                "not read its launch time, so I won't risk killing a manual "
                "session. If you are NOT using it right now, press "
                "'Stop instance' on the portal — it bills every hour it runs.",
            )
            logger.error("stop_check FAILED — running, launch_time unknown — paged")
            return {"mode": "stop_check", "state": state, "alerted": True, "self_stopped": False}
        self_stopped = _try_self_stop(ec2_client, EC2_INSTANCE_ID)
        if self_stopped:
            _publish(
                sns_client,
                "16:30 auto-stop FAILED — watchdog stopped the box itself",
                "🆘 The 4:30 PM IST auto-stop did NOT stop the box (it had been "
                "running since before 4:30 PM). I have sent the stop command "
                "myself. If you WANTED it running this evening, just press "
                "'Start instance' on the portal — a box you start manually "
                "after 4:30 PM is never auto-stopped. Also investigate why the "
                "4:30 PM stop failed: AWS console → Systems Manager → "
                "Automation executions for AWS-StopEC2Instance.",
            )
        else:
            _publish(
                sns_client,
                "16:30 auto-stop FAILED — manual stop NEEDED",
                f"🆘 The 4:30 PM IST auto-stop did NOT stop the box — and the "
                "watchdog's own stop attempt ALSO failed. Press 'Stop instance' "
                "on the portal, or run "
                f"`aws ec2 stop-instances --instance-ids {EC2_INSTANCE_ID} "
                "--region ap-south-1`. It bills every hour it runs.",
            )
        logger.error(
            "stop_check FAILED — instance running past stop, self_stopped=%s — paged",
            self_stopped,
        )
        return {"mode": "stop_check", "state": state, "alerted": True, "self_stopped": self_stopped}

    # mode == "check" (default): verify the box actually started — and fix it.
    ec2_client = boto3.client("ec2")
    state, launch_time = _instance_info(ec2_client, EC2_INSTANCE_ID)

    if state == "running":
        # Late-start detection: only meaningful inside the scheduled check
        # window (03:00-04:00 UTC). A manual `mode=check` at another hour
        # must not misread an afternoon manual start as "late".
        now = _now()
        in_window = now.hour == START_TRIGGER_UTC_HOUR
        deadline = now.replace(
            hour=START_TRIGGER_UTC_HOUR,
            minute=START_TRIGGER_UTC_MINUTE,
            second=0,
            microsecond=0,
        ) + timedelta(minutes=LATE_START_GRACE_MINUTES)
        if in_window and launch_time is not None and launch_time > deadline:
            launched_ist = (launch_time + IST_OFFSET).strftime("%I:%M %p").lstrip("0")
            _publish(
                sns_client,
                "08:30 auto-start FAILED — box was started late",
                f"⚠️ The box is running now, but it only came up at {launched_ist} IST — "
                "the scheduled 8:30 AM start did NOT work and something (or someone) "
                "started it later. Trading is OK for today. Investigate why the 8:30 "
                "start failed: AWS console → Systems Manager → Automation executions "
                "for AWS-StartEC2Instance, and the EventBridge rule's FailedInvocations.",
            )
            logger.error("check — running but launched LATE at %s — paged", launch_time)
            return {"mode": "check", "state": state, "alerted": True, "self_started": False}
        logger.info("check OK — instance is running; staying silent")
        return {"mode": "check", "state": state, "alerted": False, "self_started": False}

    # Box is NOT running at 08:45 IST — fix first, page second.
    self_started = _try_self_start(ec2_client, EC2_INSTANCE_ID)
    if self_started:
        _publish(
            sns_client,
            "08:30 auto-start FAILED — watchdog started the box itself",
            f"🆘 The 8:30 AM IST auto-start did NOT bring the box up — it was "
            f"'{state}' at 8:45 AM. I have sent the start command myself; expect the "
            "'tickvault started' message by about 8:52 AM. Market opens 9:15 AM. "
            "If no started-message arrives by 8:55 AM, press 'Start instance' on the "
            "portal. Also investigate why the 8:30 start failed: AWS console → "
            "Systems Manager → Automation executions for AWS-StartEC2Instance.",
        )
    else:
        _publish(
            sns_client,
            "08:30 auto-start FAILED — manual start NEEDED NOW",
            f"🆘 The 8:30 AM IST auto-start did NOT bring the box up — it is "
            f"'{state}' at 8:45 AM — and the watchdog's own start attempt ALSO "
            "failed. Market opens 9:15 AM. Start it NOW: portal 'Start instance' or "
            f"`aws ec2 start-instances --instance-ids {EC2_INSTANCE_ID} "
            "--region ap-south-1`.",
        )
    logger.error(
        "check FAILED — instance state=%s self_started=%s — operator paged",
        state,
        self_started,
    )
    return {"mode": "check", "state": state, "alerted": True, "self_started": self_started}
