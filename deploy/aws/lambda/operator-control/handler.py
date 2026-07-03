"""Operator portal Lambda — ONE URL to run the whole product (VIEW + CONTROL).

Open the Function URL in a browser and you get a device-locked mission-control
page with THREE tabs (2026-07-02 redesign — operator: "webpage looks completely
messy, too many buttons"):
  * Overview — box status, ticks hero, ticks/sec sparkline, guarantees shields
    (greyed to a single "box stopped" banner when the instance is off), feed
    toggles, a thin AWS strip (spend / alarms / disk, click-to-expand), a
    per-feed latency comparison card (2026-07-03: one row per feed the app
    reports — TCP/TLS probe + last-tick age + windowed ticks/sec + subscribed
    counts, best value per column highlighted; box-wide cards labeled so), and
    ONE context-aware Start/Stop instance button.
  * Data — candles per timeframe, the daily cross-verify result, and the
    READ-ONLY database console (tables + columns + query grid + CSV download —
    writes blocked server-side).
  * Admin — restart/stop app + QuestDB, a COLLAPSED danger zone (severity
    picker over the four wipes, each with its own typed confirm token), and
    the device lock.
The former GitHub + Logs tabs are gone. The `logs` API action is KEPT (the
tickvault-logs MCP server POSTs {"action":"logs"}); the gh_* actions had no
caller outside the deleted tab and are removed.

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
* DATA-DESTRUCTIVE actions (wipe-questdb/wipe-groww/docker-reset/
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
_DESTRUCTIVE = {"stop", "reboot", "restart-app", "stop-app", "wipe-questdb", "wipe-groww", "docker-reset", "docker-nuke-bare"}

# HARD LOCK — the DATA-DESTRUCTIVE subset of _DESTRUCTIVE: actions that DELETE
# market data which can NEVER be re-fetched from upstream (Dhan/Groww deliver
# live ticks once; a wiped window is gone forever). During market hours
# (09:15-15:30 IST Mon-Fri) these are REFUSED with 409 EVEN WHEN force=true —
# audit fix #2 (operator incident 2026-07-02 15:05 IST: a mid-market forced
# wipe-ALL + docker-reset deleted the QuestDB volume — ~4.5M rows wiped, 77s
# of feed darkness, upstream ticks unrecoverable). Lifecycle actions
# (stop / reboot / restart-app / stop-app) deliberately KEEP the force
# override above so emergencies stay possible.
_DATA_DESTRUCTIVE = {"wipe-questdb", "wipe-groww", "docker-reset", "docker-nuke-bare"}

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
# Latency probes run metrics-scrape-T0 + feeds-health-T0 + PARALLEL per-feed
# TCP/TLS probes (2 samples × --max-time 2, all feeds concurrently) + QuestDB
# + a 4s window floor + metrics-scrape-T1 + feeds-health-T1 ON the box.
# Worst case ≈ 3+4+4+3+4+3+4 = 25s; typical ~7-9s. Lambda timeout is 30s,
# so 26s leaves margin. Per-curl --max-time bounds every network leg — one
# dead feed endpoint can never hang the whole measure (probes are parallel).
_LATENCY_TIMEOUT_SECS = 26.0

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


_VIEW_COMMANDS = [
    "set +e",
    'echo "APP=$(systemctl is-active tickvault 2>/dev/null || echo inactive)"',
    "Q='http://127.0.0.1:9000/exp?query='",
    'echo "TICKS_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20ticks%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    # Per-feed split of today's ticks (operator portal feed card). /exp emits a
    # CSV header line + one row per feed ("dhan",123) — skip the header and
    # join rows with ';' so the labeled-line parser sees ONE line.
    'echo "TICKS_BY_FEED=$(curl -fsS "${Q}SELECT%20feed%2C%20count()%20FROM%20ticks%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20feed" 2>/dev/null | tail -n +2 | tr \'\\n\' \';\')"',
    'echo "C1M=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_1m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "C5M=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_5m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "C15M=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_15m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "C60M=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_60m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "C1D=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_1d%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    # NOTE: the `=` in `upsertKey=true` MUST be URL-encoded as %3D — it is the
    # ONLY view query carrying a raw `=` inside the ?query= value, which the
    # QuestDB /exp query-string parser mis-handled, returning empty so the
    # dashboard "Dedup key columns" panel showed "?". Encoded form = a clean
    # `count` of the 5 upsert-key columns — the REAL `ticks` DEDUP key is
    # (ts, security_id, segment, capture_seq, feed) per DEDUP_KEY_TICKS in
    # crates/storage/src/tick_persistence.rs (designated ts is always an
    # upsertKey column; received_at/payload_hash are stored columns only).
    'echo "DEDUP_KEYS=$(curl -fsS "${Q}SELECT%20count()%20FROM%20table_columns(%27ticks%27)%20WHERE%20upsertKey%3Dtrue" 2>/dev/null | tail -1)"',
    'echo "MAX_TPS=$(curl -fsS "${Q}SELECT%20max(c)%20FROM%20(SELECT%20count()%20c%20FROM%20ticks%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20ts,security_id)" 2>/dev/null | tail -1)"',
    # Tick-conservation shield inputs (audit fix #1, 2026-07-02 — kills the
    # false-OK ">1 tick/sec == no tick lost" heuristic):
    #  * CONSERVE — the latest tick_conservation_audit rows (last ~2 days, per
    #    feed; the 15:40 IST daily audit per
    #    .claude/rules/project/tick-conservation-audit-error-codes.md). CSV
    #    header skipped, rows ';'-joined (same convention as TICKS_BY_FEED).
    #    Table absent on a fresh box → curl -f fails → empty value → the UI
    #    shows "no audit yet", NEVER a fabricated green.
    #  * WS_DISC — count of TODAY's in-market socket drops. event_kind
    #    'disconnected' is the app's own market-hours disconnect class
    #    (off-hours drops are stamped 'disconnected_off_hours' — see
    #    crates/common/src/ws_event_types.rs), so this IS the 09:15-15:30 IST
    #    filter, applied at write time by the app itself. Any such drop means
    #    upstream ticks in that window never reached the box — the shield can
    #    never claim green "no tick lost" for today. `=` encoded as %3D per
    #    the DEDUP_KEYS lesson above.
    'echo "CONSERVE=$(curl -fsS "${Q}SELECT%20feed%2C%20trading_date_ist%2C%20delivery_residual%2C%20outcome_residual%2C%20partial_coverage%2C%20outcome%20FROM%20tick_conservation_audit%20WHERE%20ts%20%3E%20dateadd(%27d%27%2C%20-2%2C%20now())%20ORDER%20BY%20ts%20DESC%20LIMIT%208" 2>/dev/null | tail -n +2 | tr \'\\n\' \';\')"',
    'echo "WS_DISC=$(curl -fsS "${Q}SELECT%20count()%20FROM%20ws_event_audit%20WHERE%20ts%20IN%20today()%20AND%20event_kind%3D%27disconnected%27" 2>/dev/null | tail -1)"',
    'echo "ERRORS_BEGIN"',
    "journalctl -u tickvault -p err -n 5 --no-pager 2>/dev/null | tail -5 || true",
    'echo "ERRORS_END"',
]


def _parse_feed_counts(raw: str) -> dict:
    """Parse the TICKS_BY_FEED value — ';'-joined QuestDB CSV rows like
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
    return {
        "app": fields.get("APP", ""),
        "ticks_today": fields.get("TICKS_TODAY", ""),
        "ticks_by_feed": _parse_feed_counts(fields.get("TICKS_BY_FEED", "")),
        "candles": {
            "1m": fields.get("C1M", ""),
            "5m": fields.get("C5M", ""),
            "15m": fields.get("C15M", ""),
            "60m": fields.get("C60M", ""),
            "1d": fields.get("C1D", ""),
        },
        "dedup_key_columns": fields.get("DEDUP_KEYS", ""),
        "max_ticks_per_second": fields.get("MAX_TPS", ""),
        "conservation_rows": _parse_conserve_rows(fields.get("CONSERVE", "")),
        "ws_disconnects_today": fields.get("WS_DISC", ""),
        "recent_errors": errors,
    }


# ------------------------------------------------- tick-conservation shield
# Audit fix #1 (2026-07-02): the old "No tick lost" shield was a >1-tick/sec
# LIVENESS heuristic — it showed WORKING ✅ through a mid-market crash-loop
# where upstream ticks were genuinely lost (a false-OK, banned by
# audit-findings Rule 11). The shield is now a 3-source honest composite:
#   a. the 15:40 IST tick_conservation_audit verdict (balanced/leak/partial +
#      the two residuals) — the end-to-end WAL-vs-processed-vs-outcome proof
#      (.claude/rules/project/tick-conservation-audit-error-codes.md);
#   b. TODAY's in-market ws_event_audit 'disconnected' count — any drop means
#      Dhan-sent ticks in that window never REACHED the box, so green is
#      impossible for today (the honest capture-at-receipt envelope);
#   c. live ticks/sec — kept ONLY as a decorating label when a+b are clean.
# Classification precedence (ratcheted by test_handler.py):
#   residual (RED) > partial (AMBER) > disconnects-today (AMBER) > balanced
#   (GREEN) > no-audit/idle/unreachable. A tick rate ALONE can never go green.

