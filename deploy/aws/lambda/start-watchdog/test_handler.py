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


# ---------------------------------------------------------------------------
# NSE-holiday intentional-stop marker (2026-07-07 round-3 review fix):
# holiday-gate.sh self-stops the box at ~08:32 IST on weekday NSE holidays
# after stamping today's IST date into HOLIDAY_STOP_PARAM. The 08:45 check
# must NOT "heal" that stop — the pre-fix self-start false-paged a Critical
# "auto-start FAILED" every holiday AND fed the all-day restart war that can
# race the 09:20 IST market-hours alarm-gate sample.
# ---------------------------------------------------------------------------


def _wire_check_with_ssm(monkeypatch, sns, ec2, ssm, now=IN_WINDOW_NOW) -> None:
    def _client(svc: str):
        if svc == "ec2":
            return ec2
        if svc == "ssm":
            return ssm
        return sns

    monkeypatch.setattr(handler.boto3, "client", _client)
    monkeypatch.setattr(handler, "_now", lambda: now)


def test_check_stopped_holiday_marker_today_stays_silent_no_start(monkeypatch) -> None:
    """Marker == today's IST date → intentional holiday stop: no self-start,
    no Critical page. IN_WINDOW_NOW is 2026-06-10 03:15 UTC = 08:45 IST."""
    sns = _FakeSns()
    ec2 = _FakeEc2("stopped")
    ssm = _FakeSsm(value="2026-06-10")
    _wire_check_with_ssm(monkeypatch, sns, ec2, ssm)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is False
    assert out["self_started"] is False
    assert out["skipped"] == "holiday_stop"
    assert ec2.start_calls == []  # the restart war never begins
    assert sns.published == []  # no false Critical page on a holiday


def test_check_stopped_stale_holiday_marker_still_self_starts(monkeypatch) -> None:
    """A previous holiday's marker never matches today → normal self-heal."""
    sns = _FakeSns()
    ec2 = _FakeEc2("stopped")
    ssm = _FakeSsm(value="2026-06-09")  # yesterday's holiday
    _wire_check_with_ssm(monkeypatch, sns, ec2, ssm)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is True
    assert out["self_started"] is True
    assert ec2.start_calls == [["i-0test"]]


def test_check_stopped_marker_read_error_still_self_starts(monkeypatch) -> None:
    """FAIL-OPEN: an unreadable marker (missing param / AccessDenied) must
    never suppress the trading-day 08:45 rescue."""
    sns = _FakeSns()
    ec2 = _FakeEc2("stopped")
    ssm = _FakeSsm(raises=True)
    _wire_check_with_ssm(monkeypatch, sns, ec2, ssm)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is True
    assert out["self_started"] is True
    assert ec2.start_calls == [["i-0test"]]
    assert "started the box itself" in sns.published[0]["subject"]


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


# ---------------------------------------------------------------------------
# Curfew guard (mode="curfew_check") — operator directive 2026-07-03:
# a manually-started box left running outside operating hours must be
# auto-stopped; "manual human error is not acceptable".
# ---------------------------------------------------------------------------

# Thu 2026-07-02 23:51 IST = 18:21 UTC — the operator's real incident time.
CURFEW_NIGHT_NOW = datetime(2026, 7, 2, 18, 21, tzinfo=timezone.utc)
# Launched hours earlier (17:30 IST) — well past the 45-min grace.
CURFEW_OLD_LAUNCH = datetime(2026, 7, 2, 12, 0, tzinfo=timezone.utc)
# Launched 20 min before now — inside the 45-min grace window.
CURFEW_FRESH_LAUNCH = datetime(2026, 7, 2, 18, 1, tzinfo=timezone.utc)
# Tue 2026-06-30 10:00 IST = 04:30 UTC — mid-market, guard must never act.
MARKET_HOURS_NOW = datetime(2026, 6, 30, 4, 30, tzinfo=timezone.utc)
# Sat 2026-07-04 11:00 IST = 05:30 UTC — weekend, guard active all day.
WEEKEND_NOW = datetime(2026, 7, 4, 5, 30, tzinfo=timezone.utc)


class _FakeSsm:
    def __init__(self, value: str | None = None, raises: bool = False) -> None:
        self._value = value
        self._raises = raises

    def get_parameter(self, *, Name: str) -> dict[str, Any]:  # noqa: N803
        if self._raises or self._value is None:
            raise RuntimeError("ParameterNotFound")
        return {"Parameter": {"Name": Name, "Value": self._value}}


def _wire_curfew(
    monkeypatch,
    sns: _FakeSns,
    ec2: _FakeEc2,
    ssm: _FakeSsm,
    now: datetime,
) -> None:
    def _client(svc: str):
        if svc == "ec2":
            return ec2
        if svc == "ssm":
            return ssm
        return sns

    monkeypatch.setattr(handler.boto3, "client", _client)
    monkeypatch.setattr(handler, "_now", lambda: now)


