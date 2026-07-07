"""QuestDB console BACK Lambda (B4) — VPC-attached dumb HTTP relay, ZERO secrets.

Invoked ONLY by the front Lambda (questdb-console-front) via
lambda:InvokeFunction with a JSON envelope:
    {"method", "path", "rawQuery", "headers": {accept, accept-encoding, content-type}}
Relays the request to QuestDB on the box's PRIVATE IP inside the VPC
(env QDB_BASE, e.g. "http://<vpc-private-ip>:9000" — injected by Terraform from
aws_instance.tv_app.private_ip; NEVER hardcoded) and returns:
    {"status", "headers": {content-type, content-encoding, cache-control}, "body_b64"}
or {"err": "box_unreachable" | "too_large"}.

WHY THIS LAMBDA EXISTS + WHY IT IS SECRET-FREE: a VPC Lambda in this VPC has
NO internet/AWS-API path (single public subnet, no NAT, no VPC endpoints), so
it CANNOT read SSM at runtime. All auth/secret work lives in the non-VPC front
Lambda (identical posture to operator-control: runtime SSM read, 60s cache,
fail-closed). This hop carries zero secrets and makes ZERO AWS SDK calls at
runtime — pure stdlib urllib to the box. The box SG opens TCP 9000 to THIS
Lambda's SG only; nothing public.

DEFENSE-IN-DEPTH (zero trust of the front): this handler re-runs the SAME
read-only SQL gate on /exec + /exp and re-applies the SAME GET/HEAD + path
whitelist. A compromised or buggy front cannot turn this relay into a writer.
"""

from __future__ import annotations

import base64
import os
import socket
import urllib.error
import urllib.parse
import urllib.request

# Deployed-bytes proof marker (proof-3 ratchet: test_build_marker_present).
QDB_CONSOLE_BUILD = "b4-qdb-console-2026-07-06-r3"


class _NoFollowRedirect(urllib.request.HTTPRedirectHandler):
    """B4 r3 shell-hang fix, belt layer (2026-07-06, evidence:
    scratchpad/repro-evidence.md §9a/§10). QuestDB 9.3.5 answers GET / with an
    UNFRAMED keep-alive 301 (no Content-Length, no Transfer-Encoding, no
    Connection header) and NEVER closes the socket — even under request
    `Connection: close` (raw-socket proof: 20.017s recv gap, zero close).
    urllib's DEFAULT HTTPRedirectHandler drains that body with fp.read()
    BEFORE following, so urlopen itself blocked until _TIMEOUT_SECS.
    Returning None makes any 3xx surface as an HTTPError WITHOUT any body
    read; the HTTPError arm below relays 3xx body-less. Safe globally: this
    lambda makes exactly one kind of HTTP call (stdlib urllib to the box —
    see module docstring; no AWS SDK, ratcheted by Hygiene tests)."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


urllib.request.install_opener(urllib.request.build_opener(_NoFollowRedirect))

QDB_BASE = os.environ.get("QDB_BASE", "")  # e.g. http://<box_private_ip>:9000
# Single-timeout tradeoff (FIX 8): urllib's `timeout` is the SOCKET-op timeout
# (it bounds BOTH the connect attempt AND each blocking recv). A stopped box
# off-hours never answers the SYN, so connect blocks up to this value before
# we return box_unreachable — kept moderate (12s) so a healthy query still has
# ample per-recv headroom while an offline box fails fast, and the back
# Lambda's own 26s timeout still covers a multi-chunk read. urllib does not
# cleanly split connect vs read; a real split isn't worth the complexity here.
_TIMEOUT_SECS = 12
MAX_BODY_BYTES = 4_100_000  # keep base64+JSON under Lambda's 6 MiB invoke envelope
_READ_CHUNK = 262_144

# ------------------------------------------------------- read-only SQL gate
# BYTE-IDENTICAL to questdb-console-front/handler.py (zero trust of the front;
# parity is ratcheted by test_handler.py). This is an INTENTIONAL read-only
# SUPERSET of operator-control's _is_safe_sql: the QuestDB 9.3.5 console SPA
# issues bare introspection functions (tables(), columns('t'), ...) which the
# select/show/explain/with first-word allowlist would reject; operator-control
# wraps its OWN queries in SELECT so it never needs them.

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

MAX_SQL_LEN = 20_000

# Read-only introspection funcs the QuestDB 9.3.5 console calls bare against
# /exec. The exact set is to be LIVE-VERIFIED against the deployed console.
_SQL_ALLOWED_FUNCS = (
    "tables",
    "columns",
    "table_columns",
    "materialized_views",
    "wal_tables",
    "table_partitions",
    "functions",
    "hydrate_table_metadata",
    "memory_metrics",
    "reader_pool",
    "query_activity",
    "flush_query_cache",
)


def _is_safe_sql(query: str) -> bool:
    """Read-only gate (hardened 2026-07-02; introspection superset 2026-07-03).
    Rules, all fail-closed:
    1. SINGLE STATEMENT: one trailing ';' is stripped; ANY remaining ';'
       rejects (closes the "select 1; <unlisted mutator>" chaining gap).
    2. NO SQL COMMENTS ('--' or '/*'): keeps the banned-word scan honest —
       comments could otherwise hide/split keywords.
    3. First TOKEN must be an allowed read-only keyword (select/show/explain/
       with) OR a bare call to a read-only introspection function in
       _SQL_ALLOWED_FUNCS (e.g. `tables()`), so the QuestDB console's own
       introspection works. "explainx ..." / "selector ..." are still rejected.
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
    # Rule 3 — allowed first token: keyword OR read-only introspection func.
    first = re.split(r"[^a-z]+", q, maxsplit=1)[0]
    allowed_first = first in _SQL_ALLOWED_PREFIXES
    if not allowed_first:
        m = re.match(r"\s*([a-z_]+)\s*\(", q)
        if m and m.group(1) in _SQL_ALLOWED_FUNCS:
            allowed_first = True
    if not allowed_first:
        return False
    # Rule 4 — banned keywords anywhere (still applies to func calls).
    for kw in _SQL_BANNED:
        if re.search(r"\b" + kw + r"\b", q):
            return False
    return True


