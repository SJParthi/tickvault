"""Unit tests for the hard-stop-guard Lambda (GAP 1 — post-breach restart block).

Runs without AWS credentials — clients + clock are injected as fakes.

Run with:  python3 -m unittest test_handler
"""

from __future__ import annotations

import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import handler  # noqa: E402

# 2026-07-07 is a Tuesday. 04:30 UTC = 10:00 IST (inside 08:30-16:30 window).
IN_WINDOW = datetime(2026, 7, 7, 4, 30, tzinfo=timezone.utc)
# Tuesday 14:00 UTC = 19:30 IST (outside window).
OUT_OF_WINDOW = datetime(2026, 7, 7, 14, 0, tzinfo=timezone.utc)
# 2026-07-04 is a Saturday, 10:00 IST — weekday gate must reject.
SATURDAY = datetime(2026, 7, 4, 4, 30, tzinfo=timezone.utc)


class FakeEc2:
    def __init__(self, state: str = "running") -> None:
        self.state = state
        self.stopped: list[list[str]] = []

    def describe_instances(self, InstanceIds):  # noqa: N803 — boto3 casing
        return {
            "Reservations": [
                {
                    "Instances": [
                        {
                            "State": {"Name": self.state},
                            "LaunchTime": datetime(2026, 7, 7, 3, 0, tzinfo=timezone.utc),
                        }
                    ]
                }
            ]
        }

    def stop_instances(self, InstanceIds):  # noqa: N803
        self.stopped.append(InstanceIds)
        return {"StoppingInstances": []}


class FakeSns:
    def __init__(self) -> None:
        self.published: list[dict] = []

    def publish(self, **kwargs):
        self.published.append(kwargs)
        return {"MessageId": "m-1"}


class FakeEvents:
    def __init__(self, fail: bool = False) -> None:
        self.fail = fail
        self.disabled: list[str] = []

    def disable_rule(self, Name):  # noqa: N803
        if self.fail:
            raise RuntimeError("AccessDeniedException")
        self.disabled.append(Name)
        return {}


class FakeCe:
    """Cost Explorer fake — returns a fixed MTD or raises."""

    def __init__(self, amount: float | None) -> None:
        self.amount = amount

    def get_cost_and_usage(self, **kwargs):
        if self.amount is None:
            raise RuntimeError("CE throttled")
        return {
            "ResultsByTime": [
                {"Total": {"UnblendedCost": {"Amount": str(self.amount)}}}
            ]
        }


class ClassifyInWindowAction(unittest.TestCase):
    def test_none_mtd_is_cost_unknown(self) -> None:
        self.assertEqual(handler.classify_in_window_action(None, 55.0), "cost_unknown")

    def test_below_threshold_is_below_budget(self) -> None:
        self.assertEqual(handler.classify_in_window_action(54.99, 55.0), "below_budget")

    def test_exactly_at_threshold_is_breach(self) -> None:
        self.assertEqual(handler.classify_in_window_action(55.0, 55.0), "breach_stop")

    def test_above_threshold_is_breach(self) -> None:
        self.assertEqual(handler.classify_in_window_action(57.31, 55.0), "breach_stop")

    def test_zero_spend_is_below_budget(self) -> None:
        self.assertEqual(handler.classify_in_window_action(0.0, 55.0), "below_budget")


class InUpWindow(unittest.TestCase):
    def test_tuesday_10am_ist_is_in_window(self) -> None:
        self.assertTrue(handler.in_up_window(IN_WINDOW))

    def test_tuesday_evening_ist_is_out_of_window(self) -> None:
        self.assertFalse(handler.in_up_window(OUT_OF_WINDOW))

    def test_saturday_is_out_of_window(self) -> None:
        self.assertFalse(handler.in_up_window(SATURDAY))

    def test_0829_ist_is_out_and_0830_is_in(self) -> None:
        # 02:59 UTC = 08:29 IST; 03:00 UTC = 08:30 IST (Tuesday).
        self.assertFalse(handler.in_up_window(datetime(2026, 7, 7, 2, 59, tzinfo=timezone.utc)))
        self.assertTrue(handler.in_up_window(datetime(2026, 7, 7, 3, 0, tzinfo=timezone.utc)))

    def test_1630_ist_is_in_and_1631_is_out(self) -> None:
        # 11:00 UTC = 16:30 IST; 11:01 UTC = 16:31 IST (Tuesday).
        self.assertTrue(handler.in_up_window(datetime(2026, 7, 7, 11, 0, tzinfo=timezone.utc)))
        self.assertFalse(handler.in_up_window(datetime(2026, 7, 7, 11, 1, tzinfo=timezone.utc)))


class MtdUsd(unittest.TestCase):
    def test_returns_sum_from_ce(self) -> None:
        self.assertAlmostEqual(handler.mtd_usd(FakeCe(57.31)), 57.31)

    def test_returns_none_on_ce_error(self) -> None:
        self.assertIsNone(handler.mtd_usd(FakeCe(None)))


