#!/usr/bin/env python3
"""
Depth-200 Python Sidecar Bridge — Production Variant
====================================================

Subscribes to N depth-200 contracts simultaneously via the Dhan
`full-depth-api` WebSocket and writes parsed depth frames to QuestDB
ILP (TCP port 9009) so they appear in the `deep_market_depth` table
exactly as the Rust client would have written them.

Why this exists
---------------
Our Rust 200-depth client is wire-equivalent to Dhan's official Python
SDK (User-Agent, no-ALPN TLS, root path `/?token=...`, RequestCode 23
flat JSON), but on expiry-day with 4 ATM contracts subscribed
concurrently from the same `clientId` Dhan resets the TCP connection
~5 s after subscribe. The Python SDK with the same wire signature
streamed cleanly for 30+ minutes on 2026-04-23 against a single
far-strike, so as a guaranteed fallback we run a Python subscriber
alongside the Rust app and let it stream into the same QuestDB
table. DEDUP keys handle any cross-source overlap.

Usage
-----
    pip install -r scripts/depth_200_bridge_requirements.txt

    export DHAN_CLIENT_ID='1106656882'
    export DHAN_ACCESS_TOKEN='<fresh JWT — load from data/cache/tv-token-cache>'

    python3 scripts/depth_200_bridge.py \
        --sid 72265:NSE_FNO \
        --sid 72266:NSE_FNO \
        --sid 67522:NSE_FNO \
        --sid 67523:NSE_FNO

    # Optional flags:
    #   --questdb-host 127.0.0.1   (default)
    #   --questdb-ilp-port 9009    (default)
    #   --depth-level 200          (default; rule says 200=root path)

The script falls back to `data/cache/tv-token-cache` (JSON) if the
DHAN_* env vars are missing — same path the Rust app's TokenManager
writes — so the operator can run it next to a live Rust app without
re-exporting the token.

Wire reference
--------------
- URL: `wss://full-depth-api.dhan.co/?token=<JWT>&clientId=<ID>&authType=2`
- Subscribe: `{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"72265"}`
- Header (12 bytes, little-endian):
    bytes 0-1   i16  message length
    byte  2     u8   response code (41=BID, 51=ASK, 50=DISCONNECT)
    byte  3     u8   exchange segment numeric
    bytes 4-7   i32  security_id
    bytes 8-11  u32  row count
- Each level (16 bytes): f64 price, u32 qty, u32 orders.

QuestDB schema (must match `deep_market_depth` exactly)
--------------
    segment   SYMBOL    e.g. "NSE_FNO"
    side      SYMBOL    "BID" or "ASK"
    depth_type SYMBOL   "200"
    security_id LONG
    level     LONG      1-based
    price     DOUBLE
    quantity  LONG
    orders    LONG
    exchange_sequence LONG  (0 — Dhan does not send sequence on 200-level row)
    received_at TIMESTAMP   local wall-clock at parse time
    ts        TIMESTAMP     designated; same value as received_at
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import socket
import struct
import sys
import time
from dataclasses import dataclass
from typing import Optional

# `websockets` is only required when we actually open a connection
# (i.e. inside `run_subscription`). Importing it lazily keeps the
# pure-function unit tests in `test_depth_200_bridge.py` runnable on a
# minimal Python install without third-party deps.
def _import_websockets():
    try:
        import websockets  # type: ignore[import-not-found]

        return websockets
    except ImportError:
        print(
            "ERROR: `websockets` not installed. Run: pip install -r "
            "scripts/depth_200_bridge_requirements.txt",
            file=sys.stderr,
        )
        sys.exit(2)


DHAN_FULL_DEPTH_BASE = "wss://full-depth-api.dhan.co"
TOKEN_CACHE_PATH = "data/cache/tv-token-cache"
DEPTH_TABLE = "deep_market_depth"

# Prometheus exposition endpoint defaults. Operator can override via
# --metrics-port; setting --metrics-port 0 disables the endpoint.
DEFAULT_METRICS_PORT = 9092
METRICS_NAMESPACE = "tv_depth_200_bridge"

# Default path that the Rust app's state writer (follow-up commit)
# atomically writes whenever the access token rotates or the depth
# rebalancer swaps an ATM contract. Operator can override with
# --state-file. The state-file mode is the production mode; --sid +
# env-token is the manual-operator-test fallback.
DEFAULT_STATE_FILE = "data/depth-200-bridge/state.json"
STATE_FILE_POLL_INTERVAL_SECS = 1.0

# Wire constants (per .claude/rules/dhan/full-market-depth.md).
DISCONNECT_CODE = 50
BID_CODE = 41
ASK_CODE = 51
HEADER_BYTES = 12
LEVEL_BYTES = 16
MAX_LEVELS = 200

# Reconnect backoff.
RECONNECT_BACKOFF_INITIAL_SECS = 1.0
RECONNECT_BACKOFF_MAX_SECS = 30.0
RECONNECT_IDLE_TIMEOUT_SECS = 45.0

# Valid Dhan segment names.
VALID_SEGMENTS = {"NSE_EQ", "NSE_FNO"}


@dataclass(frozen=True)
class Subscription:
    """One depth-200 subscription target."""

    security_id: int
    segment: str  # "NSE_FNO" or "NSE_EQ"

    @classmethod
    def parse(cls, raw: str) -> "Subscription":
        # Format: "<sid>:<segment>"   e.g. "72265:NSE_FNO"
        if ":" not in raw:
            raise ValueError(f"--sid must be SID:SEGMENT (got {raw!r})")
        sid_str, segment = raw.split(":", 1)
        sid = int(sid_str)
        if segment not in VALID_SEGMENTS:
            raise ValueError(
                f"segment {segment!r} not in {sorted(VALID_SEGMENTS)}"
            )
        return cls(security_id=sid, segment=segment)


@dataclass(frozen=True)
class BridgeState:
    """Snapshot of credentials + subscriptions as written by Rust.

    The Rust app's state writer (follow-up commit) atomically writes a
    JSON document of this shape whenever any of these change:
      * access token rotated (TokenManager refresh, every ~23h)
      * depth rebalancer issued a Swap200 (typically every 1-5 min)
      * boot: initial write with the 4 ATM contracts the planner picked

    The `version` field is monotonic — Python only reconciles when it
    changes, so the file can be touched without triggering reload.
    """

    client_id: str
    access_token: str
    subscriptions: tuple[Subscription, ...]
    version: int

    @classmethod
    def from_dict(cls, raw: dict) -> "BridgeState":
        client_id = raw.get("client_id", "")
        access_token = raw.get("access_token", "")
        if not client_id or not access_token:
            raise ValueError("state file missing client_id / access_token")
        subs_raw = raw.get("subscriptions", [])
        if not isinstance(subs_raw, list):
            raise ValueError("state file `subscriptions` must be a list")
        subs: list[Subscription] = []
        for entry in subs_raw:
            if not isinstance(entry, dict):
                raise ValueError(f"subscription entry must be dict: {entry!r}")
            sid_raw = entry.get("security_id")
            seg = entry.get("segment", "")
            if sid_raw is None or seg not in VALID_SEGMENTS:
                raise ValueError(f"bad subscription entry: {entry!r}")
            subs.append(Subscription(security_id=int(sid_raw), segment=seg))
        version = int(raw.get("version", 0))
        return cls(
            client_id=client_id,
            access_token=access_token,
            subscriptions=tuple(subs),
            version=version,
        )


def diff_subscriptions(
    old: tuple[Subscription, ...], new: tuple[Subscription, ...]
) -> tuple[set[Subscription], set[Subscription]]:
    """Return (to_remove, to_add) given old and new subscription sets."""
    old_set = set(old)
    new_set = set(new)
    return old_set - new_set, new_set - old_set


# ---------------------------------------------------------------------------
# Prometheus metrics — stdlib-only exposition (no client_python dep)
# ---------------------------------------------------------------------------
class BridgeMetrics:
    """Thread-safe in-memory metric registry served via /metrics endpoint.

    All counters are monotonic, all gauges are snapshotted at scrape time.
    Output matches the Prometheus text exposition format 0.0.4 so any
    scraper (Prometheus, VictoriaMetrics, OpenTelemetry collector) can
    ingest it without adapter code.
    """

    def __init__(self) -> None:
        self._lock = __import__("threading").Lock()
        # Counters: monotonic, exposed as Prometheus `counter` type.
        self.frames_total: dict[tuple[int, str, str], int] = {}  # (sid, side, segment) -> count
        self.reconnects_total: dict[int, int] = {}  # sid -> count
        self.ilp_writes_total: int = 0
        self.ilp_failures_total: int = 0
        self.state_reloads_total: int = 0
        # Gauges: instantaneous, exposed as Prometheus `gauge` type.
        self.active_subscriptions: int = 0
        self.state_version: int = 0
        self.last_frame_unix_secs: float = 0.0

    def inc_frames(self, sid: int, side: str, segment: str, delta: int = 1) -> None:
        key = (sid, side, segment)
        with self._lock:
            self.frames_total[key] = self.frames_total.get(key, 0) + delta
            self.last_frame_unix_secs = time.time()

    def inc_reconnects(self, sid: int) -> None:
        with self._lock:
            self.reconnects_total[sid] = self.reconnects_total.get(sid, 0) + 1

    def inc_ilp_writes(self) -> None:
        with self._lock:
            self.ilp_writes_total += 1

    def inc_ilp_failures(self) -> None:
        with self._lock:
            self.ilp_failures_total += 1

    def inc_state_reloads(self) -> None:
        with self._lock:
            self.state_reloads_total += 1

    def set_active_subscriptions(self, n: int) -> None:
        with self._lock:
            self.active_subscriptions = n

    def set_state_version(self, v: int) -> None:
        with self._lock:
            self.state_version = v

    def render_prometheus(self) -> bytes:
        """Render the full metric snapshot as Prometheus text format."""
        with self._lock:
            lines: list[str] = []
            lines.append(f"# HELP {METRICS_NAMESPACE}_frames_total Depth frames parsed")
            lines.append(f"# TYPE {METRICS_NAMESPACE}_frames_total counter")
            for (sid, side, segment), count in sorted(self.frames_total.items()):
                lines.append(
                    f'{METRICS_NAMESPACE}_frames_total{{security_id="{sid}",'
                    f'side="{side}",segment="{segment}"}} {count}'
                )
            lines.append(f"# HELP {METRICS_NAMESPACE}_reconnects_total WebSocket reconnects")
            lines.append(f"# TYPE {METRICS_NAMESPACE}_reconnects_total counter")
            for sid, count in sorted(self.reconnects_total.items()):
                lines.append(
                    f'{METRICS_NAMESPACE}_reconnects_total{{security_id="{sid}"}} {count}'
                )
            lines.append(f"# HELP {METRICS_NAMESPACE}_ilp_writes_total ILP TCP writes to QuestDB")
            lines.append(f"# TYPE {METRICS_NAMESPACE}_ilp_writes_total counter")
            lines.append(f"{METRICS_NAMESPACE}_ilp_writes_total {self.ilp_writes_total}")
            lines.append(f"# HELP {METRICS_NAMESPACE}_ilp_failures_total ILP TCP write failures")
            lines.append(f"# TYPE {METRICS_NAMESPACE}_ilp_failures_total counter")
            lines.append(f"{METRICS_NAMESPACE}_ilp_failures_total {self.ilp_failures_total}")
            lines.append(f"# HELP {METRICS_NAMESPACE}_state_reloads_total State-file reloads")
            lines.append(f"# TYPE {METRICS_NAMESPACE}_state_reloads_total counter")
            lines.append(f"{METRICS_NAMESPACE}_state_reloads_total {self.state_reloads_total}")
            lines.append(f"# HELP {METRICS_NAMESPACE}_active_subscriptions Active depth-200 subs")
            lines.append(f"# TYPE {METRICS_NAMESPACE}_active_subscriptions gauge")
            lines.append(
                f"{METRICS_NAMESPACE}_active_subscriptions {self.active_subscriptions}"
            )
            lines.append(f"# HELP {METRICS_NAMESPACE}_state_version Last loaded state version")
            lines.append(f"# TYPE {METRICS_NAMESPACE}_state_version gauge")
            lines.append(f"{METRICS_NAMESPACE}_state_version {self.state_version}")
            lines.append(
                f"# HELP {METRICS_NAMESPACE}_last_frame_unix_secs Wall-clock of last frame"
            )
            lines.append(f"# TYPE {METRICS_NAMESPACE}_last_frame_unix_secs gauge")
            lines.append(
                f"{METRICS_NAMESPACE}_last_frame_unix_secs {self.last_frame_unix_secs}"
            )
            return ("\n".join(lines) + "\n").encode("utf-8")


# Module-level singleton so any task can record metrics without plumbing it through.
METRICS = BridgeMetrics()


def start_metrics_server(port: int) -> Optional["object"]:
    """Bind a background HTTP server on `port` exposing /metrics. Returns None
    if `port == 0` (operator-disabled). The server runs in a daemon thread
    so it cannot block process shutdown."""
    if port <= 0:
        logging.info("metrics endpoint disabled (--metrics-port=0)")
        return None
    import http.server
    import threading

    class _Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802 - stdlib API
            if self.path != "/metrics":
                self.send_response(404)
                self.end_headers()
                return
            payload = METRICS.render_prometheus()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; version=0.0.4")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        # Suppress noisy default access logs — bridge logs go via tracing.
        def log_message(self, fmt: str, *args: object) -> None:  # noqa: ARG002
            return

    server = http.server.HTTPServer(("0.0.0.0", port), _Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True, name="bridge-metrics")
    thread.start()
    logging.info("metrics endpoint listening on :%d/metrics", port)
    return server


def load_credentials() -> tuple[str, str]:
    """Read Dhan client_id + access_token from env or token cache."""
    client_id = os.environ.get("DHAN_CLIENT_ID", "")
    access_token = os.environ.get("DHAN_ACCESS_TOKEN", "")
    if client_id and access_token:
        return client_id, access_token
    if os.path.exists(TOKEN_CACHE_PATH):
        try:
            with open(TOKEN_CACHE_PATH, encoding="utf-8") as fp:
                cache = json.load(fp)
            if not client_id:
                client_id = cache.get("client_id", "")
            if not access_token:
                access_token = cache.get("access_token", "")
            if client_id and access_token:
                return client_id, access_token
        except (OSError, ValueError) as err:
            logging.warning("could not read %s: %s", TOKEN_CACHE_PATH, err)
    raise SystemExit(
        "ERROR: DHAN_CLIENT_ID + DHAN_ACCESS_TOKEN must be set in env or "
        f"available in {TOKEN_CACHE_PATH}."
    )


def parse_depth_frame(data: bytes) -> Optional[tuple[str, list[tuple[float, int, int]], int, int, str]]:
    """Parse one depth frame.

    Returns (side, levels, security_id, row_count, segment_str) or None
    if the frame should be skipped (disconnect, unknown code, malformed).
    """
    if len(data) < HEADER_BYTES:
        return None
    msg_len, resp_code, seg_byte, sid, rows = struct.unpack(
        "<hBBiI", data[:HEADER_BYTES]
    )
    if resp_code == DISCONNECT_CODE:
        reason = (
            struct.unpack("<H", data[HEADER_BYTES : HEADER_BYTES + 2])[0]
            if len(data) >= HEADER_BYTES + 2
            else 0
        )
        logging.warning(
            "disconnect frame: reason=%s sid=%s seg=%s", reason, sid, seg_byte
        )
        return None
    side = {BID_CODE: "BID", ASK_CODE: "ASK"}.get(resp_code)
    if side is None:
        return None
    row_count = min(rows, MAX_LEVELS)
    levels: list[tuple[float, int, int]] = []
    for i in range(row_count):
        offset = HEADER_BYTES + (i * LEVEL_BYTES)
        if offset + LEVEL_BYTES > len(data):
            break
        price = struct.unpack("<d", data[offset : offset + 8])[0]
        qty = struct.unpack("<I", data[offset + 8 : offset + 12])[0]
        orders = struct.unpack("<I", data[offset + 12 : offset + 16])[0]
        if price <= 0:
            continue  # Dhan pads remaining levels with zeros; skip them.
        levels.append((price, qty, orders))
    # Map numeric segment back to string. Dhan: 1=NSE_EQ, 2=NSE_FNO.
    seg_str = {1: "NSE_EQ", 2: "NSE_FNO"}.get(seg_byte, f"UNKNOWN_{seg_byte}")
    return side, levels, sid, row_count, seg_str


def build_ilp_lines(
    table: str,
    segment: str,
    side: str,
    security_id: int,
    levels: list[tuple[float, int, int]],
    received_nanos: int,
) -> bytes:
    """Build one ILP line per depth level. Designated timestamp = received_nanos.

    QuestDB ILP wire format per https://questdb.io/docs/reference/api/ilp/overview/:
        table,sym1=v1,sym2=v2 col1=value1,col2=value2 <timestamp_nanos>\\n
    """
    lines: list[bytes] = []
    received_micros = received_nanos // 1_000
    for idx, (price, qty, orders) in enumerate(levels, start=1):
        # ILP escape: symbols and column names allow A-Z0-9_- only; values
        # are quoted appropriately. We never embed user input here, so
        # escaping is straight numeric formatting.
        line = (
            f"{table},segment={segment},side={side},depth_type=200 "
            f"security_id={security_id}i,"
            f"level={idx}i,"
            f"price={price},"
            f"quantity={qty}i,"
            f"orders={orders}i,"
            f"exchange_sequence=0i,"
            f"received_at={received_micros}t "
            f"{received_nanos}\n"
        )
        lines.append(line.encode("utf-8"))
    return b"".join(lines)


class IlpSender:
    """Lazy TCP sender for QuestDB ILP. Reconnects on EPIPE."""

    def __init__(self, host: str, port: int) -> None:
        self.host = host
        self.port = port
        self._sock: Optional[socket.socket] = None

    def _connect(self) -> socket.socket:
        sock = socket.create_connection((self.host, self.port), timeout=5.0)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._sock = sock
        logging.info("ILP connected: %s:%d", self.host, self.port)
        return sock

    def send(self, payload: bytes) -> None:
        if not payload:
            return
        for attempt in range(2):
            sock = self._sock or self._connect()
            try:
                sock.sendall(payload)
                return
            except OSError as err:
                logging.warning(
                    "ILP send failed (attempt %d): %s — reconnecting",
                    attempt + 1,
                    err,
                )
                try:
                    sock.close()
                except OSError:
                    pass
                self._sock = None
        raise RuntimeError("ILP send failed twice; aborting payload")

    def close(self) -> None:
        if self._sock is not None:
            try:
                self._sock.close()
            except OSError:
                pass
            self._sock = None


async def run_subscription(
    sub: Subscription,
    url: str,
    sender: IlpSender,
    stop: asyncio.Event,
) -> None:
    """Run one depth-200 subscription with reconnect backoff."""
    backoff = RECONNECT_BACKOFF_INITIAL_SECS
    sub_msg = json.dumps(
        {
            "RequestCode": 23,
            "ExchangeSegment": sub.segment,
            "SecurityId": str(sub.security_id),
        }
    )
    label = f"sid={sub.security_id} seg={sub.segment}"

    websockets = _import_websockets()

    while not stop.is_set():
        try:
            async with websockets.connect(
                url,
                ping_interval=20,
                ping_timeout=20,
                close_timeout=5,
            ) as ws:
                logging.info("[%s] connected — sending subscribe", label)
                await ws.send(sub_msg)
                backoff = RECONNECT_BACKOFF_INITIAL_SECS
                frames = 0

                while not stop.is_set():
                    try:
                        data = await asyncio.wait_for(
                            ws.recv(), timeout=RECONNECT_IDLE_TIMEOUT_SECS
                        )
                    except asyncio.TimeoutError:
                        logging.warning(
                            "[%s] no frame for %ds — reconnecting",
                            label,
                            int(RECONNECT_IDLE_TIMEOUT_SECS),
                        )
                        break

                    if not isinstance(data, (bytes, bytearray)):
                        logging.debug("[%s] non-binary frame: %r", label, data)
                        continue

                    parsed = parse_depth_frame(bytes(data))
                    if parsed is None:
                        continue
                    side, levels, parsed_sid, _row_count, seg_str = parsed
                    if not levels:
                        continue
                    received_nanos = time.time_ns()
                    payload = build_ilp_lines(
                        DEPTH_TABLE,
                        seg_str,
                        side,
                        parsed_sid,
                        levels,
                        received_nanos,
                    )
                    try:
                        sender.send(payload)
                        METRICS.inc_ilp_writes()
                    except Exception:  # noqa: BLE001 - record + re-raise
                        METRICS.inc_ilp_failures()
                        raise
                    METRICS.inc_frames(parsed_sid, side, seg_str, delta=1)
                    frames += 1
                    if frames % 200 == 0:
                        logging.info(
                            "[%s] streamed %d frames (last %s, %d levels)",
                            label,
                            frames,
                            side,
                            len(levels),
                        )
        except asyncio.CancelledError:
            raise
        except Exception as err:  # noqa: BLE001 - we want broad reconnect
            logging.warning(
                "[%s] connection error: %s — backoff %.1fs", label, err, backoff
            )
            METRICS.inc_reconnects(sub.security_id)
        if stop.is_set():
            break
        await asyncio.sleep(backoff)
        backoff = min(backoff * 2, RECONNECT_BACKOFF_MAX_SECS)

    logging.info("[%s] stopped", label)


def _build_url(client_id: str, access_token: str) -> str:
    return (
        f"{DHAN_FULL_DEPTH_BASE}/?token={access_token}"
        f"&clientId={client_id}&authType=2"
    )


def _install_signal_handlers(stop: asyncio.Event) -> None:
    loop = asyncio.get_running_loop()
    try:
        import signal

        for sig_name in ("SIGINT", "SIGTERM"):
            sig = getattr(signal, sig_name, None)
            if sig is not None:
                loop.add_signal_handler(sig, stop.set)
    except (NotImplementedError, RuntimeError):
        # Windows or non-main thread — Ctrl+C still works via KeyboardInterrupt.
        pass


async def main_async(
    subs: list[Subscription],
    url: str,
    sender: IlpSender,
) -> int:
    """Static-mode entry point: subscribe to a fixed list of SIDs and never reload."""
    stop = asyncio.Event()
    _install_signal_handlers(stop)

    tasks = [
        asyncio.create_task(run_subscription(s, url, sender, stop), name=f"sub-{s.security_id}")
        for s in subs
    ]
    try:
        await asyncio.gather(*tasks)
    except KeyboardInterrupt:
        stop.set()
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
    finally:
        sender.close()
    return 0


def _read_state_file(path: str) -> Optional[BridgeState]:
    """Read and parse the bridge state file. Returns None on missing / unreadable / invalid."""
    try:
        with open(path, encoding="utf-8") as fp:
            raw = json.load(fp)
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as err:
        logging.warning("state file %s: %s", path, err)
        return None
    try:
        return BridgeState.from_dict(raw)
    except ValueError as err:
        logging.warning("state file %s invalid schema: %s", path, err)
        return None


async def main_async_with_state_file(
    state_file: str,
    sender: IlpSender,
) -> int:
    """State-file-driven entry point.

    Reads `state_file` periodically (mtime poll). When `version`
    changes, reconciles the running subscription tasks against the new
    set, and rebuilds the URL if the access token rotated.
    """
    stop = asyncio.Event()
    _install_signal_handlers(stop)

    # Wait for the first state file to appear — Rust may not have
    # written it yet at boot.
    while not stop.is_set():
        state = _read_state_file(state_file)
        if state is not None:
            break
        logging.info("waiting for state file %s ...", state_file)
        try:
            await asyncio.wait_for(stop.wait(), timeout=STATE_FILE_POLL_INTERVAL_SECS)
        except asyncio.TimeoutError:
            pass
    if stop.is_set():
        return 0

    current_state = state
    current_url = _build_url(current_state.client_id, current_state.access_token)
    tasks: dict[Subscription, asyncio.Task] = {}
    last_mtime = os.path.getmtime(state_file)

    def _spawn(sub: Subscription) -> asyncio.Task:
        return asyncio.create_task(
            run_subscription(sub, current_url, sender, stop),
            name=f"sub-{sub.security_id}",
        )

    for sub in current_state.subscriptions:
        tasks[sub] = _spawn(sub)
    METRICS.set_active_subscriptions(len(tasks))
    METRICS.set_state_version(current_state.version)
    logging.info(
        "state-file mode: spawned %d subscription(s) at version=%d",
        len(tasks),
        current_state.version,
    )

    try:
        while not stop.is_set():
            try:
                await asyncio.wait_for(
                    stop.wait(), timeout=STATE_FILE_POLL_INTERVAL_SECS
                )
            except asyncio.TimeoutError:
                pass

            try:
                mtime = os.path.getmtime(state_file)
            except OSError:
                continue
            if mtime == last_mtime:
                continue
            last_mtime = mtime

            new_state = _read_state_file(state_file)
            if new_state is None or new_state.version == current_state.version:
                continue

            token_rotated = new_state.access_token != current_state.access_token
            client_changed = new_state.client_id != current_state.client_id

            if token_rotated or client_changed:
                # Cancel ALL tasks; the URL changed, so every connection must reconnect.
                logging.info(
                    "state v%d -> v%d: token/client rotated, restarting all %d tasks",
                    current_state.version,
                    new_state.version,
                    len(tasks),
                )
                for task in tasks.values():
                    task.cancel()
                await asyncio.gather(*tasks.values(), return_exceptions=True)
                tasks.clear()
                current_url = _build_url(new_state.client_id, new_state.access_token)
                for sub in new_state.subscriptions:
                    tasks[sub] = _spawn(sub)
            else:
                # Token unchanged — diff the subscription set.
                to_remove, to_add = diff_subscriptions(
                    current_state.subscriptions, new_state.subscriptions
                )
                if to_remove:
                    logging.info(
                        "state v%d -> v%d: cancelling %d subscription(s)",
                        current_state.version,
                        new_state.version,
                        len(to_remove),
                    )
                    for sub in to_remove:
                        task = tasks.pop(sub, None)
                        if task is not None:
                            task.cancel()
                    # Drain cancellations.
                    await asyncio.gather(
                        *(asyncio.shield(t) for t in []),
                        return_exceptions=True,
                    )
                if to_add:
                    logging.info(
                        "state v%d -> v%d: spawning %d new subscription(s)",
                        current_state.version,
                        new_state.version,
                        len(to_add),
                    )
                    for sub in to_add:
                        tasks[sub] = _spawn(sub)

            current_state = new_state
            METRICS.inc_state_reloads()
            METRICS.set_active_subscriptions(len(tasks))
            METRICS.set_state_version(current_state.version)
    finally:
        for task in tasks.values():
            task.cancel()
        await asyncio.gather(*tasks.values(), return_exceptions=True)
        sender.close()
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Depth-200 Python sidecar bridge — streams Dhan 200-level "
        "depth into the QuestDB `deep_market_depth` table."
    )
    # Two modes:
    #   1. --state-file: production mode. Reads creds + active SIDs from a JSON
    #      file the Rust app updates dynamically (token rotation, depth
    #      rebalancer Swap200, etc.). This is the primary path.
    #   2. --sid (+ env-token): manual operator-test mode for ad-hoc runs
    #      without the Rust app driving state.
    mode = parser.add_mutually_exclusive_group(required=False)
    mode.add_argument(
        "--state-file",
        nargs="?",
        const=DEFAULT_STATE_FILE,
        help=(
            "Production mode. Path to the state JSON the Rust app writes "
            "(default: %(default)s). Bridge polls mtime every 1s and "
            "reconciles subscriptions when version changes."
        ),
        default=None,
    )
    mode.add_argument(
        "--sid",
        action="append",
        metavar="SID:SEGMENT",
        help="Static mode. Repeatable. e.g. --sid 72265:NSE_FNO",
    )
    parser.add_argument("--questdb-host", default="127.0.0.1")
    parser.add_argument("--questdb-ilp-port", type=int, default=9009)
    parser.add_argument(
        "--metrics-port",
        type=int,
        default=DEFAULT_METRICS_PORT,
        help=(
            "Port for the Prometheus /metrics endpoint (default: %(default)d). "
            "Set to 0 to disable. Scrape with: curl http://127.0.0.1:<port>/metrics"
        ),
    )
    parser.add_argument(
        "--log-level",
        default="INFO",
        choices=("DEBUG", "INFO", "WARNING", "ERROR"),
    )
    args = parser.parse_args(argv)
    if not args.state_file and not args.sid:
        parser.error("must pass either --state-file or one or more --sid")
    return args


def main(argv: Optional[list[str]] = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])
    logging.basicConfig(
        level=getattr(logging, args.log_level),
        format="%(asctime)s %(levelname)s %(message)s",
        stream=sys.stderr,
    )
    sender = IlpSender(args.questdb_host, args.questdb_ilp_port)
    start_metrics_server(args.metrics_port)

    if args.state_file:
        logging.info(
            "starting bridge in state-file mode: %s (poll every %.1fs)",
            args.state_file,
            STATE_FILE_POLL_INTERVAL_SECS,
        )
        try:
            return asyncio.run(main_async_with_state_file(args.state_file, sender))
        except KeyboardInterrupt:
            return 130

    # Static mode (manual operator test).
    subs = [Subscription.parse(raw) for raw in args.sid]
    if len(subs) > 5:
        # Dhan limit on depth-200 connections per clientId.
        raise SystemExit(
            "ERROR: max 5 depth-200 connections per clientId (Dhan account "
            f"limit); got {len(subs)}"
        )

    client_id, access_token = load_credentials()
    url = _build_url(client_id, access_token)

    safe_url = url.replace(access_token, access_token[:8] + "...REDACTED")
    logging.info("starting bridge in static mode: subs=%d url=%s", len(subs), safe_url)

    try:
        return asyncio.run(main_async(subs, url, sender))
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
