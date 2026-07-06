"""08:45 IST market-open readiness pager (daily-universe §19).

STATE-check, not edge-check: asks 'is the box running AND has the app
reported boot-complete in the last 10 minutes?' and publishes DIRECTLY
to SNS on a bad answer. Cannot be defeated by weekend-latched ALARM
states (2026-07-06 incident: 94-min-late launch, zero pages).
Fail-toward-paging: an unverifiable morning pages a distinct warning.

Separation of duties: this Lambda PAGES; the start-watchdog (same 08:45
cron) HEALS (it holds ec2:StartInstances; this one deliberately does not).

Honest residual: the SNS publish path RAISES on failure so a broken page
feeds AWS/Lambda Errors -> the readiness-errors alarm -> tv_alerts. A
TOTAL SNS outage silences both legs - outside the envelope, stated here.

Environment variables (set by Terraform):
  EC2_INSTANCE_ID       - the tv-app instance to check
  ALERTS_TOPIC_ARN      - operator's tv_alerts SNS topic for Telegram
  METRIC_NAMESPACE      - Tickvault/Prod
  BOOT_METRIC_NAME      - tv_boot_completed
  METRIC_HOST           - tickvault-prod (the scrape host dimension)
  BOOT_LOOKBACK_MINUTES - boot-metric freshness window (default 10)
  LOG_LEVEL             - INFO (default) / DEBUG / WARNING

No pip deps - boto3 is pre-installed in the AWS Lambda Python 3.12 runtime.
"""

from __future__ import annotations

import logging
import os
from datetime import datetime, timedelta, timezone

import boto3

logger = logging.getLogger()
logger.setLevel(os.environ.get("LOG_LEVEL", "INFO"))

EC2_INSTANCE_ID = os.environ.get("EC2_INSTANCE_ID", "")
ALERTS_TOPIC_ARN = os.environ.get("ALERTS_TOPIC_ARN", "")
METRIC_NAMESPACE = os.environ.get("METRIC_NAMESPACE", "Tickvault/Prod")
BOOT_METRIC_NAME = os.environ.get("BOOT_METRIC_NAME", "tv_boot_completed")
METRIC_HOST = os.environ.get("METRIC_HOST", "tickvault-prod")
BOOT_LOOKBACK_MINUTES = int(os.environ.get("BOOT_LOOKBACK_MINUTES", "10"))

IST = timedelta(hours=5, minutes=30)
# Launch today >= 08:25 IST followed by stop == the NSE-holiday gate's
# deliberate self-stop (tickvault-holiday-gate.service). Only the holiday
# gate ever calls StopInstances at that hour; EC2 never self-stops on app
# failure, so a trading-day misread is theoretical (boot-heartbeat gate
# window 08:50-09:10 is the backstop).
HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES = 8 * 60 + 25

# Verdicts (pure-string constants so tests assert on them)
READY_SILENT = "ready-silent"
HOLIDAY_SILENT = "holiday-silent"
NOT_RUNNING_PAGE = "not-running-page"
NOT_BOOTED_PAGE = "not-booted-page"
VERIFY_FAILED_PAGE = "verify-failed-page"


def _ist_minutes_of_day(dt_utc: datetime) -> int:
    """Minutes since IST midnight for a UTC-aware datetime."""
    ist = dt_utc + IST
    return ist.hour * 60 + ist.minute


def _is_today_ist(dt_utc: datetime, now_utc: datetime) -> bool:
    """True iff dt_utc falls on the same IST calendar date as now_utc."""
    return (dt_utc + IST).date() == (now_utc + IST).date()


def classify_readiness(
    state: str,
    launch_time_utc: datetime | None,
    boot_seen: bool,
    probe_error: bool,
    now_utc: datetime,
) -> str:
    """Pure decision core - unit-tested with zero AWS.

    Fail TOWARD paging: cannot-verify != OK. A stopped/stopping box whose
    LaunchTime is TODAY at/after 08:25 IST is the holiday gate's deliberate
    self-stop (silent); everything else not-running pages.
    """
    if probe_error:
        return VERIFY_FAILED_PAGE  # fail TOWARD paging: cannot-verify != OK
    if state == "running":
        return READY_SILENT if boot_seen else NOT_BOOTED_PAGE
    if (
        state in ("stopping", "stopped")
        and launch_time_utc is not None
        and _is_today_ist(launch_time_utc, now_utc)
        and _ist_minutes_of_day(launch_time_utc) >= HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES
    ):
        return HOLIDAY_SILENT  # 08:30 start then self-stop = holiday gate
    return NOT_RUNNING_PAGE  # stopped-old / pending / shutting-down / unknown / not-found


