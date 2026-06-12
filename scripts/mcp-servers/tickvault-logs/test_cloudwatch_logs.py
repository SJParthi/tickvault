#!/usr/bin/env python3
"""Standalone tests for the cloudwatch_logs MCP tool's PURE helpers.

Stdlib-only, no AWS access required (mirrors test_placeholder_fallback.py).
Run: python3 test_cloudwatch_logs.py  (exit 0 = pass, 1 = fail)

Verifies the argv builder + JSON parser + the graceful no-credentials path,
so the fully-automated CloudWatch read access has ratcheted coverage even
though the live `aws` call can only be exercised on a credentialed host.
"""

from __future__ import annotations

import json
import sys

import server

failures: list[str] = []


def check(cond: bool, msg: str) -> None:
    if not cond:
        failures.append(msg)


# --- build_cloudwatch_filter_args ---
args = server.build_cloudwatch_filter_args(
    "/tickvault/prod/app", "ap-south-1", 1_700_000_000_000, 50, "ERROR"
)
check(args[:3] == ["aws", "logs", "filter-log-events"], f"argv prefix: {args[:3]}")
check("/tickvault/prod/app" in args, "log group missing from argv")
check("ap-south-1" in args, "region missing from argv")
check("ERROR" in args and "--filter-pattern" in args, "filter pattern missing")

args_no_filter = server.build_cloudwatch_filter_args("/g", "r", 1, 5, None)
check("--filter-pattern" not in args_no_filter, "filter flag present with no pattern")

args_clamp = server.build_cloudwatch_filter_args("/g", "r", 1, 999_999, None)
limit_val = args_clamp[args_clamp.index("--limit") + 1]
check(limit_val == "10000", f"limit not clamped to 10000: {limit_val}")

# --- parse_cloudwatch_events (sorted oldest..newest, trimmed) ---
sample = json.dumps(
    {
        "events": [
            {"timestamp": 2, "logStreamName": "s", "message": "b\n"},
            {"timestamp": 1, "logStreamName": "s", "message": "a"},
        ]
    }
)
evs = server.parse_cloudwatch_events(sample, 100)
check([e["message"] for e in evs] == ["a", "b"], f"events not sorted oldest..newest: {evs}")
check(server.parse_cloudwatch_events("not json", 10) == [], "bad json should parse to []")
check(server.parse_cloudwatch_events("", 10) == [], "empty should parse to []")

trimmed = server.parse_cloudwatch_events(
    json.dumps({"events": [{"timestamp": i, "message": str(i)} for i in range(5)]}), 2
)
check([e["message"] for e in trimmed] == ["3", "4"], f"limit-trim keeps newest: {trimmed}")

# --- graceful failure when no aws CLI / creds / portal (must NOT crash) ---
import os as _os

_saved_aws = {
    k: _os.environ.pop(k, None)
    for k in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN")
}
try:
    result = server.tool_cloudwatch_logs(minutes=5, limit=3)
    check(result.get("ok") is False, f"expected ok=False without creds: {result}")
    check("log_group" in result, "error result should still report the log_group")
    check(bool(result.get("error")), "error result should carry a human-readable message")
finally:
    for _k, _v in _saved_aws.items():
        if _v is not None:
            _os.environ[_k] = _v

# --- portal logs parser (reuses the existing dashboard `logs` endpoint) ---
raw = "\n".join(
    [
        "ERR_BEGIN",
        "boom 1",
        "boom 2",
        "ERR_END",
        "APP_BEGIN",
        "info a",
        "WS-GAP-05 respawn",
        "APP_END",
    ]
)
pevs = server.parse_portal_logs_raw(raw)
check(
    [e["message"] for e in pevs] == ["boom 1", "boom 2", "info a", "WS-GAP-05 respawn"],
    f"portal parse should drop markers, keep order: {pevs}",
)
check(
    [e["section"] for e in pevs] == ["err", "err", "app", "app"],
    f"portal parse should tag err/app sections: {pevs}",
)
check(server.parse_portal_logs_raw("") == [], "empty portal raw -> []")
check(server.parse_portal_logs_raw("no markers here") == [], "no-marker raw -> [] (nothing in a section)")

# --- portal client-side filter + trim ---
filt = server.filter_and_trim_portal_events(pevs, "WS-GAP-05", 100)
check([e["message"] for e in filt] == ["WS-GAP-05 respawn"], f"substring filter: {filt}")
trim = server.filter_and_trim_portal_events(pevs, None, 2)
check([e["message"] for e in trim] == ["info a", "WS-GAP-05 respawn"], f"trim keeps newest: {trim}")
check(server.filter_and_trim_portal_events(pevs, "nomatch", 100) == [], "no match -> []")

