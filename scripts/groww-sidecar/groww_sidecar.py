#!/usr/bin/env python3
"""Groww validation sidecar (LOCAL-ONLY, operator lock §32) — the PRODUCER.

VERIFIED against the official growwapi-1.5.0 SDK source
(docs/groww-ref/10-live-feed-mapping-verified.md). The live feed is
NATS-over-WebSocket + Protobuf; GrowwFeed handles transport + decode. The
callback receives the topic META; the parsed ticks are pulled via get_ltp()
(stocks) / get_index_value() (indices), each shaped
`{exchange: {segment: {exchange_token: {...}}}}`.

PR-B2i (2026-06-21): the watch set is the watch file Rust writes at boot
(crates/core/src/feed/groww/instruments.rs -> data/groww/groww-watch-<date>.json).
This sidecar reads that file and subscribes BOTH:
  - STOCK entries (kind="ltp")        via subscribe_ltp        (numeric token).
  - INDEX entries (kind="index_value") via subscribe_index_value (NSE name token
    e.g. "NIFTY"; BSE SENSEX numeric "1").
The integer `security_id` STORED for each instrument comes from the Rust watch
entry (single source of truth): for stocks it is the numeric exchange_token; for
indices (whose token may be a name) it is the Groww-native stable id Rust
derived (operator decision 2026-06-21). The sidecar NEVER re-derives it — it
looks it up by (exchange, segment, exchange_token) so the index name/token never
has to map to an integer here.

Each received tick is appended as one NDJSON line to data/groww/live-ticks.ndjson
— the EXACT schema the Rust bridge (crates/app/src/groww_bridge.rs) parses.
Index ticks are emitted with segment="IDX_I" and ltp=<index value>.
Capture-at-receipt: write + flush + fsync the instant the callback fires (durable
floor one hop downstream of the socket; lock §32.3).

Volume is Option A (price-only, operator 2026-06-20): the Groww live feed carries
NO traded volume (only ltp/value + tsInMillis), so cum_volume is always 0.

Usage (auto-launched by the Rust supervisor; no manual run needed):
    export GROWW_API_KEY=...   GROWW_TOTP_SECRET=...
    python3 groww_sidecar.py
"""
import glob
import json
import os
import random
import sys
import threading
import time
import traceback
from datetime import datetime, timezone, timedelta

import pyotp

try:
    from growwapi import GrowwAPI, GrowwFeed
except Exception as exc:  # pragma: no cover
    # Log only the exception TYPE (consistent with the reconnect/watch handlers):
    # an import error's str can embed filesystem/site-packages paths.
    sys.exit(
        f"growwapi import failed ({type(exc).__name__}). "
        "`pip install -r requirements.txt` first."
    )

# The path the Rust bridge tails (GROWW_TICK_FILE_DEFAULT in groww_bridge.rs).
OUTPUT_PATH = os.environ.get("GROWW_TICK_FILE", "data/groww/live-ticks.ndjson")
# The connect+subscribe PROOF status file the Rust bridge reads (operator
# 2026-06-28). Written ATOMICALLY (temp + rename) so the bridge never reads a
# half-written file; carries ONLY counts + an event tag + a timestamp — NEVER any
# credential. Default mirrors GROWW_STATUS_FILE_DEFAULT in groww_bridge.rs.
STATUS_PATH = os.environ.get("GROWW_STATUS_FILE", "data/groww/groww-status.json")
# Directory the Rust watch-list builder writes groww-watch-<date>.json into.
WATCH_DIR = os.environ.get("GROWW_WATCH_DIR", "data/groww")
IST = timezone(timedelta(hours=5, minutes=30))

# Groww (exchange, segment) -> our canonical segment string (bridge contract),
# used for STOCK (subscribe_ltp) ticks. Verified values: NSE/BSE x CASH/FNO.
SEGMENT_MAP = {
    ("NSE", "CASH"): "NSE_EQ",
    ("NSE", "FNO"): "NSE_FNO",
    ("BSE", "CASH"): "BSE_EQ",
    ("BSE", "FNO"): "BSE_FNO",
}
# All index ticks (whatever exchange/segment Groww uses) store as IDX_I — matches
# the Dhan index convention + the bridge's segment_from_str("IDX_I").
CANONICAL_INDEX_SEGMENT = "IDX_I"

