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
Capture-at-receipt: write + flush + fsync per emitted record (durable floor one
hop downstream of the socket; lock §32.3). Since the 2026-07-03 lag fix the
NATS callbacks are O(1) dirty-flag sets and a walker thread drains the SDK
snapshot at a bounded cadence (WALK_INTERVAL_MS, default 200ms) — the SDK
snapshot only ever holds latest-per-instrument, so coalescing loses nothing
the callback-time walk could have seen, and it stops the full-tree-per-callback
decode work that starved the SDK's NATS consumer (the measured unbounded lag).

PER-CALLBACK INSTANT CAPTURE (2026-07-03, operator: "if ticks receivable within
1-3ms we must achieve the same latency"): the primary capture path now hooks
`feed._nats_client.callback` (the plain instance attribute the SDK's
`_on_data_cb` reads PER MESSAGE at nats_client.py:197 in growwapi==1.5.0) with
an ENQUEUE-ONLY wrapper (~µs: stamp time.time_ns() as capture_ns, put_nowait on
a BOUNDED queue — NEVER blocks the SDK's NATS consumer thread; on Full it drops
to a counter). A dedicated writer thread drains the queue in batches, decodes
the ONE protobuf per message via the SDK's own get_data_dict, maps the NATS
subject → instrument via a topic map built from the SDK's own topic builders,
applies the SAME change-dedup, and appends NDJSON records carrying a NEW
`capture_ns` field with ONE group-commit fsync per batch. Our-side added
latency drops from the walker's avg ~100ms to ≤1ms typical (≤5ms p99 under
GIL/fsync jitter — the honest Python envelope; the external Groww floor is
unchanged and now SEPARATELY measurable as capture_ns − tsInMillis). The
snapshot walker is DEMOTED to a slow reconciliation sweep
(GROWW_RECONCILE_INTERVAL_MS, default 5000ms) that catches anything the hook
missed; its emissions share the dedup cache so no duplicates. Kill-switch:
GROWW_CALLBACK_CAPTURE=off reverts to walker-only at WALK_INTERVAL_MS; a
failed hook attach (future-wheel attribute change) logs one line and falls
back to the walker path unchanged — the feature degrades, never breaks.

Volume is Option A (price-only, operator 2026-06-20): the Groww live feed carries
NO traded volume (only ltp/value + tsInMillis), so cum_volume is always 0.

Usage (auto-launched by the Rust supervisor; no manual run needed):
    export GROWW_SSM_TOKEN_PARAM=/tickvault/prod/groww/access-token
    python3 groww_sidecar.py
    # local dev without AWS: export GROWW_ACCESS_TOKEN=<token> instead