# --------------------------------------------------------------- path gating
# SAME whitelist + traversal defense as the front (parity ratcheted by tests).

_STATIC_EXTS = (
    ".html", ".js", ".css", ".svg", ".png", ".woff2", ".ico", ".map", ".json",
    ".txt", ".webmanifest",
)


def _bad_path(raw_path: str) -> bool:
    """Path-confusion / traversal reject (zero-trust re-check of the front).
    True = reject. Checks the raw path AND its URL-decoded form for `..`,
    `//`, `\\`, encoded separators (%2e/%2f/%5c, any case) and control chars."""
    raw = raw_path or ""
    low = raw.lower()
    if "%2e" in low or "%2f" in low or "%5c" in low:
        return True
    forms = [raw, urllib.parse.unquote(raw)]
    for f in forms:
        if ".." in f or "//" in f or "\\" in f:
            return True
        if any(ord(ch) < 0x20 for ch in f):
            return True
    return False


def _classify_path(method: str, path: str) -> str:
    p = (path or "/").rstrip()
    allowed_sql = p in ("/exec", "/exp")
    allowed_static = (
        p in ("/", "/index.html", "/chk", "/settings")
        or p.startswith("/assets/")
        or p.lower().endswith(_STATIC_EXTS)
    )
    if not (allowed_sql or allowed_static):
        return "deny"
    if method not in ("GET", "HEAD"):
        return "method"
    return "sql" if allowed_sql else "static"


def _gate(method: str, path: str, raw_query: str) -> str:
    """Returns "" when the request may be relayed, else an err code."""
    kind = _classify_path(method, path)
    if kind in ("deny", "method"):
        return "denied"
    if kind == "sql":
        q = urllib.parse.parse_qs(raw_query or "").get("query", [""])[0]
        if not q or len(q) > MAX_SQL_LEN or not _is_safe_sql(q):
            return "denied_sql"
    return ""


def _read_capped(fp) -> tuple[bytes, bool]:
    """Incremental read bounded at MAX_BODY_BYTES. Returns (body, over) where
    over=True means the cap was exceeded (body is truncated to the cap). Used
    on BOTH the success and the HTTPError path so neither reads-then-slices."""
    chunks: list[bytes] = []
    total = 0
    while True:
        chunk = fp.read(_READ_CHUNK)
        if not chunk:
            return b"".join(chunks), False
        room = MAX_BODY_BYTES - total
        if len(chunk) >= room:
            chunks.append(chunk[:room])
            return b"".join(chunks), True
        total += len(chunk)
        chunks.append(chunk)


# ------------------------------------------------------------------- handler


