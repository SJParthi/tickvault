"""Unit tests for the market-open readiness pager Lambda (daily-universe §19).

No AWS calls — the pure `classify_readiness` core is tested directly and the
`lambda_handler` paths use fake SNS/EC2/CloudWatch clients (start-watchdog
test style).
Run: python -m pytest deploy/aws/lambda/market-open-readiness/test_handler.py
"""

from __future__ import annotations

import importlib
import os
from datetime import datetime, timedelta, timezone
from typing import Any

import pytest

os.environ.setdefault("EC2_INSTANCE_ID", "i-0test")
os.environ.setdefault("ALERTS_TOPIC_ARN", "arn:aws:sns:ap-south-1:1:tv-prod-alerts")

handler = importlib.import_module("handler")

IST = timezone(timedelta(hours=5, minutes=30))

# 08:45 IST on Mon 2026-07-06 = 03:15 UTC — the scheduled invocation instant.
NOW = datetime(2026, 7, 6, 8, 45, tzinfo=IST).astimezone(timezone.utc)
LAUNCH_YESTERDAY = datetime(2026, 7, 5, 9, 0, tzinfo=IST).astimezone(timezone.utc)
LAUNCH_TODAY_0831 = datetime(2026, 7, 6, 8, 31, tzinfo=IST).astimezone(timezone.utc)
LAUNCH_TODAY_0832 = datetime(2026, 7, 6, 8, 32, tzinfo=IST).astimezone(timezone.utc)
LAUNCH_TODAY_0700 = datetime(2026, 7, 6, 7, 0, tzinfo=IST).astimezone(timezone.utc)
LAUNCH_TODAY_0825 = datetime(2026, 7, 6, 8, 25, tzinfo=IST).astimezone(timezone.utc)
LAUNCH_TODAY_0824 = datetime(2026, 7, 6, 8, 24, tzinfo=IST).astimezone(timezone.utc)


# ---------------------------------------------------------------------------
# Pure decision core — classify_readiness
# ---------------------------------------------------------------------------


def test_running_and_booted_is_ready_silent() -> None:
    verdict = handler.classify_readiness("running", LAUNCH_TODAY_0831, True, False, NOW)
    assert verdict == handler.READY_SILENT
    # A silent verdict must never map to a page message.
    assert verdict not in handler.SUBJECTS_AND_MESSAGES


def test_running_without_boot_metric_pages_not_booted() -> None:
    """The 2026-07-06 10:04 class once the box IS up: running but the app
    never reported boot-complete → page, do not trust the green box."""
    verdict = handler.classify_readiness("running", LAUNCH_TODAY_0831, False, False, NOW)
    assert verdict == handler.NOT_BOOTED_PAGE


def test_stopped_with_stale_launch_pages_not_running() -> None:
    """The 2026-07-06 never-started case: still stopped at 08:45, last
    LaunchTime is yesterday's session → page."""
    verdict = handler.classify_readiness("stopped", LAUNCH_YESTERDAY, False, False, NOW)
    assert verdict == handler.NOT_RUNNING_PAGE


def test_stopped_after_holiday_gate_self_stop_is_silent() -> None:
    """NSE holiday: 08:30 cron started the box, the holiday gate self-stopped
    it (~08:31 IST launch then stop) → deliberate, stay silent."""
    verdict = handler.classify_readiness("stopped", LAUNCH_TODAY_0831, False, False, NOW)
    assert verdict == handler.HOLIDAY_SILENT


def test_stopping_after_holiday_gate_self_stop_is_silent() -> None:
    verdict = handler.classify_readiness("stopping", LAUNCH_TODAY_0832, False, False, NOW)
    assert verdict == handler.HOLIDAY_SILENT


def test_stopped_with_early_launch_pages_not_running() -> None:
    """A launch BEFORE the 08:25 IST cutoff cannot be the holiday gate
    (its earliest possible start is the 08:30 cron) → page."""
    verdict = handler.classify_readiness("stopped", LAUNCH_TODAY_0700, False, False, NOW)
    assert verdict == handler.NOT_RUNNING_PAGE


def test_pending_pages_not_running() -> None:
    """`pending` at 08:45 (start-watchdog self-heal race) still pages — the
    message wording tells the operator to watch for the watchdog's own
    'started' message rather than double-starting."""
    verdict = handler.classify_readiness("pending", None, False, False, NOW)
    assert verdict == handler.NOT_RUNNING_PAGE
    assert "watchdog" in handler.SUBJECTS_AND_MESSAGES[verdict][1]


def test_probe_error_pages_verify_failed() -> None:
    """Fail TOWARD paging: an AWS lookup failure (either probe) is NOT an OK."""
    for state in ("running", "stopped", "unknown"):
        verdict = handler.classify_readiness(state, None, False, True, NOW)
        assert verdict == handler.VERIFY_FAILED_PAGE


def test_subjects_are_emoji_first_and_within_sns_limit() -> None:
    """SNS Subject hard limit is 100 chars (we truncate at 99); the 10
    Telegram commandments demand a severity emoji at the START."""
    for subject, message in handler.SUBJECTS_AND_MESSAGES.values():
        assert len(subject) <= 99, f"subject too long: {subject!r}"
        assert subject[0] in "🆘⚠️🧪", f"subject must start with an emoji: {subject!r}"
        assert message  # never an empty page body