# How long to wait between checks for the Rust-built watch file at startup.
WATCH_POLL_SECS = 5

# Silent-feed watchdog (2026-06-29). The Groww SDK's blocking `consume()` decodes
# NATS/Protobuf frames internally and can swallow per-frame errors (printing a bare
# SDK-internal "Error:" line) WITHOUT raising — so our `except` never fires and we
# sit "subscribed but 0 ticks" with no actionable cause. This watchdog runs in a
# daemon thread: if no record is DECODED (neither emitted nor dropped) within the
# deadline after we subscribe, it prints ONE loud, actionable diagnostic to stderr
# (and then a quieter periodic reminder with live counts) so the operator sees the
# real "feed silent" signal + the most-likely causes instead of staring at the
# SDK's empty "Error:" lines. A DROP-only flow (decoded but key-map miss) is a
# DIFFERENT, already-surfaced signal (note_drop counters), so the watchdog treats
# decoded==0 as the silent case.
SILENT_FEED_FIRST_WARN_SECS = 30
SILENT_FEED_REWARN_SECS = 60

# Honest-feed counters (operator 2026-06-29). EMITTED_TOTAL = records we DECODED
# and successfully wrote as a tick; DROPPED_TOTAL = records we DECODED but had to
# drop (a sid_map miss, or a missing ltp/value field) — previously a SILENT
# `continue`. The Rust bridge surfaces both via feed-health so a key-map mismatch
# ("streaming but 0 ticks") is visible instead of invisible.
EMITTED_TOTAL = 0
DROPPED_TOTAL = 0
# Throttle for the periodic status re-write that carries the live emitted/dropped
# counts: first emit always writes (it flips the bridge's connected=true), then at
# most once per second so a fast tick stream cannot thrash the atomic-rename write.
STATUS_REWRITE_MIN_INTERVAL_SECS = 1.0
_last_status_rewrite_monotonic = 0.0
# Cached subscribe counts so the periodic status re-write knows N stocks + M indices
# without threading them through every emit call. Set once after subscribe.
_SUBSCRIBED_STOCKS = 0
_SUBSCRIBED_INDICES = 0

# Reconnect/auth backoff (charter: exponential backoff, NO retry storms). The old
# flat 5s re-auth every loop made the sidecar 429 itself (GrowwAPIRateLimitException
# in the [auth] phase): Groww rate-limits the token endpoint, and a fixed 5s retry
# is a storm. We now back off exponentially per consecutive failure, capped, with
# jitter, and back off LONGER on a rate-limit, so a clean token call gets through.
RECONNECT_BACKOFF_BASE_SECS = 5
RECONNECT_BACKOFF_CAP_SECS = 300
# A rate-limit means we are being throttled — wait much longer before retrying so
# the limiter window clears (a short retry just extends the throttle).
RATE_LIMIT_BACKOFF_BASE_SECS = 60
# Multiplicative jitter band (±20%) so concurrent retries don't align into bursts.
BACKOFF_JITTER_FRAC = 0.2


def _is_rate_limit_error(exc) -> bool:
    """True if `exc` is (or wraps) a Groww rate-limit / HTTP 429.

    Matches by exception class name (GrowwAPIRateLimitException) and by HTTP 429
    status on the exception or its `.response`, without importing SDK-internal
    types (they are not part of the public surface we depend on).
    """
    if "ratelimit" in type(exc).__name__.replace("_", "").lower():
        return True
    status = getattr(exc, "status_code", None)
    if status is None:
        response = getattr(exc, "response", None)
        status = getattr(response, "status_code", None) if response is not None else None
    return status == 429


