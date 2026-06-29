"""Unit tests for the instance start-watchdog Lambda.

No AWS calls — fake SNS/EC2 clients capture publishes + describe results.
Run: python -m pytest deploy/aws/lambda/start-watchdog/test_handler.py
"""

from __future__ import annotations

import importlib
import os
from datetime import datetime, timezone
from typing import Any

os.environ.setdefault("EC2_INSTANCE_ID", "i-0test")
os.environ.setdefault("ALERTS_TOPIC_ARN", "arn:aws:sns:ap-south-1:1:tv-prod-alerts")

handler = importlib.import_module("handler")

# Inside the scheduled check window (03:00-04:00 UTC = 08:30-09:30 IST).
IN_WINDOW_NOW = datetime(2026, 6, 10, 3, 15, tzinfo=timezone.utc)
ON_TIME_LAUNCH = datetime(2026, 6, 10, 3, 1, tzinfo=timezone.utc)
LATE_LAUNCH = datetime(2026, 6, 10, 3, 13, tzinfo=timezone.utc)  # 08:43 IST


class _FakeSns:
    def __init__(self) -> None:
        self.published: list[dict[str, str]] = []

    def publish(self, *, TopicArn: str, Subject: str, Message: str) -> None:  # noqa: N803
        self.published.append({"topic": TopicArn, "subject": Subject, "message": Message})


class _FakeEc2:
    def __init__(
        self,
        state: str | None,
        launch_time: datetime | None = None,
        start_raises: bool = False,
        stop_raises: bool = False,
        public_ip: str | None = None,
    ) -> None:
        self._state = state
        self._launch_time = launch_time
        self._start_raises = start_raises
        self._stop_raises = stop_raises
        self._public_ip = public_ip
        self.start_calls: list[list[str]] = []
        self.stop_calls: list[list[str]] = []

    def describe_instances(self, *, InstanceIds: list[str]) -> dict[str, Any]:  # noqa: N803
        if self._state is None:
            raise RuntimeError("boom")
        inst: dict[str, Any] = {"State": {"Name": self._state}}
        if self._launch_time is not None:
            inst["LaunchTime"] = self._launch_time
        if self._public_ip is not None:
            inst["PublicIpAddress"] = self._public_ip
        return {"Reservations": [{"Instances": [inst]}]}

    def start_instances(self, *, InstanceIds: list[str]) -> dict[str, Any]:  # noqa: N803
        if self._start_raises:
            raise RuntimeError("AccessDenied")
        self.start_calls.append(InstanceIds)
        return {"StartingInstances": [{"InstanceId": InstanceIds[0]}]}

    def stop_instances(self, *, InstanceIds: list[str]) -> dict[str, Any]:  # noqa: N803
        if self._stop_raises:
            raise RuntimeError("AccessDenied")
        self.stop_calls.append(InstanceIds)
        return {"StoppingInstances": [{"InstanceId": InstanceIds[0]}]}


def _wire(monkeypatch, sns: _FakeSns, ec2: _FakeEc2, now: datetime = IN_WINDOW_NOW) -> None:
    monkeypatch.setattr(handler.boto3, "client", lambda svc: ec2 if svc == "ec2" else sns)
    monkeypatch.setattr(handler, "_now", lambda: now)


