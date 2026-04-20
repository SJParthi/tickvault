#!/usr/bin/env python3
"""
Dhan 200-level Depth — Path × Method Matrix Tester

Parthiban 2026-04-20: "in python also we can check just all these right
i mean only slash or slash depth two hundred or directly just call their
python as whatever they have defined right? can we check all the
combinations dude"

Answer: yes. This script runs a SINGLE session that tests every known
URL path (and the Dhan SDK default) sequentially with the SAME token
and SAME SecurityId, then prints a PASS/FAIL matrix at the end.

What it tests
-------------
For each path below, opens a fresh WebSocket to
`wss://full-depth-api.dhan.co<path>?token=...&clientId=...&authType=2`,
sends the 200-depth subscribe JSON, waits `WAIT_SECS` for frames, and
records:

  * connect_ok   — socket handshake succeeded
  * subscribe_ok — server didn't RST immediately after subscribe
  * frames       — count of binary frames received in `WAIT_SECS`
  * closed_by    — "server" (RST) / "client" (keepalive timeout) / "timeout"
  * err          — exception message if any

Paths under test
----------------
  1. /                    — what dhanhq SDK uses by default
  2. /twohundreddepth     — what Dhan support told us (Ticket #5519522)
  3. /fulldepth           — speculative (some Dhan docs mention this)
  4. /depth200            — speculative (pattern from /twentydepth)
  5. dhanhq SDK direct    — calls `dhanhq.FullDepth(...)` unchanged
                            (confirms what the SDK does irrespective of URL)

Usage
-----
    source .venv-dhan/bin/activate
    pip install dhanhq==2.2.0rc1 websockets

    # Option A — read token from cache (typical):
    DHAN_SECURITY_ID=63434 python scripts/dhan-200depth-matrix.py

    # Option B — env-var token (if cache unavailable):
    DHAN_CLIENT_ID=1106656882 \
    DHAN_ACCESS_TOKEN=eyJ... \
    DHAN_SECURITY_ID=63434 \
    python scripts/dhan-200depth-matrix.py

Expected result during market hours (09:15-15:30 IST)
-----------------------------------------------------
  * At least one path shows `frames > 0`. That path is the correct one.
  * Paths that Dhan's server doesn't serve will RST-close within 3-5s.

Expected result post-market
---------------------------
  * ALL paths will close with keepalive timeout (~40s), `frames = 0`.
    That doesn't mean they're broken — Dhan's server is idle.
  * Post-market matrix is useful for auth + subscribe plumbing sanity
    only; the real answer comes from market hours.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import time
from dataclasses import dataclass, field
from typing import Optional

try:
    import websockets
    from websockets.exceptions import ConnectionClosed, InvalidStatus
except ImportError:
    print("ERROR: `pip install websockets` (expected already installed from venv setup).")
    sys.exit(2)


TOKEN_CACHE_PATH = "data/cache/tv-token-cache"
HOST = "wss://full-depth-api.dhan.co"
WAIT_SECS = 15  # how long to listen per path — tune up if you want more frames

# Path variants to test. First four are raw WebSocket; last is the dhanhq SDK.
RAW_PATHS = [
    ("/",                "dhanhq SDK default path"),
    ("/twohundreddepth", "Dhan support Ticket #5519522 recommendation"),
    ("/fulldepth",       "Speculative — some Dhan docs reference this"),
    ("/depth200",        "Speculative — mirrors /twentydepth naming"),
]


@dataclass
class PathResult:
    """Per-path outcome for the final summary table."""
    label: str
    path_or_method: str
    connect_ok: bool = False
    subscribe_ok: bool = False
    frames: int = 0
    closed_by: str = "timeout"  # "server" | "client" | "timeout"
    err: Optional[str] = None
    duration_s: float = 0.0

    def verdict(self) -> str:
        if self.frames > 0:
            return "PASS (streaming)"
        if not self.connect_ok:
            return "FAIL (connect)"
        if not self.subscribe_ok:
            return "FAIL (subscribe)"
        if self.closed_by == "server":
            return "FAIL (server RST)"
        # Post-market or idle — not a bug.
        return "IDLE (no frames, no RST)"


def load_credentials() -> tuple[str, str]:
    client_id = os.environ.get("DHAN_CLIENT_ID", "").strip()
    access_token = os.environ.get("DHAN_ACCESS_TOKEN", "").strip()
    if client_id and access_token:
        return client_id, access_token
    # Fall back to token cache file.
    if os.path.exists(TOKEN_CACHE_PATH):
        try:
            with open(TOKEN_CACHE_PATH, "r", encoding="utf-8") as fh:
                cache = json.load(fh)
            return (cache.get("client_id", ""), cache.get("access_token", ""))
        except Exception as exc:
            print(f"[WARN] Failed to read token cache: {exc}")
    print("ERROR: no credentials found — set env vars or ensure token cache exists.")
    sys.exit(2)


async def test_raw_path(path: str, label: str, client_id: str, token: str, sid: str) -> PathResult:
    """Raw-websockets test against a single path."""
    url = f"{HOST}{path}?token={token}&clientId={client_id}&authType=2"
    subscribe = json.dumps({
        "RequestCode": 23,
        "ExchangeSegment": "NSE_FNO",
        "SecurityId": sid,
    })
    result = PathResult(label=label, path_or_method=path)
    started = time.monotonic()
    try:
        # ping_interval=None disables client-side keepalive — we want to see
        # exactly when the SERVER decides to close, not when our client gives up.
        async with websockets.connect(url, ping_interval=None, open_timeout=10) as ws:
            result.connect_ok = True
            await ws.send(subscribe)
            # Assume subscribe is accepted unless server closes within 2s.
            try:
                early = await asyncio.wait_for(ws.recv(), timeout=2.0)
                result.subscribe_ok = True
                # First byte received — count as a frame.
                if isinstance(early, (bytes, bytearray)):
                    result.frames += 1
            except asyncio.TimeoutError:
                # Silence within 2s is normal — server acks via subsequent frames.
                result.subscribe_ok = True
            # Main listen loop.
            deadline = started + WAIT_SECS
            while time.monotonic() < deadline:
                remaining = max(0.1, deadline - time.monotonic())
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=remaining)
                    if isinstance(msg, (bytes, bytearray)):
                        result.frames += 1
                except asyncio.TimeoutError:
                    break
    except InvalidStatus as exc:
        result.err = f"HTTP {exc.response.status_code} on handshake"
        result.closed_by = "server"
    except ConnectionClosed as exc:
        # Distinguish server RST (no close frame) from orderly close.
        if exc.rcvd is None and exc.sent is None:
            result.closed_by = "server"
        elif exc.rcvd is None:
            result.closed_by = "server"
        else:
            result.closed_by = "server"
        result.err = f"{type(exc).__name__}: code={exc.code} reason={exc.reason!r}"
    except asyncio.TimeoutError:
        result.err = "open_timeout (handshake > 10s)"
    except Exception as exc:
        result.err = f"{type(exc).__name__}: {exc}"
    result.duration_s = time.monotonic() - started
    return result


def test_sdk(client_id: str, token: str, sid: str) -> PathResult:
    """
    Dhan SDK test — calls `dhanhq.FullDepth(...)` exactly as the SDK
    intends, with no path override. Confirms whatever URL the SDK has
    baked in and reports whether it streams.

    Uses a blocking call in a separate thread since `FullDepth` is
    synchronous. The thread is hard-killed via daemon=True + main-thread
    timeout so a stuck SDK call doesn't block the matrix script.
    """
    result = PathResult(label="dhanhq SDK direct (FullDepth)", path_or_method="SDK")
    try:
        from dhanhq import DhanContext, FullDepth  # type: ignore
    except ImportError:
        result.err = "dhanhq not installed — pip install dhanhq==2.2.0rc1"
        return result

    started = time.monotonic()
    frames_ref = {"count": 0, "closed": False, "err": None}

    def run_sdk() -> None:
        try:
            ctx = DhanContext(client_id, token)
            instruments = [(2, sid)]  # segment 2 = NSE_FNO
            response = FullDepth(ctx, instruments, 200)
            response.run_forever()
            deadline = time.monotonic() + WAIT_SECS
            while time.monotonic() < deadline:
                try:
                    data = response.get_data()
                    if data is not None:
                        frames_ref["count"] += 1
                except Exception:
                    # get_data() raises on close — break cleanly.
                    break
                if getattr(response, "on_close", False):
                    frames_ref["closed"] = True
                    break
                time.sleep(0.2)  # don't spin
        except Exception as exc:
            frames_ref["err"] = f"{type(exc).__name__}: {exc}"

    import threading
    t = threading.Thread(target=run_sdk, daemon=True)
    t.start()
    t.join(timeout=WAIT_SECS + 5)

    # Translate thread outcome into PathResult fields.
    result.connect_ok = frames_ref["err"] is None
    result.subscribe_ok = result.connect_ok
    result.frames = frames_ref["count"]
    result.closed_by = "server" if frames_ref["closed"] else "timeout"
    result.err = frames_ref["err"]
    result.duration_s = time.monotonic() - started
    return result


async def run_matrix() -> int:
    client_id, token = load_credentials()
    sid = os.environ.get("DHAN_SECURITY_ID", "63434").strip()
    if not sid:
        sid = "63434"

    print("=" * 78)
    print("Dhan 200-level Depth — Path × Method Matrix")
    print("=" * 78)
    print(f"  Host         : {HOST}")
    print(f"  Client ID    : {client_id}")
    print(f"  Security ID  : {sid}")
    print(f"  Wait / path  : {WAIT_SECS}s")
    print(f"  Started at   : {time.strftime('%Y-%m-%d %H:%M:%S IST')}")
    print("=" * 78)

    results: list[PathResult] = []

    # Raw-websockets tests.
    for path, description in RAW_PATHS:
        print(f"\n[TEST] path={path!r}  ({description})")
        r = await test_raw_path(path, description, client_id, token, sid)
        results.append(r)
        print(f"       connect={r.connect_ok} subscribe={r.subscribe_ok} "
              f"frames={r.frames} closed_by={r.closed_by} "
              f"duration={r.duration_s:.1f}s")
        if r.err:
            print(f"       err    : {r.err}")

    # SDK direct.
    print(f"\n[TEST] dhanhq SDK direct (FullDepth(dhan_ctx, [(2,'{sid}')], 200))")
    sdk_result = test_sdk(client_id, token, sid)
    results.append(sdk_result)
    print(f"       connect={sdk_result.connect_ok} subscribe={sdk_result.subscribe_ok} "
          f"frames={sdk_result.frames} closed_by={sdk_result.closed_by} "
          f"duration={sdk_result.duration_s:.1f}s")
    if sdk_result.err:
        print(f"       err    : {sdk_result.err}")

    # Summary table.
    print("\n" + "=" * 78)
    print("SUMMARY")
    print("=" * 78)
    print(f"{'Method/Path':<24} {'Connect':<8} {'Sub':<5} {'Frames':<7} "
          f"{'Closed by':<10} {'Verdict':<22}")
    print("-" * 78)
    any_stream = False
    for r in results:
        if r.frames > 0:
            any_stream = True
        print(f"{r.path_or_method:<24} "
              f"{'OK' if r.connect_ok else 'FAIL':<8} "
              f"{'OK' if r.subscribe_ok else '-':<5} "
              f"{r.frames:<7} "
              f"{r.closed_by:<10} "
              f"{r.verdict():<22}")

    print("=" * 78)
    if any_stream:
        print("AT LEAST ONE PATH STREAMED FRAMES — use that path in production.")
    else:
        # Differentiate "Dhan broken" from "market closed".
        all_idle = all(r.frames == 0 and r.closed_by != "server" for r in results)
        if all_idle:
            print("ALL PATHS IDLE — likely post-market or market holiday.")
            print("Retest during market hours (09:15-15:30 IST) for the real answer.")
        else:
            print("AT LEAST ONE PATH WAS SERVER-RST — Dhan server is rejecting that path.")
            print("Post the full matrix output to the open Dhan ticket.")
    print("=" * 78)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(run_matrix()))
    except KeyboardInterrupt:
        print("\nInterrupted by operator (Ctrl-C).")
        sys.exit(1)
