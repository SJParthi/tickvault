"""Operator portal Lambda — ONE URL to run the whole product (VIEW + CONTROL).

Open the Function URL in a browser and you get a device-locked mission-control
page with THREE tabs (2026-07-02 redesign — operator: "webpage looks completely
messy, too many buttons"; 2026-07-16 REST-only cleanup — operator: "since we
have deleted and removed everything do we really need all these displaying and
views in dashboard — it can be removed?" → "approved"):
  * Overview — box status, the official-minute-candles hero (today's rows in
    spot_1m_rest + option_chain_1m + option_contract_1m_rest, per table + per
    feed — the runtime is REST-only since 2026-07-15: every live-feed
    WebSocket is deleted), the dedup-keys shield (greyed to a single "box
    stopped" banner when the instance is off), feed toggles with a
    rest_fetch_audit-sourced per-feed pull line, a thin AWS strip (spend /
    alarms / disk, click-to-expand), a latency card (QuestDB round-trip +
    clock skew + the dormant order-placement shield, plus today's per-source
    "how fast after each minute closed" table from rest_fetch_audit), and
    ONE context-aware Start/Stop instance button.
  * Data — official minute rows captured today (per table, per feed) and the
    READ-ONLY database console (tables + columns + query grid + CSV download —
    writes blocked server-side).
  * Admin — restart/stop app + QuestDB, a COLLAPSED danger zone (severity
    picker over the three wipes, each with its own typed confirm token), and
    the device lock.
The former GitHub + Logs tabs are gone. The `logs` API action is KEPT (the
tickvault-logs MCP server POSTs {"action":"logs"}); the gh_* actions had no
caller outside the deleted tab and are removed. The live-feed-era panels
(ticks hero + sparkline, tick-conservation / sub-second / peak-tps shields,
per-feed WS TCP/TLS probes, the exchange→received lag grid over `ticks`, the
tick-path histogram shields, the cross-verify card, the groww-only wipe) were
removed 2026-07-16 — their producers were deleted with the live feeds
(PRs #1522/#1569/#1581; the cross-verify card had been frozen at a 2026-07-13
FAIL forever, and the groww wipe rewrote only the frozen legacy tables).

ONE URL, two layers:
  * `GET  /` → serves the portal HTML. PUBLIC shell with ZERO secrets — it just
               renders a lock screen + tabs. It can do nothing until unlocked.
  * `POST /` → every action requires `Authorization: Bearer <secret>`
               (constant-time compare). The key is saved only in the device's
               localStorage and sent on every POST. Only the operator's laptop
               + phone hold it, so in practice only those devices work.

SECURITY MODEL (deliberately strict — this can stop a live trading box):
* Bearer secret from SSM SecureString /tickvault/<env>/operator/control-secret.
* IAM scoped to exactly this instance + read-only CloudWatch/CostExplorer +
  the SSM secret. Nothing else. (The former GitHub-PAT read went with the
  GitHub tab; Terraform may still set OPERATOR_GITHUB_TOKEN_PARAM — ignored.)
* Destructive box actions (stop/reboot/restart-app/stop-app) are blocked during
  market hours (09:15-15:30 IST Mon-Fri) unless {"force": true}.
* DATA-DESTRUCTIVE actions (wipe-questdb/docker-reset/
  docker-nuke-bare) are HARD-LOCKED during market hours — refused with 409
  even with {"force": true}. A mid-market wipe destroys data that can never
  be re-fetched (operator incident 2026-07-02 15:05 IST: forced wipe-ALL +
  docker-reset deleted ~4.5M rows + 77s of live feed). Lifecycle actions
  keep the force override so emergencies stay possible.
* The SQL box is READ-ONLY: only SELECT/SHOW/EXPLAIN/WITH are accepted; any
  mutating keyword is rejected before it ever reaches QuestDB.
"""

from __future__ import annotations

import datetime
import hmac
import json
import os
import time

REGION = os.environ.get("AWS_REGION", "ap-south-1")
INSTANCE_ID = os.environ.get("TV_INSTANCE_ID", "")
_SECRET_PARAM = os.environ.get("OPERATOR_CONTROL_SECRET_PARAM", "")

# Lazy-init boto3 clients so the pure-function tests run without boto3 installed.
_clients: dict[str, object] = {}


def _client(name: str, region: str = ""):
    key = name + (region or REGION)
    if key not in _clients:
        import boto3  # noqa: PLC0415

        _clients[key] = boto3.client(name, region_name=region or REGION)
    return _clients[key]


def _load_param(param: str) -> str:
    if not param:
        return ""
    try:
        return _client("ssm").get_parameter(Name=param, WithDecryption=True)["Parameter"]["Value"]
    except Exception:  # noqa: BLE001 — missing => deny / disable (fail closed)
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
    return _cached_param(_SECRET_PARAM)


# Destructive box actions blocked during market hours unless force=true.
# (wipe-groww removed 2026-07-16 — the groww-only wipe control retired with
# the live feeds; it rewrote only the frozen legacy ticks/candles_* tables.)
_DESTRUCTIVE = {"stop", "reboot", "restart-app", "stop-app", "wipe-questdb", "docker-reset", "docker-nuke-bare"}

# HARD LOCK — the DATA-DESTRUCTIVE subset of _DESTRUCTIVE: actions that DELETE
# market data which can NEVER be re-fetched from upstream (Dhan/Groww deliver
# live ticks once; a wiped window is gone forever). During market hours
# (09:15-15:30 IST Mon-Fri) these are REFUSED with 409 EVEN WHEN force=true —
# audit fix #2 (operator incident 2026-07-02 15:05 IST: a mid-market forced
# wipe-ALL + docker-reset deleted the QuestDB volume — ~4.5M rows wiped, 77s
# of feed darkness, upstream ticks unrecoverable). Lifecycle actions
# (stop / reboot / restart-app / stop-app) deliberately KEEP the force
# override above so emergencies stay possible.
_DATA_DESTRUCTIVE = {"wipe-questdb", "docker-reset", "docker-nuke-bare"}

_DATA_DESTRUCTIVE_LOCK_MSG = (
    "Data-destructive actions are locked during market hours "
    "(09:15-15:30 IST) — a mid-market wipe destroys data that can never "
    "be re-fetched. Run after 15:30."
)

_MKT_OPEN_SECS = 9 * 3600 + 15 * 60
_MKT_CLOSE_SECS = 15 * 3600 + 30 * 60
_IST_OFFSET_SECS = 19800  # +05:30

_VIEW_TIMEOUT_SECS = 6.0
_VIEW_POLL_SECS = 0.4
# The REST-era latency snapshot (2026-07-16) is one metrics scrape (3s) +
# one QuestDB round-trip probe (3s) + chronyc + one bounded rest_fetch_audit
# aggregate (4s). Worst case ≈ 10-11s incl. SSM registration; per-curl
# --max-time bounds every leg — a dead endpoint (or a slow QuestDB) can
# never hang the whole measure. (The old 28s budget covered the retired
# per-feed WS probe fan-out + the lag-percentile grid over `ticks`.)
_LATENCY_TIMEOUT_SECS = 15.0

# Read-only SQL gate: the first keyword must be one of these.
_SQL_ALLOWED_PREFIXES = ("select", "show", "explain", "with")
# ...and none of these mutating keywords may appear anywhere.
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
    # QuestDB-specific mutators (DB-console hardening 2026-07-02). Before this,
    # a ';'-chained second statement starting with an UNLISTED keyword — e.g.
    # "select 1; backup table ticks" — passed the gate (first-word check only
    # saw "select"; "backup" was not in the banned list). The gate now ALSO
    # rejects any ';' (single-statement rule), but these stay banned anywhere
    # as belt-and-braces.
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

# Server-side row cap for the read-only SQL console (both the Data-tab query
# box and the DB tab share the same `sql` action).
_SQL_MAX_ROWS = 1000

# B4 QuestDB console: TTL of the one-click link token minted by the
# `qdb_console_url` action. The console front Lambda verifies it (same
# control secret, HMAC over "qdblink|<exp>") and swaps it for a 12h session
# cookie — so the link in a browser history / Telegram scroll dies in 90s.
_QDB_LINK_TTL_SECS = 90


def _is_market_hours(now_utc: datetime.datetime) -> bool:
    """True during NSE market hours (Mon-Fri 09:15-15:30 IST)."""
    ist = now_utc + datetime.timedelta(seconds=_IST_OFFSET_SECS)
    if ist.weekday() >= 5:
        return False
    sod = ist.hour * 3600 + ist.minute * 60 + ist.second
    return _MKT_OPEN_SECS <= sod < _MKT_CLOSE_SECS


def _http_method(event: dict) -> str:
    try:
        return str(event["requestContext"]["http"]["method"]).upper()
    except (KeyError, TypeError):
        return str(event.get("httpMethod", "POST")).upper()


