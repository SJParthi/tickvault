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
            'TICKS_BY_FEED="dhan",152000;"groww",340;\n'
            "C1M=372\n"
            "C5M=75\n"
            "C15M=25\n"
            "C60M=6\n"
            "C1D=4\n"
            "DEDUP_KEYS=5\n"
            "MAX_TPS=5\n"
            "ERRORS_BEGIN\n"
            "Jun 01 11:00 tickvault: WARN something\n"
            "ERRORS_END\n"
        )
        out = handler._parse_view(stdout)
        self.assertEqual(out["app"], "active")
        self.assertEqual(out["ticks_today"], "152340")
        self.assertEqual(out["ticks_by_feed"], {"dhan": "152000", "groww": "340"})
        self.assertEqual(out["candles"]["1m"], "372")
        self.assertEqual(out["candles"]["1d"], "4")
        self.assertEqual(out["dedup_key_columns"], "5")
        self.assertEqual(out["max_ticks_per_second"], "5")
        self.assertEqual(len(out["recent_errors"]), 1)
        self.assertIn("WARN something", out["recent_errors"][0])

    def test_empty_stdout_yields_blank_fields(self) -> None:
        out = handler._parse_view("")
        self.assertEqual(out["app"], "")
        self.assertEqual(out["dedup_key_columns"], "")
        self.assertEqual(out["max_ticks_per_second"], "")
        self.assertEqual(out["ticks_by_feed"], {})
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

    def test_html_has_exactly_three_tabs(self) -> None:
        # 2026-07-02 redesign (operator: "webpage looks completely messy, too
        # many buttons"): exactly Overview / Data / Admin — nothing else.
        import re

        html = handler._console_html()
        tabs = re.findall(r'data-t="(\w+)"', html)
        self.assertEqual(tabs, ["overview", "data", "admin"])

    def test_html_supports_ready_made_key_link(self) -> None:
        # A #key=... link must auto-unlock + strip the fragment from the URL.
        html = handler._console_html()
        self.assertIn("location.hash", html)
        self.assertIn("replaceState", html)
        self.assertIn("tv_token", html)

    def test_nuke_message_is_honest_not_optimistic(self) -> None:
        # The nuke runs async for minutes; the UI must NOT claim "fresh
        # containers + empty data" the instant it is dispatched (false-OK).
        html = handler._console_html()
        self.assertNotIn("fresh containers + empty data", html)
        self.assertIn("wiping in background", html)
        # It must poll the REAL outcome and surface both success + the hard-gate
        # failure (volume still in-use → data NOT wiped).
        self.assertIn("pollNuke", html)
        self.assertIn("command-status", html)
        self.assertIn("DOCKER-RESET-FAILED", html)
        self.assertIn("NUKE FAILED", html)


class ViewCommands(unittest.TestCase):
    def test_dedup_key_query_url_encodes_the_equals(self) -> None:
        # The dedup-key count query is the only view query with an `=` in its
        # value; it MUST be encoded as %3D or QuestDB /exp returns empty and the
        # "Dedup key columns" panel shows "?". Guard against regression.
        dedup_cmd = next(c for c in handler._VIEW_COMMANDS if "DEDUP_KEYS=" in c)
        self.assertIn("upsertKey%3Dtrue", dedup_cmd)
        self.assertNotIn("upsertKey=true", dedup_cmd)


class Latency(unittest.TestCase):
    def test_avg_ns(self) -> None:
        self.assertEqual(handler._avg_ns("1000", "10"), 100.0)
        self.assertIsNone(handler._avg_ns("1000", "0"))
        self.assertIsNone(handler._avg_ns("", ""))

    @staticmethod
    def _health_json(rows: list) -> str:
        import json as _json

        return _json.dumps({"market_open": True, "feeds": rows})

    def test_parse_latency_full(self) -> None:
        stdout = (
            "T0=100.0\n"
            "FEEDS_T0_BEGIN\n"
            + self._health_json(
                [
                    {"feed": "dhan", "ticks_total": 1000, "subscribed_total": 776,
                     "last_tick_age_secs": 1, "verdict": "ok", "enabled": True},
                    {"feed": "groww", "ticks_total": 2000, "subscribed_total": 768,
                     "last_tick_age_secs": 0, "verdict": "ok", "enabled": True},
                ]
            )
            + "\n"
            "FEEDS_T0_END\n"
            "PROBE_dhan_BEGIN\n"
            "0.012 0.030\n"
            "0.009 0.025\n"
            "PROBE_dhan_END\n"
            "PROBE_groww_BEGIN\n"
            "0.007 0.020\n"
            "x x\n"
            "PROBE_groww_END\n"
            "QDB=0.0021\n"
            "SKEW=0.000123\n"
            "T1=110.0\n"
            "METRICS_BEGIN\n"
            "tv_tick_processing_duration_ns_sum 50000\n"
            "tv_tick_processing_duration_ns_count 1000\n"
            "tv_wire_to_done_duration_ns_sum 8000000\n"
            "tv_wire_to_done_duration_ns_count 1000\n"
            "METRICS_END\n"
            "FEEDS_T1_BEGIN\n"
            + self._health_json(
                [
                    {"feed": "dhan", "ticks_total": 1050, "subscribed_total": 776,
                     "last_tick_age_secs": 1, "verdict": "ok", "enabled": True},
                    {"feed": "groww", "ticks_total": 2200, "subscribed_total": 768,
                     "last_tick_age_secs": 0, "verdict": "ok", "enabled": True},
                ]
            )
            + "\n"
            "FEEDS_T1_END\n"
        )
        out = handler._parse_latency(stdout)
        self.assertEqual(out["questdb_ms"], "2.1")
        self.assertEqual(out["clock_skew_ms"], "0.1")
        self.assertEqual(out["tick_process_avg_ns"], 50.0)
        self.assertEqual(out["wire_to_done_avg_ns"], 8000.0)
        self.assertIsNone(out["order_place_avg_ns"])
        self.assertEqual(out["tick_count"], "1000")
        # Per-feed rows: dynamic — driven by the app's health response.
        rows = {r["feed"]: r for r in out["feeds"]}
        self.assertEqual(set(rows), {"dhan", "groww"})
        self.assertEqual(rows["dhan"]["tcp_ms"], "9.0")  # min of the samples
        self.assertEqual(rows["dhan"]["tls_ms"], "25.0")
        self.assertTrue(rows["dhan"]["endpoint_known"])
        # groww's failed sample ('x x') is skipped, not fabricated.
        self.assertEqual(rows["groww"]["tcp_ms"], "7.0")
        # ticks/sec = Δticks_total / (T1-T0): dhan 50/10, groww 200/10.
        self.assertEqual(rows["dhan"]["ticks_per_sec"], 5.0)
        self.assertEqual(rows["groww"]["ticks_per_sec"], 20.0)
        self.assertEqual(rows["dhan"]["subscribed_total"], 776)
        self.assertEqual(out["feeds_error"], "")
        # Winners: groww strictly faster + busier; subscribed goes to dhan.
        self.assertEqual(out["winners"]["tcp_ms"], "groww")
        self.assertEqual(out["winners"]["ticks_per_sec"], "groww")
        self.assertEqual(out["winners"]["subscribed_total"], "dhan")

    def test_parse_latency_empty(self) -> None:
        out = handler._parse_latency("")
        self.assertEqual(out["feeds"], [])
        self.assertTrue(out["feeds_error"])  # honest reason, never fake rows
        self.assertEqual(out["winners"], {})
        self.assertIsNone(out["tick_process_avg_ns"])
        self.assertEqual(out["tick_count"], "")
        # Windowed fields degrade to None / 0 with no scrapes at all.
        self.assertIsNone(out["tick_p50_ns"])
        self.assertIsNone(out["tick_p99_ns"])
        self.assertEqual(out["tick_window_count"], 0)

    def test_parse_latency_unknown_third_feed_row_appears(self) -> None:
        # A future feed #3 the app reports but the probe map doesn't know:
        # its row STILL appears (app-side metrics) with endpoint_known=False
        # and blank probe cells — "endpoint unknown", never fake numbers.
        stdout = (
            "T0=100.0\n"
            "FEEDS_T1_BEGIN\n"
            + self._health_json(
                [
                    {"feed": "dhan", "ticks_total": 10, "subscribed_total": 776},
                    {"feed": "groww", "ticks_total": 20, "subscribed_total": 768},
                    {"feed": "zerodha", "ticks_total": 5, "subscribed_total": 100},
                ]
            )
            + "\n"
            "FEEDS_T1_END\n"
        )
        out = handler._parse_latency(stdout)
        rows = {r["feed"]: r for r in out["feeds"]}
        self.assertEqual(set(rows), {"dhan", "groww", "zerodha"})
        self.assertFalse(rows["zerodha"]["endpoint_known"])
        self.assertIsNone(rows["zerodha"]["endpoint"])
        self.assertEqual(rows["zerodha"]["tcp_ms"], "")
        self.assertEqual(rows["zerodha"]["tls_ms"], "")
        # Known feeds keep their mapping even with no probe samples in stdout.
        self.assertTrue(rows["dhan"]["endpoint_known"])
        self.assertTrue(rows["groww"]["endpoint_known"])

    def test_ticks_per_sec_counter_reset_is_none(self) -> None:
        # App restarted mid-window → T1 ticks_total BELOW T0 → tps must be
        # None (never a fabricated negative/huge rate).
        rows = handler._feed_latency_rows(
            {"feeds": [{"feed": "dhan", "ticks_total": 5000}]},
            {"feeds": [{"feed": "dhan", "ticks_total": 10}]},
            {},
            "10.0",
        )
        self.assertIsNone(rows[0]["ticks_per_sec"])

    def test_ticks_per_sec_missing_t0_is_none(self) -> None:
        rows = handler._feed_latency_rows(
            None,
            {"feeds": [{"feed": "dhan", "ticks_total": 10}]},
            {},
            "10.0",
        )
        self.assertEqual(len(rows), 1)
        self.assertIsNone(rows[0]["ticks_per_sec"])

    def test_feed_comparison_winners_lower_and_higher_better(self) -> None:
        rows = [
            {"feed": "dhan", "tcp_ms": "9.0", "tls_ms": "30.0",
             "last_tick_age_secs": 1, "ticks_per_sec": 5.0, "subscribed_total": 776},
            {"feed": "groww", "tcp_ms": "7.0", "tls_ms": "20.0",
             "last_tick_age_secs": 0, "ticks_per_sec": 20.0, "subscribed_total": 768},
        ]
        w = handler._feed_comparison_winners(rows)
        self.assertEqual(w["tcp_ms"], "groww")  # lower is better
        self.assertEqual(w["tls_ms"], "groww")
        self.assertEqual(w["last_tick_age_secs"], "groww")
        self.assertEqual(w["ticks_per_sec"], "groww")  # higher is better
        self.assertEqual(w["subscribed_total"], "dhan")

    def test_feed_comparison_winners_tie_and_single_feed_no_winner(self) -> None:
        # Tie at the best value → no single winner claimed.
        tied = [
            {"feed": "dhan", "tcp_ms": "9.0"},
            {"feed": "groww", "tcp_ms": "9.0"},
        ]
        self.assertNotIn("tcp_ms", handler._feed_comparison_winners(tied))
        # Only one feed has a comparable value → nothing to compare → no winner.
        single = [
            {"feed": "dhan", "tcp_ms": "9.0"},
            {"feed": "groww", "tcp_ms": ""},
        ]
        self.assertNotIn("tcp_ms", handler._feed_comparison_winners(single))
        self.assertEqual(handler._feed_comparison_winners([]), {})

    def test_latency_commands_probe_all_mapped_feeds_in_parallel(self) -> None:
        cmds = handler._LATENCY_COMMANDS
        joined = "\n".join(cmds)
        for feed, host in handler._FEED_LIVE_HOSTS.items():
            self.assertIn(f"PROBE_{feed}_BEGIN", joined)
            self.assertIn(f"https://{host}/", joined)
        # Parallel background probes + wait, each curl time-bounded — one
        # dead feed endpoint can never hang the whole measure.
        self.assertIn(") &", joined)
        self.assertIn("wait", cmds)
        for c in cmds:
            if "https://" in c:
                self.assertIn(f"--max-time {handler._FEED_PROBE_MAX_SECS}", c)
        # The feed LIST is discovered from the app at measure time (twice —
        # bracketing the window for the ticks/sec delta).
        self.assertEqual(joined.count("/api/feeds/health"), 2)

    # ---- windowed percentile math (operator directive 2026-06-10) ----

    @staticmethod
    def _two_scrape_stdout() -> str:
        """T0 → 1000 ticks lifetime; T1 → 1200 ticks. The 200-tick window:
        100 ticks ≤1000ns, 190 ≤5000ns, 200 ≤10000ns (10 in the 5–10µs
        bucket), so p50=1000ns exactly and p99 lands inside (5000,10000]."""
        return (
            "T0=100.0\n"
            "METRICS_T0_BEGIN\n"
            'tv_tick_processing_duration_ns_bucket{le="1000"} 500\n'
            'tv_tick_processing_duration_ns_bucket{le="5000"} 900\n'
            'tv_tick_processing_duration_ns_bucket{le="10000"} 1000\n'
            'tv_tick_processing_duration_ns_bucket{le="+Inf"} 1000\n'
            "tv_tick_processing_duration_ns_sum 2000000\n"
            "tv_tick_processing_duration_ns_count 1000\n"
            "METRICS_T0_END\n"
            "PROBE_dhan_BEGIN\n"
            "0.012 0.030\n"
            "PROBE_dhan_END\n"
            "QDB=0.0021\n"
            "SKEW=0.000123\n"
            "T1=110.0\n"
            "METRICS_BEGIN\n"
            'tv_tick_processing_duration_ns_bucket{le="1000"} 600\n'
            'tv_tick_processing_duration_ns_bucket{le="5000"} 1090\n'
            'tv_tick_processing_duration_ns_bucket{le="10000"} 1200\n'
            'tv_tick_processing_duration_ns_bucket{le="+Inf"} 1200\n'
            "tv_tick_processing_duration_ns_sum 2400000\n"
            "tv_tick_processing_duration_ns_count 1200\n"
            "METRICS_END\n"
        )

    def test_parse_latency_windowed_fields(self) -> None:
        out = handler._parse_latency(self._two_scrape_stdout())
        # Window: 200 ticks, Δsum=400000 → windowed avg 2000ns.
        self.assertEqual(out["tick_window_count"], 200)
        self.assertEqual(out["tick_window_avg_ns"], 2000.0)
        # p50: target=100 of 200; first bucket delta (le=1000) = 100 → cum
        # reaches target exactly at le=1000 → interpolation lands on 1000.
        self.assertEqual(out["tick_p50_ns"], 1000.0)
        # p99: target=198; cum ≤5000 is 190; lands in (5000,10000] bucket
        # with width 10 ticks → 5000 + (198-190)/10 * 5000 = 9000.
        self.assertEqual(out["tick_p99_ns"], 9000.0)
        self.assertEqual(out["window_secs"], "10.0")
        # Lifetime fields still served from the second scrape (back-compat).
        self.assertEqual(out["tick_process_avg_ns"], 2000.0)
        self.assertEqual(out["tick_count"], "1200")

    def test_percentile_from_bucket_deltas_interpolates(self) -> None:
        deltas = {"buckets": {100.0: 0.0, 500.0: 50.0, 1000.0: 100.0}, "sum": 1.0, "count": 100.0}
        # p50 → target 50 → reached exactly at le=500.
        self.assertEqual(handler._percentile_from_bucket_deltas(deltas, 0.50), 500.0)
        # p75 → target 75 → halfway through the (500,1000] bucket → 750.
        self.assertEqual(handler._percentile_from_bucket_deltas(deltas, 0.75), 750.0)

    def test_percentile_empty_window_returns_none(self) -> None:
        self.assertIsNone(handler._percentile_from_bucket_deltas(None, 0.5))
        self.assertIsNone(
            handler._bucket_deltas(
                {"buckets": {100.0: 5.0}, "sum": 10.0, "count": 5.0},
                {"buckets": {100.0: 5.0}, "sum": 10.0, "count": 5.0},
            )
        )

    def test_percentile_counter_reset_returns_none(self) -> None:
        # T1 counts BELOW T0 (app restarted mid-window) → whole window invalid.
        self.assertIsNone(
            handler._bucket_deltas(
                {"buckets": {100.0: 500.0}, "sum": 100.0, "count": 500.0},
                {"buckets": {100.0: 20.0}, "sum": 5.0, "count": 30.0},
            )
        )

    def test_percentile_inf_bucket_clamps(self) -> None:
        # All window samples beyond the last finite bucket → clamp to it.
        deltas = {
            "buckets": {100.0: 0.0, float("inf"): 10.0},
            "sum": 1.0,
            "count": 10.0,
        }
        self.assertEqual(handler._percentile_from_bucket_deltas(deltas, 0.99), 100.0)


