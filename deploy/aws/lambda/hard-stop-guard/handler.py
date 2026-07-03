"""Hard auto-stop guard Lambda — hourly budget/schedule never-cross safety net.

Extracted 2026-07-03 from the inline heredoc in
deploy/aws/terraform/budget-guards.tf so the budget-breach logic is
unit-testable (GAP 1 of the budget-net audit).

Behaviour (hourly EventBridge cron):
  * Instance already stopped/stopping -> no-op.
  * Running OUTSIDE the Mon-Fri 08:30-16:30 IST up-window -> force stop +
    Telegram (the legacy never-cross guard, unchanged).
  * Running INSIDE the up-window:
      - MTD spend >= BUDGET_KILL_USD -> stop the instance, DISABLE the
        tv-<env>-daily-start EventBridge rule, page the operator.
        This closes GAP 1 (post-breach morning restart): the native AWS
        Budget Actions fire only ONCE per month-crossing, so after a 100%
        kill the next morning's start cron would otherwise restart the box
        and run it daily, unkilled, for the rest of the month.
      - Cost Explorer error (MTD unknown) -> FAIL-SAFE: never stop or
        disable on missing data; keep the hourly "still running" ping and
        say the budget check failed.
      - Below budget -> hourly "still running" cost ping (unchanged).

Environment variables (set by Terraform):
  INSTANCE_ID      — the tv-app EC2 instance
  ALERTS_TOPIC_ARN — operator's tv_alerts SNS topic (Telegram fan-out)
  START_RULE_NAME  — the tv-<env>-daily-start EventBridge rule to disable
  BUDGET_KILL_USD  — the kill line (default 55). KEEP IN SYNC with
                     budget.tf limit_amount AND BUDGET_USD in the daily
                     digest (budget-guards.tf) — the three MUST agree.

No pip deps — boto3 is pre-installed in the AWS Lambda Python 3.12
runtime. boto3 is lazily imported so unit tests run without AWS creds.
"""

from __future__ import annotations

import logging
import os
from datetime import datetime, timedelta, timezone
from typing import Any

LOG_LEVEL = os.environ.get("LOG_LEVEL", "INFO")
logger = logging.getLogger()
logger.setLevel(LOG_LEVEL)

INSTANCE_ID = os.environ.get("INSTANCE_ID", "")
ALERTS_TOPIC_ARN = os.environ.get("ALERTS_TOPIC_ARN", "")
START_RULE_NAME = os.environ.get("START_RULE_NAME", "")
# The kill line in pre-GST USD, measured on total-account UnblendedCost —
# identical basis to the AWS Budget (budget.tf limit_amount = "55") so this
# guard and the native Budget Actions agree on "breached".
BUDGET_KILL_USD = float(os.environ.get("BUDGET_KILL_USD", "55"))


def in_up_window(now_utc: datetime) -> bool:
    """True inside the Mon-Fri 08:30-16:30 IST trading up-window.

    IST = UTC+5:30. The guard NEVER stops the box during the window on
    schedule grounds (it would kill a live market-hours session) — only a
    budget breach may stop it in-window.
    """
    ist = now_utc + timedelta(hours=5, minutes=30)
    dow = ist.weekday()  # 0=Mon .. 6=Sun
    hhmm = ist.hour * 100 + ist.minute
    return dow <= 4 and 830 <= hhmm <= 1630


def mtd_usd(ce_client: Any = None) -> float | None:
    """Best-effort Cost Explorer month-to-date total (us-east-1).

    Returns None on ANY error — the caller treats None as fail-safe
    ("cost_unknown": page, never stop or disable on missing data).
    """
    try:
        if ce_client is None:
            import boto3  # noqa: PLC0415 — lazy so tests run without creds

            ce_client = boto3.client("ce", region_name="us-east-1")
        today = datetime.now(timezone.utc).date()
        start = today.replace(day=1)
        r = ce_client.get_cost_and_usage(
            TimePeriod={"Start": str(start), "End": str(today)},
            Granularity="MONTHLY",
            Metrics=["UnblendedCost"],
        )
        return sum(
            float(d["Total"]["UnblendedCost"]["Amount"]) for d in r["ResultsByTime"]
        )
    except Exception as e:  # noqa: BLE001 — cost lookup must never raise
        logger.warning("MTD lookup failed (non-fatal): %s", e)
        return None


def classify_in_window_action(mtd: float | None, budget_kill_usd: float) -> str:
    """Pure decision for the in-window arm (GAP 1 fix).

    - "breach_stop":  MTD >= kill line -> stop + disable morning auto-start.
    - "cost_unknown": Cost Explorer failed -> FAIL-SAFE page, no stop, no
                      disable (never take irreversible action on missing data).
    - "below_budget": normal hourly running ping.
    """
    if mtd is None:
        return "cost_unknown"
    if mtd >= budget_kill_usd:
        return "breach_stop"
    return "below_budget"