def _authorized(headers: dict) -> bool:
    """Constant-time bearer check. Header keys are lowercased by Function URL."""
    secret = _control_secret()
    if not secret:
        return False
    auth = (headers or {}).get("authorization", "")
    if not auth.startswith("Bearer "):
        return False
    return hmac.compare_digest(auth[len("Bearer ") :], secret)


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
    left as-is — the `/exp?limit=` param + `head` on the box remain the
    belt-and-braces output bound in every case. Pure function — unit-tested."""
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


def _mint_qdb_link_token(secret: str, now_epoch: int, ttl: int = _QDB_LINK_TTL_SECS) -> str:
    """One-click console link token: <exp>.<hexhmac(secret, "qdblink|<exp>")>.
    Verified constant-time by the questdb-console-front Lambda against the
    SAME SSM control secret — no second key to manage. Pure function."""
    import hashlib  # noqa: PLC0415

    exp = now_epoch + ttl
    sig = hmac.new(secret.encode(), f"qdblink|{exp}".encode(), hashlib.sha256).hexdigest()
    return f"{exp}.{sig}"


def _ssm_shell(commands: list[str]) -> str:
    resp = _client("ssm").send_command(
        InstanceIds=[INSTANCE_ID],
        DocumentName="AWS-RunShellScript",
        Parameters={"commands": commands},
    )
    return resp["Command"]["CommandId"]


def _ssm_shell_sync(commands: list[str], timeout: float = _VIEW_TIMEOUT_SECS) -> str:
    """Send a command and poll for output up to `timeout` seconds. Returns "" on
    any failure (e.g. the box is STOPPED — its SSM agent is offline, so
    send_command raises). Callers degrade to an empty snapshot, never a 500."""
    try:
        cid = _ssm_shell(commands)
    except Exception as exc:  # noqa: BLE001 — box stopped / SSM agent offline
        print(f"operator-portal ssm send_command failed (box offline?): {exc!r}")  # noqa: T201
        return ""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        time.sleep(_VIEW_POLL_SECS)
        try:
            inv = _client("ssm").get_command_invocation(CommandId=cid, InstanceId=INSTANCE_ID)
        except Exception:  # noqa: BLE001 — not registered yet
            continue
        if inv.get("Status") in ("Success", "Failed", "Cancelled", "TimedOut"):
            return (inv.get("StandardOutputContent", "") or "") + (
                inv.get("StandardErrorContent", "") or ""
            )
    return ""


# REST-era snapshot (2026-07-16 cleanup): the runtime is REST-only — every
# live-feed WebSocket is deleted (Dhan 2026-07-13, Groww 2026-07-15), so the
# old ticks/candles_* counters, the MAX_TPS pill, and the tick-conservation
# shield inputs all read frozen legacy tables / dead producers. The view now
# counts the LIVE tables the per-minute REST pulls write: spot_1m_rest,
# option_chain_1m, option_contract_1m_rest — totals + a per-feed split per
# table (/exp emits a CSV header + one row per feed — skip the header and
# ';'-join rows so the labeled-line parser sees ONE line, house convention).
_VIEW_COMMANDS = [
    "set +e",
    'echo "APP=$(systemctl is-active tickvault 2>/dev/null || echo inactive)"',
    "Q='http://127.0.0.1:9000/exp?query='",
    'echo "SPOT_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20spot_1m_rest%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "CHAIN_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20option_chain_1m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "CONTRACT_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20option_contract_1m_rest%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "SPOT_BY_FEED=$(curl -fsS "${Q}SELECT%20feed%2C%20count()%20FROM%20spot_1m_rest%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20feed" 2>/dev/null | tail -n +2 | tr \'\\n\' \';\')"',
    'echo "CHAIN_BY_FEED=$(curl -fsS "${Q}SELECT%20feed%2C%20count()%20FROM%20option_chain_1m%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20feed" 2>/dev/null | tail -n +2 | tr \'\\n\' \';\')"',
    'echo "CONTRACT_BY_FEED=$(curl -fsS "${Q}SELECT%20feed%2C%20count()%20FROM%20option_contract_1m_rest%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20feed" 2>/dev/null | tail -n +2 | tr \'\\n\' \';\')"',
    # NOTE: the `=` in `upsertKey=true` MUST be URL-encoded as %3D — it is the
    # ONLY view query carrying a raw `=` inside the ?query= value, which the
    # QuestDB /exp query-string parser mis-handled, returning empty so the
    # dashboard dedup panel showed "?". Encoded form = a clean `count` of the
    # 4 upsert-key columns — the REAL `spot_1m_rest` DEDUP key is
    # (ts, security_id, exchange_segment, feed) per DEDUP_KEY_SPOT_1M_REST in
    # crates/storage/src/spot_1m_rest_persistence.rs (designated ts is always
    # an upsertKey column).
    'echo "DEDUP_KEYS=$(curl -fsS "${Q}SELECT%20count()%20FROM%20table_columns(%27spot_1m_rest%27)%20WHERE%20upsertKey%3Dtrue" 2>/dev/null | tail -1)"',
    'echo "ERRORS_BEGIN"',
    "journalctl -u tickvault -p err -n 5 --no-pager 2>/dev/null | tail -5 || true",
    'echo "ERRORS_END"',
]


def _parse_feed_counts(raw: str) -> dict:
    """Parse a *_BY_FEED value — ';'-joined QuestDB CSV rows like
    '"dhan",123;"groww",45;' — into {feed: count_str}. Pure. Malformed
    fragments are skipped; an empty/absent value yields {} (the UI then shows
    nothing rather than fabricated zeros)."""
    out: dict[str, str] = {}
    for part in (raw or "").split(";"):
        part = part.strip()
        if not part or "," not in part:
            continue
        name, _, cnt = part.partition(",")
        name = name.strip().strip('"')
        cnt = cnt.strip().strip('"')
        if name and cnt:
            out[name] = cnt
    return out


def _sum_counts(vals: list) -> str:
    """Sum the parseable non-negative integer count strings; "" when NONE
    parsed (box stopped / QuestDB down) — the hero then shows nothing rather
    than a fabricated 0. Pure."""
    nums = [int(str(v).strip()) for v in vals if str(v or "").strip().isdigit()]
    return str(sum(nums)) if nums else ""


def _parse_view(stdout: str) -> dict:
    """Parse the labeled snapshot stdout into a structured dict (pure)."""
    fields: dict[str, str] = {}
    errors: list[str] = []
    in_errors = False
    for line in (stdout or "").splitlines():
        if line == "ERRORS_BEGIN":
            in_errors = True
            continue
        if line == "ERRORS_END":
            in_errors = False
            continue
        if in_errors:
            if line.strip():
                errors.append(line.rstrip())
            continue
        if "=" in line:
            key, _, val = line.partition("=")
            fields[key.strip()] = val.strip()
    spot = fields.get("SPOT_TODAY", "")
    chain = fields.get("CHAIN_TODAY", "")
    contracts = fields.get("CONTRACT_TODAY", "")
    return {
        "app": fields.get("APP", ""),
        # Official minute candles captured today by the REST pulls — the
        # three LIVE tables, per table + per feed. See _VIEW_COMMANDS.
        "rows_today": {"spot": spot, "chain": chain, "contracts": contracts},
        "rows_today_total": _sum_counts([spot, chain, contracts]),
        "rows_by_feed": {
            "spot": _parse_feed_counts(fields.get("SPOT_BY_FEED", "")),
            "chain": _parse_feed_counts(fields.get("CHAIN_BY_FEED", "")),
            "contracts": _parse_feed_counts(fields.get("CONTRACT_BY_FEED", "")),
        },
        "dedup_key_columns": fields.get("DEDUP_KEYS", ""),
        "recent_errors": errors,
    }


# ----------------------------------------------------------------- latency snap
# REST-era latency (2026-07-16 cleanup — every live-feed WebSocket is deleted,
# so the old per-feed WS TCP/TLS probes to api-feed.dhan.co /
# socket-api.groww.in, the exchange→received lag-percentile grid over `ticks`,
# and the tick-path histogram shields all measured retired edges / dead
# emitters):
#  * BOX-WIDE — local QuestDB round-trip (SELECT 1), wall-clock skew vs
#    chrony, and the dormant order-placement histogram average (KEPT — the
#    order path returns with live trading; it reads "—" until then).
#  * PER (feed, leg) — "how fast after each minute closed": today's ok-pull
#    count + p50/p99 of close_to_data_ms from rest_fetch_audit for
#    outcome='ok' rows, one row per (feed, leg) the fetch log carries
#    (spot_1m / chain_1m / contract_1m, both brokers — never hardcoded).
#    Percentiles are computed SERVER-SIDE with QuestDB's
#    approx_percentile(value, q, 3) — HdrHistogram, 3 significant figures,
#    non-negative input required, which the close_to_data_ms >= 0 filter
#    satisfies while ALSO excluding the -1 not-measured sentinel rows
#    (backfilled/swept minutes carry their real >=60s delay and stay in;
#    only unmeasured rows drop). This mirrors the 15:45 IST scorecard
#    digest language (dual-feed-scoreboard-error-codes.md §2b).
_REST_LAT_QUERY_MAX_SECS = 4


def _rest_latency_sql() -> str:
    """Today's per-(feed, leg) prompt-pull latency aggregate (pure).

    outcome='ok' rows only; `close_to_data_ms >= 0` excludes the -1
    not-measured sentinel AND satisfies approx_percentile's non-negative
    input requirement. `ts IN today()` matches the house convention every
    other view query uses on the IST-stored tables."""
    return (
        "select feed, leg, count() ok_rows, "
        "approx_percentile(close_to_data_ms, 0.5, 3) p50, "
        "approx_percentile(close_to_data_ms, 0.99, 3) p99 "
        "from rest_fetch_audit "
        "where ts in today() and outcome = 'ok' and close_to_data_ms >= 0 "
        "group by feed, leg order by feed, leg"
    )


def _latency_commands() -> list[str]:
    """Build the on-box latency snapshot script (pure — static inputs only).

    One metrics scrape (the dormant order-placement histogram only), the
    QuestDB round-trip probe, the chrony skew read, and ONE bounded
    rest_fetch_audit aggregate re-emitted as RESTLAT_ROW=<csv> labeled
    lines. Every curl is --max-time bounded — a dead endpoint can never
    hang the whole measure."""
    return [
        "set +e",
        'echo "METRICS_BEGIN"',
        "curl -fsS --max-time 3 http://127.0.0.1:9091/metrics 2>/dev/null | "
        "grep -E '^tv_order_placement_duration_ns' || echo none",
        'echo "METRICS_END"',
        "echo \"QDB=$(curl -o /dev/null -s -w '%{time_total}' --max-time 3 'http://127.0.0.1:9000/exec?query=SELECT%201' 2>/dev/null)\"",
        "echo \"SKEW=$(chronyc tracking 2>/dev/null | awk '/Last offset/{print $4}')\"",
        f"curl -fsS --max-time {_REST_LAT_QUERY_MAX_SECS} -G 'http://127.0.0.1:9000/exp' "
        f'--data-urlencode "query={_rest_latency_sql()}" 2>/dev/null '
        "| tail -n +2 | sed 's/^/RESTLAT_ROW=/'",
    ]


_LATENCY_COMMANDS = _latency_commands()


def _avg_ns(sum_v: str, count_v: str) -> float | None:
    """sum/count -> average nanoseconds, or None if no samples. Pure."""
    try:
        s = float(sum_v)
        c = float(count_v)
        return s / c if c > 0 else None
    except (TypeError, ValueError):
        return None


def _parse_rest_lat_row(raw: str) -> dict | None:
    """Parse one `<feed>,<leg>,<ok_rows>,<p50>,<p99>` CSV line (latency in
    MILLISECONDS — close_to_data_ms is stored in ms) into a dict, or None
    for anything malformed — a failed on-box query yields nothing, never a
    fabricated row. feed/leg tokens are charset-validated (defense in depth
    — they are our own symbols, but nothing unvalidated lands in HTML paths
    beyond esc()). Pure."""
    import re  # noqa: PLC0415

    parts = [p.strip().strip('"') for p in raw.split(",")]
    if len(parts) != 5:
        return None
    feed, leg = parts[0], parts[1]
    if not re.fullmatch(r"[a-z0-9_-]{1,32}", feed):
        return None
    if not re.fullmatch(r"[a-z0-9_-]{1,32}", leg):
        return None

    def _num(s: str) -> float | None:
        if not s or s.lower() in ("null", "nan"):
            return None
        try:
            return float(s)
        except ValueError:
            return None

    rows = _num(parts[2])
    if rows is None or rows < 0:
        return None
    p50, p99 = _num(parts[3]), _num(parts[4])
    return {
        "feed": feed,
        "leg": leg,
        "ok_rows": int(rows),
        "p50_ms": round(p50, 1) if p50 is not None else None,
        "p99_ms": round(p99, 1) if p99 is not None else None,
    }


def _parse_latency(stdout: str) -> dict:
    """Parse the labeled latency snapshot into a structured dict (pure).

    RESTLAT_ROW lines carry the per-(feed, leg) rest_fetch_audit aggregate;
    the METRICS block carries the dormant order-placement histogram. Empty
    stdout (box stopped) degrades to an empty table + blank shields — the
    page says so honestly instead of fabricating numbers."""
    metrics: dict[str, str] = {}
    rest_rows: list = []
    fields: dict[str, str] = {}
    mode = ""
    for line in (stdout or "").splitlines():
        if line.startswith("RESTLAT_ROW="):
            row = _parse_rest_lat_row(line[len("RESTLAT_ROW=") :])
            if row is not None:
                rest_rows.append(row)
            continue
        if line == "METRICS_BEGIN":
            mode = "METRICS"
            continue
        if line == "METRICS_END":
            mode = ""
            continue
        if mode == "METRICS":
            bits = line.split()
            if len(bits) == 2:
                metrics[bits[0]] = bits[1]
            continue
        if "=" in line:
            k, _, v = line.partition("=")
            fields[k.strip()] = v.strip()

    def sec_to_ms(s: str) -> str:
        try:
            return f"{float(s) * 1000:.1f}"
        except (TypeError, ValueError):
            return ""

    order_avg = _avg_ns(
        metrics.get("tv_order_placement_duration_ns_sum", ""),
        metrics.get("tv_order_placement_duration_ns_count", ""),
    )
    return {
        # Per-(feed, leg) "how fast after each minute closed" rows. Empty
        # list = the box returned no rows (stopped box, empty fetch log, or
        # query failed) — the page says so honestly.
        "rest_latency": rest_rows,
        "questdb_ms": sec_to_ms(fields.get("QDB", "")),
        "clock_skew_ms": sec_to_ms(fields.get("SKEW", "")),
        "order_place_avg_ns": round(order_avg, 1) if order_avg is not None else None,
    }


# ----------------------------------------------------------------- storage snap
# Disk used/free on the EBS root + QuestDB on-disk size. `df -BG` prints whole
# GB. The QuestDB data lives in the named docker volume on the root EBS.
_STORAGE_COMMANDS = [
    "set +e",
    "df -BG / | tail -1 | awk '{print \"DISK_USED=\"$3\"\\nDISK_FREE=\"$4\"\\nDISK_PCT=\"$5}'",
    "echo \"DB_SIZE=$(du -sBG /var/lib/docker/volumes/tv-questdb-data/_data 2>/dev/null | cut -f1)\"",
]


def _parse_storage(stdout: str) -> dict:
    """Parse df/du output into disk_used/free/pct + db_size (pure)."""
    f: dict[str, str] = {}
    for line in (stdout or "").splitlines():
        if "=" in line:
            k, _, v = line.partition("=")
            f[k.strip()] = v.strip()

    def g(k: str) -> str:
        return f.get(k, "").replace("G", "").strip()

    return {
        "disk_used_gb": g("DISK_USED"),
        "disk_free_gb": g("DISK_FREE"),
        "disk_pct": f.get("DISK_PCT", ""),
        "db_size_gb": g("DB_SIZE"),
    }


# ------------------------------------------------------------------- feeds card
# Feed enable/disable from the portal (the app's :3001 is closed in the SG —
# reachable ONLY via SSM RunShellScript curling 127.0.0.1, same mechanism as
# the cross-verify card above). Contract read from crates/api (2026-07-02):
#   GET  /api/feeds        -> {"dhan_enabled": bool, "groww_enabled": bool,
#                              "dhan_lane_running": bool, "groww_lane_running": bool}
#   GET  /api/feeds/health -> {"market_open": bool, "feeds": [{"feed", "verdict",
#                              "reason", "enabled", "lane_running", "connected",
#                              "ticks_total", "subscribed_total", ...}]}
#   POST /api/feeds/{feed} body {"enabled": bool} -> 200 same status JSON;
#        400 {"error", "allowed"} unknown feed; 409 {"error", "allowed"} =
#        the Dhan-disable safety guard (live trading active). Tokenless when
#        the app runs dry_run=true (feed_toggle_public); pass-through verbatim.
#
# FUTURE-FEEDS: the portal card iterates the health response's `feeds` array
# (Feed::ALL-driven in the app), so a feed #3/#4 added to the app auto-appears
# with its own toggle — zero portal changes. The Lambda therefore validates the
# feed NAME by strict charset (injection-safe) and lets the app's own 400
# "unknown feed" response be the authority, passed through verbatim.
_FEED_API_UNREACHABLE = (
    "app API unreachable on the box (curl to 127.0.0.1:3001 failed — is the app running?)"
)
_BOX_UNREACHABLE = "box unreachable (SSM offline or instance stopped)"

_FEEDS_VIEW_COMMANDS = [
    "set +e",
    'echo "FEEDS_BEGIN"',
    # `; echo` guarantees the end-marker lands on its own line even when the
    # JSON body has no trailing newline. TV_CURL_FAILED = truthful sentinel —
    # the parser reports an error, never fake zeros/disabled.
    "curl -fsS --max-time 8 http://127.0.0.1:3001/api/feeds 2>/dev/null || echo TV_CURL_FAILED; echo",
    'echo "FEEDS_END"',
    'echo "FEEDS_HEALTH_BEGIN"',
    "curl -fsS --max-time 8 http://127.0.0.1:3001/api/feeds/health 2>/dev/null || echo TV_CURL_FAILED; echo",
    'echo "FEEDS_HEALTH_END"',
    # REST-lane pulse per feed (2026-07-16 — the runtime is REST-only; the
    # old ticks/subscribed counters read frozen registries). Sourced from
    # rest_fetch_audit (LIVE — written by every feed/leg pair):
    #  * REST_AUDIT — today's fetch outcomes per (feed, outcome), CSV header
    #    skipped, rows ';'-joined (the *_BY_FEED house convention).
    #  * REST_LAT_HOUR — last hour's p50/p99 of close_to_data_ms for
    #    outcome='ok' rows per feed. `ts` is IST-shifted while QuestDB now()
    #    is UTC, so the window compares against IST_now =
    #    dateadd('m', 330, now()) — the 2026-07-07 timebase lesson. The
    #    close_to_data_ms >= 0 filter drops the -1 not-measured sentinel and
    #    satisfies approx_percentile's non-negative input requirement.
    (
        'echo "REST_AUDIT=$(curl -fsS --max-time 4 -G \'http://127.0.0.1:9000/exp\' '
        '--data-urlencode "query=select feed, outcome, count() from rest_fetch_audit '
        'where ts in today() group by feed, outcome" 2>/dev/null '
        "| tail -n +2 | tr '\\n' ';')\""
    ),
    (
        'echo "REST_LAT_HOUR=$(curl -fsS --max-time 4 -G \'http://127.0.0.1:9000/exp\' '
        '--data-urlencode "query=select feed, approx_percentile(close_to_data_ms, 0.5, 3), '
        "approx_percentile(close_to_data_ms, 0.99, 3) from rest_fetch_audit "
        "where ts > dateadd('h', -1, dateadd('m', 330, now())) "
        "and outcome = 'ok' and close_to_data_ms >= 0 group by feed\" 2>/dev/null "
        "| tail -n +2 | tr '\\n' ';')\""
    ),
]
# Two 8s curls + two 4s audit reads + SSM registration need more than the 6s
# view window.
_FEEDS_TIMEOUT_SECS = 20.0


def _extract_marked_json(stdout: str, begin: str, end: str) -> tuple[object, str]:
    """Extract + parse the JSON between two marker lines. Returns
    (parsed_or_None, error_str). Pure; never fabricates a payload."""
    raw = ""
    if begin in stdout and end in stdout:
        raw = stdout.split(begin, 1)[1].split(end, 1)[0].strip()
    if not raw or "TV_CURL_FAILED" in raw:
        return None, _FEED_API_UNREACHABLE
    try:
        return json.loads(raw), ""
    except (json.JSONDecodeError, ValueError):
        return None, "app API returned invalid JSON"


def _parse_rest_audit(raw: str) -> dict:
    """Parse the REST_AUDIT value — ';'-joined `feed,outcome,count` CSV rows —
    into {feed: {outcome: count_int}}. Pure. Feed/outcome tokens are
    charset-validated (defense in depth); malformed fragments are skipped so
    an empty/absent value yields {} — the card then says "no pulls recorded
    today" instead of fabricated zeros (audit Rule 11)."""
    import re  # noqa: PLC0415

    out: dict[str, dict[str, int]] = {}
    for part in (raw or "").split(";"):
        parts = [p.strip().strip('"') for p in part.strip().split(",")]
        if len(parts) != 3:
            continue
        feed, outcome, cnt = parts
        if not re.fullmatch(r"[a-z0-9_-]{1,32}", feed):
            continue
        if not re.fullmatch(r"[a-z0-9_-]{1,32}", outcome):
            continue
        if not cnt.isdigit():
            continue
        out.setdefault(feed, {})[outcome] = int(cnt)
    return out