class ParseStorage(unittest.TestCase):
    def test_parses_df_du(self) -> None:
        out = handler._parse_storage(
            "DISK_USED=6G\nDISK_FREE=24G\nDISK_PCT=20%\nDB_SIZE=3G\n"
        )
        self.assertEqual(out["disk_used_gb"], "6")
        self.assertEqual(out["disk_free_gb"], "24")
        self.assertEqual(out["disk_pct"], "20%")
        self.assertEqual(out["db_size_gb"], "3")

    def test_empty(self) -> None:
        out = handler._parse_storage("")
        self.assertEqual(out["disk_free_gb"], "")
        self.assertEqual(out["db_size_gb"], "")


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


class SafeSqlHardened(unittest.TestCase):
    """DB-console hardening (2026-07-02): single-statement, no comments,
    QuestDB mutators banned anywhere, server-side row cap."""

    def test_chained_unlisted_mutator_rejected(self) -> None:
        # THE pre-hardening gap: 'backup' was not in the banned list and the
        # first-word check only saw "select", so a ';'-chained second
        # statement slipped through. Now BOTH the single-statement rule and
        # the extended banned list reject it.
        self.assertFalse(handler._is_safe_sql("select 1; backup table ticks"))

    def test_multi_statement_rejected_single_trailing_semicolon_ok(self) -> None:
        self.assertFalse(handler._is_safe_sql("select 1; select 2"))
        self.assertFalse(handler._is_safe_sql("select 1;;"))
        self.assertFalse(handler._is_safe_sql(";"))
        self.assertTrue(handler._is_safe_sql("select 1;"))  # ONE trailing ';' stripped

    def test_sql_comments_rejected(self) -> None:
        self.assertFalse(handler._is_safe_sql("-- comment\nselect 1"))
        self.assertFalse(handler._is_safe_sql("select 1 -- tail comment"))
        self.assertFalse(handler._is_safe_sql("select /* hidden */ 1"))

    def test_with_insert_still_rejected(self) -> None:
        self.assertFalse(handler._is_safe_sql("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"))

    def test_questdb_mutator_keywords_rejected_anywhere(self) -> None:
        for kw in (
            "backup", "checkpoint", "snapshot", "cancel", "set", "refresh",
            "detach", "attach", "dedup", "squash", "resume", "suspend",
        ):
            self.assertFalse(handler._is_safe_sql(f"select {kw} from ticks"), kw)
            self.assertFalse(handler._is_safe_sql(f"{kw} table ticks"), kw)

    def test_plain_reads_still_pass(self) -> None:
        self.assertTrue(handler._is_safe_sql("SELECT * FROM ticks LIMIT 10"))
        self.assertTrue(handler._is_safe_sql("SHOW TABLES"))
        self.assertTrue(handler._is_safe_sql("SELECT table_name FROM tables() ORDER BY table_name"))
        self.assertTrue(handler._is_safe_sql("SELECT * FROM table_columns('ticks')"))
        # word-boundary sanity: 'offset' must not trip the 'set' ban.
        self.assertTrue(handler._is_safe_sql("select * from ticks limit 10 offset"))


class SqlRowCap(unittest.TestCase):
    def test_oversized_user_limit_is_clamped_to_1000(self) -> None:
        self.assertEqual(
            handler._cap_sql_rows("select * from ticks limit 99999"),
            "select * from ticks LIMIT 1000",
        )

    def test_missing_limit_is_appended_for_select(self) -> None:
        self.assertEqual(
            handler._cap_sql_rows("select * from ticks"),
            "select * from ticks LIMIT 1000",
        )

    def test_small_user_limit_is_kept(self) -> None:
        self.assertEqual(
            handler._cap_sql_rows("select * from ticks limit 50"),
            "select * from ticks limit 50",
        )

    def test_show_is_left_untouched(self) -> None:
        # Appending LIMIT to SHOW would be a syntax error; output is tiny.
        self.assertEqual(handler._cap_sql_rows("SHOW TABLES"), "SHOW TABLES")

    def test_trailing_semicolon_is_stripped_before_append(self) -> None:
        self.assertEqual(handler._cap_sql_rows("select 1;"), "select 1 LIMIT 1000")

    def test_sql_max_rows_constant_is_1000(self) -> None:
        self.assertEqual(handler._SQL_MAX_ROWS, 1000)


class DbConsoleTab(unittest.TestCase):
    def test_db_console_folded_into_data_tab(self) -> None:
        # 2026-07-02 redesign: the DB console (PR #1326) is no longer its own
        # tab — it lives INSIDE the Data tab. There is no data-t="db" tab, and
        # the console markup sits inside the data <section>.
        html = handler._console_html()
        self.assertNotIn('data-t="db"', html)
        data_section = html.split('<section data-tab="data"', 1)[1].split("</section>", 1)[0]
        for needle in ('id="dbtables"', 'id="dbsql"', 'id="dbout"', 'id="dbcols"', 'id="bars"', 'id="cvshields"'):
            self.assertIn(needle, data_section, needle)

    def test_db_tab_shows_read_only_badge(self) -> None:
        html = handler._console_html()
        self.assertIn("READ-ONLY — writes are blocked server-side", html)

    def test_db_tab_has_all_controls(self) -> None:
        html = handler._console_html()
        for needle in (
            "loadDbTables()", "runDbSql()", "dbDownloadCsv()", "dbPick(",
            'id="dbtables"', 'id="dbsql"', 'id="dbout"', 'id="dbcount"', 'id="dbcols"',
        ):
            self.assertIn(needle, html, needle)

    def test_data_tab_autoloads_tables_on_open(self) -> None:
        # tab('data') must trigger loadDbTables() without an extra click.
        html = handler._console_html()
        self.assertIn("name==='data' && !$('dbtables').dataset.loaded) loadDbTables()", html)

    def test_table_pick_is_index_based_no_injection(self) -> None:
        # The clicked table name comes from dbTables[i] — QuestDB's own
        # tables() output — never from user-typed input.
        html = handler._console_html()
        self.assertIn("dbTables[i]", html)
        self.assertIn("table_columns(", html)
        self.assertIn("LIMIT 100", html)

    def test_grid_rows_render_through_esc(self) -> None:
        # Every cell of the shared grid renderer must pass through esc().
        html = handler._console_html()
        self.assertIn("'<th>'+esc(c)+'</th>'", html)
        self.assertIn("'<td>'+esc(tcell(c))+'</td>'", html)


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


