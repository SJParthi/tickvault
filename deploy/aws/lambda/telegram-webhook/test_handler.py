"""Unit tests for the telegram-webhook Lambda's pure-function formatters.

Runs without AWS credentials — exercises message construction only.
SSM + HTTP paths are unit-test exempt (covered by Lambda integration
in a live deploy smoke test).

Run with:  make lambda-test
      or:  python3 -m unittest discover -s deploy/aws/lambda/telegram-webhook
      or:  python3 -m pytest deploy/aws/lambda/telegram-webhook
"""

from __future__ import annotations

import inspect
import json
import re
import sys
import unittest
from pathlib import Path
from unittest import mock

# Allow `import handler` when run from the repo root.
sys.path.insert(0, str(Path(__file__).resolve().parent))

import handler  # noqa: E402

IST_12H_RE = re.compile(r"\b\d{1,2}:\d{2} (AM|PM) IST\b")


def _alarm_record(
    name: str,
    state: str = "ALARM",
    reason: str = "Threshold Crossed: 1 datapoint [1.0] was greater than the threshold (0.0).",
    when: str = "2026-07-07T04:31:12.345+0000",
) -> dict:
    return {
        "Sns": {
            "Subject": f"{state}: {name}",
            "Message": json.dumps(
                {
                    "AlarmName": name,
                    "NewStateValue": state,
                    "NewStateReason": reason,
                    "StateChangeTime": when,
                    "Region": "ap-south-1",
                }
            ),
        }
    }


class HouseLine(unittest.TestCase):
    def test_house_line_no_raw_threshold_json(self) -> None:
        alarm = {
            "AlarmName": "tv-prod-order-update-ws-inactive",
            "NewStateValue": "ALARM",
            "NewStateReason": (
                "Threshold Crossed: 1 out of the last 1 datapoints "
                "[0.0 (07/07/26 04:26:00)] was less than or equal to the "
                "threshold (0.0) (minimum 1 datapoint for OK -> ALARM transition)."
            ),
            "StateChangeTime": "2026-07-07T04:31:12.345+0000",
        }
        out = handler._house_line(alarm)
        self.assertNotIn("Threshold Crossed", out)
        self.assertNotIn("Reason:", out)
        self.assertNotIn("datapoint", out)
        self.assertNotIn("{", out)
        self.assertTrue(out.startswith("🆘 "))
        self.assertIn("Order confirmations feed has gone quiet", out)
        self.assertRegex(out, IST_12H_RE)

    def test_alarm_state_two_lines_emoji_first(self) -> None:
        out = handler._house_line(
            {
                "AlarmName": "tv-prod-cpu-high-5min",
                "NewStateValue": "ALARM",
                "StateChangeTime": "2026-07-07T04:31:12.345+0000",
            }
        )
        lines = out.split("\n")
        self.assertEqual(len(lines), 2)
        self.assertEqual(lines[0], "🆘 Server CPU has been very high for 5 minutes")
        self.assertEqual(lines[1], "10:01 AM IST")

    def test_ok_state_single_recovered_line(self) -> None:
        out = handler._house_line(
            {
                "AlarmName": "tv-prod-cpu-high-5min",
                "NewStateValue": "OK",
                "StateChangeTime": "2026-07-07T04:31:12.345+0000",
            }
        )
        self.assertNotIn("\n", out)
        self.assertTrue(out.startswith("✅ Recovered: "))
        self.assertRegex(out, IST_12H_RE)

    def test_unknown_alarm_name_fallback_still_plain_english(self) -> None:
        out = handler._house_line(
            {
                "AlarmName": "tv-prod-some-brand-new-alarm",
                "NewStateValue": "ALARM",
                "NewStateReason": "Threshold Crossed: blah",
                "StateChangeTime": "2026-07-07T04:31:12.345+0000",
            }
        )
        self.assertTrue(out.startswith("🆘 Some brand new alarm"))
        self.assertNotIn("Threshold Crossed", out)
        self.assertNotIn("tv-prod-", out)

    def test_missing_fields_never_crash(self) -> None:
        out = handler._house_line({})
        self.assertTrue(out.startswith("🆘 "))
        self.assertRegex(out, IST_12H_RE)

    def test_insufficient_data_is_warning_emoji(self) -> None:
        out = handler._house_line(
            {
                "AlarmName": "tv-prod-mem-used-high",
                "NewStateValue": "INSUFFICIENT_DATA",
                "StateChangeTime": "2026-07-07T04:31:12.345+0000",
            }
        )
        self.assertTrue(out.startswith("⚠️ "))


