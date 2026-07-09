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


class FakeSsm:
    """SSM fake for the change-only ping state (2026-07-09).

    read_fail / write_fail simulate outages; EVERY call is recorded so the
    breach-path test can prove SSM was never touched.
    """

    def __init__(
        self,
        value: str | None = None,
        read_fail: bool = False,
        write_fail: bool = False,
    ) -> None:
        self.value = value
        self.read_fail = read_fail
        self.write_fail = write_fail
        self.reads = 0
        self.writes: list[str] = []

    def get_parameter(self, Name):  # noqa: N803 — boto3 casing
        self.reads += 1
        if self.read_fail or self.value is None:
            raise RuntimeError("ParameterNotFound")
        return {"Parameter": {"Value": self.value}}

    def put_parameter(self, Name, Value, Type, Overwrite):  # noqa: N803
        if self.write_fail:
            raise RuntimeError("AccessDeniedException")
        self.writes.append(Value)
        return {}


class ExplodingSsm:
    """An SSM client whose ANY use raises — proves a path never touches it."""

    def __getattr__(self, name):
        raise AssertionError(f"SSM must never be used on this path: {name}")


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

    def _run(self, ec2, sns, events, ce, now, ssm=None):
        return handler.lambda_handler(
            {},
            None,
            ec2=ec2,
            sns=sns,
            events_client=events,
            ce_client=ce,
            ssm_client=ssm if ssm is not None else FakeSsm(value=None),
            now_utc=now,
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

    def test_below_threshold_in_window_first_run_pings_then_same_bucket_silent(
        self,
    ) -> None:
        # First run (no state): fail-open ping + state seeded. The next
        # hour in the SAME bucket is SILENT (2026-07-09 change-only pings).
        ec2, sns, events = FakeEc2("running"), FakeSns(), FakeEvents()
        ssm = FakeSsm(value=None)
        out = self._run(ec2, sns, events, FakeCe(31.20), IN_WINDOW, ssm)
        self.assertTrue(out["noop"])
        self.assertEqual(out["budget_action"], "below_budget")
        self.assertEqual(out["ping_reason"], "ping_state_missing")
        self.assertEqual(ec2.stopped, [])
        self.assertEqual(events.disabled, [])
        self.assertEqual(len(sns.published), 1)
        self.assertIn("$31.20", sns.published[0]["Message"])
        self.assertEqual(len(ssm.writes), 1, "state seeded on the first ping")
        # Hour 2, same bucket → silent, no extra write.
        ssm.value = ssm.writes[-1]
        out2 = self._run(ec2, sns, events, FakeCe(32.50), IN_WINDOW, ssm)
        self.assertEqual(out2["ping_reason"], "silent")
        self.assertEqual(len(sns.published), 1, "identical hour stays silent")
        self.assertEqual(len(ssm.writes), 1)

    def test_ce_error_in_window_fail_safe_pages_without_disable(self) -> None:
        ec2, sns, events = FakeEc2("running"), FakeSns(), FakeEvents()
        ssm = FakeSsm(value=None)
        out = self._run(ec2, sns, events, FakeCe(None), IN_WINDOW, ssm)
        self.assertTrue(out["noop"])
        self.assertEqual(out["budget_action"], "cost_unknown")
        self.assertEqual(out["ping_reason"], "ping_cost_unknown_edge")
        # FAIL-SAFE: no stop, no disable on a Cost Explorer failure.
        self.assertEqual(ec2.stopped, [])
        self.assertEqual(events.disabled, [])
        self.assertEqual(len(sns.published), 1)
        self.assertIn("box NOT", sns.published[0]["Message"])

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


def _state(month: str, bucket: int, outcome: str = "below_budget") -> str:
    import json

    return json.dumps({"month": month, "bucket": bucket, "outcome": outcome})


class ChangeOnlyPingClassifier(unittest.TestCase):
    """Pure classifier tests (2026-07-09 — change-only budget pings)."""

    def test_spend_bucket_steps_and_clamps(self) -> None:
        self.assertEqual(handler.spend_bucket(0.0, 55.0), 0)
        self.assertEqual(handler.spend_bucket(33.4, 55.0), 6)  # 60.7%
        self.assertEqual(handler.spend_bucket(44.0, 55.0), 8)  # 80.0%
        self.assertEqual(handler.spend_bucket(49.8, 55.0), 9)  # 90.5%
        self.assertEqual(handler.spend_bucket(None, 55.0), 0)
        self.assertEqual(handler.spend_bucket(-1.0, 55.0), 0)
        self.assertEqual(handler.spend_bucket(10.0, 0.0), 0)
        self.assertEqual(handler.spend_bucket(1e12, 55.0), 99)  # capped

    def test_same_bucket_silent(self) -> None:
        prev = {"month": "2026-07", "bucket": 6, "outcome": "below_budget"}
        reason, _ = handler.classify_ping_decision(
            prev, "below_budget", 33.9, 55.0, "2026-07"
        )
        self.assertEqual(reason, "silent")

    def test_bucket_increase_pings(self) -> None:
        prev = {"month": "2026-07", "bucket": 5, "outcome": "below_budget"}
        reason, new_state = handler.classify_ping_decision(
            prev, "below_budget", 33.4, 55.0, "2026-07"
        )
        self.assertEqual(reason, "ping_bucket_changed")
        self.assertEqual(new_state["bucket"], 6)

    def test_bucket_decrease_pings(self) -> None:
        # A mid-month credit/refund decrease is informative — ping.
        prev = {"month": "2026-07", "bucket": 6, "outcome": "below_budget"}
        reason, _ = handler.classify_ping_decision(
            prev, "below_budget", 20.0, 55.0, "2026-07"
        )
        self.assertEqual(reason, "ping_bucket_changed")

    def test_month_rollover_pings_once(self) -> None:
        prev = {"month": "2026-07", "bucket": 9, "outcome": "below_budget"}
        reason, new_state = handler.classify_ping_decision(
            prev, "below_budget", 0.5, 55.0, "2026-08"
        )
        self.assertEqual(reason, "ping_new_month")
        # The seeded state silences the following identical hour.
        reason2, _ = handler.classify_ping_decision(
            new_state, "below_budget", 0.6, 55.0, "2026-08"
        )
        self.assertEqual(reason2, "silent")

    def test_cost_unknown_edge_latched(self) -> None:
        prev = {"month": "2026-07", "bucket": 6, "outcome": "below_budget"}
        reason, new_state = handler.classify_ping_decision(
            prev, "cost_unknown", None, 55.0, "2026-07"
        )
        self.assertEqual(reason, "ping_cost_unknown_edge")
        # Repeats stay silent while latched.
        reason2, _ = handler.classify_ping_decision(
            new_state, "cost_unknown", None, 55.0, "2026-07"
        )
        self.assertEqual(reason2, "silent")

    def test_cost_recovered_pings(self) -> None:
        prev = {"month": "2026-07", "bucket": 6, "outcome": "cost_unknown"}
        reason, _ = handler.classify_ping_decision(
            prev, "below_budget", 33.4, 55.0, "2026-07"
        )
        self.assertEqual(reason, "ping_cost_recovered")

    def test_state_missing_pings_and_seeds(self) -> None:
        for prev in [None, "garbage", {"month": 7}, {"bucket": "x"}, {}]:
            reason, new_state = handler.classify_ping_decision(
                prev, "below_budget", 33.4, 55.0, "2026-07"
            )
            self.assertEqual(reason, "ping_state_missing", f"prev={prev!r}")
            self.assertEqual(new_state["month"], "2026-07")
            self.assertEqual(new_state["bucket"], 6)

    def test_approaching_wording_at_bucket_8_and_9(self) -> None:
        subj8, msg8 = handler.render_running_ping(
            "ping_bucket_changed", 44.0, 8, 55.0, 2.0, "i-x"
        )
        self.assertIn("approaching stop-budget — 80% used", subj8)
        self.assertIn("STOPS", msg8)
        subj9, msg9 = handler.render_running_ping(
            "ping_bucket_changed", 49.8, 9, 55.0, 2.0, "i-x"
        )
        self.assertIn("approaching stop-budget — 90% used", subj9)
        self.assertIn("🔴", msg9)
        subj6, msg6 = handler.render_running_ping(
            "ping_bucket_changed", 33.4, 6, 55.0, 2.0, "i-x"
        )
        self.assertIn("spend crossed 60% of stop-budget", subj6)
        self.assertIn("next 10% step", msg6)


class ChangeOnlyPingHandler(unittest.TestCase):
    """Handler-level SSM failure-posture tests (2026-07-09)."""

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

    def _run(self, ce, ssm, sns=None):
        sns = sns or FakeSns()
        return (
            handler.lambda_handler(
                {},
                None,
                ec2=FakeEc2("running"),
                sns=sns,
                events_client=FakeEvents(),
                ce_client=ce,
                ssm_client=ssm,
                now_utc=IN_WINDOW,
            ),
            sns,
        )

    def test_ssm_read_failure_fails_open_to_ping(self) -> None:
        # An unreadable state must PING (one ping beats a silent guard).
        ssm = FakeSsm(value=_state("2026-07", 6), read_fail=True)
        out, sns = self._run(FakeCe(33.4), ssm)
        self.assertEqual(out["ping_reason"], "ping_state_missing")
        self.assertEqual(len(sns.published), 1)

    def test_ssm_write_failure_still_returns_ok(self) -> None:
        # A failed state write is logged; the handler stays ok. Worst case:
        # one duplicate ping next hour (duplicate-over-silent).
        ssm = FakeSsm(value=None, write_fail=True)
        out, sns = self._run(FakeCe(33.4), ssm)
        self.assertTrue(out["ok"])
        self.assertFalse(out["state_saved"])
        self.assertEqual(len(sns.published), 1)

    def test_breach_stop_ignores_ping_state(self) -> None:
        # RATCHET: the breach stop is evaluated BEFORE any state read — an
        # SSM outage (or poisoned state) can never gate the stop.
        ec2, sns, events = FakeEc2("running"), FakeSns(), FakeEvents()
        out = handler.lambda_handler(
            {},
            None,
            ec2=ec2,
            sns=sns,
            events_client=events,
            ce_client=FakeCe(57.31),
            ssm_client=ExplodingSsm(),
            now_utc=IN_WINDOW,
        )
        self.assertTrue(out["breach"])
        self.assertEqual(ec2.stopped, [["i-tvapp"]])
        self.assertEqual(events.disabled, ["tv-prod-daily-start"])

    def test_out_of_window_force_stop_never_touches_ssm(self) -> None:
        # The legacy out-of-window arm is untouched — no state read/write.
        ec2, sns, events = FakeEc2("running"), FakeSns(), FakeEvents()
        out = handler.lambda_handler(
            {},
            None,
            ec2=ec2,
            sns=sns,
            events_client=events,
            ce_client=FakeCe(10.0),
            ssm_client=ExplodingSsm(),
            now_utc=OUT_OF_WINDOW,
        )
        self.assertFalse(out["noop"])
        self.assertEqual(ec2.stopped, [["i-tvapp"]])


if __name__ == "__main__":
    unittest.main()
