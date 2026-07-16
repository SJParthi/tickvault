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
        # REST-era snapshot (2026-07-16): today's rows in the three LIVE
        # tables, totals + per-feed split, the 4-column dedup-key count.
        stdout = (
            "APP=active\n"
            "SPOT_TODAY=1125\n"
            "CHAIN_TODAY=42000\n"
            "CONTRACT_TODAY=9000\n"
            'SPOT_BY_FEED="dhan",750;"groww",375;\n'
            'CHAIN_BY_FEED="dhan",21000;"groww",21000;\n'
            'CONTRACT_BY_FEED="groww",9000;\n'
            "DEDUP_KEYS=4\n"
            "ERRORS_BEGIN\n"
            "Jun 01 11:00 tickvault: WARN something\n"
            "ERRORS_END\n"
        )
        out = handler._parse_view(stdout)
        self.assertEqual(out["app"], "active")
        self.assertEqual(out["rows_today"], {"spot": "1125", "chain": "42000", "contracts": "9000"})
        self.assertEqual(out["rows_today_total"], "52125")
        self.assertEqual(out["rows_by_feed"]["spot"], {"dhan": "750", "groww": "375"})
        self.assertEqual(out["rows_by_feed"]["chain"], {"dhan": "21000", "groww": "21000"})
        self.assertEqual(out["rows_by_feed"]["contracts"], {"groww": "9000"})
        self.assertEqual(out["dedup_key_columns"], "4")
        self.assertEqual(len(out["recent_errors"]), 1)
        self.assertIn("WARN something", out["recent_errors"][0])

    def test_empty_stdout_yields_blank_fields(self) -> None:
        # Box stopped: NO fabricated zeros — the hero shows nothing.
        out = handler._parse_view("")
        self.assertEqual(out["app"], "")
        self.assertEqual(out["dedup_key_columns"], "")
        self.assertEqual(out["rows_today"], {"spot": "", "chain": "", "contracts": ""})
        self.assertEqual(out["rows_today_total"], "")
        self.assertEqual(out["rows_by_feed"], {"spot": {}, "chain": {}, "contracts": {}})
        self.assertEqual(out["recent_errors"], [])

    def test_partial_counts_sum_only_parseable(self) -> None:
        # One table unreachable (empty value) — the total sums the rest,
        # never treating an unreachable count as 0-and-green.
        out = handler._parse_view("SPOT_TODAY=100\nCHAIN_TODAY=\nCONTRACT_TODAY=23\n")
        self.assertEqual(out["rows_today_total"], "123")

    def test_no_error_lines_between_markers(self) -> None:
        out = handler._parse_view("APP=inactive\nERRORS_BEGIN\nERRORS_END\n")
        self.assertEqual(out["app"], "inactive")
        self.assertEqual(out["recent_errors"], [])


class HonestHeroHtml(unittest.TestCase):
    """Review fix M4 (2026-07-16): _sum_counts promises the hero "shows
    nothing rather than a fabricated 0", but the JS did
    countUp(..., j.rows_today_total||'0') — a stopped/unreachable box
    rendered an animated 0. The hero + Data-tab bars must degrade to '—'."""

    def test_hero_never_renders_fabricated_zero(self) -> None:
        html = handler._console_html()
        self.assertNotIn("j.rows_today_total||'0'", html)
        self.assertIn("$('ticksbig').textContent='—'", html)
        # countUp still runs on a REAL total (the count-up animation stays).
        self.assertIn("countUp($('ticksbig'), j.rows_today_total)", html)

    def test_bars_render_dash_for_unreadable_counts(self) -> None:
        html = handler._console_html()
        # An empty per-table count maps to null (never parseInt→0)…
        self.assertIn(".trim()===''?null:", html)
        # …and bar() renders '—' for a null count instead of a 0 bar.
        self.assertIn("(miss?'—':n.toLocaleString())", html)


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

    def test_dedup_key_query_targets_spot_1m_rest(self) -> None:
        # REST-era repoint (2026-07-16): the shield reads spot_1m_rest's
        # 4-column key (ts, security_id, exchange_segment, feed per
        # DEDUP_KEY_SPOT_1M_REST), not the retired ticks 5-column key.
        dedup_cmd = next(c for c in handler._VIEW_COMMANDS if "DEDUP_KEYS=" in c)
        self.assertIn("table_columns(%27spot_1m_rest%27)", dedup_cmd)
        self.assertNotIn("'ticks'", dedup_cmd)

    def test_view_commands_target_live_rest_tables(self) -> None:
        joined = "\n".join(handler._VIEW_COMMANDS)
        for live in ("spot_1m_rest", "option_chain_1m", "option_contract_1m_rest"):
            self.assertIn(live, joined, live)
        # Today windows use the house `ts IN today()` convention.
        self.assertIn("ts%20IN%20today()", joined)

    def test_db_console_default_query_targets_live_table(self) -> None:
        html = handler._console_html()
        self.assertIn("SELECT * FROM spot_1m_rest ORDER BY ts DESC LIMIT 50", html)
        self.assertNotIn("SELECT * FROM ticks ORDER BY ts DESC LIMIT 50", html)