class Ist12Hour(unittest.TestCase):
    def test_ist_12_hour_timestamp(self) -> None:
        # 04:31 UTC + 05:30 = 10:01 AM IST
        self.assertEqual(handler._ist_12h("2026-07-07T04:31:12.345+0000"), "10:01 AM")

    def test_pm_and_z_suffix(self) -> None:
        # 10:00 UTC + 05:30 = 3:30 PM IST
        self.assertEqual(handler._ist_12h("2026-07-07T10:00:00Z"), "3:30 PM")

    def test_midnight_boundary_is_12_am(self) -> None:
        # 18:30 UTC = 00:00 IST
        self.assertEqual(handler._ist_12h("2026-07-07T18:30:00+0000"), "12:00 AM")

    def test_malformed_input_falls_back_without_crash(self) -> None:
        out = handler._ist_12h("not-a-timestamp")
        self.assertRegex(out, re.compile(r"^\d{1,2}:\d{2} (AM|PM)$"))
        out2 = handler._ist_12h("")
        self.assertRegex(out2, re.compile(r"^\d{1,2}:\d{2} (AM|PM)$"))

    def test_edge_dated_timestamps_fall_back_without_crash(self) -> None:
        # Regression (2026-07-07 refute round 1): year-0001/9999 inputs
        # parse fine but OverflowError'd on the IST conversion, crashing
        # the whole SNS batch before any Telegram send.
        for raw in ("9999-12-31T23:59:59+00:00", "0001-01-01T00:00:00+05:31"):
            out = handler._ist_12h(raw)
            self.assertRegex(out, re.compile(r"^\d{1,2}:\d{2} (AM|PM)$"), raw)


class AlarmPhrase(unittest.TestCase):
    def test_known_alarm_maps_to_plain_english(self) -> None:
        self.assertEqual(
            handler._alarm_phrase("tv-prod-questdb-disconnected"),
            "The database has been unreachable for too long",
        )

    def test_env_prefix_is_stripped_for_any_environment(self) -> None:
        self.assertEqual(
            handler._alarm_phrase("tv-staging-cpu-high-5min"),
            handler._alarm_phrase("tv-prod-cpu-high-5min"),
        )

    def test_empty_name_falls_back_to_generic(self) -> None:
        self.assertEqual(handler._alarm_phrase(""), "A cloud alarm changed state")