def _retry_after_secs(exc):
    """Return a server-advised retry-after (seconds) if the exception exposes one.

    Honors a `retry_after` attribute or a `Retry-After` header on `exc.response`
    (numeric seconds form). Returns None if absent/unparseable.
    """
    candidate = getattr(exc, "retry_after", None)
    if candidate is None:
        response = getattr(exc, "response", None)
        headers = getattr(response, "headers", None) if response is not None else None
        if headers is not None:
            try:
                candidate = headers.get("Retry-After") or headers.get("retry-after")
            except (AttributeError, TypeError):
                candidate = None
    if candidate is None:
        return None
    try:
        secs = float(candidate)
    except (ValueError, TypeError):
        return None
    return secs if secs > 0 else None


def _backoff_secs(consecutive_failures: int, rate_limited: bool, retry_after) -> float:
    """Exponential backoff with jitter for the Nth consecutive failure.

    Base 5s (60s when rate-limited), doubling per consecutive failure, capped at
    300s, ±20% jitter. A server-advised retry-after (if larger) takes precedence.
    """
    base = RATE_LIMIT_BACKOFF_BASE_SECS if rate_limited else RECONNECT_BACKOFF_BASE_SECS
    exp = base * (2 ** max(0, consecutive_failures - 1))
    delay = min(exp, RECONNECT_BACKOFF_CAP_SECS)
    if retry_after is not None:
        delay = max(delay, min(retry_after, RECONNECT_BACKOFF_CAP_SECS))
    jitter = 1.0 + random.uniform(-BACKOFF_JITTER_FRAC, BACKOFF_JITTER_FRAC)
    return delay * jitter


def _redact(text: str, secrets) -> str:
    """Scrub known secret values out of a string before it is logged.

    A Groww SDK HTTP error's str/repr can embed the request that carried the
    access token / api_key, or echo the response body (security-review MEDIUM
    2026-06-19). We DO want the cause (status code, error message, SDK detail)
    for triage, so instead of dropping the whole detail we surface it with every
    known secret value masked. Anything secret-shaped that we did not anticipate
    is still a residual risk, so callers also cap the length.
    """
    if not text:
        return text
    for secret in secrets:
        if secret:
            text = text.replace(secret, "***REDACTED***")
    return text


def _exception_detail(exc, secrets, max_len: int = 1200) -> str:
    """Build a redacted, length-capped detail string for `exc`.

    Surfaces the WHY so the operator can tell a credential error (auth fails)
    from an off-market feed reject / no-entitlement (auth OK, feed rejects) —
    with every known secret masked and the whole thing capped.

    CRITICAL (2026-06-29): the Groww SDK exceptions store their real detail in
    `.msg` + `.code` attributes (docs/groww-ref/05-exceptions.md), NOT in
    `str(exc)`. For `GrowwBaseException`/`GrowwFeedException`/`GrowwAPIException`
    a bare `str(exc)` is frequently EMPTY — which is exactly why the operator saw
    a blank message. We now read `.msg`/`.code` FIRST, fall back to `repr(exc)`
    (never empty), and ALWAYS append the redacted, capped traceback so we can
    never again be blind to the cause.
    """
    parts = []
    # 1. SDK-native detail: Groww exceptions carry their message in `.msg` and an
    #    error code in `.code` (see docs/groww-ref/05-exceptions.md). These are the
    #    fields that are populated when `str(exc)` is empty.
    msg = getattr(exc, "msg", None)
    if msg:
        parts.append(f"msg={_redact(str(msg), secrets)}")
    code = getattr(exc, "code", None)
    if code is not None and code != "":
        parts.append(f"code={_redact(str(code), secrets)}")
    # GrowwFeedNotSubscribedException carries the topic that must be subscribed.
    topic = getattr(exc, "topic", None)
    if topic:
        parts.append(f"topic={_redact(str(topic), secrets)}")
    # 2. str(exc) — may duplicate `.msg`, may be empty; include only if it adds
    #    info (compare REDACTED-to-REDACTED so a redacted `.msg` doesn't re-appear).
    detail = _redact(str(exc), secrets)
    msg_redacted = _redact(str(msg), secrets) if msg else ""
    if detail and detail != msg_redacted:
        parts.append(detail)
    # 3. Optional HTTP detail some SDK exceptions carry.
    status = getattr(exc, "status_code", None)
    if status is None:
        response = getattr(exc, "response", None)
        status = getattr(response, "status_code", None) if response is not None else None
    if status is not None:
        parts.append(f"status={status}")
    body = None
    response = getattr(exc, "response", None)
    if response is not None:
        body = getattr(response, "text", None) or getattr(response, "body", None)
    if body is None:
        body = getattr(exc, "body", None)
    if body:
        parts.append(f"body={_redact(str(body), secrets)}")
    # 4. repr(exc) is NEVER empty (it always carries the class name) — the final
    #    guarantee that the line is never blank even if every attribute above is
    #    empty/absent.
    parts.append(f"repr={_redact(repr(exc), secrets)}")
    summary = " | ".join(p for p in parts if p)
    if len(summary) > max_len:
        summary = summary[:max_len] + "…(truncated)"
    return summary or "(no detail available)"