class WipeGate(unittest.TestCase):
    def setUp(self) -> None:
        self._orig = handler._control_secret
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]
        # Pin the clock OFF-hours: since the audit-fix-#2 hard lock, a forced
        # data-destructive action is 409'd in-window regardless of force, so
        # these dispatch tests must be deterministic at any wall-clock time.
        self._orig_mkt = handler._is_market_hours
        handler._is_market_hours = lambda _now: False  # type: ignore[assignment]

    def tearDown(self) -> None:
        handler._control_secret = self._orig  # type: ignore[assignment]
        handler._is_market_hours = self._orig_mkt  # type: ignore[assignment]

    def _wipe(self, force: bool, confirm: str = "WIPE"):
        return handler.lambda_handler(
            {
                "requestContext": {"http": {"method": "POST"}},
                "headers": {"authorization": "Bearer s3cret-token"},
                "body": json.dumps({"action": "wipe-questdb", "force": force, "confirm": confirm}),
            },
            None,
        )

    def test_wipe_without_force_is_blocked(self) -> None:
        # Either the destructive market-hours guard (409) or the explicit
        # force-required guard (409) — never reaches boto3.
        resp = self._wipe(force=False)
        self.assertEqual(resp["statusCode"], 409)

    def test_wipe_is_in_destructive_set(self) -> None:
        self.assertIn("wipe-questdb", handler._DESTRUCTIVE)

    def test_legacy_destructive_actions_require_server_side_tokens(self) -> None:
        # PR-5 H-1 (2026-07-02 security MEDIUM): a scripted call with a stolen
        # bearer + force=true must be 409'd unless it carries the SAME typed
        # word the portal makes the operator type — verified SERVER-side for
        # ALL THREE legacy destructive actions (wipe-groww already had this).
        for resp in (
            self._wipe(force=True, confirm=""),
            self._wipe(force=True, confirm="wipe"),
            self._docker_reset(force=True, confirm=""),
            self._docker_reset(force=True, confirm="YES"),
            self._docker_nuke_bare(force=True, confirm=""),
            self._docker_nuke_bare(force=True, confirm="erase"),
        ):
            self.assertEqual(resp["statusCode"], 409)
            self.assertIn("confirm", json.loads(resp["body"])["error"])
        # And the client JS forwards each token (no dead server gate).
        import inspect

        src = inspect.getsource(handler)
        self.assertIn("call('wipe-questdb',{force:true,confirm:'WIPE'})", src)
        self.assertIn("call('docker-reset',{force:$('force').checked,confirm:'NUKE-DOCKER'})", src)
        self.assertIn("call('docker-nuke-bare',{force:true,confirm:'ERASE'})", src)

    def test_wipe_questdb_removes_replay_sources_before_truncate_and_disables_unit(self) -> None:
        # PR-5 H-2 (hostile F5): the resurrection race — an external
        # `systemctl start` between TRUNCATE and the replay-source rm let the
        # booting bridge re-tail the capture file. Two pinned layers:
        #  (a) replay sources are removed BEFORE the TRUNCATE python block;
        #  (b) the unit is DISABLED for the wipe window and re-enabled before
        #      the final start;
        #  (c) prev_day_ohlcv is in the dynamic truncate targets.
        import inspect

        src = inspect.getsource(handler)
        rm_pos = src.index("feed capture/replay sources removed")
        truncate_pos = src.index("PYWIPE")
        self.assertLess(rm_pos, truncate_pos, "replay-source rm must precede TRUNCATE")
        disable_pos = src.index('"systemctl disable tickvault || true",\n                "pkill -f groww_sidecar.py')
        self.assertLess(disable_pos, rm_pos, "unit must be disabled before the wipe body")
        self.assertIn("t == 'prev_day_ohlcv'", src)
        self.assertIn('"systemctl enable tickvault || true"', src)

    def test_wipe_is_resurrect_proof_and_dynamic(self) -> None:
        # Operator incident 2026-07-02: the wipe TRUNCATEd 6 hardcoded tables
        # (of 27) and never touched the feed capture/replay files, so Groww
        # rows RESURRECTED via the bridge's byte-0 re-tail and most candle
        # tables survived for BOTH feeds. Pin the rewrite by source-scan:
        import inspect

        src = inspect.getsource(handler)
        # (a) dynamic table discovery — no hardcoded 6-table list.
        self.assertIn("SELECT table_name FROM tables()", src)
        self.assertIn("t == 'ticks' or t.startswith('candles_')", src)
        # (b) every feed's capture/replay source removed (feed-agnostic).
        for needle in ("/ws_wal", "/groww", "/spill", "/dlq", "live-ticks.ndjson"):
            self.assertIn(needle, src, f"wipe must remove the {needle} replay source")
        # (c) the app is stopped before + started after the wipe.
        wipe_block = src.split('action == "wipe-questdb"', 1)[1].split('action == "docker-reset"', 1)[0]
        self.assertIn("systemctl stop tickvault", wipe_block)
        self.assertIn("systemctl start tickvault", wipe_block)
        # (d) honest completion marker — never a fake OK.
        self.assertIn("WIPE-COMPLETE", wipe_block)
        self.assertIn("WIPE-PARTIAL", wipe_block)

    def test_docker_reset_and_bare_nuke_remove_feed_capture_sources(self) -> None:
        # The full nuke + bare nuke must ALSO sweep the feed capture/replay
        # dirs — the Groww capture file survived both before 2026-07-02.
        import inspect

        src = inspect.getsource(handler)
        reset_block = src.split('action == "docker-reset"', 1)[1].split('action == "docker-nuke-bare"', 1)[0]
        bare_block = src.split('action == "docker-nuke-bare"', 1)[1]
        for name, block in (("docker-reset", reset_block), ("docker-nuke-bare", bare_block)):
            self.assertIn("/ws_wal", block, f"{name} must remove the Dhan WAL")
            self.assertIn("/groww", block, f"{name} must remove the Groww capture dir")
            self.assertIn("live-ticks.ndjson", block, f"{name} must sweep future feeds' capture files")

    def _docker_reset(self, force: bool, confirm: str = "NUKE-DOCKER", token: str = "s3cret-token"):
        return handler.lambda_handler(
            {
                "requestContext": {"http": {"method": "POST"}},
                "headers": {"authorization": f"Bearer {token}"},
                "body": json.dumps({"action": "docker-reset", "force": force, "confirm": confirm}),
            },
            None,
        )

    def test_docker_reset_unauthorized_is_401(self) -> None:
        # Bearer-auth gate fires BEFORE anything else — a wrong token never
        # reaches the confirm/market-hours guards or boto3.
        resp = self._docker_reset(force=True, token="wrong-token")
        self.assertEqual(resp["statusCode"], 401)

    def test_docker_reset_without_confirm_is_blocked(self) -> None:
        # Full-docker-nuke typed-confirm guard (operator demand 2026-07-03,
        # mirroring the #1325 wipe-groww pattern): even a forced, authenticated
        # call without {"confirm": "NUKE-DOCKER"} is 409 and never reaches SSM.
        orig = handler._ssm_shell
        handler._ssm_shell = lambda cmds: self.fail("SSM must not be called without the typed confirm")  # type: ignore[assignment]
        try:
            for bad in ("", "NUKE", "nuke-docker", "DOCKER-NUKE", "WIPE"):
                resp = self._docker_reset(force=True, confirm=bad)
                self.assertEqual(resp["statusCode"], 409, repr(bad))
                self.assertIn("NUKE-DOCKER", json.loads(resp["body"])["error"])
        finally:
            handler._ssm_shell = orig  # type: ignore[assignment]

    def test_docker_reset_blocked_during_market_hours_without_force(self) -> None:
        # 09:15-15:30 IST guard: with the correct confirm phrase but no force,
        # the nuke is 409-blocked while the market is open.
        orig_mh = handler._is_market_hours
        orig_shell = handler._ssm_shell
        handler._is_market_hours = lambda utc: True  # type: ignore[assignment]
        handler._ssm_shell = lambda cmds: self.fail("SSM must not be called during market hours without force")  # type: ignore[assignment]
        try:
            resp = self._docker_reset(force=False)
        finally:
            handler._is_market_hours = orig_mh  # type: ignore[assignment]
            handler._ssm_shell = orig_shell  # type: ignore[assignment]
        self.assertEqual(resp["statusCode"], 409)
        self.assertIn("market hours", json.loads(resp["body"])["error"])

    def test_docker_reset_is_in_destructive_set(self) -> None:
        # Membership = market-hours-blocked during 09:15-15:30 IST.
        self.assertIn("docker-reset", handler._DESTRUCTIVE)

    def test_docker_reset_forced_is_hardened_full_nuke(self) -> None:
        # Regression 2026-06-05: "the nuke didn't wipe the data". The forced
        # docker-reset must (a) remove containers by VOLUME (not just the literal
        # name tv-questdb, which missed an off-project QuestDB), (b) fail LOUD
        # without recreating if the volume survives, and (c) wipe the HOST app
        # caches the Docker nuke can't see (instrument-cache/spill/dlq).
        captured: dict = {}
        orig = handler._ssm_shell
        handler._ssm_shell = lambda cmds: (captured.__setitem__("cmds", cmds) or "cmd-123")  # type: ignore[assignment]
        try:
            resp = self._docker_reset(force=True)
        finally:
            handler._ssm_shell = orig  # type: ignore[assignment]
        self.assertEqual(resp["statusCode"], 200)
        joined = "\n".join(captured["cmds"])
        # (a) robust container removal by VOLUME, not just by name
        self.assertIn("--filter volume=tv-questdb-data", joined)
        # (b) hard fail-loud gate — must NOT recreate if the volume survives
        self.assertIn("DOCKER-RESET-FAILED", joined)
        # (c) host caches wiped too (the dirs the Docker nuke cannot see)
        self.assertIn("/opt/tickvault/data/instrument-cache", joined)
        self.assertIn("/opt/tickvault/data/spill", joined)
        self.assertIn("/opt/tickvault/data/dlq", joined)
        # (d) the FULL-NUKE sequence: stop app → compose down -v (containers +
        #     volumes = full QuestDB wipe) → prune → fresh rebuild + app restart
        #     (fresh-start state) — operator demand 2026-07-03.
        self.assertIn("systemctl stop tickvault", joined)
        self.assertIn("docker compose down -v", joined)
        self.assertIn("docker system prune -af --volumes", joined)
        self.assertIn("ensure-questdb.sh", joined)
        self.assertIn("systemctl restart tickvault", joined)

    def _docker_nuke_bare(self, force: bool, confirm: str = "ERASE"):
        return handler.lambda_handler(
            {
                "requestContext": {"http": {"method": "POST"}},
                "headers": {"authorization": "Bearer s3cret-token"},
                "body": json.dumps({"action": "docker-nuke-bare", "force": force, "confirm": confirm}),
            },
            None,
        )

    def test_docker_nuke_bare_without_force_is_blocked(self) -> None:
        self.assertEqual(self._docker_nuke_bare(force=False)["statusCode"], 409)

    def test_docker_nuke_bare_is_in_destructive_set(self) -> None:
        self.assertIn("docker-nuke-bare", handler._DESTRUCTIVE)

    def test_docker_nuke_bare_forced_deletes_all_and_does_not_rebuild(self) -> None:
        # Operator request 2026-06-12: behave like Docker Desktop "delete all" —
        # remove EVERY container + image + volume and DO NOT rebuild (the box is
        # left bare). This is what distinguishes it from docker-reset.
        captured: dict = {}
        orig = handler._ssm_shell
        handler._ssm_shell = lambda cmds: (captured.__setitem__("cmds", cmds) or "cmd-bare")  # type: ignore[assignment]
        try:
            resp = self._docker_nuke_bare(force=True)
        finally:
            handler._ssm_shell = orig  # type: ignore[assignment]
        self.assertEqual(resp["statusCode"], 200)
        joined = "\n".join(captured["cmds"])
        # deletes ALL of the three object types
        self.assertIn("docker ps -aq", joined)
        self.assertIn("docker images -aq", joined)
        self.assertIn("docker volume ls -q", joined)
        # truthful verify line
        self.assertIn("BARE-NUKE-RESULT", joined)
        self.assertIn("bare-nuke-complete", joined)
        # the WHOLE POINT: it must NOT rebuild / restart the app (unlike docker-reset)
        self.assertNotIn("ensure-questdb.sh", joined)
        self.assertNotIn("systemctl restart tickvault", joined)
        self.assertNotIn("docker compose up", joined)