class LambdaHandlerEnvGuards(unittest.TestCase):
    def setUp(self) -> None:
        self._iid = handler.INSTANCE_ID
        self._arn = handler.ALERTS_TOPIC_ARN

    def tearDown(self) -> None:
        handler.INSTANCE_ID = self._iid
        handler.ALERTS_TOPIC_ARN = self._arn

    def test_missing_instance_id_returns_not_ok(self) -> None:
        handler.INSTANCE_ID = ""
        handler.ALERTS_TOPIC_ARN = "arn:aws:sns:ap-south-1:111:tv-prod-alerts"
        out = handler.lambda_handler({}, None)
        self.assertFalse(out["ok"])
        self.assertIn("INSTANCE_ID", out["reason"])

    def test_missing_topic_arn_returns_not_ok(self) -> None:
        handler.INSTANCE_ID = "i-x"
        handler.ALERTS_TOPIC_ARN = ""
        out = handler.lambda_handler({}, None)
        self.assertFalse(out["ok"])
        self.assertIn("ALERTS_TOPIC_ARN", out["reason"])


class HandlerScenarios(unittest.TestCase):
    """The three GAP 1 contract scenarios + legacy behaviour preservation."""

    def setUp(self) -> None:
        self._saved = (
            handler.INSTANCE_ID,
            handler.ALERTS_TOPIC_ARN,
            handler.START_RULE_NAME,
            handler.BUDGET_KILL_USD,
        )
        handler.INSTANCE_ID = "i-tvapp"
        handler.ALERTS_TOPIC_ARN = "arn:aws:sns:ap-south-1:111:tv-prod-alerts"
        handler.START_RULE_NAME = "tv-prod-daily-start"
        handler.BUDGET_KILL_USD = 55.0

    def tearDown(self) -> None:
        (
            handler.INSTANCE_ID,
            handler.ALERTS_TOPIC_ARN,
            handler.START_RULE_NAME,
            handler.BUDGET_KILL_USD,
        ) = self._saved

    def _run(self, ec2, sns, events, ce, now):
        return handler.lambda_handler(
            {}, None, ec2=ec2, sns=sns, events_client=events, ce_client=ce, now_utc=now
        )

    def test_breach_in_window_stops_disables_and_pages(self) -> None:
        ec2, sns, events = FakeEc2("running"), FakeSns(), FakeEvents()
        out = self._run(ec2, sns, events, FakeCe(57.31), IN_WINDOW)
        self.assertTrue(out["breach"])
        self.assertTrue(out["disable_ok"])
        self.assertEqual(ec2.stopped, [["i-tvapp"]])
        self.assertEqual(events.disabled, ["tv-prod-daily-start"])
        self.assertEqual(len(sns.published), 1)
        msg = sns.published[0]["Message"]
        self.assertIn("Budget breached", msg)
        self.assertIn("auto-start DISABLED", msg)
        self.assertIn("aws events enable-rule --name tv-prod-daily-start", msg)
        self.assertLessEqual(len(sns.published[0]["Subject"]), 99)

    def test_below_threshold_in_window_is_noop_ping(self) -> None:
        ec2, sns, events = FakeEc2("running"), FakeSns(), FakeEvents()
        out = self._run(ec2, sns, events, FakeCe(31.20), IN_WINDOW)
        self.assertTrue(out["noop"])
        self.assertEqual(out["budget_action"], "below_budget")
        self.assertEqual(ec2.stopped, [])
        self.assertEqual(events.disabled, [])
        self.assertEqual(len(sns.published), 1)
        self.assertIn("Box running (in window)", sns.published[0]["Message"])
        self.assertIn("$31.20", sns.published[0]["Message"])

    def test_ce_error_in_window_fail_safe_pages_without_disable(self) -> None:
        ec2, sns, events = FakeEc2("running"), FakeSns(), FakeEvents()
        out = self._run(ec2, sns, events, FakeCe(None), IN_WINDOW)
        self.assertTrue(out["noop"])
        self.assertEqual(out["budget_action"], "cost_unknown")
        # FAIL-SAFE: no stop, no disable on a Cost Explorer failure.
        self.assertEqual(ec2.stopped, [])
        self.assertEqual(events.disabled, [])
        self.assertEqual(len(sns.published), 1)
        self.assertIn("NOT disabling auto-start", sns.published[0]["Message"])

    def test_disable_failure_still_stops_and_pages_honestly(self) -> None:
        ec2, sns = FakeEc2("running"), FakeSns()
        events = FakeEvents(fail=True)
        out = self._run(ec2, sns, events, FakeCe(60.0), IN_WINDOW)
        self.assertTrue(out["breach"])
        self.assertFalse(out["disable_ok"])
        # Stop still landed even though the disable failed.
        self.assertEqual(ec2.stopped, [["i-tvapp"]])
        self.assertIn("disable FAILED", sns.published[0]["Subject"])
        self.assertIn("manually NOW", sns.published[0]["Message"])

    def test_out_of_window_force_stop_never_disables(self) -> None:
        ec2, sns, events = FakeEc2("running"), FakeSns(), FakeEvents()
        out = self._run(ec2, sns, events, FakeCe(57.31), OUT_OF_WINDOW)
        self.assertFalse(out["noop"])
        self.assertEqual(ec2.stopped, [["i-tvapp"]])
        # Legacy out-of-window arm preserved: stop + page, NO rule disable.
        self.assertEqual(events.disabled, [])
        self.assertIn("Hard Auto-Stop Guard Fired", sns.published[0]["Message"])

    def test_already_stopped_is_noop(self) -> None:
        ec2, sns, events = FakeEc2("stopped"), FakeSns(), FakeEvents()
        out = self._run(ec2, sns, events, FakeCe(57.31), IN_WINDOW)
        self.assertTrue(out["noop"])
        self.assertEqual(ec2.stopped, [])
        self.assertEqual(events.disabled, [])
        self.assertEqual(sns.published, [])


if __name__ == "__main__":
    unittest.main()