def ms_to_ist_nanos(ts_millis: int) -> int:
    """Groww `tsInMillis` (UTC epoch ms) -> IST epoch nanoseconds.

    Mirrors the Dhan "store IST wall-clock directly" rule (data-integrity.md):
    convert UTC ms -> IST wall clock, then to nanos. Keeps ms precision.
    """
    dt_utc = datetime.fromtimestamp(ts_millis / 1000.0, tz=timezone.utc)
    dt_ist = dt_utc.astimezone(IST)
    return int(dt_ist.replace(tzinfo=timezone.utc).timestamp() * 1_000_000_000)


def now_ist_nanos() -> int:
    """Current wall-clock time as IST epoch nanoseconds (for the status file ts)."""
    return int(datetime.now(tz=IST).timestamp() * 1_000_000_000)


def write_status(event: str, stocks: int, indices: int) -> None:
    """Atomically write the connect+subscribe PROOF status the Rust bridge reads.

    The bridge (crates/app/src/groww_bridge.rs) tails this file to emit the ONE
    structured "Groww live feed CONNECTED — subscribed N stocks + M indices" log,
    record the subscribe counts in feed-health, and flip `connected=true` only on
    the `streaming` event (no false-OK; honest-feed fix 2026-06-29 — `streaming` is
    now written ONLY on the FIRST real decoded+emitted tick, never optimistically).
    Contains ONLY: event tag + counts + the live emitted/dropped totals + a
    timestamp — NEVER a credential. Atomic temp+rename so the bridge never reads a
    torn file. Best-effort: a write failure is logged (type only) and ignored — the
    stream itself is unaffected, and the bridge's first-tick fallback still flips
    connected.
    """
    rec = {
        "event": event,
        "stocks": int(stocks),
        "indices": int(indices),
        "total": int(stocks) + int(indices),
        # Honest-feed PROOF (2026-06-29): records DECODED+EMITTED vs DECODED-but-
        # DROPPED so the bridge can surface "streaming but 0 ticks" with a cause.
        "emitted": int(EMITTED_TOTAL),
        "dropped": int(DROPPED_TOTAL),
        "ts_ist_nanos": now_ist_nanos(),
    }
    try:
        directory = os.path.dirname(STATUS_PATH) or "."
        os.makedirs(directory, exist_ok=True)
        tmp = f"{STATUS_PATH}.{os.getpid()}.tmp"
        with open(tmp, "w") as fh:
            fh.write(json.dumps(rec))
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, STATUS_PATH)  # atomic rename
    except OSError as exc:
        # Never embed paths/values that could leak anything; type only.
        print(
            f"groww sidecar: status write failed ({type(exc).__name__}); continuing",
            flush=True,
        )


def note_drop() -> None:
    """Count one DECODED-but-DROPPED record (sid_map miss / missing field).

    Previously these were a SILENT `continue`. Counting them lets the Rust bridge
    surface "streaming but 0 ticks" with a visible cause (operator 2026-06-29).
    Does NOT re-write the status file (drops without emits do not flip connected;
    the count is carried on the next periodic emit re-write).
    """
    global DROPPED_TOTAL
    DROPPED_TOTAL += 1


