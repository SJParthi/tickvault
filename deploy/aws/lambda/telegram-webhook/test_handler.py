"""Unit tests for the telegram-webhook Lambda's pure-function formatters.

Runs without AWS credentials — exercises message construction only.
SSM + HTTP paths are unit-test exempt (covered by Lambda integration
in a live deploy smoke test).

Run with:  python3 -m unittest deploy.aws.lambda.telegram-webhook.test_handler
"""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path

# Allow `import handler` when run from the repo root.
sys.path.insert(0, str(Path(__file__).resolve().parent))

import handler  # noqa: E402


class FormatCloudWatchAlarm(unittest.TestCase):
    def test_alarm_state_uses_emergency_emoji(self) -> None:
        alarm = {
            "AlarmName": "tv-prod-instance-status-failed",
            "NewStateValue": "ALARM",
            "NewStateReason": "Threshold Crossed: 1 datapoint > 0.0",
            "Region": "ap-south-1",
        }
        out = handler._format_cloudwatch_alarm(alarm)
        self.assertTrue(out.startswith("🆘 *tv-prod-instance-status-failed*"))
        self.assertIn("`ALARM`", out)
        self.assertIn("ap-south-1", out)

    def test_ok_state_uses_check_emoji(self) -> None:
        alarm = {
            "AlarmName": "tv-prod-cpu-high-5min",
            "NewStateValue": "OK",
            "NewStateReason": "Threshold no longer crossed",
            "Region": "ap-south-1",
        }
        out = handler._format_cloudwatch_alarm(alarm)
        self.assertTrue(out.startswith("✅"))

    def test_long_reason_is_truncated(self) -> None:
        alarm = {
            "AlarmName": "noisy",
            "NewStateValue": "ALARM",
            "NewStateReason": "x" * 5000,
            "Region": "ap-south-1",
        }
        out = handler._format_cloudwatch_alarm(alarm)
        self.assertIn("…(truncated)", out)
        # Telegram bot API caps message body at 4096 chars.
        self.assertLess(len(out), 4096)


class FormatPlainSns(unittest.TestCase):
    def test_deploy_ok_subject_uses_check_emoji(self) -> None:
        out = handler._format_plain_sns("DLT deploy OK", "commit=abc ref=main")
        self.assertTrue(out.startswith("✅ *DLT deploy OK*"))
        self.assertIn("commit=abc", out)

    def test_deploy_failed_subject_uses_emergency_emoji(self) -> None:
        out = handler._format_plain_sns("DLT deploy FAILED", "commit=abc run=999")
        self.assertTrue(out.startswith("🆘 *DLT deploy FAILED*"))

    def test_no_subject_falls_back_to_bell(self) -> None:
        out = handler._format_plain_sns(None, "operator-test")
        self.assertTrue(out.startswith("🔔"))


class BuildMessageDispatch(unittest.TestCase):
    def test_cloudwatch_json_routes_to_alarm_formatter(self) -> None:
        record = {
            "Subject": "ALARM: tv-prod-cpu-high-5min",
            "Message": json.dumps(
                {
                    "AlarmName": "tv-prod-cpu-high-5min",
                    "NewStateValue": "ALARM",
                    "NewStateReason": "CPU > 90 for 5 min",
                    "Region": "ap-south-1",
                }
            ),
        }
        out = handler._build_message(record)
        self.assertIn("*tv-prod-cpu-high-5min*", out)
        self.assertIn("`ALARM`", out)

    def test_plain_text_routes_to_plain_formatter(self) -> None:
        record = {"Subject": "DLT deploy OK", "Message": "commit=abc ref=main"}
        out = handler._build_message(record)
        self.assertIn("*DLT deploy OK*", out)
        self.assertIn("commit=abc", out)

    def test_malformed_json_falls_back_to_plain(self) -> None:
        record = {"Subject": "weird", "Message": "{not really json"}
        out = handler._build_message(record)
        # No crash; treated as plain text.
        self.assertIn("{not really json", out)


class SeverityEmojiHeuristic(unittest.TestCase):
    def test_alarm_state_beats_subject(self) -> None:
        # State=ALARM wins even if Subject says "ok"
        self.assertEqual(handler._severity_emoji("ok thing", "ALARM"), "🆘")

    def test_insufficient_data_is_warning(self) -> None:
        self.assertEqual(handler._severity_emoji("anything", "INSUFFICIENT_DATA"), "⚠️")

    def test_unknown_falls_back_to_bell(self) -> None:
        self.assertEqual(handler._severity_emoji("hello", None), "🔔")


if __name__ == "__main__":
    unittest.main()
