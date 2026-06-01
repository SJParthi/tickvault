"""Unit tests for the operator-control Lambda's pure-function helpers + routing.

Runs without AWS credentials — exercises method routing, the bearer-auth gate,
the market-hours guard, the labeled-snapshot parser, and the public HTML shell.
The boto3 EC2/SSM action paths are unit-test exempt (covered by a live deploy
smoke test against the one instance).

Run with:  python3 -m unittest test_handler
"""

from __future__ import annotations

import datetime
import json
import os
import sys
import unittest
from pathlib import Path

# handler.py reads these at import time and constructs boto3 clients (region
# only — no creds needed to construct). Set them BEFORE importing.
os.environ.setdefault("AWS_REGION", "ap-south-1")
os.environ.setdefault("TV_INSTANCE_ID", "i-0test000000000000")

sys.path.insert(0, str(Path(__file__).resolve().parent))

import handler  # noqa: E402


class HttpMethod(unittest.TestCase):
    def test_function_url_v2_get(self) -> None:
        ev = {"requestContext": {"http": {"method": "get"}}}
        self.assertEqual(handler._http_method(ev), "GET")

    def test_missing_method_defaults_to_post(self) -> None:
        # Fail closed: a malformed event is treated as an action (hits auth),
        # never as a free page serve.
        self.assertEqual(handler._http_method({}), "POST")


class MarketHoursGuard(unittest.TestCase):
    def test_inside_window_monday_1100_ist_is_market_hours(self) -> None:
        # 11:00 IST Mon == 05:30 UTC Mon.
        utc = datetime.datetime(2026, 6, 1, 5, 30, 0)
        self.assertTrue(handler._is_market_hours(utc))

    def test_after_close_monday_1600_ist_is_not_market_hours(self) -> None:
        # 16:00 IST Mon == 10:30 UTC Mon.
        utc = datetime.datetime(2026, 6, 1, 10, 30, 0)
        self.assertFalse(handler._is_market_hours(utc))

    def test_weekend_is_never_market_hours(self) -> None:
        # 2026-06-06 is a Saturday; any time → closed.
        utc = datetime.datetime(2026, 6, 6, 5, 30, 0)
        self.assertFalse(handler._is_market_hours(utc))


class Authorization(unittest.TestCase):
    def setUp(self) -> None:
        self._orig = handler._control_secret
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]

    def tearDown(self) -> None:
        handler._control_secret = self._orig  # type: ignore[assignment]

    def test_correct_bearer_is_authorized(self) -> None:
        self.assertTrue(handler._authorized({"authorization": "Bearer s3cret-token"}))

    def test_wrong_bearer_is_rejected(self) -> None:
        self.assertFalse(handler._authorized({"authorization": "Bearer nope"}))

    def test_missing_header_is_rejected(self) -> None:
        self.assertFalse(handler._authorized({}))

    def test_no_configured_secret_denies_all(self) -> None:
        handler._control_secret = lambda: ""  # type: ignore[assignment]
        self.assertFalse(handler._authorized({"authorization": "Bearer anything"}))


class ParseView(unittest.TestCase):
    def test_parses_labeled_snapshot(self) -> None:
        stdout = (
            "APP=active\n"
            "TICKS_TODAY=152340\n"
            "C1M=372\n"
            "C5M=75\n"
            "C15M=25\n"
            "C60M=6\n"
            "C1D=4\n"
            "DEDUP_KEYS=4\n"
            "MAX_TPS=5\n"
            "ERRORS_BEGIN\n"
            "Jun 01 11:00 tickvault: WARN something\n"
            "ERRORS_END\n"
        )
        out = handler._parse_view(stdout)
        self.assertEqual(out["app"], "active")
        self.assertEqual(out["ticks_today"], "152340")
        self.assertEqual(out["candles"]["1m"], "372")
        self.assertEqual(out["candles"]["1d"], "4")
        self.assertEqual(out["dedup_key_columns"], "4")
        self.assertEqual(out["max_ticks_per_second"], "5")
        self.assertEqual(len(out["recent_errors"]), 1)
        self.assertIn("WARN something", out["recent_errors"][0])

    def test_empty_stdout_yields_blank_fields(self) -> None:
        out = handler._parse_view("")
        self.assertEqual(out["app"], "")
        self.assertEqual(out["dedup_key_columns"], "")
        self.assertEqual(out["max_ticks_per_second"], "")
        self.assertEqual(out["recent_errors"], [])

    def test_no_error_lines_between_markers(self) -> None:
        out = handler._parse_view("APP=inactive\nERRORS_BEGIN\nERRORS_END\n")
        self.assertEqual(out["app"], "inactive")
        self.assertEqual(out["recent_errors"], [])


class GetServesPublicHtml(unittest.TestCase):
    def test_get_returns_html_without_token(self) -> None:
        ev = {"requestContext": {"http": {"method": "GET"}}}
        resp = handler.lambda_handler(ev, None)
        self.assertEqual(resp["statusCode"], 200)
        self.assertIn("text/html", resp["headers"]["content-type"])
        self.assertIn("operator portal", resp["body"])

    def test_html_contains_no_secret(self) -> None:
        # The page is a static shell — it must NOT embed any token/secret.
        html = handler._console_html()
        self.assertNotIn("Bearer s3cret", html)
        self.assertIn("localStorage", html)  # token kept client-side only

    def test_html_has_all_tabs(self) -> None:
        html = handler._console_html()
        for t in ("overview", "data", "github", "logs", "aws", "latency"):
            self.assertIn('data-t="' + t + '"', html)

    def test_html_supports_ready_made_key_link(self) -> None:
        # A #key=... link must auto-unlock + strip the fragment from the URL.
        html = handler._console_html()
        self.assertIn("location.hash", html)
        self.assertIn("replaceState", html)
        self.assertIn("tv_token", html)


