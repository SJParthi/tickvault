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
        Evaluated FIRST — the breach stop is NEVER gated on ping state.
      - Otherwise (2026-07-09 operator escalation — Telegram noise N2):
        the running ping fires only on CHANGE, never as an identical
        hourly repeat. State = ONE SSM String param (PING_STATE_PARAM,
        JSON {"month","bucket","outcome"}); ping reasons:
          * ping_state_missing   — no/corrupt state (fail-open: one ping
                                   beats a silent guard; also the first run)
          * ping_new_month       — calendar month rolled over
          * ping_cost_unknown_edge — Cost Explorer failed (rising edge
                                   only; latched repeats stay silent)
          * ping_cost_recovered  — the cost check recovered (falling edge)
          * ping_bucket_changed  — spend crossed a 10%-of-stop-budget
                                   bucket boundary (any increase; a
                                   DECREASE only when >= 2 buckets down —
                                   Cost Explorer estimates are revised
                                   both ways, so a 1-bucket dip at a
                                   boundary is hysteresis-suppressed to
                                   keep hourly flapping out of Telegram)
        The 17:30 IST daily digest (budget-guards.tf) remains the
        end-of-day summary and is untouched.

Environment variables (set by Terraform):
  INSTANCE_ID      — the tv-app EC2 instance
  ALERTS_TOPIC_ARN — operator's tv_alerts SNS topic (Telegram fan-out)
  START_RULE_NAME  — the tv-<env>-daily-start EventBridge rule to disable
  BUDGET_KILL_USD  — the kill line (default 55). KEEP IN SYNC with
                     budget.tf limit_amount AND BUDGET_USD in the daily
                     digest (budget-guards.tf) — the three MUST agree.
  PING_STATE_PARAM — SSM String param holding the change-only ping state
                     (default /tickvault/prod/budget-guard/ping-state).