class HtmlWipeButton(unittest.TestCase):
    def test_html_has_wipe_button(self) -> None:
        html = handler._console_html()
        self.assertIn("wipeData()", html)
        self.assertIn("Wipe ALL data", html)

    def test_html_has_docker_reset_button(self) -> None:
        # Operator demand 2026-07-03 ("entire docker nuke") — the Full Docker
        # Nuke must be a clearly-labeled red action inside the danger zone.
        html = handler._console_html()
        self.assertIn("dockerReset()", html)
        self.assertIn("Full Docker nuke", html)
        self.assertIn("fresh start", html)
        # The nuke must spell out the SEBI-audit-data loss + require typing
        # the server-verified confirm phrase NUKE-DOCKER.
        self.assertIn("NUKE-DOCKER", html)
        self.assertIn("audit", html)
        # The UI must SEND the typed confirm phrase server-side (no client-only
        # prompt) and respect the force checkbox (market-hours guard).
        self.assertIn("call('docker-reset',{force:$('force').checked,confirm:'NUKE-DOCKER'})", html)

    def test_html_has_bare_nuke_button(self) -> None:
        html = handler._console_html()
        self.assertIn("bareNuke()", html)
        self.assertIn("Bare Nuke", html)
        # must require typing ERASE + spell out that the box is left EMPTY/dead.
        self.assertIn("ERASE", html)
        self.assertIn("redeploy", html)


class ParseCrossVerify(unittest.TestCase):
    def test_parse_cross_verify(self) -> None:
        stdout = (
            "CV_DATE=2026-06-10\n"
            "CV_MISMATCH_ROWS=0\n"
            "CV_INSTRUMENTS=243\n"
            "CV_COMPARED=91230\n"
            "CV_MISSING=0\n"
            "CV_DEGRADED=False\n"
        )
        got = handler._parse_cross_verify(stdout)
        self.assertEqual(got["date"], "2026-06-10")
        self.assertEqual(got["mismatch_rows"], "0")
        self.assertEqual(got["instruments"], "243")
        self.assertEqual(got["compared"], "91230")
        self.assertEqual(got["missing"], "0")
        self.assertEqual(got["degraded"], "False")

    def test_parse_cross_verify_empty_stdout_yields_blank_fields(self) -> None:
        # Box stopped / app down / no run yet → blank fields → the card
        # shows a truthful "no run yet", never a fabricated PASS.
        got = handler._parse_cross_verify("")
        self.assertEqual(got["date"], "")
        self.assertEqual(got["compared"], "")
        self.assertEqual(got["degraded"], "")


class CrossVerifyCard(unittest.TestCase):
    def test_html_has_cross_verify_card(self) -> None:
        html = handler._console_html()
        self.assertIn("loadCrossVerify()", html)
        self.assertIn('id="cvshields"', html)
        self.assertIn("cross-verify — daily candle check vs exchange record", html)
        self.assertIn("3:31 PM IST", html)

    def test_card_renders_values_through_esc(self) -> None:
        # 2026-06-10 pre-impl security review (S-H1): every box-derived
        # string field in the card must pass through esc() before landing
        # in innerHTML.
        html = handler._console_html()
        for field in ("j.date", "j.instruments", "j.compared", "j.mismatch_rows", "j.missing"):
            self.assertIn(f"esc({field}", html, f"{field} must be esc()-wrapped")

    def test_card_pass_requires_compared_positive(self) -> None:
        # False-OK guard parity with the Telegram event severity: PASS
        # requires compared>0 — "nothing compared" must never render PASS.
        html = handler._console_html()
        self.assertIn("compared>0", html)

    def test_cross_verify_action_is_read_only(self) -> None:
        # Read-only action: must NOT be market-hours-blocked.
        self.assertNotIn("cross_verify", handler._DESTRUCTIVE)


class FeedCounts(unittest.TestCase):
    def test_parse_feed_counts_two_feeds(self) -> None:
        got = handler._parse_feed_counts('"dhan",152000;"groww",340;')
        self.assertEqual(got, {"dhan": "152000", "groww": "340"})

    def test_parse_feed_counts_empty_or_garbage_yields_empty(self) -> None:
        # No fabricated zeros — an unreachable QuestDB renders NOTHING.
        self.assertEqual(handler._parse_feed_counts(""), {})
        self.assertEqual(handler._parse_feed_counts(";;"), {})
        self.assertEqual(handler._parse_feed_counts("garbage-without-comma"), {})

    def test_view_commands_query_ticks_grouped_by_feed(self) -> None:
        cmd = next(c for c in handler._VIEW_COMMANDS if "TICKS_BY_FEED=" in c)
        self.assertIn("GROUP%20BY%20feed", cmd)
        self.assertIn("FROM%20ticks", cmd)


class FeedsView(unittest.TestCase):
    def test_parse_feeds_view_full(self) -> None:
        stdout = (
            "FEEDS_BEGIN\n"
            '{"dhan_enabled": true, "groww_enabled": false, '
            '"dhan_lane_running": true, "groww_lane_running": false}\n'
            "FEEDS_END\n"
            "FEEDS_HEALTH_BEGIN\n"
            '{"market_open": true, "feeds": [{"feed": "dhan", "verdict": "ok", '
            '"reason": "streaming", "enabled": true, "lane_running": true, '
            '"ticks_total": 152000}]}\n'
            "FEEDS_HEALTH_END\n"
        )
        out = handler._parse_feeds_view(stdout)
        self.assertEqual(out["feeds_error"], "")
        self.assertEqual(out["health_error"], "")
        self.assertTrue(out["feeds"]["dhan_enabled"])
        self.assertFalse(out["feeds"]["groww_enabled"])
        self.assertTrue(out["feeds"]["dhan_lane_running"])
        self.assertEqual(out["health"]["feeds"][0]["feed"], "dhan")
        self.assertEqual(out["health"]["feeds"][0]["verdict"], "ok")

    def test_parse_feeds_view_curl_failed_is_structured_error_not_zeros(self) -> None:
        stdout = (
            "FEEDS_BEGIN\nTV_CURL_FAILED\nFEEDS_END\n"
            "FEEDS_HEALTH_BEGIN\nTV_CURL_FAILED\nFEEDS_HEALTH_END\n"
        )
        out = handler._parse_feeds_view(stdout)
        self.assertIsNone(out["feeds"])
        self.assertIsNone(out["health"])
        self.assertIn("unreachable", out["feeds_error"])
        self.assertIn("unreachable", out["health_error"])

    def test_parse_feeds_view_empty_stdout_is_box_unreachable(self) -> None:
        out = handler._parse_feeds_view("")
        self.assertIsNone(out["feeds"])
        self.assertIn("SSM offline or instance stopped", out["feeds_error"])
        self.assertIn("SSM offline or instance stopped", out["health_error"])

    def test_parse_feeds_view_invalid_json_is_error(self) -> None:
        stdout = (
            "FEEDS_BEGIN\n{not json\nFEEDS_END\n"
            "FEEDS_HEALTH_BEGIN\n{}\nFEEDS_HEALTH_END\n"
        )
        out = handler._parse_feeds_view(stdout)
        self.assertIsNone(out["feeds"])
        self.assertIn("invalid JSON", out["feeds_error"])
        self.assertEqual(out["health"], {})
        self.assertEqual(out["health_error"], "")


class FeedToggleValidation(unittest.TestCase):
    def test_valid_feed_and_bool_accepted(self) -> None:
        self.assertEqual(handler._validate_feed_toggle("dhan", True), "")
        self.assertEqual(handler._validate_feed_toggle("groww", False), "")
        # Future feed #3: a well-formed lowercase name passes the Lambda; the
        # APP is the authority that 400s unknown feeds (passed through).
        self.assertEqual(handler._validate_feed_toggle("kite", True), "")

    def test_bad_feed_name_rejected(self) -> None:
        for bad in ("", "DHAN", "dhan; rm -rf /", "a b", "../etc", None, 7, ["dhan"]):
            self.assertNotEqual(handler._validate_feed_toggle(bad, True), "", repr(bad))

    def test_enabled_must_be_json_bool(self) -> None:
        for bad in (1, 0, "true", "false", None, "yes"):
            msg = handler._validate_feed_toggle("dhan", bad)
            self.assertIn("boolean", msg, repr(bad))

    def test_lambda_rejects_bad_feed_with_400(self) -> None:
        orig = handler._control_secret
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]
        try:
            resp = handler.lambda_handler(
                {
                    "requestContext": {"http": {"method": "POST"}},
                    "headers": {"authorization": "Bearer s3cret-token"},
                    "body": json.dumps({"action": "feed-toggle", "feed": "x; reboot", "enabled": True}),
                },
                None,
            )
        finally:
            handler._control_secret = orig  # type: ignore[assignment]
        self.assertEqual(resp["statusCode"], 400)

    def test_toggle_commands_target_correct_url_and_body(self) -> None:
        cmds = handler._feed_toggle_commands("groww", True)
        joined = "\n".join(cmds)
        self.assertIn("http://127.0.0.1:3001/api/feeds/groww", joined)
        self.assertIn('{"enabled": true}', joined)
        self.assertIn("-X POST", joined)
        cmds_off = "\n".join(handler._feed_toggle_commands("dhan", False))
        self.assertIn("http://127.0.0.1:3001/api/feeds/dhan", cmds_off)
        self.assertIn('{"enabled": false}', cmds_off)

    def test_feed_toggle_is_not_market_hours_blocked(self) -> None:
        # The APP owns the safety gate (409 on Dhan-disable during live
        # trading); the Lambda must not double-gate a read-mostly feed flip.
        self.assertNotIn("feed-toggle", handler._DESTRUCTIVE)
        self.assertNotIn("feeds-view", handler._DESTRUCTIVE)