# The honest-envelope sentence surfaced as the shield tooltip (task 3).
_CONSERVATION_ENVELOPE = (
    "Proves every tick that REACHED the box is stored. Ticks Dhan sent during "
    "a disconnect window never arrived and are outside this proof."
)


def _parse_conserve_rows(raw: str) -> list:
    """Parse the CONSERVE value — ';'-joined QuestDB /exp CSV rows like
    '"dhan","2026-07-01T00:00:00.000000Z",0,0,false,"balanced"' — into a list
    of dicts. Pure. Malformed fragments are skipped; empty/absent input (table
    missing on a fresh box, QuestDB down) yields [] — the classifier then
    reports "no audit yet"/"unreachable", never a fabricated verdict."""
    rows: list = []
    for part in (raw or "").split(";"):
        part = part.strip()
        if not part:
            continue
        cells = [c.strip().strip('"') for c in part.split(",")]
        if len(cells) != 6:
            continue
        feed, tdate, dres, ores, partial, outcome = cells
        try:
            drow = {
                "feed": feed,
                "date": tdate[:10],  # trading_date_ist → YYYY-MM-DD
                "delivery_residual": int(dres),
                "outcome_residual": int(ores),
                "partial_coverage": partial.lower() == "true",
                "outcome": outcome.lower(),
            }
        except ValueError:
            continue
        if drow["feed"] and len(drow["date"]) == 10:
            rows.append(drow)
    return rows


def _classify_tick_conservation(rows: list, disconnects_raw: str, market_hours: bool, max_tps_raw: str) -> dict:
    """The shield's state machine (pure — one unit test per state).

    | state        | colour | when                                              |
    |--------------|--------|---------------------------------------------------|
    | residual     | RED    | latest audit day has residual>0 or outcome=leak   |
    | partial      | AMBER  | latest audit day has partial_coverage/partial     |
    | disconnects  | AMBER  | ≥1 in-market socket drop TODAY (ws_event_audit)   |
    | balanced     | GREEN  | audit balanced AND zero drops today               |
    | no_audit     | AMBER  | market open, no audit row yet — NEVER green       |
    | idle         | grey-ok| market closed, no audit row, QuestDB reachable    |
    | unreachable  | AMBER  | neither source answered (QuestDB/box down)        |

    Only the latest trading day's rows count (one per feed, newest first —
    the CONSERVE query is ORDER BY ts DESC). tps decorates the balanced label
    only; it can no longer produce green by itself (the old false-OK)."""
    try:
        disconnects = int(str(disconnects_raw or "").strip())
    except ValueError:
        disconnects = None
    try:
        tps = int(str(max_tps_raw or "").strip())
    except ValueError:
        tps = 0

    latest: list = []
    if rows:
        latest_date = max(r["date"] for r in rows)
        seen_feeds: set = set()
        for r in rows:  # newest first — keep the latest row per feed
            if r["date"] == latest_date and r["feed"] not in seen_feeds:
                seen_feeds.add(r["feed"])
                latest.append(r)

    def _result(state: str, label: str, good: bool, cls: str) -> dict:
        return {
            "state": state,
            "label": label,
            "good": good,
            "cls": cls,
            "audit_date": latest[0]["date"] if latest else "",
            "disconnects_today": disconnects,
            "envelope": _CONSERVATION_ENVELOPE,
        }

    # 1. RESIDUAL — ticks delivered to the box but unaccounted. Hard red.
    residual_rows = [
        r
        for r in latest
        if r["delivery_residual"] > 0 or r["outcome_residual"] > 0 or r["outcome"] == "leak"
    ]
    if residual_rows:
        n = sum(max(r["delivery_residual"], 0) + max(r["outcome_residual"], 0) for r in residual_rows)
        return _result("residual", f"RESIDUAL: {n} ❌ ({latest[0]['date']})", False, "bad")
    # 2. PARTIAL — the audit could not vouch for the full session (mid-day boot
    #    / a source missing). Honest "cannot vouch", never a false OK.
    if any(r["partial_coverage"] or r["outcome"] == "partial" for r in latest):
        return _result("partial", f"partial coverage ⚠ — mid-day restart ({latest[0]['date']})", False, "warn")
    # 3. DISCONNECTS TODAY — socket-down evidence outranks yesterday's clean
    #    audit: upstream ticks in those windows are outside the capture proof.
    if disconnects is not None and disconnects > 0:
        return _result(
            "disconnects",
            f"{disconnects} disconnect(s) today ⚠ — upstream ticks in those windows are unverifiable",
            False,
            "warn",
        )
    # 4. BALANCED — audit vouches, zero drops today. Liveness decorates only.
    if latest:
        if market_hours and tps > 1:
            return _result("balanced", f"BALANCED ✅ · capturing ({tps} ticks/sec)", True, "ok")
        return _result("balanced", f"BALANCED ✅ (audit {latest[0]['date']})", True, "ok")
    # 5. No audit rows at all.
    if disconnects is None:
        # Neither source answered — QuestDB (or the box) is unreachable.
        return _result("unreachable", "unreachable", False, "warn")
    if not market_hours:
        return _result("idle", "idle (market closed)", True, "ok")
    return _result("no_audit", "no audit yet ⚠ (daily audit runs 15:40 IST)", False, "warn")


# ----------------------------------------------------------------- latency snap
# Measures the REAL per-feed + box-wide latency on the box (operator demand
# 2026-07-03: dhan + groww run side by side — show "which is extremely faster
# and more precise" PER FEED, auto-extending to any future feed):
#  * PER FEED — network RTT to that feed's live edge (TCP connect + TLS
#    handshake, parallel probes), plus the app's own per-feed reporting from
#    GET /api/feeds/health (last-tick age, subscribed counts) curled TWICE so
#    ticks/sec per feed = Δticks_total over the measured window. The feed
#    LIST is the app's health response — never hardcoded here.
#  * BOX-WIDE — local QuestDB round-trip (SELECT 1), wall-clock skew vs
#    chrony, and the app's histograms at :9091/metrics (tick parse+process /
#    wire->done / order placement). These carry NO feed label in the app
#    (tick_processor.rs emits them unlabeled), so they are HONESTLY presented
#    as box-wide — no fake per-feed processing splits.
#
# Feed → live-edge host map for the network probe ONLY. The table rows come
# from the app's /api/feeds/health; a feed reported by the app but missing
# here still gets its row — its network cells read "endpoint unknown" (never
# fake numbers). Hosts are code-controlled constants (shell-safe by
# construction; the app's response is never interpolated into commands).
#   dhan:  wss://api-feed.dhan.co (main feed, 2-WS lock)
#   groww: wss://socket-api.groww.in (NATS-over-WSS edge, docs/groww-ref/02)
_FEED_LIVE_HOSTS = {
    "dhan": "api-feed.dhan.co",
    "groww": "socket-api.groww.in",
}
_FEED_PROBE_SAMPLES = 2
_FEED_PROBE_MAX_SECS = 2


def _latency_commands() -> list[str]:
    """Build the on-box latency snapshot script (pure — static inputs only).

    Windowed-percentile design (operator directive 2026-06-10): scrape the
    metrics endpoint TWICE — once BEFORE and once AFTER the network probes —
    and let the Lambda compute p50/p99 + avg from the histogram-bucket DELTAS
    between the two scrapes. The same T0/T1 bracketing now also wraps TWO
    /api/feeds/health scrapes so per-feed ticks/sec is a real windowed delta.

    Per-feed TCP/TLS probes run as PARALLEL background jobs (`( … ) &` +
    `wait`), each curl bounded by --max-time — a single dead feed endpoint
    costs at most samples×max-time and never delays the other feeds. Probe
    hosts come exclusively from the static _FEED_LIVE_HOSTS map above (feed
    names are code-controlled lowercase tokens — safe in paths/markers)."""
    metrics_curl = (
        "curl -fsS --max-time 3 http://127.0.0.1:9091/metrics 2>/dev/null | "
        "grep -E '^tv_(tick_processing|wire_to_done|order_placement|tick_flush)_duration_ns' "
        "|| echo none"
    )
    health_curl = (
        "curl -fsS --max-time 4 http://127.0.0.1:3001/api/feeds/health 2>/dev/null "
        "|| echo TV_CURL_FAILED; echo"
    )
    cmds = [
        "set +e",
        'echo "T0=$(date +%s.%N)"',
        'echo "METRICS_T0_BEGIN"',
        metrics_curl,
        'echo "METRICS_T0_END"',
        'echo "FEEDS_T0_BEGIN"',
        health_curl,
        'echo "FEEDS_T0_END"',
    ]
    samples = " ".join(str(i + 1) for i in range(_FEED_PROBE_SAMPLES))
    for feed, host in sorted(_FEED_LIVE_HOSTS.items()):
        cmds.append(
            f"(for i in {samples}; do "
            f"curl -o /dev/null -s -w '%{{time_connect}} %{{time_appconnect}}\\n' "
            f"--max-time {_FEED_PROBE_MAX_SECS} https://{host}/ 2>/dev/null || echo 'x x'; "
            f"done > /tmp/tv-probe-{feed}.txt 2>/dev/null) &"
        )
    cmds.append("wait")
    for feed in sorted(_FEED_LIVE_HOSTS):
        cmds += [
            f'echo "PROBE_{feed}_BEGIN"',
            f"cat /tmp/tv-probe-{feed}.txt 2>/dev/null",
            f'echo "PROBE_{feed}_END"',
            f"rm -f /tmp/tv-probe-{feed}.txt",
        ]
    cmds += [
        "echo \"QDB=$(curl -o /dev/null -s -w '%{time_total}' --max-time 3 'http://127.0.0.1:9000/exec?query=SELECT%201' 2>/dev/null)\"",
        "echo \"SKEW=$(chronyc tracking 2>/dev/null | awk '/Last offset/{print $4}')\"",
        # Floor the window at ~4 s so fast-failing probes (network down →
        # instant curl errors) still leave a meaningful sample window.
        "sleep 4",
        'echo "T1=$(date +%s.%N)"',
        'echo "METRICS_BEGIN"',
        metrics_curl,
        'echo "METRICS_END"',
        'echo "FEEDS_T1_BEGIN"',
        health_curl,
        'echo "FEEDS_T1_END"',
    ]
    return cmds


