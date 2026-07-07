"""Unit tests for the questdb-console-proxy (VPC back) Lambda (B4).

Runs without AWS credentials — the handler makes ZERO AWS SDK calls at
runtime (asserted below). Covers the defense-in-depth re-gate on /exec + /exp,
path/method whitelist parity with the front, the 5.5MB size cap, timeout /
URLError mapping to box_unreachable, HTTPError relay, and the deployed-bytes
proof marker.

Run with:  python3 -m unittest test_handler
"""

from __future__ import annotations

import base64
import importlib.util
import io
import os
import socket
import sys
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from unittest import mock

os.environ.setdefault("AWS_REGION", "ap-south-1")

sys.path.insert(0, str(Path(__file__).resolve().parent))

import handler  # noqa: E402


def _load_front():
    p = Path(__file__).resolve().parent.parent / "questdb-console-front" / "handler.py"
    spec = importlib.util.spec_from_file_location("qdb_front_for_parity", p)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _load_opctl():
    p = Path(__file__).resolve().parent.parent / "operator-control" / "handler.py"
    spec = importlib.util.spec_from_file_location("opctl_for_parity_back", p)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class FakeResp:
    """Minimal stand-in for urllib's HTTPResponse (context manager + chunked read)."""

    def __init__(self, chunks: list[bytes], status: int = 200, headers: dict | None = None):
        self._chunks = list(chunks)
        self.status = status
        self.headers = headers or {"content-type": "text/html"}

    def read(self, _n: int = -1) -> bytes:
        return self._chunks.pop(0) if self._chunks else b""

    def __enter__(self):
        return self

    def __exit__(self, *_a):
        return False


class WithBase(unittest.TestCase):
    def setUp(self) -> None:
        self._orig = handler.QDB_BASE
        handler.QDB_BASE = "http://10.42.1.99:9000"

    def tearDown(self) -> None:
        handler.QDB_BASE = self._orig