class RestLatency(unittest.TestCase):
    """REST-era latency snapshot (2026-07-16 cleanup): per-(feed, leg)
    prompt-pull percentiles from rest_fetch_audit + the KEPT box-wide probes
    (QuestDB RTT, clock skew, dormant order-placement histogram). The old
    per-feed WS TCP/TLS probe table, the exchange->received lag-percentile
    grid over `ticks`, and the tick-path histogram machinery measured
    retired edges / dead emitters and are gone (see
    LegacyLiveFeedPanelsRemoved)."""

    def test_avg_ns(self) -> None:
        self.assertEqual(handler._avg_ns("1000", "10"), 100.0)
        self.assertIsNone(handler._avg_ns("1000", "0"))
        self.assertIsNone(handler._avg_ns("", ""))

    def test_rest_latency_sql_filters_ok_and_sentinel(self) -> None:
        # outcome='ok' rows only; close_to_data_ms >= 0 drops the -1
        # not-measured sentinel AND satisfies approx_percentile's
        # non-negative-input requirement; today window + per-(feed, leg).
        sql = handler._rest_latency_sql()
        self.assertIn("from rest_fetch_audit", sql)
        self.assertIn("outcome = 'ok'", sql)
        self.assertIn("close_to_data_ms >= 0", sql)
        self.assertIn("approx_percentile(close_to_data_ms, 0.5, 3)", sql)
        self.assertIn("approx_percentile(close_to_data_ms, 0.99, 3)", sql)
        self.assertIn("ts in today()", sql)
        self.assertIn("group by feed, leg", sql)

    def test_latency_commands_rest_era_bounded(self) -> None:
        joined = "\n".join(handler._LATENCY_COMMANDS)
        # The audit aggregate is re-emitted as RESTLAT_ROW= labeled lines,
        # every curl is --max-time bounded, and the box-wide probes are kept.
        self.assertIn("RESTLAT_ROW=", joined)
        self.assertIn("rest_fetch_audit", joined)
        self.assertIn("--max-time", joined)
        self.assertIn("QDB=", joined)
        self.assertIn("SKEW=", joined)
        self.assertIn("tv_order_placement_duration_ns", joined)
        # The retired live-feed probes must never be dialed again.
        self.assertNotIn("api-feed.dhan.co", joined)
        self.assertNotIn("socket-api.groww.in", joined)
        self.assertNotIn("FROM ticks", joined)
        self.assertNotIn("received_at", joined)

    def test_latency_timeout_reduced_with_margin(self) -> None:
        # No 25s WS-probe fan-out anymore — worst case is one 3s metrics curl
        # + one 3s QDB probe + one 4s audit read + SSM registration.
        self.assertEqual(handler._LATENCY_TIMEOUT_SECS, 15.0)
        self.assertEqual(handler._REST_LAT_QUERY_MAX_SECS, 4)

    def test_parse_rest_lat_row_happy(self) -> None:
        row = handler._parse_rest_lat_row('"dhan","spot_1m",370,1450.04,5200.55')
        self.assertEqual(
            row,
            {"feed": "dhan", "leg": "spot_1m", "ok_rows": 370, "p50_ms": 1450.0, "p99_ms": 5200.6},
        )

    def test_parse_rest_lat_row_null_percentiles_degrade_to_none(self) -> None:
        row = handler._parse_rest_lat_row("groww,chain_1m,0,null,NaN")
        self.assertEqual(row["ok_rows"], 0)
        self.assertIsNone(row["p50_ms"])
        self.assertIsNone(row["p99_ms"])

    def test_parse_rest_lat_row_rejects_malformed_and_bad_tokens(self) -> None:
        # Malformed on-box output yields NOTHING — never a fabricated row.
        for bad in (
            "",
            "a,b",
            "dhan,spot_1m,x,1,2",
            "DH AN,leg,1,2,3",
            "<script>,leg,1,2,3",
            "dhan,<b>leg</b>,1,2,3",
            "dhan,leg,-5,1,2",
            "dhan,leg,1,2,3,4",
        ):
            self.assertIsNone(handler._parse_rest_lat_row(bad), repr(bad))

    def test_parse_latency_full(self) -> None:
        stdout = (
            "METRICS_BEGIN\n"
            "tv_order_placement_duration_ns_sum 5000\n"
            "tv_order_placement_duration_ns_count 10\n"
            "METRICS_END\n"
            "QDB=0.0021\n"
            "SKEW=0.000123\n"
            'RESTLAT_ROW="dhan","chain_1m",374,1300.0,2100.0\n'
            'RESTLAT_ROW="groww","spot_1m",372,900.0,1600.0\n'
        )
        out = handler._parse_latency(stdout)
        self.assertEqual(out["questdb_ms"], "2.1")
        self.assertEqual(out["clock_skew_ms"], "0.1")
        self.assertEqual(out["order_place_avg_ns"], 500.0)
        rows = {(r["feed"], r["leg"]): r for r in out["rest_latency"]}
        self.assertEqual(set(rows), {("dhan", "chain_1m"), ("groww", "spot_1m")})
        self.assertEqual(rows[("dhan", "chain_1m")]["ok_rows"], 374)
        self.assertEqual(rows[("dhan", "chain_1m")]["p99_ms"], 2100.0)
        self.assertEqual(rows[("groww", "spot_1m")]["p50_ms"], 900.0)

    def test_parse_latency_empty(self) -> None:
        # Box stopped -> empty table + blank shields, never fabricated numbers.
        out = handler._parse_latency("")
        self.assertEqual(out["rest_latency"], [])
        self.assertEqual(out["questdb_ms"], "")
        self.assertEqual(out["clock_skew_ms"], "")
        self.assertIsNone(out["order_place_avg_ns"])

    def test_parse_latency_malformed_rest_rows_skipped(self) -> None:
        out = handler._parse_latency(
            "RESTLAT_ROW=garbage\nRESTLAT_ROW=dhan,spot_1m,10,1.0,2.0\n"
        )
        self.assertEqual(len(out["rest_latency"]), 1)
        self.assertEqual(out["rest_latency"][0]["feed"], "dhan")


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
        for needle in ('id="dbtables"', 'id="dbsql"', 'id="dbout"', 'id="dbcols"', 'id="bars"'):
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
        # all three destructive actions (wipe-questdb / docker-reset /
        # docker-nuke-bare; the wipe-groww action that pioneered this gate
        # was removed 2026-07-16 with the Groww live feed).
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
        disable_pos = src.index('"systemctl disable tickvault || true",')
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

    def test_wipe_questdb_truncates_live_rest_tables_too(self) -> None:
        # 2026-07-16 destructive-surface extension (called out in the commit
        # body): a "fresh start" must also drop today's official minute
        # candles + the fetch log — the four LIVE tables the REST pulls
        # write. rest_fetch_audit is per-fetch forensics, NOT a SEBI
        # never-delete table; the SEBI *_audit family stays preserved by the
        # dynamic filter (it matches only the named targets).
        import inspect

        src = inspect.getsource(handler)
        wipe_block = src.split('action == "wipe-questdb"', 1)[1].split('action == "docker-reset"', 1)[0]
        self.assertIn(
            "{'spot_1m_rest', 'option_chain_1m', 'option_contract_1m_rest', 'rest_fetch_audit'}",
            wipe_block,
        )
        self.assertIn("t in live_rest", wipe_block)
        # Review fix M2 (2026-07-16): honest completion verifies EVERY
        # truncate-target family — the legacy pair AND all FOUR live REST
        # tables (a TRUNCATE-FAILED on option_chain_1m /
        # option_contract_1m_rest / rest_fetch_audit previously still
        # printed WIPE-COMPLETE — false-OK, audit Rule 11).
        for t in (
            "ticks",
            "candles_1m",
            "spot_1m_rest",
            "option_chain_1m",
            "option_contract_1m_rest",
            "rest_fetch_audit",
        ):
            self.assertIn(f"$(qc {t})", wipe_block, t)
        self.assertIn("spot_1m_rest=${S:-?}", wipe_block)
        # Review fix M2: a missing/erroring count defaults to 0 (absent
        # table = nothing left = wiped). The old default-to-1 made EVERY
        # post-nuke wipe read WIPE-PARTIAL forever, because nothing
        # recreates ticks/candles_1m on the REST-only runtime (their
        # ensure-DDL has zero callers). The per-table TRUNCATE-FAILED lines
        # remain the loud failure path for a live-table truncate error.
        for default in ("${T:-0}", "${C:-0}", "${S:-0}", "${O:-0}", "${K:-0}", "${A:-0}"):
            self.assertIn(default, wipe_block, default)
            self.assertNotIn(default.replace(":-0", ":-1"), wipe_block, default)
        self.assertIn("TRUNCATE-FAILED", wipe_block)

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