_LATENCY_COMMANDS = _latency_commands()


def _avg_ns(sum_v: str, count_v: str) -> float | None:
    """sum/count -> average nanoseconds, or None if no samples. Pure."""
    try:
        s = float(sum_v)
        c = float(count_v)
        return s / c if c > 0 else None
    except (TypeError, ValueError):
        return None


def _parse_bucket_lines(lines: list[str]) -> dict:
    """Parse Prometheus exposition lines into {metric: {"buckets": {le: count},
    "sum": float, "count": float}}. Pure. Unparseable lines are skipped."""
    out: dict = {}
    for line in lines:
        bits = line.split()
        if len(bits) != 2:
            continue
        name, raw_val = bits
        try:
            val = float(raw_val)
        except ValueError:
            continue
        if "_bucket{" in name:
            base = name.split("_bucket{", 1)[0]
            # le label, e.g. tv_x_duration_ns_bucket{le="10000"} or le="+Inf"
            le_part = name.split('le="', 1)
            if len(le_part) != 2:
                continue
            le_raw = le_part[1].split('"', 1)[0]
            le = float("inf") if le_raw == "+Inf" else None
            if le is None:
                try:
                    le = float(le_raw)
                except ValueError:
                    continue
            out.setdefault(base, {"buckets": {}, "sum": 0.0, "count": 0.0})
            out[base]["buckets"][le] = val
        elif name.endswith("_sum"):
            base = name[: -len("_sum")]
            out.setdefault(base, {"buckets": {}, "sum": 0.0, "count": 0.0})
            out[base]["sum"] = val
        elif name.endswith("_count"):
            base = name[: -len("_count")]
            out.setdefault(base, {"buckets": {}, "sum": 0.0, "count": 0.0})
            out[base]["count"] = val
    return out


def _bucket_deltas(t0: dict, t1: dict) -> dict | None:
    """Per-bucket cumulative-count deltas between two scrapes of ONE metric.
    Returns {"buckets": {le: delta}, "sum": dsum, "count": dcount} or None
    when the window is empty/invalid (no ticks, or counter reset between
    scrapes — any negative delta invalidates the whole window). Pure."""
    if not t0 or not t1:
        return None
    d_count = t1.get("count", 0.0) - t0.get("count", 0.0)
    d_sum = t1.get("sum", 0.0) - t0.get("sum", 0.0)
    if d_count <= 0 or d_sum < 0:
        return None
    deltas: dict = {}
    for le, c1 in t1.get("buckets", {}).items():
        c0 = t0.get("buckets", {}).get(le, 0.0)
        d = c1 - c0
        if d < 0:
            return None  # counter reset mid-window — whole window invalid
        deltas[le] = d
    if not deltas:
        return None
    return {"buckets": deltas, "sum": d_sum, "count": d_count}


def _percentile_from_bucket_deltas(deltas: dict | None, q: float) -> float | None:
    """Quantile (0<q<1) from windowed cumulative-bucket deltas, linear
    interpolation inside the owning bucket. A quantile landing in the +Inf
    bucket clamps to the last finite bound (honest 'off the scale'). Pure."""
    if not deltas:
        return None
    total = deltas.get("count", 0.0)
    if total <= 0:
        return None
    target = q * total
    prev_bound = 0.0
    prev_cum = 0.0
    for le in sorted(deltas["buckets"].keys()):
        cum = deltas["buckets"][le]
        if cum >= target:
            if le == float("inf"):
                return prev_bound  # clamp: off the top of the scale
            width = cum - prev_cum
            if width <= 0:
                return le
            frac = (target - prev_cum) / width
            return prev_bound + (le - prev_bound) * frac
        prev_bound = 0.0 if le == float("inf") else le
        prev_cum = cum
    return prev_bound


def _feed_comparison_winners(rows: list) -> dict:
    """column -> feed name holding the STRICTLY best value (pure).

    Lower is better for tcp_ms / tls_ms / last_tick_age_secs; higher is
    better for ticks_per_sec / subscribed_total. A winner is only declared
    when at least TWO feeds have a comparable value AND the best is strictly
    unique — a tie, a single-feed measure, or unknown values yield NO winner
    (an honest comparison needs something real to compare against)."""
    lower_better = ("tcp_ms", "tls_ms", "last_tick_age_secs")
    higher_better = ("ticks_per_sec", "subscribed_total")
    winners: dict[str, str] = {}
    for col in lower_better + higher_better:
        vals: list[tuple[float, str]] = []
        for r in rows:
            try:
                vals.append((float(r.get(col)), str(r.get("feed"))))
            except (TypeError, ValueError):
                continue
        if len(vals) < 2:
            continue
        vals.sort(key=lambda x: x[0], reverse=col in higher_better)
        if vals[0][0] == vals[1][0]:
            continue  # tie — no single winner
        winners[col] = vals[0][1]
    return winners


def _feed_latency_rows(health_t0: object, health_t1: object, probes: dict, window: str) -> list:
    """Join the app's per-feed health rows with the on-box network probes
    (pure). One output row per feed the APP reports (never hardcoded);
    probe columns come from _FEED_LIVE_HOSTS-keyed samples — a feed with no
    host mapping gets endpoint_known=False and blank probe cells. ticks/sec
    is the Δticks_total between the two health scrapes over the window;
    None when either scrape is missing or the counter reset (app restart)."""

    def feed_map(payload: object) -> dict:
        out: dict[str, dict] = {}
        if isinstance(payload, dict) and isinstance(payload.get("feeds"), list):
            for r in payload["feeds"]:
                if isinstance(r, dict) and r.get("feed"):
                    out[str(r["feed"])] = r
        return out

    def min_ms(values: list[float]) -> str:
        vals = [v for v in values if v > 0]
        return f"{min(vals) * 1000:.1f}" if vals else ""

    t0_rows = feed_map(health_t0)
    t1_rows = feed_map(health_t1)
    # T1 is authoritative (post-window state); fall back to T0 when the
    # second scrape failed so the operator still sees the rows.
    base = t1_rows or t0_rows
    rows: list = []
    for name, r in base.items():
        samples = probes.get(name, [])
        tps = None
        try:
            w = float(window)
            delta = float(t1_rows[name].get("ticks_total")) - float(
                t0_rows[name].get("ticks_total")
            )
            if w > 0 and delta >= 0:
                tps = round(delta / w, 1)
        except (TypeError, ValueError, KeyError):
            tps = None
        rows.append(
            {
                "feed": name,
                "endpoint": _FEED_LIVE_HOSTS.get(name),
                "endpoint_known": name in _FEED_LIVE_HOSTS,
                "tcp_ms": min_ms([s[0] for s in samples]),
                "tls_ms": min_ms([s[1] for s in samples]),
                "last_tick_age_secs": r.get("last_tick_age_secs"),
                "ticks_total": r.get("ticks_total"),
                "ticks_per_sec": tps,
                "subscribed_total": r.get("subscribed_total"),
                "verdict": r.get("verdict"),
                "enabled": r.get("enabled"),
            }
        )
    return rows