class ReGate(WithBase):
    def test_regate_rejects_mutator_sql(self) -> None:
        # Zero trust of the front: even if the front is bypassed, the mutator
        # dies HERE, before any socket is opened.
        with mock.patch("urllib.request.urlopen") as up:
            r = handler.lambda_handler(
                {"method": "GET", "path": "/exec", "rawQuery": "query=drop+table+ticks"}, None
            )
            self.assertEqual(r, {"err": "denied_sql"})
            up.assert_not_called()

    def test_regate_rejects_chained_statement(self) -> None:
        with mock.patch("urllib.request.urlopen") as up:
            r = handler.lambda_handler(
                {"method": "GET", "path": "/exp", "rawQuery": "query=select+1%3B+drop+table+ticks"},
                None,
            )
            self.assertEqual(r, {"err": "denied_sql"})
            up.assert_not_called()

    def test_regate_rejects_missing_and_overlong_query(self) -> None:
        with mock.patch("urllib.request.urlopen") as up:
            self.assertEqual(
                handler.lambda_handler({"method": "GET", "path": "/exec", "rawQuery": ""}, None),
                {"err": "denied_sql"},
            )
            long_q = "query=" + "a" * (handler.MAX_SQL_LEN + 1)
            self.assertEqual(
                handler.lambda_handler({"method": "GET", "path": "/exec", "rawQuery": long_q}, None),
                {"err": "denied_sql"},
            )
            up.assert_not_called()

    def test_path_whitelist_parity(self) -> None:
        front = _load_front()
        cases = [
            ("GET", "/exec"), ("GET", "/exp"), ("GET", "/"), ("GET", "/index.html"),
            ("GET", "/chk"), ("GET", "/settings"), ("GET", "/assets/app.js"),
            ("GET", "/favicon.ico"), ("GET", "/imp"), ("GET", "/api/x"),
            ("POST", "/exec"), ("PUT", "/settings"), ("POST", "/imp"), ("HEAD", "/"),
            ("DELETE", "/exp"), ("GET", "/weird/path"),
        ]
        for method, path in cases:
            self.assertEqual(
                handler._classify_path(method, path),
                front._classify_path(method, path),
                f"whitelist parity broken for {method} {path}",
            )

    def test_denied_paths_and_methods(self) -> None:
        with mock.patch("urllib.request.urlopen") as up:
            self.assertEqual(
                handler.lambda_handler({"method": "GET", "path": "/imp"}, None), {"err": "denied"}
            )
            self.assertEqual(
                handler.lambda_handler(
                    {"method": "POST", "path": "/exec", "rawQuery": "query=select+1"}, None
                ),
                {"err": "denied"},
            )
            up.assert_not_called()

    def test_sql_gate_parity_with_operator_control_shared_subset(self) -> None:
        # The back gate is a read-only SUPERSET of operator-control (adds bare
        # introspection funcs); on the NON-func corpus they must agree.
        opctl = _load_opctl()
        self.assertEqual(handler._SQL_ALLOWED_PREFIXES, opctl._SQL_ALLOWED_PREFIXES)
        self.assertEqual(handler._SQL_BANNED, opctl._SQL_BANNED)
        for q in (
            "select 1; drop table ticks",
            "select 1 -- x",
            "select /* x */ 1",
            "backup table ticks",
            "select backup from t",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "show tables",
            "select 1;",
            "",
        ):
            self.assertEqual(handler._is_safe_sql(q), opctl._is_safe_sql(q), q)

    def test_introspection_funcs_allowed_and_rechecked(self) -> None:
        # FIX 3: the console's bare introspection funcs pass; unknown funcs +
        # chaining + banned-word-after still reject at the VPC hop.
        for fn in handler._SQL_ALLOWED_FUNCS:
            self.assertTrue(handler._is_safe_sql(f"{fn}()"), fn)
        self.assertTrue(handler._is_safe_sql("columns('ticks')"))
        self.assertFalse(handler._is_safe_sql("evil_func()"))
        self.assertFalse(handler._is_safe_sql("tables(); drop table ticks"))
        self.assertFalse(handler._is_safe_sql("tables() delete"))
        self.assertFalse(handler._is_safe_sql("insert into t values(1)"))

    def test_introspection_func_relays_through_gate(self) -> None:
        resp = FakeResp([b"[]"], headers={"content-type": "application/json"})
        with mock.patch("urllib.request.urlopen", return_value=resp) as up:
            r = handler.lambda_handler(
                {"method": "GET", "path": "/exec", "rawQuery": "query=tables%28%29"}, None
            )
            self.assertEqual(r["status"], 200)
            up.assert_called_once()

    def test_bad_path_rejected_at_vpc_hop(self) -> None:
        # FIX 1 zero-trust: even if the front is bypassed, traversal dies here.
        with mock.patch("urllib.request.urlopen") as up:
            for path in ("/assets/../exec", "/exec/../imp", "/%2e%2e%2fexec", "/a//b", "/x\\y"):
                r = handler.lambda_handler(
                    {"method": "GET", "path": path, "rawQuery": "query=drop+table+ticks"}, None
                )
                self.assertEqual(r, {"err": "bad_path"}, path)
            up.assert_not_called()

    def test_body_cap_constant_within_6mib_envelope(self) -> None:
        self.assertLessEqual(handler.MAX_BODY_BYTES, 4_100_000)