class RemovedActionsReturn400(unittest.TestCase):
    """2026-07-16 cleanup: the cross-verify card (its producer was deleted in
    PR-C3 2026-07-14 — the card sat frozen at a 2026-07-13 FAIL forever) and
    the groww-only wipe (it rewrote only the frozen legacy ticks/candles_*
    tables) are REMOVED. Their POST actions fall through to the
    unknown-action 400 — never a silent success."""

    def setUp(self) -> None:
        self._orig = handler._control_secret
        handler._control_secret = lambda: "s3cret-token"  # type: ignore[assignment]
        self._orig_mkt = handler._is_market_hours
        handler._is_market_hours = lambda _now: False  # type: ignore[assignment]

    def tearDown(self) -> None:
        handler._control_secret = self._orig  # type: ignore[assignment]
        handler._is_market_hours = self._orig_mkt  # type: ignore[assignment]

    def _post(self, action: str, extra: dict | None = None):
        return handler.lambda_handler(
            {
                "requestContext": {"http": {"method": "POST"}},
                "headers": {"authorization": "Bearer s3cret-token"},
                "body": json.dumps({"action": action, **(extra or {})}),
            },
            None,
        )

    def test_cross_verify_action_removed(self) -> None:
        resp = self._post("cross_verify")
        self.assertEqual(resp["statusCode"], 400)
        self.assertIn("unknown action", json.loads(resp["body"])["error"])

    def test_wipe_groww_action_removed(self) -> None:
        # Even the historically-correct payload (force + typed confirm) must
        # 400 — the action no longer exists and never reaches SSM.
        orig_shell = handler._ssm_shell
        handler._ssm_shell = lambda cmds: self.fail("SSM must not be called for a removed action")  # type: ignore[assignment]
        try:
            resp = self._post("wipe-groww", {"force": True, "confirm": "GROWW"})
        finally:
            handler._ssm_shell = orig_shell  # type: ignore[assignment]
        self.assertEqual(resp["statusCode"], 400)
        self.assertIn("unknown action", json.loads(resp["body"])["error"])

    def test_removed_actions_left_the_destructive_sets(self) -> None:
        self.assertNotIn("wipe-groww", handler._DESTRUCTIVE)
        self.assertNotIn("wipe-groww", handler._DATA_DESTRUCTIVE)
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

    def test_view_commands_query_live_tables_grouped_by_feed(self) -> None:
        for label, table in (
            ("SPOT_BY_FEED=", "spot_1m_rest"),
            ("CHAIN_BY_FEED=", "option_chain_1m"),
            ("CONTRACT_BY_FEED=", "option_contract_1m_rest"),
        ):
            cmd = next(c for c in handler._VIEW_COMMANDS if label in c)
            self.assertIn("GROUP%20BY%20feed", cmd)
            self.assertIn(f"FROM%20{table}", cmd)


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

    def test_parse_feeds_view_rest_lane_fields(self) -> None:
        # The REST-lane pulse rides the same snapshot as labeled lines
        # OUTSIDE the marker blocks (2026-07-16).
        stdout = (
            "FEEDS_BEGIN\n{}\nFEEDS_END\n"
            "FEEDS_HEALTH_BEGIN\n{}\nFEEDS_HEALTH_END\n"
            'REST_AUDIT="dhan","ok",370;"dhan","error",3;"groww","ok",372;"groww","rate_limited",2;\n'
            'REST_LAT_HOUR="dhan",1450.04,5200.55;"groww",900.0,1600.0;\n'
        )
        out = handler._parse_feeds_view(stdout)
        self.assertEqual(out["rest_audit"]["dhan"], {"ok": 370, "error": 3})
        self.assertEqual(out["rest_audit"]["groww"], {"ok": 372, "rate_limited": 2})
        self.assertEqual(out["rest_lat_hour"]["dhan"], {"p50_ms": 1450.0, "p99_ms": 5200.6})
        self.assertEqual(out["rest_lat_hour"]["groww"], {"p50_ms": 900.0, "p99_ms": 1600.0})

    def test_parse_feeds_view_rest_lane_empty_or_garbage_is_empty(self) -> None:
        # Empty/absent/garbage values yield {} — the card then says
        # "no pulls recorded today", never fabricated zeros (Rule 11).
        out = handler._parse_feeds_view("FEEDS_BEGIN\n{}\nFEEDS_END\n")
        self.assertEqual(out["rest_audit"], {})
        self.assertEqual(out["rest_lat_hour"], {})
        self.assertEqual(handler._parse_rest_audit(";;garbage;a,b;<x>,ok,3;dhan,ok,x;"), {})
        self.assertEqual(handler._parse_rest_lat_hour(";;garbage;dhan,null,null;"), {})

    def test_parse_feeds_view_json_body_never_mistaken_for_labeled_line(self) -> None:
        # A '=' inside the marker-delimited JSON must not leak into the
        # labeled-line scan.
        stdout = 'FEEDS_BEGIN\n{"note": "REST_AUDIT=fake"}\nFEEDS_END\n'
        out = handler._parse_feeds_view(stdout)
        self.assertEqual(out["rest_audit"], {})