TOKEN ARCHITECTURE (shared token-minter lock 2026-07-02 —
.claude/rules/project/groww-shared-token-minter-2026-07-02.md): this sidecar
NEVER mints a Groww access token. The bruteX-owned `groww-token-minter` AWS
Lambda is the SOLE minter (daily ~06:05 IST, right after Groww's 06:00 IST
token reset); it writes the token to the SSM parameter named by
GROWW_SSM_TOKEN_PARAM. This sidecar READS that ONE parameter (read-only IAM
reader role, default credential chain) and builds GrowwAPI(access_token) +
GrowwFeed from it — token only, never credentials. On an auth-class failure
(the daily reset, a stale token) it drops the cached token and RE-READS the
parameter on the next cycle, paced at >=60s for auth failures; if the token is
still stale after ~10 minutes it prints ONE edge-triggered `GROWW LIVE FEED
REJECTED:` alert line (routed by the Rust supervisor to feed-health +
Telegram) and keeps retrying — it never mints (an uncoordinated mint can
invalidate the shared token mid-session for BruteX).
"""
import asyncio
import glob
import json
import logging
import os
import queue
import random
import re
import sys
import threading
import time
import traceback
from datetime import datetime, timezone, timedelta

# DIAGNOSTIC (2026-06-29): turn ON the Groww SDK's OWN logging so the per-frame
# decode errors it currently SWALLOWS (the bare "Error:" lines with no detail)
# print with full context to stderr. The SDK's blocking consume() decodes
# NATS/Protobuf internally and logs swallowed errors through the standard
# `logging` module at DEBUG/ERROR — but only if a handler is configured. Without
# this, those errors vanish. Routed to STDERR (the Rust supervisor captures both
# streams) and scoped to the `growwapi` logger at DEBUG; redaction of OUR
# credentials is unaffected because the SDK logs protocol detail, not our
# any secret (and the supervisor-captured stream is operator-local per lock §32).
logging.basicConfig(
    level=logging.DEBUG,
    stream=sys.stderr,
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
)
logging.getLogger("growwapi").setLevel(logging.DEBUG)
# The bare "Error:" lines originate from the SDK's NATS client
# (nats_client.py:151), which logs a SWALLOWED NATS permissions/authorization
# violation through the `nats` logger. Only the `nats` logger at DEBUG surfaces
# the real reason text (e.g. the actual "Authorization Violation"/subject the
# socket was rejected on) — without this it vanishes and we see the empty line.
logging.getLogger("nats").setLevel(logging.DEBUG)
# SECURITY (2026-07-03 High finding): the root logger is DEBUG (needed for the
# SDK/NATS decode diagnostics above), but botocore at DEBUG prints the SigV4
# CanonicalRequest — INCLUDING the full `x-amz-security-token` (the instance
# role's session token) — to stderr, which the Rust supervisor forwards to
# CloudWatch. The sidecar's redaction filter scrubs GROWW secrets, not
# botocore's own DEBUG output. Clamp every AWS/HTTP client logger to WARNING
# so no AWS credential material can reach the log stream. Applied BEFORE the
# deferred `import boto3` (loggers are configured by name, import-order-safe).
for _noisy_logger in ("botocore", "boto3", "urllib3", "s3transfer"):
    logging.getLogger(_noisy_logger).setLevel(logging.WARNING)

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
# The set of exchange names that legitimately appear as the TOP-LEVEL keys of a
# BARE get_ltp()/get_index_value() tree. Used to DEFENSIVELY UNWRAP a possible
# top-level wrapper (2026-06-29): our two Groww docs CONTRADICT on the get_ltp()
# shape — docs/groww-ref/07-feed-websocket.md shows it WRAPPED under a top-level
# "ltp" key (`{"ltp": {"NSE": {...}}}`), docs/groww-ref/10-live-feed-mapping-
# verified.md shows it BARE (`{"NSE": {...}}`). If the live SDK returns the
# WRAPPED form, the top-level key is the literal "ltp" (or a similar wrapper),
# no exchange matches, and EVERY tick is silently dropped → 0 ticks. We handle
# BOTH shapes by descending into a known wrapper key only when the inner dict's
# keys actually look like exchanges, so a correct BARE tree is NEVER mangled.
KNOWN_EXCHANGES = {"NSE", "BSE", "MCX", "NCDEX"}

# One-shot shape introspection (2026-06-29): the very first time each feed
# callback fires, print the REAL top-level keys of get_ltp()/get_index_value()
# to stderr ONCE so the NEXT run definitively reveals which doc shape is correct
# — zero ambiguity, no guessing. Module-level flags so it prints only once each.
_ltp_shape_logged = False
_index_shape_logged = False


def _unwrap_feed_tree(tree, wrapper_keys):
    """Descend through a possible top-level wrapper so BOTH doc shapes work.

    If `tree`'s top-level keys do NOT look like exchanges (none in
    KNOWN_EXCHANGES) AND one of `wrapper_keys` (e.g. "ltp"/"value") maps to a
    dict whose OWN keys DO look like exchanges, return that inner dict. Otherwise
    return `tree` unchanged. Conservative on purpose: a correct BARE tree (whose
    top-level keys are already exchanges) is returned untouched, never mangled.
    """
    if not isinstance(tree, dict):
        return tree
    if set(tree) & KNOWN_EXCHANGES:
        return tree  # already a BARE exchange-keyed tree — leave it alone.
    for wrapper_key in wrapper_keys:
        inner = tree.get(wrapper_key)
        if isinstance(inner, dict) and (set(inner) & KNOWN_EXCHANGES):
            return inner
    return tree
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

# ACTIVE self-heal (2026-06-30). The watchdog above only PRINTED; it never
# recovered. The Groww NATS server can close the socket WITHOUT raising (a
# swallowed "Authorization Violation"); `nats-py` closes the connection but never
# raises, so the blocking consume call never returns and the except→reconnect
# never fires — the feed sits dead forever (the live 10:31 IST incident). The
# watchdog now FORCE-CLOSES the NATS socket when DECODED records stall during
# market hours, so `consume()` returns and the reconnect loop re-subscribes.
# `STALL_DEADLINE_SECS` is the feed-level (whole-universe) silence window: at
# market open ticks flow every second across the ~767-SID universe, so a short
# silence is a real dead socket, not illiquidity. It is the IN-PROCESS fast path;
# the Rust supervisor's process-kill stall-watchdog is the slower backstop.
STALL_DEADLINE_SECS = 5
# How often the watchdog samples the decoded counters while consuming.
STALL_POLL_SECS = 1
# Stall self-heal exponential backoff (2026-07-06 exam fix — session churn
# storm). On a starved/thin fleet shard the FIXED 5s force-close loop fired
# 1,393 times in one exam hour: every force-close opens a NEW Groww NATS
# session while the dead ones pile up against the account's limited
# concurrent-session slots (~33), WORSENING the starvation for every shard —
# including after restarts. Fix: the FIRST stall episode force-closes after
# STALL_DEADLINE_SECS exactly as before; each CONSECUTIVE stall episode
# WITHOUT an intervening real decoded live record (a watermark advance)
# DOUBLES the wait before the next force-close — 5s → 10s → 20s → 40s →
# capped at STALL_BACKOFF_CAP_SECS. The ladder resets to the fast 5s base
# ONLY after a DWELL of sustained health (see the hostile-review hardening
# below), so a healthy feed regains today's fast detection within ~1 minute
# of genuine recovery. Pure math in `compute_stall_backoff_secs` +
# `should_reset_stall_backoff` (self-tested + test_dedup.py).
STALL_BACKOFF_CAP_SECS = 60.0
# Overflow guard for the ladder exponent: 2**32 already dwarfs any cap, so
# clamping the exponent keeps the float math exact without changing behavior.
STALL_BACKOFF_MAX_EXPONENT = 32
# Hostile-review hardening (2026-07-06, exam-fix round 2): the ladder MUST NOT
# reset on a SINGLE watermark advance. `_note_ts_advance` advances the
# watermark for EVERY decoded record BEFORE dedup/drop, and a starved/thin
# shard's dominant flap mode is: force-close → reconnect → re-subscribe
# snapshot delivers ≥1 record with a fresh exchange ts (indices tick every
# second) → session starves again. A single-advance reset would zero the
# ladder every cycle, pinning the force-close cadence at the 5s base — the
# exact 1,393-fires/hour churn storm this backoff exists to stop. So the
# reset is DWELL-GATED: the ladder returns to the 5s base only when a
# watermark advance is observed AND at least
# STALL_BACKOFF_RESET_DWELL_SECS have passed since the LAST force-close
# (either criterion). Set equal to the cap: in the flap mode every fire is
# ≤ cap seconds after the previous advance, so the advance always lands
# inside the dwell and the ladder holds at the cap (~60 fires/hour/shard
# worst case); a genuinely recovered feed streams continuously, so its next
# advance after the dwell expires restores the fast 5s detection.
STALL_BACKOFF_RESET_DWELL_SECS = 60.0
# Cold-silent diagnostics rate limit (same exam fix, item 3): once the feed
# has been persistently silent long enough that the stall ladder would sit at
# its cap (STILL_SILENT_FAST_WARNS re-warns — the ladder depth 5→10→20→40→60),
# the repeating "STILL SILENT" reminder carries no new information. It then
# slows from the 60s cadence to at most ONE line per
# STILL_SILENT_RATE_LIMIT_SECS per process. Pure cadence decision in
# `still_silent_rewarn_interval_secs` (self-tested).
STILL_SILENT_RATE_LIMIT_SECS = 300.0
STILL_SILENT_FAST_WARNS = 5
# NSE trading window [09:15, 15:30) IST — the market-hours gate for the ms
# reconnect ladder + the force-reconnect (off-hours silence is NORMAL; don't
# fight a legitimately-idle feed).
MARKET_OPEN_SEC_OF_DAY = 9 * 3600 + 15 * 60   # 09:15:00 IST, inclusive
MARKET_CLOSE_SEC_OF_DAY = 15 * 3600 + 30 * 60  # 15:30:00 IST, exclusive
# Market-hours reconnect ladder: start at 50ms and double to a 5s cap so a drop
# during the session reconnects in milliseconds (operator: "reconnects within
# seconds and re-subscribes"). NEVER give up while the market is open.
MARKET_OPEN_RECONNECT_BASE_SECS = 0.05
MARKET_OPEN_RECONNECT_CAP_SECS = 5.0

# Coalesced snapshot walks (2026-07-03 lag forensics — THE cure). Before this
# fix every NATS callback walked the ENTIRE 768-entry get_ltp()/
# get_index_value() snapshot (~42 full walks/sec ≈ 32,500 record decodes/sec of
# pure Python on one core), starving the SDK's NATS consumer so its snapshot —
# our ONLY data source — fell behind wall-clock UNBOUNDED (measured 8s → 428s
# over 13 min, consuming exchange-time at 0.53× real-time). Callbacks now do
# ONLY an O(1) dirty-flag set; a dedicated walker thread drains each dirty
# snapshot at most once per WALK_INTERVAL_MS. Net: ≤ 2×5 walks/sec instead of
# ~42, the NATS consumer runs real-time, watermark lag bounded to ≈ one walk
# interval + drain cadence. The walk bodies + change-dedup are UNCHANGED.
WALK_INTERVAL_MS_DEFAULT = 200
# Clamp bounds for the env override (GROWW_WALK_INTERVAL_MS): a 0/negative
# interval must never spin-loop the walker; a huge one must never stall the
# feed behind a fat coalesce window.
WALK_INTERVAL_MS_MIN = 20
WALK_INTERVAL_MS_MAX = 5000

# Per-callback instant capture (2026-07-03 ≤1ms latency study, variant c2).
# The NATS-layer hook enqueues (subject, raw_payload, capture_ns) per message
# onto this BOUNDED queue; the writer thread drains it. 65,536 slots ≈ 25+
# minutes of buffer at the measured ~42 msgs/sec steady rate (and > 1 minute
# even at a full-universe 768 msgs/sec burst) — if the writer thread ever
# falls that far behind, the hook DROPS to a counter instead of blocking the
# SDK's NATS consumer thread (the #1344 starvation class must never recur).
CAPTURE_QUEUE_MAX = 65536
# Max records drained per group-commit batch (one flush+fsync per batch). A
# bound keeps worst-case batch latency small; queue.Empty ends a batch early.
CAPTURE_BATCH_MAX = 512
# With the hook active the walker is DEMOTED to a slow reconciliation sweep:
# it re-walks the SDK snapshot at this cadence to catch anything the hook
# missed (its emissions share the change-dedup cache, so no duplicates).
RECONCILE_INTERVAL_MS_DEFAULT = 5000
RECONCILE_INTERVAL_MS_MIN = 1000
RECONCILE_INTERVAL_MS_MAX = 60000
# Bounded stderr CAPTURE-STATS heartbeat cadence (writer thread).
CAPTURE_STATS_INTERVAL_SECS = 300.0

# Watermark-lag stall criterion (2026-07-03 lag forensics — fix #2). The
# timestamp-ADVANCING liveness (the 5s frozen-watermark criterion below + the
# Rust 30s FEED-STALL-01) has a blind spot: a +2 ms micro-advance resets the
# clock while the watermark drifts MINUTES behind wall-clock (measured: 0 stall
# events in 90 min despite a 7-min lag). During market hours, a watermark lag
# (now_ms − max_ts_millis) STRICTLY greater than this threshold for
# WATERMARK_LAG_CONSECUTIVE_CHECKS consecutive 1s polls is treated as stalled
# and force-closes the NATS socket — the SAME restart path as the frozen
# criterion. tsInMillis is UTC epoch ms, so the comparison clock is
# time.time()*1000 (UTC ms) — no IST offset anywhere in this math.
WATERMARK_MAX_LAG_MS_DEFAULT = 120_000
WATERMARK_MAX_LAG_MS_MIN = 10_000
WATERMARK_MAX_LAG_MS_MAX = 3_600_000
WATERMARK_LAG_CONSECUTIVE_CHECKS = 3
# Refire cooldown: a backlog that survives one reconnect refires at a bounded
# ~3-min cadence, never a 3s kill storm (the reconnected feed needs time to
# re-seed its snapshot and catch the watermark up to wall-clock).
WATERMARK_LAG_REFIRE_COOLDOWN_SECS = 180.0


def _resolve_env_int(raw, default: int, lo: int, hi: int) -> int:
    """Pure clamp for an integer env override: garbage/absent → default;
    otherwise clamped to [lo, hi]. Never raises."""
    try:
        value = int(float(raw))
    except (TypeError, ValueError):
        return default
    return max(lo, min(value, hi))


def resolve_walk_interval_ms(raw) -> int:
    """Pure: GROWW_WALK_INTERVAL_MS env value → clamped walk interval (ms)."""
    return _resolve_env_int(
        raw, WALK_INTERVAL_MS_DEFAULT, WALK_INTERVAL_MS_MIN, WALK_INTERVAL_MS_MAX
    )


def resolve_reconcile_interval_ms(raw) -> int:
    """Pure: GROWW_RECONCILE_INTERVAL_MS env → clamped reconcile-sweep interval
    for the demoted walker while per-callback capture is active. Garbage/absent
    → 5000ms default; clamped to [1000, 60000]. Never raises."""
    return _resolve_env_int(
        raw,
        RECONCILE_INTERVAL_MS_DEFAULT,
        RECONCILE_INTERVAL_MS_MIN,
        RECONCILE_INTERVAL_MS_MAX,
    )


def resolve_callback_capture_enabled(raw) -> bool:
    """Pure kill-switch: GROWW_CALLBACK_CAPTURE env → capture on/off.
    Default ON (unset/empty/anything else); `off`/`0`/`false`/`no`
    (case-insensitive) turn it OFF → walker-only at WALK_INTERVAL_MS."""
    return (raw or "").strip().lower() not in ("off", "0", "false", "no")


def resolve_watermark_max_lag_ms(raw) -> int:
    """Pure: GROWW_WATERMARK_MAX_LAG_MS env value → clamped lag threshold (ms)."""
    return _resolve_env_int(
        raw,
        WATERMARK_MAX_LAG_MS_DEFAULT,
        WATERMARK_MAX_LAG_MS_MIN,
        WATERMARK_MAX_LAG_MS_MAX,
    )


def watermark_lag_stalled(
    now_ms: float, max_ts_millis: int, market_open: bool, lag_threshold_ms: int
) -> bool:
    """PURE per-check verdict (2026-07-03 lag forensics fix #2): is the feed
    watermark lagging wall-clock beyond the threshold?

    True ONLY when the market is open, the watermark is KNOWN (> 0 — a
    cold/never-streamed feed is owned by the silent-feed diagnostic + the Rust
    process-kill backstop, never by this criterion), and the lag is STRICTLY
    greater than `lag_threshold_ms` (119s no / 120s no / 121s yes at the
    default 120_000). Off-hours lag is NORMAL (overnight watermark = yesterday
    15:29) → never stalled. O(1), no I/O; unit-tested in test_dedup.py."""
    if not market_open:
        return False
    if max_ts_millis <= 0:
        return False
    return (now_ms - max_ts_millis) > lag_threshold_ms


def watermark_lag_should_fire(
    consecutive_stalled_checks: int, needed: int, cooldown_remaining_secs: float
) -> bool:
    """PURE fire decision: force-reconnect only after `needed` CONSECUTIVE
    stalled verdicts AND outside the refire cooldown (a persistent backlog
    refires at a bounded cadence, never a kill storm). O(1), no I/O."""
    if cooldown_remaining_secs > 0.0:
        return False
    return consecutive_stalled_checks >= needed


# NATS reject-reason surfacing (2026-06-29). VERIFIED against growwapi-1.5.0 +
# nats-py 2.15.0 source (quoted in groww_sidecar.py header / the plan):
#   - growwapi.groww.feed.GrowwFeed holds its NATS client at `feed._nats_client`
#     (feed.py:149); the underlying nats.aio.client.Client is `_nats_client._socket`
#     (nats_client.py:51).
#   - The bare empty "Error:" line is growwapi.groww.nats_client.NatsClient
#     ._on_error_cb (nats_client.py:150-151): `logger.error("Error: %s", e)`.
#   - nats-py Client._process_err routes a server "-ERR":
#       * "Authorization Violation" -> stores errors.AuthorizationError() in
#         Client._err and CLOSES the connection — it NEVER calls _error_cb, so
#         the real reason reaches NO growwapi callback; it survives ONLY in
#         `_socket._err` / `_socket.last_error` (a property returning `_err`).
#       * "...Permissions Violation..." -> stores errors.Error(msg) in _err AND
#         calls _error_cb(err) -> the growwapi "Error:" line (str non-empty).
#   So the reliable, durable source of the REAL reason for BOTH classes is the
#   underlying socket's `last_error` / `_err`. We (a) wrap the SDK's own
#   callbacks so the empty "Error:" becomes a real repr(), and (b) poll the
#   socket's last_error and print one edge-triggered GROWW LIVE FEED REJECTED
#   line + a periodic heartbeat. ALL of this is best-effort: any attribute
#   mismatch on a future wheel is caught and logged, never breaks the sidecar.
NATS_REASON_POLL_SECS = 2
NATS_REASON_HEARTBEAT_SECS = 60
# Substrings that mark a true authorization / permissions / protocol reject.
_REJECT_MARKERS = (
    "authorization",
    "permissions",
    "authorization violation",
    "permissions violation",
    "-err",
)


def _nats_socket_from_feed(feed):
    """Best-effort reach the underlying nats.aio.client.Client from a GrowwFeed.

    Returns the socket Client or None. Wrapped so a future-wheel attribute rename
    NEVER breaks the sidecar — on any failure we log the type once and return None
    (the silent-feed watchdog still covers the "0 ticks" case).
    """
    try:
        nats_client = getattr(feed, "_nats_client", None)
        if nats_client is None:
            return None
        return getattr(nats_client, "_socket", None)
    except Exception as exc:  # noqa: BLE001 - reason-surfacing must never break the feed
        print(
            f"groww sidecar: could not reach NATS socket ({type(exc).__name__}); "
            "reason-surfacing degraded (silent-feed watchdog still active)",
            file=sys.stderr,
            flush=True,
        )
        return None


def _force_close_nats_socket(feed) -> bool:
    """Best-effort FORCE the blocking consume call to return by closing the
    underlying NATS socket from the watchdog thread, so the except→reconnect loop
    runs and re-subscribes (the active self-heal for the swallowed-close case that
    left the feed dead at 10:31 IST). Returns True if a close was scheduled.

    nats-py's `Client.close()` is a coroutine running on the SDK's own asyncio
    loop; we schedule it thread-safely with `run_coroutine_threadsafe` against the
    socket's loop. ALL of this is best-effort and wrapped: any attribute mismatch
    on a future wheel is caught + logged and returns False (the Rust supervisor's
    process-kill stall-watchdog is the backstop). It NEVER raises out of the
    watchdog thread."""
    socket = _nats_socket_from_feed(feed)
    if socket is None:
        return False
    try:
        close_coro = getattr(socket, "close", None)
        loop = getattr(socket, "_loop", None)
        if close_coro is None or loop is None:
            return False
        # Schedule the async close on the SDK's event loop from this thread.
        asyncio.run_coroutine_threadsafe(close_coro(), loop)
        return True
    except Exception as exc:  # noqa: BLE001 - self-heal must never break the feed
        print(
            f"groww sidecar: force-close of NATS socket failed ({type(exc).__name__}); "
            "relying on the Rust supervisor process-kill backstop",
            file=sys.stderr,
            flush=True,
        )
        return False


def _socket_error_detail(socket, secrets):
    """Return a redacted, non-empty repr of the socket's last error, or None.

    Reads `last_error` (the public property -> Client._err) then `_err` directly.
    repr() is used because some nats-py errors (e.g. AuthorizationError) render an
    informative repr while their str may be terse; we surface both when they differ.
    """
    if socket is None:
        return None
    err = None
    try:
        err = getattr(socket, "last_error", None)
    except Exception:  # noqa: BLE001 - property access must never raise out
        err = None
    if err is None:
        err = getattr(socket, "_err", None)
    if err is None:
        return None
    err_repr = _redact(repr(err), secrets)
    err_str = _redact(str(err), secrets)
    if err_str and err_str != err_repr:
        return f"{err_repr} ({err_str})"
    return err_repr


def _is_reject_reason(detail: str) -> bool:
    """True if `detail` looks like an authorization / permissions / -ERR reject."""
    if not detail:
        return False
    low = detail.lower()
    return any(marker in low for marker in _REJECT_MARKERS)


def install_nats_reason_hooks(feed, secrets) -> None:
    """Replace the SDK's empty "Error:" callbacks with REAL-detail versions.

    Monkeypatches the SDK NatsClient instance bound to THIS feed so its
    `_on_error_cb` / `_on_closed_cb` / `_on_disconnected_cb` print the real
    exception repr() + the socket's stored last_error instead of an empty string.
    Best-effort: any failure is logged (type only) and ignored — the original SDK
    callbacks keep working and the poller below is the backstop.
    """
    try:
        nats_client = getattr(feed, "_nats_client", None)
        if nats_client is None:
            return
        socket = getattr(nats_client, "_socket", None)

        async def on_error(e):  # mirrors growwapi NatsClient._on_error_cb signature
            detail = _redact(repr(e), secrets)
            str_detail = _redact(str(e), secrets)
            if str_detail and str_detail != detail:
                detail = f"{detail} ({str_detail})"
            print(
                f"groww sidecar: NATS error_cb -> {detail}",
                file=sys.stderr,
                flush=True,
            )

        async def on_closed():
            detail = _socket_error_detail(socket, secrets) or "(no stored error)"
            print(
                f"groww sidecar: NATS connection closed -> last_error={detail}",
                file=sys.stderr,
                flush=True,
            )

        async def on_disconnected():
            detail = _socket_error_detail(socket, secrets) or "(no stored error)"
            print(
                f"groww sidecar: NATS disconnected -> last_error={detail}",
                file=sys.stderr,
                flush=True,
            )

        # Bind as the instance's callbacks. The SDK passes these to nats.connect()
        # at __init__ time (already happened), so nats-py already holds references
        # to the ORIGINAL bound methods. We therefore ALSO replace the attributes
        # for any code that re-reads them, AND rely on the poller below as the
        # guaranteed backstop (nats-py keeps the old refs). The poller is what makes
        # this robust regardless of when nats captured the callbacks.
        nats_client._on_error_cb = on_error
        nats_client._on_closed_cb = on_closed
        nats_client._on_disconnected_cb = on_disconnected
    except Exception as exc:  # noqa: BLE001 - never break the feed
        print(
            f"groww sidecar: could not install NATS reason hooks "
            f"({type(exc).__name__}); relying on the last_error poller",
            file=sys.stderr,
            flush=True,
        )


def nats_reject_poller(feed, secrets) -> None:
    """Daemon thread: surface the REAL NATS reject reason from the socket.

    nats-py stores the swallowed reason in Client._err / Client.last_error even
    when (for an "Authorization Violation") it never calls any growwapi callback.
    We poll that store and, on a NEW reject-class reason, print ONE loud
    `GROWW LIVE FEED REJECTED: <reason>` line (edge-triggered — never spammed every
    poll). Thereafter a periodic heartbeat repeats the current reason so the
    operator keeps proof without log flooding. The raw last_error is also printed
    on each CHANGE so a multi-step reject sequence is fully captured.
    """
    socket = _nats_socket_from_feed(feed)
    if socket is None:
        return  # nothing to poll; the silent-feed watchdog still covers 0-ticks.
    last_reported = None
    last_heartbeat = 0.0
    while True:
        time.sleep(NATS_REASON_POLL_SECS)
        # Stop once real data flows — a reject that streams is not a reject.
        if EMITTED_TOTAL > 0:
            return
        detail = _socket_error_detail(socket, secrets)
        if detail and detail != last_reported:
            # New / changed stored error — always print the raw detail.
            print(
                f"groww sidecar: NATS last_error -> {detail}",
                file=sys.stderr,
                flush=True,
            )
            if _is_reject_reason(detail):
                print(
                    f"GROWW LIVE FEED REJECTED: {detail}",
                    file=sys.stderr,
                    flush=True,
                )
            last_reported = detail
            last_heartbeat = time.monotonic()
            continue
        # Heartbeat: repeat the standing reject reason periodically (count too).
        if (
            last_reported
            and _is_reject_reason(last_reported)
            and time.monotonic() - last_heartbeat >= NATS_REASON_HEARTBEAT_SECS
        ):
            print(
                f"groww sidecar: GROWW LIVE FEED STILL REJECTED: {last_reported} "
                f"(emitted={EMITTED_TOTAL}, dropped={DROPPED_TOTAL})",
                file=sys.stderr,
                flush=True,
            )
            last_heartbeat = time.monotonic()

# Honest-feed counters (operator 2026-06-29). EMITTED_TOTAL = records we DECODED
# and successfully wrote as a tick; DROPPED_TOTAL = records we DECODED but had to
# drop (a sid_map miss, or a missing ltp/value field) — previously a SILENT
# `continue`. The Rust bridge surfaces both via feed-health so a key-map mismatch
# ("streaming but 0 ticks") is visible instead of invisible.
EMITTED_TOTAL = 0
DROPPED_TOTAL = 0
# Change-dedup (2026-07-03 live incident): DEDUPED_TOTAL = decoded snapshot
# entries SKIPPED because they were byte-identical re-dumps of the last emitted
# (tsInMillis, price) for that instrument. The SDK's get_ltp()/get_index_value()
# return the WHOLE snapshot tree on EVERY NATS callback, so without this the
# sidecar re-emitted ALL ~25 subscribed indices per callback (~525 rows/sec of
# frozen duplicates = 530K junk rows 09:00–09:17 IST, one fsync each). A skip is
# neither an emit nor a drop — deliberately, so the decoded counter
# (EMITTED+DROPPED) that drives the stall self-heal goes quiet on a frozen
# snapshot flood and `should_force_reconnect` fires as designed.
DEDUPED_TOTAL = 0
# last-emitted cache: (kind, exchange, segment, token) -> (ts_millis, price).
# Naturally bounded by the subscribed universe (Groww hard cap 1000 instruments;
# ~768 + 25 today). The `kind` discriminant ("ltp" vs "idx") keeps a BSE index
# token "1" distinct from a hypothetical BSE stock token "1".
_LAST_EMITTED = {}
# Per-reason drop breakdown (2026-07-03 forensics: dropped=276,077 with no
# breakdown left the "stocks 100% dropped on a key-map miss?" hypothesis
# unconfirmable from logs). reason -> count; carried in the status file. A
# capped number of SAMPLE lines per reason (market-data identifiers only —
# never a credential) names the exact keys involved.
DROP_REASONS = {}
_DROP_SAMPLES_LOGGED = {}
DROP_SAMPLE_LIMIT_PER_REASON = 5
# Per-callback capture counters (2026-07-03). Single-writer-per-counter
# threading model (documented, no locks needed for the counters themselves):
#   _HOOK_ENQUEUED / _HOOK_DROPPED_FULL — written ONLY by the SDK's NATS
#     consumer thread (inside the enqueue-only hook); read by the writer
#     thread + status writer (a stale read is advisory-only).
#   CAPTURE_EMITTED_TOTAL — written ONLY by the capture writer thread.
#   RECONCILE_EMITTED_TOTAL — written ONLY by the walker thread (counts its
#     reconcile-sweep emissions while capture is active — i.e. what the hook
#     path MISSED; steady non-zero growth means the hook is degraded).
# One-element list cells so the hook closure mutates by reference (same
# pattern as _CURRENT_FEED); item assignment is atomic under the GIL.
_HOOK_ENQUEUED = [0]
_HOOK_DROPPED_FULL = [0]
CAPTURE_EMITTED_TOTAL = 0
RECONCILE_EMITTED_TOTAL = 0
# NDJSON out-handle lock: with the capture writer thread AND the walker
# (reconcile) thread both appending records, interleaved partial writes /
# rotation races must be impossible. Every record write (and the writer
# thread's group-commit flush+fsync) holds this lock; the critical section is
# one line write (+ optional fsync), so contention is µs-scale.
_OUT_LOCK = threading.Lock()
# Status-file lock: write_status uses a pid-based temp name, which two threads
# in the SAME process would collide on. Serialize the temp+rename.
_STATUS_LOCK = threading.Lock()
# Max exchange timestamp (tsInMillis) seen across ALL decoded records this
# process lifetime + the monotonic instant it last ADVANCED. This is the stall
# detector's liveness signal (2026-07-03 feed-death forensics): the 09:07:55
# frozen snapshot kept EMITTED_TOTAL climbing for 31 minutes, so a decode-count
# detector saw a "live" feed while Groww delivered nothing fresh. Updated for
# every decoded record (emitted, deduped, OR dropped — a fresh-but-mismapped
# record is still proof the SOCKET is alive; force-reconnect cannot fix a
# mapping bug and must not storm on one).
MAX_TS_MILLIS_SEEN = 0
_LAST_TS_ADVANCE_MONOTONIC = 0.0
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


def _sec_of_day_ist(now_utc_epoch: float) -> int:
    """IST second-of-day [0, 86400) for a UTC epoch. Pure (clock injected)."""
    ist_epoch = now_utc_epoch + 5.5 * 3600
    return int(ist_epoch) % 86400


def _is_within_market_hours_ist(now_utc_epoch: float) -> bool:
    """True iff `now` is inside the NSE window [09:15, 15:30) IST (start-inclusive,
    end-exclusive). Pure — the clock is injected so it is unit-testable. Mirrors
    the Rust `is_within_market_hours_ist` gate so the Python ms ladder + the
    force-reconnect agree with the Rust stall-watchdog (no fighting a
    legitimately-idle off-hours feed)."""
    sec = _sec_of_day_ist(now_utc_epoch)
    return MARKET_OPEN_SEC_OF_DAY <= sec < MARKET_CLOSE_SEC_OF_DAY


def should_force_reconnect(
    decoded_total: int,
    last_decode_total: int,
    secs_since_change: float,
    market_open: bool,
    deadline_secs: float,
) -> bool:
    """Pure decision: should the silent-feed watchdog FORCE the blocking
    `consume()` to return (by closing the NATS socket) so the reconnect loop
    re-subscribes? True ONLY when the market is open, at least one record decoded
    before (so we are not in the cold pre-first-tick case the diagnostic covers),
    the decoded total has NOT advanced, and the silence has lasted past the
    deadline. Off-hours silence is normal → never force-reconnect. O(1), no I/O."""
    if not market_open:
        return False
    if decoded_total == 0:
        # No record EVER decoded — cold pre-open / entitlement case; the silent-
        # feed diagnostic + the Rust process-kill backstop handle it, not a
        # close-loop here (closing a never-streamed socket just churns).
        return False
    if decoded_total != last_decode_total:
        return False  # data is still flowing — not stalled.
    return secs_since_change >= deadline_secs


def compute_stall_backoff_secs(
    consecutive_stalls: int, base_secs: float, cap_secs: float
) -> float:
    """Pure exponential-backoff ladder for the stall self-heal force-close
    (2026-07-06 exam fix). `consecutive_stalls` = stall episodes already fired
    WITHOUT an intervening real decoded live record (watermark advance).

    0 (fresh / just reset) → base_secs (today's fast 5s detection);
    N ≥ 1 → min(base_secs * 2**N, cap_secs): 5 → 10 → 20 → 40 → 60 cap.
    Negative counts clamp to base; huge counts clamp the exponent (overflow
    guard) and land on the cap. O(1), no I/O, never raises."""
    if consecutive_stalls <= 0:
        return base_secs
    exponent = min(consecutive_stalls, STALL_BACKOFF_MAX_EXPONENT)
    return min(base_secs * (2.0**exponent), cap_secs)


def should_reset_stall_backoff(secs_since_last_fire, dwell_secs: float) -> bool:
    """Pure dwell-gated reset decision for the stall force-close backoff
    ladder (2026-07-06 hostile-review hardening — see the
    STALL_BACKOFF_RESET_DWELL_SECS comment). Called when a watermark advance
    is observed. Returns True ONLY when the session has been stall-free for
    at least `dwell_secs` since the LAST force-close — i.e. the advance is
    part of sustained recovery, not the single post-reconnect snapshot record
    that every flap cycle produces. `secs_since_last_fire is None` means no
    force-close has fired yet this process (nothing to distrust — reset is
    allowed; the ladder is 0 anyway). O(1), no I/O, never raises."""
    if secs_since_last_fire is None:
        return True
    return secs_since_last_fire >= dwell_secs


def still_silent_rewarn_interval_secs(
    warns_so_far: int,
    fast_interval_secs: float,
    fast_warns: int,
    slow_interval_secs: float,
) -> float:
    """Pure cadence decision for the cold-silent "STILL SILENT" reminder
    (2026-07-06 exam fix, item 3). The first `fast_warns` reminders keep the
    fast cadence (operator sees the problem promptly); after that the feed is
    persistently starved — equivalent to the stall ladder sitting at its cap —
    and the reminder slows to `slow_interval_secs` (at most one line per ~5
    minutes per process). O(1), no I/O, never raises."""
    if warns_so_far < fast_warns:
        return fast_interval_secs
    return slow_interval_secs


# ---------------------------------------------------------------------------
# Capture-file rotation (PR-3, 2026-07-02) — the WRITER owns rotation.
# At the first write after IST midnight the current file is archived to
# live-ticks-YYYYMMDD.ndjson (the COMPLETED IST day) and a fresh file opens;
# archives older than NDJSON_ARCHIVE_KEEP_DAYS are deleted (the rows are in
# QuestDB, cross-verified at 15:31 + conservation-audited at 15:40 daily — the
# archive is only a crash-recovery window). Rotation happens BETWEEN records
# (never splits a line) and ONLY at the IST day boundary: the market is closed
# at midnight, so the Rust bridge's per-file capture_seq restart can never
# collide same-second dedup keys across the boundary. A rotation failure NEVER
# stops capture — the sidecar keeps appending to the current file.
# ---------------------------------------------------------------------------

NDJSON_ARCHIVE_KEEP_DAYS = 2


def _ist_day(now_secs: float) -> int:
    """IST calendar-day ordinal (pure): days since epoch of the IST wall date."""
    return int((now_secs + 19800) // 86400)


def _ist_date_str(day_ordinal: int) -> str:
    """YYYYMMDD for an IST day ordinal (pure)."""
    return (datetime(1970, 1, 1, tzinfo=timezone.utc) + timedelta(days=day_ordinal)).strftime("%Y%m%d")


def _archive_path(base: str, day_ordinal: int) -> str:
    """Dated archive path for the COMPLETED IST day (pure)."""
    root, ext = os.path.splitext(base)
    return f"{root}-{_ist_date_str(day_ordinal)}{ext}"


def _archives_to_delete(paths: list, base: str, today_ordinal: int, keep_days: int) -> list:
    """Pure retention selector: dated archives older than keep_days."""
    root, ext = os.path.splitext(os.path.basename(base))
    out = []
    for p in paths:
        name = os.path.basename(p)
        if not (name.startswith(root + "-") and name.endswith(ext)):
            continue
        stamp = name[len(root) + 1 : len(name) - len(ext)]
        if len(stamp) != 8 or not stamp.isdigit():
            continue
        try:
            d = datetime.strptime(stamp, "%Y%m%d").replace(tzinfo=timezone.utc)
        except ValueError:
            continue
        ordinal = int(d.timestamp() + 19800) // 86400 if False else (d - datetime(1970, 1, 1, tzinfo=timezone.utc)).days
        if today_ordinal - ordinal > keep_days:
            out.append(p)
    return out


class _RotatingOut:
    """Line-buffered append handle that rotates at the IST day boundary.

    Duck-typed drop-in for the plain file handle (`write`/`flush`/`fileno`).
    The rotation check is one integer compare per write (O(1)).
    """

    def __init__(self, path: str):
        self._path = path
        self._fh = open(path, "a", buffering=1)
        self._day = _ist_day(time.time())
        self._rotate_error_printed = False

    def _maybe_rotate(self) -> None:
        now_day = _ist_day(time.time())
        if now_day == self._day:
            return
        completed = self._day
        # Advance the day FIRST: a failed rotation retries at the NEXT
        # boundary, never per-write (capture must not churn on a bad disk).
        self._day = now_day
        try:
            self._fh.flush()
            os.fsync(self._fh.fileno())
            self._fh.close()
            archive = _archive_path(self._path, completed)
            os.replace(self._path, archive)
            print(
                f"groww capture rotated: {archive} (completed IST day) -> fresh {self._path}",
                flush=True,
            )
            for stale in _archives_to_delete(
                glob.glob(_archive_path(self._path, 0).replace("19700101", "*")),
                self._path,
                now_day,
                NDJSON_ARCHIVE_KEEP_DAYS,
            ):
                try:
                    os.remove(stale)
                    print(f"groww capture archive removed (retention): {stale}", flush=True)
                except OSError as exc:
                    print(
                        f"groww sidecar error [rotate-retention]: {exc} — will retry next midnight",
                        file=sys.stderr,
                        flush=True,
                    )
        except Exception as exc:  # noqa: BLE001 - rotation must never stop capture
            if not self._rotate_error_printed:
                print(
                    f"groww sidecar error [rotate]: {type(exc).__name__}: {exc} — "
                    "capture continues on the current file (retry next midnight)",
                    file=sys.stderr,
                    flush=True,
                )
                self._rotate_error_printed = True
        finally:
            # Reopen: the fresh file after a successful rename, or the same
            # file if the rename failed — capture NEVER stops for rotation.
            if self._fh.closed:
                self._fh = open(self._path, "a", buffering=1)

    def write(self, s: str) -> int:
        self._maybe_rotate()
        return self._fh.write(s)

    def flush(self) -> None:
        self._fh.flush()

    def fileno(self) -> int:
        return self._fh.fileno()


# ---------------------------------------------------------------------------
# Shared-token acquisition (READ-ONLY — the bruteX groww-token-minter Lambda is
# the sole minter; lock 2026-07-02). No TOTP generation, no SDK mint call, ever.
# ---------------------------------------------------------------------------

# Floor for retry pacing when the failure is auth-class (stale/unusable token):
# ~60s rides the daily 06:00->06:05 IST mint gap without hammering anything.
AUTH_RETRY_FLOOR_SECS = 60
# Safety net: every Nth consecutive NON-auth-shaped failure also drops the
# cached token (forces a fresh SSM read next cycle) — so a daily reset that
# surfaces as a bare socket close can never loop a stale token forever.
TOKEN_REREAD_EVERY_N_FAILURES = 5
# After this many seconds of CONTINUOUS auth-class failure, print ONE
# edge-triggered `GROWW LIVE FEED REJECTED:` alert line (the Rust supervisor
# routes it to feed-health + Telegram) — then keep retrying, NEVER mint.
TOKEN_STALE_ALERT_SECS = 600


def _read_access_token():
    """Read the shared Groww access token — env override, else SSM (read-only).

    Order: GROWW_ACCESS_TOKEN env (local dev without AWS; token only, never a
    credential) -> SSM GetParameter(WithDecryption=True) on the parameter named
    by GROWW_SSM_TOKEN_PARAM, via boto3's default credential chain (the
    instance-profile-delivered reader role in prod). Raises on any failure so
    the caller's auth-phase backoff paces the retry. NEVER mints.
    """
    env_token = os.environ.get("GROWW_ACCESS_TOKEN")
    if env_token and env_token.strip():
        return env_token.strip()
    param = os.environ.get("GROWW_SSM_TOKEN_PARAM")
    if not param:
        raise RuntimeError(
            "no token source: set GROWW_SSM_TOKEN_PARAM (prod; SSM parameter "
            "path of the minter-written access token) or GROWW_ACCESS_TOKEN "
            "(local dev)"
        )
    import boto3  # deferred: only the SSM path needs AWS

    region = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION") or "ap-south-1"
    ssm = boto3.client("ssm", region_name=region)
    value = ssm.get_parameter(Name=param, WithDecryption=True)["Parameter"]["Value"]
    token = (value or "").strip()
    if not token or token.lower() in ("placeholder", "changeme", "todo", "unset"):
        raise RuntimeError(
            f"SSM parameter {param} holds no usable token — the "
            "groww-token-minter Lambda has not written today's token yet"
        )
    return token


def _is_auth_error(exc) -> bool:
    """True if `exc` looks auth-class (401/403 or auth/token wording).

    Used to drop the cached token on ANY phase's auth-shaped failure — the
    daily 06:00 IST reset surfaces as a 401 on feed-connect/subscribe, not in
    the [auth] read phase — so the NEXT cycle re-reads the SSM parameter
    instead of retrying forever with the stale token.
    """
    status = getattr(exc, "status_code", None)
    if status is None:
        response = getattr(exc, "response", None)
        status = getattr(response, "status_code", None) if response is not None else None
    if status in (401, 403):
        return True
    name = type(exc).__name__.lower()
    if "auth" in name or "token" in name:
        return True
    text = str(exc).lower()
    return "unauthorized" in text or "authorization violation" in text or "token expired" in text


def _token_stale_alert_due(stale_since, now_secs, already_alerted) -> bool:
    """Pure: is the ONE edge-triggered token-stale alert due?

    True only when auth has been continuously failing for
    TOKEN_STALE_ALERT_SECS and the alert has not fired yet this stale episode.
    """
    if already_alerted or stale_since is None:
        return False
    return (now_secs - stale_since) >= TOKEN_STALE_ALERT_SECS


def _backoff_secs(consecutive_failures: int, rate_limited: bool, retry_after) -> float:
    """Exponential backoff with jitter for the Nth consecutive failure.

    Base 5s (60s when rate-limited), doubling per consecutive failure, capped at
    300s, ±20% jitter. A server-advised retry-after (if larger) takes precedence.

    DURING MARKET HOURS (and NOT rate-limited / no server retry-after), use the
    fast ms ladder (50ms→…→5s cap) instead, so a mid-session drop reconnects in
    milliseconds (operator: "reconnects within seconds and re-subscribes"). A
    rate-limit ALWAYS uses the 60s ladder even in market hours — hammering the
    throttled token endpoint just extends the ban.
    """
    if rate_limited or retry_after is not None:
        base = RATE_LIMIT_BACKOFF_BASE_SECS if rate_limited else RECONNECT_BACKOFF_BASE_SECS
        exp = base * (2 ** max(0, consecutive_failures - 1))
        delay = min(exp, RECONNECT_BACKOFF_CAP_SECS)
        if retry_after is not None:
            delay = max(delay, min(retry_after, RECONNECT_BACKOFF_CAP_SECS))
        jitter = 1.0 + random.uniform(-BACKOFF_JITTER_FRAC, BACKOFF_JITTER_FRAC)
        return delay * jitter
    if _is_within_market_hours_ist(time.time()):
        # Fast ms ladder while the market is open: 50ms, 100ms, … capped at 5s.
        exp = MARKET_OPEN_RECONNECT_BASE_SECS * (2 ** max(0, consecutive_failures - 1))
        delay = min(exp, MARKET_OPEN_RECONNECT_CAP_SECS)
        jitter = 1.0 + random.uniform(-BACKOFF_JITTER_FRAC, BACKOFF_JITTER_FRAC)
        return delay * jitter
    # Off-hours, no rate-limit: the original 5s→300s ladder (don't churn after close).
    base = RECONNECT_BACKOFF_BASE_SECS
    exp = base * (2 ** max(0, consecutive_failures - 1))
    delay = min(exp, RECONNECT_BACKOFF_CAP_SECS)
    jitter = 1.0 + random.uniform(-BACKOFF_JITTER_FRAC, BACKOFF_JITTER_FRAC)
    return delay * jitter


# Belt-and-suspenders structural masks (2026-06-29 security fix). Even if a token
# value is NOT in the `secrets` set (an unanticipated/refreshed shape), these
# scrub anything secret-SHAPED so a NATS auth error / SDK DEBUG line can never
# write the Groww bearer token to stderr → the Rust supervisor → CloudWatch.
#   - JWT: three base64url segments joined by dots, starting `eyJ` (the b64 of
#     `{"`). Groww/Dhan access tokens are JWTs (`eyJ...`).
#   - LONG BEARER: a `Bearer <token>` header carrying a long opaque token.
_JWT_RE = re.compile(r"eyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}")
_BEARER_RE = re.compile(r"(?i)(bearer\s+)[A-Za-z0-9._\-]{16,}")


def _redact(text: str, secrets) -> str:
    """Scrub known secret values out of a string before it is logged.

    A Groww SDK HTTP error's str/repr can embed the request that carried the
    access token, or echo the response body (security-review MEDIUM
    2026-06-19). We DO want the cause (status code, error message, SDK detail)
    for triage, so instead of dropping the whole detail we surface it with every
    known secret value masked. Two layers:
      1. Exact-match every value in `secrets` (the shared access token — since
         2026-06-29 — the live access token, kept current across refreshes).
      2. Structural masks for any JWT-shaped / bearer-ish token even if it is NOT
         in `secrets` (an unanticipated token shape), so the redaction is no
         longer purely per-known-value. Callers also cap the length.
    """
    if not text:
        return text
    for secret in secrets:
        if secret:
            text = text.replace(secret, "***REDACTED***")
    # Structural fallback — mask any JWT / bearer-shaped token we did NOT
    # anticipate by exact value.
    text = _JWT_RE.sub("***REDACTED_JWT***", text)
    text = _BEARER_RE.sub(r"\1***REDACTED***", text)
    return text


def _add_secret(secrets: list, value) -> None:
    """Add `value` to the live redaction set if it is a non-empty new secret.

    `secrets` is a MUTABLE list captured BY REFERENCE by the NATS reason hooks +
    the reject poller, so adding the access token here (the moment it is acquired
    / refreshed) makes those long-lived consumers mask it too — no re-arming
    needed. Short values are skipped to avoid masking incidental substrings.
    """
    if value and len(value) >= 8 and value not in secrets:
        secrets.append(value)


class _RedactingLogFilter(logging.Filter):
    """A logging.Filter that scrubs secrets out of SDK-emitted records.

    The 2026-06-29 diagnostic turns the `growwapi` + `nats` SDK loggers ON at
    DEBUG (stderr). Those records are emitted by THIRD-PARTY code through Python's
    logging module, so they NEVER pass through our per-print-call _redact — an
    auth/connect line the SDK logs at DEBUG (CONNECT frame, connect URL, exception
    repr) could carry the access token and the Rust supervisor
    forwards it verbatim to CloudWatch. This filter intercepts each record BEFORE
    emit, fully materialises its message (msg % args), runs it through _redact
    (exact-value masks for everything in `secrets` PLUS the structural JWT/bearer
    masks), then replaces record.msg with the scrubbed text and clears record.args
    so the handler emits ONLY the redacted form. Holding `secrets` by reference
    means a later access-token addition applies retroactively. Returns True so the
    (now-scrubbed) record is still emitted — diagnostics are preserved, secrets
    are not. Never raises out of filter() (a redaction bug must not drop the line).
    """

    def __init__(self, secrets: list) -> None:
        super().__init__()
        self._secrets = secrets

    def filter(self, record: logging.LogRecord) -> bool:  # noqa: A003 - logging API
        try:
            scrubbed = _redact(record.getMessage(), self._secrets)
            record.msg = scrubbed
            record.args = None
            # A traceback can embed a credential in a frame's locals/repr. Scrub
            # the cached formatted exception AND drop the raw exc_info 3-tuple so
            # a formatter that renders exc_info fresh (when exc_text is not yet
            # cached) cannot re-leak the unredacted traceback (security-review
            # MEDIUM 2026-06-29). We keep a short, secret-free marker so the
            # operator still sees that an exception was attached.
            if record.exc_info is not None or getattr(record, "exc_text", None):
                if getattr(record, "exc_text", None):
                    record.exc_text = _redact(record.exc_text, self._secrets)
                else:
                    record.exc_text = "(exception detail suppressed by redaction)"
                record.exc_info = None
        except Exception:  # noqa: BLE001 - redaction must never drop a log line
            record.msg = "groww sidecar: <log line suppressed; redaction failed>"
            record.args = None
        return True


def _install_sdk_log_redaction(secrets: list) -> None:
    """Scrub SDK-emitted log records before they reach stderr.

    Belt-and-suspenders for the SDK-DEBUG-leak finding (2026-06-29): the
    diagnostic that enables the `growwapi` + `nats` loggers at DEBUG would
    otherwise route SDK-emitted records to stderr WITHOUT redaction. We attach the
    redacting filter at the HANDLER level on the root logger (where basicConfig
    put the stderr StreamHandler) — a handler-level filter runs for EVERY record
    that reaches the handler, INCLUDING records that propagate up from arbitrary
    SDK child loggers (e.g. `growwapi.feed`, `nats.aio.client`), which a
    logger-level filter on the parent would miss. We ALSO add it directly on the
    `growwapi`/`nats` loggers as a second layer (covers a future non-propagating
    handler). Best-effort: any failure is logged (type only) and ignored — and if
    we could not attach a handler filter at all, the SDK loggers are demoted to
    WARNING so their unredacted per-frame DEBUG protocol/auth detail is simply not
    emitted. The reject-reason surfacing (#1254) is OUR print() path, not the SDK
    logger, so it is unaffected either way.
    """
    redactor = _RedactingLogFilter(secrets)
    handler_attached = False
    try:
        root_handlers = list(logging.getLogger().handlers)
        for h in root_handlers:
            h.addFilter(redactor)
            handler_attached = True
    except Exception as exc:  # noqa: BLE001 - never break the feed
        print(
            f"groww sidecar: could not attach root-handler SDK-log redaction "
            f"({type(exc).__name__})",
            file=sys.stderr,
            flush=True,
        )
    # Second layer: directly on the SDK loggers (covers records that do NOT
    # propagate to a root handler).
    for name in ("growwapi", "nats"):
        try:
            logging.getLogger(name).addFilter(redactor)
        except Exception:  # noqa: BLE001 - never break the feed
            pass
    if not handler_attached:
        # FALLBACK: no handler to scrub through → demote the noisy SDK loggers to
        # WARNING so their unredacted DEBUG detail cannot reach stderr at all.
        for name in ("growwapi", "nats"):
            try:
                logging.getLogger(name).setLevel(logging.WARNING)
            except Exception:  # noqa: BLE001
                pass
        print(
            "groww sidecar: SDK-log redaction could not attach a handler filter; "
            "demoted growwapi+nats loggers to WARNING (no DEBUG leak)",
            file=sys.stderr,
            flush=True,
        )
    else:
        print(
            "groww sidecar: SDK-log redaction filter attached "
            "(secrets scrubbed before emit)",
            file=sys.stderr,
            flush=True,
        )


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
    """Current wall-clock time as IST epoch nanoseconds (for the status file ts).

    EPOCH CONTRACT (2026-07-06 fix, cross-language — ratcheted by
    test_sidecar_status_stamp_shares_the_ist_epoch_convention in
    crates/app/src/groww_bridge.rs): this MUST use the same
    `replace(tzinfo=timezone.utc)` trick as ms_to_ist_nanos so the stamp is
    "IST epoch" = UTC epoch + 19,800s — the SAME convention the Rust bridge's
    receipt clock (`receipt_ist_nanos`) uses. The previous
    `datetime.now(tz=IST).timestamp()` returned the PLAIN UTC epoch
    (`.timestamp()` on an aware datetime ignores the zone), so the bridge's
    freshness gate (`groww_status_is_live`, 120s window) read every genuinely
    fresh status record as ~19,800s old and classified it STALE forever —
    killing the connect/subscribe evidence path and false-firing the
    groww-ws-inactive alarm every morning.
    """
    dt_ist = datetime.now(tz=IST)
    return int(dt_ist.replace(tzinfo=timezone.utc).timestamp() * 1_000_000_000)


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
        # Change-dedup skips + per-reason drop breakdown (2026-07-03). The Rust
        # status reader ignores unknown fields (serde default semantics), so
        # these are additive-safe.
        "deduped": int(DEDUPED_TOTAL),
        "dropped_reasons": dict(DROP_REASONS),
        # Per-callback capture counters (2026-07-03; additive-safe — the Rust
        # status reader ignores unknown fields).
        "hook_captured": int(CAPTURE_EMITTED_TOTAL),
        "hook_dropped_full": int(_HOOK_DROPPED_FULL[0]),
        "reconcile_emitted": int(RECONCILE_EMITTED_TOTAL),
        "ts_ist_nanos": now_ist_nanos(),
    }
    try:
        directory = os.path.dirname(STATUS_PATH) or "."
        os.makedirs(directory, exist_ok=True)
        tmp = f"{STATUS_PATH}.{os.getpid()}.tmp"
        # _STATUS_LOCK: the writer thread AND the walker thread both call
        # note_emit → write_status; the pid-based temp name is shared within
        # the process, so the temp+rename must be serialized.
        with _STATUS_LOCK:
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


def dedup_should_emit(cache: dict, key: tuple, ts_millis: int, price: float) -> bool:
    """PURE change-dedup decision (2026-07-03): emit only when (ts_millis, price)
    differs from the last-emitted pair for `key`; update the cache on emit.

    The key MUST include the instrument identity incl. a kind discriminant; the
    VALUE pair must include ts_millis so a genuine new print with an identical
    price but an advancing timestamp is NEVER swallowed (adversarial guard).
    Same-ts price corrections (two prints in one millisecond slot) also emit
    because the price differs. Only an EXACT (ts, price) re-dump of the
    already-captured snapshot entry is skipped. O(1) dict lookup; the cache is
    bounded by the subscribed universe. Unit-tested in test_dedup.py and under
    `--selftest` (_selftest_dedup)."""
    pair = (ts_millis, price)
    if cache.get(key) == pair:
        return False
    cache[key] = pair
    return True


def note_dedup() -> None:
    """Count one decoded-but-UNCHANGED snapshot entry skipped by change-dedup.

    Not a drop (nothing was lost — the identical (ts, price) was already
    captured + fsynced) and not an emit (nothing new flowed). Carried in the
    status file as `deduped` so the flood magnitude stays observable."""
    global DEDUPED_TOTAL
    DEDUPED_TOTAL += 1


def ts_watermark_advanced(prev_max_ms: int, ts_millis) -> bool:
    """PURE liveness decision (2026-07-03 feed-death fix): does `ts_millis`
    STRICTLY advance the max exchange timestamp seen? Non-numeric / missing
    timestamps never advance. Mirrors the Rust bridge's `liveness_ts_advanced`
    so both stall layers key on the same signal: fresh-timestamped delivery,
    not decode volume. Unit-tested in test_dedup.py."""
    try:
        return int(ts_millis) > prev_max_ms
    except (ValueError, TypeError):
        return False


def _note_ts_advance(tick) -> None:
    """Advance the module ts watermark from a decoded record's tsInMillis.

    Called for EVERY decoded record dict in both emit paths — before the
    dedup/drop decisions — so the stall detector sees the truth: a frozen
    snapshot re-dump (ts never advances) reads as STALLED even while duplicate
    decodes flood in, and a fresh-but-dropped record still reads as alive
    (the socket delivers; a mapping bug must not trigger a reconnect storm)."""
    global MAX_TS_MILLIS_SEEN, _LAST_TS_ADVANCE_MONOTONIC
    if not isinstance(tick, dict):
        return
    ts = tick.get("tsInMillis", 0)
    if ts_watermark_advanced(MAX_TS_MILLIS_SEEN, ts):
        MAX_TS_MILLIS_SEEN = int(ts)
        _LAST_TS_ADVANCE_MONOTONIC = time.monotonic()


def note_drop(
    reason: str = "unspecified",
    exchange: str = "",
    segment: str = "",
    token: str = "",
    detail: str = "",
) -> None:
    """Count one DECODED-but-DROPPED record (sid_map miss / missing field).

    Previously these were a SILENT `continue`. Counting them lets the Rust bridge
    surface "streaming but 0 ticks" with a visible cause (operator 2026-06-29).
    Does NOT re-write the status file (drops without emits do not flip connected;
    the count is carried on the next periodic emit re-write).

    2026-07-03: per-reason breakdown + capped sample lines. The live incident's
    dropped=276,077 (starting exactly at the first get_ltp() callback, ~320/sec
    ≈ the 743-stock rate) could not be attributed from logs because the counter
    had no breakdown. Reasons: `no_price_field` (leaf missing ltp/value),
    `sid_map_miss` (index token unresolvable — stocks fall back to the numeric
    token), `coerce_error` (ts/price coercion raised), `unmapped_segment`
    (a whole (exchange, segment) subtree with no SEGMENT_MAP entry — previously
    a fully SILENT skip). Sample lines carry market-data identifiers only —
    never a credential.
    """
    global DROPPED_TOTAL
    DROPPED_TOTAL += 1
    DROP_REASONS[reason] = DROP_REASONS.get(reason, 0) + 1
    sampled = _DROP_SAMPLES_LOGGED.get(reason, 0)
    if sampled < DROP_SAMPLE_LIMIT_PER_REASON:
        _DROP_SAMPLES_LOGGED[reason] = sampled + 1
        print(
            f"groww sidecar: DROP[{reason}] sample "
            f"{sampled + 1}/{DROP_SAMPLE_LIMIT_PER_REASON}: "
            f"exchange={exchange} segment={segment} token={token} {detail}",
            file=sys.stderr,
            flush=True,
        )


def note_drop_bulk(reason: str, count: int) -> None:
    """Count `count` drops of one reason in a single call (writer thread).

    Used to fold the hook's queue-full counter (mutated only on the SDK's NATS
    consumer thread — the hook must stay enqueue-only, no accounting there)
    into the shared DROP_REASONS breakdown, with ONE bounded sample line per
    reason (reuses the note_drop sample cap)."""
    global DROPPED_TOTAL
    if count <= 0:
        return
    DROPPED_TOTAL += count
    DROP_REASONS[reason] = DROP_REASONS.get(reason, 0) + count
    sampled = _DROP_SAMPLES_LOGGED.get(reason, 0)
    if sampled < DROP_SAMPLE_LIMIT_PER_REASON:
        _DROP_SAMPLES_LOGGED[reason] = sampled + 1
        print(
            f"groww sidecar: DROP[{reason}] sample "
            f"{sampled + 1}/{DROP_SAMPLE_LIMIT_PER_REASON}: bulk count={count}",
            file=sys.stderr,
            flush=True,
        )


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


# ---------------------------------------------------------------------------
# RAW-TICK-PROBE (2026-07-03, LOG-ONLY) — settle whether Groww's live feed
# POPULATES the SDK-exposed `volume` field. The growwapi==1.5.0 SDK decodes the
# full 16-field StocksLivePriceProto per instrument (volume, openInterest,
# OHLC, avgPrice, …) even though Groww's docs list only tsInMillis+ltp — but
# proto3 doubles have NO presence, so `volume: 0.0` appears whether the server
# omits it or sends zero. Only inspecting REAL live tick dicts answers the
# question. This probe:
#   1. Prints the FIRST RAW_PROBE_SAMPLE_COUNT stock/FNO tick dicts (and the
#      first RAW_PROBE_INDEX_SAMPLE_COUNT index dicts) verbatim as ONE
#      `RAW-TICK-PROBE kind=… token=… fields=<compact json>` stderr line each,
#      then ONE `RAW-TICK-PROBE-SUMMARY …` line scanning those samples.
#   2. Keeps CHEAP process-lifetime running counters (two float compares + int
#      increments per drained tick — O(1)) of how many drained stock/FNO ticks
#      had volume != 0 / openInterest != 0, surfaced via a bounded
#      `RAW-TICK-PROBE-HEARTBEAT` stderr line at most once per
#      RAW_PROBE_HEARTBEAT_SECS — so even if the first 8 ticks are all zero we
#      learn whether ANY tick all day carries volume.
# It observes DRAINED snapshot entries inside the walker thread's emit paths —
# NEVER the O(1) NATS callbacks (the 2026-07-03 lag-fix contract is untouched).
# Emission/persistence are UNCHANGED: cum_volume stays 0 pending the verdict.
# Market-data identifiers/values only — never a credential.
# Default: sample once per process. GROWW_RAW_PROBE=always re-arms the sampler
# on every reconnect cycle (running counters are NEVER reset).
# CloudWatch: log group /tickvault/prod/app, filter "RAW-TICK-PROBE".
# ---------------------------------------------------------------------------

RAW_PROBE_SAMPLE_COUNT = 8
RAW_PROBE_INDEX_SAMPLE_COUNT = 2
RAW_PROBE_HEARTBEAT_SECS = 300.0
# The OHLC keys scanned for the summary's nonzero_ohlc count.
_RAW_PROBE_OHLC_KEYS = ("open", "high", "low", "close")


def probe_field_nonzero(value) -> bool:
    """Pure: True iff `value` coerces to a NON-ZERO float. Non-numeric /
    missing values are honestly False (never raise) — proto3's no-presence
    `0.0` and an absent key both read as \"not populated\". O(1)."""
    try:
        return float(value) != 0.0
    except (TypeError, ValueError):
        return False


class RawTickProbe:
    """Bounded raw-tick sampler + cheap running field-population counters.

    Injectable `emit` (defaults to a flushed stderr print) so unit tests can
    capture lines without patching stdio (test_dedup.py::RawTickProbeTests).
    All state is plain ints/lists mutated from the single walker thread — no
    locks needed (same threading model as the module counters above)."""

    def __init__(
        self,
        mode: str = "",
        ltp_limit: int = RAW_PROBE_SAMPLE_COUNT,
        index_limit: int = RAW_PROBE_INDEX_SAMPLE_COUNT,
        emit=None,
    ) -> None:
        self.always = (mode or "").strip().lower() == "always"
        self._ltp_limit = ltp_limit
        self._index_limit = index_limit
        self._emit = emit or (
            lambda line: print(line, file=sys.stderr, flush=True)
        )
        # Per-arming sampling state (reset by rearm_for_reconnect in `always`).
        self._ltp_sampled = 0
        self._index_sampled = 0
        self._samples = []  # the sampled stock/FNO dicts, for the summary scan
        self._summary_printed = False
        # Process-lifetime running counters — NEVER reset, even on re-arm.
        self.total_ltp_ticks = 0
        self.nonzero_volume_ticks = 0
        self.nonzero_oi_ticks = 0
        self._last_heartbeat_monotonic = None

    def observe(self, kind: str, token, tick) -> None:
        """Observe ONE drained snapshot entry. O(1) fast path: once sampling
        is exhausted this is an int compare + (for kind=\"ltp\") two float
        compares + int increments. Called from the walker thread ONLY."""
        if not isinstance(tick, dict):
            return
        if kind == "ltp":
            self.total_ltp_ticks += 1
            if probe_field_nonzero(tick.get("volume")):
                self.nonzero_volume_ticks += 1
            if probe_field_nonzero(tick.get("openInterest")):
                self.nonzero_oi_ticks += 1
            if self._ltp_sampled < self._ltp_limit:
                self._ltp_sampled += 1
                self._samples.append(dict(tick))
                self._emit_sample("ltp", token, tick)
                if self._ltp_sampled == self._ltp_limit and not self._summary_printed:
                    self._print_summary()
        elif kind == "index":
            if self._index_sampled < self._index_limit:
                self._index_sampled += 1
                self._emit_sample("index", token, tick)

    def _emit_sample(self, kind: str, token, tick: dict) -> None:
        try:
            fields = json.dumps(tick, sort_keys=True, separators=(",", ":"), default=str)
        except (TypeError, ValueError):
            fields = repr(tick)
        self._emit(f"RAW-TICK-PROBE kind={kind} token={token} fields={fields}")

    def _print_summary(self) -> None:
        """ONE summary line scanning the sampled stock/FNO dicts."""
        self._summary_printed = True
        total = len(self._samples)
        nz_vol = sum(1 for t in self._samples if probe_field_nonzero(t.get("volume")))
        nz_oi = sum(
            1 for t in self._samples if probe_field_nonzero(t.get("openInterest"))
        )
        nz_ohlc = sum(
            1
            for t in self._samples
            if any(probe_field_nonzero(t.get(k)) for k in _RAW_PROBE_OHLC_KEYS)
        )
        self._emit(
            f"RAW-TICK-PROBE-SUMMARY nonzero_volume_ticks={nz_vol}/{total} "
            f"nonzero_oi={nz_oi}/{total} nonzero_ohlc={nz_ohlc}/{total}"
        )

    def rearm_for_reconnect(self) -> None:
        """Re-arm the bounded sampler for a new reconnect cycle — ONLY under
        GROWW_RAW_PROBE=always (default: once per process). The running
        counters are process-lifetime truth and are NEVER reset."""
        if not self.always:
            return
        self._ltp_sampled = 0
        self._index_sampled = 0
        self._samples = []
        self._summary_printed = False

    def maybe_heartbeat(self, now_monotonic: float) -> bool:
        """Bounded periodic running-counter line (≤ 1 per
        RAW_PROBE_HEARTBEAT_SECS; nothing before the first drained stock/FNO
        tick). One float compare per call — safe to invoke every walker
        iteration. Returns True iff a line was emitted (unit-testable)."""
        if self.total_ltp_ticks == 0:
            return False
        if self._last_heartbeat_monotonic is None:
            # Arm the cadence at the first observed tick; first line fires one
            # full interval later (the samples already covered t=0).
            self._last_heartbeat_monotonic = now_monotonic
            return False
        if now_monotonic - self._last_heartbeat_monotonic < RAW_PROBE_HEARTBEAT_SECS:
            return False
        self._last_heartbeat_monotonic = now_monotonic
        self._emit(
            "RAW-TICK-PROBE-HEARTBEAT "
            f"nonzero_volume_ticks={self.nonzero_volume_ticks}/{self.total_ltp_ticks} "
            f"nonzero_oi={self.nonzero_oi_ticks}/{self.total_ltp_ticks}"
        )
        return True


# Module singleton wired into the walker-thread emit paths. Env read once at
# import (the sidecar process is restarted by the Rust supervisor anyway).
RAW_PROBE = RawTickProbe(mode=os.environ.get("GROWW_RAW_PROBE", ""))


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


def _write_record(
    out,
    security_id: int,
    segment: str,
    ts_millis: int,
    price,
    capture_ns=None,
    sync: bool = True,
) -> None:
    """Append one NDJSON tick (Rust bridge schema) + capture-at-receipt fsync.

    2026-07-03 per-callback capture additions (both backward-compatible —
    the Rust bridge treats an absent `capture_ns` as the old format):
    - `capture_ns` (optional int, UTC epoch nanos from time.time_ns() stamped
      INSIDE the NATS-callback hook) — the true capture-at-receipt instant,
      making our-side latency (received_at − capture) and the external Groww
      floor (capture − tsInMillis) SEPARATELY measurable columns.
    - `sync=False` lets the capture writer thread group-commit: it writes N
      records then does ONE flush+fsync per batch (the out handle is
      line-buffered, so the bridge's notify watcher sees each row at write
      time; the fsync is the durability floor per batch).
    Every record write holds _OUT_LOCK — the capture writer thread and the
    walker (reconcile) thread share the same rotating out handle.
    """
    rec = {
        "security_id": int(security_id),
        "segment": segment,
        "ts_ist_nanos": ms_to_ist_nanos(ts_millis) if ts_millis else 0,
        "exchange_ts_millis": ts_millis,
        "ltp": float(price),
        # Option A: Groww docs list no volume; the SDK exposes a `volume`
        # field — population under live probe (RAW-TICK-PROBE, 2026-07-03);
        # emission unchanged pending verdict -> always 0.
        "cum_volume": 0,
    }
    if capture_ns is not None:
        rec["capture_ns"] = int(capture_ns)
    line = json.dumps(rec) + "\n"
    with _OUT_LOCK:
        out.write(line)
        if sync:
            out.flush()
            os.fsync(out.fileno())  # capture-at-receipt durability


def emit_ltp_records(out, ltp_tree: dict, sid_map: dict, reconcile: bool = False) -> None:
    """Flatten get_ltp() `{exchange:{segment:{token:{ltp,tsInMillis}}}}` -> NDJSON.

    Stock identity comes from the tree path; security_id from sid_map (falls back
    to the numeric token, which IS the stock security_id).

    `reconcile=True` (walker thread while per-callback capture is active):
    every record that still emits from this SNAPSHOT walk was MISSED by the
    hook path (the shared change-dedup cache already swallowed everything the
    hook captured) — counted in RECONCILE_EMITTED_TOTAL as the hook-health
    signal. Snapshot rows carry no per-message capture stamp, so no
    capture_ns (the Rust bridge falls back to its per-wake receipt stamp).
    """
    if not isinstance(ltp_tree, dict):
        return
    # One-shot shape proof: reveal the REAL top-level keys ONCE so the next run
    # confirms which doc (07 WRAPPED vs 10 BARE) is correct — zero ambiguity.
    global _ltp_shape_logged
    if not _ltp_shape_logged:
        print(
            f"groww get_ltp() top-level keys: {sorted(map(str, ltp_tree.keys()))}",
            file=sys.stderr,
            flush=True,
        )
        _ltp_shape_logged = True
    # Defensively UNWRAP a possible top-level "ltp"/"stockLivePrice" wrapper so
    # BOTH the WRAPPED (doc 07) and BARE (doc 10) shapes work (2026-06-29).
    ltp_tree = _unwrap_feed_tree(ltp_tree, ("ltp", "stockLivePrice"))
    for exchange, segs in ltp_tree.items():
        if not isinstance(segs, dict):
            continue
        for segment, tokens in segs.items():
            canonical = SEGMENT_MAP.get((str(exchange), str(segment)))
            if canonical is None or not isinstance(tokens, dict):
                # 2026-07-03: previously a FULLY SILENT subtree skip — a
                # Groww-side segment-string change would vanish the entire
                # stock feed with zero signal. Count + sample it.
                if canonical is None:
                    note_drop(
                        "unmapped_segment",
                        str(exchange),
                        str(segment),
                        detail=f"subtree_entries={len(tokens) if isinstance(tokens, dict) else 0}",
                    )
                continue
            for token, tick in tokens.items():
                # Advance the stall detector's ts watermark for EVERY decoded
                # record — before dedup/drop — so liveness = fresh timestamps.
                _note_ts_advance(tick)
                # RAW-TICK-PROBE (log-only): observe the raw decoded dict —
                # before drop/dedup — from the walker thread (never the O(1)
                # NATS callbacks). Emission below is unchanged.
                RAW_PROBE.observe("ltp", token, tick)
                # A decoded record with no `ltp` field — DROP (honest-feed count).
                if not isinstance(tick, dict) or "ltp" not in tick:
                    note_drop(
                        "no_price_field",
                        str(exchange),
                        str(segment),
                        str(token),
                        detail=f"leaf_keys={sorted(map(str, tick.keys())) if isinstance(tick, dict) else type(tick).__name__}",
                    )
                    continue
                token = str(token)
                security_id = sid_map.get((str(exchange), str(segment), token))
                if security_id is None:
                    security_id = int(token) if token.isdigit() else 0
                # sid_map miss + non-numeric token → no resolvable id — DROP.
                if security_id <= 0:
                    note_drop("sid_map_miss", str(exchange), str(segment), token)
                    continue
                try:
                    ts_millis = int(tick.get("tsInMillis", 0))
                    price = float(tick["ltp"])
                    # Change-dedup (2026-07-03): skip an EXACT (ts, price)
                    # re-dump of this stock's already-emitted snapshot entry.
                    if not dedup_should_emit(
                        _LAST_EMITTED,
                        ("ltp", str(exchange), str(segment), token),
                        ts_millis,
                        price,
                    ):
                        note_dedup()
                        continue
                    _write_record(out, security_id, canonical, ts_millis, price)
                    note_emit()
                    if reconcile:
                        _note_reconcile_emit()
                except (KeyError, ValueError, TypeError) as exc:
                    note_drop(
                        "coerce_error",
                        str(exchange),
                        str(segment),
                        token,
                        detail=f"exc={type(exc).__name__}",
                    )
                    continue


def emit_index_records(out, index_tree: dict, sid_map: dict, reconcile: bool = False) -> None:
    """Flatten get_index_value() `{exchange:{segment:{token:{value,tsInMillis}}}}`.

    Index value field is `value` (not `ltp`); stored as ltp with segment IDX_I.
    security_id MUST come from sid_map (the token may be a NAME) — no fallback.
    `reconcile` semantics: see emit_ltp_records.
    """
    if not isinstance(index_tree, dict):
        return
    # One-shot shape proof: reveal the REAL top-level keys ONCE. Doc 07 shows the
    # index tree BARE (no wrapper), but harden it the same way in case the live
    # SDK wraps it under "value"/"stocksLiveIndices"/"indexValue".
    global _index_shape_logged
    if not _index_shape_logged:
        print(
            f"groww get_index_value() top-level keys: "
            f"{sorted(map(str, index_tree.keys()))}",
            file=sys.stderr,
            flush=True,
        )
        _index_shape_logged = True
    # Defensively UNWRAP a possible top-level index wrapper so a BARE (doc 07)
    # or a WRAPPED tree both work; conservative — never mangles a BARE tree.
    index_tree = _unwrap_feed_tree(
        index_tree, ("value", "stocksLiveIndices", "indexValue")
    )
    for exchange, segs in index_tree.items():
        if not isinstance(segs, dict):
            continue
        for segment, tokens in segs.items():
            if not isinstance(tokens, dict):
                continue
            for token, tick in tokens.items():
                # Advance the stall detector's ts watermark for EVERY decoded
                # record — before dedup/drop — so liveness = fresh timestamps.
                _note_ts_advance(tick)
                # RAW-TICK-PROBE (log-only): first 2 raw index dicts, verbatim.
                RAW_PROBE.observe("index", token, tick)
                # A decoded index record with no `value` field — DROP (count it).
                if not isinstance(tick, dict) or "value" not in tick:
                    note_drop(
                        "no_price_field",
                        str(exchange),
                        str(segment),
                        str(token),
                        detail=f"leaf_keys={sorted(map(str, tick.keys())) if isinstance(tick, dict) else type(tick).__name__}",
                    )
                    continue
                security_id = sid_map.get((str(exchange), str(segment), str(token)))
                # sid_map miss (index token may be a NAME — no fallback) — DROP.
                if security_id is None or security_id <= 0:
                    note_drop("sid_map_miss", str(exchange), str(segment), str(token))
                    continue
                try:
                    ts_millis = int(tick.get("tsInMillis", 0))
                    price = float(tick["value"])
                    # Change-dedup (2026-07-03 live incident): the index tree is
                    # re-dumped WHOLE on every NATS callback — ~25 indices ×
                    # ~21 msgs/sec of frozen (ts, value) pairs = 530K junk rows
                    # in 17 min. Skip the unchanged re-dumps.
                    if not dedup_should_emit(
                        _LAST_EMITTED,
                        ("idx", str(exchange), str(segment), str(token)),
                        ts_millis,
                        price,
                    ):
                        note_dedup()
                        continue
                    _write_record(
                        out, security_id, CANONICAL_INDEX_SEGMENT, ts_millis, price
                    )
                    note_emit()
                    if reconcile:
                        _note_reconcile_emit()
                except (KeyError, ValueError, TypeError) as exc:
                    note_drop(
                        "coerce_error",
                        str(exchange),
                        str(segment),
                        str(token),
                        detail=f"exc={type(exc).__name__}",
                    )
                    continue


# ---------------------------------------------------------------------------
# PER-CALLBACK INSTANT CAPTURE (2026-07-03 ≤1ms latency study, variant c2 with
# walker fallback). The SDK's NATS consumer invokes `self.callback(subject,
# data)` PER MESSAGE (nats_client.py:197, growwapi==1.5.0) where `callback` is
# a plain instance attribute read at call time — so wrapping it captures the
# EXACT raw protobuf payload + subject per message (zero intra-window loss,
# unlike the latest-only snapshot the walker drains). The wrapper is
# ENQUEUE-ONLY (~µs): stamp time.time_ns(), put_nowait on the bounded queue,
# delegate to the original callback. ALL decode/dedup/write happens on the
# dedicated writer thread — the SDK's consumer thread is never blocked (the
# #1344 starvation contract). Precedent for SDK attribute hooking:
# install_nats_reason_hooks above. Guarded: any attach failure logs one line
# and the sidecar falls back to the CURRENT walker path unchanged.
# ---------------------------------------------------------------------------

# The bounded hand-off queue (process-lifetime; survives reconnect cycles —
# each cycle's fresh feed gets a fresh hook that feeds the same queue).
_CAPTURE_QUEUE = queue.Queue(maxsize=CAPTURE_QUEUE_MAX)
# subject -> (kind, exchange, segment, token, security_id, canonical_segment).
# Rebuilt per reconnect cycle (same contents — same watch lists); a 1-element
# cell so the long-lived writer thread always reads the CURRENT map.
_CAPTURE_TOPIC_MAP = [None]
# True while the hook is attached to the CURRENT feed — the walker reads this
# per iteration to pick its cadence (reconcile sweep vs full-speed walk).
_CAPTURE_MODE = [False]
# Kill-switch (GROWW_CALLBACK_CAPTURE=off → walker-only, exactly the pre-PR
# behaviour). Resolved once at import; pure resolver unit-tested.
CAPTURE_ENABLED = resolve_callback_capture_enabled(
    os.environ.get("GROWW_CALLBACK_CAPTURE")
)


def _note_reconcile_emit() -> None:
    """Count one walker-thread reconcile-sweep emission (hook-missed record)."""
    global RECONCILE_EMITTED_TOTAL
    RECONCILE_EMITTED_TOTAL += 1


def make_capture_hook(orig_callback, cap_queue, enqueued_cell, dropped_cell):
    """Build the enqueue-only NATS-callback wrapper (PURE factory, unit-tested).

    The wrapper runs on the SDK's NATS consumer thread and MUST stay ~µs:
    one clock read + one bounded put_nowait + two int bumps, then delegate to
    the ORIGINAL callback so the SDK snapshot/dirty-flag path is untouched.
    On a full queue it DROPS to the counter — it NEVER blocks, NEVER raises
    (queue.Full is the only exception put_nowait raises; the counters are
    plain list-cell int bumps that cannot raise)."""

    def hooked(subject, data):
        capture_ns = time.time_ns()
        try:
            cap_queue.put_nowait((subject, data, capture_ns))
            enqueued_cell[0] += 1
        except queue.Full:
            dropped_cell[0] += 1
        return orig_callback(subject, data)

    return hooked


def install_callback_capture(feed, cap_queue) -> bool:
    """Attach the capture hook to THIS feed's NATS client. Returns True on
    success. Best-effort: a future-wheel attribute rename (no `_nats_client`,
    no callable `callback`) or ANY exception returns False — the caller falls
    back to the walker path unchanged (feature degrades, never breaks)."""
    try:
        nats_client = getattr(feed, "_nats_client", None)
        orig = getattr(nats_client, "callback", None) if nats_client is not None else None
        if nats_client is None or not callable(orig):
            return False
        nats_client.callback = make_capture_hook(
            orig, cap_queue, _HOOK_ENQUEUED, _HOOK_DROPPED_FULL
        )
        return True
    except Exception as exc:  # noqa: BLE001 - capture must never break the feed
        print(
            f"groww sidecar: callback-capture attach failed ({type(exc).__name__})",
            file=sys.stderr,
            flush=True,
        )
        return False


def build_capture_topic_map(stock_list, index_list, sid_map):
    """subject → (kind, exchange, segment, token, security_id, canonical_segment).

    Built with the SDK's OWN topic builders (FeedConstants.get_live_price_topic
    / get_live_index_topic — the exact functions subscribe_ltp /
    subscribe_index_value use via _get_topics), so the subject strings and the
    topic META (exchange/segment/feed_key) are IDENTICAL to what the SDK keys
    its snapshot trees on. The dedup keys derived from this map therefore
    match the walker's tree-path keys exactly — the reconcile sweep can never
    double-emit a hook-captured record. security_id resolution mirrors the
    walker: sid_map lookup by the META triple, entry-key fallback, then the
    numeric-token fallback for stocks (never for indices).

    Returns None on ANY failure (e.g. a future wheel moved FeedConstants) —
    the caller falls back to walker-only capture.
    """
    try:
        from growwapi.groww.constants import FeedConstants

        def resolve_sid(meta_key, entry_key, token, is_stock):
            sid = sid_map.get(meta_key)
            if sid is None:
                sid = sid_map.get(entry_key)
            if (sid is None or sid <= 0) and is_stock and token.isdigit():
                sid = int(token)
            return sid if isinstance(sid, int) else 0

        topic_map = {}
        for kind, subs, builder in (
            ("ltp", stock_list, FeedConstants.get_live_price_topic),
            ("idx", index_list, FeedConstants.get_live_index_topic),
        ):
            for sub in subs:
                topic = builder(
                    sub["segment"], sub["exchange"], sub["exchange_token"]
                )
                meta = topic.get_meta()
                exchange = str(meta[FeedConstants.EXCHANGE])
                segment = str(meta[FeedConstants.SEGMENT])
                token = str(meta[FeedConstants.FEED_KEY])
                entry_key = (
                    str(sub["exchange"]),
                    str(sub["segment"]),
                    str(sub["exchange_token"]),
                )
                sid = resolve_sid(
                    (exchange, segment, token), entry_key, token, kind == "ltp"
                )
                canonical = (
                    SEGMENT_MAP.get((exchange, segment))
                    if kind == "ltp"
                    else CANONICAL_INDEX_SEGMENT
                )
                topic_map[topic.get_topic()] = (
                    kind,
                    exchange,
                    segment,
                    token,
                    sid,
                    canonical,
                )
        return topic_map
    except Exception as exc:  # noqa: BLE001 - degrade to walker-only, never break
        print(
            f"groww sidecar: capture topic-map build failed ({type(exc).__name__}); "
            "falling back to walker-only capture",
            file=sys.stderr,
            flush=True,
        )
        return None


def _decode_captured_payload(kind: str, payload):
    """Decode ONE raw NATS protobuf payload via the SDK's OWN parser —
    get_data_dict(data, feed_type) — producing the EXACT leaf-dict shape the
    walker's snapshot trees carry ({ltp, tsInMillis, volume, …} for stocks;
    {value, tsInMillis} for indices). One ~20-50µs decode per message (vs the
    walker's 768-decode full-tree walk). Imported lazily so unit tests run
    without the wheel (they monkeypatch this function)."""
    from growwapi.groww.constants import FeedConstants
    from growwapi.groww.proto.proto_parser import get_data_dict

    feed_type = (
        FeedConstants.LIVE_DATA if kind == "ltp" else FeedConstants.LIVE_INDEX
    )
    return get_data_dict(payload, feed_type)


def _capture_emit_one(out, subject, payload, capture_ns) -> int:
    """Decode + dedup + write ONE captured message (writer thread). Returns the
    number of records written (0 or 1) so the batch loop knows whether a
    group-commit fsync is due. Reuses the walker's exact accounting: watermark
    advance + RAW-TICK-PROBE observation before drop/dedup decisions; the SAME
    dedup-cache keys as the snapshot walk (reconcile parity)."""
    global CAPTURE_EMITTED_TOTAL
    topic_map = _CAPTURE_TOPIC_MAP[0]
    entry = topic_map.get(subject) if topic_map else None
    if entry is None:
        note_drop("capture_subject_miss", detail=f"subject={subject}")
        return 0
    kind, exchange, segment, token, security_id, canonical = entry
    try:
        tick = _decode_captured_payload(kind, payload)
    except Exception as exc:  # noqa: BLE001 - one bad payload must never kill the writer
        note_drop(
            "capture_decode_error",
            exchange,
            segment,
            token,
            detail=f"exc={type(exc).__name__}",
        )
        return 0
    if not isinstance(tick, dict):
        note_drop("capture_decode_error", exchange, segment, token, detail="non_dict")
        return 0
    # Same liveness + probe accounting as the walker paths (before drop/dedup).
    _note_ts_advance(tick)
    RAW_PROBE.observe("ltp" if kind == "ltp" else "index", token, tick)
    price_field = "ltp" if kind == "ltp" else "value"
    if price_field not in tick:
        note_drop(
            "no_price_field",
            exchange,
            segment,
            token,
            detail=f"leaf_keys={sorted(map(str, tick.keys()))}",
        )
        return 0
    if canonical is None:
        note_drop("unmapped_segment", exchange, segment, token)
        return 0
    if not isinstance(security_id, int) or security_id <= 0:
        note_drop("sid_map_miss", exchange, segment, token)
        return 0
    try:
        ts_millis = int(tick.get("tsInMillis", 0))
        price = float(tick[price_field])
    except (ValueError, TypeError) as exc:
        note_drop(
            "coerce_error", exchange, segment, token, detail=f"exc={type(exc).__name__}"
        )
        return 0
    # SAME dedup keys as emit_ltp_records / emit_index_records — the reconcile
    # sweep and the hook path share one cache, so neither double-emits.
    if not dedup_should_emit(_LAST_EMITTED, (kind, exchange, segment, token), ts_millis, price):
        note_dedup()
        return 0
    # sync=False: the batch loop group-commits ONE flush+fsync per drain.
    _write_record(
        out, security_id, canonical, ts_millis, price, capture_ns=capture_ns, sync=False
    )
    note_emit()
    CAPTURE_EMITTED_TOTAL += 1
    return 1


def _capture_writer_loop(out, cap_queue) -> None:
    """Daemon: drain the capture queue in group-commit batches (writer thread).

    Blocking get() for the first item (zero idle CPU), then get_nowait() up to
    CAPTURE_BATCH_MAX — decode/dedup/write each, then ONE flush+fsync for the
    whole batch (the fsync ~0.5-2ms cost is amortized under burst; the
    line-buffered handle makes each row visible to the bridge's notify watcher
    at write time, pre-fsync). Folds the hook's queue-full counter into the
    drop breakdown and prints a bounded CAPTURE-STATS stderr line. The
    per-item broad except in _capture_emit_one makes death practically
    unreachable; if this thread ever died anyway, the walker's reconcile sweep
    still emits everything (at its slow cadence) and the frozen-watermark /
    Rust FEED-STALL-01 backstops remain — never a silent-forever failure."""
    print(
        "groww sidecar: per-callback capture writer armed "
        f"(queue={CAPTURE_QUEUE_MAX}, batch≤{CAPTURE_BATCH_MAX}, group-commit fsync)",
        flush=True,
    )
    reported_dropped_full = 0
    last_stats_monotonic = time.monotonic()
    while True:
        batch = [cap_queue.get()]  # blocking — wakes per message, sub-ms
        try:
            while len(batch) < CAPTURE_BATCH_MAX:
                batch.append(cap_queue.get_nowait())
        except queue.Empty:
            pass
        wrote = 0
        for subject, payload, capture_ns in batch:
            try:
                wrote += _capture_emit_one(out, subject, payload, capture_ns)
            except Exception as exc:  # noqa: BLE001 - the writer must never die
                print(
                    f"groww sidecar: capture emit failed ({type(exc).__name__}); "
                    "record dropped to counter, writer continues",
                    file=sys.stderr,
                    flush=True,
                )
                note_drop("capture_emit_error", detail=f"exc={type(exc).__name__}")
        if wrote:
            with _OUT_LOCK:
                out.flush()
                os.fsync(out.fileno())  # ONE durability fsync per batch
        # Fold the hook thread's queue-full drops into the shared breakdown.
        dropped_full_now = _HOOK_DROPPED_FULL[0]
        if dropped_full_now > reported_dropped_full:
            note_drop_bulk("capture_queue_full", dropped_full_now - reported_dropped_full)
            reported_dropped_full = dropped_full_now
        now_monotonic = time.monotonic()
        if now_monotonic - last_stats_monotonic >= CAPTURE_STATS_INTERVAL_SECS:
            last_stats_monotonic = now_monotonic
            print(
                "groww sidecar: CAPTURE-STATS "
                f"hook_captured={CAPTURE_EMITTED_TOTAL} "
                f"hook_enqueued={_HOOK_ENQUEUED[0]} "
                f"hook_dropped_full={_HOOK_DROPPED_FULL[0]} "
                f"reconcile_emitted={RECONCILE_EMITTED_TOTAL} "
                f"qsize={cap_queue.qsize()}",
                file=sys.stderr,
                flush=True,
            )


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


# The CURRENT GrowwFeed handle, updated by the reconnect loop each cycle so the
# long-lived stall-recovery watchdog (armed once per process) always force-closes
# the LIVE socket, not the stale first-cycle one. A list (1 element) is the
# simplest thread-safe shared cell — reads/writes of a single reference are atomic
# under the GIL.
_CURRENT_FEED = [None]

# Per-kind dirty flags for the coalesced snapshot walker (2026-07-03 lag
# forensics fix #1). The NATS callbacks do ONLY `Event.set()` (O(1)) so the
# SDK's consumer is never blocked by our decode work; the walker below drains
# each dirty snapshot at most once per WALK_INTERVAL_MS. The flag race is
# BENIGN BY DESIGN: the walker clears the flag BEFORE walking, so an SDK
# update landing during/after the walk re-marks dirty and is drained on the
# next interval — never lost (the SDK snapshot only holds latest-per-
# instrument anyway), at worst one redundant walk that change-dedup collapses.
_LTP_DIRTY = threading.Event()
_INDEX_DIRTY = threading.Event()
# Capped walk-failure logging (first 5, then every 100th) so a transient
# mid-walk mutation (e.g. dict-changed-size during iteration while the SDK
# consumer writes) can never flood the log; the next interval retries and
# change-dedup makes the retry idempotent.
_WALK_ERRORS_TOTAL = 0


def _snapshot_walker_loop(out, sid_map, feed_cell) -> None:
    """Daemon: drain the dirty SDK snapshots at a bounded cadence (fix #1).

    Every `WALK_INTERVAL_MS` (default 200ms, env GROWW_WALK_INTERVAL_MS,
    clamped [20, 5000] — pure `resolve_walk_interval_ms`), and ONLY when the
    matching dirty flag is set, walk `get_ltp()` / `get_index_value()` through
    the UNCHANGED emit paths (capture-at-receipt fsync + change-dedup + drop
    accounting + watermark advance all preserved). Reads the CURRENT feed from
    the shared cell each iteration so reconnect cycles are followed
    automatically; a freshly-connected pre-subscribe feed walks an empty tree
    (emits nothing). Clear-BEFORE-walk ordering makes the dirty-flag race
    benign (see the flag comment above). The status-file heartbeat is NOT
    starved: `note_emit`'s 1/s status re-write runs inside these walks exactly
    as it did inside the callbacks; the stall detector + reject poller run on
    their own threads. Worst added first-tick latency = one walk interval.

    Backstop honesty: if this thread ever died, walks stop → emits stop → the
    watermark freezes → the EXISTING frozen-watermark criterion force-closes
    and the Rust FEED-STALL-01 process-kill is the outer backstop — never a
    silent-forever failure. The per-iteration broad except makes death
    practically unreachable."""
    global _WALK_ERRORS_TOTAL
    walk_secs = (
        resolve_walk_interval_ms(os.environ.get("GROWW_WALK_INTERVAL_MS")) / 1000.0
    )
    # DEMOTION (2026-07-03 per-callback capture): while the NATS-callback hook
    # is attached (_CAPTURE_MODE), this walker is a slow RECONCILIATION sweep
    # — it catches anything the hook missed; the shared dedup cache swallows
    # everything the hook already emitted. If capture is disabled (kill-switch)
    # or the attach failed, the walker runs at its full WALK_INTERVAL_MS
    # cadence exactly as before — the unchanged fallback path.
    reconcile_secs = (
        resolve_reconcile_interval_ms(os.environ.get("GROWW_RECONCILE_INTERVAL_MS"))
        / 1000.0
    )
    print(
        f"groww sidecar: coalesced snapshot walker armed "
        f"(interval={int(walk_secs * 1000)}ms; "
        f"reconcile={int(reconcile_secs * 1000)}ms while callback-capture is "
        "active; callbacks are O(1) flag sets)",
        flush=True,
    )
    while True:
        # Re-read per iteration: capture mode can flip on a reconnect cycle
        # (attach succeeded/failed on the fresh feed).
        capture_active = _CAPTURE_MODE[0]
        time.sleep(reconcile_secs if capture_active else walk_secs)
        feed = feed_cell[0]
        if feed is None:
            continue
        try:
            if _LTP_DIRTY.is_set():
                # Clear BEFORE the walk: an update landing mid-walk re-marks
                # dirty and is drained next interval — never lost.
                _LTP_DIRTY.clear()
                emit_ltp_records(out, feed.get_ltp(), sid_map, reconcile=capture_active)
            if _INDEX_DIRTY.is_set():
                _INDEX_DIRTY.clear()
                emit_index_records(
                    out, feed.get_index_value(), sid_map, reconcile=capture_active
                )
            # RAW-TICK-PROBE heartbeat: one float compare per iteration; a
            # bounded stderr line at most once per RAW_PROBE_HEARTBEAT_SECS.
            RAW_PROBE.maybe_heartbeat(time.monotonic())
        except Exception as exc:  # noqa: BLE001 - the walker must never die
            _WALK_ERRORS_TOTAL += 1
            if _WALK_ERRORS_TOTAL <= 5 or _WALK_ERRORS_TOTAL % 100 == 0:
                print(
                    f"groww sidecar: snapshot walk failed "
                    f"({type(exc).__name__}, total={_WALK_ERRORS_TOTAL}); "
                    "next interval retries (dedup makes retries idempotent)",
                    file=sys.stderr,
                    flush=True,
                )


def _stall_recovery_loop(feed_cell) -> None:
    """ACTIVE self-heal (2026-06-30): once data has started flowing, force the
    blocking `consume()` to return (by closing the NATS socket) whenever decoded
    records STALL across the whole universe DURING MARKET HOURS — so the
    except→reconnect loop re-subscribes within ms. This is the in-process fast
    path for the swallowed-close case that left the feed dead at 10:31 IST. Runs
    for the process lifetime as a daemon. Off-hours silence is normal → never
    force-reconnect (don't fight a legitimately-idle feed). Best-effort: any
    failure is caught; the Rust supervisor's process-kill stall-watchdog is the
    backstop. Pure decision via `should_force_reconnect` (unit-tested).

    `feed_cell` is the shared 1-element cell holding the CURRENT GrowwFeed (the
    reconnect loop updates it each cycle) so a force-close always targets the live
    socket, never the stale first-cycle one.

    LIVENESS SIGNAL (revised 2026-07-03 feed-death forensics): the loop keys on
    the ADVANCE of the max exchange timestamp seen (`MAX_TS_MILLIS_SEEN`), NOT
    on the decoded count. On 2026-07-03 the Groww snapshot froze at 09:07:55.770
    IST but kept re-broadcasting stale payloads for 31 minutes — the decoded
    count climbed past 1.16M "alive" records while zero fresh data arrived, so
    neither stall layer fired until the server itself closed the socket at
    09:38:53. A frozen-timestamp re-dump now reads as STALLED and force-closes
    within the deadline; the same pure `should_force_reconnect` decision applies
    (the watermark is the progress counter: 0 = cold/never-streamed → never
    close-loop; unchanged past the deadline in market hours → force-close).

    SECOND CRITERION — WATERMARK LAG (2026-07-03 lag forensics fix #2): the
    advance-based criterion above has a blind spot — a +2 ms micro-advance
    resets its clock while the watermark drifts MINUTES behind wall-clock
    (measured: 8s → 428s over 13 min with ZERO stall events). So each poll
    ALSO evaluates the pure `watermark_lag_stalled` (now_ms − max_ts_millis
    strictly > WATERMARK_MAX_LAG_MS during market hours, watermark known);
    after WATERMARK_LAG_CONSECUTIVE_CHECKS consecutive stalled verdicts —
    gated by the pure `watermark_lag_should_fire` refire cooldown — it fires
    the SAME force-close restart path. A healthy post-reconnect feed advances
    the watermark to ≈ now, the verdict flips False and the counter resets; a
    backlog that survives the reconnect refires at a bounded ~3-min cadence."""
    last_watermark = MAX_TS_MILLIS_SEEN
    last_change_monotonic = time.monotonic()
    lag_threshold_ms = resolve_watermark_max_lag_ms(
        os.environ.get("GROWW_WATERMARK_MAX_LAG_MS")
    )
    lag_consecutive = 0
    last_lag_fire_monotonic = None
    # Consecutive stall episodes (force-closes). Drives the exponential
    # force-close backoff (compute_stall_backoff_secs, 2026-07-06 exam fix)
    # so a persistently starved shard cannot churn a fresh Groww NATS
    # session every 5s (1,393 force-closes/hour observed) and pile dead
    # sessions against the account's concurrent-session slots. The ladder
    # resets to the 5s base ONLY on a DWELL-GATED watermark advance
    # (should_reset_stall_backoff, hostile-review hardening): a single
    # post-reconnect snapshot record inside the flap cycle must NOT reset —
    # otherwise the churn cadence stays pinned at the 5s base forever.
    consecutive_stall_episodes = 0
    # Monotonic instant of the LAST force-close fired by EITHER criterion
    # (frozen-watermark or watermark-lag). None until the first fire.
    last_stall_fire_monotonic = None
    while True:
        time.sleep(STALL_POLL_SECS)
        watermark = MAX_TS_MILLIS_SEEN
        market_open = _is_within_market_hours_ist(time.time())
        if watermark != last_watermark:
            last_watermark = watermark
            last_change_monotonic = time.monotonic()
            # A real decoded live record arrived. Reset the force-close
            # backoff ladder to the fast 5s base ONLY after a stall-free
            # dwell since the last force-close (hostile-review hardening):
            # the single post-reconnect snapshot record every flap cycle
            # delivers must NOT zero the ladder, or the churn cadence
            # stays pinned at the 5s base forever.
            if consecutive_stall_episodes > 0:
                secs_since_fire = (
                    None
                    if last_stall_fire_monotonic is None
                    else time.monotonic() - last_stall_fire_monotonic
                )
                if should_reset_stall_backoff(
                    secs_since_fire, STALL_BACKOFF_RESET_DWELL_SECS
                ):
                    consecutive_stall_episodes = 0
        else:
            secs_since_change = time.monotonic() - last_change_monotonic
            stall_deadline = compute_stall_backoff_secs(
                consecutive_stall_episodes,
                STALL_DEADLINE_SECS,
                STALL_BACKOFF_CAP_SECS,
            )
            if should_force_reconnect(
                watermark,
                last_watermark,
                secs_since_change,
                market_open,
                stall_deadline,
            ):
                consecutive_stall_episodes += 1
                next_wait = compute_stall_backoff_secs(
                    consecutive_stall_episodes,
                    STALL_DEADLINE_SECS,
                    STALL_BACKOFF_CAP_SECS,
                )
                # ONE coalesced line per stall episode (never per-poll spam):
                # the episode number + the backed-off wait before the next
                # force-close ride along with the diagnostic counts.
                print(
                    f"groww sidecar: FEED STALLED — {stall_deadline:g}s with NO advancing "
                    f"exchange timestamp across the universe during market hours "
                    f"(max_ts_millis={MAX_TS_MILLIS_SEEN}, emitted={EMITTED_TOTAL}, "
                    f"deduped={DEDUPED_TOTAL}, dropped={DROPPED_TOTAL}); a frozen snapshot "
                    "re-dump counts as stalled. Force-closing the NATS socket to trigger "
                    "reconnect + re-subscribe (self-heal). "
                    f"stall self-heal #{consecutive_stall_episodes}, "
                    f"next check in {next_wait:g}s",
                    file=sys.stderr,
                    flush=True,
                )
                feed = feed_cell[0]
                if feed is not None:
                    _force_close_nats_socket(feed)
                # Reset the clock so we don't hammer close() every poll while the
                # reconnect is in flight; the next real record re-arms the detector.
                last_change_monotonic = time.monotonic()
                # Stamp the fire instant — the dwell-gated ladder reset
                # measures stall-free time from HERE.
                last_stall_fire_monotonic = last_change_monotonic
                # One restart is in flight — the lag criterion must not stack a
                # second force-close on top of it this window.
                lag_consecutive = 0
                continue
        # Criterion 2 — watermark LAG (micro-advances do NOT reset this one).
        if watermark_lag_stalled(
            time.time() * 1000.0, watermark, market_open, lag_threshold_ms
        ):
            lag_consecutive += 1
        else:
            lag_consecutive = 0
        cooldown_remaining = 0.0
        if last_lag_fire_monotonic is not None:
            cooldown_remaining = WATERMARK_LAG_REFIRE_COOLDOWN_SECS - (
                time.monotonic() - last_lag_fire_monotonic
            )
        if watermark_lag_should_fire(
            lag_consecutive, WATERMARK_LAG_CONSECUTIVE_CHECKS, cooldown_remaining
        ):
            # A lag fire is ALSO a stall episode without an intervening real
            # live record — advance the shared force-close backoff ladder so
            # the frozen-watermark criterion cannot resume 5s churn on top of
            # a lag-driven restart (the lag path itself stays bounded by its
            # own ~3-min refire cooldown).
            consecutive_stall_episodes += 1
            next_wait = compute_stall_backoff_secs(
                consecutive_stall_episodes,
                STALL_DEADLINE_SECS,
                STALL_BACKOFF_CAP_SECS,
            )
            lag_ms = int(time.time() * 1000.0 - watermark)
            print(
                f"groww sidecar: FEED STALLED — watermark lag {lag_ms}ms behind "
                f"wall-clock (> {lag_threshold_ms}ms for "
                f"{WATERMARK_LAG_CONSECUTIVE_CHECKS} consecutive checks) during "
                f"market hours (max_ts_millis={watermark}, emitted={EMITTED_TOTAL}, "
                f"deduped={DEDUPED_TOTAL}, dropped={DROPPED_TOTAL}); micro-advancing "
                "timestamps that never catch up count as stalled. Force-closing the "
                "NATS socket to trigger reconnect + re-subscribe (self-heal). "
                f"stall self-heal #{consecutive_stall_episodes}, "
                f"next check in {next_wait:g}s",
                file=sys.stderr,
                flush=True,
            )
            feed = feed_cell[0]
            if feed is not None:
                _force_close_nats_socket(feed)
            lag_consecutive = 0
            last_lag_fire_monotonic = time.monotonic()
            # Also reset the advance clock so the frozen-watermark criterion
            # does not immediately double-fire while the reconnect is in flight.
            last_change_monotonic = time.monotonic()
            # Stamp the shared fire instant — a lag fire also arms the
            # dwell gate on the ladder reset.
            last_stall_fire_monotonic = last_lag_fire_monotonic


def silent_feed_watchdog(stocks: int, indices: int, feed_cell=None) -> None:
    """Warn LOUDLY (stderr) if subscribed but NO record decodes within the deadline,
    then ACTIVELY self-heal a mid-session stall.

    Runs as a daemon thread. The SDK's blocking `consume()` can swallow per-frame
    decode errors without raising, leaving us "subscribed but 0 ticks" with no
    actionable cause on stdout/stderr. This surfaces that truth: it samples the
    DECODED total (emitted + dropped) and, if it is still 0 after the first
    deadline, prints the most-likely causes; thereafter it re-warns periodically
    with the live counts until data flows. Once data IS flowing it hands off to
    `_stall_recovery_loop`, which force-reconnects on a market-hours stall (the
    10:31 IST swallowed-close case). `feed_cell` is the shared 1-element cell
    holding the CURRENT GrowwFeed handle used to reach the NATS socket for the
    force-close; if None (legacy / test), the active recovery is skipped and only
    the diagnostic runs.
    """
    time.sleep(SILENT_FEED_FIRST_WARN_SECS)
    if EMITTED_TOTAL + DROPPED_TOTAL > 0:
        # Data already decoding — hand off to the active stall-recovery loop so a
        # LATER mid-session stall self-heals (the 10:31 case: data flowed, then
        # stopped). Never returns — it watches for the process lifetime.
        if feed_cell is not None:
            _stall_recovery_loop(feed_cell)
        return
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
    still_silent_warns = 0
    while EMITTED_TOTAL + DROPPED_TOTAL == 0:
        # Rate-limited reminder cadence (2026-07-06 exam fix, item 3): the
        # first STILL_SILENT_FAST_WARNS reminders keep the 60s cadence; after
        # that the feed is persistently starved (the stall backoff ladder
        # would be at its cap) and the reminder slows to at most one line per
        # STILL_SILENT_RATE_LIMIT_SECS per process.
        time.sleep(
            still_silent_rewarn_interval_secs(
                still_silent_warns,
                SILENT_FEED_REWARN_SECS,
                STILL_SILENT_FAST_WARNS,
                STILL_SILENT_RATE_LIMIT_SECS,
            )
        )
        if EMITTED_TOTAL + DROPPED_TOTAL > 0:
            break
        still_silent_warns += 1
        print(
            "groww sidecar: STILL SILENT — 0 live records decoded "
            f"(emitted={EMITTED_TOTAL}, dropped={DROPPED_TOTAL}). "
            "See the first SILENT FEED diagnostic above for likely causes.",
            file=sys.stderr,
            flush=True,
        )
    # Data started flowing after the cold-silent phase → hand off to active
    # stall-recovery so a later mid-session stall self-heals too.
    if feed_cell is not None:
        _stall_recovery_loop(feed_cell)


def main() -> None:
    if not (os.environ.get("GROWW_SSM_TOKEN_PARAM") or os.environ.get("GROWW_ACCESS_TOKEN")):
        sys.exit(
            "Set GROWW_SSM_TOKEN_PARAM (prod: SSM path of the minter-written "
            "access token) or GROWW_ACCESS_TOKEN (local dev). This sidecar "
            "never mints — see the shared token-minter lock 2026-07-02."
        )

    os.makedirs(os.path.dirname(OUTPUT_PATH) or ".", exist_ok=True)
    out = _RotatingOut(OUTPUT_PATH)  # line-buffered append + IST-midnight rotation (PR-3)
    print(f"groww sidecar → appending NDJSON to {OUTPUT_PATH}", flush=True)

    stock_list, index_list, sid_map = wait_for_subscriptions()

    # Secrets to mask out of any logged exception detail (never log their values).
    # A MUTABLE list (not a tuple) so the access token can be added to it the
    # moment it is acquired/refreshed (line ~886) and the NATS reason hooks +
    # reject poller — which capture this object by reference — mask it too. Also
    # installs a redacting logging.Filter on the SDK loggers so SDK-emitted DEBUG
    # records are scrubbed structurally, not just our own print() lines.
    secrets = []  # the access token is added the moment it is read (below)
    _install_sdk_log_redaction(secrets)

    # Cached access token, reused across FEED reconnects. Re-READ from SSM
    # ONLY when there is no token yet or the previous failure was auth-class
    # (the shared token actually rotated/stale) — "never cache the token past
    # an auth failure" (lock 2026-07-02) while keeping the ms-ladder feed
    # reconnects from hammering SSM. NEVER minted.
    access_token = None
    # Continuous auth-failure episode tracking for the ONE edge-triggered
    # token-stale alert (>= TOKEN_STALE_ALERT_SECS -> REJECTED marker line).
    auth_stale_since = None
    token_stale_alerted = False
    # Count of consecutive failed cycles — drives the exponential backoff. Reset to
    # 0 after a fully successful cycle (auth OK + connected + consuming).
    consecutive_failures = 0
    # Start the silent-feed watchdog at most ONCE (the first time we reach a
    # successful subscribe), not per reconnect cycle — it watches the global
    # decoded counters for the whole process lifetime.
    watchdog_started = False
    # Print the SDK version + the REAL feed methods available in this environment
    # exactly ONCE (first feed-connect), not per reconnect cycle.
    feed_introspection_printed = False
    # Log the callback-capture kill-switch state / attach failure exactly once.
    capture_disabled_logged = False
    capture_attach_failed_logged = False

    # Reconnect loop — never give up (lock: not a single received tick missed).
    while True:
        # Track which phase fails so the log names auth vs feed-connect vs
        # subscribe vs consume (the cause is otherwise indistinguishable).
        phase = "auth"
        try:
            if access_token is None:
                access_token = _read_access_token()
                # Add the freshly-acquired bearer token to the live redaction set
                # (security fix 2026-06-29): a NATS-over-WS auth/handshake error's
                # repr can embed the CONNECT frame / auth payload carrying this
                # token, and the Rust supervisor forwards every sidecar stderr line
                # to CloudWatch — so the token MUST be masked before it can be
                # logged. The hooks/poller hold `secrets` by reference, so this also
                # covers a refreshed token without re-arming. (The JWT-shape mask in
                # _redact is the belt-and-suspenders backstop for any new shape.)
                _add_secret(secrets, access_token)
                # Explicit auth-success signal — distinguishes "auth succeeded, feed
                # connect failed" from "auth failed". Log only the token LENGTH,
                # never the token value.
                print(
                    "groww auth OK: shared access token read "
                    f"(len={len(access_token or '')}) — minted by the "
                    "groww-token-minter Lambda, never by this sidecar",
                    flush=True,
                )
            else:
                # Reusing the still-valid token from a previous successful auth —
                # no token-endpoint call, so no rate-limit pressure on reconnect.
                print("groww auth OK: reusing cached access token", flush=True)

            phase = "feed-connect"
            groww = GrowwAPI(access_token)
            feed = GrowwFeed(groww)
            # Publish the CURRENT feed so the long-lived stall-recovery watchdog
            # always force-closes the live socket on a mid-session stall, not the
            # stale first-cycle handle (the 10:31 IST self-heal).
            _CURRENT_FEED[0] = feed

            # PER-CALLBACK INSTANT CAPTURE (2026-07-03): attach the enqueue-only
            # hook to THIS cycle's fresh NATS client BEFORE subscribing, so the
            # very first message is captured. Re-attached every reconnect cycle
            # (each cycle constructs a new GrowwFeed). On kill-switch OFF or an
            # attach/topic-map failure: one log line, walker path unchanged.
            if CAPTURE_ENABLED:
                capture_topic_map = build_capture_topic_map(
                    stock_list, index_list, sid_map
                )
                attached = capture_topic_map is not None and install_callback_capture(
                    feed, _CAPTURE_QUEUE
                )
                if attached:
                    _CAPTURE_TOPIC_MAP[0] = capture_topic_map
                    if not _CAPTURE_MODE[0]:
                        print(
                            "groww sidecar: per-callback capture ARMED — "
                            "walker demoted to reconcile sweep "
                            "(GROWW_CALLBACK_CAPTURE=off reverts)",
                            flush=True,
                        )
                    _CAPTURE_MODE[0] = True
                else:
                    if not capture_attach_failed_logged:
                        capture_attach_failed_logged = True
                        print(
                            "groww sidecar: callback-capture hook NOT attached "
                            "(SDK internals changed?) — falling back to the "
                            "walker path unchanged",
                            file=sys.stderr,
                            flush=True,
                        )
                    _CAPTURE_MODE[0] = False
            elif not capture_disabled_logged:
                capture_disabled_logged = True
                print(
                    "groww sidecar: per-callback capture DISABLED by "
                    "GROWW_CALLBACK_CAPTURE — walker-only mode",
                    flush=True,
                )

            # DIAGNOSTIC (2026-06-29): print the installed SDK version + the REAL
            # public feed methods available in THIS environment, exactly once. This
            # definitively resolves whether `subscribe_index_value` exists (vs only
            # `subscribe_ltp` with segment="CASH" for indices) instead of guessing
            # from the docs — `dir(feed)` is the ground truth of the running wheel.
            if not feed_introspection_printed:
                try:
                    import growwapi as _growwapi_mod
                    sdk_version = getattr(_growwapi_mod, "__version__", "unknown")
                except Exception:  # noqa: BLE001 - introspection must never break the feed
                    sdk_version = "unknown"
                feed_methods = sorted(m for m in dir(feed) if not m.startswith("_"))
                print(
                    f"groww sidecar DIAGNOSTIC: growwapi.__version__={sdk_version} | "
                    f"feed methods={feed_methods}",
                    file=sys.stderr,
                    flush=True,
                )
                feed_introspection_printed = True

            phase = "subscribe"
            # Cache the subscribe counts so the first-emit `streaming` status write
            # (driven by note_emit on a REAL decoded tick) knows N stocks + M indices.
            global _SUBSCRIBED_STOCKS, _SUBSCRIBED_INDICES
            _SUBSCRIBED_STOCKS = len(stock_list)
            _SUBSCRIBED_INDICES = len(index_list)
            # COALESCED WALKS (2026-07-03 lag forensics fix #1): the callbacks
            # are O(1) dirty-flag sets ONLY — walking the full 768-entry
            # snapshot inside the SDK's consumer context starved the NATS
            # consumer (~42 walks/sec ≈ 32,500 decodes/sec) and let the SDK
            # snapshot lag wall-clock unboundedly. The walker thread (armed
            # once below) drains the dirty snapshots at ≤ 1/WALK_INTERVAL_MS.
            if stock_list:
                def on_ltp(_meta) -> None:
                    _LTP_DIRTY.set()

                feed.subscribe_ltp(stock_list, on_data_received=on_ltp)
            if index_list:
                def on_index(_meta) -> None:
                    _INDEX_DIRTY.set()

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
            # RAW-TICK-PROBE: under GROWW_RAW_PROBE=always, re-arm the bounded
            # sampler each reconnect cycle (default: once per process; the
            # running counters are never reset). No-op on the first cycle.
            RAW_PROBE.rearm_for_reconnect()
            # A full cycle succeeded up to the blocking consume — reset backoff so
            # the next genuine disconnect retries quickly, not at the capped delay.
            consecutive_failures = 0
            # Auth is healthy — close any token-stale episode (edge reset).
            auth_stale_since = None
            token_stale_alerted = False
            # Surface the REAL NATS reject reason (2026-06-29). The SDK swallows it:
            # an "Authorization Violation" never reaches a growwapi callback (it is
            # stored only on the underlying socket's last_error) and the bare
            # "Error:" line is empty. (1) Replace the SDK's own callbacks with
            # real-detail versions; (2) start a daemon poller that reads the socket's
            # last_error and prints one edge-triggered `GROWW LIVE FEED REJECTED:
            # <reason>` line + a periodic heartbeat. Both best-effort — a future-wheel
            # attribute rename is caught and logged, never breaks the feed. Armed once
            # per process (the poller watches the global counters for the lifetime).
            if not watchdog_started:
                install_nats_reason_hooks(feed, secrets)
                threading.Thread(
                    target=nats_reject_poller,
                    args=(feed, secrets),
                    name="groww-nats-reject-poller",
                    daemon=True,
                ).start()
                # Arm the silent-feed watchdog once: if the SDK's blocking consume()
                # streams nothing (swallowed feed reject / no entitlement / closed
                # market), it surfaces a loud, actionable diagnostic to stderr instead
                # of leaving the operator with only the SDK's empty "Error:" lines.
                # Pass the SHARED current-feed cell so the watchdog's active
                # stall-recovery (force-close → reconnect) always targets the LIVE
                # socket across reconnect cycles, not the stale first-cycle one.
                threading.Thread(
                    target=silent_feed_watchdog,
                    args=(len(stock_list), len(index_list), _CURRENT_FEED),
                    name="groww-silent-feed-watchdog",
                    daemon=True,
                ).start()
                # Arm the coalesced snapshot walker once per process (fix #1):
                # it follows reconnects via the shared current-feed cell, and
                # it only walks when a callback has marked a snapshot dirty.
                # (With callback-capture active it runs as the reconcile sweep.)
                threading.Thread(
                    target=_snapshot_walker_loop,
                    args=(out, sid_map, _CURRENT_FEED),
                    name="groww-snapshot-walker",
                    daemon=True,
                ).start()
                # Arm the per-callback capture writer once per process (2026-07-03):
                # it drains the process-lifetime bounded queue across reconnect
                # cycles (each cycle's fresh hook feeds the same queue). Armed
                # only when the kill-switch is on; if the hook never attaches,
                # the queue stays empty and this thread idles on a blocking get.
                if CAPTURE_ENABLED:
                    threading.Thread(
                        target=_capture_writer_loop,
                        args=(out, _CAPTURE_QUEUE),
                        name="groww-capture-writer",
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
            # Drop the cached token on ANY auth-class failure — the [auth] read
            # phase itself, OR a 401/auth-shaped reject in feed-connect/subscribe
            # (how the daily 06:00 IST token reset actually surfaces) — so the
            # NEXT iteration RE-READS the SSM parameter (never mints). A pure
            # feed-side failure keeps the token cached so ms-ladder reconnects
            # do not hammer SSM.
            auth_class = (phase == "auth" and not rate_limited) or _is_auth_error(exc)
            if auth_class:
                access_token = None
                if auth_stale_since is None:
                    auth_stale_since = time.time()
            elif consecutive_failures % TOKEN_REREAD_EVERY_N_FAILURES == 0:
                # Safety net (2026-07-02 adversarial finding M4): the 06:00 IST
                # daily reset can surface as a BARE socket close (not auth-shaped),
                # which would otherwise retry the cached stale token forever. Every
                # Nth consecutive failure, drop the cache so the next cycle re-reads
                # SSM — one cheap GetParameter, no Groww call, never a mint.
                access_token = None
            delay = _backoff_secs(consecutive_failures, rate_limited, retry_after)
            if auth_class:
                # Ride the 06:00->06:05 daily mint gap at a calm >=60s pace —
                # an auth-stale token cannot be fixed by fast reconnects, only
                # by the minter Lambda writing a fresh one.
                delay = max(delay, AUTH_RETRY_FLOOR_SECS)
                if _token_stale_alert_due(auth_stale_since, time.time(), token_stale_alerted):
                    token_stale_alerted = True
                    print(
                        "GROWW LIVE FEED REJECTED: access token stale for "
                        f"{int(time.time() - auth_stale_since)}s (>10min) — the "
                        "bruteX groww-token-minter Lambda may not have run; "
                        "check its last daily mint. This sidecar keeps retrying "
                        "the SSM read and NEVER mints a replacement.",
                        file=sys.stderr,
                        flush=True,
                    )
            # Surface the WHY for triage, with every known secret value masked and
            # the detail length-capped: a Groww SDK HTTP error can embed the
            # response body (and thus the access token) in its
            # str/repr/response, which the Rust supervisor captures from stdout
            # (security-review MEDIUM 2026-06-19). `_exception_detail` redacts the
            # access token and caps length so the cause is visible without
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
            # so it can never leak the access token that an SDK frame's
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


def _selftest_redaction() -> None:
    """Prove the redaction layers mask (a) a tuple value and (b) a JWT shape.

    Guarded behind `--selftest` so it NEVER runs in prod (prod runs with no args
    → main()). Run with: `python3 groww_sidecar.py --selftest`.
    """
    # (a) exact-value masking of a secret in the set (mutable-list path).
    secrets = ["my-api-key-12345678", "TOTPSEEDABCDEF"]
    token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJncm93dyJ9.s3cr3t-SIGNATURE_abcdef"
    _add_secret(secrets, token)
    assert token in secrets, "access token must be added to the live redaction set"

    line = (
        "NATS connect failed: CONNECT {\"auth_token\":\"my-api-key-12345678\"} "
        f"bearer {token} url=wss://socket-api.groww.in"
    )
    red = _redact(line, secrets)
    assert "my-api-key-12345678" not in red, "api_key value must be masked"
    assert token not in red, "access token value must be masked"
    assert "***REDACTED***" in red, "exact-value mask must fire"

    # (b) structural JWT-shape masking even when the token is NOT in `secrets`.
    unknown_jwt = "eyJ0eXAiOiJKV1QifQ.eyJ1aWQiOiI5OTkifQ.UNKNOWN-sig_0123456789"
    red2 = _redact(f"auth error: token={unknown_jwt} expired", secrets=[])
    assert unknown_jwt not in red2, "unanticipated JWT-shaped token must be masked"
    assert "***REDACTED_JWT***" in red2, "structural JWT mask must fire"

    # (c) the SDK logging filter scrubs a record BEFORE emit (structural path):
    rec = logging.LogRecord(
        name="growwapi.feed", level=logging.DEBUG, pathname=__file__, lineno=1,
        msg="connecting with bearer %s", args=(token,), exc_info=None,
    )
    _RedactingLogFilter(secrets).filter(rec)
    assert token not in rec.getMessage(), "SDK DEBUG record must be scrubbed"
    assert rec.args is None, "args must be cleared after structural redaction"

    # (d) the filter drops the raw exc_info 3-tuple so a fresh-render formatter
    # cannot re-leak an unredacted traceback (security-review MEDIUM 2026-06-29).
    try:
        raise ValueError(f"boom token={token}")
    except ValueError:
        rec2 = logging.LogRecord(
            name="nats.aio.client", level=logging.ERROR, pathname=__file__,
            lineno=1, msg="connect failed", args=None, exc_info=sys.exc_info(),
        )
    _RedactingLogFilter(secrets).filter(rec2)
    assert rec2.exc_info is None, "raw exc_info tuple must be cleared"

    print("groww sidecar redaction self-test: PASS")


def _selftest_self_heal() -> None:
    """Prove the ACTIVE self-heal decision functions (2026-06-30). Guarded behind
    `--selftest` so it NEVER runs in prod. Run: `python3 groww_sidecar.py --selftest`.
    """
    # Market-hours gate (start-inclusive 09:15, end-exclusive 15:30 IST).
    # 06:00 UTC = 11:30 IST → open.
    assert _is_within_market_hours_ist(_utc_epoch_for_ist(11, 30)), "11:30 IST is open"
    # 03:30 UTC = 09:00 IST → before open.
    assert not _is_within_market_hours_ist(_utc_epoch_for_ist(9, 0)), "09:00 IST is pre-open"
    # exactly 09:15 IST → open (inclusive).
    assert _is_within_market_hours_ist(_utc_epoch_for_ist(9, 15)), "09:15 IST is open (inclusive)"
    # exactly 15:30 IST → closed (exclusive).
    assert not _is_within_market_hours_ist(_utc_epoch_for_ist(15, 30)), "15:30 IST is closed (exclusive)"

    # should_force_reconnect: only fires on a market-hours stall AFTER data flowed.
    # decoded > 0, no change, past deadline, market open → True.
    assert should_force_reconnect(10, 10, STALL_DEADLINE_SECS + 1, True, STALL_DEADLINE_SECS), \
        "a market-hours stall after data flowed must force reconnect"
    # off-hours → never.
    assert not should_force_reconnect(10, 10, STALL_DEADLINE_SECS + 1, False, STALL_DEADLINE_SECS), \
        "off-hours silence must NOT force reconnect"
    # cold (no record ever decoded) → never (diagnostic + Rust backstop handle it).
    assert not should_force_reconnect(0, 0, STALL_DEADLINE_SECS + 1, True, STALL_DEADLINE_SECS), \
        "a never-streamed feed must NOT be close-looped"
    # data still advancing → never.
    assert not should_force_reconnect(11, 10, STALL_DEADLINE_SECS + 1, True, STALL_DEADLINE_SECS), \
        "advancing data is not a stall"
    # within deadline → never (no premature kill).
    assert not should_force_reconnect(10, 10, STALL_DEADLINE_SECS - 1, True, STALL_DEADLINE_SECS), \
        "a brief lull under the deadline must NOT force reconnect"

    print("groww sidecar self-heal self-test: PASS")


def _selftest_stall_backoff() -> None:
    """Prove the stall self-heal exponential-backoff ladder math (2026-07-06
    exam fix — 5s force-close churn storm) + the STILL SILENT rate-limit
    cadence. Guarded behind `--selftest` so it NEVER runs in prod. Run:
    `python3 groww_sidecar.py --selftest`. Full matrix in test_dedup.py."""
    base, cap = STALL_DEADLINE_SECS, STALL_BACKOFF_CAP_SECS
    # The ladder: 5 → 10 → 20 → 40 → 60 (cap) → 60 forever.
    assert compute_stall_backoff_secs(0, base, cap) == 5.0, \
        "fresh/reset ladder must use the fast 5s base"
    assert compute_stall_backoff_secs(1, base, cap) == 10.0, \
        "after 1 stall episode the wait doubles to 10s"
    assert compute_stall_backoff_secs(2, base, cap) == 20.0, \
        "after 2 stall episodes the wait doubles to 20s"
    assert compute_stall_backoff_secs(3, base, cap) == 40.0, \
        "after 3 stall episodes the wait doubles to 40s"
    assert compute_stall_backoff_secs(4, base, cap) == 60.0, \
        "after 4 stall episodes the wait caps at 60s (80 clamps)"
    assert compute_stall_backoff_secs(5, base, cap) == 60.0, \
        "the cap holds for every later episode"
    assert compute_stall_backoff_secs(10_000, base, cap) == 60.0, \
        "a huge episode count never overflows and stays at the cap"
    assert compute_stall_backoff_secs(-3, base, cap) == 5.0, \
        "a negative count clamps to the base (defensive)"
    # Reset semantics (hostile-review hardening): the ladder zeroes ONLY on a
    # dwell-gated advance — a single post-reconnect snapshot record inside
    # the flap cycle (seconds after the fire) must NOT reset.
    assert compute_stall_backoff_secs(0, base, cap) == float(STALL_DEADLINE_SECS), \
        "reset ladder == today's fast detection deadline"
    dwell = STALL_BACKOFF_RESET_DWELL_SECS
    assert not should_reset_stall_backoff(3.0, dwell), \
        "an advance seconds after a fire (the flap mode) must NOT reset"
    assert not should_reset_stall_backoff(dwell - 0.001, dwell), \
        "just under the dwell must NOT reset"
    assert should_reset_stall_backoff(dwell, dwell), \
        "exactly the dwell resets (inclusive boundary)"
    assert should_reset_stall_backoff(dwell + 300.0, dwell), \
        "a long-healthy session resets"
    assert should_reset_stall_backoff(None, dwell), \
        "no fire yet this process → reset allowed (ladder is 0 anyway)"
    assert dwell >= cap, \
        "dwell must be >= cap so a capped-cadence flap can never reset"

    # STILL SILENT cadence: fast (60s) for the first N warns, then one per 5min.
    fast, slow = SILENT_FEED_REWARN_SECS, STILL_SILENT_RATE_LIMIT_SECS
    for n in range(STILL_SILENT_FAST_WARNS):
        assert still_silent_rewarn_interval_secs(
            n, fast, STILL_SILENT_FAST_WARNS, slow
        ) == fast, "early reminders keep the fast cadence"
    assert still_silent_rewarn_interval_secs(
        STILL_SILENT_FAST_WARNS, fast, STILL_SILENT_FAST_WARNS, slow
    ) == 300.0, "once persistently silent, at most one reminder per 5 minutes"
    assert still_silent_rewarn_interval_secs(
        999, fast, STILL_SILENT_FAST_WARNS, slow
    ) == 300.0, "the slow cadence holds forever"

    print("groww sidecar stall-backoff self-test: PASS")


def _selftest_dedup() -> None:
    """Prove the change-dedup decision (2026-07-03 snapshot-flood fix). Guarded
    behind `--selftest` so it NEVER runs in prod. Run:
    `python3 groww_sidecar.py --selftest`. Full matrix in test_dedup.py."""
    cache = {}
    key = ("idx", "NSE", "CASH", "NIFTY")
    # First sight always emits.
    assert dedup_should_emit(cache, key, 1_783_069_200_183, 26965.05), \
        "first print must emit"
    # EXACT (ts, price) re-dump (the 530K-row flood shape) must be skipped.
    assert not dedup_should_emit(cache, key, 1_783_069_200_183, 26965.05), \
        "a frozen (ts, price) re-dump must be skipped"
    # Advancing ts with the SAME price is a genuine new print — never swallowed.
    assert dedup_should_emit(cache, key, 1_783_069_201_000, 26965.05), \
        "advancing tsInMillis with an identical price must emit"
    # Same ts, changed price (same-millisecond correction) must emit.
    assert dedup_should_emit(cache, key, 1_783_069_201_000, 26966.00), \
        "a changed price at the same tsInMillis must emit"
    # Keys are independent per instrument + kind discriminant.
    other = ("ltp", "NSE", "CASH", "2885")
    assert dedup_should_emit(cache, other, 1_783_069_200_183, 26965.05), \
        "a different instrument's first print must emit"
    assert len(cache) == 2, "cache is bounded by distinct instrument keys"

    # Stall-liveness watermark (pure): frozen ts never advances; fresh ts does;
    # garbage ts never advances (and never raises).
    assert ts_watermark_advanced(0, 1_783_069_200_183), "first ts must advance"
    assert not ts_watermark_advanced(1_783_069_200_183, 1_783_069_200_183), \
        "a frozen tsInMillis must NOT count as liveness"
    assert not ts_watermark_advanced(1_783_069_200_183, 1_783_069_100_000), \
        "an older (replayed) tsInMillis must NOT count as liveness"
    assert ts_watermark_advanced(1_783_069_200_183, 1_783_069_201_000.0), \
        "an advancing float tsInMillis must count as liveness"
    assert not ts_watermark_advanced(0, None) and not ts_watermark_advanced(0, "x"), \
        "unparseable tsInMillis must never advance (and never raise)"
    print("groww sidecar dedup self-test: PASS")


def _selftest_coalesce_and_watermark_lag() -> None:
    """Prove the coalesced-walk interval clamp + the watermark-lag stall
    decisions (2026-07-03 lag fix). Guarded behind `--selftest` so it NEVER
    runs in prod. Full matrix in test_dedup.py."""
    # Walk-interval clamp: garbage/absent → default; out-of-range → clamped.
    assert resolve_walk_interval_ms(None) == WALK_INTERVAL_MS_DEFAULT, \
        "absent env must resolve to the default walk interval"
    assert resolve_walk_interval_ms("garbage") == WALK_INTERVAL_MS_DEFAULT, \
        "garbage env must resolve to the default walk interval"
    assert resolve_walk_interval_ms("0") == WALK_INTERVAL_MS_MIN, \
        "a 0 interval must clamp up (never spin-loop the walker)"
    assert resolve_walk_interval_ms("999999") == WALK_INTERVAL_MS_MAX, \
        "a huge interval must clamp down (never stall the feed)"
    assert resolve_walk_interval_ms("200") == 200, "in-range passes through"

    # Watermark-lag verdict boundaries (threshold = 120_000ms default).
    thr = WATERMARK_MAX_LAG_MS_DEFAULT
    now = 1_783_069_320_000.0
    assert not watermark_lag_stalled(now, int(now) - 119_000, True, thr), \
        "119s lag must NOT be stalled"
    assert not watermark_lag_stalled(now, int(now) - 120_000, True, thr), \
        "exactly-threshold lag must NOT be stalled (strict >)"
    assert watermark_lag_stalled(now, int(now) - 121_000, True, thr), \
        "121s lag must be stalled"
    assert not watermark_lag_stalled(now, int(now) - 999_000, False, thr), \
        "off-market lag must NEVER be stalled"
    assert not watermark_lag_stalled(now, 0, True, thr), \
        "an unknown watermark must NEVER be stalled"

    # Fire decision: consecutive count + refire cooldown.
    assert not watermark_lag_should_fire(2, 3, 0.0), "below N consecutive: no fire"
    assert watermark_lag_should_fire(3, 3, 0.0), "at N consecutive: fire"
    assert not watermark_lag_should_fire(5, 3, 1.0), "cooldown suppresses refire"
    print("groww sidecar coalesce+watermark-lag self-test: PASS")


def _selftest_raw_probe() -> None:
    """Prove the RAW-TICK-PROBE sampler (2026-07-03, log-only). Guarded behind
    `--selftest` so it NEVER runs in prod. Full matrix in test_dedup.py."""
    lines = []
    probe = RawTickProbe(mode="", emit=lines.append)
    # Bounded at N: 20 drained stock ticks -> exactly N sample lines + 1 summary.
    for i in range(20):
        probe.observe(
            "ltp",
            str(2885 + i),
            {"tsInMillis": 1_783_069_200_000 + i, "ltp": 100.0 + i,
             "volume": 5000.0 if i % 2 == 0 else 0.0, "openInterest": 0.0,
             "open": 99.0, "high": 101.0, "low": 98.0, "close": 0.0},
        )
    samples = [l for l in lines if l.startswith("RAW-TICK-PROBE kind=ltp")]
    summaries = [l for l in lines if l.startswith("RAW-TICK-PROBE-SUMMARY")]
    assert len(samples) == RAW_PROBE_SAMPLE_COUNT, "sampler must be bounded at N"
    assert len(summaries) == 1, "exactly ONE summary after sampling completes"
    assert f"nonzero_volume_ticks=4/{RAW_PROBE_SAMPLE_COUNT}" in summaries[0], \
        "summary must count nonzero-volume samples (i=0,2,4,6 of the first 8)"
    assert f"nonzero_oi=0/{RAW_PROBE_SAMPLE_COUNT}" in summaries[0], \
        "summary must count nonzero-OI samples"
    assert f"nonzero_ohlc={RAW_PROBE_SAMPLE_COUNT}/{RAW_PROBE_SAMPLE_COUNT}" in summaries[0], \
        "any nonzero O/H/L/C marks the sample OHLC-populated"
    # Running counters keep counting PAST the sample bound (all 20).
    assert probe.total_ltp_ticks == 20 and probe.nonzero_volume_ticks == 10, \
        "running counters must cover every drained tick, not just samples"
    # Index sampling bounded at its own limit; never touches the ltp counters.
    for i in range(5):
        probe.observe("index", "NIFTY", {"tsInMillis": 1, "value": 26965.05})
    idx = [l for l in lines if l.startswith("RAW-TICK-PROBE kind=index")]
    assert len(idx) == RAW_PROBE_INDEX_SAMPLE_COUNT, "index sampler bounded"
    assert probe.total_ltp_ticks == 20, "index dicts never count as ltp ticks"
    # Default mode: rearm is a no-op. always mode: rearm resets sampling ONLY.
    probe.rearm_for_reconnect()
    probe.observe("ltp", "1", {"ltp": 1.0, "volume": 0.0})
    assert len([l for l in lines if l.startswith("RAW-TICK-PROBE kind=ltp")]) \
        == RAW_PROBE_SAMPLE_COUNT, "default mode samples once per process"
    always = RawTickProbe(mode="always", ltp_limit=2, emit=lines.append)
    assert always.always, "GROWW_RAW_PROBE=always must arm re-triggering"
    # probe_field_nonzero: proto3 no-presence 0.0 / absent / garbage are False.
    assert probe_field_nonzero(12.5) and probe_field_nonzero("3")
    assert not probe_field_nonzero(0.0) and not probe_field_nonzero(None)
    assert not probe_field_nonzero("x") and not probe_field_nonzero({})
    print("groww sidecar raw-probe self-test: PASS")


def _selftest_callback_capture() -> None:
    """Prove the per-callback capture primitives (2026-07-03, ≤1ms study
    variant c2). Guarded behind `--selftest` so it NEVER runs in prod. The
    full matrix (incl. dedup parity + capture_ns end-to-end) is in
    test_dedup.py; the Rust-side capture_ns parse is in groww_bridge.rs."""
    # Kill-switch resolver: default ON; off/0/false/no (any case) → OFF.
    assert resolve_callback_capture_enabled(None)
    assert resolve_callback_capture_enabled("") and resolve_callback_capture_enabled("on")
    for off in ("off", "OFF", "0", "false", "no"):
        assert not resolve_callback_capture_enabled(off), f"{off!r} must disable"
    # Reconcile-sweep interval clamp.
    assert resolve_reconcile_interval_ms(None) == RECONCILE_INTERVAL_MS_DEFAULT
    assert resolve_reconcile_interval_ms("50") == RECONCILE_INTERVAL_MS_MIN
    assert resolve_reconcile_interval_ms("999999") == RECONCILE_INTERVAL_MS_MAX
    # Hook: enqueue-only, bounded, never blocks, ALWAYS delegates.
    delegated = []
    bounded_queue = queue.Queue(maxsize=2)
    enqueued_cell, dropped_cell = [0], [0]
    hook = make_capture_hook(
        lambda subject, data: delegated.append((subject, data)),
        bounded_queue,
        enqueued_cell,
        dropped_cell,
    )
    before_ns = time.time_ns()
    for i in range(5):
        hook(f"/ld/eq/nse/price.{i}", b"payload")
    assert len(delegated) == 5, "original callback must ALWAYS be invoked"
    assert enqueued_cell[0] == 2 and dropped_cell[0] == 3, \
        "bounded queue: 2 enqueued, 3 dropped to counter (never blocked)"
    subject, payload, capture_ns = bounded_queue.get_nowait()
    assert subject == "/ld/eq/nse/price.0" and payload == b"payload"
    assert before_ns <= capture_ns <= time.time_ns(), "capture_ns is a real stamp"
    print("groww sidecar callback-capture self-test: PASS")


def _utc_epoch_for_ist(hour: int, minute: int) -> float:
    """Helper: a UTC epoch whose IST wall-clock is exactly `hour:minute` today.
    IST = UTC + 5:30, so UTC h:m = IST h:m − 5:30. Pure arithmetic for the test."""
    ist_sec_of_day = hour * 3600 + minute * 60
    # Pick a fixed day's UTC midnight, add the IST sec-of-day minus the offset.
    base_utc_midnight = 1_780_000_000 - (1_780_000_000 % 86400)
    return float(base_utc_midnight + ist_sec_of_day - int(5.5 * 3600))


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        _selftest_redaction()
        _selftest_self_heal()
        _selftest_stall_backoff()
        _selftest_dedup()
        _selftest_coalesce_and_watermark_lag()
        _selftest_raw_probe()
        _selftest_callback_capture()
    else:
        main()