class Relay(WithBase):
    def test_happy_path_relays_status_headers_body(self) -> None:
        resp = FakeResp(
            [b"hello ", b"world"],
            status=200,
            headers={"content-type": "text/csv", "content-encoding": "gzip"},
        )
        with mock.patch("urllib.request.urlopen", return_value=resp) as up:
            r = handler.lambda_handler(
                {"method": "GET", "path": "/exp", "rawQuery": "query=show+tables",
                 "headers": {"accept": "text/csv"}},
                None,
            )
            self.assertEqual(r["status"], 200)
            self.assertEqual(base64.b64decode(r["body_b64"]), b"hello world")
            self.assertEqual(r["headers"]["content-type"], "text/csv")
            self.assertEqual(r["headers"]["content-encoding"], "gzip")
            req = up.call_args[0][0]
            self.assertTrue(req.full_url.startswith("http://10.42.1.99:9000/exp?"))
            self.assertEqual(up.call_args[1]["timeout"], handler._TIMEOUT_SECS)

    def test_shell_get_forces_identity_and_connection_close(self) -> None:
        # B4 r3 (honest wording — the r2 premise is DISPROVEN): these two
        # headers are RETAINED hygiene, NOT the shell fix. The 2026-07-06
        # raw-socket probe (scratchpad/repro-evidence.md §3/§10) proved
        # QuestDB 9.3.5 IGNORES request `Connection: close` on the / 301 and
        # never closes the socket. The actual fix is the GET / -> /index.html
        # rewrite (asserted below) + the _NoFollowRedirect body-less 3xx
        # relay. `Accept-Encoding: identity` still keeps framed paths
        # Content-Length'd (uncompressed) under the relay cap.
        resp = FakeResp([b"<!doctype html><html></html>"])
        with mock.patch("urllib.request.urlopen", return_value=resp) as up:
            r = handler.lambda_handler(
                {
                    "method": "GET",
                    "path": "/",
                    "headers": {"accept": "text/html", "accept-encoding": "gzip, br"},
                },
                None,
            )
            self.assertEqual(r["status"], 200)
            req = up.call_args[0][0]
            # urllib title-cases header keys via add_header().
            self.assertEqual(req.get_header("Accept-encoding"), "identity")
            self.assertEqual(req.get_header("Connection"), "close")
            self.assertEqual(req.get_header("Accept"), "text/html")  # browser accept still relayed
            # B4 r3: the shell request must target the framed /index.html.
            self.assertEqual(req.full_url, "http://10.42.1.99:9000/index.html")

    def test_root_get_is_rewritten_to_index_html(self) -> None:
        # B4 r3 L1: GET / never reaches QuestDB — the back fetches the framed
        # shell directly (the / 301 is unframed + never-closed → 12s hang).
        resp = FakeResp([b"<!DOCTYPE html>\n<html>"], headers={"content-type": "text/html"})
        with mock.patch("urllib.request.urlopen", return_value=resp) as up:
            r = handler.lambda_handler(
                {"method": "GET", "path": "/", "headers": {"accept": "text/html"}}, None
            )
            self.assertEqual(r["status"], 200)
            self.assertEqual(up.call_args[0][0].full_url, "http://10.42.1.99:9000/index.html")

    def test_root_rewrite_preserves_whitelisted_query(self) -> None:
        # The front's whitelisted static params must survive the rewrite.
        resp = FakeResp([b"<!DOCTYPE html>"])
        with mock.patch("urllib.request.urlopen", return_value=resp) as up:
            r = handler.lambda_handler(
                {"method": "GET", "path": "/", "rawQuery": "v=1"}, None
            )
            self.assertEqual(r["status"], 200)
            self.assertEqual(up.call_args[0][0].full_url, "http://10.42.1.99:9000/index.html?v=1")

    def test_head_root_not_rewritten(self) -> None:
        # Scope pin (repro §8): HEAD / is a fast FRAMED chunked 405 from
        # QuestDB — deliberately unchanged (HEAD /index.html is
        # live-unverified). The rewrite is GET-gated.
        resp = FakeResp([b""], status=405, headers={"content-type": "text/plain"})
        with mock.patch("urllib.request.urlopen", return_value=resp) as up:
            handler.lambda_handler({"method": "HEAD", "path": "/"}, None)
            self.assertEqual(up.call_args[0][0].full_url, "http://10.42.1.99:9000/")

    def test_non_root_paths_not_rewritten(self) -> None:
        # Guards an over-eager rewrite: ONLY exactly GET "/" is rewritten.
        for p in ("/index.html", "/chk", "/settings", "/assets/app.js"):
            resp = FakeResp([b"x"])
            with mock.patch("urllib.request.urlopen", return_value=resp) as up:
                handler.lambda_handler({"method": "GET", "path": p}, None)
                self.assertTrue(
                    up.call_args[0][0].full_url.endswith(p),
                    f"{p} was unexpectedly rewritten to {up.call_args[0][0].full_url}",
                )
        resp = FakeResp([b"[]"], headers={"content-type": "application/json"})
        with mock.patch("urllib.request.urlopen", return_value=resp) as up:
            handler.lambda_handler(
                {"method": "GET", "path": "/exec", "rawQuery": "query=select+1"}, None
            )
            self.assertTrue(
                up.call_args[0][0].full_url.startswith("http://10.42.1.99:9000/exec?")
            )

    def test_root_rewrite_kills_the_unframed_301_timeout_class(self) -> None:
        # Behavioral proof of the fix: a fake box that HANGS (socket.timeout,
        # exactly what the unframed keep-alive 301 read-until-EOF produced)
        # for every path EXCEPT /index.html. On r2 bytes GET / returns
        # {"err": "upstream_timeout"} (the prod 504); on r3 it returns the
        # shell with status 200 because the 301 is never elicited.
        def fake_urlopen(req, timeout=None):
            if req.full_url.rstrip("?").endswith("/index.html"):
                return FakeResp([b"<!DOCTYPE html>"])
            raise socket.timeout("unframed 301 read-until-EOF hang")

        with mock.patch("urllib.request.urlopen", side_effect=fake_urlopen):
            r = handler.lambda_handler({"method": "GET", "path": "/"}, None)
            self.assertEqual(r["status"], 200)
            self.assertIn(b"<!DOCTYPE html>", base64.b64decode(r["body_b64"]))

    def test_redirect_relayed_bodyless_never_reads_body(self) -> None:
        # B4 r3 L2 (non-vacuous hang guard): a 3xx body may be DELIMITER-LESS
        # on a socket QuestDB never closes — reading it blocks until the 12s
        # timeout. The bomb-fp proves the 3xx arm NEVER touches the body.
        class _BombFp:
            def read(self, *_a):
                raise AssertionError("3xx body must NEVER be read (unframed; socket never closes)")

            def close(self):
                return None

        err = urllib.error.HTTPError(
            url="http://10.42.1.99:9000/settings", code=301,
            msg="Moved Permanently", hdrs={"location": "/index.html"}, fp=_BombFp(),
        )
        # /settings is NOT rewritten, so this exercises the HTTPError 3xx arm.
        with mock.patch("urllib.request.urlopen", side_effect=err):
            r = handler.lambda_handler({"method": "GET", "path": "/settings"}, None)
            self.assertEqual(
                r, {"status": 301, "headers": {"location": "/index.html"}, "body_b64": ""}
            )

    def test_no_follow_redirect_opener_installed(self) -> None:
        # The belt layer: redirect_request returns None (no fp.read() drain;
        # every 3xx surfaces as HTTPError) and the opener is installed
        # module-wide so plain urllib.request.urlopen uses it.
        # Fixer round 1 (2026-07-06): assert the LIVE installed opener object,
        # not handler.py source text — a source grep ('install_opener' in src)
        # stays green if the install_opener() call is moved into dead code
        # (demonstrated in adversarial review), leaving the belt inert while
        # every other test mock.patches urlopen and bypasses the opener chain.
        # urllib.request._opener is the exact module global that urlopen()
        # consults when no explicit opener is passed (CPython stdlib).
        self.assertIsNone(
            handler._NoFollowRedirect().redirect_request(None, None, 301, "Moved", {}, "/index.html")
        )
        live_opener = getattr(urllib.request, "_opener", None)
        self.assertIsNotNone(
            live_opener,
            "no global opener installed — importing handler must call urllib.request.install_opener(...)",
        )
        self.assertTrue(
            any(isinstance(h, handler._NoFollowRedirect) for h in live_opener.handlers),
            "_NoFollowRedirect is NOT in the LIVE installed opener chain — the L2 belt layer is inert",
        )

    def test_disproven_r2_close_claim_removed(self) -> None:
        # Source ratchet: the factually-wrong r2 claim (that QuestDB
        # "closes the socket AFTER the body") can never return — the
        # 2026-07-06 raw-socket probe disproved it verbatim.
        src = (Path(__file__).resolve().parent / "handler.py").read_text()
        self.assertNotIn("closes the socket AFTER the body", src)

    def test_size_cap_returns_too_large(self) -> None:
        chunk = b"x" * handler._READ_CHUNK
        n = handler.MAX_BODY_BYTES // len(chunk) + 2
        resp = FakeResp([chunk] * n)
        with mock.patch("urllib.request.urlopen", return_value=resp):
            r = handler.lambda_handler({"method": "GET", "path": "/index.html"}, None)
            self.assertEqual(r, {"err": "too_large"})

    def test_urlerror_refused_maps_to_box_unreachable(self) -> None:
        # A genuine connection refusal / reset stays the offline-503 path.
        with mock.patch(
            "urllib.request.urlopen", side_effect=urllib.error.URLError("refused")
        ):
            r = handler.lambda_handler({"method": "GET", "path": "/"}, None)
            self.assertEqual(r, {"err": "box_unreachable"})

    def test_socket_timeout_maps_to_upstream_timeout(self) -> None:
        # B4 r2 diagnostic honesty: a read/connect timeout is a REACHABLE-but-
        # slow box, NOT a refused connection — distinct code so the front
        # returns 504, never a false 503 "offline".
        with mock.patch("urllib.request.urlopen", side_effect=socket.timeout("slow")):
            r = handler.lambda_handler({"method": "GET", "path": "/"}, None)
            self.assertEqual(r, {"err": "upstream_timeout"})

    def test_urlerror_wrapping_timeout_maps_to_upstream_timeout(self) -> None:
        # urllib may wrap a socket timeout in a URLError; still an upstream
        # timeout, not box_unreachable.
        with mock.patch(
            "urllib.request.urlopen",
            side_effect=urllib.error.URLError(socket.timeout("slow")),
        ):
            r = handler.lambda_handler({"method": "GET", "path": "/"}, None)
            self.assertEqual(r, {"err": "upstream_timeout"})

    def test_http_error_is_relayed_not_swallowed(self) -> None:
        # QuestDB's own 400 (bad column etc.) must reach the console UI.
        err = urllib.error.HTTPError(
            url="http://x/exec",
            code=400,
            msg="Bad Request",
            hdrs={"content-type": "application/json"},
            fp=io.BytesIO(b'{"error":"bad column"}'),
        )
        with mock.patch("urllib.request.urlopen", side_effect=err):
            r = handler.lambda_handler(
                {"method": "GET", "path": "/exec", "rawQuery": "query=select+bad+from+t"}, None
            )
            self.assertEqual(r["status"], 400)
            self.assertEqual(base64.b64decode(r["body_b64"]), b'{"error":"bad column"}')

    def test_empty_qdb_base_is_box_unreachable(self) -> None:
        handler.QDB_BASE = ""
        r = handler.lambda_handler({"method": "GET", "path": "/"}, None)
        self.assertEqual(r, {"err": "box_unreachable"})


class Hygiene(unittest.TestCase):
    def test_no_boto3_import(self) -> None:
        # The VPC hop is secret-free and SDK-free by contract (it has no
        # AWS-API network path anyway — no NAT, no VPC endpoints).
        src = (Path(__file__).resolve().parent / "handler.py").read_text()
        self.assertNotIn("boto3", src)
        self.assertNotIn("localhost", src.lower())
        self.assertNotIn("127.0.0.1", src)

    def test_no_hardcoded_private_ip(self) -> None:
        src = (Path(__file__).resolve().parent / "handler.py").read_text()
        self.assertNotIn("10.42.", src)  # IP comes ONLY from TF-injected env

    def test_build_marker_present(self) -> None:
        self.assertTrue(handler.QDB_CONSOLE_BUILD.startswith("b4-qdb-console-"))
        front_src = (
            Path(__file__).resolve().parent.parent / "questdb-console-front" / "handler.py"
        ).read_text()
        self.assertIn(handler.QDB_CONSOLE_BUILD, front_src)


if __name__ == "__main__":
    unittest.main()