class FeedsViewCommandsPinned(unittest.TestCase):
    """Review fixes M1 + M5 (2026-07-16): the feeds-card snapshot commands
    were unpinned (a revert to the retired ticks/subscribed counters would
    have passed the suite) and the SSM budget sat BELOW the worst-case curl
    total, so a slow-but-RUNNING box read as the FALSE "box unreachable".
    The SQL literals below appear RAW in the commands — `curl -G
    --data-urlencode` does the URL-encoding at request time."""

    def test_feeds_timeout_budget_exceeds_curl_max_time_sum(self) -> None:
        # M1: parse every --max-time out of the command strings (drift-proof
        # — adding a curl without raising the budget fails this test).
        import re

        total = sum(
            int(m)
            for c in handler._FEEDS_VIEW_COMMANDS
            for m in re.findall(r"--max-time\s+(\d+)", c)
        )
        self.assertEqual(total, 24)  # 2×8s app curls + 2×4s audit curls
        self.assertGreater(handler._FEEDS_TIMEOUT_SECS, total)
        self.assertEqual(handler._FEEDS_TIMEOUT_SECS, 28.0)

    def test_rest_audit_curl_targets_todays_fetch_log(self) -> None:
        cmd = next(c for c in handler._FEEDS_VIEW_COMMANDS if "REST_AUDIT=" in c)
        self.assertIn("from rest_fetch_audit", cmd)
        self.assertIn("ts in today()", cmd)
        self.assertIn("group by feed, outcome", cmd)
        self.assertIn("--data-urlencode", cmd)

    def test_rest_lat_hour_curl_ist_timebase_ok_filter_and_sentinel(self) -> None:
        cmd = next(c for c in handler._FEEDS_VIEW_COMMANDS if "REST_LAT_HOUR=" in c)
        # IST timebase: ts is IST-shifted while QuestDB now() is UTC — the
        # window compares against dateadd('m', 330, now()) (2026-07-07 lesson).
        self.assertIn("dateadd('m', 330, now())", cmd)
        self.assertIn("dateadd('h', -1,", cmd)
        # Successful pulls only + the -1 not-measured sentinel excluded.
        self.assertIn("outcome = 'ok'", cmd)
        self.assertIn("close_to_data_ms >= 0", cmd)
        self.assertIn("from rest_fetch_audit", cmd)
        self.assertIn("approx_percentile(close_to_data_ms, 0.5, 3)", cmd)
        self.assertIn("approx_percentile(close_to_data_ms, 0.99, 3)", cmd)


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