def _execute_breach_stop(
    ec2: Any, sns: Any, events_client: Any, mtd: float, was_state: str
) -> dict[str, Any]:
    """Stop the box + disable the morning start rule + page (GAP 1).

    Ordering is deliberate: stop FIRST (caps spend even if the disable
    fails), then disable the start rule, then page — the page honestly
    reports whether the disable landed so the operator can finish by hand.
    """
    ec2.stop_instances(InstanceIds=[INSTANCE_ID])

    disable_ok = False
    disable_err = ""
    if START_RULE_NAME:
        try:
            events_client.disable_rule(Name=START_RULE_NAME)
            disable_ok = True
        except Exception as e:  # noqa: BLE001 — page-don't-crash
            disable_err = str(e)
            logger.exception("events:DisableRule failed for %s", START_RULE_NAME)
    else:
        disable_err = "START_RULE_NAME env var empty"

    if disable_ok:
        subject = "[BUDGET] breached — box stopped + auto-start disabled"
        disable_line = f"Morning auto-start rule `{START_RULE_NAME}` DISABLED."
    else:
        subject = "[BUDGET] breached — box stopped; auto-start disable FAILED"
        disable_line = (
            f"⚠ could NOT disable the morning auto-start ({disable_err}) — "
            f"disable rule `{START_RULE_NAME}` manually NOW or the box "
            "restarts at 8:30 AM tomorrow."
        )

    sns.publish(
        TopicArn=ALERTS_TOPIC_ARN,
        Subject=subject[:99],
        Message=(
            "🛑 *Budget breached — box stopped + morning auto-start DISABLED "
            "until operator re-enables*\n"
            f"_instance_: `{INSTANCE_ID}`\n"
            f"_was_state_: {was_state}\n"
            f"_MTD spend_: ~${mtd:.2f} >= ${BUDGET_KILL_USD:.0f} stop-budget\n"
            f"{disable_line}\n"
            "Why: the native AWS Budget stop-actions fire only ONCE per\n"
            "month-crossing. Without disabling the start rule, tomorrow's\n"
            "8:30 AM auto-start would restart the box and it would run\n"
            "every day for the rest of the month, unkilled.\n"
            "After investigating the spend, re-enable with:\n"
            f"  aws events enable-rule --name {START_RULE_NAME}\n"
        ),
    )
    return {
        "ok": True,
        "noop": False,
        "breach": True,
        "disable_ok": disable_ok,
        "was_state": was_state,
    }


def lambda_handler(
    event: dict[str, Any],
    _context: Any,
    ec2: Any = None,
    sns: Any = None,
    events_client: Any = None,
    ce_client: Any = None,
    now_utc: datetime | None = None,
) -> dict[str, Any]:
    """Entry point — hourly EventBridge cron.

    The optional client/clock parameters exist for unit tests; the Lambda
    runtime invokes with (event, context) only, so real boto3 clients are
    lazily constructed.
    """
    if not INSTANCE_ID:
        logger.error("INSTANCE_ID env var is empty — cannot guard instance")
        return {"ok": False, "reason": "missing INSTANCE_ID"}
    if not ALERTS_TOPIC_ARN:
        logger.error("ALERTS_TOPIC_ARN env var is empty — cannot page operator")
        return {"ok": False, "reason": "missing ALERTS_TOPIC_ARN"}

    if ec2 is None or sns is None or events_client is None:
        import boto3  # noqa: PLC0415 — lazy so tests run without creds

        ec2 = ec2 or boto3.client("ec2")
        sns = sns or boto3.client("sns")
        events_client = events_client or boto3.client("events")

    now = now_utc or datetime.now(timezone.utc)
    desc = ec2.describe_instances(InstanceIds=[INSTANCE_ID])
    inst = desc["Reservations"][0]["Instances"][0]
    state = inst["State"]["Name"]

    if state in ("stopped", "stopping", "shutting-down", "terminated"):
        logger.info("Instance %s already %s, no-op.", INSTANCE_ID, state)
        return {"ok": True, "noop": True, "state": state}

    if in_up_window(now):
        # Running DURING the trading window — expected. GAP 1 check first:
        # even in-window, a breached budget stops the box + kills the
        # morning restart. Otherwise hourly "still running" cost ping.
        usd = mtd_usd(ce_client)
        action = classify_in_window_action(usd, BUDGET_KILL_USD)
        if action == "breach_stop":
            return _execute_breach_stop(ec2, sns, events_client, usd, state)

        launch = inst.get("LaunchTime")
        hrs_up = (now - launch).total_seconds() / 3600 if launch else 0.0
        if action == "cost_unknown":
            usd_str = "n/a (cost check failed — fail-safe: NOT disabling auto-start)"
        else:
            usd_str = f"~${usd:.2f}"
        sns.publish(
            TopicArn=ALERTS_TOPIC_ARN,
            Subject="[BUDGET] box still running",
            Message=(
                "🟢 *Box running (in window)*\n"
                f"_instance_: `{INSTANCE_ID}`\n"
                f"_up_: {hrs_up:.1f}h this boot\n"
                f"_MTD spend_: {usd_str} / ${BUDGET_KILL_USD:.0f} stop-budget\n"
                "In the 08:30-16:30 IST trading window — left running.\n"
            ),
        )
        return {
            "ok": True,
            "noop": True,
            "in_window": True,
            "state": state,
            "budget_action": action,
        }

    # Running OUTSIDE the up-window — force stop + alert (the legacy
    # never-cross guard, unchanged by GAP 1).
    ec2.stop_instances(InstanceIds=[INSTANCE_ID])
    sns.publish(
        TopicArn=ALERTS_TOPIC_ARN,
        Subject="[BUDGET] hard auto-stop guard fired",
        Message=(
            "🔴 *Hard Auto-Stop Guard Fired*\n"
            f"_instance_: `{INSTANCE_ID}`\n"
            f"_was_state_: {state}\n"
            "Hourly out-of-window guard found the box running OUTSIDE the\n"
            "08:30-16:30 IST Mon-Fri window. EventBridge 16:30 stop (or a\n"
            "manual start) left it up. Now stopped to protect the budget.\n"
        ),
    )
    return {"ok": True, "noop": False, "was_state": state}