# --- portal preferred when env set (and no AWS key): ok=False, source=portal, never crashes ---
# AWS creds (if any) are cleared so the SigV4 path does NOT pre-empt the portal here.
_saved_aws_portal = {
    k: _os.environ.pop(k, None)
    for k in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN")
}
_os.environ["TICKVAULT_PORTAL_TOKEN"] = "test-token"
try:
    # https unreachable -> ok=False via the urlopen-failure path (no crash).
    _os.environ["TICKVAULT_PORTAL_URL"] = "https://127.0.0.1:1/unreachable"
    pres = server.tool_cloudwatch_logs(minutes=5, limit=3)
    check(pres.get("source") == "portal", f"portal path should be preferred when env set: {pres}")
    check(pres.get("ok") is False, f"unreachable portal -> ok=False (no crash): {pres}")
    check(bool(pres.get("error")), "portal failure should carry an error message")

    # SECURITY: plaintext http:// must be REFUSED before any token is sent.
    _os.environ["TICKVAULT_PORTAL_URL"] = "http://127.0.0.1:1/unreachable"
    hres = server.tool_cloudwatch_logs(minutes=5, limit=3)
    check(hres.get("ok") is False, f"http:// portal must be refused: {hres}")
    check("https" in (hres.get("error") or "").lower(), f"refusal must cite https: {hres}")
finally:
    _os.environ.pop("TICKVAULT_PORTAL_URL", None)
    _os.environ.pop("TICKVAULT_PORTAL_TOKEN", None)
    for _k, _v in _saved_aws_portal.items():
        if _v is not None:
            _os.environ[_k] = _v

# --- build_cloudwatch_sigv4_request (pure, deterministic via injected amz_now) ---
import datetime as _dt

_FIXED_NOW = _dt.datetime(2026, 6, 12, 1, 2, 3, tzinfo=_dt.timezone.utc)
_ACCESS = "AKIDEXAMPLE"
# Distinct, easily-greppable sentinels so we can prove the secret never leaks.
_SECRET = "SECRET-DO-NOT-LEAK-1234567890"
_TOKEN = "SESSION-TOKEN-DO-NOT-LEAK-abcdef"


def _build(filter_pattern=None, limit=50, token=None):
    return server.build_cloudwatch_sigv4_request(
        "ap-south-1",
        "/tickvault/prod/app",
        1_700_000_000_000,
        limit,
        filter_pattern,
        _ACCESS,
        _SECRET,
        token,
        _FIXED_NOW,
    )


url, body, headers = _build(filter_pattern="ERROR")

# Endpoint + canonical date are deterministic from the injected amz_now.
check(url == "https://logs.ap-south-1.amazonaws.com/", f"sigv4 url: {url}")
check(headers.get("X-Amz-Date") == "20260612T010203Z", f"amz_date: {headers.get('X-Amz-Date')}")
check(
    headers.get("X-Amz-Target") == "Logs_20140328.FilterLogEvents",
    f"x-amz-target: {headers.get('X-Amz-Target')}",
)
check(
    headers.get("Content-Type") == "application/x-amz-json-1.1",
    f"content-type: {headers.get('Content-Type')}",
)

# Authorization is a well-formed SigV4 header for the right scope.
auth = headers.get("Authorization", "")
check(auth.startswith("AWS4-HMAC-SHA256 "), f"auth scheme: {auth[:40]}")
check(
    f"Credential={_ACCESS}/20260612/ap-south-1/logs/aws4_request" in auth,
    f"auth credential scope: {auth}",
)
check(
    "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-target" in auth,
    f"signed headers (no token): {auth}",
)
import re as _re

_sig = _re.search(r"Signature=([0-9a-f]+)$", auth)
check(_sig is not None, f"signature hex tail missing: {auth}")
check(_sig is not None and len(_sig.group(1)) == 64, "signature must be 64 lowercase-hex chars")

# x-amz-content-sha256 must equal sha256(body) (payload-signed request).
import hashlib as _hashlib

check(
    headers.get("X-Amz-Content-Sha256") == _hashlib.sha256(body).hexdigest(),
    "x-amz-content-sha256 must match sha256(body)",
)