class Latency(unittest.TestCase):
    def test_avg_ns(self) -> None:
        self.assertEqual(handler._avg_ns("1000", "10"), 100.0)
        self.assertIsNone(handler._avg_ns("1000", "0"))
        self.assertIsNone(handler._avg_ns("", ""))

    def test_parse_latency_full(self) -> None:
        stdout = (
            "DHAN_BEGIN\n"
            "0.012 0.030\n"
            "0.009 0.025\n"
            "0.011 0.028\n"
            "DHAN_END\n"
            "QDB=0.0021\n"
            "SKEW=0.000123\n"
            "METRICS_BEGIN\n"
            "tv_tick_processing_duration_ns_sum 50000\n"
            "tv_tick_processing_duration_ns_count 1000\n"
            "tv_wire_to_done_duration_ns_sum 8000000\n"
            "tv_wire_to_done_duration_ns_count 1000\n"
            "METRICS_END\n"
        )
        out = handler._parse_latency(stdout)
        self.assertEqual(out["dhan_tcp_ms"], "9.0")  # min of the 3 samples
        self.assertEqual(out["questdb_ms"], "2.1")
        self.assertEqual(out["clock_skew_ms"], "0.1")
        self.assertEqual(out["tick_process_avg_ns"], 50.0)
        self.assertEqual(out["wire_to_done_avg_ns"], 8000.0)
        self.assertIsNone(out["order_place_avg_ns"])
        self.assertEqual(out["tick_count"], "1000")

    def test_parse_latency_empty(self) -> None:
        out = handler._parse_latency("")
        self.assertEqual(out["dhan_tcp_ms"], "")
        self.assertIsNone(out["tick_process_avg_ns"])
        self.assertEqual(out["tick_count"], "")


class Latency(unittest.TestCase):
    def test_avg_ns(self) -> None:
        self.assertEqual(handler._avg_ns("1000", "10"), 100.0)
        self.assertIsNone(handler._avg_ns("1000", "0"))
        self.assertIsNone(handler._avg_ns("", ""))

    def test_parse_latency_full(self) -> None:
        stdout = (
            "DHAN_BEGIN\n"
            "0.012 0.030\n"
            "0.009 0.025\n"
            "0.011 0.028\n"
            "DHAN_END\n"
            "QDB=0.0021\n"
            "SKEW=0.000123\n"
            "METRICS_BEGIN\n"
            "tv_tick_processing_duration_ns_sum 50000\n"
            "tv_tick_processing_duration_ns_count 1000\n"
            "tv_wire_to_done_duration_ns_sum 8000000\n"
            "tv_wire_to_done_duration_ns_count 1000\n"
            "METRICS_END\n"
        )
        out = handler._parse_latency(stdout)
        self.assertEqual(out["dhan_tcp_ms"], "9.0")  # min of the 3 samples
        self.assertEqual(out["questdb_ms"], "2.1")
        self.assertEqual(out["clock_skew_ms"], "0.1")
        self.assertEqual(out["tick_process_avg_ns"], 50.0)
        self.assertEqual(out["wire_to_done_avg_ns"], 8000.0)
        self.assertIsNone(out["order_place_avg_ns"])
        self.assertEqual(out["tick_count"], "1000")

    def test_parse_latency_empty(self) -> None:
        out = handler._parse_latency("")
        self.assertEqual(out["dhan_tcp_ms"], "")
        self.assertIsNone(out["tick_process_avg_ns"])
        self.assertEqual(out["tick_count"], "")


class SafeSql(unittest.TestCase):
    def test_select_is_allowed(self) -> None:
        self.assertTrue(handler._is_safe_sql("SELECT count() FROM ticks"))
        self.assertTrue(handler._is_safe_sql("  with x as (select 1) select * from x"))
        self.assertTrue(handler._is_safe_sql("SHOW COLUMNS FROM ticks"))

    def test_mutations_are_rejected(self) -> None:
        for q in (
            "DROP TABLE ticks",
            "delete from ticks",
            "insert into ticks values (1)",
            "update ticks set x=1",
            "truncate table ticks",
            "alter table ticks add column z int",
            "select 1; drop table ticks",  # banned keyword anywhere
        ):
            self.assertFalse(handler._is_safe_sql(q), q)

    def test_empty_or_non_read_is_rejected(self) -> None:
        self.assertFalse(handler._is_safe_sql(""))
        self.assertFalse(handler._is_safe_sql("   "))
        self.assertFalse(handler._is_safe_sql("explainx select 1"))  # not a prefix word? still starts with 'explain'
        self.assertFalse(handler._is_safe_sql("vacuum"))


class PostRequiresAuth(unittest.TestCase):
    def setUp(self) -> None:
        self._orig = handler._control_secret
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]

    def tearDown(self) -> None:
        handler._control_secret = self._orig  # type: ignore[assignment]

    def test_post_without_token_is_401(self) -> None:
        ev = {
            "requestContext": {"http": {"method": "POST"}},
            "headers": {},
            "body": json.dumps({"action": "view"}),
        }
        resp = handler.lambda_handler(ev, None)
        self.assertEqual(resp["statusCode"], 401)


if __name__ == "__main__":
    unittest.main()