def _parse_latency(stdout: str) -> dict:
    """Parse the labeled latency snapshot into a structured dict (pure).

    Windowed-percentile design (2026-06-10): the box emits TWO metric
    scrapes — METRICS_T0 (before the network probes) and METRICS (after).
    Lifetime averages come from the second scrape (back-compat); the
    p50/p99 + windowed averages come from the bucket DELTAS between them.

    Per-feed design (2026-07-03): PROBE_<feed>_BEGIN/END blocks carry each
    feed's TCP/TLS samples, and FEEDS_T0/FEEDS_T1 blocks carry the app's
    /api/feeds/health JSON at both window edges — joined into one comparison
    row per feed the app reports, plus a winners map for the green highlight.
    """
    probes: dict[str, list[tuple[float, float]]] = {}
    metrics: dict[str, str] = {}
    t0_lines: list[str] = []
    t1_lines: list[str] = []
    feeds_t0_lines: list[str] = []
    feeds_t1_lines: list[str] = []
    fields: dict[str, str] = {}
    mode = ""
    probe_feed = ""
    for line in (stdout or "").splitlines():
        if line in ("METRICS_BEGIN", "METRICS_T0_BEGIN", "FEEDS_T0_BEGIN", "FEEDS_T1_BEGIN"):
            mode = {
                "METRICS_BEGIN": "METRICS",
                "METRICS_T0_BEGIN": "T0",
                "FEEDS_T0_BEGIN": "FEEDS_T0",
                "FEEDS_T1_BEGIN": "FEEDS_T1",
            }[line]
            continue
        if line in ("METRICS_END", "METRICS_T0_END", "FEEDS_T0_END", "FEEDS_T1_END"):
            mode = ""
            continue
        if line.startswith("PROBE_") and line.endswith("_BEGIN"):
            probe_feed = line[len("PROBE_") : -len("_BEGIN")]
            probes.setdefault(probe_feed, [])
            mode = "PROBE"
            continue
        if line.startswith("PROBE_") and line.endswith("_END"):
            mode = ""
            probe_feed = ""
            continue
        if mode == "PROBE":
            parts = line.split()
            if len(parts) == 2:
                try:
                    probes[probe_feed].append((float(parts[0]), float(parts[1])))
                except ValueError:
                    pass  # 'x x' sentinel — failed sample, honestly skipped
            continue
        if mode == "T0":
            t0_lines.append(line)
            continue
        if mode == "FEEDS_T0":
            feeds_t0_lines.append(line)
            continue
        if mode == "FEEDS_T1":
            feeds_t1_lines.append(line)
            continue
        if mode == "METRICS":
            # e.g. "tv_tick_processing_duration_ns_sum 1.23e6"
            t1_lines.append(line)
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

    def avg(name: str):
        a = _avg_ns(metrics.get(name + "_sum", ""), metrics.get(name + "_count", ""))
        return round(a, 1) if a is not None else None

    # Windowed stats from bucket deltas between the two scrapes. None when
    # the window saw no samples (market closed) or a counter reset — the
    # dashboard falls back to the lifetime averages in that case.
    t0 = _parse_bucket_lines(t0_lines)
    t1 = _parse_bucket_lines(t1_lines)

    def windowed(name: str) -> tuple[float | None, float | None, float | None, float]:
        d = _bucket_deltas(t0.get(name, {}), t1.get(name, {}))
        if d is None:
            return (None, None, None, 0.0)
        p50 = _percentile_from_bucket_deltas(d, 0.50)
        p99 = _percentile_from_bucket_deltas(d, 0.99)
        w_avg = d["sum"] / d["count"] if d["count"] > 0 else None
        return (
            round(p50, 1) if p50 is not None else None,
            round(p99, 1) if p99 is not None else None,
            round(w_avg, 1) if w_avg is not None else None,
            d["count"],
        )

    tick_p50, tick_p99, tick_wavg, tick_wcount = windowed("tv_tick_processing_duration_ns")
    wire_p50, wire_p99, wire_wavg, _ = windowed("tv_wire_to_done_duration_ns")
    flush_p50, flush_p99, flush_wavg, _ = windowed("tv_tick_flush_duration_ns")

    def window_secs() -> str:
        try:
            return f"{float(fields.get('T1', '')) - float(fields.get('T0', '')):.1f}"
        except (TypeError, ValueError):
            return ""

    def health_json(lines: list[str]) -> tuple[object, str]:
        raw = "\n".join(lines).strip()
        if not raw or "TV_CURL_FAILED" in raw:
            return None, _FEED_API_UNREACHABLE
        try:
            return json.loads(raw), ""
        except (json.JSONDecodeError, ValueError):
            return None, "app API returned invalid JSON"

    health_t0, err_t0 = health_json(feeds_t0_lines)
    health_t1, err_t1 = health_json(feeds_t1_lines)
    feed_rows = _feed_latency_rows(health_t0, health_t1, probes, window_secs())

    return {
        # Per-feed comparison (operator demand 2026-07-03): one row per feed
        # the APP reports, with the on-box probe joined in. Empty + error set
        # when the app was unreachable on the box — never fabricated rows.
        "feeds": feed_rows,
        "feeds_error": "" if feed_rows else (err_t1 or err_t0 or ""),
        "winners": _feed_comparison_winners(feed_rows),
        "questdb_ms": sec_to_ms(fields.get("QDB", "")),
        "clock_skew_ms": sec_to_ms(fields.get("SKEW", "")),
        "tick_process_avg_ns": avg("tv_tick_processing_duration_ns"),
        "wire_to_done_avg_ns": avg("tv_wire_to_done_duration_ns"),
        "order_place_avg_ns": avg("tv_order_placement_duration_ns"),
        "tick_count": metrics.get("tv_tick_processing_duration_ns_count", ""),
        # Windowed precise latencies (operator directive 2026-06-10) —
        # computed over the probe window (~10 s), None when no live ticks.
        "window_secs": window_secs(),
        "tick_window_count": int(tick_wcount),
        "tick_p50_ns": tick_p50,
        "tick_p99_ns": tick_p99,
        "tick_window_avg_ns": tick_wavg,
        "wire_p50_ns": wire_p50,
        "wire_p99_ns": wire_p99,
        "wire_window_avg_ns": wire_wavg,
        "flush_p50_ns": flush_p50,
        "flush_p99_ns": flush_p99,
        "flush_window_avg_ns": flush_wavg,
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


# ------------------------------------------------------------ cross-verify snap
# The 15:31 IST daily candle check vs the exchange record (visibility
# directive 2026-06-10). Reads the app's read-only endpoint on the box and
# re-emits ONLY the summary as labeled lines — the mismatch CSV itself stays
# on the box (SSM command output is size-capped). Read-only; degrades to
# blank fields when the box/app is down or no run has happened yet.
_CROSS_VERIFY_COMMANDS = [
    "set +e",
    (
        "curl -fsS --max-time 4 http://127.0.0.1:3001/api/debug/cross-verify/latest 2>/dev/null | "
        'python3 -c \'import json,sys; d=json.load(sys.stdin); s=d.get("summary") or {}; '
        'print("CV_DATE="+str(d.get("date",""))); '
        'print("CV_MISMATCH_ROWS="+str(d.get("mismatch_rows",""))); '
        'print("CV_INSTRUMENTS="+str(s.get("instruments_checked",""))); '
        'print("CV_COMPARED="+str(s.get("compared",""))); '
        'print("CV_MISSING="+str(s.get("missing_ours",""))); '
        'print("CV_DEGRADED="+str(s.get("degraded","")))\' '
        "2>/dev/null || echo CV_DATE="
    ),
]


def _parse_cross_verify(stdout: str) -> dict:
    """Parse the labeled cross-verify snapshot into a dict (pure). Blank
    fields mean: box stopped, app down, or no run yet — the card shows a
    truthful 'no run yet', never a fabricated PASS."""
    f: dict[str, str] = {}
    for line in (stdout or "").splitlines():
        if "=" in line:
            k, _, v = line.partition("=")
            f[k.strip()] = v.strip()
    return {
        "date": f.get("CV_DATE", ""),
        "mismatch_rows": f.get("CV_MISMATCH_ROWS", ""),
        "instruments": f.get("CV_INSTRUMENTS", ""),
        "compared": f.get("CV_COMPARED", ""),
        "missing": f.get("CV_MISSING", ""),
        "degraded": f.get("CV_DEGRADED", ""),
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
]
# Two 8s curls + SSM registration need more than the 6s view window.
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


def _parse_feeds_view(stdout: str) -> dict:
    """Parse the marked feeds-view snapshot (pure). On any failure the matching
    *_error field carries a structured reason and the payload is None — the UI
    renders the error verbatim instead of defaulting to zeros."""
    if not (stdout or "").strip():
        return {
            "feeds": None,
            "feeds_error": _BOX_UNREACHABLE,
            "health": None,
            "health_error": _BOX_UNREACHABLE,
        }
    feeds, feeds_error = _extract_marked_json(stdout, "FEEDS_BEGIN", "FEEDS_END")
    health, health_error = _extract_marked_json(stdout, "FEEDS_HEALTH_BEGIN", "FEEDS_HEALTH_END")
    return {"feeds": feeds, "feeds_error": feeds_error, "health": health, "health_error": health_error}


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


# --------------------------------------------------------------- groww-only wipe
# Surgical per-feed wipe (operator demand 2026-07-02: delete every groww row
# WITHOUT touching dhan data or the SEBI audit tables). QuestDB cannot DELETE
# rows and groww+dhan share partitions, so removal = a per-table REWRITE:
#   CREATE <t>_gwipe with the EXACT canonical DDL (mirrored from
#   crates/storage/src/tick_persistence.rs::TICKS_CREATE_DDL — 19 cols,
#   TIMESTAMP(ts) PARTITION BY HOUR WAL, DEDUP UPSERT KEYS(ts, security_id,
#   segment, capture_seq, feed) — and crates/storage/src/shadow_persistence.rs
#   — 15 cols, timestamp(ts) PARTITION BY DAY, DEDUP UPSERT KEYS(ts,
#   security_id, segment, feed))
#   → INSERT the kept rows (feed != 'groww' OR feed IS NULL — NULL-feed rows
#     are legacy dhan rows and MUST be kept; explicit column lists make the
#     copy immune to live column-ORDER drift from historical ALTER ADDs)
#   → VERIFY count(new) == count(old) - count(groww) BEFORE any DROP (WAL
#     apply is async — poll up to 120s)
#   → only then DROP old + RENAME new into place.
# Mid-rewrite failure safety: the ORIGINAL table is NEVER dropped until the
# copy verified; any failure aborts THAT table (original untouched, temp
# dropped) and the run ends GROWW-WIPE-PARTIAL — never a fake OK.
# SCOPE LOCK: market-data tables ONLY (ticks + dynamically-discovered
# candles_*). The SEBI never-delete tables (instrument lifecycle + every
# *_audit + index constituency + prev-day OHLCV) are NOT in the target filter.
# Dhan replay sources (data/ws_wal, spill, dlq, instrument-cache) are NOT
# removed — only the groww capture dir (capture file + status + bridge
# flushed-offset snapshot), the proven resurrection vector, BEFORE restart.
_GROWW_WIPE_PY = """
import json, time, urllib.request, urllib.parse

BASE = 'http://127.0.0.1:9000/exec?query='

def q(sql, timeout=60):
    with urllib.request.urlopen(BASE + urllib.parse.quote(sql), timeout=timeout) as r:
        return json.load(r)

def one(sql):
    ds = q(sql).get('dataset') or []
    if not ds or not ds[0] or ds[0][0] is None:
        return 0
    return int(ds[0][0])

TICKS_COLS = ['feed','segment','security_id','ltp','open','high','low','close','volume','oi','avg_price','last_trade_qty','total_buy_qty','total_sell_qty','exchange_timestamp','received_at','payload_hash','capture_seq','ts']
TICKS_TYPES = {'feed':'SYMBOL','segment':'SYMBOL','security_id':'LONG','ltp':'DOUBLE','open':'DOUBLE','high':'DOUBLE','low':'DOUBLE','close':'DOUBLE','volume':'LONG','oi':'LONG','avg_price':'DOUBLE','last_trade_qty':'LONG','total_buy_qty':'LONG','total_sell_qty':'LONG','exchange_timestamp':'LONG','received_at':'TIMESTAMP','payload_hash':'LONG','capture_seq':'LONG','ts':'TIMESTAMP'}
CANDLE_COLS = ['feed','segment','security_id','ts','open','high','low','close','volume','oi','tick_count','close_pct_from_prev_day','open_pct','change_pct','open_gap_pct']
CANDLE_TYPES = {'feed':'SYMBOL','segment':'SYMBOL','security_id':'LONG','ts':'TIMESTAMP','open':'DOUBLE','high':'DOUBLE','low':'DOUBLE','close':'DOUBLE','volume':'LONG','oi':'LONG','tick_count':'LONG','close_pct_from_prev_day':'DOUBLE','open_pct':'DOUBLE','change_pct':'DOUBLE','open_gap_pct':'DOUBLE'}

def ddl(new, cols, types, tail):
    body = ', '.join('%s %s' % (c, types[c]) for c in cols)
    return 'CREATE TABLE %s (%s) %s' % (new, body, tail)

def rewrite(table, cols, types, tail, dedup_key):
    new = table + '_gwipe'
    try:
        live = [r[0] for r in (q("SELECT * FROM table_columns('%s')" % table).get('dataset') or [])]
        if sorted(live) != sorted(cols):
            print('GWIPE-SKIP %s: live schema differs from canonical (live=%s) - original untouched' % (table, ','.join(sorted(live))))
            return False
        total = one('SELECT count() FROM %s' % table)
        groww = one("SELECT count() FROM %s WHERE feed = 'groww'" % table)
        keep = total - groww
        if groww == 0:
            print('GWIPE-CLEAN %s: rows=%d groww=0 (nothing to remove)' % (table, total))
            return True
        q('DROP TABLE IF EXISTS %s' % new)
        q(ddl(new, cols, types, tail))
        if dedup_key:
            q('ALTER TABLE %s DEDUP ENABLE UPSERT KEYS(%s)' % (new, dedup_key))
        collist = ', '.join(cols)
        q("INSERT INTO %s (%s) SELECT %s FROM %s WHERE feed != 'groww' OR feed IS NULL" % (new, collist, collist, table), timeout=1800)
        got = -1
        deadline = time.time() + 120
        while time.time() < deadline:
            got = one('SELECT count() FROM %s' % new)
            if got == keep:
                break
            time.sleep(2)
        if got != keep:
            print('GWIPE-ABORT %s: copy verify failed (new=%d expected=%d) - ORIGINAL UNTOUCHED, temp dropped' % (table, got, keep))
            q('DROP TABLE IF EXISTS %s' % new)
            return False
    except Exception as exc:
        print('GWIPE-ABORT %s: %r - ORIGINAL UNTOUCHED' % (table, exc))
        try:
            q('DROP TABLE IF EXISTS %s' % new)
        except Exception:
            pass
        return False
    try:
        q('DROP TABLE %s' % table)
        q('RENAME TABLE %s TO %s' % (new, table))
    except Exception as exc:
        print('GWIPE-CRITICAL %s: swap failed midway (%r) - dhan rows are SAFE in %s; run manually: RENAME TABLE %s TO %s' % (table, exc, new, new, table))
        return False
    print('GWIPE-OK %s: before=%d groww_removed=%d after=%d' % (table, total, groww, keep))
    return True

names = [r[0] for r in (q('SELECT table_name FROM tables()').get('dataset') or []) if isinstance(r, list) and r]
targets = (['ticks'] if 'ticks' in names else [])
targets += sorted(t for t in names if t.startswith('candles_') and not t.endswith('_gwipe'))
print('GWIPE-TARGETS %d %s' % (len(targets), targets))
all_ok = bool(targets)
for t in targets:
    if t == 'ticks':
        okr = rewrite(t, TICKS_COLS, TICKS_TYPES, 'TIMESTAMP(ts) PARTITION BY HOUR WAL', 'ts, security_id, segment, capture_seq, feed')
    else:
        okr = rewrite(t, CANDLE_COLS, CANDLE_TYPES, 'timestamp(ts) PARTITION BY DAY DEDUP UPSERT KEYS(ts, security_id, segment, feed)', None)
    all_ok = all_ok and okr
print('GWIPE-TABLES-%s' % ('OK' if all_ok else 'PARTIAL'))
"""


def _wipe_groww_commands() -> list[str]:
    """Build the SSM command list for the groww-only surgical wipe. Pure —
    unit-tested by content assertions without touching boto3. See the block
    comment above `_GROWW_WIPE_PY` for the full design + safety contract."""
    data_dir = "/opt/tickvault/data"
    return [
        "set +e",
        # 1. stop the app + the groww sidecar FIRST: releases QuestDB writers,
        #    stops the capture-file appender + the bridge so nothing re-appends
        #    or re-tails groww rows mid-wipe.
        "systemctl stop tickvault || true",
        "pkill -f groww_sidecar.py 2>/dev/null || true",
        # Best-effort safety net: if SSM kills this script mid-run (execution
        # timeout on an enormous table / manual cancel), restart the app on
        # the way out so the box is never left silently dead. SIGKILL cannot
        # be trapped — that residual case is documented for the operator.
        "trap 'systemctl start tickvault >/dev/null 2>&1 || true' EXIT TERM INT",
        # 2. remove ONLY the groww capture/replay dir (capture file, status,
        #    bridge flushed-offset snapshot) — the proven resurrection vector.
        #    Dhan replay sources (ws_wal/spill/dlq/instrument-cache) KEPT.
        f"rm -rf {data_dir}/groww 2>/dev/null || true",
        "echo 'OK: groww capture/status/offset files removed (data/groww) - dhan replay sources untouched'",
        # 3. surgical per-table rewrite (ticks + every candles_*), verified
        #    copy-before-drop. Full transcript kept at /tmp/tv-gwipe.log.
        "python3 - <<'PYGROWW' 2>&1 | tee /tmp/tv-gwipe.log" + _GROWW_WIPE_PY + "PYGROWW",
        # 4. verify groww counts are 0 BEFORE restarting the app (no live
        #    writes racing the count).
        (
            "sleep 2; "
            "Q='http://127.0.0.1:9000/exec?query='; "
            "TG=$(curl -fsS \"${Q}SELECT%20count()%20FROM%20ticks%20WHERE%20feed%20%3D%20%27groww%27\" 2>/dev/null | grep -o '\\[\\[[0-9]*' | grep -o '[0-9]*'); "
            "CG=$(curl -fsS \"${Q}SELECT%20count()%20FROM%20candles_1m%20WHERE%20feed%20%3D%20%27groww%27\" 2>/dev/null | grep -o '\\[\\[[0-9]*' | grep -o '[0-9]*'); "
            "echo \"GROWW-WIPE-RESULT ticks_groww=${TG:-?} candles_1m_groww=${CG:-?}\""
        ),
        # 5. restart the app — feeds config unchanged (a disabled groww feed
        #    stays disabled; an enabled one resumes LIVE-only from now).
        "systemctl start tickvault || true",
        # 6. honest verdict: COMPLETE only when every table rewrite reported
        #    OK AND both groww counts read 0. Anything else = PARTIAL.
        (
            "if grep -q 'GWIPE-TABLES-OK' /tmp/tv-gwipe.log && [ \"${TG:-1}\" = 0 ] && [ \"${CG:-1}\" = 0 ]; "
            "then echo GROWW-WIPE-COMPLETE; "
            "else echo 'GROWW-WIPE-PARTIAL: some tables were not rewritten or groww rows remain (originals kept safe) - inspect the GWIPE lines above'; fi"
        ),
    ]


# --------------------------------------------------------------------- response
def _resp(status: int, body: dict) -> dict:
    return {"statusCode": status, "headers": {"content-type": "application/json"}, "body": json.dumps(body)}


def _html_resp() -> dict:
    return {
        "statusCode": 200,
        "headers": {
            "content-type": "text/html; charset=utf-8",
            "cache-control": "no-store",
            "referrer-policy": "no-referrer",
        },
        "body": _console_html(),
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
            #      candles_1m are both 0 — a partial wipe reads WIPE-PARTIAL,
            #      never a fake OK.
            # Audit tables are intentionally preserved (SEBI retention) — the
            # dynamic list matches ONLY ticks + candles_*.
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
                "pkill -f groww_sidecar.py 2>/dev/null || true",
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
                #    yesterday's reference rows). python3 is on the box.
                (
                    "python3 - <<'PYWIPE'\n"
                    "import json, urllib.request, urllib.parse\n"
                    "base = 'http://127.0.0.1:9000/exec?query='\n"
                    "q = urllib.parse.quote(\"SELECT table_name FROM tables()\")\n"
                    "rows = json.load(urllib.request.urlopen(base + q, timeout=15)).get('dataset', [])\n"
                    "names = [r[0] for r in rows if isinstance(r, list) and r]\n"
                    "targets = [t for t in names if t == 'ticks' or t.startswith('candles_') or t == 'prev_day_ohlcv']\n"
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
                    "echo \"WIPE-RESULT ticks=${T:-?} candles_1m=${C:-?}\"; "
                    "if [ \"${T:-1}\" = 0 ] && [ \"${C:-1}\" = 0 ]; then echo WIPE-COMPLETE; else echo 'WIPE-PARTIAL: rows remain (or count unavailable) — inspect above'; fi"
                ),
            ]
            cid = _ssm_shell(cmds)
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
        if action == "wipe-groww":
            # DESTRUCTIVE but feed-SCOPED: surgically removes every groww row
            # from the market-data tables (per-table rewrite — QuestDB cannot
            # DELETE rows) + the groww capture/offset files, leaving dhan data
            # byte-identical and the SEBI never-delete tables untouched. In
            # _DESTRUCTIVE (market-hours-blocked 09:15-15:30 IST unless force).
            # Three server-side guards, in order:
            #   1. confirm token — the caller must send {"confirm": "GROWW"}
            #      (typed by the operator in the UI prompt; a scripted call
            #      without the token can never fire this accidentally).
            #   2. box must be RUNNING — the rewrite needs QuestDB + the SSM
            #      agent live; dispatching to a stopped box would silently
            #      no-op and mislead the operator.
            #   3. the command list itself never drops an original table until
            #      the copied row count verified (see _GROWW_WIPE_PY).
            if str(payload.get("confirm", "")).strip() != "GROWW":
                return _resp(
                    409,
                    {"error": 'wipe-groww deletes every groww tick+candle — re-send with {"confirm": "GROWW"}', "action": action},
                )
            inst = _client("ec2").describe_instances(InstanceIds=[INSTANCE_ID])["Reservations"][0]["Instances"][0]
            state = inst["State"]["Name"]
            if state != "running":
                return _resp(
                    409,
                    {"error": f"box is {state} — the groww wipe needs the box RUNNING (start it first)", "action": action},
                )
            cid = _ssm_shell(_wipe_groww_commands())
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
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
            # Server-side classification (pure, unit-tested) — the UI renders
            # the verdict verbatim; it can no longer invent green from a rate.
            snap["tick_conservation"] = _classify_tick_conservation(
                snap.get("conservation_rows", []),
                snap.get("ws_disconnects_today", ""),
                mh,
                snap.get("max_ticks_per_second", ""),
            )
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
        if action == "cross_verify":
            # READ-ONLY (not in _DESTRUCTIVE): the 15:31 IST daily candle
            # check vs the exchange record. Bearer auth already enforced by
            # the top-level _authorized() gate.
            return _resp(200, {"ok": True, "action": "cross_verify", **_parse_cross_verify(_ssm_shell_sync(_CROSS_VERIFY_COMMANDS))})
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
  .spark{ width:100%; height:70px; display:block; }
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
          <div class="bignum"><div class="n" id="ticksbig">0</div><div class="c">ticks captured today</div><div class="c" id="feedsplit"></div></div>
          <div class="pills">
            <div class="pill"><div class="v" id="p_inst">—</div><div class="k">instance</div></div>
            <div class="pill"><div class="v" id="p_app">—</div><div class="k">app</div></div>
            <div class="pill"><div class="v" id="p_mkt">—</div><div class="k">market</div></div>
            <div class="pill"><div class="v" id="p_tps">—</div><div class="k">peak ticks/sec</div></div>
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
      <div class="card"><div class="lbl">live ticks/sec — proof no sub-second tick is lost</div>
        <svg class="spark" id="spark" viewBox="0 0 300 70" preserveAspectRatio="none"></svg></div>
      <div class="card"><div class="lbl">guarantees — live proof read from the box</div>
        <div class="banner" id="stoppedbanner" hidden>⏸ Box stopped (auto-stops 16:30 IST, auto-starts 08:30 Mon–Fri) — guarantees resume on start.</div>
        <div class="shields" id="shields"></div></div>
      <div class="card"><div class="lbl">feeds — live market-data sources (toggle on/off)</div>
        <div class="row" style="margin-bottom:10px"><button class="b-blu" onclick="loadFeeds()">🔄 Load feeds</button></div>
        <div id="feeds"><span class="muted">not loaded yet</span></div>
        <div class="muted" id="feedsmsg" style="margin-top:8px"></div>
        <div class="muted" style="margin-top:6px">Every feed the app reports appears here automatically (a future feed #3 needs zero portal changes). Turning a feed OFF asks for confirmation; disabling Dhan during live trading is refused by the app itself.</div>
      </div>
      <div class="card"><div class="lbl">latency — measured live on the box, per feed</div>
        <div class="row" style="margin-bottom:12px"><button class="b-blu" onclick="loadLatency()">⚡ Measure now</button></div>
        <div id="latfeeds" style="overflow:auto"></div>
        <div class="muted" id="latload" style="margin-top:8px"></div>
        <div class="lbl" style="margin-top:14px">box-wide (shared by all feeds)</div>
        <div class="shields" id="latnet"></div>
        <div class="shields" id="latproc" style="margin-top:10px"></div>
        <div class="muted" id="lattick" style="margin-top:10px"></div>
        <div class="muted" style="margin-top:6px">Takes ~15s (live probe on the box). Every feed the app reports gets a row —
          best value per column is <span class="ok">green</span>; a feed without a known probe endpoint shows "endpoint unknown" (never fake numbers).
          Tick processing / DB round-trip / clock skew are box-wide — the app does not split them per feed, so we don't pretend it does.
          Budgets: tick parse ≤10 ns · full tick process ≤10 µs · order ≤100 ns.</div>
      </div>
    </section>

    <!-- DATA -->
    <section data-tab="data" hidden>
      <div class="card"><div class="lbl">candles sealed today (per timeframe)</div><div id="bars"></div></div>
      <div class="card"><div class="lbl">cross-verify — daily candle check vs exchange record (3:31 PM IST)</div>
        <div class="row" style="margin-bottom:10px"><button class="b-blu" onclick="loadCrossVerify()">🔄 Load result</button></div>
        <div class="shields" id="cvshields"></div>
        <div class="muted" id="cvnote" style="margin-top:8px"></div>
      </div>
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
            <textarea id="dbsql" spellcheck="false">SELECT * FROM ticks ORDER BY ts DESC LIMIT 50</textarea>
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
          <summary><span class="lbl" style="display:inline">⚠️ danger zone — destructive data actions (tap to open)</span> <span class="muted" style="font-size:11px">(contains: Wipe GROWW · Wipe ALL · Docker reset · Bare nuke)</span></summary>
          <div class="warn" id="dangerlock" hidden style="margin-top:10px">🔒 Locked until 3:30 PM IST — data-destructive actions are refused during market hours, even with force. A mid-market wipe destroys data that can never be re-fetched.</div>
          <div class="muted" style="margin-top:10px">Pick ONE severity, then Execute. Every action still asks you to type its own confirm word — nothing fires from a mis-click.</div>
          <label class="danger-opt"><input type="radio" name="danger" value="groww">
            <span><b>🧹 Wipe GROWW data only</b> — <span class="muted">surgical per-feed wipe: deletes every <b>groww</b> tick &amp; candle from every timeframe table (per-table rewrite — the DB can't delete rows in place) AND the groww capture file + offsets so nothing resurrects. <b>Dhan data untouched. Audit tables kept (SEBI).</b> Box must be RUNNING. Asks you to type GROWW.</span></span></label>
          <label class="danger-opt"><input type="radio" name="danger" value="wipe">
            <span><b>🗑️ Wipe ALL data → fresh start</b> — <span class="muted">deletes every tick &amp; candle in ALL timeframe tables for BOTH feeds (Dhan + Groww + any future feed) AND their capture/replay files (Groww capture file, Dhan WAL, spill, dlq) so NOTHING resurrects after restart — audit tables kept. Run when the box is RUNNING; next session = fresh data. Asks you to type WIPE.</span></span></label>
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
let timer=null, hist=[], lastTicks=0, curTab='overview';

function toast(m){ const t=$('toast'); t.textContent=m; t.classList.add('show'); clearTimeout(t._h); t._h=setTimeout(()=>t.classList.remove('show'),2600); }
function esc(s){ return String(s).replace(/[<>&]/g,c=>({'<':'&lt;','>':'&gt;','&':'&amp;'}[c])); }
function setLive(on){ $('livedot').classList.toggle('on',on); $('livetxt').textContent=on?'live':'offline'; }

function unlock(){ const v=($('tok').value||'').trim(); if(!v){ toast('Paste your key first'); return; }
  TOKEN=v; localStorage.setItem('tv_token',v); $('lock').hidden=true; $('app').hidden=false; startAuto(); loadOverview(); }
function lock(){ TOKEN=''; localStorage.removeItem('tv_token'); $('tok').value=''; stopAuto(); $('app').hidden=true; $('lock').hidden=false; setLive(false); toast('Locked — key forgotten on this device'); }

function tab(name){ curTab=name; document.querySelectorAll('.tab').forEach(t=>t.classList.toggle('active',t.dataset.t===name));
  document.querySelectorAll('section[data-tab]').forEach(s=>s.hidden=s.dataset.tab!==name);
  if(name==='data' && !$('cvshields').dataset.loaded) loadCrossVerify();
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

function drawSpark(){ const w=300,h=70,pad=6,xs=hist.slice(-40),max=Math.max(2,...xs);
  const pts=xs.map((v,i)=>{ const x=pad+(w-2*pad)*(xs.length<2?0:i/(xs.length-1)); const y=h-pad-(h-2*pad)*(v/max); return x.toFixed(1)+','+y.toFixed(1); });
  const line=pts.join(' '); const area=pts.length?'M'+pad+','+(h-pad)+' L'+line.replace(/ /g,' L')+' L'+(w-pad)+','+(h-pad)+' Z':'';
  $('spark').innerHTML='<defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#38e1d6" stop-opacity=".5"/><stop offset="1" stop-color="#38e1d6" stop-opacity="0"/></linearGradient></defs>'+
    (area?'<path d="'+area+'" fill="url(#g)"/>':'')+(pts.length>1?'<polyline points="'+line+'" fill="none" stroke="#38e1d6" stroke-width="2.5" stroke-linejoin="round" stroke-linecap="round"/>':'')+
    (pts.length?'<circle cx="'+pts[pts.length-1].split(',')[0]+'" cy="'+pts[pts.length-1].split(',')[1]+'" r="3.5" fill="#38e1d6"/>':''); }

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
  countUp($('ticksbig'), j.ticks_today||'0');
  $('p_inst').innerHTML='<span class="'+(running?'ok':'bad')+'">'+(j.instance_state||'?')+'</span>';
  $('p_app').innerHTML='<span class="'+(appOk?'ok':'bad')+'">'+(appOk?'up':(j.app||'down'))+'</span>';
  $('p_mkt').innerHTML='<span class="'+(j.market_hours?'warn':'')+'">'+(j.market_hours?'OPEN':'closed')+'</span>';
  // Danger-zone hard-lock label: data-destructive actions are server-refused
  // (409, no force escape) while the market is open — surface that BEFORE a click.
  const dl=$('dangerlock'); if(dl) dl.hidden=!j.market_hours;
  const tpsN=parseInt(j.max_ticks_per_second,10)||0; $('p_tps').innerHTML='<span class="cy">'+tpsN+'</span>';
  hist.push(tpsN); if(hist.length>60) hist.shift(); drawSpark();
  // Per-feed split of today's ticks, e.g. "dhan: 152,340 | groww: 8,102" —
  // rendered generically from whatever feeds QuestDB reports (no hardcoding).
  const fb=j.ticks_by_feed||{}, fbKeys=Object.keys(fb).sort();
  $('feedsplit').textContent = fbKeys.length ? fbKeys.map(k=>k+': '+((parseInt(fb[k],10)||0).toLocaleString())).join(' | ') : '';
  const c=j.candles||{}, vals=['1m','5m','15m','60m','1d'].map(k=>parseInt(c[k],10)||0), mx=Math.max(1,...vals);
  $('bars').innerHTML=['1m','5m','15m','60m','1d'].map((k,i)=>bar(k,vals[i],mx)).join('');
  // Dedup-key check: the real ticks upsert key is 5 columns
  // (ts, security_id, segment, capture_seq, feed). Three distinct states:
  //   5 = OK (green) · 0 = DEDUP disabled entirely (RED — duplicate rows will
  //   accumulate) · any other number = schema drift (amber). A fetch failure
  //   (box/QuestDB unreachable) renders "unreachable" (amber), never a fake 0.
  // Stopped box → ONE calm banner + greyed "—" shields instead of a scatter
  // of scary per-shield "unreachable" warnings. Gated STRICTLY on
  // instance_state!=='running' — a RUNNING box with unreachable data keeps
  // the real unreachable/drift/disabled warnings below.
  $('stoppedbanner').hidden=running;
  if(!running){
    $('shields').innerHTML=shieldIdle('Dedup key columns')+shieldIdle('Sub-second fix')+
      shieldIdle('Tick conservation')+shieldIdle('Peak ticks / second');
  } else {
  const dkRaw=String(j.dedup_key_columns||'').trim(), dkN=parseInt(dkRaw,10);
  const dkUnknown=(dkRaw===''||isNaN(dkN)), keysOk=dkN===5;
  let dkTxt,dkCls;
  if(dkUnknown){ dkTxt='unreachable'; dkCls='warn'; }
  else if(dkN===5){ dkTxt='5 ✅'; dkCls='ok'; }
  else if(dkN===0){ dkTxt='0 — DEDUP disabled!'; dkCls='bad'; }
  else { dkTxt=dkRaw+' — schema drift (expected 5)'; dkCls='warn'; }
  const capOk=tpsN>1;
  // Tick-conservation shield: the verdict is computed SERVER-SIDE from the
  // 15:40 audit + today's ws_event_audit disconnects (audit fix #1 — the old
  // ">1 tick/sec" liveness false-OK heuristic is gone). The UI renders the
  // server verdict verbatim; the tooltip states the envelope.
  const tc=j.tick_conservation||{state:'unreachable',label:'unreachable',good:false,cls:'warn',envelope:''};
  $('shields').innerHTML=shield('Dedup key columns',dkTxt,keysOk,dkCls)+
    shield('Sub-second fix',keysOk?'LIVE ✅':(dkUnknown?'unreachable':'OLD ⚠'),keysOk,dkUnknown?'warn':undefined)+
    shield('Tick conservation',esc(tc.label||'unreachable'),!!tc.good,tc.cls||'warn',tc.envelope||'')+
    shield('Peak ticks / second',tpsN,capOk);
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
    const detail=h?('<div class="muted">'+esc(h.reason||'')+(h.ticks_total!=null?' · ticks '+esc(String(h.ticks_total)):'')
      +(h.subscribed_total!=null?' · subscribed '+esc(String(h.subscribed_total)):'')+'</div>'):'';
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

// Cross-verify card (3:31 PM IST daily candle check vs exchange record).
// PASS requires the hardened condition: zero mismatches AND zero missing
// minutes AND not degraded AND at least one minute actually compared —
// "nothing compared" must never render as PASS. All string values are
// rendered through esc() (operator data never lands raw in HTML).
async function loadCrossVerify(){ $('cvshields').dataset.loaded='1'; $('cvshields').innerHTML='<span class="muted">loading…</span>'; $('cvnote').textContent='';
  const j=await call('cross_verify'); if(!j){ $('cvshields').innerHTML=''; return; }
  if(!j.date){ $('cvshields').innerHTML='<span class="muted">no run yet — the check fires at 3:31 PM IST on trading days (box must be running)</span>'; return; }
  const mis=parseInt(j.mismatch_rows,10)||0, missing=parseInt(j.missing,10)||0, compared=parseInt(j.compared,10)||0;
  const degraded=String(j.degraded).toLowerCase()==='true';
  const hasSummary=(j.compared!=null && j.compared!=='');
  const pass = hasSummary ? (mis===0 && missing===0 && !degraded && compared>0) : false;
  const good = hasSummary ? pass : (mis===0 && !degraded);
  const badge = hasSummary ? (pass?'PASS ✅':'FAIL ⚠') : (good?'OK (no summary)':'FAIL ⚠');
  $('cvshields').innerHTML=
    shield('Result', badge, good)+
    shield('Date', esc(j.date||'—'), true)+
    shield('Instruments', esc(j.instruments||'—'), true)+
    shield('Minutes compared', esc(j.compared||'—'), !hasSummary||compared>0)+
    shield('Mismatches', esc(j.mismatch_rows||'0'), mis===0)+
    shield('Missing minutes', esc(j.missing||'—'), missing===0);
  $('cvnote').textContent = degraded ? 'Coverage was PARTIAL — the check could not vouch for the full universe.'
    : (pass ? 'Every 1-minute candle matches the exchange record exactly.'
    : (hasSummary ? 'Differences found — review which minutes differ from the exchange record.'
    : 'This run pre-dates the summary artefact — mismatch count shown from the day\'s file.')); }

function fmtNs(n){ if(n==null||n==='') return '—'; n=Number(n); if(n<1000) return n.toFixed(0)+' ns';
  if(n<1e6) return (n/1e3).toFixed(2)+' µs'; if(n<1e9) return (n/1e6).toFixed(2)+' ms'; return (n/1e9).toFixed(2)+' s'; }
function fmtMs(s){ return (s===''||s==null)?'—':s+' ms'; }
// Per-feed comparison table (operator demand 2026-07-03). Rendered by
// ITERATING the server's j.feeds rows — which themselves come from the app's
// own /api/feeds/health at measure time. NO feed name is hardcoded here, so a
// future feed #3 gets its row (and honest "endpoint unknown" network cells if
// the probe map doesn't know it) with zero portal changes. The best value per
// column (server-computed j.winners — strict best, never on a tie or a
// single-feed measure) is highlighted green.
async function loadLatency(){ $('latnet').dataset.loaded='1'; $('latnet').innerHTML='<span class="muted">measuring…</span>'; $('latproc').innerHTML='';
  $('latfeeds').innerHTML='<span class="muted">measuring…</span>'; $('latload').textContent='';
  const j=await call('latency'); if(!j){ $('latnet').innerHTML=''; $('latfeeds').innerHTML=''; return; }
  const feeds=Array.isArray(j.feeds)?j.feeds:[]; const w=j.winners||{};
  if(feeds.length){
    const cols=[['tcp_ms','TCP connect'],['tls_ms','+ TLS'],['last_tick_age_secs','last tick age'],['ticks_per_sec','ticks/sec (window)'],['subscribed_total','subscribed']];
    let h='<table><tr><th>feed</th>'+cols.map(c=>'<th>'+c[1]+'</th>').join('')+'</tr>';
    feeds.forEach(f=>{
      h+='<tr><td><b>'+esc(String(f.feed))+'</b></td>';
      cols.forEach(c=>{ const k=c[0]; const v=f[k]; let txt;
        if(k==='tcp_ms'||k==='tls_ms') txt=f.endpoint_known?fmtMs(v):'endpoint unknown';
        else if(k==='last_tick_age_secs') txt=(v==null)?'—':v+' s ago';
        else if(k==='ticks_per_sec') txt=(v==null)?'—':v+'/s';
        else txt=(v==null)?'—':Number(v).toLocaleString();
        h+='<td class="'+(w[k]===f.feed?'win':'')+'">'+esc(String(txt))+'</td>'; });
      h+='</tr>'; });
    $('latfeeds').innerHTML=h+'</table>';
    $('latload').textContent='instrument load — '+feeds.map(f=>String(f.feed)+': '+
      (f.subscribed_total!=null?Number(f.subscribed_total).toLocaleString()+' subscribed':'no count reported')).join(' · ');
  } else {
    $('latfeeds').innerHTML='<span class="warn">'+esc(j.feeds_error||'no per-feed data returned — is the app running?')+'</span>';
  }
  const skew=Math.abs(parseFloat(j.clock_skew_ms));
  $('latnet').innerHTML=
    shield('QuestDB round-trip (box-wide)', fmtMs(j.questdb_ms), true)+
    shield('Clock skew (box-wide)', fmtMs(j.clock_skew_ms), isNaN(skew)||skew<50);
  // Windowed precise latencies (live ticks during the ~10s probe window).
  // Falls back to lifetime averages when the window saw no ticks (market closed).
  const live = j.tick_p50_ns != null;
  $('latproc').innerHTML = live ? (
    shield('Per-tick p50 (live)', fmtNs(j.tick_p50_ns), j.tick_p50_ns<10000)+
    shield('Per-tick p99 (live)', fmtNs(j.tick_p99_ns), j.tick_p99_ns<100000)+
    shield('Per-tick avg (live)', fmtNs(j.tick_window_avg_ns), j.tick_window_avg_ns<10000)+
    shield('Wire → done p99 (live)', fmtNs(j.wire_p99_ns), (j.wire_p99_ns||1e12)<100000)+
    shield('DB batch flush avg', fmtNs(j.flush_window_avg_ns), true)+
    shield('Order placement', fmtNs(j.order_place_avg_ns), true)
  ) : (
    shield('Per-tick process (lifetime)', fmtNs(j.tick_process_avg_ns), (j.tick_process_avg_ns||1e12)<10000)+
    shield('Wire → done (lifetime)', fmtNs(j.wire_to_done_avg_ns), (j.wire_to_done_avg_ns||1e12)<100000)+
    shield('Order placement', fmtNs(j.order_place_avg_ns), true)
  );
  $('lattick').textContent = live
    ? ('measured live over '+Number(j.tick_window_count).toLocaleString()+' ticks in a '+(j.window_secs||'~10')+'s window — lifetime avg '+fmtNs(j.tick_process_avg_ns)+' over '+Number(j.tick_count||0).toLocaleString()+' ticks since boot')
    : (j.tick_count? ('no live ticks in the probe window (market closed?) — lifetime avg over '+Number(j.tick_count).toLocaleString()+' ticks since boot') : 'no ticks measured yet (market closed?)'); }

// forceId lets each tab keep its own force checkbox next to its own buttons:
// Overview's instance button uses #force_inst, Admin's app controls use #force.
async function act(action,forceId){ if(!confirm('Run "'+action+'" on the trading box?')) return;
  const fEl=$(forceId||'force'); const force=!!(fEl&&fEl.checked); toast(action+'…');
  const j=await call(action,{force}); if(j){ toast('✅ '+action+' sent'); setTimeout(loadOverview,1600); } }

// Danger-zone severity picker → the UNCHANGED per-action functions. Each still
// asks for its OWN typed confirm token (GROWW / WIPE / NUKE-DOCKER / ERASE) — the
// picker only chooses WHICH prompt fires; there is no token bypass path.
function dangerExecute(){ const sel=document.querySelector('input[name="danger"]:checked');
  if(!sel){ toast('Pick a danger-zone action first'); return; }
  const fn={groww:wipeGroww, wipe:wipeData, nuke:dockerReset, erase:bareNuke}[sel.value];
  if(fn) fn(); else toast('Unknown action'); }

async function wipeData(){
  if(prompt('This DELETES every tick and candle, then restarts empty. The box must be RUNNING. Type WIPE to confirm:')!=='WIPE'){ toast('cancelled'); return; }
  toast('wiping → fresh start…');
  const j=await call('wipe-questdb',{force:true,confirm:'WIPE'});
  if(j&&j.ok){ toast('✅ wipe started — fresh data from next session'); setTimeout(loadOverview,4000); }
  else { toast((j&&j.error)||'wipe failed — is the box running?'); } }
async function wipeGroww(){
  if(prompt('🧹 GROWW-ONLY WIPE. Deletes EVERY groww tick + candle (per-table rewrite) and the groww capture/offset files so nothing resurrects. Dhan data is UNTOUCHED; audit tables kept (SEBI). The box must be RUNNING. Type GROWW to confirm:')!=='GROWW'){ toast('cancelled'); return; }
  toast('🧹 groww-only wipe dispatched → rewriting tables in background…');
  const j=await call('wipe-groww',{force:$('force').checked,confirm:'GROWW'});
  if(!(j&&j.ok)){ toast((j&&j.error)||'groww wipe failed — is the box running?'); return; }
  if(!j.command_id){ toast('⚠️ dispatched but no command id — re-check the groww tick count in ~2 min'); return; }
  pollNuke(j.command_id,0); }
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
    if(out.indexOf('GROWW-WIPE-COMPLETE')>=0){ toast('✅ GROWW wipe complete — 0 groww rows left, dhan data intact; app restarting'); }
    else if(out.indexOf('GROWW-WIPE-PARTIAL')>=0){ toast('🟠 GROWW wipe PARTIAL — some tables not rewritten (originals kept SAFE, nothing lost). Inspect the box output (GWIPE lines).'); }
    else if(out.indexOf('DOCKER-RESET-FAILED')>=0){ toast('🔴 NUKE FAILED — QuestDB volume still in use, data NOT wiped. Check the box.'); }
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
