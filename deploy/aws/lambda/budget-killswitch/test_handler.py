"""Unit tests for budget-killswitch Lambda pure-function helpers.

Runs without AWS credentials — exercises message formatting + alert
payload construction only. The boto3 SSM/EC2/SNS paths are unit-test
exempt (covered by Lambda integration in a live deploy smoke test).

Run with:  python3 -m unittest test_handler
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import handler  # noqa: E402


class ExtractBudgetMessage(unittest.TestCase):
    def test_empty_event_returns_placeholder(self) -> None:
        out = handler._extract_budget_message({})
        self.assertIn("<no SNS Records>", out)

    def test_typical_sns_envelope_extracts_subject_and_body(self) -> None:
        event = {
            "Records": [
                {
                    "Sns": {
                        "Subject": "AWS Budgets: budget threshold ALARM",
                        "Message": "Budget Name: tv-prod-monthly-budget\nActual: $12.50",
                    }
                }
            ]
        }
        out = handler._extract_budget_message(event)
        self.assertIn("AWS Budgets", out)
        self.assertIn("tv-prod-monthly-budget", out)
        self.assertIn("$12.50", out)

    def test_long_message_truncates_under_telegram_cap(self) -> None:
        event = {"Records": [{"Sns": {"Subject": "x", "Message": "y" * 5000}}]}
        out = handler._extract_budget_message(event)
        self.assertIn("…(truncated)", out)
        self.assertLess(len(out), 1500)


class FormatAlertPayload(unittest.TestCase):
    def test_payload_subject_is_under_sns_99_char_limit(self) -> None:
        # SNS hard limit is 100 chars; lambda truncates at 99 to leave room.
        # Payload subject is built in the handler — verify it stays short.
        stop_result = {
            "StoppingInstances": [
                {
                    "InstanceId": "i-deadbeef",
                    "PreviousState": {"Name": "running"},
                    "CurrentState": {"Name": "stopping"},
                }
            ]
        }
        out = handler._format_alert_payload("i-deadbeef", stop_result, "some context")
        self.assertLessEqual(len(out["Subject"]), 99)

    def test_payload_includes_kill_switch_marker(self) -> None:
        out = handler._format_alert_payload("i-x", {"StoppingInstances": []}, "ctx")
        self.assertIn("BUDGET KILL-SWITCH ACTIVATED", out["Subject"])
        self.assertIn("BUDGET KILL-SWITCH ACTIVATED", out["Message"])

    def test_payload_renders_state_transition(self) -> None:
        stop_result = {
            "StoppingInstances": [
                {
                    "InstanceId": "i-cafe",
                    "PreviousState": {"Name": "running"},
                    "CurrentState": {"Name": "stopping"},
                }
            ]
        }
        out = handler._format_alert_payload("i-cafe", stop_result, "ctx")
        self.assertIn("i-cafe: running -> stopping", out["Message"])

    def test_payload_handles_empty_state_changes(self) -> None:
        out = handler._format_alert_payload("i-y", {"StoppingInstances": []}, "ctx")
        self.assertIn("<no state change reported>", out["Message"])
        # Body should still carry operator next-steps section.
        self.assertIn("Next steps:", out["Message"])

    def test_payload_includes_cost_note_for_stopped_state(self) -> None:
        out = handler._format_alert_payload("i-z", {"StoppingInstances": []}, "ctx")
        self.assertIn("aws-budget.md", out["Message"])

    def test_payload_handles_missing_state_keys_gracefully(self) -> None:
        # AWS APIs sometimes return partial responses on degraded paths.
        stop_result = {"StoppingInstances": [{"InstanceId": "i-no-state"}]}
        out = handler._format_alert_payload("i-no-state", stop_result, "ctx")
        self.assertIn("i-no-state", out["Message"])
        self.assertNotIn("Traceback", out["Message"])


class LambdaHandlerGuards(unittest.TestCase):
    def setUp(self) -> None:
        # Save + restore env so test order doesn't leak state.
        self._saved_iid = handler.EC2_INSTANCE_ID
        self._saved_arn = handler.ALERTS_TOPIC_ARN

    def tearDown(self) -> None:
        handler.EC2_INSTANCE_ID = self._saved_iid
        handler.ALERTS_TOPIC_ARN = self._saved_arn

    def test_missing_instance_id_returns_not_ok(self) -> None:
        handler.EC2_INSTANCE_ID = ""
        handler.ALERTS_TOPIC_ARN = "arn:aws:sns:ap-south-1:111:tv_alerts"
        out = handler.lambda_handler({}, None)
        self.assertFalse(out["ok"])
        self.assertIn("EC2_INSTANCE_ID", out["reason"])

    def test_missing_topic_arn_returns_not_ok(self) -> None:
        handler.EC2_INSTANCE_ID = "i-x"
        handler.ALERTS_TOPIC_ARN = ""
        out = handler.lambda_handler({}, None)
        self.assertFalse(out["ok"])
        self.assertIn("ALERTS_TOPIC_ARN", out["reason"])


if __name__ == "__main__":
    unittest.main()