def _parse_rest_lat_hour(raw: str) -> dict:
    """Parse the REST_LAT_HOUR value — ';'-joined `feed,p50,p99` CSV rows
    (milliseconds — close_to_data_ms) — into {feed: {p50_ms, p99_ms}}.
    Pure; malformed fragments skipped, never fabricated."""
    import re  # noqa: PLC0415

    def _num(s: str) -> float | None:
        if not s or s.lower() in ("null", "nan"):
            return None
        try:
            return float(s)
        except ValueError:
            return None

    out: dict[str, dict] = {}
    for part in (raw or "").split(";"):
        parts = [p.strip().strip('"') for p in part.strip().split(",")]
        if len(parts) != 3:
            continue
        feed = parts[0]
        if not re.fullmatch(r"[a-z0-9_-]{1,32}", feed):
            continue
        p50, p99 = _num(parts[1]), _num(parts[2])
        if p50 is None and p99 is None:
            continue
        out[feed] = {
            "p50_ms": round(p50, 1) if p50 is not None else None,
            "p99_ms": round(p99, 1) if p99 is not None else None,
        }
    return out


def _parse_feeds_view(stdout: str) -> dict:
    """Parse the marked feeds-view snapshot (pure). On any failure the matching
    *_error field carries a structured reason and the payload is None — the UI
    renders the error verbatim instead of defaulting to zeros. The REST-lane
    pulse (rest_audit / rest_lat_hour, sourced from rest_fetch_audit) rides
    the same snapshot as labeled lines OUTSIDE the marker blocks."""
    if not (stdout or "").strip():
        return {
            "feeds": None,
            "feeds_error": _BOX_UNREACHABLE,
            "health": None,
            "health_error": _BOX_UNREACHABLE,
            "rest_audit": {},
            "rest_lat_hour": {},
        }
    feeds, feeds_error = _extract_marked_json(stdout, "FEEDS_BEGIN", "FEEDS_END")
    health, health_error = _extract_marked_json(stdout, "FEEDS_HEALTH_BEGIN", "FEEDS_HEALTH_END")
    fields: dict[str, str] = {}
    in_block = False
    for line in (stdout or "").splitlines():
        # Skip the marker-delimited JSON blocks — a JSON body containing '='
        # must never be mistaken for a labeled line.
        if line in ("FEEDS_BEGIN", "FEEDS_HEALTH_BEGIN"):
            in_block = True
            continue
        if line in ("FEEDS_END", "FEEDS_HEALTH_END"):
            in_block = False
            continue
        if in_block:
            continue
        if "=" in line:
            k, _, v = line.partition("=")
            fields[k.strip()] = v.strip()
    return {
        "feeds": feeds,
        "feeds_error": feeds_error,
        "health": health,
        "health_error": health_error,
        "rest_audit": _parse_rest_audit(fields.get("REST_AUDIT", "")),
        "rest_lat_hour": _parse_rest_lat_hour(fields.get("REST_LAT_HOUR", "")),
    }


def _validate_feed_toggle(feed: object, enabled: object) -> str:
    """Strict validation for the feed-toggle action (pure). Returns "" when
    valid, else the rejection reason. The feed name is charset-locked
    (lowercase token — injection-safe for the shell/URL interpolation) but NOT
    allowlisted, so a future feed #3 toggles with zero Lambda changes; the app
    itself 400s unknown names (passed through verbatim). `enabled` must be a
    JSON boolean — 1/"true"/None are rejected so nothing ambiguous ever
    reaches the shell."""
    import re  # noqa: PLC0415

    if not isinstance(feed, str) or not re.fullmatch(r"[a-z][a-z0-9_-]{0,31}", feed):
        return "feed must be a lowercase feed name like 'dhan' or 'groww'"
    if not isinstance(enabled, bool):
        return "enabled must be a JSON boolean (true or false)"
    return ""


def _feed_toggle_commands(feed: str, enabled: bool) -> list[str]:
    """Build the SSM curl for POST /api/feeds/{feed}. Only called AFTER
    _validate_feed_toggle passed, so `feed` is a charset-locked lowercase
    token and `enabled` is a real bool — the interpolations cannot carry
    shell metacharacters."""
    body = json.dumps({"enabled": enabled})
    return [
        "set +e",
        # -w %{http_code} captures the app's status; the body goes to a temp
        # file so status + body are cleanly separable in the labeled output.
        'echo "FEED_TOGGLE_STATUS=$(curl -sS --max-time 8 -o /tmp/tv-feed-toggle.json '
        f"-w %{{http_code}} -X POST -H 'content-type: application/json' -d '{body}' "
        f'http://127.0.0.1:3001/api/feeds/{feed} 2>/dev/null)"',
        'echo "FEED_TOGGLE_BODY_BEGIN"',
        "cat /tmp/tv-feed-toggle.json 2>/dev/null; echo",
        'echo "FEED_TOGGLE_BODY_END"',
        "rm -f /tmp/tv-feed-toggle.json",
    ]


def _parse_feed_toggle(stdout: str) -> dict:
    """Parse the toggle result (pure): the app's HTTP status + its JSON body
    passed through VERBATIM (incl. the 409 Dhan-guard message). curl prints
    000 when it could not connect — reported as unreachable, never success."""
    if not (stdout or "").strip():
        return {"app_status": None, "app_response": None, "error": _BOX_UNREACHABLE}
    status = None
    for line in stdout.splitlines():
        if line.startswith("FEED_TOGGLE_STATUS="):
            raw = line[len("FEED_TOGGLE_STATUS=") :].strip()
            if raw.isdigit():
                status = int(raw)
    body_raw = ""
    if "FEED_TOGGLE_BODY_BEGIN" in stdout and "FEED_TOGGLE_BODY_END" in stdout:
        body_raw = stdout.split("FEED_TOGGLE_BODY_BEGIN", 1)[1].split("FEED_TOGGLE_BODY_END", 1)[0].strip()
    body: object = None
    if body_raw:
        try:
            body = json.loads(body_raw)
        except (json.JSONDecodeError, ValueError):
            body = {"raw": body_raw[:500]}
    if status is None or status == 0:
        return {"app_status": None, "app_response": body, "error": _FEED_API_UNREACHABLE}
    return {"app_status": status, "app_response": body, "error": ""}


# --------------------------------------------------------- deploy provenance
# B9 deploy provenance: the GET page footer shows `binary <7> · portal <7> ·
# main <7>` so drift between the deployed binary, this Lambda's code tree,
# and main HEAD is visible at a glance. HARD RULE: the GET must NEVER fail
# or hang because of provenance — every lookup is individually fail-soft to
# "unknown", the GitHub call is bounded to 3s and cached 60s module-globally.

_BINARY_SHA_PARAM = os.environ.get("BINARY_SHA_PARAM", "/tickvault/prod/deploy/binary-git-sha")
_PROVENANCE_GH_REPO = os.environ.get("GH_REPO", "SJParthi/tickvault")
_PROVENANCE_GH_TOKEN_PARAM = os.environ.get("OPERATOR_GITHUB_TOKEN_PARAM", "")

_MAIN_SHA_TTL_SECS = 60.0
# 2026-07-03 hardening (adversarial-review LOW): on sustained GitHub failure
# the cached main-HEAD value must not be served forever — past this hard
# max-age a failed refresh degrades to "unknown" instead of a stale sha.
_MAIN_SHA_MAX_AGE_SECS = 600.0
_main_sha_cache: dict = {"value": "", "ts": 0.0}


def _binary_sha() -> str:
    """Git SHA of the last successfully deployed binary — SSM param written
    by deploy-aws.yml after a verified swap. Fail-soft to "unknown"."""
    try:
        return _cached_param(_BINARY_SHA_PARAM).strip() or "unknown"
    except Exception:  # noqa: BLE001 — provenance must never break the page
        return "unknown"


def _portal_sha() -> str:
    """Git SHA of the repo tree this Lambda zip was applied from — injected
    by terraform (var.portal_git_sha, set by CI TF_VAR_portal_git_sha)."""
    return os.environ.get("PORTAL_GIT_SHA", "").strip() or "unknown"


def _main_sha() -> str:
    """HEAD sha of main via the GitHub API (existing operator token), with a
    60s module-global cache so the page adds at most one 3s-bounded GitHub
    round-trip per minute. Fail-soft to the cached value while it is younger
    than the 600s hard max-age, else "unknown" (never an unboundedly stale
    sha)."""
    now = time.monotonic()
    if _main_sha_cache["value"] and (now - _main_sha_cache["ts"]) <= _MAIN_SHA_TTL_SECS:
        return _main_sha_cache["value"]
    try:
        import urllib.request  # noqa: PLC0415 — lazy, matches file style

        req = urllib.request.Request(
            f"https://api.github.com/repos/{_PROVENANCE_GH_REPO}/commits/main",
            method="GET",
        )
        token = _cached_param(_PROVENANCE_GH_TOKEN_PARAM)
        if token:
            req.add_header("authorization", "Bearer " + token)
        req.add_header("accept", "application/vnd.github+json")
        req.add_header("user-agent", "tickvault-operator-portal")
        with urllib.request.urlopen(req, timeout=3) as r:  # noqa: S310 — fixed host
            sha = str(json.loads(r.read().decode() or "{}").get("sha") or "").strip()
        if sha:
            _main_sha_cache["value"] = sha
            _main_sha_cache["ts"] = now
            return sha
    except Exception:  # noqa: BLE001 — provenance must never break the page
        pass
    # Refresh failed: serve the cached value only while it is younger than
    # the 600s hard max-age; a sustained GitHub outage degrades to "unknown"
    # instead of an unboundedly stale sha (2026-07-03 adversarial-review LOW).
    if _main_sha_cache["value"] and (now - _main_sha_cache["ts"]) <= _MAIN_SHA_MAX_AGE_SECS:
        return _main_sha_cache["value"]
    return "unknown"


def _safe_provenance_sha(sha: object) -> str:
    """Hex-validate one provenance sha (mirrors the Rust
    is_full_lower_hex_sha guard, widened to 7-40 for short values): anything
    that is not 7-40 lowercase-hex becomes "unknown", so a poisoned SSM
    param / GitHub response can never smuggle markup into the footer
    (2026-07-03 adversarial-review MEDIUM fix)."""
    import re  # noqa: PLC0415

    if isinstance(sha, str) and re.fullmatch(r"[0-9a-f]{7,40}", sha):
        return sha
    return "unknown"


def _provenance_line(binary_sha: str, portal_sha: str, main_sha: str) -> str:
    """Pure formatter (unit-tested): short-7 provenance triple. Every input
    is hex-validated via _safe_provenance_sha before use."""
    b = _safe_provenance_sha(binary_sha)
    p = _safe_provenance_sha(portal_sha)
    m = _safe_provenance_sha(main_sha)
    return f"binary {b[:7]} · portal {p[:7]} · main {m[:7]}"


def _provenance_footer_html() -> str:
    # Defense in depth: the line is already hex-validated per sha, and the
    # assembled string is ALSO html-escaped before splicing into the page.
    import html as _html  # noqa: PLC0415

    line = _html.escape(_provenance_line(_binary_sha(), _portal_sha(), _main_sha()))
    return (
        '<footer style="margin-top:26px;text-align:center;font-size:11px;'
        'color:var(--mut);opacity:.75">' + line + "</footer>"
    )


# --------------------------------------------------------------------- response
def _resp(status: int, body: dict) -> dict:
    return {"statusCode": status, "headers": {"content-type": "application/json"}, "body": json.dumps(body)}


def _html_resp() -> dict:
    # B9 deploy provenance: inject the footer just before </body> so the
    # giant static console template stays untouched. Fail-soft: if the
    # footer render itself blows up, serve the page WITHOUT it.
    html = _console_html()
    try:
        html = html.replace("</body>", _provenance_footer_html() + "\n</body>", 1)
    except Exception:  # noqa: BLE001 — provenance must never break the page
        pass
    return {
        "statusCode": 200,
        "headers": {
            "content-type": "text/html; charset=utf-8",
            "cache-control": "no-store",
            "referrer-policy": "no-referrer",
        },
        "body": html,
    }