def test_holiday_cutoff_boundary_0825_ist() -> None:
    """Pin the exact cutoff: a launch at exactly 08:25 IST is
    holiday-eligible; 08:24 is not."""
    assert (
        handler._ist_minutes_of_day(LAUNCH_TODAY_0825)
        >= handler.HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES
    )
    assert (
        handler._ist_minutes_of_day(LAUNCH_TODAY_0824)
        < handler.HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES
    )
    assert (
        handler.classify_readiness("stopped", LAUNCH_TODAY_0825, False, False, NOW)
        == handler.HOLIDAY_SILENT
    )
    assert (
        handler.classify_readiness("stopped", LAUNCH_TODAY_0824, False, False, NOW)
        == handler.NOT_RUNNING_PAGE
    )


# ---------------------------------------------------------------------------
# lambda_handler wiring — fake clients, no AWS
# ---------------------------------------------------------------------------


class _FakeSns:
    def __init__(self, raises: bool = False) -> None:
        self._raises = raises
        self.published: list[dict[str, str]] = []

    def publish(self, *, TopicArn: str, Subject: str, Message: str) -> None:  # noqa: N803
        if self._raises:
            raise RuntimeError("sns down")
        self.published.append({"topic": TopicArn, "subject": Subject, "message": Message})


class _FakeEc2:
    def __init__(self, state: str | None, launch_time: datetime | None = None) -> None:
        self._state = state
        self._launch_time = launch_time

    def describe_instances(self, *, InstanceIds: list[str]) -> dict[str, Any]:  # noqa: N803
        if self._state is None:
            raise RuntimeError("boom")
        inst: dict[str, Any] = {"State": {"Name": self._state}}
        if self._launch_time is not None:
            inst["LaunchTime"] = self._launch_time
        return {"Reservations": [{"Instances": [inst]}]}


class _FakeCw:
    def __init__(self, values: list[float] | None = None, raises: bool = False) -> None:
        self._values = values or []
        self._raises = raises

    def get_metric_data(self, **_kwargs: Any) -> dict[str, Any]:
        if self._raises:
            raise RuntimeError("cw down")
        return {"MetricDataResults": [{"Id": "boot", "Values": self._values}]}


def _wire(monkeypatch, sns: _FakeSns, ec2: _FakeEc2, cw: _FakeCw) -> None:
    def _client(svc: str):
        if svc == "ec2":
            return ec2
        if svc == "cloudwatch":
            return cw
        return sns

    monkeypatch.setattr(handler.boto3, "client", _client)


def test_handler_healthy_morning_stays_silent(monkeypatch) -> None:
    sns = _FakeSns()
    _wire(monkeypatch, sns, _FakeEc2("running", LAUNCH_TODAY_0831), _FakeCw([1.0]))
    out = handler.lambda_handler({"mode": "readiness"})
    assert out["verdict"] == handler.READY_SILENT
    assert out["boot_seen"] is True
    assert sns.published == []  # no spam on a healthy box


def test_handler_running_unbooted_pages(monkeypatch) -> None:
    sns = _FakeSns()
    _wire(monkeypatch, sns, _FakeEc2("running", LAUNCH_TODAY_0831), _FakeCw([]))
    out = handler.lambda_handler({"mode": "readiness"})
    assert out["verdict"] == handler.NOT_BOOTED_PAGE
    assert len(sns.published) == 1
    assert "NOT ready" in sns.published[0]["subject"]


def test_handler_cw_probe_error_pages_verify_failed(monkeypatch) -> None:
    sns = _FakeSns()
    _wire(monkeypatch, sns, _FakeEc2("running", LAUNCH_TODAY_0831), _FakeCw(raises=True))
    out = handler.lambda_handler({"mode": "readiness"})
    assert out["verdict"] == handler.VERIFY_FAILED_PAGE
    assert "could not verify" in sns.published[0]["subject"]


def test_handler_drill_mode_force_publishes_test_page(monkeypatch) -> None:
    """Round-1 review fix: the SNS end-to-end drill. An evening stopped-box
    invoke classifies HOLIDAY_SILENT (LaunchTime = today's 08:30 auto-start),
    so it can NEVER prove the page route. {"mode": "drill"} must publish a
    clearly-labelled test page regardless of EC2/boot state."""
    sns = _FakeSns()
    # This EC2/CW shape would be HOLIDAY_SILENT on a readiness run -- the
    # drill must page anyway (it never consults the probes).
    _wire(monkeypatch, sns, _FakeEc2("stopped", LAUNCH_TODAY_0831), _FakeCw())
    out = handler.lambda_handler({"mode": "drill"})
    assert out["verdict"] == handler.DRILL_PAGE
    assert len(sns.published) == 1
    assert "test" in sns.published[0]["subject"].lower()
    assert "no action needed" in sns.published[0]["message"]


def test_drill_verdict_never_returned_by_classifier() -> None:
    """DRILL_PAGE exists only for the manual drill branch -- classify_readiness
    can never emit it (its 5 real verdicts are pinned by the tests above)."""
    for state in ("running", "stopped", "stopping", "pending", "unknown"):
        for boot_seen in (True, False):
            for probe_error in (True, False):
                verdict = handler.classify_readiness(
                    state, LAUNCH_TODAY_0831, boot_seen, probe_error, NOW
                )
                assert verdict != handler.DRILL_PAGE


def test_sns_publish_failure_propagates(monkeypatch) -> None:
    """The pager must never die silently: a failed publish RAISES so the
    invocation error feeds AWS/Lambda Errors -> the readiness-errors alarm."""
    sns = _FakeSns(raises=True)
    _wire(monkeypatch, sns, _FakeEc2("stopped", LAUNCH_YESTERDAY), _FakeCw())
    with pytest.raises(RuntimeError, match="sns down"):
        handler.lambda_handler({"mode": "readiness"})
