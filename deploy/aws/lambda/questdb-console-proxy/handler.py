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
import urllib.error
import urllib.parse
import urllib.request

# Deployed-bytes proof marker (proof-3 ratchet: test_build_marker_present).
QDB_CONSOLE_BUILD = "b4-qdb-console-2026-07-03-r1"

QDB_BASE = os.environ.get("QDB_BASE", "")  # e.g. http://<box_private_ip>:9000
_TIMEOUT_SECS = 25
MAX_BODY_BYTES = 5_500_000  # keep the front's Lambda-payload envelope honest
_READ_CHUNK = 262_144

# ------------------------------------------------------- read-only SQL gate
# VERBATIM mirror of operator-control/handler.py — duplicated here on purpose
# (zero trust of the front; parity is ratcheted by test_handler.py).

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


def _is_safe_sql(query: str) -> bool:
    """Read-only gate (hardened 2026-07-02) — same 4 fail-closed rules as
    operator-control: single statement, no comments, allowed first word,
    no banned mutator anywhere. Pure function — unit-tested."""
    q = (query or "").strip().lower()
    if not q:
        return False
    import re  # noqa: PLC0415

    if q.endswith(";"):
        q = q[:-1].rstrip()
    if not q or ";" in q:
        return False
    if "--" in q or "/*" in q:
        return False
    first = re.split(r"[^a-z]+", q, maxsplit=1)[0]
    if first not in _SQL_ALLOWED_PREFIXES:
        return False
    for kw in _SQL_BANNED:
        if re.search(r"\b" + kw + r"\b", q):
            return False
    return True


# --------------------------------------------------------------- path gating
# SAME whitelist as the front (parity ratcheted by tests).

_STATIC_EXTS = (
    ".html", ".js", ".css", ".svg", ".png", ".woff2", ".ico", ".map", ".json",
    ".txt", ".webmanifest",
)


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


# ------------------------------------------------------------------- handler


def lambda_handler(event, _context):
    method = str((event or {}).get("method", "GET")).upper()
    path = str((event or {}).get("path", "/"))
    raw_query = str((event or {}).get("rawQuery", "") or "")
    in_headers = (event or {}).get("headers") or {}

    if not QDB_BASE:
        return {"err": "box_unreachable"}

    denied = _gate(method, path, raw_query)
    if denied:
        return {"err": denied}

    url = QDB_BASE.rstrip("/") + path + (f"?{raw_query}" if raw_query else "")
    req = urllib.request.Request(url, method=method)
    for k in ("accept", "accept-encoding", "content-type"):
        v = in_headers.get(k)
        if v:
            req.add_header(k, str(v))

    try:
        with urllib.request.urlopen(req, timeout=_TIMEOUT_SECS) as resp:  # noqa: S310 — QDB_BASE is TF-pinned to the box private IP
            chunks: list[bytes] = []
            total = 0
            while True:
                chunk = resp.read(_READ_CHUNK)
                if not chunk:
                    break
                total += len(chunk)
                if total > MAX_BODY_BYTES:
                    return {"err": "too_large"}
                chunks.append(chunk)
            out_headers = {}
            for k in ("content-type", "content-encoding", "cache-control"):
                v = resp.headers.get(k)
                if v:
                    out_headers[k] = v
            return {
                "status": int(resp.status),
                "headers": out_headers,
                "body_b64": base64.b64encode(b"".join(chunks)).decode(),
            }
    except urllib.error.HTTPError as exc:
        # QuestDB error responses (e.g. /exec 400 with JSON body) are VALID
        # console traffic — relay them so the UI shows the real message.
        body = exc.read()[:MAX_BODY_BYTES]
        out_headers = {}
        for k in ("content-type", "content-encoding", "cache-control"):
            v = exc.headers.get(k) if exc.headers else None
            if v:
                out_headers[k] = v
        return {
            "status": int(exc.code),
            "headers": out_headers,
            "body_b64": base64.b64encode(body).decode(),
        }
    except Exception:  # noqa: BLE001 — URLError / socket timeout / box stopped
        return {"err": "box_unreachable"}