def lambda_handler(event, _context):
    if _http_method(event) == "GET":
        return _html_resp()

    headers = event.get("headers") or {}
    if not _authorized(headers):
        return _resp(401, {"error": "unauthorized"})

    raw = event.get("body") or "{}"
    try:
        payload = raw if isinstance(raw, dict) else json.loads(raw)
    except json.JSONDecodeError:
        return _resp(400, {"error": "invalid JSON body"})

    action = str(payload.get("action", "")).strip()
    force = bool(payload.get("force", False))

    # HARD gate first: data-destructive actions have NO force escape during
    # market hours (audit fix #2 — see _DATA_DESTRUCTIVE above). Must run
    # BEFORE the soft gate below because _DATA_DESTRUCTIVE ⊂ _DESTRUCTIVE.
    if action in _DATA_DESTRUCTIVE and _is_market_hours(datetime.datetime.utcnow()):
        return _resp(
            409,
            {"error": _DATA_DESTRUCTIVE_LOCK_MSG, "action": action, "market_hours_locked": True},
        )

    if action in _DESTRUCTIVE and _is_market_hours(datetime.datetime.utcnow()) and not force:
        return _resp(
            409,
            {"error": 'blocked during market hours (09:15-15:30 IST). Re-send with {"force": true} to override.', "action": action},
        )

    try:
        # ---- box control ----
        if action == "start":
            _client("ec2").start_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action})
        if action == "stop":
            _client("ec2").stop_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action})
        if action == "reboot":
            _client("ec2").reboot_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action})
        if action == "restart-app":
            return _resp(200, {"ok": True, "action": action, "command_id": _ssm_shell(["systemctl restart tickvault"])})
        if action == "stop-app":
            cid = _ssm_shell(["systemctl stop tickvault || true", "systemctl disable tickvault || true"])
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
        if action == "restart-questdb":
            # ensure-questdb.sh (create-or-restart), robust to docker-compose
            # v1/v2 absence + the CORRECT service name (tv-questdb) + SSM creds
            # for the recreate case. The old `docker compose up -d questdb` had
            # all three bugs (incident 2026-06-08) and silently no-op'd, leaving
            # QuestDB down + the app crash-looping on BOOT-02. The helper handles
            # running/stopped/removed and falls back to `docker run`.
            # (Does NOT fix QuestDB CRASHING on start — disk-full / volume
            # corruption: see docs/runbooks/aws-docker-daemon-dead.md.)
            cid = _ssm_shell(["bash /opt/tickvault/repo/scripts/ensure-questdb.sh"])
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
        if action == "command-status":
            # READ-ONLY (not in _DESTRUCTIVE, allowed off-hours, no force). Lets
            # the UI poll the REAL outcome of an async SSM command (e.g. the
            # docker-reset nuke, which runs for minutes) instead of showing a
            # fake "done" the instant it is dispatched. Returns the SSM status +
            # a tail of stdout/stderr so the UI can tell "docker-reset-dispatched"
            # (success) from "DOCKER-RESET-FAILED" (the hard-gate: volume still
            # in-use → data NOT wiped) — no more false-OK.
            cid = str(payload.get("command_id", "")).strip()
            if not cid:
                return _resp(400, {"error": "command-status requires command_id", "action": action})
            try:
                inv = _client("ssm").get_command_invocation(CommandId=cid, InstanceId=INSTANCE_ID)
            except Exception:  # noqa: BLE001 — not registered yet / box offline
                # Not an error: the command may not have propagated to SSM yet.
                return _resp(200, {"ok": True, "action": action, "status": "Pending", "stdout_tail": ""})
            out = (inv.get("StandardOutputContent", "") or "") + (inv.get("StandardErrorContent", "") or "")
            return _resp(
                200,
                {"ok": True, "action": action, "status": inv.get("Status", ""), "stdout_tail": out[-1500:]},
            )
        if action == "wipe-questdb":
            # DESTRUCTIVE: empties the market-data tables + EVERY feed's
            # capture/replay files for a fresh start. Requires force=true (even
            # off-hours) + is in _DESTRUCTIVE (market-hours-blocked).
            #
            # FEED-AGNOSTIC RESURRECT-PROOF REWRITE (operator incident
            # 2026-07-02: "wipe removed only Dhan data, Groww came back"):
            #   1. The old code TRUNCATEd a hardcoded list of just 6 tables —
            #      but 27 candles_* tables exist (1s..7d), so most candles
            #      SURVIVED for BOTH feeds. Now the table list is discovered
            #      DYNAMICALLY from QuestDB tables() at wipe time (ticks +
            #      every candles_* — any future timeframe/table is wiped
            #      automatically, nothing hardcoded to rot).
            #   2. The Groww sidecar's capture file (data/groww/
            #      live-ticks.ndjson) is a durable REPLAY SOURCE the DB wipe
            #      cannot see — the bridge re-tails it from byte 0 on restart
            #      and RESURRECTS every Groww row. Same class: the Dhan WAL
            #      (data/ws_wal) replays residual frames, and spill/dlq
            #      re-drain. ALL feed capture/replay dirs are now removed —
            #      any FUTURE feed's capture dir under data/ is caught by the
            #      generic sweep, so every feed (present and future) wipes
            #      IDENTICALLY.
            #   3. The app is STOPPED first (releases writers + drops
            #      in-memory bars so nothing re-seals stale candles) and
            #      restarted after — it recreates schemas + resumes live-only.
            #   4. Honest completion: post-wipe row counts are echoed; the
            #      marker line WIPE-COMPLETE only prints when ticks AND
            #      candles_1m AND spot_1m_rest are all 0 — a partial wipe
            #      reads WIPE-PARTIAL, never a fake OK.
            # SEBI audit tables are intentionally preserved — the dynamic list
            # matches ONLY ticks + candles_* + prev_day_ohlcv + the four LIVE
            # REST tables (rest_fetch_audit is per-fetch forensics, not a SEBI
            # never-delete table).
            if not force:
                return _resp(409, {"error": 'wipe is destructive — re-send with {"force": true}', "action": action})
            # PR-5 H-1 (2026-07-02 security MEDIUM): server-side token — a
            # scripted call with a stolen bearer can no longer fire this; the
            # word the portal already makes the operator TYPE is now VERIFIED
            # here (mirror of the wipe-groww gate).
            if str(payload.get("confirm", "")).strip() != "WIPE":
                return _resp(
                    409,
                    {"error": 'wipe-questdb deletes every tick and candle — re-send with {"confirm": "WIPE"}', "action": action},
                )
            data_dir = "/opt/tickvault/data"
            cmds = [
                "set +e",
                # 1. stop the app: releases QuestDB writers + drops in-memory
                #    bars + stops the Groww bridge/sidecar so nothing re-appends
                #    the capture file mid-wipe. PR-5 H-2 (hostile F5): ALSO
                #    disable the unit for the wipe window — a deploy watchdog /
                #    autopilot / concurrent-deploy `systemctl start` mid-wipe
                #    was the resurrection race (booting bridge re-tails the
                #    capture file). Re-enabled just before the final start.
                "systemctl stop tickvault || true",
                "systemctl disable tickvault || true",
                # 2. PR-5 H-2 REORDER: remove EVERY feed's capture/replay
                #    sources FIRST (was after TRUNCATE) — even a forced start
                #    mid-window now finds NOTHING to replay: Dhan WAL, Groww
                #    capture, spill, dlq, caches + a GENERIC sweep of any
                #    future feed's capture file under data/*/ . Logs KEPT.
                f"rm -rf {data_dir}/ws_wal {data_dir}/groww {data_dir}/spill {data_dir}/dlq {data_dir}/instrument-cache 2>/dev/null || true",
                f"rm -f {data_dir}/*/live-ticks.ndjson {data_dir}/*/*-status.json 2>/dev/null || true",
                "echo 'OK: feed capture/replay sources removed (ws_wal, groww, spill, dlq, instrument-cache)'",
                # 3. discover + truncate EVERY market-data table dynamically
                #    (ticks + all candles_* — 27 today, future ones included —
                #    + prev_day_ohlcv, PR-5: a \"fresh start\" previously left
                #    yesterday's reference rows; + the four LIVE REST-era
                #    tables the per-minute pulls write — spot_1m_rest,
                #    option_chain_1m, option_contract_1m_rest, rest_fetch_audit
                #    (2026-07-16: a \"fresh start\" that leaves today's official
                #    minute candles behind is not fresh; rest_fetch_audit is
                #    per-fetch forensics, NOT a SEBI never-delete table).
                #    python3 is on the box.
                (
                    "python3 - <<'PYWIPE'\n"
                    "import json, urllib.request, urllib.parse\n"
                    "base = 'http://127.0.0.1:9000/exec?query='\n"
                    "q = urllib.parse.quote(\"SELECT table_name FROM tables()\")\n"
                    "rows = json.load(urllib.request.urlopen(base + q, timeout=15)).get('dataset', [])\n"
                    "names = [r[0] for r in rows if isinstance(r, list) and r]\n"
                    "live_rest = {'spot_1m_rest', 'option_chain_1m', 'option_contract_1m_rest', 'rest_fetch_audit'}\n"
                    "targets = [t for t in names if t == 'ticks' or t.startswith('candles_') or t == 'prev_day_ohlcv' or t in live_rest]\n"
                    "print('WIPE-TARGETS', len(targets), sorted(targets))\n"
                    "for t in targets:\n"
                    "    tq = urllib.parse.quote(f'TRUNCATE TABLE {t}')\n"
                    "    try:\n"
                    "        urllib.request.urlopen(base + tq, timeout=30).read()\n"
                    "        print('TRUNCATED', t)\n"
                    "    except Exception as exc:\n"
                    "        print('TRUNCATE-FAILED', t, exc)\n"
                    "PYWIPE"
                ),
                # 4. re-enable + restart the app — recreates schemas, LIVE-only.
                "systemctl enable tickvault || true",
                "systemctl start tickvault || true",
                # 5. honest verification: both counts must be 0 (QuestDB may
                #    need a moment; count right after truncate, before live
                #    ticks resume mid-market is inherently racy off-hours only —
                #    wipe is market-hours-blocked, so 0 is the honest baseline).
                (
                    "sleep 3; "
                    "T=$(curl -fsS 'http://127.0.0.1:9000/exec?query=SELECT%20count()%20FROM%20ticks' 2>/dev/null | grep -o '\\[\\[[0-9]*' | grep -o '[0-9]*'); "
                    "C=$(curl -fsS 'http://127.0.0.1:9000/exec?query=SELECT%20count()%20FROM%20candles_1m' 2>/dev/null | grep -o '\\[\\[[0-9]*' | grep -o '[0-9]*'); "
                    "S=$(curl -fsS 'http://127.0.0.1:9000/exec?query=SELECT%20count()%20FROM%20spot_1m_rest' 2>/dev/null | grep -o '\\[\\[[0-9]*' | grep -o '[0-9]*'); "
                    "echo \"WIPE-RESULT ticks=${T:-?} candles_1m=${C:-?} spot_1m_rest=${S:-?}\"; "
                    "if [ \"${T:-1}\" = 0 ] && [ \"${C:-1}\" = 0 ] && [ \"${S:-1}\" = 0 ]; then echo WIPE-COMPLETE; else echo 'WIPE-PARTIAL: rows remain (or count unavailable) — inspect above'; fi"
                ),
            ]
            cid = _ssm_shell(cmds)
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
        # (wipe-groww removed 2026-07-16 — the groww-only wipe retired with the
        # Groww live feed; a POST now falls through to the unknown-action 400.)
        if action == "docker-reset":
            # MOST DESTRUCTIVE: the FULL DOCKER NUKE — full Docker teardown +
            # fresh rebuild (operator Option B, 2026-06-04; re-demanded verbatim
            # 2026-07-03: "if I want to do the entire docker wipe off I mean
            # entire docker nuke"). Unlike wipe-questdb (TRUNCATE rows, keeps
            # audit tables for SEBI), this DELETES the Docker volumes + images
            # entirely, so EVERY table — including the SEBI-retention audit
            # tables — is gone. The operator explicitly chose this; the UI
            # asks the operator to TYPE the confirm phrase and spells out the
            # audit-data loss.
            #
            # Server-side guards (mirror the #1325 wipe-groww pattern):
            #   1. typed confirm token — the caller must send
            #      {"confirm": "NUKE-DOCKER"} (typed by the operator in the UI
            #      prompt; a scripted call without the token can never fire
            #      this accidentally — force alone is NOT enough).
            #   2. market-hours guard — in _DESTRUCTIVE, so blocked
            #      09:15-15:30 IST Mon-Fri unless {"force": true}.
            #   3. audit trail — the dispatch is print-logged to the Lambda's
            #      CloudWatch log group (who/when/command_id) and the full
            #      command transcript lives in SSM command history.
            #
            # Sequence (each step fail-soft so one hiccup can't wedge the box):
            #   1. stop the app (systemd) so it releases the QuestDB connection
            #      and the volume (an in-use volume is exactly why `volume rm`
            #      silently failed before).
            #   2. remove EVERY container that mounts `tv-questdb-data` — by
            #      VOLUME, not by name — so QuestDB is caught even when it runs
            #      under a different container name / compose project (the
            #      documented "outside the compose project" case). THIS is the
            #      fix for "the nuke didn't wipe the data": the old code removed
            #      only the literal name `tv-questdb`, so an off-project QuestDB
            #      kept the volume in-use and `volume rm` no-op'd under `|| true`.
            #   3. compose down -v + `docker system prune -af --volumes` — drop
            #      remaining containers, named volumes, and images.
            #   4. HARD GATE: if `tv-questdb-data` still exists it is still
            #      in-use → DO NOT `up` (that would re-attach the SAME old
            #      ticks — the silent-survival bug). Fail LOUD + `exit 1` so the
            #      operator sees the nuke did not complete, instead of a fake OK.
            #   5. wipe the HOST app caches too (instrument-cache, spill, dlq)
            #      under /opt/tickvault/data — these are host dirs the Docker
            #      nuke cannot see, so "reuse the existing list" survived every
            #      reset. Logs are preserved for forensics.
            #   6. `docker compose up -d` on the now-empty volume + restart app —
            #      it recreates `ticks` + candle + audit tables fresh via
            #      `ensure_*_table_dedup_keys` with the correct DEDUP keys.
            if str(payload.get("confirm", "")).strip() != "NUKE-DOCKER":
                return _resp(
                    409,
                    {
                        "error": 'docker-reset is the FULL DOCKER NUKE (deletes containers + volumes + images = ALL data incl. SEBI audit tables, then fresh start) — re-send with {"confirm": "NUKE-DOCKER"}',
                        "action": action,
                    },
                )
            compose_dir = "/opt/tickvault/repo/deploy/docker"
            data_dir = "/opt/tickvault/data"
            cmds = [
                "set +e",
                # 1. stop the app so it releases the QuestDB connection + volume
                "systemctl stop tickvault || true",
                # 2. ROBUST: remove EVERY container mounting the data volume, by
                #    VOLUME not by name — catches an off-project / renamed QuestDB
                #    that the old by-name `docker rm -f tv-questdb` missed, which
                #    left the volume in-use so `volume rm` silently no-op'd.
                "docker ps -aq --filter volume=tv-questdb-data | xargs -r docker rm -f 2>/dev/null || true",
                # also drop the well-known sidecar containers by name
                "docker rm -f tv-questdb tv-loki tv-alloy 2>/dev/null || true",
                # 3. compose-level teardown for anything still managed there
                f"cd {compose_dir} || exit 0",
                "docker compose down -v --remove-orphans || true",
                # 4. the volume is now unreferenced — remove it, then prune images
                "docker volume rm -f tv-questdb-data 2>/dev/null || true",
                "docker system prune -af --volumes || true",
                # 5. HARD GATE — fail LOUD instead of silently re-attaching stale
                #    data. If the volume survived, it is still in-use; do NOT `up`.
                "if docker volume inspect tv-questdb-data >/dev/null 2>&1; then "
                "echo 'DOCKER-RESET-FAILED: tv-questdb-data still present (in-use) — NOT recreating to avoid re-attaching stale data. Holders:'; "
                "docker ps -a --filter volume=tv-questdb-data --format '{{.Names}} ({{.Status}})'; "
                "echo docker-reset-FAILED; exit 1; "
                "fi",
                "echo 'OK: tv-questdb-data removed'",
                # 6. wipe HOST app caches too (the Docker nuke can't see these) —
                #    instrument-cache (the 'reused existing list' that survived),
                #    spill + dlq. Logs are KEPT for forensics.
                #    + EVERY feed's capture/replay source (operator incident
                #    2026-07-02: the Groww capture file data/groww/
                #    live-ticks.ndjson survived the nuke and RESURRECTED all
                #    Groww rows via the bridge's byte-0 re-tail; the Dhan WAL
                #    replays residual frames the same way). Feed-agnostic:
                #    the generic data/*/live-ticks.ndjson sweep catches any
                #    future feed's capture file too.
                f"rm -rf {data_dir}/instrument-cache {data_dir}/spill {data_dir}/dlq {data_dir}/ws_wal {data_dir}/groww 2>/dev/null || true",
                f"rm -f {data_dir}/*/live-ticks.ndjson {data_dir}/*/*-status.json 2>/dev/null || true",
                "echo 'OK: host caches + feed capture/replay sources wiped (instrument-cache, spill, dlq, ws_wal, groww); logs preserved'",
                # 7. recreate QuestDB fresh (empty volume) + restart app. Use
                #    ensure-questdb.sh — robust to docker-compose v1/v2 absence +
                #    correct service name + SSM creds. The old bare
                #    `docker compose up -d` silently no-op'd on a box without the
                #    v2 plugin, so the nuke DELETED QuestDB but never rebuilt it
                #    → BOOT-02 crash-loop (incident 2026-06-08).
                "bash /opt/tickvault/repo/scripts/ensure-questdb.sh || true",
                "systemctl enable tickvault || true",
                "systemctl restart tickvault || true",
                "echo docker-reset-dispatched",
            ]
            cid = _ssm_shell(cmds)
            # Audit line → the Lambda's CloudWatch log group (same trail the
            # other actions use); the full transcript is in SSM history.
            print(f"operator-portal AUDIT: docker-reset (FULL DOCKER NUKE) dispatched command_id={cid} force={force}")  # noqa: T201
            return _resp(200, {"ok": True, "action": action, "command_id": cid})

        if action == "docker-nuke-bare":
            # TRUE BARE WIPE (operator request 2026-06-12): behave EXACTLY like
            # opening Docker Desktop and deleting all containers + all images +
            # all volumes by hand — everything GONE and it STAYS gone. Unlike
            # `docker-reset` (which wipes then REBUILDS QuestDB + restarts the
            # app, so the empty shells reappear — the source of "it didn't
            # delete"), this does NOT rebuild and does NOT restart the app. The
            # box is left completely bare: no containers, no images, no volumes,
            # no app running, until the operator redeploys.
            if not force:
                return _resp(
                    409,
                    {
                        "error": 'docker-nuke-bare deletes ALL containers+images+volumes and '
                        'LEAVES THE BOX DEAD (no rebuild, no app). re-send with {"force": true}',
                        "action": action,
                    },
                )
            # PR-5 H-1: server-side token (mirror of the wipe-groww gate).
            if str(payload.get("confirm", "")).strip() != "ERASE":
                return _resp(
                    409,
                    {"error": 'docker-nuke-bare leaves the box BARE and DEAD — re-send with {"confirm": "ERASE"}', "action": action},
                )
            data_dir = "/opt/tickvault/data"
            cmds = [
                "set +e",
                # 1. stop + disable the app so nothing re-ups Docker mid-wipe
                "systemctl stop tickvault || true",
                "systemctl disable tickvault || true",
                # 2. remove ALL containers (running + stopped) — like Docker
                #    Desktop "delete all containers"
                "docker ps -aq | xargs -r docker rm -f 2>/dev/null || true",
                # 3. remove ALL images
                "docker images -aq | xargs -r docker rmi -f 2>/dev/null || true",
                # 4. remove ALL volumes
                "docker volume ls -q | xargs -r docker volume rm -f 2>/dev/null || true",
                # 5. final sweep for anything dangling (networks/build cache too)
                "docker system prune -af --volumes 2>/dev/null || true",
                # 6. wipe HOST app caches + EVERY feed's capture/replay source
                #    so the box truly looks fresh (logs KEPT for forensics —
                #    they are not a Docker object). Feed-agnostic sweep incl.
                #    the Groww capture file + Dhan WAL (operator incident
                #    2026-07-02: capture files survived and resurrected rows).
                f"rm -rf {data_dir}/instrument-cache {data_dir}/spill {data_dir}/dlq {data_dir}/ws_wal {data_dir}/groww 2>/dev/null || true",
                f"rm -f {data_dir}/*/live-ticks.ndjson {data_dir}/*/*-status.json 2>/dev/null || true",
                # 7. VERIFY + report the exact remaining counts — truthful, never
                #    a fake OK. 0/0/0 => bare-nuke-complete; otherwise PARTIAL
                #    (something is still in-use) so the operator is not misled.
                "C=$(docker ps -aq 2>/dev/null | wc -l | tr -d ' '); "
                "I=$(docker images -aq 2>/dev/null | wc -l | tr -d ' '); "
                "V=$(docker volume ls -q 2>/dev/null | wc -l | tr -d ' '); "
                'echo "BARE-NUKE-RESULT containers=$C images=$I volumes=$V"; '
                'if [ "$C" = 0 ] && [ "$I" = 0 ] && [ "$V" = 0 ]; then echo bare-nuke-complete; '
                "else echo 'bare-nuke-PARTIAL: something is still present (likely in-use)'; fi",
            ]
            cid = _ssm_shell(cmds)
            return _resp(200, {"ok": True, "action": action, "command_id": cid})

        # ---- overview / data ----
        if action in ("status", "view"):
            inst = _client("ec2").describe_instances(InstanceIds=[INSTANCE_ID])["Reservations"][0]["Instances"][0]
            state = inst["State"]["Name"]
            # The on-box snapshot needs the SSM agent, which is only reachable
            # while the instance is RUNNING. The box auto-stops 16:30 IST, so
            # off-hours we skip SSM entirely and return a clean "stopped" view
            # (instance_state + empty snapshot) instead of 500ing. The operator
            # then taps ▶ Start. _ssm_shell_sync also fails-soft, but skipping
            # avoids a pointless ~6s wait.
            snap = _parse_view(_ssm_shell_sync(_VIEW_COMMANDS) if state == "running" else "")
            mh = _is_market_hours(datetime.datetime.utcnow())
            return _resp(
                200,
                {"ok": True, "action": "view", "instance_state": state, "market_hours": mh, **snap},
            )
        if action == "sql":
            # READ-ONLY console query (Data tab box + DB tab). Writes are
            # blocked server-side by _is_safe_sql (single statement, no
            # comments, banned mutators); _cap_sql_rows clamps/appends LIMIT;
            # the /exp `limit=` param + `head` bound the output regardless.
            q = str(payload.get("query", "")).strip()
            if not _is_safe_sql(q):
                return _resp(400, {"error": "only read-only single-statement SELECT/SHOW/EXPLAIN/WITH queries are allowed (no ';' chaining, no comments)"})
            q = _cap_sql_rows(q)
            import urllib.parse  # noqa: PLC0415

            enc = urllib.parse.quote(q)
            out = _ssm_shell_sync(["set +e", f"curl -fsS 'http://127.0.0.1:9000/exp?query={enc}&limit={_SQL_MAX_ROWS}' 2>/dev/null | head -{_SQL_MAX_ROWS + 1} || echo 'query failed'"])
            return _resp(200, {"ok": True, "action": "sql", "csv": out})
        if action == "qdb_console_url":
            # READ-ONLY: mint a one-click 90s HMAC link to the B4 QuestDB
            # console (questdb-console-front Lambda). Same control secret
            # signs the token; the console verifies it constant-time and
            # swaps it for a 12h session cookie. Env QDB_CONSOLE_URL is
            # injected by Terraform only when var.enable_questdb_console.
            base = os.environ.get("QDB_CONSOLE_URL", "").rstrip("/")
            if not base:
                return _resp(400, {"error": "console not enabled", "action": action})
            tok = _mint_qdb_link_token(_control_secret(), int(time.time()))
            return _resp(200, {"ok": True, "action": action, "url": f"{base}/open?tok={tok}"})
        if action == "logs":
            out = _ssm_shell_sync(
                [
                    "set +e",
                    "echo ERR_BEGIN",
                    "journalctl -u tickvault -p err -n 40 --no-pager 2>/dev/null | tail -40 || true",
                    "echo ERR_END",
                    "echo APP_BEGIN",
                    "journalctl -u tickvault -n 40 --no-pager 2>/dev/null | tail -40 || true",
                    "echo APP_END",
                ]
            )
            return _resp(200, {"ok": True, "action": "logs", "raw": out})
        if action == "latency":
            return _resp(200, {"ok": True, "action": "latency", **_parse_latency(_ssm_shell_sync(_LATENCY_COMMANDS, timeout=_LATENCY_TIMEOUT_SECS))})
        # (cross_verify removed 2026-07-16 — the 15:31 IST Dhan live-vs-
        # historical comparer was deleted in PR-C3 2026-07-14; the card showed
        # a frozen 2026-07-13 FAIL forever. Unknown-action 400 now.)
        if action == "feeds-view":
            # READ-ONLY: current per-feed enabled/lane state + per-feed health,
            # via SSM-curl of the app's local API (SG keeps :3001 closed).
            return _resp(
                200,
                {
                    "ok": True,
                    "action": action,
                    **_parse_feeds_view(_ssm_shell_sync(_FEEDS_VIEW_COMMANDS, timeout=_FEEDS_TIMEOUT_SECS)),
                },
            )
        if action == "feed-toggle":
            # MUTATING but feed-scoped (not box-destructive): flips one feed's
            # runtime flag via the app's POST /api/feeds/{feed}. The dangerous
            # direction (disabling Dhan during live trading) is guarded by the
            # APP's own 409 — passed through verbatim below. Not in
            # _DESTRUCTIVE: the app is the authority on when a flip is unsafe.
            feed = payload.get("feed")
            enabled = payload.get("enabled")
            verr = _validate_feed_toggle(feed, enabled)
            if verr:
                return _resp(400, {"error": verr, "action": action})
            out = _ssm_shell_sync(_feed_toggle_commands(feed, enabled), timeout=_FEEDS_TIMEOUT_SECS)
            r = _parse_feed_toggle(out)
            return _resp(
                200,
                {
                    "ok": r["app_status"] == 200,
                    "action": action,
                    "feed": feed,
                    "enabled": enabled,
                    **r,
                },
            )

        # (gh_prs / gh_merge / gh_deploy removed 2026-07-02 with the GitHub tab —
        # no caller existed outside the deleted tab UI. The `logs` action above
        # is KEPT even though its tab is gone: the tickvault-logs MCP server's
        # cloudwatch_logs tool POSTs {"action":"logs"} to this portal.)

        # ---- aws ----
        if action == "aws_status":
            # Each read is independently fail-soft so a throttle on one doesn't
            # 500 the whole tab.
            try:
                alarms = _client("cloudwatch").describe_alarms(StateValue="ALARM", MaxRecords=20)
                firing = [a["AlarmName"] for a in alarms.get("MetricAlarms", [])]
            except Exception as exc:  # noqa: BLE001
                print(f"operator-portal describe_alarms failed: {exc!r}")  # noqa: T201
                firing = None
            # Disk space + QuestDB size from the box (empty when the box is
            # stopped — SSM offline — which is fine, the UI shows —).
            storage = _parse_storage(_ssm_shell_sync(_STORAGE_COMMANDS))
            return _resp(200, {"ok": True, "action": action, "alarms_firing": firing, "cost_mtd_usd": _month_to_date_cost(), **storage})

        return _resp(400, {"error": f"unknown action: {action!r}"})
    except Exception as exc:  # noqa: BLE001
        print(f"operator-portal action={action!r} failed: {exc!r}")  # noqa: T201 — CloudWatch
        return _resp(500, {"error": "action failed", "action": action})