# 10 Telegram commandments: plain English, emoji first, IST 12h, numbered
# action verbs, no file paths / library names.
SUBJECTS_AND_MESSAGES = {
    NOT_RUNNING_PAGE: (
        "🆘 8:45 AM: trading computer is NOT running",
        "🆘 At 8:45 AM the trading computer is not running. Market opens 9:15 AM.\n"
        "What you need to do RIGHT NOW:\n"
        "1. The 8:45 AM watchdog may be starting it - watch for its 'started' message by 8:55 AM.\n"
        "2. No message by 8:55 AM: press 'Start instance' on the portal yourself.\n"
        "3. After it starts, watch for the 'tickvault started' message within 5 minutes.",
    ),
    NOT_BOOTED_PAGE: (
        "🆘 8:45 AM: app NOT ready - market opens 9:15 AM",
        "🆘 The trading computer is ON but the app has NOT reported a finished boot in the last 10 minutes. Market opens 9:15 AM.\n"
        "What you need to do RIGHT NOW:\n"
        "1. Open the portal and check the app status.\n"
        "2. No 'tickvault started' message by 8:55 AM: restart the app from the portal.\n"
        "3. Still stuck at 9:05 AM: treat today as a no-trade morning until it is fixed.",
    ),
    VERIFY_FAILED_PAGE: (
        "⚠️ 8:45 AM readiness check could not verify the app",
        "⚠️ I could not confirm whether the trading app is ready for the 9:15 AM open (an AWS lookup failed). "
        "Treat this as NOT READY until proven otherwise: open the portal now - if the app shows healthy, ignore this.",
    ),
}


def _instance_state(ec2, instance_id: str) -> tuple[str, datetime | None, bool]:
    """Return (state name, LaunchTime, probe_error).

    Any exception (incl. not-found) -> ("unknown", None, True) - the caller's
    classify_readiness turns a probe error into the verify-failed page.
    """
    try:
        resp = ec2.describe_instances(InstanceIds=[instance_id])
        reservations = resp.get("Reservations", [])
        if not reservations or not reservations[0].get("Instances"):
            return "unknown", None, True
        inst = reservations[0]["Instances"][0]
        state = inst.get("State", {}).get("Name", "unknown")
        return state, inst.get("LaunchTime"), False
    except Exception:  # noqa: BLE001 - probe error routes to verify-failed page
        logger.exception("describe_instances failed")
        return "unknown", None, True


def _boot_metric_seen(cw, now_utc: datetime) -> tuple[bool, bool]:
    """Return (boot metric >= 1 in the lookback window, probe_error)."""
    try:
        resp = cw.get_metric_data(
            MetricDataQueries=[
                {
                    "Id": "boot",
                    "MetricStat": {
                        "Metric": {
                            "Namespace": METRIC_NAMESPACE,
                            "MetricName": BOOT_METRIC_NAME,
                            "Dimensions": [{"Name": "host", "Value": METRIC_HOST}],
                        },
                        "Period": 60,
                        "Stat": "Maximum",
                    },
                    "ReturnData": True,
                }
            ],
            StartTime=now_utc - timedelta(minutes=BOOT_LOOKBACK_MINUTES),
            EndTime=now_utc,
        )
        for result in resp.get("MetricDataResults", []):
            if any(value >= 1.0 for value in result.get("Values", [])):
                return True, False
        return False, False
    except Exception:  # noqa: BLE001 - probe error routes to verify-failed page
        logger.exception("get_metric_data failed")
        return False, True


def _publish(subject: str, message: str) -> None:
    """Publish to tv_alerts. On failure: log + RAISE.

    Deliberate inversion of the swallow-idiom used by the watchdogs: the
    invocation error feeds AWS/Lambda Errors -> the readiness-errors alarm ->
    tv_alerts. The pager must never die silently. Honest residual: a TOTAL
    SNS outage silences both legs - outside the envelope, stated here.
    """
    try:
        boto3.client("sns").publish(
            TopicArn=ALERTS_TOPIC_ARN, Subject=subject[:99], Message=message
        )
    except Exception:
        logger.exception("sns publish FAILED - raising so the Errors alarm pages")
        raise


def lambda_handler(event, _context=None):
    """Entry point - 08:45 IST EventBridge (mode=readiness) or manual invoke."""
    now_utc = datetime.now(timezone.utc)
    state, launch_time, ec2_err = _instance_state(boto3.client("ec2"), EC2_INSTANCE_ID)
    boot_seen, cw_err = (False, False)
    if state == "running" and not ec2_err:
        boot_seen, cw_err = _boot_metric_seen(boto3.client("cloudwatch"), now_utc)
    verdict = classify_readiness(state, launch_time, boot_seen, ec2_err or cw_err, now_utc)
    logger.info(
        "readiness verdict=%s state=%s launch=%s boot_seen=%s ec2_err=%s cw_err=%s",
        verdict,
        state,
        launch_time,
        boot_seen,
        ec2_err,
        cw_err,
    )
    if verdict in SUBJECTS_AND_MESSAGES:
        subject, message = SUBJECTS_AND_MESSAGES[verdict]
        _publish(subject, message)
    return {"verdict": verdict, "state": state, "boot_seen": boot_seen}