class FeedTogglePassthrough(unittest.TestCase):
    DHAN_GUARD = (
        "refusing to disable the Dhan feed while live trading is active "
        "(orders/positions open) — Dhan can only be turned off in the no-orders "
        "data-pull phase, so the system is never blinded mid-trade"
    )

    def test_parse_feed_toggle_200_success(self) -> None:
        stdout = (
            "FEED_TOGGLE_STATUS=200\n"
            "FEED_TOGGLE_BODY_BEGIN\n"
            '{"dhan_enabled": true, "groww_enabled": true, '
            '"dhan_lane_running": true, "groww_lane_running": false}\n'
            "FEED_TOGGLE_BODY_END\n"
        )
        out = handler._parse_feed_toggle(stdout)
        self.assertEqual(out["app_status"], 200)
        self.assertEqual(out["error"], "")
        self.assertTrue(out["app_response"]["groww_enabled"])

    def test_parse_feed_toggle_409_dhan_guard_message_verbatim(self) -> None:
        stdout = (
            "FEED_TOGGLE_STATUS=409\n"
            "FEED_TOGGLE_BODY_BEGIN\n"
            + json.dumps({"error": self.DHAN_GUARD, "allowed": ["groww"]})
            + "\nFEED_TOGGLE_BODY_END\n"
        )
        out = handler._parse_feed_toggle(stdout)
        self.assertEqual(out["app_status"], 409)
        # The app's 409 guard message must survive VERBATIM.
        self.assertEqual(out["app_response"]["error"], self.DHAN_GUARD)

    def test_parse_feed_toggle_curl_000_is_unreachable_not_success(self) -> None:
        out = handler._parse_feed_toggle(
            "FEED_TOGGLE_STATUS=000\nFEED_TOGGLE_BODY_BEGIN\nFEED_TOGGLE_BODY_END\n"
        )
        self.assertIsNone(out["app_status"])
        self.assertIn("unreachable", out["error"])

    def test_parse_feed_toggle_empty_stdout_is_box_unreachable(self) -> None:
        out = handler._parse_feed_toggle("")
        self.assertIsNone(out["app_status"])
        self.assertIn("SSM offline or instance stopped", out["error"])

    def test_lambda_passes_409_through_with_ok_false(self) -> None:
        stdout = (
            "FEED_TOGGLE_STATUS=409\n"
            "FEED_TOGGLE_BODY_BEGIN\n"
            + json.dumps({"error": self.DHAN_GUARD, "allowed": ["groww"]})
            + "\nFEED_TOGGLE_BODY_END\n"
        )
        orig_secret = handler._control_secret
        orig_sync = handler._ssm_shell_sync
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]
        handler._ssm_shell_sync = lambda cmds, timeout=6.0: stdout  # type: ignore[assignment]
        try:
            resp = handler.lambda_handler(
                {
                    "requestContext": {"http": {"method": "POST"}},
                    "headers": {"authorization": "Bearer s3cret-token"},
                    "body": json.dumps({"action": "feed-toggle", "feed": "dhan", "enabled": False}),
                },
                None,
            )
        finally:
            handler._control_secret = orig_secret  # type: ignore[assignment]
            handler._ssm_shell_sync = orig_sync  # type: ignore[assignment]
        self.assertEqual(resp["statusCode"], 200)
        body = json.loads(resp["body"])
        self.assertFalse(body["ok"])
        self.assertEqual(body["app_status"], 409)
        self.assertEqual(body["app_response"]["error"], self.DHAN_GUARD)


class DedupFiveColumnCheck(unittest.TestCase):
    def test_view_comment_names_the_five_real_key_columns(self) -> None:
        # The dedup comment must name the REAL 5-column upsert key
        # (ts, security_id, segment, capture_seq, feed) — not the stale
        # 4-column (received_at) wording.
        src = Path(handler.__file__).read_text(encoding="utf-8")
        self.assertIn("(ts, security_id, segment, capture_seq, feed)", src)
        self.assertNotIn("(ts, security_id, segment, received_at)", src)

    def test_html_distinguishes_ok_disabled_drift_unreachable(self) -> None:
        html = handler._console_html()
        # 5 = OK (green): the check compares against 5, not the stale 4.
        self.assertIn("dkN===5", html)
        self.assertNotIn("==='4'", html)
        # 0 = DEDUP disabled entirely (RED).
        self.assertIn("DEDUP disabled!", html)
        self.assertIn("'bad'", html)
        # other = schema drift (amber).
        self.assertIn("schema drift (expected 5)", html)
        # fetch failure = "unreachable" (amber), never a fake 0/OLD.
        self.assertIn("'unreachable'", html)


class FeedsCardHtml(unittest.TestCase):
    def test_html_has_feeds_card_and_actions(self) -> None:
        html = handler._console_html()
        self.assertIn("loadFeeds()", html)
        self.assertIn("feedToggle(", html)
        self.assertIn("feeds-view", html)
        self.assertIn("feed-toggle", html)
        self.assertIn('id="feeds"', html)
        self.assertIn('id="feedsplit"', html)

    def test_feeds_card_iterates_feed_list_no_hardcoded_names(self) -> None:
        # Future-feeds property: the card renders whatever the app reports —
        # it iterates health.feeds / derives names from *_enabled keys, and
        # must NOT hardcode row('dhan')/row('groww') calls.
        html = handler._console_html()
        self.assertIn("j.health.feeds.map", html)
        self.assertIn("_enabled'", html)
        self.assertNotIn("row('dhan'", html)
        self.assertNotIn("row('groww'", html)

    def test_disable_asks_for_confirmation(self) -> None:
        html = handler._console_html()
        self.assertIn("Turn OFF the ", html)
        self.assertIn("confirm(", html)

    def test_errors_render_verbatim_through_esc(self) -> None:
        html = handler._console_html()
        self.assertIn("esc(j.feeds_error", html)
        self.assertIn("j.app_response.error", html)


class LatencyCardHtml(unittest.TestCase):
    """Per-feed latency comparison card (operator demand 2026-07-03)."""

    @staticmethod
    def _load_latency_js() -> str:
        html = handler._console_html()
        return html.split("async function loadLatency()", 1)[1].split("async function act(", 1)[0]

    def test_html_has_per_feed_table_and_instrument_load_line(self) -> None:
        html = handler._console_html()
        self.assertIn('id="latfeeds"', html)
        self.assertIn('id="latload"', html)
        self.assertIn("instrument load", html)
        self.assertIn("endpoint unknown", html)
        self.assertIn("td.win", html)  # green best-per-column highlight style

    def test_latency_card_iterates_feed_list_no_hardcoded_names(self) -> None:
        # Future-feeds ratchet: the table renders whatever j.feeds carries
        # (the app's own /api/feeds/health list) and highlights via the
        # server-computed winners map — NO feed name may be hardcoded in the
        # portal JS, so a future feed #3 needs zero portal changes.
        js = self._load_latency_js()
        self.assertIn("j.feeds", js)
        self.assertIn("j.winners", js)
        self.assertNotIn("dhan", js.lower())
        self.assertNotIn("groww", js.lower())

    def test_box_wide_cards_labeled_honestly(self) -> None:
        # Tick processing / QuestDB RTT / clock skew carry NO feed label in
        # the app's metrics — the card must say "box-wide", never fake a
        # per-feed processing split.
        html = handler._console_html()
        self.assertIn("box-wide (shared by all feeds)", html)
        self.assertIn("QuestDB round-trip (box-wide)", html)
        self.assertIn("Clock skew (box-wide)", html)


class _FakeEc2:
    """Minimal describe_instances stub for the box-must-be-running gate."""

    def __init__(self, state: str) -> None:
        self._state = state

    def describe_instances(self, InstanceIds):  # noqa: N803 — boto3 casing
        return {"Reservations": [{"Instances": [{"State": {"Name": self._state}}]}]}


class WipeGrowwGate(unittest.TestCase):
    def setUp(self) -> None:
        self._orig_secret = handler._control_secret
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]
        # Off-hours pin — see WipeGate.setUp (audit-fix-#2 hard lock).
        self._orig_mkt = handler._is_market_hours
        handler._is_market_hours = lambda _now: False  # type: ignore[assignment]

    def tearDown(self) -> None:
        handler._control_secret = self._orig_secret  # type: ignore[assignment]
        handler._is_market_hours = self._orig_mkt  # type: ignore[assignment]

    def _wipe_groww(self, force: bool = True, confirm: str = "GROWW"):
        return handler.lambda_handler(
            {
                "requestContext": {"http": {"method": "POST"}},
                "headers": {"authorization": "Bearer s3cret-token"},
                "body": json.dumps({"action": "wipe-groww", "force": force, "confirm": confirm}),
            },
            None,
        )

    def test_wipe_groww_is_in_destructive_set(self) -> None:
        # Membership = market-hours-blocked during 09:15-15:30 IST Mon-Fri.
        self.assertIn("wipe-groww", handler._DESTRUCTIVE)

    def test_wipe_groww_without_confirm_token_is_blocked(self) -> None:
        # The server-side confirm token is the anti-bypass guard: even a
        # forced, authenticated call without {"confirm": "GROWW"} is 409 and
        # never reaches boto3/SSM.
        for bad in ("", "WIPE", "groww", "NUKE"):
            resp = self._wipe_groww(force=True, confirm=bad)
            self.assertEqual(resp["statusCode"], 409, repr(bad))
            self.assertIn("GROWW", json.loads(resp["body"])["error"])

    def test_wipe_groww_requires_running_box(self) -> None:
        # Dispatching the rewrite to a stopped box would silently no-op —
        # the gate must 409 with the real state and never call SSM.
        orig_client = handler._client
        orig_shell = handler._ssm_shell
        handler._client = lambda name, region="": _FakeEc2("stopped")  # type: ignore[assignment]
        handler._ssm_shell = lambda cmds: self.fail("SSM must not be called for a stopped box")  # type: ignore[assignment]
        try:
            resp = self._wipe_groww(force=True)
        finally:
            handler._client = orig_client  # type: ignore[assignment]
            handler._ssm_shell = orig_shell  # type: ignore[assignment]
        self.assertEqual(resp["statusCode"], 409)
        self.assertIn("stopped", json.loads(resp["body"])["error"])

    def test_wipe_groww_forced_is_surgical_and_scoped(self) -> None:
        # The dispatched command list must be groww-SCOPED and SURGICAL:
        # exact-DDL per-table rewrite, dhan replay sources untouched, SEBI
        # tables never named, honest completion markers.
        captured: dict = {}
        orig_client = handler._client
        orig_shell = handler._ssm_shell
        handler._client = lambda name, region="": _FakeEc2("running")  # type: ignore[assignment]
        handler._ssm_shell = lambda cmds: (captured.__setitem__("cmds", cmds) or "cmd-groww")  # type: ignore[assignment]
        try:
            resp = self._wipe_groww(force=True)
        finally:
            handler._client = orig_client  # type: ignore[assignment]
            handler._ssm_shell = orig_shell  # type: ignore[assignment]
        self.assertEqual(resp["statusCode"], 200)
        self.assertEqual(json.loads(resp["body"])["command_id"], "cmd-groww")
        joined = "\n".join(captured["cmds"])
        # app + groww sidecar stopped first; app restarted after.
        self.assertIn("systemctl stop tickvault", joined)
        self.assertIn("pkill -f groww_sidecar.py", joined)
        self.assertIn("systemctl start tickvault", joined)
        # ONLY the groww capture dir removed (capture file + status + bridge
        # offset snapshot) — the resurrection vector, removed BEFORE restart.
        self.assertIn("rm -rf /opt/tickvault/data/groww", joined)
        # Dhan replay sources + caches MUST survive a groww-only wipe.
        for keep in ("/ws_wal", "/spill", "/dlq", "instrument-cache"):
            self.assertNotIn(keep, joined, f"groww-only wipe must NOT touch {keep}")
        # Surgical rewrite, never TRUNCATE (that would kill dhan rows too).
        self.assertNotIn("TRUNCATE", joined)
        # Dynamic candles_* discovery — nothing hardcoded to rot.
        self.assertIn("SELECT table_name FROM tables()", joined)
        self.assertIn("t.startswith('candles_')", joined)
        # Exact canonical DDL anchors (mirrored from tick_persistence.rs +
        # shadow_persistence.rs) incl. the real DEDUP keys.
        self.assertIn("TIMESTAMP(ts) PARTITION BY HOUR WAL", joined)
        self.assertIn("DEDUP ENABLE UPSERT KEYS(%s)", joined)
        self.assertIn("'ts, security_id, segment, capture_seq, feed'", joined)
        self.assertIn("timestamp(ts) PARTITION BY DAY DEDUP UPSERT KEYS(ts, security_id, segment, feed)", joined)
        # Keep-filter preserves NULL-feed legacy dhan rows.
        self.assertIn("feed != 'groww' OR feed IS NULL", joined)
        # Verified copy-before-drop + honest completion markers.
        self.assertIn("GWIPE-ABORT", joined)
        self.assertIn("ORIGINAL UNTOUCHED", joined)
        self.assertIn("GROWW-WIPE-COMPLETE", joined)
        self.assertIn("GROWW-WIPE-PARTIAL", joined)
        # SEBI scope lock: the never-delete tables are never named.
        for sebi in ("_audit", "instrument_lifecycle", "index_constituency", "prev_day_ohlcv", "ws_event"):
            self.assertNotIn(sebi, joined, f"groww wipe must never name {sebi}")

    def test_wipe_groww_never_drops_original_before_verify(self) -> None:
        # Ordering ratchet inside the embedded rewrite: INSERT copy → count
        # verify (with the abort path) → only then DROP the original table.
        py = handler._GROWW_WIPE_PY
        i_insert = py.index("INSERT INTO %s (%s) SELECT")
        i_verify = py.index("copy verify failed")
        i_drop = py.index("q('DROP TABLE %s' % table)")
        self.assertLess(i_insert, i_verify)
        self.assertLess(i_verify, i_drop)
        # The abort path drops the TEMP table, never the original.
        self.assertIn("DROP TABLE IF EXISTS %s' % new", py)
        # The worst-case swap failure names the safe copy + the manual fix.
        self.assertIn("GWIPE-CRITICAL", py)
        self.assertIn("RENAME TABLE %s TO %s", py)