def note_emit() -> None:
    """Count one DECODED+EMITTED record and drive the honest `streaming` status.

    On the FIRST successful emit (EMITTED_TOTAL 0 -> 1) this writes the
    `streaming` status — the ONLY place "streaming" is written, now backed by a
    REAL decoded+emitted tick (the optimistic pre-consume write was removed). After
    that it re-writes the status (refreshed emitted/dropped) at most once per second
    so the bridge surfaces live counts without status-file thrash.
    """
    global EMITTED_TOTAL, _last_status_rewrite_monotonic
    EMITTED_TOTAL += 1
    now = time.monotonic()
    if EMITTED_TOTAL == 1:
        # First real tick: this is the honest proof the feed is streaming.
        write_status("streaming", _SUBSCRIBED_STOCKS, _SUBSCRIBED_INDICES)
        _last_status_rewrite_monotonic = now
        return
    if now - _last_status_rewrite_monotonic >= STATUS_REWRITE_MIN_INTERVAL_SECS:
        write_status("streaming", _SUBSCRIBED_STOCKS, _SUBSCRIBED_INDICES)
        _last_status_rewrite_monotonic = now


def latest_watch_file(watch_dir: str):
    """Return the path of the most recent groww-watch-*.json, or None."""
    matches = sorted(glob.glob(os.path.join(watch_dir, "groww-watch-*.json")))
    return matches[-1] if matches else None


def load_subscriptions(watch_path: str):
    """Read the Rust watch file -> (stock_list, index_list, sid_map).

    stock_list / index_list = [{exchange, segment, exchange_token}] for the SDK
    subscribe calls. sid_map = {(exchange, segment, exchange_token): security_id}
    so emit can stamp the Rust-assigned integer security_id (the index name/token
    never has to be parsed to an int here).
    """
    with open(watch_path, "r") as fh:
        doc = json.load(fh)
    stock_list = []
    index_list = []
    sid_map = {}
    skipped_non_numeric = 0
    for entry in doc.get("entries", []):
        exchange = str(entry.get("exchange", "NSE"))
        segment = str(entry.get("segment", "CASH"))
        token = str(entry.get("exchange_token", ""))
        if not token:
            continue
        security_id = int(entry.get("security_id", 0))
        sub = {"exchange": exchange, "segment": segment, "exchange_token": token}
        kind = entry.get("kind")
        if kind == "index_value":
            index_list.append(sub)
            sid_map[(exchange, segment, token)] = security_id
        else:
            # Stocks must have a numeric token (also their security_id).
            if not token.isdigit():
                skipped_non_numeric += 1
                continue
            stock_list.append(sub)
            sid_map[(exchange, segment, token)] = security_id or int(token)
    if skipped_non_numeric:
        print(
            f"groww sidecar: skipped {skipped_non_numeric} non-numeric stock tokens",
            flush=True,
        )
    return stock_list, index_list, sid_map


def _write_record(out, security_id: int, segment: str, ts_millis: int, price) -> None:
    """Append one NDJSON tick (Rust bridge schema) + capture-at-receipt fsync."""
    rec = {
        "security_id": int(security_id),
        "segment": segment,
        "ts_ist_nanos": ms_to_ist_nanos(ts_millis) if ts_millis else 0,
        "exchange_ts_millis": ts_millis,
        "ltp": float(price),
        # Option A: Groww live feed carries no volume -> always 0.
        "cum_volume": 0,
    }
    out.write(json.dumps(rec) + "\n")
    out.flush()
    os.fsync(out.fileno())  # capture-at-receipt durability


def emit_ltp_records(out, ltp_tree: dict, sid_map: dict) -> None:
    """Flatten get_ltp() `{exchange:{segment:{token:{ltp,tsInMillis}}}}` -> NDJSON.

    Stock identity comes from the tree path; security_id from sid_map (falls back
    to the numeric token, which IS the stock security_id).
    """
    if not isinstance(ltp_tree, dict):
        return
    for exchange, segs in ltp_tree.items():
        if not isinstance(segs, dict):
            continue
        for segment, tokens in segs.items():
            canonical = SEGMENT_MAP.get((str(exchange), str(segment)))
            if canonical is None or not isinstance(tokens, dict):
                continue
            for token, tick in tokens.items():
                # A decoded record with no `ltp` field — DROP (honest-feed count).
                if not isinstance(tick, dict) or "ltp" not in tick:
                    note_drop()
                    continue
                token = str(token)
                security_id = sid_map.get((str(exchange), str(segment), token))
                if security_id is None:
                    security_id = int(token) if token.isdigit() else 0
                # sid_map miss + non-numeric token → no resolvable id — DROP.
                if security_id <= 0:
                    note_drop()
                    continue
                try:
                    ts_millis = int(tick.get("tsInMillis", 0))
                    _write_record(out, security_id, canonical, ts_millis, tick["ltp"])
                    note_emit()
                except (KeyError, ValueError, TypeError):
                    note_drop()
                    continue


