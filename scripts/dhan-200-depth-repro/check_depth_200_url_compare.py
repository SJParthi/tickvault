#!/usr/bin/env python3
"""
tickvault — depth-200 URL-path comparison (`/` vs `/twohundreddepth`).

Bypasses the Dhan Python SDK (which hides the URL) and connects directly via
the `websockets` library. Prints a side-by-side outcome for both URL paths so
the operator has hard proof of which one actually works with the SELF token.

Usage:
    source /tmp/dhan-venv/bin/activate
    pip install 'websockets>=12'   # if not already installed
    export DHAN_CLIENT_ID='1106656882'
    export DHAN_ACCESS_TOKEN='<paste SELF JWT here>'
    python scripts/dhan-200-depth-repro/check_depth_200_url_compare.py \\
        --sid 74147 --secs 12

This will run TWO 12-second tests in sequence:
    1. wss://full-depth-api.dhan.co/?token=...           (root path — verified 2026-04-23)
    2. wss://full-depth-api.dhan.co/twohundreddepth?token=...   (Dhan docs / Ticket #5519522)

Outputs a comparison table at the end:

    +------------------+----------+--------+-----------------------+
    | URL path         | Frames   | RST?   | Verdict               |
    +------------------+----------+--------+-----------------------+
    | /                | 1187     | no     | PASS sustained stream |
    | /twohundreddepth | 0        | yes    | FAIL reset-without... |
    +------------------+----------+--------+-----------------------+

Exit codes:
    0  at least one URL streamed successfully
    1  both URLs failed
    3  bad env / token
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import json
import os
import sys
import time
from dataclasses import dataclass

try:
    import websockets
except ImportError:
    print("ERROR: websockets not installed. Run: pip install 'websockets>=12'", file=sys.stderr)
    sys.exit(3)


WSS_BASE = "wss://full-depth-api.dhan.co"

# Subscribe code 23, NSE_FNO segment, 1 instrument per Dhan 200-level rules.
# Per .claude/rules/dhan/full-market-depth.md rule 11: 200-level uses flat JSON
# (no InstrumentList wrapper), but the SDK + working clients use the
# InstrumentList form too — both shapes are accepted server-side.
def build_subscribe_msg(security_id: str) -> str:
    return json.dumps({
        "RequestCode": 23,
        "InstrumentCount": 1,
        "InstrumentList": [{"ExchangeSegment": "NSE_FNO", "SecurityId": security_id}],
    })


def decode_jwt_payload(jwt: str) -> dict:
    payload = jwt.split(".")[1]
    payload += "=" * ((4 - len(payload) % 4) % 4)
    return json.loads(base64.urlsafe_b64decode(payload))


@dataclass
class TestResult:
    url_path: str
    frame_count: int
    rst_observed: bool
    error: str
    elapsed_secs: float


async def test_one_url(
    url_path: str,
    token: str,
    client_id: str,
    security_id: str,
    secs: int,
) -> TestResult:
    full_url = f"{WSS_BASE}{url_path}?token={token}&clientId={client_id}&authType=2"
    print(f"\n=== Testing {WSS_BASE}{url_path}?token=<...>&clientId={client_id}&authType=2 ===")

    start = time.time()
    frame_count = 0
    rst_observed = False
    error_msg = ""

    try:
        async with websockets.connect(full_url, max_size=None) as ws:
            print(f"[{time.time()-start:.2f}s] WebSocket OPEN")
            await ws.send(build_subscribe_msg(security_id))
            print(f"[{time.time()-start:.2f}s] Subscribe sent for SID={security_id}")

            deadline = time.time() + secs
            while time.time() < deadline:
                remaining = deadline - time.time()
                try:
                    frame = await asyncio.wait_for(ws.recv(), timeout=min(1.0, remaining))
                    if isinstance(frame, (bytes, bytearray)) and len(frame) >= 12:
                        frame_count += 1
                        if frame_count % 50 == 0:
                            print(f"[{time.time()-start:.2f}s] frame {frame_count} (binary, {len(frame)} bytes)")
                    elif isinstance(frame, str):
                        print(f"[{time.time()-start:.2f}s] text frame: {frame[:120]}")
                except asyncio.TimeoutError:
                    continue
    except websockets.exceptions.ConnectionClosedError as e:
        # Reset-without-handshake = the APP-token-rejection signature.
        # rcvd_then_sent / rcvd / no close-frame → server tore the connection.
        rst_observed = True
        error_msg = f"ConnectionClosedError: code={e.code} reason={e.reason!r}"
        print(f"[{time.time()-start:.2f}s] {error_msg}")
    except websockets.exceptions.InvalidStatusCode as e:
        error_msg = f"HTTP upgrade rejected: status={e.status_code}"
        print(f"[{time.time()-start:.2f}s] {error_msg}")
    except Exception as e:  # noqa: BLE001 — diagnostic script
        error_msg = f"{type(e).__name__}: {e}"
        print(f"[{time.time()-start:.2f}s] {error_msg}")
        if "no close" in str(e).lower() or "reset" in str(e).lower():
            rst_observed = True

    elapsed = time.time() - start
    print(f"[{elapsed:.2f}s] DONE — frames={frame_count}, rst={rst_observed}")
    return TestResult(
        url_path=url_path,
        frame_count=frame_count,
        rst_observed=rst_observed,
        error=error_msg,
        elapsed_secs=elapsed,
    )


def render_table(results: list[TestResult]) -> str:
    lines = []
    lines.append("\n" + "=" * 78)
    lines.append("  COMPARISON RESULT")
    lines.append("=" * 78)
    lines.append(f"  {'URL path':<22} {'Frames':>8} {'RST?':>6} {'Elapsed':>10}  Verdict")
    lines.append("  " + "-" * 74)
    for r in results:
        verdict = (
            "PASS sustained stream"
            if r.frame_count >= 10
            else f"WEAK ({r.frame_count} frames)"
            if r.frame_count > 0
            else "FAIL no frames"
        )
        if r.rst_observed:
            verdict += " [RESET]"
        lines.append(
            f"  {r.url_path:<22} {r.frame_count:>8} {('yes' if r.rst_observed else 'no'):>6} "
            f"{r.elapsed_secs:>9.1f}s  {verdict}"
        )
    lines.append("=" * 78)
    return "\n".join(lines)


async def amain() -> int:
    parser = argparse.ArgumentParser(description="depth-200 URL-path comparison")
    parser.add_argument("--sid", required=True, help="SecurityId, e.g. 74147")
    parser.add_argument("--secs", type=int, default=12, help="Per-URL test duration (default 12s)")
    parser.add_argument(
        "--paths",
        nargs="+",
        default=["/", "/twohundreddepth"],
        help="URL paths to test (default tests both)",
    )
    args = parser.parse_args()

    client_id = os.environ.get("DHAN_CLIENT_ID")
    token = os.environ.get("DHAN_ACCESS_TOKEN")
    if not client_id or not token:
        print("ERROR: DHAN_CLIENT_ID and DHAN_ACCESS_TOKEN must be set", file=sys.stderr)
        return 3

    try:
        claims = decode_jwt_payload(token)
        consumer_type = claims.get("tokenConsumerType", "<unknown>")
        print(f"Token type: tokenConsumerType={consumer_type!r}, dhanClientId={claims.get('dhanClientId')!r}")
        if consumer_type == "APP":
            print("WARN: APP-type token — depth-200 is expected to fail with RST")
            print("      To prove the SELF fix, paste a SELF token from web.dhan.co")
    except Exception as e:  # noqa: BLE001
        print(f"WARN: could not decode JWT: {e}", file=sys.stderr)

    results: list[TestResult] = []
    for path in args.paths:
        results.append(await test_one_url(path, token, client_id, args.sid, args.secs))
        await asyncio.sleep(1.0)  # let server clean up before next attempt

    print(render_table(results))

    any_pass = any(r.frame_count >= 10 for r in results)
    return 0 if any_pass else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(amain()))