class DataDestructiveMarketHoursLock(unittest.TestCase):
    """Audit fix #2 (operator incident 2026-07-02 15:05 IST): the four
    data-destructive actions have NO force escape during market hours.
    A mid-market forced wipe-ALL + docker-reset deleted ~4.5M rows and 77s
    of live feed — upstream ticks in that window are unrecoverable."""

    def setUp(self) -> None:
        self._orig_secret = handler._control_secret
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]
        # Pin the clock IN-window (market OPEN) — deterministic at any time.
        self._orig_mkt = handler._is_market_hours
        handler._is_market_hours = lambda _now: True  # type: ignore[assignment]
        # Any AWS reach-through from a locked action is a test failure.
        self._orig_shell = handler._ssm_shell
        handler._ssm_shell = lambda cmds: self.fail("SSM must not be called for a market-hours-locked action")  # type: ignore[assignment]

    def tearDown(self) -> None:
        handler._control_secret = self._orig_secret  # type: ignore[assignment]
        handler._is_market_hours = self._orig_mkt  # type: ignore[assignment]
        handler._ssm_shell = self._orig_shell  # type: ignore[assignment]

    def _act(self, action: str, extra: dict):
        return handler.lambda_handler(
            {
                "requestContext": {"http": {"method": "POST"}},
                "headers": {"authorization": "Bearer s3cret-token"},
                "body": json.dumps({"action": action, **extra}),
            },
            None,
        )

    def test_all_data_destructive_actions_locked_in_window_even_with_force(self) -> None:
        # force=true + the CORRECT confirm word — the exact call that wiped
        # the box on 2026-07-02 — must be 409'd for every destructive action.
        confirms = {
            "wipe-questdb": "WIPE",
            "wipe-groww": "GROWW",
            "docker-reset": "NUKE",
            "docker-nuke-bare": "ERASE",
        }
        self.assertEqual(set(confirms), handler._DATA_DESTRUCTIVE)
        for action, word in confirms.items():
            resp = self._act(action, {"force": True, "confirm": word})
            self.assertEqual(resp["statusCode"], 409, action)
            body = json.loads(resp["body"])
            self.assertTrue(body.get("market_hours_locked"), action)
            self.assertIn("locked during market hours", body["error"])
            self.assertIn("never be re-fetched", body["error"])
            self.assertIn("Run after 15:30", body["error"])

    def test_lifecycle_actions_keep_force_override_in_window(self) -> None:
        # Emergencies stay possible: stop/reboot/restart-app/stop-app with
        # force=true must pass BOTH market-hours gates (they then reach the
        # boto3/SSM layer, which we stub — reaching it proves not-blocked).
        orig_client = handler._client
        handler._client = lambda name, region="": _FakeLifecycleEc2()  # type: ignore[assignment]
        handler._ssm_shell = lambda cmds: "cmd-lifecycle"  # type: ignore[assignment]
        try:
            for action in ("stop", "reboot", "restart-app", "stop-app"):
                resp = self._act(action, {"force": True})
                self.assertEqual(resp["statusCode"], 200, action)
        finally:
            handler._client = orig_client  # type: ignore[assignment]

    def test_lifecycle_actions_without_force_still_soft_blocked_in_window(self) -> None:
        # The pre-existing soft gate is unchanged: no force → 409 with the
        # force hint (NOT the hard-lock body).
        resp = self._act("stop", {"force": False})
        self.assertEqual(resp["statusCode"], 409)
        body = json.loads(resp["body"])
        self.assertNotIn("market_hours_locked", body)
        self.assertIn('"force": true', body["error"])

    def test_data_destructive_is_strict_subset_of_destructive(self) -> None:
        self.assertTrue(handler._DATA_DESTRUCTIVE < handler._DESTRUCTIVE)
        for lifecycle in ("stop", "reboot", "restart-app", "stop-app"):
            self.assertNotIn(lifecycle, handler._DATA_DESTRUCTIVE)

    def test_out_of_window_forced_wipe_still_dispatches(self) -> None:
        # After 15:30 IST the operator's forced+confirmed wipe works as before.
        handler._is_market_hours = lambda _now: False  # type: ignore[assignment]
        captured: dict = {}
        handler._ssm_shell = lambda cmds: (captured.__setitem__("cmds", cmds) or "cmd-off-hours")  # type: ignore[assignment]
        resp = self._act("wipe-questdb", {"force": True, "confirm": "WIPE"})
        self.assertEqual(resp["statusCode"], 200)
        self.assertEqual(json.loads(resp["body"])["command_id"], "cmd-off-hours")
        self.assertIn("WIPE-COMPLETE", "\n".join(captured["cmds"]))

    def test_boundary_minutes_pin_exact_semantics(self) -> None:
        # Pinned lock window semantics: 09:15:00 ≤ t < 15:30:00 IST Mon-Fri.
        # 2026-06-01 is a Monday; IST = UTC + 05:30.
        cases = [
            (datetime.datetime(2026, 6, 1, 3, 44, 59), False),  # 09:14:59 IST — open
            (datetime.datetime(2026, 6, 1, 3, 45, 0), True),    # 09:15:00 IST — locked
            (datetime.datetime(2026, 6, 1, 9, 59, 59), True),   # 15:29:59 IST — locked
            (datetime.datetime(2026, 6, 1, 10, 0, 0), False),   # 15:30:00 IST — open
        ]
        for utc, locked in cases:
            self.assertEqual(self._orig_mkt(utc), locked, utc.isoformat())

    def test_danger_zone_shows_lock_label_when_market_open(self) -> None:
        html = handler._console_html()
        # The label exists, starts hidden, and names the lock honestly.
        self.assertIn('id="dangerlock"', html)
        self.assertIn("Locked until 3:30 PM IST", html)
        self.assertIn("even with force", html)
        # loadOverview un-hides it from the server's market_hours flag.
        self.assertIn("dl.hidden=!j.market_hours", html)


class _FakeLifecycleEc2:
    """Stub accepting the lifecycle EC2 calls (stop/reboot)."""

    def stop_instances(self, InstanceIds):  # noqa: N803 — boto3 casing
        return {}

    def reboot_instances(self, InstanceIds):  # noqa: N803 — boto3 casing
        return {}


class HtmlWipeGrowwButton(unittest.TestCase):
    def test_html_has_wipe_groww_button(self) -> None:
        html = handler._console_html()
        self.assertIn("wipeGroww()", html)
        self.assertIn("Wipe GROWW data only", html)
        # Confirm prompt: the operator must type GROWW.
        self.assertIn("Type GROWW to confirm", html)
        # The explanatory line must spell out the scope honestly.
        self.assertIn("Dhan data untouched", html)
        self.assertIn("SEBI", html)

    def test_wipe_groww_sends_confirm_token_and_polls_real_outcome(self) -> None:
        html = handler._console_html()
        self.assertIn("confirm:'GROWW'", html)
        # Truthful async outcome via the shared command-status poller.
        self.assertIn("GROWW-WIPE-COMPLETE", html)
        self.assertIn("GROWW-WIPE-PARTIAL", html)