def _month_to_date_cost() -> str:
    """Month-to-date AWS spend via Cost Explorer (us-east-1 endpoint)."""
    try:
        today = datetime.date.today()
        start = today.replace(day=1).isoformat()
        end = (today + datetime.timedelta(days=1)).isoformat()
        ce = _client("ce", region="us-east-1")
        r = ce.get_cost_and_usage(
            TimePeriod={"Start": start, "End": end},
            Granularity="MONTHLY",
            Metrics=["UnblendedCost"],
        )
        amt = r["ResultsByTime"][0]["Total"]["UnblendedCost"]["Amount"]
        return f"{float(amt):.2f}"
    except Exception:  # noqa: BLE001
        return ""


def _console_html() -> str:
    """The single-page operator portal. Vanilla JS, no deps, no secrets. The
    token is the device key: saved only in this device's localStorage, sent as
    `Authorization: Bearer <token>` on every POST. Raw string so embedded JS
    regex/escapes survive verbatim."""
    return r"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1">
<title>tickvault — operator portal</title>
<style>
  :root{ color-scheme:dark; --bg:#070a12; --card:#111726cc; --line:#222c40; --txt:#e9eef7;
         --mut:#8a93a6; --grn:#28d17c; --red:#ff5d6c; --amb:#f4b740; --blu:#4f8cff; --cyan:#38e1d6; }
  *{ box-sizing:border-box; -webkit-tap-highlight-color:transparent; }
  body{ margin:0; font-family:-apple-system,BlinkMacSystemFont,Segoe UI,Roboto,sans-serif; color:var(--txt);
        min-height:100vh; padding:18px 14px 48px;
        background:radial-gradient(1200px 600px at 10% -10%, #15233f55, transparent 60%),
          radial-gradient(1000px 500px at 110% 0%, #1c2a1f55, transparent 55%),
          linear-gradient(180deg,#070a12,#0a0f1c); background-attachment:fixed; }
  .wrap{ max-width:920px; margin:0 auto; }
  .hdr{ display:flex; align-items:center; gap:12px; margin-bottom:14px; }
  .logo{ font-size:22px; font-weight:800; background:linear-gradient(90deg,#7fb2ff,#38e1d6,#28d17c);
         -webkit-background-clip:text; background-clip:text; color:transparent; }
  .live{ display:flex; align-items:center; gap:7px; font-size:12px; color:var(--mut); margin-left:auto; }
  .dot{ width:11px; height:11px; border-radius:50%; background:var(--red); }
  .dot.on{ background:var(--grn); animation:pulse 1.6s infinite; }
  @keyframes pulse{ 0%{box-shadow:0 0 0 0 #28d17caa} 70%{box-shadow:0 0 0 12px #28d17c00} 100%{box-shadow:0 0 0 0 #28d17c00} }
  .tabs{ display:flex; gap:6px; flex-wrap:wrap; margin-bottom:14px; }
  .tab{ padding:9px 14px; border-radius:11px; background:#0c1322; border:1px solid var(--line); color:var(--mut);
        font-size:13px; font-weight:700; cursor:pointer; }
  .tab.active{ color:#fff; background:linear-gradient(90deg,#1c3158,#163b39); border-color:#2f5fb0; }
  .card{ background:var(--card); border:1px solid var(--line); border-radius:16px; padding:16px; margin-bottom:14px;
         backdrop-filter:blur(6px); box-shadow:0 10px 30px #00000055; animation:rise .45s ease both; }
  @keyframes rise{ from{opacity:0; transform:translateY(8px)} to{opacity:1; transform:none} }
  .lbl{ font-size:11px; text-transform:uppercase; letter-spacing:1px; color:var(--mut); margin-bottom:12px; }
  input,textarea{ width:100%; padding:12px; border-radius:11px; border:1px solid #2a3346; background:#070a12;
        color:var(--txt); font-size:14px; font-family:inherit; }
  textarea{ min-height:64px; font-family:ui-monospace,Menlo,monospace; }
  button{ border:0; border-radius:12px; padding:13px 15px; font-size:14px; font-weight:700; cursor:pointer;
          color:#06101e; transition:transform .08s; }
  button:active{ transform:scale(.97); }
  .b-blu{ background:linear-gradient(90deg,#4f8cff,#38e1d6); color:#fff; } .b-go{ background:linear-gradient(90deg,#28d17c,#7be0a3); }
  .b-amb{ background:linear-gradient(90deg,#f4b740,#ffd479); } .b-stop{ background:linear-gradient(90deg,#ff5d6c,#ff8a93); color:#fff; }
  .b-ghost{ background:#1a2336; color:var(--txt); }
  .row{ display:flex; gap:10px; flex-wrap:wrap; } .row>button{ flex:1 1 150px; }
  .hero{ display:flex; gap:16px; flex-wrap:wrap; }
  .bignum{ flex:2 1 240px; background:linear-gradient(135deg,#0e1626,#0a1120); border:1px solid var(--line);
           border-radius:16px; padding:18px; position:relative; overflow:hidden; }
  .bignum .n{ font-size:44px; font-weight:900; line-height:1; background:linear-gradient(90deg,#9fd0ff,#38e1d6);
              -webkit-background-clip:text; background-clip:text; color:transparent; }
  .bignum .c{ font-size:12px; color:var(--mut); margin-top:6px; }
  .bignum::after{ content:""; position:absolute; inset:0; background:linear-gradient(120deg,transparent 30%,#ffffff14 50%,transparent 70%);
        transform:translateX(-120%); animation:shine 3.4s infinite; } @keyframes shine{ to{transform:translateX(120%)} }
  .pills{ flex:1 1 160px; display:grid; grid-template-columns:1fr 1fr; gap:10px; }
  .pill{ border:1px solid var(--line); border-radius:13px; padding:12px; background:#070b15; }
  .pill .v{ font-size:18px; font-weight:800; } .pill .k{ font-size:11px; color:var(--mut); margin-top:3px; }
  .bar{ display:flex; align-items:center; gap:10px; margin:9px 0; }
  .bar .name{ width:46px; font-size:12px; color:var(--mut); }
  .bar .track{ flex:1; height:14px; background:#0a1120; border-radius:8px; overflow:hidden; border:1px solid var(--line); }
  .bar .fill{ height:100%; width:0; border-radius:8px; background:linear-gradient(90deg,#4f8cff,#38e1d6); transition:width .9s cubic-bezier(.2,.8,.2,1); }
  .bar .num{ width:60px; text-align:right; font-weight:700; font-size:13px; }
  .shields{ display:grid; grid-template-columns:repeat(auto-fit,minmax(150px,1fr)); gap:10px; }
  .shield{ border:1px solid var(--line); border-radius:14px; padding:14px; background:#070b15; animation:pop .45s ease both; }
  @keyframes pop{ from{opacity:0; transform:scale(.94)} to{opacity:1} }
  .shield .ttl{ font-size:12px; color:var(--mut); } .shield .st{ font-size:17px; font-weight:800; margin-top:5px; }
  .shield.good{ border-color:#1d5b3d; box-shadow:0 0 18px #28d17c22; } .shield.bad{ border-color:#5b2330; box-shadow:0 0 18px #ff5d6c22; }
  .shield.idle{ border-color:var(--line); box-shadow:none; opacity:.55; } .shield.idle .st{ color:var(--mut); }
  .banner{ border:1px solid #3a3110; background:#141207; color:var(--amb); border-radius:12px; padding:12px 14px;
           font-size:13px; margin-bottom:10px; }
  details.fold>summary{ cursor:pointer; list-style:none; } details.fold>summary::-webkit-details-marker{ display:none; }
  /* Visible disclosure affordance — the operator twice read the folded danger
     zone as "the wipes were DELETED" because the fold had no visual cue. */
  details.fold>summary::after{ content:' ▸'; color:var(--mut); font-size:12px; }
  details.fold[open]>summary::after{ content:' ▾'; }
  .strip{ display:flex; gap:14px; flex-wrap:wrap; align-items:center; font-size:13px; font-weight:700; }
  .danger-opt{ display:flex; gap:10px; align-items:flex-start; margin:12px 0; cursor:pointer; }
  .danger-opt input{ width:auto; margin-top:3px; }
  .ok{ color:var(--grn);} .bad{ color:var(--red);} .warn{ color:var(--amb);} .cy{ color:var(--cyan);}
  .muted{ color:var(--mut); font-size:12px; }
  pre{ background:#070b15; border:1px solid var(--line); border-radius:11px; padding:10px; overflow:auto;
       font-size:11.5px; white-space:pre-wrap; max-height:300px; }
  table{ width:100%; border-collapse:collapse; font-size:12px; } th,td{ text-align:left; padding:6px 8px; border-bottom:1px solid var(--line); }
  th{ color:var(--mut); font-weight:600; }
  td.win{ color:var(--grn); font-weight:800; } /* best-per-column in the per-feed latency table */
  .pr{ display:flex; align-items:center; gap:10px; padding:10px 0; border-bottom:1px solid var(--line); flex-wrap:wrap; }
  .pr .t{ flex:1 1 200px; } .pr .num{ color:var(--mut); font-weight:700; margin-right:6px; }
  .badge{ font-size:11px; font-weight:800; padding:3px 9px; border-radius:20px; }
  .badge.success{ background:#103a27; color:var(--grn);} .badge.pending{ background:#3a3110; color:var(--amb);}
  .badge.failure{ background:#3a1620; color:var(--red);} .badge.unknown{ background:#1a2336; color:var(--mut);}
  .mini{ padding:9px 12px; font-size:12px; flex:0 0 auto; }
  .switch{ display:flex; align-items:center; gap:8px; font-size:12px; color:var(--mut); }
  .toast{ position:fixed; left:50%; bottom:22px; transform:translateX(-50%) translateY(20px); background:#1a2336;
          border:1px solid var(--line); padding:12px 18px; border-radius:12px; font-size:14px; opacity:0; transition:.25s;
          pointer-events:none; box-shadow:0 10px 30px #000; } .toast.show{ opacity:1; transform:translateX(-50%) translateY(0); }
  [hidden]{ display:none !important; }
  .lock{ text-align:center; padding:26px 18px; } .lock .ic{ font-size:40px; }
  .spin{ animation:spin 1s linear infinite; display:inline-block; } @keyframes spin{ to{transform:rotate(360deg)} }
</style>
</head>
<body>
<div class="wrap">
  <div class="hdr">
    <div class="logo">🛰️ tickvault</div>
    <div class="live"><span class="dot" id="livedot"></span><span id="livetxt">offline</span></div>
  </div>

  <div class="card lock" id="lock">
    <div class="ic">🔐</div>
    <div class="lbl" style="margin-top:8px">device key</div>
    <p class="muted" style="max-width:460px;margin:6px auto 14px">This portal only works on a device that holds your
      secret key — your laptop and your phone. Anyone else who opens this link sees this screen and can do nothing.
      Paste your key once; it is saved on THIS device only.</p>
    <input id="tok" type="password" placeholder="paste your operator key" autocomplete="off">
    <div class="row" style="margin-top:12px"><button class="b-blu" onclick="unlock()">🔓 Unlock this device</button></div>
  </div>

  <div id="app" hidden>
    <div class="tabs">
      <div class="tab active" data-t="overview" onclick="tab('overview')">📊 Overview</div>
      <div class="tab" data-t="data" onclick="tab('data')">📈 Data</div>
      <div class="tab" data-t="admin" onclick="tab('admin')">🛠️ Admin</div>
    </div>

    <!-- OVERVIEW -->
    <section data-tab="overview">
      <div class="card">
        <div class="hero">
          <div class="bignum"><div class="n" id="ticksbig">0</div><div class="c">official minute rows captured today</div><div class="c" id="feedsplit"></div></div>
          <div class="pills">
            <div class="pill"><div class="v" id="p_inst">—</div><div class="k">instance</div></div>
            <div class="pill"><div class="v" id="p_app">—</div><div class="k">app</div></div>
            <div class="pill"><div class="v" id="p_mkt">—</div><div class="k">market</div></div>
          </div>
        </div>
        <div class="row" style="margin-top:14px">
          <button class="b-blu" id="refbtn" onclick="loadOverview()">🔄 Refresh now</button>
          <button class="b-go" id="instbtn" onclick="instAct()" hidden>▶ Start instance</button>
        </div>
        <div style="display:flex;align-items:center;gap:14px;margin-top:10px;flex-wrap:wrap">
          <label class="switch"><input type="checkbox" id="auto" checked onchange="autotoggle()"> auto-refresh (8s)</label>
          <label class="switch"><input type="checkbox" id="force_inst"> force (override market-hours guard)</label>
          <span class="muted" id="updated"></span>
        </div>
        <div class="muted" style="margin-top:8px">One button, context-aware: ▶ Start when the box is stopped, ■ Stop when it is running. Stop is blocked 9:15 AM–3:30 PM IST Mon–Fri unless force is ticked.</div>
      </div>
      <div class="card">
        <details class="fold" id="awsdetails">
          <summary><div class="lbl" style="margin-bottom:6px">aws — tap to expand alarms + storage</div>
            <div class="strip" id="awsstrip"><span class="muted">not loaded yet</span></div></summary>
          <div class="row" style="margin:12px 0"><button class="b-ghost mini" onclick="loadAws()">🔄 Refresh AWS</button></div>
          <div id="alarms"></div>
          <div class="lbl" style="margin-top:14px">storage — disk space &amp; database size (on the box)</div>
          <div class="shields" id="storage"></div>
          <div class="muted" style="margin-top:8px">EBS auto-archives partitions &gt;90d to S3, and grows online — so it won't fill. Watch "DB size" climb day-by-day to see your GB/day. (Shows — when the box is stopped.)</div>
        </details>
      </div>
      <div class="card"><div class="lbl">guarantees — live proof read from the box</div>
        <div class="banner" id="stoppedbanner" hidden>⏸ Box stopped (auto-stops 16:30 IST, auto-starts 08:30 Mon–Fri) — guarantees resume on start.</div>
        <div class="shields" id="shields"></div></div>
      <div class="card"><div class="lbl">feeds — per-broker official-candle pulls (toggle on/off)</div>
        <div class="row" style="margin-bottom:10px"><button class="b-blu" onclick="loadFeeds()">🔄 Load feeds</button></div>
        <div id="feeds"><span class="muted">not loaded yet</span></div>
        <div class="muted" id="feedsmsg" style="margin-top:8px"></div>
        <div class="muted" style="margin-top:6px">Every feed the app reports appears here automatically (a future feed #3 needs zero portal changes). Turning a feed OFF asks for confirmation; disabling Dhan during live trading is refused by the app itself. The pull line below each feed comes from today's fetch log: how many minute-pulls succeeded / failed / were rate-limited, and how fast the successful pulls landed in the last hour.</div>
      </div>
      <div class="card"><div class="lbl">latency — how fast each official minute candle arrives after its minute closes</div>
        <div class="row" style="margin-bottom:12px"><button class="b-blu" onclick="loadLatency()">⚡ Measure now</button></div>
        <div id="latrest" style="overflow:auto"></div>
        <div class="muted" id="latrestnote" style="margin-top:8px"></div>
        <div class="muted" style="margin-top:6px">One row per (broker, pull type) read from today's fetch log on the box —
          p50/p99 of "minute closed → data in hand", successful pulls only (approximate percentiles, 3 significant
          figures, computed inside the database). Late repairs keep their real ≥60 s delay; unmeasured rows are
          excluded, never faked as fast. "-" = no successful pulls recorded today for that pair.</div>
        <div class="lbl" style="margin-top:14px">box-wide (shared by all pulls)</div>
        <div class="shields" id="latnet"></div>
        <div class="muted" style="margin-top:6px">DB round-trip / clock skew are box-wide. Order placement reads "—"
          until live trading returns (the order path is dormant by design).</div>
      </div>
    </section>

    <!-- DATA -->
    <section data-tab="data" hidden>
      <div class="card"><div class="lbl">official minute rows captured today (per table)</div><div id="bars"></div>
        <div class="muted" style="margin-top:6px">spot = index minute candles · chain = option-chain snapshots · contracts = per-contract minute candles — all pulled once a minute from the brokers' official records.</div></div>
      <div class="card">
        <div class="lbl">database console &nbsp;<span class="badge unknown">READ-ONLY — writes are blocked server-side</span></div>
        <div class="row" style="margin-bottom:10px"><button class="b-blu" onclick="openQdbConsole()">🗄 Open QuestDB Console</button></div>
        <div style="display:flex;gap:16px;flex-wrap:wrap">
          <div style="flex:1 1 200px;min-width:180px">
            <div class="lbl">tables</div>
            <div class="row" style="margin-bottom:8px"><button class="b-ghost mini" onclick="loadDbTables()">🔄 Reload tables</button></div>
            <div id="dbtables" style="max-height:420px;overflow:auto"><span class="muted">not loaded yet</span></div>
          </div>
          <div style="flex:2 1 380px;min-width:280px">
            <div class="lbl">query</div>
            <textarea id="dbsql" spellcheck="false">SELECT * FROM spot_1m_rest ORDER BY ts DESC LIMIT 50</textarea>
            <div class="row" style="margin-top:10px">
              <button class="b-blu" onclick="runDbSql()">▶ Run</button>
              <button class="b-ghost" onclick="dbDownloadCsv()">⬇ Download CSV</button></div>
            <div class="muted" id="dbcount" style="margin-top:8px"></div>
            <div id="dbout" style="margin-top:10px;overflow:auto;max-height:480px"></div>
            <div class="muted" style="margin-top:8px">Only single-statement SELECT / SHOW / EXPLAIN / WITH — mutations, ';' chaining and comments are rejected by the server. Row cap 1000 (a bigger LIMIT is clamped). Click a table on the left for its columns + first 100 rows.</div>
          </div>
        </div>
        <div class="lbl" style="margin-top:16px">columns <span class="muted" id="dbcolname"></span></div>
        <div id="dbcols" style="overflow:auto;max-height:320px"><span class="muted">click a table</span></div>
      </div>
    </section>

    <!-- ADMIN -->
    <section data-tab="admin" hidden>
      <div class="card"><div class="lbl">app control</div>
        <div class="row" style="margin-bottom:10px">
          <button class="b-amb" onclick="act('restart-app')">♻ Restart app</button>
          <button class="b-amb" onclick="act('restart-questdb')">♻ Restart QuestDB</button>
          <button class="b-stop" onclick="act('stop-app')">⏹ Stop app</button></div>
        <div class="muted" style="margin-top:10px">Stop / restart are blocked 9:15 AM–3:30 PM IST Mon–Fri.</div>
        <label class="switch" style="margin-top:8px"><input type="checkbox" id="force"> force (override market-hours guard)</label>
      </div>
      <div class="card">
        <details class="fold" id="danger">
          <summary><span class="lbl" style="display:inline">⚠️ danger zone — destructive data actions (tap to open)</span> <span class="muted" style="font-size:11px">(contains: Wipe ALL · Docker reset · Bare nuke)</span></summary>
          <div class="warn" id="dangerlock" hidden style="margin-top:10px">🔒 Locked until 3:30 PM IST — data-destructive actions are refused during market hours, even with force. A mid-market wipe destroys data that can never be re-fetched.</div>
          <div class="muted" style="margin-top:10px">Pick ONE severity, then Execute. Every action still asks you to type its own confirm word — nothing fires from a mis-click.</div>
          <label class="danger-opt"><input type="radio" name="danger" value="wipe">
            <span><b>🗑️ Wipe ALL data → fresh start</b> — <span class="muted">deletes every legacy tick &amp; candle table AND today's official minute candles + the fetch log (spot / option-chain / per-contract tables) for BOTH brokers, plus every capture/replay file so NOTHING resurrects after restart — SEBI audit tables kept. Run when the box is RUNNING; next session = fresh data. Asks you to type WIPE.</span></span></label>
          <label class="danger-opt"><input type="radio" name="danger" value="nuke">
            <span><b style="color:#f66">☢ Full Docker nuke — wipes ALL data (QuestDB volumes) + fresh start</b> — <span class="muted">⚠️ NUCLEAR: stops the app, deletes Docker containers + volumes + images (full QuestDB wipe INCLUDING the SEBI-retention audit tables) + prune, then rebuilds QuestDB fresh + restarts the app. Box must be RUNNING. Asks you to type NUKE-DOCKER.</span></span></label>
          <label class="danger-opt"><input type="radio" name="danger" value="erase">
            <span><b>☢️ Bare Nuke → delete ALL &amp; leave EMPTY</b> — <span class="muted">☢️ Like Docker Desktop "delete all": removes <b>every</b> container + image + volume and does <b>NOT</b> rebuild — the box is left completely bare with <b>nothing running</b> (trading OFF until you redeploy). Asks you to type ERASE.</span></span></label>
          <div class="row" style="margin-top:12px"><button class="b-stop" onclick="dangerExecute()">☠ Execute selected action</button></div>
        </details>
      </div>
      <div class="card"><div class="lbl">device</div>
        <div class="row"><button class="b-ghost" onclick="lock()">🔒 Lock / forget this device</button></div>
        <div class="muted" style="margin-top:8px">Forgets the operator key on THIS device — the portal locks until the key is pasted again.</div>
      </div>
    </section>
  </div>
</div>
<div class="toast" id="toast"></div>

<script>
const $=id=>document.getElementById(id);
let TOKEN=localStorage.getItem('tv_token')||'';
let timer=null, lastTicks=0, curTab='overview';

function toast(m){ const t=$('toast'); t.textContent=m; t.classList.add('show'); clearTimeout(t._h); t._h=setTimeout(()=>t.classList.remove('show'),2600); }
function esc(s){ return String(s).replace(/[<>&]/g,c=>({'<':'&lt;','>':'&gt;','&':'&amp;'}[c])); }
function setLive(on){ $('livedot').classList.toggle('on',on); $('livetxt').textContent=on?'live':'offline'; }

function unlock(){ const v=($('tok').value||'').trim(); if(!v){ toast('Paste your key first'); return; }
  TOKEN=v; localStorage.setItem('tv_token',v); $('lock').hidden=true; $('app').hidden=false; startAuto(); loadOverview(); }
function lock(){ TOKEN=''; localStorage.removeItem('tv_token'); $('tok').value=''; stopAuto(); $('app').hidden=true; $('lock').hidden=false; setLive(false); toast('Locked — key forgotten on this device'); }

function tab(name){ curTab=name; document.querySelectorAll('.tab').forEach(t=>t.classList.toggle('active',t.dataset.t===name));
  document.querySelectorAll('section[data-tab]').forEach(s=>s.hidden=s.dataset.tab!==name);
  if(name==='data' && !$('dbtables').dataset.loaded) loadDbTables(); }

async function call(action, extra){ if(!TOKEN){ toast('Locked'); return null; }
  try{ const r=await fetch(location.href,{method:'POST',
      headers:{'authorization':'Bearer '+TOKEN,'content-type':'application/json'},
      body:JSON.stringify(Object.assign({action},extra||{}))});
    const j=await r.json().catch(()=>({}));
    if(r.status===401){ toast('Wrong key (401)'); setLive(false); return null; }
    if(r.status===409){ toast(j.error||'Blocked during market hours'); return null; }
    if(!r.ok){ toast(j.error||('Error '+r.status)); return null; }
    return j;
  }catch(e){ toast('Network error'); setLive(false); return null; } }

function countUp(el,target){ target=parseInt(String(target).replace(/[^0-9]/g,''),10)||0; const from=lastTicks||0; lastTicks=target;
  const t0=performance.now(),dur=900; function step(t){ const k=Math.min(1,(t-t0)/dur);
    el.textContent=Math.round(from+(target-from)*(1-Math.pow(1-k,3))).toLocaleString(); if(k<1) requestAnimationFrame(step); } requestAnimationFrame(step); }

function bar(name,n,max){ const pct=max>0?Math.max(3,Math.round(100*n/max)):0;
  return '<div class="bar"><div class="name">'+name+'</div><div class="track"><div class="fill" style="width:'+pct+'%"></div></div><div class="num">'+n.toLocaleString()+'</div></div>'; }
// Optional 4th arg picks the value colour explicitly: 'ok' green, 'warn' amber,
// 'bad' red — so a critical state (red) is distinguishable from drift (amber).
// Optional 5th arg `tip` renders a hover tooltip (title attribute) — used by
// the Tick-conservation shield to state its honest envelope.
function shield(t,s,good,cls,tip){ return '<div class="shield '+(good?'good':'bad')+'"'+(tip?' title="'+esc(tip).replace(/"/g,'&quot;')+'"':'')+'><div class="ttl">'+t+'</div><div class="st '+(cls||(good?'ok':'warn'))+'">'+s+'</div></div>'; }
// Greyed "box stopped" shield: neither good nor bad — the box is off, so the
// guarantee is neither proven nor violated. Used ONLY when instance_state is
// not 'running' (a RUNNING box with unreachable data keeps the REAL warnings).
function shieldIdle(t){ return '<div class="shield idle"><div class="ttl">'+t+'</div><div class="st">—</div></div>'; }

async function loadOverview(){ $('refbtn').innerHTML='<span class="spin">🔄</span> Refreshing…'; const j=await call('view'); $('refbtn').textContent='🔄 Refresh now'; if(!j) return;
  const appOk=j.app==='active', running=j.instance_state==='running'; setLive(appOk&&running);
  // ONE context-aware instance button: ▶ Start when stopped, ■ Stop when
  // running. Server-side market-hours guard on 'stop' is unchanged; the
  // force checkbox next to it feeds through act().
  instState=j.instance_state||'';
  const ib=$('instbtn'); ib.hidden=false;
  if(running){ ib.textContent='■ Stop instance'; ib.className='b-stop'; }
  else{ ib.textContent='▶ Start instance'; ib.className='b-go'; }
  countUp($('ticksbig'), j.rows_today_total||'0');
  $('p_inst').innerHTML='<span class="'+(running?'ok':'bad')+'">'+(j.instance_state||'?')+'</span>';
  $('p_app').innerHTML='<span class="'+(appOk?'ok':'bad')+'">'+(appOk?'up':(j.app||'down'))+'</span>';
  $('p_mkt').innerHTML='<span class="'+(j.market_hours?'warn':'')+'">'+(j.market_hours?'OPEN':'closed')+'</span>';
  // Danger-zone hard-lock label: data-destructive actions are server-refused
  // (409, no force escape) while the market is open — surface that BEFORE a click.
  const dl=$('dangerlock'); if(dl) dl.hidden=!j.market_hours;
  // Per-broker split of today's official minute rows — summed across the
  // three live tables, rendered generically from whatever feeds QuestDB
  // reports (no hardcoding; a future broker appears automatically).
  const perFeed={};
  Object.values(j.rows_by_feed||{}).forEach(t=>{ Object.keys(t||{}).forEach(f=>{ perFeed[f]=(perFeed[f]||0)+(parseInt(t[f],10)||0); }); });
  const fbKeys=Object.keys(perFeed).sort();
  $('feedsplit').textContent = fbKeys.length ? fbKeys.map(k=>k+': '+perFeed[k].toLocaleString()).join(' | ') : '';
  // Data-tab bars: today's rows per LIVE table (spot / chain / contracts).
  const rt=j.rows_today||{}, tkeys=['spot','chain','contracts'];
  const vals=tkeys.map(k=>parseInt(rt[k],10)||0), mx=Math.max(1,...vals);
  $('bars').innerHTML=tkeys.map((k,i)=>bar(k,vals[i],mx)).join('');
  // Dedup-key check: the real spot_1m_rest upsert key is 4 columns
  // (ts, security_id, exchange_segment, feed) per DEDUP_KEY_SPOT_1M_REST.
  // Three distinct states:
  //   4 = OK (green) · 0 = DEDUP disabled entirely (RED — duplicate rows will
  //   accumulate) · any other number = schema drift (amber). A fetch failure
  //   (box/QuestDB unreachable) renders "unreachable" (amber), never a fake 0.
  // Stopped box → ONE calm banner + a greyed "—" shield instead of a scary
  // "unreachable" warning. Gated STRICTLY on instance_state!=='running' — a
  // RUNNING box with unreachable data keeps the real warnings below.
  $('stoppedbanner').hidden=running;
  if(!running){
    $('shields').innerHTML=shieldIdle('Dedup key columns');
  } else {
  const dkRaw=String(j.dedup_key_columns||'').trim(), dkN=parseInt(dkRaw,10);
  const dkUnknown=(dkRaw===''||isNaN(dkN)), keysOk=dkN===4;
  let dkTxt,dkCls;
  if(dkUnknown){ dkTxt='unreachable'; dkCls='warn'; }
  else if(dkN===4){ dkTxt='4 ✅'; dkCls='ok'; }
  else if(dkN===0){ dkTxt='0 — DEDUP disabled!'; dkCls='bad'; }
  else { dkTxt=dkRaw+' — schema drift (expected 4)'; dkCls='warn'; }
  $('shields').innerHTML=shield('Dedup key columns',dkTxt,keysOk,dkCls);
  }
  if(!$('feeds').dataset.loaded) loadFeeds();
  if(!$('alarms').dataset.loaded) loadAws();
  $('updated').textContent='updated '+new Date().toLocaleTimeString(); }

// The one context-aware instance action. 'start' is never market-hours
// blocked server-side; 'stop' keeps the guard — force feeds through act().
let instState='';
function instAct(){ act(instState==='running'?'stop':'start','force_inst'); }

// FEEDS card. Rendered by ITERATING the app's own per-feed list — primarily the
// health response's `feeds` array (one row per feed the app knows about), with
// a generic fallback that derives feed names from the `<name>_enabled` keys of
// GET /api/feeds. No feed name is hardcoded, so a future feed #3/#4 added to
// the app automatically appears here with its own toggle. Errors are rendered
// VERBATIM (esc()-wrapped) — never silent defaults.
async function loadFeeds(){ $('feeds').dataset.loaded='1'; $('feeds').innerHTML='<span class="muted">loading…</span>'; $('feedsmsg').textContent='';
  const j=await call('feeds-view'); if(!j){ $('feeds').innerHTML='<span class="warn">feeds-view request failed</span>'; return; }
  let list=[];
  if(j.health && Array.isArray(j.health.feeds) && j.health.feeds.length){
    list=j.health.feeds.map(r=>({name:String(r.feed), enabled:!!r.enabled, lane:!!r.lane_running, h:r}));
  } else if(j.feeds && typeof j.feeds==='object'){
    list=Object.keys(j.feeds).filter(k=>k.endsWith('_enabled')).map(k=>k.slice(0,-'_enabled'.length))
      .map(n=>({name:n, enabled:!!j.feeds[n+'_enabled'], lane:!!j.feeds[n+'_lane_running'], h:null}));
  }
  // Prefer the /api/feeds runtime flags when both payloads are present.
  if(j.feeds && typeof j.feeds==='object'){ list.forEach(f=>{ if((f.name+'_enabled') in j.feeds){ f.enabled=!!j.feeds[f.name+'_enabled']; }
    if((f.name+'_lane_running') in j.feeds){ f.lane=!!j.feeds[f.name+'_lane_running']; } }); }
  if(!list.length){ $('feeds').innerHTML='<span class="warn">'+esc(j.feeds_error||j.health_error||'no feed data returned')+'</span>'; return; }
  const vb=v=>({ok:'success',down:'failure',disabled:'unknown',unknown:'unknown'})[v]||'pending';
  $('feeds').innerHTML=list.map(f=>{
    // safe token for the onclick attribute (display uses esc()); the Lambda
    // re-validates the same charset server-side before any shell command.
    const safeName=String(f.name).toLowerCase().replace(/[^a-z0-9_-]/g,'');
    const h=f.h; const badge=h?('<span class="badge '+vb(h.verdict)+'">'+esc(h.verdict||'?')+'</span>'):'<span class="badge unknown">no health</span>';
    // REST-pull line (2026-07-16): today's fetch-log outcomes + last-hour
    // prompt-pull latency from rest_fetch_audit — the old ticks/subscribed
    // counters read frozen live-feed registries and are gone. "failed" =
    // every non-ok, non-rate-limited outcome (error/empty/no_token/…).
    const ra=(j.rest_audit||{})[f.name], rl=(j.rest_lat_hour||{})[f.name];
    let pull;
    if(ra && Object.keys(ra).length){
      const ok=ra.ok||0, rlim=ra.rate_limited||0;
      const failed=Object.keys(ra).filter(k=>k!=='ok'&&k!=='rate_limited').reduce((s,k)=>s+(ra[k]||0),0);
      pull='pulls today: '+ok.toLocaleString()+' ok · '+failed.toLocaleString()+' failed · '+rlim.toLocaleString()+' rate-limited';
      if(rl&&(rl.p50_ms!=null||rl.p99_ms!=null)) pull+=' — last hour: p50 '+fmtLagMs(rl.p50_ms)+' / p99 '+fmtLagMs(rl.p99_ms)+' after minute close';
    } else { pull='no official-candle pulls recorded today'; }
    const detail='<div class="muted">'+(h&&h.reason?esc(h.reason)+' · ':'')+pull+'</div>';
    return '<div class="pr"><div class="t"><b>'+esc(f.name)+'</b> — <span class="'+(f.enabled?'ok':'bad')+'">'+(f.enabled?'ENABLED':'disabled')+'</span>'
      +' · lane '+(f.lane?'<span class="ok">running</span>':'<span class="warn">not running</span>')+' '+badge+detail+'</div>'
      +'<button class="'+(f.enabled?'b-stop':'b-go')+' mini" onclick="feedToggle(\''+safeName+'\','+(!f.enabled)+')">'+(f.enabled?'⏹ turn OFF':'▶ turn ON')+'</button></div>'; }).join('');
  const errs=[j.feeds_error,j.health_error].filter(Boolean).join(' · ');
  if(errs) $('feedsmsg').textContent=errs; }
async function feedToggle(feed,enabled){
  if(!enabled && !confirm('Turn OFF the "'+feed+'" live feed? Ticks from '+feed+' stop until you turn it back on.')){ toast('cancelled'); return; }
  toast((enabled?'enabling ':'disabling ')+feed+'…');
  const j=await call('feed-toggle',{feed:feed,enabled:enabled}); if(!j) return;
  if(j.ok){ toast('✅ '+feed+' '+(enabled?'enabled':'disabled')); }
  else{ // Surface the APP's own message verbatim — incl. the 409 Dhan
        // live-trading guard ("refusing to disable the Dhan feed …").
    const m=(j.app_response&&j.app_response.error)||j.error||('feed toggle failed (app status '+j.app_status+')');
    $('feedsmsg').textContent='⚠ '+m; toast('⚠ '+String(m).slice(0,120)); }
  setTimeout(loadFeeds,1200); }

// Shared CSV → grid renderer (Data tab query box + DB tab reuse it).
function parseCsv(csv){ const lines=(csv||'').split(/\r?\n/).filter(x=>x.length);
  return lines.map(l=>l.split(',').map(c=>c.replace(/^"|"$/g,''))); }
// QuestDB renders our IST-stored timestamps as "...T15:28:00.000000Z" — the
// value is already IST (project storage convention); strip the noisy
// microseconds + Z and the T so it reads "2026-06-01 15:28:00 IST".
function tcell(c){ const m=/^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2}:\d{2})(?:\.\d+)?Z?$/.exec(c); return m? m[1]+' '+m[2]+' IST' : c; }
// Renders rows (header + data, all esc()-wrapped) into el; returns data-row count.
function renderGrid(el,rows){ if(!rows.length){ el.innerHTML='<span class="muted">no rows</span>'; return 0; }
  let h='<table><tr>'+rows[0].map(c=>'<th>'+esc(c)+'</th>').join('')+'</tr>';
  for(let i=1;i<rows.length;i++) h+='<tr>'+rows[i].map(c=>'<td>'+esc(tcell(c))+'</td>').join('')+'</tr>';
  el.innerHTML=h+'</table>'; return rows.length-1; }

// ---- DB console (read-only, lives in the Data tab) ----
// Writes are blocked SERVER-SIDE by the hardened _is_safe_sql gate (single
// statement, no comments, banned mutators) + the 1000-row cap — the UI is a
// convenience, never the security boundary.
let dbTables=[], dbCsv='';
async function loadDbTables(){ $('dbtables').dataset.loaded='1'; $('dbtables').innerHTML='<span class="muted">loading…</span>';
  const j=await call('sql',{query:'SELECT table_name FROM tables() ORDER BY table_name'});
  if(!j){ $('dbtables').innerHTML='<span class="warn">load failed</span>'; return; }
  dbTables=parseCsv(j.csv).slice(1).map(r=>r[0]).filter(Boolean);
  if(!dbTables.length){ $('dbtables').innerHTML='<span class="warn">no tables returned — is the box running?</span>'; return; }
  // Click handlers pass an INDEX into the just-fetched list — the table name
  // used in follow-up queries always comes from QuestDB's own tables() output,
  // never from user-typed input (no injection surface).
  $('dbtables').innerHTML=dbTables.map((t,i)=>'<div style="padding:6px 0;border-bottom:1px solid var(--line)"><a href="#" style="color:var(--cyan);text-decoration:none;font-size:13px" onclick="dbPick('+i+');return false">'+esc(t)+'</a></div>').join(''); }
async function dbPick(i){ const t=dbTables[i]; if(t==null) return;
  $('dbcolname').textContent='— '+t; $('dbcols').innerHTML='<span class="muted">loading…</span>';
  const j=await call('sql',{query:"SELECT * FROM table_columns('"+t.replace(/'/g,"''")+"')"});
  if(j) renderGrid($('dbcols'),parseCsv(j.csv)); else $('dbcols').innerHTML='<span class="warn">columns load failed</span>';
  $('dbsql').value='SELECT * FROM "'+t.replace(/"/g,'')+'" LIMIT 100';
  runDbSql(); }
async function runDbSql(){ const q=$('dbsql').value.trim(); if(!q){ toast('Type a query'); return; }
  $('dbout').innerHTML='<span class="muted">running…</span>'; $('dbcount').textContent=''; dbCsv='';
  const j=await call('sql',{query:q}); if(!j){ $('dbout').innerHTML=''; return; }
  dbCsv=j.csv||''; const n=renderGrid($('dbout'),parseCsv(dbCsv));
  $('dbcount').textContent=n+' row'+(n===1?'':'s')+(n>=1000?' — server cap 1000 reached':''); }
async function openQdbConsole(){ const w=window.open('','_blank'); // open SYNC in the click (popup-safe) BEFORE the await
  const j=await call('qdb_console_url');
  if(j&&j.url){ if(w) w.location=j.url; else window.open(j.url,'_blank'); }
  else { if(w) w.close(); } } // call() already toasts the {error}/non-ok case
function dbDownloadCsv(){ if(!dbCsv){ toast('Run a query first'); return; }
  const a=document.createElement('a'); a.href=URL.createObjectURL(new Blob([dbCsv],{type:'text/csv'}));
  a.download='tickvault-query-'+new Date().toISOString().slice(0,19).replace(/[:T]/g,'-')+'.csv';
  document.body.appendChild(a); a.click(); a.remove(); URL.revokeObjectURL(a.href); }

// AWS strip on Overview (spend $ · alarms firing · disk used %) — sourced from
// the same aws_status action the old AWS tab used. The <details> expands to
// the full alarm list + storage shields. Loaded once on first Overview load
// (NOT on the 8s auto-refresh — aws_status does an SSM probe + Cost Explorer
// call) and on demand via the 🔄 Refresh AWS button.
async function loadAws(){ $('alarms').dataset.loaded='1'; $('awsstrip').innerHTML='<span class="muted">loading…</span>'; $('alarms').innerHTML='<span class="muted">loading…</span>';
  const j=await call('aws_status'); if(!j){ $('awsstrip').innerHTML='<span class="warn">AWS read failed — retry</span>'; $('alarms').innerHTML=''; return; }
  const cwFail=(j.alarms_firing===null||j.alarms_firing===undefined), al=j.alarms_firing||[];
  const alTxt=cwFail?'?':String(al.length), alCls=cwFail?'warn':(al.length?'bad':'ok');
  $('awsstrip').innerHTML='<span>💵 $'+esc(j.cost_mtd_usd||'—')+' spent this month</span>'+
    '<span class="'+alCls+'">🔔 '+alTxt+' alarm'+(alTxt==='1'?'':'s')+' firing</span>'+
    '<span>💾 disk '+esc(j.disk_pct||'—')+' used</span>';
  if(cwFail){ $('alarms').innerHTML='<span class="warn">CloudWatch read failed — retry.</span>'; }
  else{ $('alarms').innerHTML=al.length? al.map(a=>'<div class="pr"><span class="badge failure">ALARM</span> <span class="t">'+esc(a)+'</span></div>').join('') : '<span class="ok">all clear 🎉</span>'; }
  const free=parseInt(j.disk_free_gb,10), pct=parseInt(j.disk_pct,10);
  const freeOk=isNaN(free)?true:free>5; const pctOk=isNaN(pct)?true:pct<85;
  $('storage').innerHTML=
    shield('Disk free', (j.disk_free_gb?j.disk_free_gb+' GB':'—'), freeOk)+
    shield('Disk used', (j.disk_used_gb?j.disk_used_gb+' GB ('+(j.disk_pct||'')+')':'—'), pctOk)+
    shield('Database size', (j.db_size_gb?j.db_size_gb+' GB':'—'), true); }

function fmtNs(n){ if(n==null||n==='') return '—'; n=Number(n); if(n<1000) return n.toFixed(0)+' ns';
  if(n<1e6) return (n/1e3).toFixed(2)+' µs'; if(n<1e9) return (n/1e6).toFixed(2)+' ms'; return (n/1e9).toFixed(2)+' s'; }
function fmtMs(s){ return (s===''||s==null)?'—':s+' ms'; }
function fmtLagMs(v){ if(v==null) return '-'; v=Number(v);
  if(v>=1000) return (v/1000).toFixed(2)+' s'; return v.toFixed(2)+' ms'; }
// REST-era latency card (2026-07-16). The per-(broker, pull type) table
// iterates the SERVER's j.rest_latency rows — sourced from today's
// rest_fetch_audit on the box (feed + leg discovered from the data, never
// hardcoded), so a future broker/leg gets its row with zero portal changes.
// "-" = no successful pulls recorded today for that pair (never fake zeros).
async function loadLatency(){ $('latnet').dataset.loaded='1'; $('latnet').innerHTML='<span class="muted">measuring…</span>';
  $('latrest').innerHTML='<span class="muted">measuring…</span>'; $('latrestnote').textContent='';
  const j=await call('latency'); if(!j){ $('latnet').innerHTML=''; $('latrest').innerHTML=''; return; }
  const rows=Array.isArray(j.rest_latency)?j.rest_latency:[];
  if(rows.length){
    let h='<table><tr><th>broker</th><th>pull type</th><th>ok pulls today</th><th>p50 after close</th><th>p99 after close</th></tr>';
    rows.forEach(r=>{ h+='<tr><td><b>'+esc(String(r.feed))+'</b></td><td>'+esc(String(r.leg))+'</td>'
      +'<td>'+Number(r.ok_rows||0).toLocaleString()+'</td>'
      +'<td>'+esc(fmtLagMs(r.p50_ms))+'</td><td>'+esc(fmtLagMs(r.p99_ms))+'</td></tr>'; });
    $('latrest').innerHTML=h+'</table>';
  } else {
    $('latrest').innerHTML='<span class="warn">no successful pulls recorded today — market closed, box just started, or the fetch log is unreachable</span>';
  }
  const skew=Math.abs(parseFloat(j.clock_skew_ms));
  $('latnet').innerHTML=
    shield('QuestDB round-trip (box-wide)', fmtMs(j.questdb_ms), true)+
    shield('Clock skew (box-wide)', fmtMs(j.clock_skew_ms), isNaN(skew)||skew<50)+
    shield('Order placement', fmtNs(j.order_place_avg_ns), true); }

// forceId lets each tab keep its own force checkbox next to its own buttons:
// Overview's instance button uses #force_inst, Admin's app controls use #force.
async function act(action,forceId){ if(!confirm('Run "'+action+'" on the trading box?')) return;
  const fEl=$(forceId||'force'); const force=!!(fEl&&fEl.checked); toast(action+'…');
  const j=await call(action,{force}); if(j){ toast('✅ '+action+' sent'); setTimeout(loadOverview,1600); } }

// Danger-zone severity picker → the UNCHANGED per-action functions. Each still
// asks for its OWN typed confirm token (WIPE / NUKE-DOCKER / ERASE) — the
// picker only chooses WHICH prompt fires; there is no token bypass path.
function dangerExecute(){ const sel=document.querySelector('input[name="danger"]:checked');
  if(!sel){ toast('Pick a danger-zone action first'); return; }
  const fn={wipe:wipeData, nuke:dockerReset, erase:bareNuke}[sel.value];
  if(fn) fn(); else toast('Unknown action'); }

async function wipeData(){
  if(prompt('This DELETES every stored market-data row — legacy ticks/candles AND the official minute candles + fetch log for BOTH brokers — then restarts empty. The box must be RUNNING. Type WIPE to confirm:')!=='WIPE'){ toast('cancelled'); return; }
  toast('wiping → fresh start…');
  const j=await call('wipe-questdb',{force:true,confirm:'WIPE'});
  if(j&&j.ok){ toast('✅ wipe started — fresh data from next session'); setTimeout(loadOverview,4000); }
  else { toast((j&&j.error)||'wipe failed — is the box running?'); } }
async function dockerReset(){
  if(prompt('☢ FULL DOCKER NUKE. Stops the app, DELETES Docker containers + volumes + images (full QuestDB wipe — ALL data INCLUDING the SEBI-retention audit tables) + prune, then rebuilds QuestDB fresh + restarts the app. The box must be RUNNING. Type NUKE-DOCKER to confirm:')!=='NUKE-DOCKER'){ toast('cancelled'); return; }
  if(!confirm('Last check: every table is destroyed, including audit history. Continue?')){ toast('cancelled'); return; }
  toast('☢ full docker nuke dispatched → wiping in background (~2-3 min)…');
  const j=await call('docker-reset',{force:$('force').checked,confirm:'NUKE-DOCKER'});
  if(!(j&&j.ok)){ toast((j&&j.error)||'docker-reset failed — is the box running?'); return; }
  if(!j.command_id){ toast('⚠️ nuke dispatched but no command id — re-check the ticks count in ~3 min'); setTimeout(loadOverview,8000); return; }
  pollNuke(j.command_id,0); }
async function bareNuke(){
  if(prompt('☢️ BARE NUKE. Deletes EVERY Docker container + image + volume and LEAVES THE BOX COMPLETELY EMPTY — nothing rebuilt, nothing running, trading OFF until you redeploy (exactly like deleting all three in Docker Desktop). Type ERASE to confirm:')!=='ERASE'){ toast('cancelled'); return; }
  if(!confirm('Final check: the box is left BARE and DEAD (no QuestDB, no app). You will need to redeploy to bring it back. Continue?')){ toast('cancelled'); return; }
  toast('☢️ bare nuke dispatched → erasing everything…');
  const j=await call('docker-nuke-bare',{force:true,confirm:'ERASE'});
  if(!(j&&j.ok)){ toast((j&&j.error)||'bare-nuke failed — is the box running?'); return; }
  if(!j.command_id){ toast('⚠️ dispatched but no command id — re-check the box in ~1 min'); return; }
  pollNuke(j.command_id,0); }
// Poll the REAL nuke outcome instead of claiming success the instant it is
// dispatched. The teardown runs for a minute or two; we show the truthful
// result: complete, FAILED/PARTIAL (something still in-use), or still-running.
async function pollNuke(cid,n){
  if(n>40){ toast('⏳ still running after 3+ min — re-check the box manually'); setTimeout(loadOverview,4000); return; }
  const s=await call('command-status',{command_id:cid}); const st=(s&&s.status)||'', out=(s&&s.stdout_tail)||'';
  if(st==='Success'||st==='Failed'||st==='Cancelled'||st==='TimedOut'){
    if(out.indexOf('DOCKER-RESET-FAILED')>=0){ toast('🔴 NUKE FAILED — QuestDB volume still in use, data NOT wiped. Check the box.'); }
    else if(out.indexOf('bare-nuke-complete')>=0){ toast('☢️ BARE NUKE complete — 0 containers, 0 images, 0 volumes. Box is empty + DEAD (redeploy to restart).'); }
    else if(out.indexOf('bare-nuke-PARTIAL')>=0){ toast('🟠 bare nuke PARTIAL — something is still in-use. '+(out.match(/BARE-NUKE-RESULT[^\n]*/)||[''])[0]); }
    else if(out.indexOf('docker-reset-dispatched')>=0){ toast('✅ nuke complete — empty DB; app restarting'); }
    else { toast('⚠️ nuke finished ('+st+') — re-check the box'); }
    setTimeout(loadOverview,6000); return; }
  setTimeout(()=>pollNuke(cid,n+1),5000); }

function startAuto(){ stopAuto(); if($('auto').checked) timer=setInterval(()=>{ if(curTab==='overview') loadOverview(); },8000); }
function stopAuto(){ if(timer){ clearInterval(timer); timer=null; } }
function autotoggle(){ $('auto').checked?startAuto():stopAuto(); }

// Ready-made link support: a URL ending in #key=<secret> (or ?key=) auto-saves
// the device key, strips it from the address bar, and unlocks — so the Telegram
// link "just works" with zero typing. The fragment never reaches the server.
(function(){ var k=(location.hash.match(/key=([^&]+)/)||[])[1] || new URLSearchParams(location.search).get('key');
  if(k){ try{ k=decodeURIComponent(k); }catch(e){} TOKEN=k; localStorage.setItem('tv_token',TOKEN);
    try{ history.replaceState(null,'',location.pathname); }catch(e){} } })();
if(TOKEN){ $('tok').value=TOKEN; $('lock').hidden=true; $('app').hidden=false; startAuto(); loadOverview(); }
</script>
</body>
</html>"""
