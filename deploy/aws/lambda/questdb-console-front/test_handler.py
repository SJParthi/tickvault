"""Unit tests for the questdb-console-front Lambda (B4) — pure functions only.

Runs without AWS credentials or boto3 (all clients are lazy). Covers:
  * SQL-gate PARITY with operator-control/_is_safe_sql (the contract) over an
    adversarial corpus — smuggling, chaining, comments, banned mutators.
  * Auth: link token / session cookie / Bearer / login POST — expired, forged,
    garbage, missing; constant-time compare asserted via source inspection.
  * Path/method deny-by-default gate.
  * Relay plumbing: base64 passthrough, header preservation, size cap → 502,
    box offline → 503.
  * Deployed-bytes proof marker (proof-3 ratchet).

Run with:  python3 -m unittest test_handler
"""

from __future__ import annotations

import base64
import importlib.util
import inspect
import json
import os
import sys
import time
import unittest
import urllib.parse
from pathlib import Path

os.environ.setdefault("AWS_REGION", "ap-south-1")

sys.path.insert(0, str(Path(__file__).resolve().parent))

import handler  # noqa: E402

SECRET = "test-device-key-0123456789"


def _load_opctl():
    """Load the operator-control handler by path (module-name-clash safe) so
    parity with ITS _is_safe_sql can be asserted explicitly."""
    p = Path(__file__).resolve().parent.parent / "operator-control" / "handler.py"
    spec = importlib.util.spec_from_file_location("opctl_handler_for_parity", p)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# The adversarial corpus. Every entry is run through BOTH gates; behaviour
# must be IDENTICAL (parity is the contract — copying without a ratchet rots).
_SQL_CORPUS = [
    # multi-statement / chaining
    "select 1; drop table ticks",
    "select 1;;",
    "select 1 ; select 2",
    "select 1;",  # single trailing ';' is OK
    "select 1 ;",
    # comments
    "select 1 -- drop table ticks",
    "select /* drop */ 1",
    "--select 1",
    # first-word gate
    "selector 1",
    "explainx select 1",
    "(select 1)",
    "insert into t values(1)",
    # banned words embedded, any case/position
    "select * from t where x = 'UPDATE'",
    "SELECT INSERT",
    "with x as (delete from t) select 1",
    "select 1 union all select 2",  # allowed — no mutator
    "WITH x AS (SELECT 1) SELECT * FROM x",
    # leading whitespace / newlines / tabs / uppercase
    "  \n\t SELECT 1",
    "\nshow tables",
    "explain select * from ticks",
    # empty / whitespace-only
    "",
    "   ",
    ";",
]
# banned keywords exercised individually (mirrored _SQL_BANNED tuple)
_SQL_CORPUS += [f"select {kw} from t" for kw in handler._SQL_BANNED]
_SQL_CORPUS += [f"select * from t where a = {kw.upper()}" for kw in handler._SQL_BANNED]


def _event(
    method: str = "GET",
    path: str = "/",
    qs: dict | None = None,
    headers: dict | None = None,
    cookies: list | None = None,
    body: str | None = None,
    b64: bool = False,
) -> dict:
    return {
        "requestContext": {"http": {"method": method}},
        "rawPath": path,
        "rawQueryString": urllib.parse.urlencode(qs or {}),
        "queryStringParameters": dict(qs) if qs else None,
        "headers": headers or {},
        "cookies": cookies or [],
        "body": body,
        "isBase64Encoded": b64,
    }


def _bearer() -> dict:
    return {"authorization": f"Bearer {SECRET}"}


class WithSecret(unittest.TestCase):
    """Base: monkeypatch the SSM secret + the back-Lambda invoke."""

    def setUp(self) -> None:
        self._orig_secret = handler._control_secret
        self._orig_invoke = handler._invoke_back
        handler._control_secret = lambda: SECRET  # type: ignore[assignment]
        self.back_calls: list[dict] = []

        def fake_back(payload: dict) -> dict:
            self.back_calls.append(payload)
            return {
                "status": 200,
                "headers": {"content-type": "text/html"},
                "body_b64": base64.b64encode(b"ok").decode(),
            }

        handler._invoke_back = fake_back  # type: ignore[assignment]

    def tearDown(self) -> None:
        handler._control_secret = self._orig_secret  # type: ignore[assignment]
        handler._invoke_back = self._orig_invoke  # type: ignore[assignment]