def emit_index_records(out, index_tree: dict, sid_map: dict) -> None:
    """Flatten get_index_value() `{exchange:{segment:{token:{value,tsInMillis}}}}`.

    Index value field is `value` (not `ltp`); stored as ltp with segment IDX_I.
    security_id MUST come from sid_map (the token may be a NAME) — no fallback.
    """
    if not isinstance(index_tree, dict):
        return
    for exchange, segs in index_tree.items():
        if not isinstance(segs, dict):
            continue
        for segment, tokens in segs.items():
            if not isinstance(tokens, dict):
                continue
            for token, tick in tokens.items():
                # A decoded index record with no `value` field — DROP (count it).
                if not isinstance(tick, dict) or "value" not in tick:
                    note_drop()
                    continue
                security_id = sid_map.get((str(exchange), str(segment), str(token)))
                # sid_map miss (index token may be a NAME — no fallback) — DROP.
                if security_id is None or security_id <= 0:
                    note_drop()
                    continue
                try:
                    ts_millis = int(tick.get("tsInMillis", 0))
                    _write_record(
                        out, security_id, CANONICAL_INDEX_SEGMENT, ts_millis, tick["value"]
                    )
                    note_emit()
                except (KeyError, ValueError, TypeError):
                    note_drop()
                    continue


def wait_for_subscriptions():
    """Block until the Rust watch file exists with >=1 entry; return its lists."""
    while True:
        watch_path = latest_watch_file(WATCH_DIR)
        if watch_path is not None:
            try:
                stocks, indices, sid_map = load_subscriptions(watch_path)
            except (OSError, ValueError) as exc:
                print(
                    f"groww sidecar: watch file unreadable ({type(exc).__name__}); retrying",
                    flush=True,
                )
                stocks, indices, sid_map = [], [], {}
            if stocks or indices:
                print(
                    f"groww sidecar: loaded {len(stocks)} stock + {len(indices)} index "
                    f"subscriptions from {os.path.basename(watch_path)}",
                    flush=True,
                )
                return stocks, indices, sid_map
        print(
            f"groww sidecar: waiting for Rust watch file in {WATCH_DIR} …",
            flush=True,
        )
        time.sleep(WATCH_POLL_SECS)


def silent_feed_watchdog(stocks: int, indices: int) -> None:
    """Warn LOUDLY (stderr) if subscribed but NO record decodes within the deadline.

    Runs as a daemon thread. The SDK's blocking `consume()` can swallow per-frame
    decode errors without raising, leaving us "subscribed but 0 ticks" with no
    actionable cause on stdout/stderr. This surfaces that truth: it samples the
    DECODED total (emitted + dropped) and, if it is still 0 after the first
    deadline, prints the most-likely causes; thereafter it re-warns periodically
    with the live counts until data flows, then goes quiet.
    """
    time.sleep(SILENT_FEED_FIRST_WARN_SECS)
    if EMITTED_TOTAL + DROPPED_TOTAL > 0:
        return  # data is decoding — nothing to warn about.
    print(
        f"groww sidecar: SILENT FEED — subscribed {stocks} stocks + {indices} "
        f"indices but received NO live records in {SILENT_FEED_FIRST_WARN_SECS}s. "
        "Auth succeeded (token acquired) and subscribe returned, so the most "
        "likely causes are: (1) this Groww account lacks a LIVE market-data "
        "feed entitlement (the socket connects but streams nothing); (2) the "
        "market is closed / pre-open for these instruments; (3) a Groww-side "
        "feed/socket reject the SDK is swallowing internally (look for the SDK's "
        "own 'Error:' lines above). The feed will keep retrying; it is NOT "
        "marked streaming until a real tick arrives.",
        file=sys.stderr,
        flush=True,
    )
    while EMITTED_TOTAL + DROPPED_TOTAL == 0:
        time.sleep(SILENT_FEED_REWARN_SECS)
        if EMITTED_TOTAL + DROPPED_TOTAL > 0:
            break
        print(
            "groww sidecar: STILL SILENT — 0 live records decoded "
            f"(emitted={EMITTED_TOTAL}, dropped={DROPPED_TOTAL}). "
            "See the first SILENT FEED diagnostic above for likely causes.",
            file=sys.stderr,
            flush=True,
        )


