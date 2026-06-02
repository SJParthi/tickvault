"""Unit tests for the instance start-watchdog Lambda.

No AWS calls — fake SNS/EC2 clients capture publishes + describe results.
Run: python -m pytest deploy/aws/lambda/start-watchdog/test_handler.py
"""

from __future__ import annotations

import importlib
import os
from typing import Any

os.environ.setdefault("EC2_INSTANCE_ID", "i-0test")
os.environ.setdefault("ALERTS_TOPIC_ARN", "arn:aws:sns:ap-south-1:1:tv-prod-alerts")

handler = importlib.import_module("handler")


class _FakeSns:
    def __init__(self) -> None:
        self.published: list[dict[str, str]] = []

    def publish(self, *, TopicArn: str, Subject: str, Message: str) -> None:  # noqa: N803
        self.published.append({"topic": TopicArn, "subject": Subject, "message": Message})


class _FakeEc2:
    def __init__(self, state: str | None) -> None:
        self._state = state

    def describe_instances(self, *, InstanceIds: list[str]) -> dict[str, Any]:  # noqa: N803
        if self._state is None:
            raise RuntimeError("boom")
        return {"Reservations": [{"Instances": [{"State": {"Name": self._state}}]}]}


def test_ping_publishes_positive_signal(monkeypatch) -> None:
    sns = _FakeSns()
    monkeypatch.setattr(handler.boto3, "client", lambda svc: sns)
    out = handler.lambda_handler({"mode": "ping"})
    assert out["published"] is True
    assert len(sns.published) == 1
    assert "start triggered" in sns.published[0]["subject"].lower()


def test_check_running_stays_silent(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("running")
    monkeypatch.setattr(handler.boto3, "client", lambda svc: ec2 if svc == "ec2" else sns)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is False
    assert sns.published == []  # no spam on a healthy box


def test_check_stopped_alerts_critical(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("stopped")
    monkeypatch.setattr(handler.boto3, "client", lambda svc: ec2 if svc == "ec2" else sns)
    out = handler.lambda_handler({"mode": "check"})
    assert out["alerted"] is True
    assert out["state"] == "stopped"
    assert "FAILED" in sns.published[0]["subject"]


def test_check_describe_error_treated_as_not_running(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2(None)  # describe_instances raises
    monkeypatch.setattr(handler.boto3, "client", lambda svc: ec2 if svc == "ec2" else sns)
    out = handler.lambda_handler({"mode": "check"})
    # An error means we can't confirm 'running' → must alert, not stay silent.
    assert out["alerted"] is True
    assert out["state"] == "unknown"


def test_default_mode_is_check(monkeypatch) -> None:
    sns = _FakeSns()
    ec2 = _FakeEc2("running")
    monkeypatch.setattr(handler.boto3, "client", lambda svc: ec2 if svc == "ec2" else sns)
    out = handler.lambda_handler({})
    assert out["mode"] == "check"