class SqlGateParity(unittest.TestCase):
    def test_sql_gate_parity_with_operator_control(self) -> None:
        opctl = _load_opctl()
        # The mirrored constants must be IDENTICAL tuples.
        self.assertEqual(handler._SQL_ALLOWED_PREFIXES, opctl._SQL_ALLOWED_PREFIXES)
        self.assertEqual(handler._SQL_BANNED, opctl._SQL_BANNED)
        for q in _SQL_CORPUS:
            self.assertEqual(
                handler._is_safe_sql(q),
                opctl._is_safe_sql(q),
                f"gate parity broken for query: {q!r}",
            )

    def test_cap_sql_rows_parity_with_operator_control(self) -> None:
        opctl = _load_opctl()
        for q in (
            "select * from ticks",
            "select * from ticks LIMIT 5000",
            "select * from ticks limit 10",
            "select * from ticks limit 0,5000",
            "select * from ticks limit -5",
            "show tables",
            "explain select 1",
            "with x as (select 1) select * from x;",
        ):
            self.assertEqual(handler._cap_sql_rows(q), opctl._cap_sql_rows(q), q)

    def test_multi_statement_rejected(self) -> None:
        self.assertFalse(handler._is_safe_sql("select 1; drop table ticks"))
        self.assertFalse(handler._is_safe_sql("select 1; select 2"))
        self.assertTrue(handler._is_safe_sql("select 1;"))  # one trailing ';' OK

    def test_comment_smuggling_rejected(self) -> None:
        self.assertFalse(handler._is_safe_sql("select 1 -- drop table ticks"))
        self.assertFalse(handler._is_safe_sql("select /* hidden */ 1"))

    def test_banned_words_rejected_any_case(self) -> None:
        for kw in handler._SQL_BANNED:
            self.assertFalse(handler._is_safe_sql(f"select {kw} from t"), kw)
            self.assertFalse(handler._is_safe_sql(f"select * from t where {kw.upper()}=1"), kw)

    def test_cte_hiding_mutator_rejected(self) -> None:
        self.assertFalse(handler._is_safe_sql("with x as (insert into t values(1)) select 1"))
        self.assertTrue(handler._is_safe_sql("WITH x AS (SELECT 1) SELECT * FROM x"))

    def test_empty_whitespace_and_leading_ws(self) -> None:
        self.assertFalse(handler._is_safe_sql(""))
        self.assertFalse(handler._is_safe_sql("   "))
        self.assertFalse(handler._is_safe_sql(";"))
        self.assertTrue(handler._is_safe_sql("  \n\t SELECT 1"))

    def test_parenthesised_select_rejected_like_operator_control(self) -> None:
        # The first-word split sees a leading non-letter → "" → reject. This
        # matches operator-control exactly (fail-closed parity).
        opctl = _load_opctl()
        self.assertEqual(handler._is_safe_sql("(select 1)"), opctl._is_safe_sql("(select 1)"))
        self.assertFalse(handler._is_safe_sql("(select 1)"))


class SqlIntrospectionSuperset(unittest.TestCase):
    """FIX 3: bare read-only introspection function calls the QuestDB 9.3.5
    console issues must PASS (a superset of operator-control's gate)."""

    def test_each_allowed_func_passes_bare_and_with_args(self) -> None:
        for fn in handler._SQL_ALLOWED_FUNCS:
            self.assertTrue(handler._is_safe_sql(f"{fn}()"), fn)
            self.assertTrue(handler._is_safe_sql(f"{fn.upper()}()"), fn)
        self.assertTrue(handler._is_safe_sql("columns('ticks')"))
        self.assertTrue(handler._is_safe_sql("table_columns('ticks')"))
        self.assertTrue(handler._is_safe_sql("  tables()  "))

    def test_unknown_func_still_rejected(self) -> None:
        self.assertFalse(handler._is_safe_sql("evil_func()"))
        self.assertFalse(handler._is_safe_sql("pg_sleep(10)"))

    def test_func_chaining_rejected(self) -> None:
        self.assertFalse(handler._is_safe_sql("tables(); drop table ticks"))
        self.assertFalse(handler._is_safe_sql("tables() ; select 1"))

    def test_func_with_banned_word_rejected(self) -> None:
        # A banned mutator after an allowed func still trips the whole-word scan.
        self.assertFalse(handler._is_safe_sql("tables() delete"))
        self.assertFalse(handler._is_safe_sql("columns('t') drop"))

    def test_insert_still_rejected(self) -> None:
        self.assertFalse(handler._is_safe_sql("insert into t values(1)"))

    def test_front_back_gate_byte_identical(self) -> None:
        # FIX 3: front and back _is_safe_sql must agree on EVERYTHING.
        import importlib.util as _u

        p = Path(__file__).resolve().parent.parent / "questdb-console-proxy" / "handler.py"
        spec = _u.spec_from_file_location("qdb_back_for_parity", p)
        back = _u.module_from_spec(spec)
        spec.loader.exec_module(back)
        self.assertEqual(handler._SQL_ALLOWED_FUNCS, back._SQL_ALLOWED_FUNCS)
        corpus = list(_SQL_CORPUS) + [f"{fn}()" for fn in handler._SQL_ALLOWED_FUNCS] + [
            "columns('ticks')", "tables(); drop table ticks", "evil_func()", "tables() delete",
        ]
        for q in corpus:
            self.assertEqual(handler._is_safe_sql(q), back._is_safe_sql(q), q)