# Body is compact JSON carrying the filter + clamped limit.
_obj = json.loads(body.decode("utf-8"))
check(_obj.get("logGroupName") == "/tickvault/prod/app", f"body log group: {_obj}")
check(_obj.get("startTime") == 1_700_000_000_000, f"body startTime: {_obj}")
check(_obj.get("filterPattern") == "ERROR", f"body filter: {_obj}")
check(b" " not in body, "body must be compact (no spaces)")

# No filter -> no filterPattern key.
_, body_nf, _ = _build(filter_pattern=None)
check("filterPattern" not in json.loads(body_nf.decode("utf-8")), "no-filter body must omit filterPattern")

# limit clamped into [1, 10000].
_, body_hi, _ = _build(limit=999_999)
check(json.loads(body_hi.decode("utf-8"))["limit"] == 10000, "limit must clamp to 10000")
_, body_lo, _ = _build(limit=0)
check(json.loads(body_lo.decode("utf-8"))["limit"] == 1, "limit must floor to 1")

# Determinism: identical inputs -> byte-identical signed request.
url2, body2, headers2 = _build(filter_pattern="ERROR")
check((url2, body2, headers2) == (url, body, headers), "sigv4 build must be deterministic")

# Session token: present -> signed + sent as header, and listed in SignedHeaders
# in the CORRECT alphabetical position (x-amz-security-token BEFORE x-amz-target,
# else the signature is invalid -> AWS 403).
_, _, headers_tok = _build(filter_pattern="ERROR", token=_TOKEN)
check(headers_tok.get("X-Amz-Security-Token") == _TOKEN, "session token must be sent as header")
check(
    "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;"
    "x-amz-security-token;x-amz-target" in headers_tok.get("Authorization", ""),
    f"signed headers must be alphabetically sorted with token: {headers_tok.get('Authorization')}",
)

# SECURITY: the AWS SECRET must NEVER appear anywhere in the built request
# (url, body, or any header value) — it is only used to derive the HMAC.
for _label, _hdrs, _bdy in (
    ("no-token", headers, body),
    ("with-token", headers_tok, body),
):
    _blob = url + _bdy.decode("utf-8", "replace") + "".join(f"{k}:{v}" for k, v in _hdrs.items())
    check(_SECRET not in _blob, f"AWS SECRET leaked into the {_label} request blob")

# SECURITY: the secret/token must NOT appear in the TOOL's returned dict on the
# unreachable/bad-creds path (an invalid region fails DNS fast, no real call).
_saved_sig = {
    k: _os.environ.get(k)
    for k in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_DEFAULT_REGION", "AWS_REGION")
}
try:
    _os.environ["AWS_ACCESS_KEY_ID"] = _ACCESS
    _os.environ["AWS_SECRET_ACCESS_KEY"] = _SECRET
    _os.environ["AWS_SESSION_TOKEN"] = _TOKEN
    # Invalid region -> logs.<region>.amazonaws.com is NXDOMAIN -> fast failure.
    _os.environ["AWS_DEFAULT_REGION"] = "invalid-region-zzz9"
    _os.environ.pop("AWS_REGION", None)
    sres = server.tool_cloudwatch_logs(minutes=5, limit=3)
    check(sres.get("source") == "cloudwatch_sigv4", f"sigv4 path preferred when key in env: {sres}")
    check(sres.get("ok") is False, f"unreachable sigv4 -> ok=False (no crash): {sres}")
    check(bool(sres.get("error")), "sigv4 failure must carry an error message")
    _sres_blob = json.dumps(sres)
    check(_SECRET not in _sres_blob, "AWS SECRET leaked into sigv4 result dict")
    check(_TOKEN not in _sres_blob, "AWS SESSION TOKEN leaked into sigv4 result dict")

    # SECURITY: a region with host/path-injection chars is refused BEFORE any
    # network call (and without leaking the secret/token).
    _os.environ["AWS_DEFAULT_REGION"] = "evil@attacker.com/x"
    rres = server.tool_cloudwatch_logs(minutes=5, limit=3)
    check(rres.get("ok") is False, f"injection region must be refused: {rres}")
    check("region" in (rres.get("error") or "").lower(), f"refusal must cite region: {rres}")
    _rres_blob = json.dumps(rres)
    check(_SECRET not in _rres_blob and _TOKEN not in _rres_blob, "creds leaked into region-refusal dict")
finally:
    for _k, _v in _saved_sig.items():
        if _v is None:
            _os.environ.pop(_k, None)
        else:
            _os.environ[_k] = _v

if failures:
    print("FAIL:")
    for f in failures:
        print("  -", f)
    sys.exit(1)
print("cloudwatch_logs helper tests: ALL PASS")
sys.exit(0)