class RedesignThreeTabs(unittest.TestCase):
    """2026-07-02 portal redesign — Overview / Data / Admin, collapsed danger
    zone, Logs + GitHub tabs removed. UI reorganization ONLY: every action
    semantic, guard, and confirm token is unchanged (pinned by the classes
    above); these tests pin the new structure."""

    def test_logs_and_github_tabs_absent(self) -> None:
        html = handler._console_html()
        self.assertNotIn('data-t="logs"', html)
        self.assertNotIn('data-t="github"', html)
        # ...and their now-unused JS is gone too.
        for dead in ("loadLogs", "loadGithub", "ghMerge", "ghDeploy", "ciBadge", "runSql"):
            self.assertNotIn(dead, html, dead)

    def test_logs_route_kept_for_mcp_server(self) -> None:
        # The tickvault-logs MCP server POSTs {"action":"logs"} to this portal
        # (scripts/mcp-servers/tickvault-logs/server.py) — the API route must
        # survive the tab removal.
        orig_secret = handler._control_secret
        orig_sync = handler._ssm_shell_sync
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]
        handler._ssm_shell_sync = lambda cmds, timeout=6.0: "ERR_BEGIN\nERR_END\nAPP_BEGIN\nAPP_END\n"  # type: ignore[assignment]
        try:
            resp = handler.lambda_handler(
                {
                    "requestContext": {"http": {"method": "POST"}},
                    "headers": {"authorization": "Bearer s3cret-token"},
                    "body": json.dumps({"action": "logs"}),
                },
                None,
            )
        finally:
            handler._control_secret = orig_secret  # type: ignore[assignment]
            handler._ssm_shell_sync = orig_sync  # type: ignore[assignment]
        self.assertEqual(resp["statusCode"], 200)
        self.assertIn("raw", json.loads(resp["body"]))

    def test_gh_actions_are_removed_from_backend(self) -> None:
        # No caller existed outside the deleted GitHub tab → the routes are
        # fully dead and now 400 as unknown actions.
        orig_secret = handler._control_secret
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]
        try:
            for action in ("gh_prs", "gh_merge", "gh_deploy"):
                resp = handler.lambda_handler(
                    {
                        "requestContext": {"http": {"method": "POST"}},
                        "headers": {"authorization": "Bearer s3cret-token"},
                        "body": json.dumps({"action": action}),
                    },
                    None,
                )
                self.assertEqual(resp["statusCode"], 400, action)
                self.assertIn("unknown action", json.loads(resp["body"])["error"])
        finally:
            handler._control_secret = orig_secret  # type: ignore[assignment]

    def test_danger_zone_collapsed_by_default(self) -> None:
        # The four destructive wipes live inside a <details> WITHOUT the open
        # attribute — collapsed until the operator deliberately taps it.
        html = handler._console_html()
        self.assertIn('<details class="fold" id="danger">', html)
        danger = html.split('<details class="fold" id="danger">', 1)[1].split("</details>", 1)[0]
        for needle in ('value="groww"', 'value="wipe"', 'value="nuke"', 'value="erase"', "dangerExecute()"):
            self.assertIn(needle, danger, needle)
        # No <details ... open> anywhere (both folds start collapsed).
        self.assertNotIn("<details open", html)
        self.assertNotIn('id="danger" open', html)

    def test_fold_has_visible_disclosure_affordance(self) -> None:
        # The operator TWICE read the folded danger zone as "the wipes were
        # DELETED" because the fold had no visual cue. Pin: every .fold summary
        # shows a ▸ chevron that flips to ▾ when open (CSS ::after), and the
        # danger summary NAMES its contents so nothing looks removed.
        html = handler._console_html()
        self.assertIn("details.fold>summary::after{ content:' ▸'", html)
        self.assertIn("details.fold[open]>summary::after{ content:' ▾'", html)

    def test_danger_summary_names_its_contents(self) -> None:
        html = handler._console_html()
        summary = html.split('<details class="fold" id="danger">', 1)[1].split("</summary>", 1)[0]
        self.assertIn("(contains: Wipe GROWW · Wipe ALL · Docker reset · Bare nuke)", summary)

    def test_severity_picker_maps_each_choice_to_action_and_token(self) -> None:
        html = handler._console_html()
        # One dispatch map, radio value → the UNCHANGED per-action function.
        self.assertIn("{groww:wipeGroww, wipe:wipeData, nuke:dockerReset, erase:bareNuke}", html)
        # Each function still demands its OWN typed confirm token — the picker
        # introduces no token-bypass path.
        self.assertIn("Type GROWW to confirm:')!=='GROWW'", html)
        self.assertIn("Type WIPE to confirm:')!=='WIPE'", html)
        self.assertIn("Type NUKE-DOCKER to confirm:')!=='NUKE-DOCKER'", html)
        self.assertIn("Type ERASE to confirm:')!=='ERASE'", html)
        # No selection → no dispatch.
        self.assertIn("Pick a danger-zone action first", html)

    def test_context_aware_instance_button(self) -> None:
        # ONE instance button: ▶ Start when stopped, ■ Stop when running —
        # label + action derived from the live instance_state.
        html = handler._console_html()
        self.assertIn('id="instbtn"', html)
        self.assertIn("■ Stop instance", html)
        self.assertIn("▶ Start instance", html)
        self.assertIn("instState==='running'?'stop':'start'", html)
        # Its force checkbox sits next to it and feeds through act().
        self.assertIn('id="force_inst"', html)
        self.assertIn("'force_inst'", html)

    def test_stopped_box_banner_and_grey_shields(self) -> None:
        html = handler._console_html()
        # One calm banner replaces the per-shield "unreachable" scatter…
        self.assertIn('id="stoppedbanner"', html)
        self.assertIn("Box stopped (auto-stops 16:30 IST, auto-starts 08:30 Mon–Fri) — guarantees resume on start", html)
        self.assertIn("$('stoppedbanner').hidden=running", html)
        # …and the shields grey out as "—" ONLY when the box is not running.
        self.assertIn("shieldIdle", html)
        self.assertIn("if(!running){", html)
        # The REAL warning paths for a RUNNING box must still exist (a stopped
        # banner must never hide genuine failures while running).
        self.assertIn("'unreachable'", html)
        self.assertIn("DEDUP disabled!", html)
        self.assertIn("schema drift (expected 5)", html)

    def test_overview_has_aws_strip_and_latency_card(self) -> None:
        html = handler._console_html()
        overview = html.split('<section data-tab="overview"', 1)[1].split("</section>", 1)[0]
        # Thin AWS strip (spend / alarms / disk) with click-to-expand details.
        self.assertIn('id="awsstrip"', overview)
        self.assertIn('id="awsdetails"', overview)
        self.assertIn('id="alarms"', overview)
        self.assertIn('id="storage"', overview)
        # Compact latency card reusing the existing endpoint + renderers.
        self.assertIn("loadLatency()", overview)
        self.assertIn("Measure now", overview)
        self.assertIn('id="latnet"', overview)
        self.assertIn('id="latproc"', overview)
        # Strip is fed by the same aws_status action the old AWS tab used.
        self.assertIn("call('aws_status')", html)

    def test_admin_tab_holds_ops_and_lock(self) -> None:
        html = handler._console_html()
        admin = html.split('<section data-tab="admin"', 1)[1].split("</section>", 1)[0]
        for needle in (
            "act('restart-app')", "act('restart-questdb')", "act('stop-app')",
            'id="force"', 'id="danger"', "lock()",
        ):
            self.assertIn(needle, admin, needle)
        # The bare start/stop instance buttons left the Admin tab — the ONE
        # context-aware button on Overview owns instance lifecycle now.
        self.assertNotIn("act('start')", admin)
        self.assertNotIn("act('stop')", admin)


class TickConservationShield(unittest.TestCase):
    """Audit fix #1 — the shield is a conservation-backed verdict, not a
    liveness heuristic. One test per state + the precedence ratchet +
    the false-OK-is-dead ratchet."""

    # Raw CONSERVE snapshots (QuestDB /exp CSV rows, ';'-joined, header
    # already skipped on the box).
    _BALANCED = '"dhan","2026-07-01T00:00:00.000000Z",0,0,false,"balanced";"groww","2026-07-01T00:00:00.000000Z",0,0,false,"balanced";'
    _RESIDUAL = '"dhan","2026-07-01T00:00:00.000000Z",42,0,false,"leak";'
    _PARTIAL = '"dhan","2026-07-01T00:00:00.000000Z",0,0,true,"partial";'

    def _classify(self, raw: str, disc: str, market_hours: bool, tps: str) -> dict:
        return handler._classify_tick_conservation(handler._parse_conserve_rows(raw), disc, market_hours, tps)

    # ---- one test per shield state ----
    def test_state_balanced_market_open_decorated_with_liveness(self) -> None:
        tc = self._classify(self._BALANCED, "0", True, "500")
        self.assertEqual(tc["state"], "balanced")
        self.assertTrue(tc["good"])
        self.assertEqual(tc["cls"], "ok")
        self.assertIn("BALANCED ✅", tc["label"])
        self.assertIn("capturing (500 ticks/sec)", tc["label"])

    def test_state_balanced_market_closed_names_audit_date(self) -> None:
        tc = self._classify(self._BALANCED, "0", False, "0")
        self.assertEqual(tc["state"], "balanced")
        self.assertEqual(tc["label"], "BALANCED ✅ (audit 2026-07-01)")

    def test_state_residual_is_red_and_counts_ticks(self) -> None:
        tc = self._classify(self._RESIDUAL, "0", True, "500")
        self.assertEqual(tc["state"], "residual")
        self.assertFalse(tc["good"])
        self.assertEqual(tc["cls"], "bad")
        self.assertIn("RESIDUAL: 42", tc["label"])
        self.assertIn("2026-07-01", tc["label"])

    def test_state_partial_is_amber(self) -> None:
        tc = self._classify(self._PARTIAL, "0", True, "500")
        self.assertEqual(tc["state"], "partial")
        self.assertFalse(tc["good"])
        self.assertEqual(tc["cls"], "warn")
        self.assertIn("partial coverage", tc["label"])

    def test_state_disconnects_today_blocks_green_despite_balanced_audit(self) -> None:
        # Yesterday's audit was balanced, but the socket dropped twice TODAY —
        # upstream ticks in those windows never reached the box, so the shield
        # must NOT claim green (the honest capture-at-receipt envelope).
        tc = self._classify(self._BALANCED, "2", True, "500")
        self.assertEqual(tc["state"], "disconnects")
        self.assertFalse(tc["good"])
        self.assertEqual(tc["cls"], "warn")
        self.assertIn("2 disconnect(s) today", tc["label"])
        self.assertIn("unverifiable", tc["label"])

    def test_state_idle_market_closed_no_audit(self) -> None:
        tc = self._classify("", "0", False, "0")
        self.assertEqual(tc["state"], "idle")
        self.assertEqual(tc["label"], "idle (market closed)")

    def test_state_no_audit_market_open_is_amber_never_green(self) -> None:
        # Fresh box: tick_conservation_audit table absent → CONSERVE empty.
        tc = self._classify("", "0", True, "500")
        self.assertEqual(tc["state"], "no_audit")
        self.assertFalse(tc["good"])
        self.assertEqual(tc["cls"], "warn")
        self.assertIn("no audit yet", tc["label"])

    def test_state_unreachable_when_no_source_answers(self) -> None:
        tc = self._classify("", "", True, "")
        self.assertEqual(tc["state"], "unreachable")
        self.assertFalse(tc["good"])
        self.assertEqual(tc["label"], "unreachable")

    # ---- the precedence ratchet: residual > partial > disconnects > balanced
    def test_precedence_residual_beats_partial_and_disconnects(self) -> None:
        raw = (
            '"dhan","2026-07-01T00:00:00.000000Z",0,7,false,"leak";'
            '"groww","2026-07-01T00:00:00.000000Z",0,0,true,"partial";'
        )
        tc = self._classify(raw, "3", True, "500")
        self.assertEqual(tc["state"], "residual")

    def test_precedence_partial_beats_disconnects(self) -> None:
        tc = self._classify(self._PARTIAL, "3", True, "500")
        self.assertEqual(tc["state"], "partial")

    def test_precedence_disconnects_beats_balanced(self) -> None:
        tc = self._classify(self._BALANCED, "1", True, "500")
        self.assertEqual(tc["state"], "disconnects")

    # ---- the false-OK is DEAD: a tick rate alone can never produce green
    def test_tick_rate_alone_never_produces_balanced(self) -> None:
        for raw, disc in (("", "0"), ("", ""), (self._PARTIAL, "0"), (self._BALANCED, "5")):
            tc = self._classify(raw, disc, True, "9999")
            self.assertNotEqual(tc["state"], "balanced", f"raw={raw!r} disc={disc!r}")
            self.assertFalse(tc["good"] and tc["state"] != "idle", f"raw={raw!r} disc={disc!r}")

    def test_only_latest_trading_day_rows_count(self) -> None:
        # Yesterday had a residual; TODAY's audit is balanced — the shield
        # reports the latest day's verdict (yesterday's row is history).
        raw = (
            '"dhan","2026-07-02T00:00:00.000000Z",0,0,false,"balanced";'
            '"dhan","2026-07-01T00:00:00.000000Z",42,0,false,"leak";'
        )
        tc = self._classify(raw, "0", False, "0")
        self.assertEqual(tc["state"], "balanced")
        self.assertEqual(tc["audit_date"], "2026-07-02")

    def test_envelope_sentence_is_carried_on_every_verdict(self) -> None:
        for raw, disc in ((self._BALANCED, "0"), (self._RESIDUAL, "0"), ("", "")):
            tc = self._classify(raw, disc, True, "5")
            self.assertEqual(
                tc["envelope"],
                "Proves every tick that REACHED the box is stored. Ticks Dhan sent "
                "during a disconnect window never arrived and are outside this proof.",
            )

    # ---- parser ----
    def test_parse_conserve_rows_happy_and_malformed(self) -> None:
        rows = handler._parse_conserve_rows(self._BALANCED + "garbage;1,2;")
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]["feed"], "dhan")
        self.assertEqual(rows[0]["date"], "2026-07-01")
        self.assertEqual(rows[0]["outcome"], "balanced")
        self.assertFalse(rows[0]["partial_coverage"])
        self.assertEqual(handler._parse_conserve_rows(""), [])

    # ---- view plumbing ----
    def test_parse_view_carries_conservation_fields(self) -> None:
        out = handler._parse_view(f"CONSERVE={self._BALANCED}\nWS_DISC=0\n")
        self.assertEqual(len(out["conservation_rows"]), 2)
        self.assertEqual(out["ws_disconnects_today"], "0")
        # Empty stdout (box stopped) degrades honestly.
        empty = handler._parse_view("")
        self.assertEqual(empty["conservation_rows"], [])
        self.assertEqual(empty["ws_disconnects_today"], "")

    def test_view_commands_query_both_audit_tables(self) -> None:
        conserve = next(c for c in handler._VIEW_COMMANDS if "CONSERVE=" in c)
        self.assertIn("tick_conservation_audit", conserve)
        # `>` must be URL-encoded (%3E) and the CSV header skipped.
        self.assertIn("ts%20%3E%20dateadd", conserve)
        self.assertIn("tail -n +2", conserve)
        disc = next(c for c in handler._VIEW_COMMANDS if "WS_DISC=" in c)
        self.assertIn("ws_event_audit", disc)
        # `=` encoded as %3D (the DEDUP_KEYS lesson) + in-market kind only.
        self.assertIn("event_kind%3D%27disconnected%27", disc)
        self.assertNotIn("event_kind='disconnected'", disc)

    # ---- the UI no longer holds shield logic ----
    def test_html_renames_shield_and_drops_liveness_heuristic(self) -> None:
        html = handler._console_html()
        self.assertIn("Tick conservation", html)
        self.assertNotIn("No tick lost", html)
        # The old always-green path — capOk driving a WORKING ✅ shield — is gone.
        self.assertNotIn("WORKING ✅", html)
        # The UI renders the server verdict + its envelope tooltip.
        self.assertIn("j.tick_conservation", html)
        self.assertIn("tc.envelope", html)


