#!/usr/bin/env python3
"""
Dhan Official 200-Level Full Market Depth Test
==============================================

EXACT script shared by Dhan Support team after the Google Meet
(ref: Ticket #5519522, #5543510, Apr 17 2026 Dhan Cares email:
 "try connecting to the WebSocket for Full Market Depth, as discussed
  over the G-Meet, since it is working fine now from our end").

This uses Dhan's OFFICIAL Python SDK — `dhanhq.FullDepth` — so the
result is a clean apples-to-apples test against Dhan's own reference
implementation. If this script connects and streams data, the fault
is in our Rust client. If it fails the same way as the Rust client
(TCP reset within 3-5 seconds), the fault is on Dhan's side and we
have verbatim proof to send back.

PREREQUISITES
-------------
    pip install dhanhq==2.2.0rc1

USAGE
-----
    # Credentials via env (recommended — never commit tokens):
    export DHAN_CLIENT_ID="1106656882"
    export DHAN_ACCESS_TOKEN="eyJ..."
    python3 scripts/dhan-official-200depth.py

    # Override SecurityId if Dhan gives a specific contract to test
    # (default is NIFTY 21 APR 24150 CALL = 63424 from Dhan's own sample):
    DHAN_SECURITY_ID="63424" DHAN_SEGMENT="2" \
        python3 scripts/dhan-official-200depth.py

SEGMENT CODES (from Dhan annexure-enums)
----------------------------------------
    0 = IDX_I        (indices)
    1 = NSE_EQ       (NSE equity)
    2 = NSE_FNO      (NSE F&O)          ← default, matches Dhan's sample
    3 = NSE_CURRENCY
    4 = BSE_EQ
    5 = MCX_COMM
    7 = BSE_CURRENCY (no 6)
    8 = BSE_FNO

OUTPUT
------
Streams depth frames to stdout until Ctrl-C OR server disconnect.
On disconnect prints a clear message so you know immediately whether
the connection held or was reset by Dhan's server.

Share stdout back on the Dhan ticket — they asked for this exact
reproducer.
"""

import os
import sys
import time
from datetime import datetime


def main() -> int:
    client_id = os.environ.get("DHAN_CLIENT_ID", "").strip()
    access_token = os.environ.get("DHAN_ACCESS_TOKEN", "").strip()
    security_id = os.environ.get("DHAN_SECURITY_ID", "63424").strip()
    segment = int(os.environ.get("DHAN_SEGMENT", "2").strip())
    depth_level = int(os.environ.get("DHAN_DEPTH_LEVEL", "200").strip())

    if not client_id or not access_token:
        print("ERROR: set DHAN_CLIENT_ID and DHAN_ACCESS_TOKEN env vars.")
        print("       (Never hardcode tokens — they rotate every 24h.)")
        return 2

    try:
        from dhanhq import DhanContext, FullDepth  # type: ignore
    except ImportError:
        print("ERROR: dhanhq SDK not installed.")
        print("       Run:  pip install dhanhq==2.2.0rc1")
        return 2

    print("=" * 70)
    print("Dhan Official 200-Level Full Market Depth Test")
    print("=" * 70)
    print(f"  Started at        : {datetime.now().isoformat()}")
    print(f"  Client ID         : {client_id}")
    print(f"  Security ID       : {security_id}")
    print(f"  Segment (numeric) : {segment}")
    print(f"  Depth level       : {depth_level}")
    print("=" * 70)

    dhan_context = DhanContext(client_id, access_token)
    instruments = [(segment, security_id)]

    try:
        response = FullDepth(dhan_context, instruments, depth_level)
        response.run_forever()

        frames = 0
        started = time.time()
        while True:
            data = response.get_data()
            if data is not None:
                frames += 1
                if frames <= 5 or frames % 50 == 0:
                    print(f"[frame {frames:>5}] {data}")

            if getattr(response, "on_close", False):
                elapsed = time.time() - started
                print("-" * 70)
                print(f"RESULT: Server disconnection after {elapsed:.1f}s")
                print(f"        Total frames received: {frames}")
                if elapsed < 10.0 and frames == 0:
                    print("        Pattern matches the TCP-reset-within-5s bug")
                    print("        (Ticket #5519522 / #5543510). Send this log")
                    print("        back to Dhan support.")
                else:
                    print("        Clean disconnect after real streaming.")
                break
    except KeyboardInterrupt:
        print("\nInterrupted by operator (Ctrl-C). Exiting cleanly.")
    except Exception as exc:  # noqa: BLE001 — diagnostic script, print all
        print(f"EXCEPTION: {type(exc).__name__}: {exc}")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
