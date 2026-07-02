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
        # Windowed fields degrade to None / 0 with no scrapes at all.
        self.assertIsNone(out["tick_p50_ns"])
        self.assertIsNone(out["tick_p99_ns"])
        self.assertEqual(out["tick_window_count"], 0)

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
            "DHAN_BEGIN\n"
            "0.012 0.030\n"
            "DHAN_END\n"
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

    def tearDown(self) -> None:
        handler._control_secret = self._orig  # type: ignore[assignment]

    def _wipe(self, force: bool):
        return handler.lambda_handler(
            {
                "requestContext": {"http": {"method": "POST"}},
                "headers": {"authorization": "Bearer s3cret-token"},
                "body": json.dumps({"action": "wipe-questdb", "force": force}),
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

    def _docker_reset(self, force: bool):
        return handler.lambda_handler(
            {
                "requestContext": {"http": {"method": "POST"}},
                "headers": {"authorization": "Bearer s3cret-token"},
                "body": json.dumps({"action": "docker-reset", "force": force}),
            },
            None,
        )

    def test_docker_reset_without_force_is_blocked(self) -> None:
        # Force-required guard (409) OR the destructive market-hours guard (409)
        # — must never reach SSM/boto3 without an explicit force.
        resp = self._docker_reset(force=False)
        self.assertEqual(resp["statusCode"], 409)

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

    def _docker_nuke_bare(self, force: bool):
        return handler.lambda_handler(
            {
                "requestContext": {"http": {"method": "POST"}},
                "headers": {"authorization": "Bearer s3cret-token"},
                "body": json.dumps({"action": "docker-nuke-bare", "force": force}),
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
        html = handler._console_html()
        self.assertIn("dockerReset()", html)
        self.assertIn("Full Docker reset", html)
        # The nuke must spell out the SEBI-audit-data loss + require typing NUKE.
        self.assertIn("NUKE", html)
        self.assertIn("audit", html)

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


if __name__ == "__main__":
    unittest.main()