def main() -> None:
    api_key = os.environ.get("GROWW_API_KEY")
    totp_secret = os.environ.get("GROWW_TOTP_SECRET")
    if not api_key or not totp_secret:
        sys.exit("Set GROWW_API_KEY and GROWW_TOTP_SECRET in the environment.")

    os.makedirs(os.path.dirname(OUTPUT_PATH) or ".", exist_ok=True)
    out = open(OUTPUT_PATH, "a", buffering=1)  # line-buffered append
    print(f"groww sidecar → appending NDJSON to {OUTPUT_PATH}", flush=True)

    stock_list, index_list, sid_map = wait_for_subscriptions()

    # Secrets to mask out of any logged exception detail (never log their values).
    secrets = (api_key, totp_secret)

    # Cached access token, reused across FEED reconnects so a feed-connect /
    # subscribe / consume failure does NOT re-hit the rate-limited token endpoint.
    # We re-acquire ONLY when there is no token yet or the previous failure was an
    # auth-class error (token actually invalid/expired). This, plus the exponential
    # backoff below, is what stops the self-inflicted [auth] rate-limit storm.
    access_token = None
    # Count of consecutive failed cycles — drives the exponential backoff. Reset to
    # 0 after a fully successful cycle (auth OK + connected + consuming).
    consecutive_failures = 0
    # Start the silent-feed watchdog at most ONCE (the first time we reach a
    # successful subscribe), not per reconnect cycle — it watches the global
    # decoded counters for the whole process lifetime.
    watchdog_started = False

    # Reconnect loop — never give up (lock: not a single received tick missed).
    while True:
        # Track which phase fails so the log names auth vs feed-connect vs
        # subscribe vs consume (the cause is otherwise indistinguishable).
        phase = "auth"
        try:
            if access_token is None:
                totp = pyotp.TOTP(totp_secret).now()
                access_token = GrowwAPI.get_access_token(api_key=api_key, totp=totp)
                # Explicit auth-success signal — distinguishes "auth succeeded, feed
                # connect failed" from "auth failed". Log only the token LENGTH,
                # never the token value.
                print(
                    f"groww auth OK: access token acquired (len={len(access_token or '')})",
                    flush=True,
                )
            else:
                # Reusing the still-valid token from a previous successful auth —
                # no token-endpoint call, so no rate-limit pressure on reconnect.
                print("groww auth OK: reusing cached access token", flush=True)

            phase = "feed-connect"
            groww = GrowwAPI(access_token)
            feed = GrowwFeed(groww)

            phase = "subscribe"
            # Cache the subscribe counts so the first-emit `streaming` status write
            # (driven by note_emit on a REAL decoded tick) knows N stocks + M indices.
            global _SUBSCRIBED_STOCKS, _SUBSCRIBED_INDICES
            _SUBSCRIBED_STOCKS = len(stock_list)
            _SUBSCRIBED_INDICES = len(index_list)
            if stock_list:
                def on_ltp(_meta) -> None:
                    emit_ltp_records(out, feed.get_ltp(), sid_map)

                feed.subscribe_ltp(stock_list, on_data_received=on_ltp)
            if index_list:
                def on_index(_meta) -> None:
                    emit_index_records(out, feed.get_index_value(), sid_map)

                feed.subscribe_index_value(index_list, on_data_received=on_index)

            print(
                f"subscribed {len(stock_list)} stocks + {len(index_list)} indices "
                f"— awaiting first tick…",
                flush=True,
            )
            # Connect+subscribe PROOF (2026-06-28): write the atomic status the Rust
            # bridge reads → it emits the ONE structured CONNECT log + records the
            # subscribe counts in feed-health. Counts only, never a credential. This
            # is the honest "attempted" signal; it does NOT flip `connected=true`.
            write_status("subscribed", len(stock_list), len(index_list))
            # A full cycle succeeded up to the blocking consume — reset backoff so
            # the next genuine disconnect retries quickly, not at the capped delay.
            consecutive_failures = 0
            # Arm the silent-feed watchdog once: if the SDK's blocking consume()
            # streams nothing (swallowed feed reject / no entitlement / closed
            # market), it surfaces a loud, actionable diagnostic to stderr instead
            # of leaving the operator with only the SDK's empty "Error:" lines.
            if not watchdog_started:
                threading.Thread(
                    target=silent_feed_watchdog,
                    args=(len(stock_list), len(index_list)),
                    name="groww-silent-feed-watchdog",
                    daemon=True,
                ).start()
                watchdog_started = True
            phase = "consume"
            # HONEST-FEED FIX (2026-06-29): the optimistic pre-consume
            # write_status("streaming", …) was REMOVED. "streaming" (which flips the
            # bridge's connected=true) is now written ONLY by note_emit() on the
            # FIRST real decoded+emitted tick — never before any data flows. So a
            # subscribed-but-silent feed honestly reads NOT streaming.
            feed.consume()  # blocking
        except KeyboardInterrupt:
            print("stopping.", flush=True)
            break
        except Exception as exc:  # noqa: BLE001 - reconnect on any error
            consecutive_failures += 1
            rate_limited = _is_rate_limit_error(exc)
            retry_after = _retry_after_secs(exc)
            # Drop the cached token only on an auth-class failure (token actually
            # bad/expired) so the NEXT iteration re-acquires it. A feed-side failure
            # keeps the token cached → reconnect the feed without re-hitting the
            # rate-limited token endpoint. A rate-limit in the [auth] phase is NOT
            # an invalid token — keep any token we already have.
            if phase == "auth" and not rate_limited:
                access_token = None
            delay = _backoff_secs(consecutive_failures, rate_limited, retry_after)
            # Surface the WHY for triage, with every known secret value masked and
            # the detail length-capped: a Groww SDK HTTP error can embed the
            # response body (and thus the access token / api_key) in its
            # str/repr/response, which the Rust supervisor captures from stdout
            # (security-review MEDIUM 2026-06-19). `_exception_detail` redacts the
            # api_key + TOTP secret and caps length so the cause is visible without
            # leaking the credentials. We print to STDERR (errors belong there; the
            # supervisor captures both) with the exception TYPE + the full redacted
            # detail (now incl. the SDK `.msg`/`.code`, never just an empty str).
            rl_note = " [rate-limited — backing off longer]" if rate_limited else ""
            print(
                f"groww sidecar error [{phase}]: {type(exc).__name__}: "
                f"{_exception_detail(exc, secrets)}{rl_note} — reconnecting in "
                f"{delay:.0f}s (attempt {consecutive_failures})",
                file=sys.stderr,
                flush=True,
            )
            # Full traceback — surfaced on the FIRST failure and then every 100th so
            # a fast-looping `consume()` that returns-then-raises can never flood the
            # log, while the operator still always sees the real stack on the first
            # occurrence (the deliverable that unblocks diagnosis). Redacted + capped
            # so it can never leak the api_key / TOTP secret that an SDK frame's
            # locals/repr might embed.
            if consecutive_failures == 1 or consecutive_failures % 100 == 0:
                tb = _redact(traceback.format_exc(), secrets)
                if len(tb) > 4000:
                    tb = tb[:4000] + "…(traceback truncated)"
                print(
                    f"groww sidecar error [{phase}] traceback "
                    f"(failure #{consecutive_failures}):\n{tb}",
                    file=sys.stderr,
                    flush=True,
                )
            time.sleep(delay)


if __name__ == "__main__":
    main()