def lambda_handler(event, _context):
    method = str((event or {}).get("method", "GET")).upper()
    path = str((event or {}).get("path", "/"))
    raw_query = str((event or {}).get("rawQuery", "") or "")
    in_headers = (event or {}).get("headers") or {}

    if not QDB_BASE:
        return {"err": "box_unreachable"}

    # Zero-trust path-confusion reject BEFORE classification (the front already
    # rejects these, but the VPC hop trusts nothing).
    if _bad_path(path):
        return {"err": "bad_path"}

    denied = _gate(method, path, raw_query)
    if denied:
        return {"err": denied}

    # B4 r3 shell-load fix (2026-07-06, evidence: scratchpad/repro-evidence.md).
    # QuestDB 9.3.5 does NOT serve the console shell at "/": it answers an
    # UNFRAMED keep-alive 301 -> /index.html and never closes the socket, so
    # any read-until-EOF relay hangs until its socket timeout (GET / had
    # NEVER returned 200 through this console). Never elicit the 301: fetch
    # the framed shell directly. /index.html is 200 + Content-Length: 765 in
    # ~4ms through this exact relay code and itself carries the per-release
    # hashed asset refs (assets/index-<hash>.js), so nothing drifts on a
    # QuestDB upgrade. GET-only on purpose: HEAD / keeps relaying verbatim
    # (QuestDB answers a FRAMED chunked 405 in 1.2ms; HEAD /index.html is
    # live-unverified — don't change untested behavior). The browser URL
    # stays "/"; the shell's RELATIVE asset refs resolve identically.
    if method == "GET" and path == "/":
        path = "/index.html"

    url = QDB_BASE.rstrip("/") + path + (f"?{raw_query}" if raw_query else "")
    req = urllib.request.Request(url, method=method)
    # Forward only `accept` + `content-type` from the browser. `accept-encoding`
    # is deliberately NOT forwarded — see below.
    for k in ("accept", "content-type"):
        v = in_headers.get(k)
        if v:
            req.add_header(k, str(v))
    # B4 r3 header hygiene (2026-07-06). These two headers are RETAINED as
    # defense-in-depth ONLY — they are NOT the shell fix. Raw-socket proof
    # 2026-07-06 (scratchpad/repro-evidence.md §3/§10): QuestDB 9.3.5 does
    # NOT honor request `Connection: close` on the / 301 (no close after a
    # 20s recv gap), disproving the r2 theory that the server would EOF the
    # socket once the body was sent. The actual fixes are (1) the
    # GET / -> /index.html rewrite above (never elicit the only
    # delimiter-less response the server produces) and (2) the
    # _NoFollowRedirect opener + body-less 3xx relay.
    # `Accept-Encoding: identity` still guarantees Content-Length-framed
    # uncompressed bodies on framed paths, under the MAX_BODY_BYTES cap.
    req.add_header("Accept-Encoding", "identity")
    req.add_header("Connection", "close")

    try:
        with urllib.request.urlopen(req, timeout=_TIMEOUT_SECS) as resp:  # noqa: S310 — QDB_BASE is TF-pinned to the box private IP
            body, over = _read_capped(resp)
            if over:
                return {"err": "too_large"}  # → front 502, never 503
            out_headers = {}
            for k in ("content-type", "content-encoding", "cache-control", "location"):
                v = resp.headers.get(k)
                if v:
                    out_headers[k] = v
            return {
                "status": int(resp.status),
                "headers": out_headers,
                "body_b64": base64.b64encode(body).decode(),
            }
    except urllib.error.HTTPError as exc:
        # QuestDB error responses (e.g. /exec 400 with JSON body) are VALID
        # console traffic — relay them. 3xx is special: with _NoFollowRedirect
        # installed it lands here WITHOUT a drained body, and it may be
        # DELIMITER-LESS on a socket QuestDB never closes (the / 301 class) —
        # reading it would block until _TIMEOUT_SECS. Relay 3xx body-less
        # (status + Location); the front forwards Location and the browser
        # follows. Non-3xx errors keep the size-capped incremental read.
        out_headers = {}
        for k in ("content-type", "content-encoding", "cache-control", "location"):
            v = exc.headers.get(k) if exc.headers else None
            if v:
                out_headers[k] = v
        if 300 <= int(exc.code) < 400:
            return {"status": int(exc.code), "headers": out_headers, "body_b64": ""}
        body, _over = _read_capped(exc)
        return {
            "status": int(exc.code),
            "headers": out_headers,
            "body_b64": base64.b64encode(body).decode(),
        }
    except (TimeoutError, socket.timeout):
        # Diagnostic honesty (B4 r2): a connect/read timeout means the box was
        # REACHABLE but slow (or the response never framed an EOF) — NOT a
        # refused connection. Map it to a distinct `upstream_timeout` so the
        # front returns 504 (gateway timeout), reserving the offline-503 for a
        # genuine connection refusal. (socket.timeout is TimeoutError on 3.10+.)
        return {"err": "upstream_timeout"}
    except urllib.error.URLError as exc:
        # A URLError WRAPPING a socket timeout is still an upstream timeout; a
        # refused / DNS / reset connection is a genuine box_unreachable.
        if isinstance(exc.reason, (TimeoutError, socket.timeout)):
            return {"err": "upstream_timeout"}
        return {"err": "box_unreachable"}
    except Exception:  # noqa: BLE001 — any other error → box stopped / unreachable
        return {"err": "box_unreachable"}