class DedupKeyShield(unittest.TestCase):
    """The dedup shield reads spot_1m_rest's REAL 4-column upsert key
    (ts, security_id, exchange_segment, feed per DEDUP_KEY_SPOT_1M_REST in
    crates/storage/src/spot_1m_rest_persistence.rs) — repointed 2026-07-16
    from the retired ticks 5-column key."""

    def test_view_comment_names_the_four_real_key_columns(self) -> None:
        src = Path(handler.__file__).read_text(encoding="utf-8")
        self.assertIn("(ts, security_id, exchange_segment, feed)", src)
        self.assertIn("DEDUP_KEY_SPOT_1M_REST", src)

    def test_html_distinguishes_ok_disabled_drift_unreachable(self) -> None:
        html = handler._console_html()
        # 4 = OK (green): the check compares against 4, not the stale 5.
        self.assertIn("dkN===4", html)
        self.assertNotIn("dkN===5", html)
        # 0 = DEDUP disabled entirely (RED).
        self.assertIn("DEDUP disabled!", html)
        self.assertIn("'bad'", html)
        # other = schema drift (amber).
        self.assertIn("schema drift (expected 4)", html)
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
        # Review fix M3 (2026-07-16): the confirm dialog speaks REST-lane
        # truth — there is no live feed and no ticks; turning a broker off
        # stops its official-candle pulls.
        html = handler._console_html()
        self.assertIn("confirm(", html)
        self.assertIn(
            "official-candle pulls (spot + option chain) stop until you turn it back on",
            html,
        )
        self.assertNotIn("live feed? Ticks", html)

    def test_failed_bucket_excludes_never_attempted_minutes(self) -> None:
        # Review fix L4 (2026-07-16): skipped / boundary_skipped audit rows
        # are minutes the leg never ATTEMPTED (trading-day gate, missed
        # boundaries) — lumping them into "failed" inflated the failure
        # count. no_token IS a real failure and stays counted.
        html = handler._console_html()
        self.assertIn("k!=='skipped'", html)
        self.assertIn("k!=='boundary_skipped'", html)
        # no_token must NOT be excluded from the failed fold.
        self.assertNotIn("k!=='no_token'", html)

    def test_errors_render_verbatim_through_esc(self) -> None:
        html = handler._console_html()
        self.assertIn("esc(j.feeds_error", html)
        self.assertIn("j.app_response.error", html)

    def test_feeds_card_shows_rest_pull_line_not_tick_counters(self) -> None:
        # 2026-07-16: the per-feed detail line is today's fetch-log pulse
        # (ok/failed/rate-limited + last-hour p50/p99 after minute close),
        # sourced from rest_fetch_audit — the old ticks/subscribed counters
        # read frozen live-feed registries and are GONE.
        html = handler._console_html()
        self.assertIn("j.rest_audit", html)
        self.assertIn("j.rest_lat_hour", html)
        self.assertIn("pulls today:", html)
        self.assertIn("rate-limited", html)
        self.assertIn("after minute close", html)
        self.assertIn("no official-candle pulls recorded today", html)
        self.assertNotIn("ticks_total", html)
        self.assertNotIn("subscribed_total", html)
        self.assertNotIn("' · ticks '", html)
        self.assertNotIn("' · subscribed '", html)