def test_curfew_running_no_override_out_of_hours_stops_and_pages(monkeypatch) -> None:
    """The 23:51 incident: box left running at night, no keep-alive → STOP."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=CURFEW_OLD_LAUNCH)
    _wire_curfew(monkeypatch, sns, ec2, _FakeSsm(None), CURFEW_NIGHT_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is True
    assert ec2.stop_calls == [["i-0test"]]
    assert "Curfew auto-stop" in sns.published[0]["subject"]
    assert "💤" in sns.published[0]["message"]
    assert "keep-alive" in sns.published[0]["message"]


def test_curfew_keep_alive_override_skips(monkeypatch) -> None:
    """Explicit keep-alive in the future → the operator WANTS it running."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=CURFEW_OLD_LAUNCH)
    ssm = _FakeSsm("2026-07-03T06:00:00+05:30")  # tomorrow 6 AM IST
    _wire_curfew(monkeypatch, sns, ec2, ssm, CURFEW_NIGHT_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is False
    assert out["skipped"] == "keep_alive"
    assert ec2.stop_calls == []
    assert sns.published == []


def test_curfew_expired_keep_alive_still_stops(monkeypatch) -> None:
    """A keep-alive that already lapsed no longer protects the box."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=CURFEW_OLD_LAUNCH)
    ssm = _FakeSsm("2026-07-02T20:00:00+05:30")  # 8 PM IST — already past
    _wire_curfew(monkeypatch, sns, ec2, ssm, CURFEW_NIGHT_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is True
    assert ec2.stop_calls == [["i-0test"]]


def test_curfew_naive_keep_alive_is_interpreted_as_ist(monkeypatch) -> None:
    """Operator pastes a plain IST timestamp with no offset — still honored."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=CURFEW_OLD_LAUNCH)
    ssm = _FakeSsm("2026-07-03T06:00:00")  # naive → IST → still in the future
    _wire_curfew(monkeypatch, sns, ec2, ssm, CURFEW_NIGHT_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is False
    assert out["skipped"] == "keep_alive"


def test_curfew_malformed_keep_alive_treated_as_no_override(monkeypatch) -> None:
    """Garbage in the param must not silently disable budget protection."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=CURFEW_OLD_LAUNCH)
    _wire_curfew(monkeypatch, sns, ec2, _FakeSsm("not-a-timestamp"), CURFEW_NIGHT_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is True


def test_curfew_grace_window_skips_fresh_manual_start(monkeypatch) -> None:
    """Launched 20 min ago (<45 min grace) → operator is actively working."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=CURFEW_FRESH_LAUNCH)
    _wire_curfew(monkeypatch, sns, ec2, _FakeSsm(None), CURFEW_NIGHT_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is False
    assert out["skipped"] == "grace"
    assert ec2.stop_calls == []
    assert sns.published == []


def test_curfew_never_fires_during_operating_hours(monkeypatch) -> None:
    """08:00-17:00 IST Mon-Fri belongs to the normal schedule — the curfew
    guard must return WITHOUT touching EC2, SSM, or SNS."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=CURFEW_OLD_LAUNCH)
    _wire_curfew(monkeypatch, sns, ec2, _FakeSsm(None), MARKET_HOURS_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["skipped"] == "operating_hours"
    assert out["stopped"] is False
    assert ec2.stop_calls == []
    assert sns.published == []


def test_curfew_active_on_weekend_midday(monkeypatch) -> None:
    """Saturday 11:00 IST is OUTSIDE operating hours (Mon-Fri only) —
    a forgotten weekend box is stopped."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=CURFEW_OLD_LAUNCH)
    _wire_curfew(monkeypatch, sns, ec2, _FakeSsm(None), WEEKEND_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is True
    assert ec2.stop_calls == [["i-0test"]]


def test_curfew_stopped_box_stays_silent(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("stopped")
    _wire_curfew(monkeypatch, sns, ec2, _FakeSsm(None), CURFEW_NIGHT_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is False
    assert sns.published == []


def test_curfew_unknown_launch_time_pages_but_never_stops(monkeypatch) -> None:
    """Fail-safe (same idiom as stop_check): can't prove it isn't a
    seconds-old manual start → page, don't stop; next hour retries."""
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=None)
    _wire_curfew(monkeypatch, sns, ec2, _FakeSsm(None), CURFEW_NIGHT_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is False
    assert out["alerted"] is True
    assert ec2.stop_calls == []
    assert "could not verify start time" in sns.published[0]["subject"]


def test_curfew_stop_failure_pages_manual(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("running", launch_time=CURFEW_OLD_LAUNCH, stop_raises=True)
    _wire_curfew(monkeypatch, sns, ec2, _FakeSsm(None), CURFEW_NIGHT_NOW)
    out = handler.lambda_handler({"mode": "curfew_check"})
    assert out["stopped"] is False
    assert out["alerted"] is True
    assert "manual stop NEEDED" in sns.published[0]["subject"]
    assert "stop-instances" in sns.published[0]["message"]


def test_operating_hours_boundaries() -> None:
    """Pin the exact window: [08:00, 17:00) IST, Mon-Fri only."""
    ist = timezone(handler.IST_OFFSET)
    # Wed 2026-07-01 boundaries.
    assert not handler._is_within_operating_hours(
        datetime(2026, 7, 1, 7, 59, tzinfo=ist).astimezone(timezone.utc)
    )
    assert handler._is_within_operating_hours(
        datetime(2026, 7, 1, 8, 0, tzinfo=ist).astimezone(timezone.utc)
    )
    assert handler._is_within_operating_hours(
        datetime(2026, 7, 1, 16, 59, tzinfo=ist).astimezone(timezone.utc)
    )
    assert not handler._is_within_operating_hours(
        datetime(2026, 7, 1, 17, 0, tzinfo=ist).astimezone(timezone.utc)
    )
    # Sunday midday is never operating hours.
    assert not handler._is_within_operating_hours(
        datetime(2026, 7, 5, 12, 0, tzinfo=ist).astimezone(timezone.utc)
    )