class QdbConsoleUrlAction(unittest.TestCase):
    """B4: the `qdb_console_url` action mints a 90s one-click HMAC link to the
    QuestDB console front Lambda (env QDB_CONSOLE_URL, TF-injected)."""

    SECRET = "s3cret-token"

    def setUp(self) -> None:
        self._orig = handler._control_secret
        handler._control_secret = lambda: self.SECRET  # type: ignore[assignment]
        self._env = os.environ.pop("QDB_CONSOLE_URL", None)

    def tearDown(self) -> None:
        handler._control_secret = self._orig  # type: ignore[assignment]
        if self._env is not None:
            os.environ["QDB_CONSOLE_URL"] = self._env
        else:
            os.environ.pop("QDB_CONSOLE_URL", None)

    def _dispatch(self) -> dict:
        ev = {
            "requestContext": {"http": {"method": "POST"}},
            "headers": {"authorization": f"Bearer {self.SECRET}"},
            "body": json.dumps({"action": "qdb_console_url"}),
        }
        return handler.lambda_handler(ev, None)

    def test_qdb_console_url_disabled_returns_error(self) -> None:
        # Console not deployed (env empty) → honest error, no fake URL.
        resp = self._dispatch()
        self.assertEqual(resp["statusCode"], 400)
        self.assertIn("console not enabled", json.loads(resp["body"])["error"])

    def test_qdb_console_url_enabled_returns_signed_link(self) -> None:
        import hashlib
        import hmac as hmac_mod
        import time as time_mod

        os.environ["QDB_CONSOLE_URL"] = "https://console.example.test/"
        before = int(time_mod.time())
        resp = self._dispatch()
        after = int(time_mod.time())
        self.assertEqual(resp["statusCode"], 200)
        url = json.loads(resp["body"])["url"]
        # trailing slash on the env base must not produce '//open'
        self.assertTrue(url.startswith("https://console.example.test/open?tok="), url)
        tok = url.split("tok=", 1)[1]
        exp_s, _, sig = tok.partition(".")
        exp = int(exp_s)
        # ~90s TTL (token shape + expiry ratchet)
        self.assertGreaterEqual(exp, before + handler._QDB_LINK_TTL_SECS)
        self.assertLessEqual(exp, after + handler._QDB_LINK_TTL_SECS)
        expected = hmac_mod.new(
            self.SECRET.encode(), f"qdblink|{exp}".encode(), hashlib.sha256
        ).hexdigest()
        self.assertEqual(sig, expected)

    def test_mint_helper_ttl_constant_is_90(self) -> None:
        self.assertEqual(handler._QDB_LINK_TTL_SECS, 90)

    def test_html_has_qdb_console_button(self) -> None:
        html = handler._console_html()
        self.assertIn("Open QuestDB Console", html)
        self.assertIn("qdb_console_url", html)
        self.assertIn("openQdbConsole", html)

    def test_qdb_console_open_is_popup_safe(self) -> None:
        # FIX 4: the window must be opened SYNCHRONOUSLY in the click, before
        # the await, then navigated — so the browser popup blocker allows it.
        html = handler._console_html()
        self.assertIn("window.open('','_blank')", html)
        self.assertIn("w.location=j.url", html)
        self.assertIn("w.close()", html)


class DeployProvenance(unittest.TestCase):
    """B9 deploy provenance — pure formatter + fail-soft GET contract."""

    def test_provenance_line_short_shas(self) -> None:
        line = handler._provenance_line(
            "abc1234def567890abc1234def567890abc12345",
            "def5678900000000000000000000000000000000",
            "fed4321",  # already short hex — must not blow up
        )
        self.assertEqual(line, "binary abc1234 · portal def5678 · main fed4321")

    def test_provenance_line_unknown_values(self) -> None:
        # "unknown" is not hex — the validator maps it (and any other
        # non-hex value) to the literal "unknown"; never raises.
        line = handler._provenance_line("unknown", "unknown", "unknown")
        self.assertEqual(line, "binary unknown · portal unknown · main unknown")

    def test_provenance_line_rejects_markup_as_unknown(self) -> None:
        # 2026-07-03 hardening: a poisoned SSM param / GitHub response
        # carrying markup must render as "unknown", never as markup.
        line = handler._provenance_line(
            "<script>alert(1)</script>",
            '"><img src=x onerror=alert(1)>',
            "abc1234def567890abc1234def567890abc12345",
        )
        self.assertEqual(line, "binary unknown · portal unknown · main abc1234")

    def test_safe_provenance_sha_validation(self) -> None:
        ok = "abc1234def567890abc1234def567890abc12345"
        self.assertEqual(handler._safe_provenance_sha(ok), ok)
        self.assertEqual(handler._safe_provenance_sha("abc1234"), "abc1234")
        for bad in (
            "<script",
            "ABC1234",  # uppercase — validator is lowercase-only
            "abc123",  # 6 chars — below the 7-char floor
            "a" * 41,  # above the 40-char ceiling
            "",
            None,
            1234567,
        ):
            self.assertEqual(handler._safe_provenance_sha(bad), "unknown")

    def test_provenance_footer_html_escapes_line(self) -> None:
        # Defense in depth: even if the formatter ever regressed, the footer
        # html-escapes the assembled line. Prove the escape path by checking
        # the footer for known-safe content and absence of raw markup chars
        # beyond the footer element itself.
        footer = handler._provenance_footer_html()
        self.assertIn("<footer", footer)
        self.assertIn("· portal", footer)
        # Every sha in this env is "unknown" (no AWS/GitHub) — no angle
        # brackets may appear inside the rendered line.
        inner = footer.split(">", 1)[1].rsplit("</footer>", 1)[0]
        self.assertNotIn("<", inner)
        self.assertNotIn(">", inner)

    def test_portal_sha_defaults_to_unknown(self) -> None:
        old = os.environ.pop("PORTAL_GIT_SHA", None)
        try:
            self.assertEqual(handler._portal_sha(), "unknown")
            os.environ["PORTAL_GIT_SHA"] = "1234567890abcdef1234567890abcdef12345678"
            self.assertEqual(
                handler._portal_sha(), "1234567890abcdef1234567890abcdef12345678"
            )
        finally:
            if old is None:
                os.environ.pop("PORTAL_GIT_SHA", None)
            else:
                os.environ["PORTAL_GIT_SHA"] = old

    def test_main_sha_hard_max_age_degrades_to_unknown(self) -> None:
        # 2026-07-03 hardening: on sustained GitHub failure the cached main
        # sha is served only up to the 600s hard max-age; beyond that a
        # failed refresh returns "unknown" instead of the stale value.
        import time as _time
        from unittest import mock

        saved = dict(handler._main_sha_cache)
        sha = "abc1234def567890abc1234def567890abc12345"
        try:
            with mock.patch("urllib.request.urlopen", side_effect=OSError("gh down")):
                now = _time.monotonic()
                # Past the 60s fresh TTL but inside the 600s max-age: the
                # stale-but-bounded cached value is still served.
                handler._main_sha_cache.update({"value": sha, "ts": now - 120.0})
                self.assertEqual(handler._main_sha(), sha)
                # Older than 600s: degrade to "unknown".
                handler._main_sha_cache.update({"value": sha, "ts": now - 601.0})
                self.assertEqual(handler._main_sha(), "unknown")
        finally:
            handler._main_sha_cache.clear()
            handler._main_sha_cache.update(saved)

    def test_get_html_carries_provenance_footer(self) -> None:
        # No AWS creds / no GitHub in the test env — every lookup must
        # fail-soft to "unknown" and the page must still render (the
        # provenance footer can NEVER break the GET).
        resp = handler._html_resp()
        self.assertEqual(resp["statusCode"], 200)
        self.assertIn("· portal", resp["body"])
        self.assertIn("· main", resp["body"])
        self.assertIn("<footer", resp["body"])


if __name__ == "__main__":
    unittest.main()