class LatencyCardHtml(unittest.TestCase):
    """REST-era latency card (2026-07-16): per-(broker, pull type) prompt-pull
    percentiles from today's fetch log + the kept box-wide shields."""

    @staticmethod
    def _load_latency_js() -> str:
        html = handler._console_html()
        return html.split("async function loadLatency()", 1)[1].split("async function act(", 1)[0]

    def test_html_has_rest_latency_table(self) -> None:
        html = handler._console_html()
        self.assertIn('id="latrest"', html)
        self.assertIn("how fast each official minute candle arrives", html)
        self.assertIn("pull type", html)
        self.assertIn("ok pulls today", html)
        self.assertIn("p50 after close", html)
        self.assertIn("p99 after close", html)

    def test_latency_card_iterates_rows_no_hardcoded_names(self) -> None:
        # Future-feeds ratchet: the table renders whatever j.rest_latency
        # carries (feed+leg discovered from the fetch log at measure time) —
        # NO feed/leg name may be hardcoded in the portal JS.
        js = self._load_latency_js()
        self.assertIn("j.rest_latency", js)
        self.assertNotIn("dhan", js.lower())
        self.assertNotIn("groww", js.lower())
        self.assertNotIn("spot_1m", js)
        self.assertNotIn("chain_1m", js)

    def test_empty_table_is_honest_not_fake_zero(self) -> None:
        html = handler._console_html()
        self.assertIn("no successful pulls recorded today", html)
        self.assertIn("never fake", html)

    def test_box_wide_cards_labeled_honestly(self) -> None:
        # QuestDB RTT / clock skew carry NO feed label — the card must say
        # "box-wide"; the dormant order-placement shield is KEPT and reads
        # "—" until live trading returns.
        html = handler._console_html()
        self.assertIn("box-wide (shared by all pulls)", html)
        self.assertIn("QuestDB round-trip (box-wide)", html)
        self.assertIn("Clock skew (box-wide)", html)
        self.assertIn("'Order placement'", html)
        self.assertIn("dormant", html)