class Signing(unittest.TestCase):
    def test_mint_verify_roundtrip(self) -> None:
        now = int(time.time())
        tok = handler._mint_signed(SECRET, "qdblink", now + 90)
        self.assertTrue(handler._verify_signed(SECRET, "qdblink", tok, now))

    def test_expired_token_rejected(self) -> None:
        now = int(time.time())
        tok = handler._mint_signed(SECRET, "qdblink", now - 1)
        self.assertFalse(handler._verify_signed(SECRET, "qdblink", tok, now))

    def test_forged_hmac_rejected(self) -> None:
        now = int(time.time())
        tok = handler._mint_signed("some-other-secret", "qdblink", now + 90)
        self.assertFalse(handler._verify_signed(SECRET, "qdblink", tok, now))

    def test_wrong_prefix_rejected(self) -> None:
        # A session cookie can never be replayed as a link token (domain sep).
        now = int(time.time())
        tok = handler._mint_signed(SECRET, "qdbsess", now + 90)
        self.assertFalse(handler._verify_signed(SECRET, "qdblink", tok, now))

    def test_garbage_and_empty_rejected(self) -> None:
        now = int(time.time())
        for bad in ("", "garbage", "abc.def", "12345", "99999999999999999999.x", ".", "1e9.aa"):
            self.assertFalse(handler._verify_signed(SECRET, "qdblink", bad, now), bad)

    def test_no_secret_rejects_everything(self) -> None:
        now = int(time.time())
        tok = handler._mint_signed(SECRET, "qdblink", now + 90)
        self.assertFalse(handler._verify_signed("", "qdblink", tok, now))

    def test_constant_time_compare_used(self) -> None:
        # The ratchet the brief demands: every secret compare goes through
        # hmac.compare_digest — token/cookie verify AND the two direct key
        # compares (Bearer + login POST).
        self.assertIn("hmac.compare_digest", inspect.getsource(handler._verify_signed))
        self.assertIn("hmac.compare_digest", inspect.getsource(handler._authenticated))
        self.assertIn("hmac.compare_digest", inspect.getsource(handler.lambda_handler))