class FoldRecords(unittest.TestCase):
    def test_ok_flip_single_line_recovered(self) -> None:
        texts = handler._fold_records(
            [_alarm_record("tv-prod-cpu-high-5min", state="OK")],
            now_epoch=1_000_000.0,
            cache={},
        )
        self.assertEqual(len(texts), 1)
        self.assertNotIn("\n", texts[0])
        self.assertTrue(texts[0].startswith("✅ Recovered: "))
        self.assertIn("Server CPU has been very high for 5 minutes", texts[0])
        self.assertRegex(texts[0], IST_12H_RE)

    def test_alarm_ok_pair_in_batch_folds_to_recovered_only(self) -> None:
        texts = handler._fold_records(
            [
                _alarm_record("tv-prod-cpu-high-5min", state="ALARM"),
                _alarm_record("tv-prod-cpu-high-5min", state="OK"),
            ],
            now_epoch=1_000_000.0,
            cache={},
        )
        self.assertEqual(len(texts), 1)
        self.assertTrue(texts[0].startswith("✅"))
        self.assertNotIn("🆘", texts[0])

    def test_ok_then_re_alarm_keeps_the_alarm(self) -> None:
        # A 🆘 must NEVER be dropped by an older ✅ in the same batch.
        texts = handler._fold_records(
            [
                _alarm_record("tv-prod-cpu-high-5min", state="OK"),
                _alarm_record("tv-prod-cpu-high-5min", state="ALARM"),
            ],
            now_epoch=1_000_000.0,
            cache={},
        )
        self.assertEqual(len(texts), 1)
        self.assertTrue(texts[0].startswith("🆘"))

    def test_multiple_lone_oks_fold_into_one_recovered_line(self) -> None:
        texts = handler._fold_records(
            [
                _alarm_record("tv-prod-cpu-high-5min", state="OK"),
                _alarm_record("tv-prod-mem-used-high", state="OK"),
            ],
            now_epoch=1_000_000.0,
            cache={},
        )
        self.assertEqual(len(texts), 1)
        self.assertTrue(texts[0].startswith("✅ Recovered: "))
        self.assertIn("Server CPU has been very high for 5 minutes", texts[0])
        self.assertIn("Server memory is almost full", texts[0])

    def test_alarms_stay_individual_never_digested(self) -> None:
        texts = handler._fold_records(
            [
                _alarm_record("tv-prod-cpu-high-5min", state="ALARM"),
                _alarm_record("tv-prod-questdb-disconnected", state="ALARM"),
            ],
            now_epoch=1_000_000.0,
            cache={},
        )
        self.assertEqual(len(texts), 2)
        for text in texts:
            self.assertTrue(text.startswith("🆘"))

    def test_alarm_never_suppressed_by_warm_cache(self) -> None:
        cache = {
            "tv-prod-cpu-high-5min": ("ALARM", 999_999.0),
            "tv-prod-questdb-disconnected": ("OK", 999_999.0),
        }
        texts = handler._fold_records(
            [
                _alarm_record("tv-prod-cpu-high-5min", state="ALARM"),
                _alarm_record("tv-prod-questdb-disconnected", state="ALARM"),
            ],
            now_epoch=1_000_000.0,
            cache=cache,
        )
        self.assertEqual(len(texts), 2)
        for text in texts:
            self.assertTrue(text.startswith("🆘"))

    def test_repeat_ok_within_window_is_suppressed(self) -> None:
        cache: dict = {}
        first = handler._fold_records(
            [_alarm_record("tv-prod-cpu-high-5min", state="OK")],
            now_epoch=1_000_000.0,
            cache=cache,
        )
        self.assertEqual(len(first), 1)
        repeat = handler._fold_records(
            [_alarm_record("tv-prod-cpu-high-5min", state="OK")],
            now_epoch=1_000_000.0 + 30.0,
            cache=cache,
        )
        self.assertEqual(repeat, [])
        # Past the window the OK flows again.
        later = handler._fold_records(
            [_alarm_record("tv-prod-cpu-high-5min", state="OK")],
            now_epoch=1_000_000.0 + handler.OK_REPEAT_SUPPRESS_SECS + 1.0,
            cache=cache,
        )
        self.assertEqual(len(later), 1)

    def test_malformed_sns_record_fails_open_to_generic_line(self) -> None:
        texts = handler._fold_records(
            [None, {"Sns": None}, {"Sns": {"Message": {"weird": "shape"}}}],
            now_epoch=1_000_000.0,
            cache={},
        )
        self.assertTrue(len(texts) >= 1)
        for text in texts:
            self.assertNotIn("Threshold Crossed", text)
            self.assertNotIn("NewStateReason", text)

    def test_no_raw_reason_json_in_any_folded_text(self) -> None:
        texts = handler._fold_records(
            [
                _alarm_record("tv-prod-cpu-high-5min", state="ALARM"),
                _alarm_record("tv-prod-mem-used-high", state="OK"),
                {"Sns": {"Subject": "DLT deploy OK", "Message": "commit=abc ref=main"}},
            ],
            now_epoch=1_000_000.0,
            cache={},
        )
        for text in texts:
            self.assertNotIn("Threshold Crossed", text)
            self.assertNotIn("Reason:", text)
            self.assertNotIn("NewStateReason", text)


class FormatPlainSns(unittest.TestCase):
    def test_deploy_ok_subject_uses_check_emoji(self) -> None:
        out = handler._format_plain_sns("DLT deploy OK", "commit=abc ref=main")
        self.assertTrue(out.startswith("✅ DLT deploy OK"))
        self.assertIn("commit=abc", out)

    def test_deploy_failed_subject_uses_emergency_emoji(self) -> None:
        out = handler._format_plain_sns("DLT deploy FAILED", "commit=abc run=999")
        self.assertTrue(out.startswith("🆘 DLT deploy FAILED"))

    def test_no_subject_falls_back_to_bell(self) -> None:
        out = handler._format_plain_sns(None, "operator-test")
        self.assertTrue(out.startswith("🔔"))