def test_ping_publishes_positive_signal(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("running", public_ip="13.200.1.2")
    _wire(monkeypatch, sns, ec2)
    out = handler.lambda_handler({"mode": "ping"})
    assert out["published"] is True
    assert len(sns.published) == 1
    assert "start triggered" in sns.published[0]["subject"].lower()
    # Stale-wording regression lock: the schedule is 08:30 IST, not 08:00.
    assert "8:30" in sns.published[0]["message"]
    assert "08:00" not in sns.published[0]["message"]


def test_ping_includes_dashboard_link(monkeypatch) -> None:
    """The operator's 08:30 start ping must carry a tappable Feed Control link
    (http://<public_ip>:3001/feeds) built from the live public IP + port."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", public_ip="13.200.1.2")
    _wire(monkeypatch, sns, ec2)
    handler.lambda_handler({"mode": "ping"})
    msg = sns.published[0]["message"]
    assert "/feeds" in msg
    assert "3001" in msg
    assert "http://13.200.1.2:3001/feeds" in msg
    # The original start-triggered text must remain intact.
    assert "start triggered" in msg.lower()


def test_ping_missing_public_ip_degrades_gracefully(monkeypatch) -> None:
    """No public IP yet → the start message STILL sends, with no link and a
    clear 'not ready' line — never a silent omission, never a crash."""
    sns = _FakeSns()
    ec2 = _FakeEc2("pending", public_ip=None)  # box up but no IP assigned yet
    _wire(monkeypatch, sns, ec2)
    out = handler.lambda_handler({"mode": "ping"})
    assert out["published"] is True
    msg = sns.published[0]["message"]
    assert "http://" not in msg
    assert "3001/feeds" not in msg
    assert "not ready yet" in msg
    assert "8:30" in msg  # start signal preserved


def test_ping_describe_error_degrades_gracefully(monkeypatch) -> None:
    """describe_instances raising must not crash the ping — it still sends the
    start message without a dashboard link."""
    sns = _FakeSns()
    ec2 = _FakeEc2(None)  # describe_instances raises RuntimeError
    _wire(monkeypatch, sns, ec2)
    out = handler.lambda_handler({"mode": "ping"})
    assert out["published"] is True
    msg = sns.published[0]["message"]
    assert "http://" not in msg
    assert "not ready yet" in msg


def test_check_running_on_time_stays_silent(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=ON_TIME_LAUNCH)
    _wire(monkeypatch, sns, ec2)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is False
    assert sns.published == []  # no spam on a healthy box


def test_check_running_late_launch_pages_warning(monkeypatch) -> None:
    """2026-06-10 masking case: manual 08:43 start beat the 08:45 check —
    the box is running, but the 08:30 auto-start FAILED and must be flagged."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=LATE_LAUNCH)
    _wire(monkeypatch, sns, ec2)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is True
    assert out["self_started"] is False
    assert ec2.start_calls == []  # running box is never re-started
    assert "started late" in sns.published[0]["subject"]


def test_check_running_late_launch_outside_window_stays_silent(monkeypatch) -> None:
    """A manual `mode=check` in the afternoon must not misread an afternoon
    manual start as 'late' — late detection applies only in the 03:xx UTC window."""
    sns = _FakeSns()
    afternoon = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
    ec2 = _FakeEc2("running", launch_time=datetime(2026, 6, 10, 11, 30, tzinfo=timezone.utc))
    _wire(monkeypatch, sns, ec2, now=afternoon)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is False
    assert sns.published == []


def test_check_stopped_self_starts_and_pages(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("stopped")
    _wire(monkeypatch, sns, ec2)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is True
    assert out["state"] == "stopped"
    assert out["self_started"] is True
    assert ec2.start_calls == [["i-0test"]]  # the self-heal actually fired
    assert "FAILED" in sns.published[0]["subject"]
    assert "started the box itself" in sns.published[0]["subject"]


def test_check_stopped_self_start_failure_still_pages_manual(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("stopped", start_raises=True)
    _wire(monkeypatch, sns, ec2)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is True
    assert out["self_started"] is False
    assert "manual start NEEDED NOW" in sns.published[0]["subject"]
    assert "start-instances" in sns.published[0]["message"]


def test_check_describe_error_treated_as_not_running(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2(None)  # describe_instances raises
    _wire(monkeypatch, sns, ec2)
    out = handler.lambda_handler({"mode": "check"})
    # An error means we can't confirm 'running' → must alert, not stay silent.
    assert out["alerted"] is True
    assert out["state"] == "unknown"


# Stop-check window: 16:45 IST = 11:15 UTC; the stop trigger is 11:00 UTC.
STOP_CHECK_NOW = datetime(2026, 6, 10, 11, 15, tzinfo=timezone.utc)
MORNING_LAUNCH = datetime(2026, 6, 10, 3, 13, tzinfo=timezone.utc)  # since morning
EVENING_LAUNCH = datetime(2026, 6, 10, 11, 46, tzinfo=timezone.utc)  # manual 17:16 IST


def test_stop_check_stopped_box_stays_silent(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("stopped")
    _wire(monkeypatch, sns, ec2, now=STOP_CHECK_NOW)
    out = handler.lambda_handler({"mode": "stop_check"})
    assert out["alerted"] is False
    assert sns.published == []


def test_stop_check_running_since_morning_self_stops_and_pages(monkeypatch) -> None:
    """2026-06-10 incident: 16:30 stop silently failed; box (up since 08:43)
    still running at 18:18 IST. The 16:45 stop_check must stop it + page."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=MORNING_LAUNCH)
    _wire(monkeypatch, sns, ec2, now=STOP_CHECK_NOW)
    out = handler.lambda_handler({"mode": "stop_check"})
    assert out["alerted"] is True
    assert out["self_stopped"] is True
    assert ec2.stop_calls == [["i-0test"]]
    assert "stopped the box itself" in sns.published[0]["subject"]


def test_stop_check_manual_evening_session_is_never_touched(monkeypatch) -> None:
    """Operator lock: out-of-window runs are manual + deliberate. A box
    launched AFTER the 16:30 trigger must be left alone, silently."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=EVENING_LAUNCH)
    _wire(monkeypatch, sns, ec2, now=STOP_CHECK_NOW)
    out = handler.lambda_handler({"mode": "stop_check"})
    assert out["alerted"] is False
    assert ec2.stop_calls == []
    assert sns.published == []


def test_stop_check_unknown_launch_pages_but_never_stops(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=None)
    _wire(monkeypatch, sns, ec2, now=STOP_CHECK_NOW)
    out = handler.lambda_handler({"mode": "stop_check"})
    assert out["alerted"] is True
    assert out["self_stopped"] is False
    assert ec2.stop_calls == []  # fail-safe: never kill a possible manual session
    assert "still running" in sns.published[0]["subject"]


def test_stop_check_self_stop_failure_pages_manual(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=MORNING_LAUNCH, stop_raises=True)
    _wire(monkeypatch, sns, ec2, now=STOP_CHECK_NOW)
    out = handler.lambda_handler({"mode": "stop_check"})
    assert out["alerted"] is True
    assert out["self_stopped"] is False
    assert "manual stop NEEDED" in sns.published[0]["subject"]


def test_default_mode_is_check(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=ON_TIME_LAUNCH)
    _wire(monkeypatch, sns, ec2)
    out = handler.lambda_handler({})
    assert out["mode"] == "check"