class AuthFlow(WithSecret):
    def test_unauthenticated_get_returns_login_page(self) -> None:
        r = handler.lambda_handler(_event("GET", "/"), None)
        self.assertEqual(r["statusCode"], 401)
        self.assertIn("device key", r["body"])
        self.assertNotIn(SECRET, r["body"])  # never echo the secret

    def test_open_with_valid_token_sets_cookie_and_redirects(self) -> None:
        tok = handler._mint_signed(SECRET, "qdblink", int(time.time()) + 90)
        r = handler.lambda_handler(_event("GET", "/open", {"tok": tok}), None)
        self.assertEqual(r["statusCode"], 302)
        self.assertEqual(r["headers"]["location"], "/")
        cookie = r["cookies"][0]
        self.assertIn("qdb_sess=", cookie)
        self.assertIn("HttpOnly", cookie)
        self.assertIn("Secure", cookie)
        self.assertIn("SameSite=Lax", cookie)
        self.assertIn(f"Max-Age={handler.SESSION_TTL_SECS}", cookie)

    def test_open_with_expired_token_401(self) -> None:
        tok = handler._mint_signed(SECRET, "qdblink", int(time.time()) - 5)
        r = handler.lambda_handler(_event("GET", "/open", {"tok": tok}), None)
        self.assertEqual(r["statusCode"], 401)

    def test_open_with_forged_token_401(self) -> None:
        tok = handler._mint_signed("attacker-secret", "qdblink", int(time.time()) + 90)
        r = handler.lambda_handler(_event("GET", "/open", {"tok": tok}), None)
        self.assertEqual(r["statusCode"], 401)

    def test_open_with_garbage_token_401(self) -> None:
        r = handler.lambda_handler(_event("GET", "/open", {"tok": "zzz"}), None)
        self.assertEqual(r["statusCode"], 401)

    def test_valid_session_cookie_authenticates(self) -> None:
        sess = handler._mint_signed(SECRET, "qdbsess", int(time.time()) + 3600)
        r = handler.lambda_handler(_event("GET", "/", cookies=[f"qdb_sess={sess}"]), None)
        self.assertEqual(r["statusCode"], 200)  # forwarded to (fake) back

    def test_expired_cookie_rejected(self) -> None:
        sess = handler._mint_signed(SECRET, "qdbsess", int(time.time()) - 5)
        r = handler.lambda_handler(_event("GET", "/", cookies=[f"qdb_sess={sess}"]), None)
        self.assertEqual(r["statusCode"], 401)

    def test_forged_cookie_rejected(self) -> None:
        sess = handler._mint_signed("attacker", "qdbsess", int(time.time()) + 3600)
        r = handler.lambda_handler(_event("GET", "/", cookies=[f"qdb_sess={sess}"]), None)
        self.assertEqual(r["statusCode"], 401)

    def test_bearer_right_key_authorized(self) -> None:
        r = handler.lambda_handler(_event("GET", "/", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 200)

    def test_bearer_wrong_key_rejected(self) -> None:
        r = handler.lambda_handler(
            _event("GET", "/", headers={"authorization": "Bearer nope"}), None
        )
        self.assertEqual(r["statusCode"], 401)

    def test_login_post_right_key_sets_cookie(self) -> None:
        body = urllib.parse.urlencode({"key": SECRET})
        r = handler.lambda_handler(
            _event("POST", "/login", headers={"content-type": "application/x-www-form-urlencoded"}, body=body),
            None,
        )
        self.assertEqual(r["statusCode"], 302)
        self.assertIn("qdb_sess=", r["cookies"][0])

    def test_login_post_wrong_key_401(self) -> None:
        body = urllib.parse.urlencode({"key": "wrong"})
        r = handler.lambda_handler(_event("POST", "/login", body=body), None)
        self.assertEqual(r["statusCode"], 401)
        self.assertIn("wrong key", r["body"])

    def test_login_post_base64_body_json(self) -> None:
        body = base64.b64encode(json.dumps({"key": SECRET}).encode()).decode()
        r = handler.lambda_handler(
            _event("POST", "/login", headers={"content-type": "application/json"}, body=body, b64=True),
            None,
        )
        self.assertEqual(r["statusCode"], 302)

    def test_no_secret_configured_denies_all(self) -> None:
        handler._control_secret = lambda: ""  # type: ignore[assignment]
        r = handler.lambda_handler(_event("GET", "/", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 401)
        # Even a "valid-looking" open link dies without a secret (fail closed).
        r = handler.lambda_handler(_event("GET", "/open", {"tok": "1.a"}), None)
        self.assertEqual(r["statusCode"], 401)


class PathMethodGate(WithSecret):
    def test_imp_path_403(self) -> None:
        r = handler.lambda_handler(_event("GET", "/imp", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 403)
        self.assertEqual(self.back_calls, [])  # never forwarded

    def test_path_traversal_confusion_denied(self) -> None:
        # FIX 1: none of these may reach the classifier / SQL gate / back.
        for path, qs in (
            ("/assets/../exec", {"query": "drop table ticks"}),
            ("/exec/../imp", None),
            ("/index.html/../exec", {"query": "select 1"}),
            ("/assets//app.js", None),
            ("/assets/\\..\\exec", None),
        ):
            r = handler.lambda_handler(_event("GET", path, qs, headers=_bearer()), None)
            self.assertEqual(r["statusCode"], 403, path)
            self.assertEqual(self.back_calls, [], path)

    def test_encoded_traversal_denied(self) -> None:
        # %2e%2e%2fexec, %2f, %5c encoded separators — rejected on the raw form.
        for path in ("/%2e%2e%2fexec", "/assets/%2e%2e/exec", "/%2fexec", "/a%5c..%5cexec"):
            r = handler.lambda_handler(
                _event("GET", path, {"query": "drop table ticks"}, headers=_bearer()), None
            )
            self.assertEqual(r["statusCode"], 403, path)
            self.assertEqual(self.back_calls, [], path)

    def test_static_path_does_not_execute_sql(self) -> None:
        # FIX 1: `?query=...` on a static path must NOT be forwarded to QuestDB.
        r = handler.lambda_handler(
            _event("GET", "/index.html", {"query": "drop table ticks", "v": "9"}, headers=_bearer()),
            None,
        )
        self.assertEqual(r["statusCode"], 200)
        self.assertEqual(self.back_calls[0]["path"], "/index.html")
        fwd = urllib.parse.parse_qs(self.back_calls[0]["rawQuery"])
        self.assertNotIn("query", fwd)  # never smuggled onto a static path
        self.assertEqual(fwd.get("v"), ["9"])  # whitelisted param survives

    def test_chk_forwards_only_whitelisted_params(self) -> None:
        r = handler.lambda_handler(
            _event("GET", "/chk", {"f": "json", "j": "ticks", "query": "drop table ticks"}, headers=_bearer()),
            None,
        )
        self.assertEqual(r["statusCode"], 200)
        fwd = urllib.parse.parse_qs(self.back_calls[0]["rawQuery"])
        self.assertEqual(fwd["f"], ["json"])
        self.assertEqual(fwd["j"], ["ticks"])
        self.assertNotIn("query", fwd)

    def test_post_exec_405(self) -> None:
        r = handler.lambda_handler(
            _event("POST", "/exec", {"query": "select 1"}, headers=_bearer()), None
        )
        self.assertEqual(r["statusCode"], 405)
        self.assertEqual(self.back_calls, [])

    def test_unknown_path_403(self) -> None:
        r = handler.lambda_handler(_event("GET", "/api/thing", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 403)

    def test_settings_write_405(self) -> None:
        r = handler.lambda_handler(_event("PUT", "/settings", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 405)

    def test_assets_forwarded(self) -> None:
        r = handler.lambda_handler(_event("GET", "/assets/app.js", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 200)
        self.assertEqual(self.back_calls[0]["path"], "/assets/app.js")

    def test_chk_forwarded(self) -> None:
        r = handler.lambda_handler(_event("GET", "/chk", {"f": "json", "j": "ticks"}, headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 200)
        self.assertEqual(self.back_calls[0]["path"], "/chk")

    def test_exec_without_query_400(self) -> None:
        r = handler.lambda_handler(_event("GET", "/exec", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 400)
        self.assertIn("missing query", r["body"])

    def test_exec_mutator_400_questdb_shaped(self) -> None:
        r = handler.lambda_handler(
            _event("GET", "/exec", {"query": "drop table ticks"}, headers=_bearer()), None
        )
        self.assertEqual(r["statusCode"], 400)
        body = json.loads(r["body"])
        # QuestDB /exec error JSON shape so the console UI renders it.
        self.assertEqual(set(body), {"query", "error", "position"})
        self.assertIn("read-only console", body["error"])
        self.assertEqual(self.back_calls, [])

    def test_exec_overlength_query_400(self) -> None:
        q = "select " + "1," * handler.MAX_SQL_LEN
        r = handler.lambda_handler(_event("GET", "/exec", {"query": q}, headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 400)
        self.assertIn("too long", json.loads(r["body"])["error"])

    def test_exec_forwards_capped_query_and_whitelisted_params_only(self) -> None:
        r = handler.lambda_handler(
            _event(
                "GET",
                "/exec",
                {"query": "select * from ticks", "count": "true", "evil": "1"},
                headers=_bearer(),
            ),
            None,
        )
        self.assertEqual(r["statusCode"], 200)
        fwd = urllib.parse.parse_qs(self.back_calls[0]["rawQuery"])
        self.assertEqual(fwd["query"], ["select * from ticks LIMIT 1000"])
        self.assertEqual(fwd["count"], ["true"])
        self.assertNotIn("evil", fwd)

    def test_exec_limit_param_clamped(self) -> None:
        handler.lambda_handler(
            _event("GET", "/exec", {"query": "show tables", "limit": "0,999999"}, headers=_bearer()),
            None,
        )
        fwd = urllib.parse.parse_qs(self.back_calls[0]["rawQuery"])
        self.assertEqual(fwd["limit"], ["0,1000"])

    def test_exp_gets_belt_and_braces_limit(self) -> None:
        handler.lambda_handler(
            _event("GET", "/exp", {"query": "show tables"}, headers=_bearer()), None
        )
        fwd = urllib.parse.parse_qs(self.back_calls[0]["rawQuery"])
        self.assertEqual(fwd["limit"], ["1000"])


class RelayPlumbing(WithSecret):
    def test_binary_base64_passthrough_preserves_headers(self) -> None:
        blob = base64.b64encode(b"\x1f\x8b\x00binary").decode()

        def fake_back(_payload: dict) -> dict:
            return {
                "status": 200,
                "headers": {
                    "content-type": "font/woff2",
                    "content-encoding": "gzip",
                    "cache-control": "max-age=60",
                },
                "body_b64": blob,
            }

        handler._invoke_back = fake_back  # type: ignore[assignment]
        r = handler.lambda_handler(_event("GET", "/assets/font.woff2", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 200)
        self.assertTrue(r["isBase64Encoded"])
        self.assertEqual(r["body"], blob)
        self.assertEqual(r["headers"]["content-type"], "font/woff2")
        self.assertEqual(r["headers"]["content-encoding"], "gzip")
        self.assertEqual(r["headers"]["cache-control"], "max-age=60")

    def test_oversize_body_502(self) -> None:
        handler._invoke_back = lambda _p: {"err": "too_large"}  # type: ignore[assignment]
        r = handler.lambda_handler(_event("GET", "/", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 502)
        self.assertIn("too large", json.loads(r["body"])["error"])

    def test_box_unreachable_503_honest_offline_message(self) -> None:
        handler._invoke_back = lambda _p: {"err": "box_unreachable"}  # type: ignore[assignment]
        r = handler.lambda_handler(_event("GET", "/", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 503)
        self.assertIn("offline", json.loads(r["body"])["error"])
        self.assertIn("08:30", json.loads(r["body"])["error"])

    def test_body_cap_constant_within_6mib_envelope(self) -> None:
        # FIX 2: base64(cap) + JSON must stay under Lambda's 6 MiB response.
        self.assertLessEqual(handler.MAX_BODY_BYTES, 4_100_000)

    def test_over_cap_back_body_maps_to_502_not_503(self) -> None:
        # A too_large from the back is a 502, never the 503 offline path.
        handler._invoke_back = lambda _p: {"err": "too_large"}  # type: ignore[assignment]
        r = handler.lambda_handler(_event("GET", "/", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 502)
        # And an over-sized base64 body_b64 from a (mis)reporting back → 502.
        big = "A" * (int(handler.MAX_BODY_BYTES * 4 / 3) + 100)
        handler._invoke_back = lambda _p: {"status": 200, "headers": {}, "body_b64": big}  # type: ignore[assignment]
        r = handler.lambda_handler(_event("GET", "/", headers=_bearer()), None)
        self.assertEqual(r["statusCode"], 502)

    def test_back_status_relayed(self) -> None:
        def fake_back(_payload: dict) -> dict:
            return {"status": 400, "headers": {"content-type": "application/json"},
                    "body_b64": base64.b64encode(b'{"error":"x"}').decode()}

        handler._invoke_back = fake_back  # type: ignore[assignment]
        r = handler.lambda_handler(
            _event("GET", "/exec", {"query": "select bad_col from ticks"}, headers=_bearer()), None
        )
        self.assertEqual(r["statusCode"], 400)


class BuildMarker(unittest.TestCase):
    def test_build_marker_present(self) -> None:
        # Proof-3 ratchet: the deployed-bytes marker exists in BOTH handlers.
        self.assertTrue(handler.QDB_CONSOLE_BUILD.startswith("b4-qdb-console-"))
        front_src = (Path(__file__).resolve().parent / "handler.py").read_text()
        back_src = (
            Path(__file__).resolve().parent.parent / "questdb-console-proxy" / "handler.py"
        ).read_text()
        self.assertIn("QDB_CONSOLE_BUILD", front_src)
        self.assertIn("QDB_CONSOLE_BUILD", back_src)
        self.assertIn(handler.QDB_CONSOLE_BUILD, back_src)  # same marker value


if __name__ == "__main__":
    unittest.main()