class LambdaHandlerDelivery(unittest.TestCase):
    """End-to-end never-drop coverage through lambda_handler itself.

    Pins the delivery boundary: an ALARM record must reach the Telegram
    POST. A regression that filters or suppresses folded ALARM texts
    between _fold_records and _post_to_telegram — or a per-record crash
    that kills the whole batch — fails HERE, not just in theory.
    SSM + HTTP are mocked; no AWS creds needed.
    """

    def setUp(self) -> None:
        handler._LAST_SENT.clear()

    def _invoke(self, records: list) -> tuple[dict, list[str]]:
        posted: list[str] = []

        def _fake_post(_token: str, _chat_id: str, text: str) -> tuple[int, str]:
            posted.append(text)
            return 200, '{"ok":true}'

        with (
            mock.patch.object(handler, "_get_credentials", return_value=("tok", "chat")),
            mock.patch.object(handler, "_post_to_telegram", side_effect=_fake_post),
            mock.patch.object(handler.time, "sleep", lambda _s: None),
        ):
            result = handler.lambda_handler({"Records": records}, None)
        return result, posted

    def test_alarm_record_reaches_telegram_post_through_lambda_handler(self) -> None:
        result, posted = self._invoke([_alarm_record("tv-prod-ws-pool-all-dead")])
        self.assertEqual(result["sent"], 1)
        self.assertEqual(result["failures"], [])
        self.assertEqual(len(posted), 1)
        self.assertTrue(posted[0].startswith("🆘"))
        self.assertIn("ALL live market data connections are down", posted[0])

    def test_poisoned_timestamp_record_never_drops_genuine_alarm_in_batch(self) -> None:
        # Regression (2026-07-07 refute round 1): a batch containing one
        # edge-dated StateChangeTime crashed the ENTIRE invocation before
        # any send — the genuine 🆘 in the same batch was dropped forever.
        result, posted = self._invoke(
            [
                _alarm_record("tv-prod-ws-pool-all-dead"),
                _alarm_record("tv-prod-cpu-high-5min", when="9999-12-31T23:59:59+00:00"),
            ]
        )
        self.assertEqual(result["failures"], [])
        self.assertEqual(result["sent"], 2)
        genuine = [t for t in posted if "ALL live market data connections are down" in t]
        self.assertEqual(len(genuine), 1)
        self.assertTrue(genuine[0].startswith("🆘"))
        # The edge-dated alarm still pages (degraded timestamp, not dropped).
        poisoned = [t for t in posted if "Server CPU has been very high for 5 minutes" in t]
        self.assertEqual(len(poisoned), 1)
        self.assertTrue(poisoned[0].startswith("🆘"))

    def test_empty_event_skips_cleanly_without_credentials(self) -> None:
        result = handler.lambda_handler({}, None)
        self.assertEqual(result, {"sent": 0, "skipped": 1})


class SeverityEmojiHeuristic(unittest.TestCase):
    def test_alarm_state_beats_subject(self) -> None:
        # State=ALARM wins even if Subject says "ok"
        self.assertEqual(handler._severity_emoji("ok thing", "ALARM"), "🆘")

    def test_insufficient_data_is_warning(self) -> None:
        self.assertEqual(handler._severity_emoji("anything", "INSUFFICIENT_DATA"), "⚠️")

    def test_unknown_falls_back_to_bell(self) -> None:
        self.assertEqual(handler._severity_emoji("hello", None), "🔔")


class PlainTextTransport(unittest.TestCase):
    def test_post_payload_has_no_parse_mode(self) -> None:
        # Plain-text mode is load-bearing: an alarm name containing '*'
        # must never cause a silent Markdown-parse 400 drop.
        source = inspect.getsource(handler._post_to_telegram)
        self.assertNotIn("parse_mode", source)

    def test_alarm_phrases_pass_telegram_commandments(self) -> None:
        banned = ("rkyv", "papaya", "mpsc", ".rs", "data/", "QuestDB", "SSM", "WAL")
        for phrase in handler.ALARM_PHRASES.values():
            for word in banned:
                self.assertNotIn(word, phrase, f"banned token {word!r} in {phrase!r}")


if __name__ == "__main__":
    unittest.main()