class DataDestructiveMarketHoursLock(unittest.TestCase):
    """Audit fix #2 (operator incident 2026-07-02 15:05 IST): the
    data-destructive actions (three since the 2026-07-16 wipe-groww removal)
    have NO force escape during market hours. A mid-market forced wipe-ALL +
    docker-reset deleted ~4.5M rows and 77s of live feed — upstream data in
    that window is unrecoverable."""

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
            "docker-reset": "NUKE-DOCKER",
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


class LegacyLiveFeedPanelsRemoved(unittest.TestCase):
    """2026-07-16 REST-only cleanup ratchet: the live-feed-era helpers and
    panels must not resurrect — their producers were deleted with the live
    feeds (Dhan WS 2026-07-13 PR-C2/C3, Groww WS 2026-07-15)."""

    def test_backend_helpers_gone(self) -> None:
        for name in (
            "_classify_tick_conservation",
            "_parse_conserve_rows",
            "_CONSERVATION_ENVELOPE",
            "_parse_cross_verify",
            "_CROSS_VERIFY_COMMANDS",
            "_GROWW_WIPE_PY",
            "_wipe_groww_commands",
            "_FEED_LIVE_HOSTS",
        ):
            self.assertFalse(hasattr(handler, name), name)
        # The pctl/windowed-histogram machinery is gone from the source too.
        src_text = Path(handler.__file__).read_text(encoding="utf-8")
        self.assertNotIn("_pctl_", src_text)
        self.assertNotIn("percentile_winners", src_text)
        self.assertNotIn("tv_tick_processing_duration_ns", src_text)
        self.assertNotIn("tv_wire_to_done_duration_ns", src_text)

    def test_retired_ws_probe_hosts_absent_from_code(self) -> None:
        # Review fix L1 (2026-07-16): the retired WS probe hosts legitimately
        # appear in dated retirement COMMENTS, so scan the source with
        # #-comments stripped (tokenize — never a naive '#' split, which
        # would also eat CSS colours inside string literals) and assert the
        # hosts appear NOWHERE in code (strings, URLs, commands).
        import io
        import tokenize

        src_text = Path(handler.__file__).read_text(encoding="utf-8")
        code_only = " ".join(
            tok.string
            for tok in tokenize.generate_tokens(io.StringIO(src_text).readline)
            if tok.type != tokenize.COMMENT
        )
        for host in ("api-feed.dhan.co", "socket-api.groww.in"):
            self.assertNotIn(host, code_only, host)
            # Sanity: the scan is non-vacuous — the host IS still present in
            # the raw source (inside the dated retirement comments).
            self.assertIn(host, src_text, host)

    def test_html_panels_gone(self) -> None:
        html = handler._console_html()
        for dead in (
            "drawSpark",
            'id="spark"',
            'id="p_tps"',
            "peak ticks/sec",
            "loadCrossVerify",
            'id="cvshields"',
            "wipeGroww",
            'value="groww"',
            'id="latfeeds"',
            'id="latpctl"',
            "Tick conservation",
            "Sub-second fix",
            "Peak ticks / second",
            "ticks captured today",
            "GROWW-WIPE-COMPLETE",
        ):
            self.assertNotIn(dead, html, dead)

    def test_view_sql_targets_no_dead_tables(self) -> None:
        joined = "\n".join(handler._VIEW_COMMANDS)
        for dead in (
            "FROM%20ticks",
            "candles_1m",
            "tick_conservation_audit",
            "ws_event_audit",
            "MAX_TPS",
            "TICKS_TODAY",
            "CONSERVE",
            "WS_DISC",
        ):
            self.assertNotIn(dead, joined, dead)


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
        # The three destructive wipes live inside a <details> WITHOUT the open
        # attribute — collapsed until the operator deliberately taps it.
        # (The groww-only wipe left with the Groww live feed, 2026-07-16.)
        html = handler._console_html()
        self.assertIn('<details class="fold" id="danger">', html)
        danger = html.split('<details class="fold" id="danger">', 1)[1].split("</details>", 1)[0]
        for needle in ('value="wipe"', 'value="nuke"', 'value="erase"', "dangerExecute()"):
            self.assertIn(needle, danger, needle)
        self.assertNotIn('value="groww"', danger)
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
        self.assertIn("(contains: Wipe ALL · Docker reset · Bare nuke)", summary)

    def test_severity_picker_maps_each_choice_to_action_and_token(self) -> None:
        html = handler._console_html()
        # One dispatch map, radio value → the UNCHANGED per-action function.
        self.assertIn("{wipe:wipeData, nuke:dockerReset, erase:bareNuke}", html)
        # Each function still demands its OWN typed confirm token — the picker
        # introduces no token-bypass path.
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
        # …and the shield greys out as "—" ONLY when the box is not running.
        self.assertIn("shieldIdle", html)
        self.assertIn("if(!running){", html)
        # The REAL warning paths for a RUNNING box must still exist (a stopped
        # banner must never hide genuine failures while running).
        self.assertIn("'unreachable'", html)
        self.assertIn("DEDUP disabled!", html)
        self.assertIn("schema drift (expected 4)", html)

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
        self.assertIn('id="latrest"', overview)
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
