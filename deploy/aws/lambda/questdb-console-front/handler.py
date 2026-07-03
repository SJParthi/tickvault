"""QuestDB console FRONT Lambda (B4) — auth + read-only gate, NON-VPC.

Browser → API-Gateway v2 ($default, payload v2) → THIS Lambda → (invoke) →
the VPC-attached back Lambda (questdb-console-proxy) → http://<box>:9000.

WHY TWO LAMBDAS: a VPC Lambda in this VPC has NO internet/AWS-API path (single
public subnet, no NAT, no VPC endpoints), so it cannot read the SSM device-key
secret at runtime. THIS front Lambda stays outside the VPC and handles ALL
secret work exactly like the existing operator-control Lambda (runtime SSM
read, 60s cache, fail-closed, never in env vars / TF state); the VPC back
Lambda is a dumb, secret-free HTTP relay. The box SG opens TCP 9000 to the
back-Lambda SG ONLY — nothing public.

AUTH (stateless, HMAC over the SSM control secret, constant-time compares):
  * One-click link token (≤90s): minted by the operator portal's
    `qdb_console_url` action as  <exp>.<hexhmac(secret, "qdblink|<exp>")>,
    consumed via GET /open?tok=... → 302 / + session cookie.
  * Session cookie qdb_sess = <exp>.<hexhmac(secret, "qdbsess|<exp>")>
    (HttpOnly; Secure; SameSite=Lax; 12h).
  * Fallback login page (works from ANY browser without the portal): paste the
    device key → POST /login → compare_digest vs the SSM secret → cookie.
  * `Authorization: Bearer <secret>` accepted directly on any request.

PROXY GATE (deny-by-default, applied only when authenticated):
  * GET/HEAD only (405 otherwise) — blocks /imp uploads + settings writes.
  * /exec + /exp: the `query` param must pass the VERBATIM operator-control
    `_is_safe_sql` mirror (single statement, no comments, read-only first
    word, banned mutators anywhere) + a 20 000-char length cap; the row cap
    is applied via the `_cap_sql_rows` mirror (LIMIT clamp/append).
  * /chk, /settings (read), /, /index.html, /assets/* + static console files
    pass through; /imp and anything else → 403.

Never logs secrets, tokens, or cookie values. Structured JSON log per request.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import time
import urllib.parse

# Deployed-bytes proof marker (proof-3 ratchet: test_build_marker_present).
QDB_CONSOLE_BUILD = "b4-qdb-console-2026-07-03-r1"

REGION = os.environ.get("AWS_REGION", "ap-south-1")
BACK_FN_ARN = os.environ.get("BACK_FN_ARN", "")
CONTROL_SECRET_PARAM = os.environ.get(
    "CONTROL_SECRET_PARAM", "/tickvault/prod/operator/control-secret"
)
SESSION_TTL_SECS = 12 * 3600
LINK_TOKEN_TTL_SECS = 90
MAX_SQL_LEN = 20_000

# Lambda response payload hard limit is 6 MB; cap the relayed body below it
# (base64 inflation ~4/3 → keep the raw body ≤ ~4.1 MB after encoding; the
# back Lambda already caps its read at this many raw bytes).
MAX_BODY_BYTES = 5_500_000

# Lazy-init boto3 clients so the pure-function tests run without boto3
# installed (mirrors operator-control/handler.py).
_clients: dict[str, object] = {}


def _client(name: str):
    if name not in _clients:
        import boto3  # noqa: PLC0415

        _clients[name] = boto3.client(name, region_name=REGION)
    return _clients[name]


# --------------------------------------------------------------- secret (SSM)
# VERBATIM mirror of operator-control: runtime SSM read, 60s cache,
# fail-closed on any error (missing secret => deny everything).


def _load_param(param: str) -> str:
    if not param:
        return ""
    try:
        return _client("ssm").get_parameter(Name=param, WithDecryption=True)["Parameter"]["Value"]
    except Exception:  # noqa: BLE001 — missing => deny (fail closed)
        return ""


_SECRET_TTL_SECS = 60.0
_cache: dict[str, dict] = {}


def _cached_param(param: str) -> str:
    now = time.monotonic()
    c = _cache.get(param)
    if not c or not c["value"] or (now - c["ts"]) > _SECRET_TTL_SECS:
        _cache[param] = {"value": _load_param(param), "ts": now}
    return _cache[param]["value"]


def _control_secret() -> str:
    return _cached_param(CONTROL_SECRET_PARAM)


# ------------------------------------------------------- read-only SQL gate
# VERBATIM mirror of operator-control/handler.py (_SQL_ALLOWED_PREFIXES,
# _SQL_BANNED, _is_safe_sql, _cap_sql_rows). Parity with that gate is the
# contract — test_handler.py asserts it case-by-case.

_SQL_ALLOWED_PREFIXES = ("select", "show", "explain", "with")
_SQL_BANNED = (
    "insert",
    "update",
    "delete",
    "drop",
    "alter",
    "truncate",
    "create",
    "copy",
    "rename",
    "reindex",
    "vacuum",
    "grant",
    "revoke",
    # QuestDB-specific mutators (DB-console hardening 2026-07-02) — banned
    # anywhere as belt-and-braces on top of the single-statement rule.
    "backup",
    "checkpoint",
    "snapshot",
    "cancel",
    "set",
    "refresh",
    "detach",
    "attach",
    "dedup",
    "squash",
    "resume",
    "suspend",
)

_SQL_MAX_ROWS = 1000


def _is_safe_sql(query: str) -> bool:
    """Read-only gate (hardened 2026-07-02). Rules, all fail-closed:
    1. SINGLE STATEMENT: one trailing ';' is stripped; ANY remaining ';'
       rejects (closes the "select 1; <unlisted mutator>" chaining gap).
    2. NO SQL COMMENTS ('--' or '/*'): keeps the banned-word scan honest —
       comments could otherwise hide/split keywords.
    3. First WORD must be an allowed read-only keyword (not just a prefix,
       so "explainx ..." / "selector ..." are rejected).
    4. No mutating keyword (whole-word) anywhere — incl. QuestDB mutators
       (BACKUP/CHECKPOINT/SNAPSHOT/...). False-positive rejects on string
       literals are accepted (fail-closed).
    Pure function — fully unit-tested."""
    q = (query or "").strip().lower()
    if not q:
        return False
    import re  # noqa: PLC0415

    # Rule 1 — single statement: strip ONE trailing ';', reject any other.
    if q.endswith(";"):
        q = q[:-1].rstrip()
    if not q or ";" in q:
        return False
    # Rule 2 — no comments.
    if "--" in q or "/*" in q:
        return False
    # Rule 3 — allowed first word.
    first = re.split(r"[^a-z]+", q, maxsplit=1)[0]
    if first not in _SQL_ALLOWED_PREFIXES:
        return False
    # Rule 4 — banned keywords anywhere.
    for kw in _SQL_BANNED:
        if re.search(r"\b" + kw + r"\b", q):
            return False
    return True


def _cap_sql_rows(query: str, cap: int = _SQL_MAX_ROWS) -> str:
    """Enforce a server-side row cap on an already-validated read-only query.
    A trailing `LIMIT n` above the cap is clamped to the cap; a SELECT/WITH
    query with no trailing LIMIT gets `LIMIT <cap>` appended. SHOW/EXPLAIN
    are left untouched (tiny output; appending LIMIT to SHOW is a syntax
    error). QuestDB's range/negative forms (`LIMIT lo,hi` / `LIMIT -n`) are
    left as-is — the 5.5 MB relay body cap remains the belt-and-braces
    output bound in every case. Pure function — unit-tested."""
    import re  # noqa: PLC0415

    q = (query or "").strip()
    if q.endswith(";"):
        q = q[:-1].rstrip()
    m = re.search(r"(?i)\blimit\s+(-?\d+)(\s*,\s*-?\d+)?\s*$", q)
    if m:
        lo, hi = m.group(1), m.group(2)
        if hi is None and not lo.startswith("-") and int(lo) > cap:
            return q[: m.start()] + f"LIMIT {cap}"
        return q
    first = re.split(r"[^a-z]+", q.lower(), maxsplit=1)[0]
    if first in ("select", "with"):
        return f"{q} LIMIT {cap}"
    return q


# ------------------------------------------------------------------- signing


def _hmac_hex(secret: str, msg: str) -> str:
    return hmac.new(secret.encode(), msg.encode(), hashlib.sha256).hexdigest()


def _mint_signed(secret: str, prefix: str, exp_epoch: int) -> str:
    """<exp>.<hexhmac(secret, "<prefix>|<exp>")> — the shared token/cookie
    shape. The portal's qdb_console_url action mints link tokens with
    prefix="qdblink"; sessions here use prefix="qdbsess"."""
    return f"{exp_epoch}.{_hmac_hex(secret, f'{prefix}|{exp_epoch}')}"


def _verify_signed(secret: str, prefix: str, value: str, now_epoch: int) -> bool:
    """Constant-time verify of an <exp>.<hexhmac> value. Fail-closed on any
    malformation or expiry. Never raises."""
    if not secret or not value or "." not in value:
        return False
    exp_s, _, sig = value.partition(".")
    try:
        exp = int(exp_s)
    except ValueError:
        return False
    if exp <= now_epoch:
        return False
    expected = _hmac_hex(secret, f"{prefix}|{exp}")
    return hmac.compare_digest(sig, expected)


# ------------------------------------------------------------- event helpers


def _http_method(event: dict) -> str:
    try:
        return str(event["requestContext"]["http"]["method"]).upper()
    except (KeyError, TypeError):
        return str(event.get("httpMethod", "POST")).upper()


def _path(event: dict) -> str:
    return str(event.get("rawPath") or event.get("path") or "/")


def _cookies(event: dict) -> dict:
    """API-GW v2 delivers cookies as a list of 'k=v' strings."""
    out: dict[str, str] = {}
    for c in event.get("cookies") or []:
        k, _, v = str(c).partition("=")
        if k:
            out[k.strip()] = v
    return out


def _authenticated(event: dict, now_epoch: int) -> bool:
    secret = _control_secret()
    if not secret:
        return False  # fail closed — no secret => nobody gets in
    headers = event.get("headers") or {}
    auth = headers.get("authorization", "")
    if auth.startswith("Bearer ") and hmac.compare_digest(auth[len("Bearer ") :], secret):
        return True
    return _verify_signed(secret, "qdbsess", _cookies(event).get("qdb_sess", ""), now_epoch)


# ------------------------------------------------------------------ responses


def _json_resp(status: int, body: dict, extra_headers: dict | None = None) -> dict:
    h = {"content-type": "application/json", "cache-control": "no-store"}
    h.update(extra_headers or {})
    return {"statusCode": status, "headers": h, "body": json.dumps(body)}


def _sql_reject_resp(query: str, msg: str) -> dict:
    # Mimic QuestDB's /exec error JSON so the console UI renders the message
    # in its own error surface instead of a blank grid.
    return _json_resp(400, {"query": query, "error": msg, "position": 0})


_LOGIN_HTML = """<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>tickvault · QuestDB console</title>
<style>
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;
background:#0b0f14;color:#dbe6ee;font:15px/1.5 -apple-system,system-ui,sans-serif}
.card{background:#121821;border:1px solid #1f2a36;border-radius:12px;padding:28px;max-width:360px;width:90%}
h1{font-size:17px;margin:0 0 6px}p{color:#7d8b98;font-size:13px;margin:0 0 16px}
input{width:100%;box-sizing:border-box;background:#0b0f14;border:1px solid #2a3947;color:#dbe6ee;
border-radius:8px;padding:10px;font-size:14px}
button{margin-top:12px;width:100%;background:#1667d9;border:0;color:#fff;border-radius:8px;
padding:10px;font-size:14px;cursor:pointer}
.err{color:#ff7a7a;font-size:13px;margin-top:10px}
</style></head><body>
<div class="card"><h1>🗄 QuestDB console</h1>
<p>Read-only. Paste your operator device key to unlock this browser for 12 hours.</p>
<form method="POST" action="/login">
<input type="password" name="key" placeholder="device key" autofocus autocomplete="off">
<button type="submit">Unlock</button></form>
__ERR__
</div></body></html>"""


def _login_page(status: int = 401, err: str = "") -> dict:
    body = _LOGIN_HTML.replace(
        "__ERR__", f'<div class="err">{err}</div>' if err else ""
    )
    return {
        "statusCode": status,
        "headers": {
            "content-type": "text/html; charset=utf-8",
            "cache-control": "no-store",
            "referrer-policy": "no-referrer",
        },
        "body": body,
    }


def _session_redirect(secret: str, now_epoch: int) -> dict:
    exp = now_epoch + SESSION_TTL_SECS
    cookie = (
        f"qdb_sess={_mint_signed(secret, 'qdbsess', exp)}; "
        f"HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={SESSION_TTL_SECS}"
    )
    return {
        "statusCode": 302,
        "headers": {"location": "/", "cache-control": "no-store"},
        "cookies": [cookie],
        "body": "",
    }


# --------------------------------------------------------------- path gating

# Query params forwarded on /exec + /exp (the console sends these; anything
# else is dropped — deny-by-default).
_PASSTHROUGH_PARAMS = ("limit", "count", "nm", "timings", "explain", "src", "version")

_STATIC_EXTS = (
    ".html", ".js", ".css", ".svg", ".png", ".woff2", ".ico", ".map", ".json",
    ".txt", ".webmanifest",
)


def _classify_path(method: str, path: str) -> str:
    """Deny-by-default path/method classifier. Returns one of:
    "sql"     — /exec or /exp (gate the query param)
    "static"  — console shell/assets + /chk + read-only /settings
    "method"  — allowed path but non-GET/HEAD method (→ 405)
    "deny"    — everything else (→ 403), incl. /imp and settings writes
    """
    p = path.rstrip()
    allowed_sql = p in ("/exec", "/exp")
    allowed_static = (
        p in ("/", "/index.html", "/chk", "/settings")
        or p.startswith("/assets/")
        or p.lower().endswith(_STATIC_EXTS)
    )
    if not (allowed_sql or allowed_static):
        return "deny"  # /imp, /imp?..., unknown paths — never forwarded
    if method not in ("GET", "HEAD"):
        return "method"  # settings writes / /imp POST / console POSTs → 405
    return "sql" if allowed_sql else "static"


def _clamp_limit_param(raw: str, cap: int = _SQL_MAX_ROWS) -> str:
    """Clamp the console's `limit` pagination param (`n` or `lo,hi`) so a
    single page can never exceed the row cap. Malformed → the cap."""
    try:
        if "," in raw:
            lo_s, _, hi_s = raw.partition(",")
            lo, hi = int(lo_s), int(hi_s)
            if hi - lo > cap:
                hi = lo + cap
            return f"{lo},{hi}"
        n = int(raw)
        return str(min(n, cap)) if n >= 0 else raw
    except ValueError:
        return str(cap)


def _build_sql_raw_query(path: str, params: dict) -> tuple[str, str] | dict:
    """Validate + rebuild the query string for /exec | /exp.
    Returns (capped_query, raw_query_string) on success, or an API-GW error
    response dict on rejection."""
    # API-GW v2 already URL-decodes queryStringParameters values.
    q = params.get("query") or ""
    if not q.strip():
        return _sql_reject_resp("", "missing query parameter")
    if len(q) > MAX_SQL_LEN:
        return _sql_reject_resp(q[:200], f"query too long (max {MAX_SQL_LEN} chars)")
    if not _is_safe_sql(q):
        return _sql_reject_resp(
            q, "read-only console: only single-statement SELECT/SHOW/EXPLAIN/WITH"
        )
    capped = _cap_sql_rows(q)
    fwd = {"query": capped}
    for k in _PASSTHROUGH_PARAMS:
        if k in params and params[k] is not None:
            fwd[k] = _clamp_limit_param(str(params[k])) if k == "limit" else str(params[k])
    if path == "/exp" and "limit" not in fwd:
        fwd["limit"] = str(_SQL_MAX_ROWS)  # same belt-and-braces operator-control uses
    return capped, urllib.parse.urlencode(fwd)


# ------------------------------------------------------------------ back relay


def _invoke_back(payload: dict) -> dict:
    """lambda:InvokeFunction (RequestResponse) to the VPC back Lambda. Returns
    the back's JSON dict, or {"err": "..."} on any invoke failure."""
    if not BACK_FN_ARN:
        return {"err": "back_not_configured"}
    try:
        r = _client("lambda").invoke(
            FunctionName=BACK_FN_ARN,
            InvocationType="RequestResponse",
            Payload=json.dumps(payload).encode(),
        )
        body = r["Payload"].read()
        if r.get("FunctionError"):
            return {"err": "back_function_error"}
        return json.loads(body)
    except Exception:  # noqa: BLE001 — invoke/network failure
        return {"err": "back_invoke_failed"}


_OFFLINE_MSG = (
    "tickvault box is offline (auto-stopped outside 08:30–16:30 IST) — "
    "try during market hours"
)


def _relay(method: str, path: str, raw_query: str, headers: dict) -> dict:
    fwd_headers = {
        k: v
        for k, v in (headers or {}).items()
        if k.lower() in ("accept", "accept-encoding", "content-type")
    }
    back = _invoke_back(
        {"method": method, "path": path, "rawQuery": raw_query, "headers": fwd_headers}
    )
    err = back.get("err")
    if err == "too_large":
        return _json_resp(502, {"error": "response too large (>5.5MB) — narrow the query"})
    if err:
        return _json_resp(503, {"error": _OFFLINE_MSG})
    resp_headers = {"cache-control": "no-store"}
    for k in ("content-type", "content-encoding", "cache-control"):
        v = (back.get("headers") or {}).get(k)
        if v:
            resp_headers[k] = v
    body_b64 = back.get("body_b64", "") or ""
    if len(body_b64) > int(MAX_BODY_BYTES * 4 / 3) + 8:
        return _json_resp(502, {"error": "response too large (>5.5MB) — narrow the query"})
    return {
        "statusCode": int(back.get("status", 200)),
        "headers": resp_headers,
        "body": body_b64,
        "isBase64Encoded": True,
    }


# ---------------------------------------------------------------------- log


def _log(path: str, method: str, outcome: str, status: int, sql_head: str = "") -> None:
    # Structured JSON per request. NEVER logs secrets, tokens, or cookies.
    print(  # noqa: T201 — CloudWatch structured log line
        json.dumps(
            {
                "evt": "qdb_console",
                "path": path,
                "method": method,
                "outcome": outcome,
                "sql_head": sql_head[:200],
                "status": status,
            }
        )
    )


# ------------------------------------------------------------------- handler


def _body_text(event: dict) -> str:
    raw = event.get("body") or ""
    if event.get("isBase64Encoded"):
        try:
            return base64.b64decode(raw).decode("utf-8", "replace")
        except Exception:  # noqa: BLE001 — malformed body → empty (fail closed)
            return ""
    return str(raw)


def lambda_handler(event, _context):
    now = int(time.time())
    method = _http_method(event)
    path = _path(event)
    params = event.get("queryStringParameters") or {}
    secret = _control_secret()

    # ---- unauthenticated endpoints: /open (link token) + /login (paste key)
    if method == "GET" and path == "/open":
        tok = params.get("tok", "") or ""
        if secret and _verify_signed(secret, "qdblink", tok, now):
            _log(path, method, "ok", 302)
            return _session_redirect(secret, now)
        _log(path, method, "denied_auth", 401)
        return _login_page(401, "link expired — paste your device key instead")

    if method == "POST" and path == "/login":
        body = _body_text(event)
        key = ""
        ctype = ((event.get("headers") or {}).get("content-type") or "").lower()
        if "json" in ctype:
            try:
                key = str(json.loads(body or "{}").get("key", ""))
            except json.JSONDecodeError:
                key = ""
        else:  # the login <form> posts x-www-form-urlencoded
            key = urllib.parse.parse_qs(body).get("key", [""])[0]
        if secret and key and hmac.compare_digest(key, secret):
            _log(path, method, "ok", 302)
            return _session_redirect(secret, now)
        _log(path, method, "denied_auth", 401)
        return _login_page(401, "wrong key")

    # ---- everything else requires auth (cookie or Bearer) — fail closed
    if not _authenticated(event, now):
        _log(path, method, "denied_auth", 401)
        return _login_page(401)

    kind = _classify_path(method, path)
    if kind == "deny":
        _log(path, method, "denied_path", 403)
        return _json_resp(403, {"error": "path not allowed on the read-only console"})
    if kind == "method":
        _log(path, method, "denied_path", 405)
        return _json_resp(405, {"error": "read-only console: GET/HEAD only"})

    if kind == "sql":
        built = _build_sql_raw_query(path, params)
        if isinstance(built, dict):  # rejection response
            _log(path, method, "denied_sql", built["statusCode"], str(params.get("query", ""))[:200])
            return built
        capped_query, raw_query = built
        resp = _relay(method, path, raw_query, event.get("headers") or {})
        _log(path, method, "back_error" if resp["statusCode"] >= 502 else "ok", resp["statusCode"], capped_query)
        return resp

    # static shell/assets/chk/settings(read) passthrough
    raw_query = event.get("rawQueryString", "") or ""
    resp = _relay(method, path, raw_query, event.get("headers") or {})
    _log(path, method, "back_error" if resp["statusCode"] >= 502 else "ok", resp["statusCode"])
    return resp