No pip deps — boto3 is pre-installed in the AWS Lambda Python 3.12
runtime. boto3 is lazily imported so unit tests run without AWS creds.
"""

from __future__ import annotations

import json
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
# Change-only ping state (2026-07-09) — ONE SSM String param, JSON
# {"month": "YYYY-MM", "bucket": int, "outcome": str}. Created lazily by
# PutParameter(Overwrite=True); NOT under the banned groww/* namespace.
PING_STATE_PARAM = os.environ.get(
    "PING_STATE_PARAM", "/tickvault/prod/budget-guard/ping-state"
)
# Spend buckets are 10%-of-stop-budget steps; bucket 8 (80%) starts the
# "approaching stop-budget" subject flavor, bucket 9+ is the red flavor.
BUCKET_PCT_STEP = 10
APPROACH_BUCKET = 8


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


def spend_bucket(mtd: float | None, budget_kill_usd: float) -> int:
    """Pure: which 10%-of-stop-budget bucket the MTD spend sits in.

    Total — never raises: unknown/negative spend or a non-positive kill
    line clamps to bucket 0; the percentage is capped so a runaway spend
    cannot overflow anything.
    """
    try:
        if mtd is None or mtd < 0 or budget_kill_usd <= 0:
            return 0
        pct = (mtd / budget_kill_usd) * 100.0
        return int(min(pct, 999.0) // BUCKET_PCT_STEP)
    except Exception:  # noqa: BLE001 — pure classifier must never raise
        return 0


def classify_ping_decision(
    prev: Any,
    action: str,
    mtd: float | None,
    budget_kill_usd: float,
    month: str,
) -> tuple[str, dict[str, Any]]:
    """Pure change-only ping decision (2026-07-09 — Telegram noise N2).

    Returns (reason, new_state). reason is one of:
      ping_state_missing / ping_new_month / ping_cost_unknown_edge /
      ping_cost_recovered / ping_bucket_changed / silent.
    Fail-open by design: a missing or corrupt prev state PINGS (one ping
    beats a silent guard). The breach stop never reaches this function —
    the caller returns via _execute_breach_stop BEFORE any state read.
    """
    bucket = spend_bucket(mtd, budget_kill_usd)
    new_state: dict[str, Any] = {"month": month, "bucket": bucket, "outcome": action}
    # cost_unknown first: it has no trustworthy spend figure, so no other
    # reason may render one; edge-latched so repeats stay silent.
    if action == "cost_unknown":
        if isinstance(prev, dict) and prev.get("outcome") == "cost_unknown":
            return ("silent", new_state)
        return ("ping_cost_unknown_edge", new_state)
    if (
        not isinstance(prev, dict)
        or not isinstance(prev.get("month"), str)
        or not isinstance(prev.get("bucket"), int)
        or not isinstance(prev.get("outcome"), str)
    ):
        return ("ping_state_missing", new_state)
    if prev["month"] != month:
        return ("ping_new_month", new_state)
    if prev["outcome"] == "cost_unknown":
        return ("ping_cost_recovered", new_state)
    if bucket > prev["bucket"]:
        return ("ping_bucket_changed", new_state)
    if prev["bucket"] - bucket >= 2:
        # A >= 2-bucket decrease is a real mid-month credit/refund —
        # rare and genuinely informative.
        return ("ping_bucket_changed", new_state)
    if bucket < prev["bucket"]:
        # Hysteresis (hostile-review fix 2026-07-09): Cost Explorer
        # UnblendedCost estimates are revised BOTH ways, so spend sitting
        # on a 10% boundary can flip one bucket down and back up every
        # hour — exactly the N2 hourly-repeat class. A 1-bucket dip stays
        # SILENT and RETAINS the stored bucket, so the flip back up is
        # silent too.
        new_state["bucket"] = prev["bucket"]
        return ("silent", new_state)
    return ("silent", new_state)


def render_running_ping(
    reason: str,
    mtd: float | None,
    bucket: int,
    budget_kill_usd: float,
    hrs_up: float,
    instance_id: str,
) -> tuple[str, str]:
    """Pure: (subject, message) for a change-only running ping."""
    if reason == "ping_cost_unknown_edge":
        return (
            "[BUDGET] cost check failed — box left running",
            (
                "🟡 Could not read this month's spend (fail-safe: box NOT "
                "stopped, auto-start NOT disabled).\n"
                f"_instance_: `{instance_id}` — up {hrs_up:.1f}h this boot.\n"
                "Will report again when the cost check recovers.\n"
            ),
        )
    mtd_val = mtd if mtd is not None else 0.0
    pct = (mtd_val / budget_kill_usd) * 100.0 if budget_kill_usd > 0 else 0.0
    spend_line = (
        f"Month spend ~${mtd_val:.2f} of ${budget_kill_usd:.0f} "
        f"stop-budget ({pct:.0f}%)."
    )
    if reason == "ping_cost_recovered":
        return (
            "[BUDGET] cost check recovered",
            (
                f"🟢 Cost check is working again. {spend_line}\n"
                f"_instance_: `{instance_id}` — up {hrs_up:.1f}h this boot.\n"
                "Next update only when spend crosses the next 10% step.\n"
            ),
        )
    if reason == "ping_new_month":
        subject = "[BUDGET] new month — spend counter reset"
    elif reason == "ping_state_missing":
        subject = "[BUDGET] box running — first spend status"
    elif bucket > APPROACH_BUCKET:
        subject = "[BUDGET] approaching stop-budget — 90% used"
    elif bucket == APPROACH_BUCKET:
        subject = "[BUDGET] approaching stop-budget — 80% used"
    else:
        subject = f"[BUDGET] spend crossed {bucket * BUCKET_PCT_STEP}% of stop-budget"
    if bucket >= APPROACH_BUCKET:
        tail = (
            f"At ${budget_kill_usd:.0f} the box STOPS and the morning "
            "auto-start is DISABLED. If this month's spend is expected, "
            "raise the budget before it trips.\n"
        )
        emoji = "🔴" if bucket > APPROACH_BUCKET else "⚠️"
    else:
        tail = (
            "Box is in the trading window and left running. Next update "
            "only when spend crosses the next 10% step.\n"
        )
        emoji = "🟡"
    return (
        subject,
        (
            f"{emoji} {spend_line}\n"
            f"_instance_: `{instance_id}` — up {hrs_up:.1f}h this boot.\n"
            f"{tail}"
        ),
    )


def load_ping_state(ssm_client: Any) -> dict[str, Any] | None:
    """Best-effort SSM state read — ANY error returns None (fail-open:
    the classifier then pings; one ping beats a silent guard)."""
    try:
        r = ssm_client.get_parameter(Name=PING_STATE_PARAM)
        value = json.loads(r["Parameter"]["Value"])
        return value if isinstance(value, dict) else None
    except Exception as e:  # noqa: BLE001 — state read must never raise
        logger.warning("ping-state read failed (fail-open to ping): %s", e)
        return None


def save_ping_state(ssm_client: Any, state: dict[str, Any]) -> bool:
    """Best-effort SSM state write — a failed write is logged and the
    handler still returns ok (worst case: one duplicate ping/hour until
    the write lands — duplicate-over-silent)."""
    try:
        ssm_client.put_parameter(
            Name=PING_STATE_PARAM,
            Value=json.dumps(state),
            Type="String",
            Overwrite=True,
        )
        return True
    except Exception as e:  # noqa: BLE001 — state write must never raise
        logger.warning("ping-state write failed (next hour may re-ping): %s", e)
        return False


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
    ssm_client: Any = None,
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
        # morning restart. The breach stop is evaluated BEFORE any ping
        # state read — it can NEVER be gated on ping-state problems.
        usd = mtd_usd(ce_client)
        action = classify_in_window_action(usd, BUDGET_KILL_USD)
        if action == "breach_stop":
            return _execute_breach_stop(ec2, sns, events_client, usd, state)

        # 2026-07-09 (operator escalation — Telegram noise N2): the running
        # ping is CHANGE-ONLY — never an identical hourly repeat. The
        # 17:30 IST daily digest remains the end-of-day summary.
        if ssm_client is None:
            import boto3  # noqa: PLC0415 — lazy so tests run without creds

            ssm_client = boto3.client("ssm")
        launch = inst.get("LaunchTime")
        hrs_up = (now - launch).total_seconds() / 3600 if launch else 0.0
        prev = load_ping_state(ssm_client)
        month = now.strftime("%Y-%m")
        reason, new_state = classify_ping_decision(
            prev, action, usd, BUDGET_KILL_USD, month
        )
        state_saved = True
        if reason != "silent":
            subject, message = render_running_ping(
                reason, usd, new_state["bucket"], BUDGET_KILL_USD, hrs_up, INSTANCE_ID
            )
            sns.publish(
                TopicArn=ALERTS_TOPIC_ARN,
                Subject=subject[:99],
                Message=message,
            )
            # Save only when something was said — a silent hour by
            # construction leaves the state semantically unchanged.
            state_saved = save_ping_state(ssm_client, new_state)
        logger.info(
            "in-window budget check: action=%s ping_reason=%s bucket=%s",
            action,
            reason,
            new_state["bucket"],
        )
        return {
            "ok": True,
            "noop": True,
            "in_window": True,
            "state": state,
            "budget_action": action,
            "ping_reason": reason,
            "state_saved": state_saved,
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
