#!/usr/bin/env python3
"""
Dhan 200-Level Depth — Paste-and-Run (no env vars needed)
=========================================================

Minimum-friction reproducer for Dhan Ticket #5519522 / #5543510.
Paste your CLIENT_ID, ACCESS_TOKEN, and SECURITY_ID directly below,
save, run:

    pip install dhanhq==2.2.0rc1
    python3 scripts/dhan-200depth-paste-and-run.py

Uses Dhan's OFFICIAL `dhanhq.FullDepth` class — same code path the Dhan
Support team uses internally. If this succeeds, our Rust client is the
odd one out. If it fails the same way (TCP reset in <5s), we have clean
evidence to send back on the ticket.

SECURITY NOTE
-------------
DO NOT commit a real token. The placeholder below is intentionally
obviously-bogus so an accidental commit is caught by the secret scanner.
If you push this file with a real token, rotate it immediately at
web.dhan.co.
"""

# ============================================================
# PASTE YOUR VALUES HERE — then save and run
# ============================================================

CLIENT_ID      = "1106656882"                        # your Dhan client id
ACCESS_TOKEN   = "PASTE_YOUR_JWT_HERE_eyJ..."        # 24h JWT from web.dhan.co
SECURITY_ID    = "63424"                             # integer as string; e.g. NIFTY 24150 CE
SEGMENT        = 2                                   # 2 = NSE_FNO, 1 = NSE_EQ, 0 = IDX_I
DEPTH_LEVEL    = 200                                 # 20 or 200

# ============================================================
# Nothing below this line needs editing
# ============================================================

import sys
import time
from datetime import datetime


def guard_placeholders() -> None:
    if "PASTE_YOUR" in ACCESS_TOKEN or not ACCESS_TOKEN.strip():
        print("ERROR: edit this file and paste your ACCESS_TOKEN at the top.")
        sys.exit(2)
    if not CLIENT_ID.strip() or not SECURITY_ID.strip():
        print("ERROR: CLIENT_ID and SECURITY_ID cannot be empty.")
        sys.exit(2)


def main() -> int:
    guard_placeholders()

    try:
        from dhanhq import DhanContext, FullDepth  # type: ignore
    except ImportError:
        print("ERROR: dhanhq SDK not installed.")
        print("       Run:  pip install dhanhq==2.2.0rc1")
        return 2

    print("=" * 70)
    print("Dhan 200-Level Full Market Depth — Paste-and-Run")
    print("=" * 70)
    print(f"  Started at      : {datetime.now().isoformat()}")
    print(f"  Client ID       : {CLIENT_ID}")
    print(f"  Security ID     : {SECURITY_ID}")
    print(f"  Segment         : {SEGMENT} (2=NSE_FNO, 1=NSE_EQ, 0=IDX_I)")
    print(f"  Depth level     : {DEPTH_LEVEL}")
    print("=" * 70)

    dhan_context = DhanContext(CLIENT_ID, ACCESS_TOKEN)
    instruments = [(SEGMENT, SECURITY_ID)]

    started = time.time()
    frames = 0
    try:
        response = FullDepth(dhan_context, instruments, DEPTH_LEVEL)
        response.run_forever()

        while True:
            data = response.get_data()
            if data is not None:
                frames += 1
                if frames <= 5 or frames % 50 == 0:
                    print(f"[frame {frames:>5}] {data}")

            if getattr(response, "on_close", False):
                elapsed = time.time() - started
                print("-" * 70)
                print(f"RESULT : server closed after {elapsed:.1f}s")
                print(f"         frames received = {frames}")
                if elapsed < 10.0 and frames == 0:
                    print("         PATTERN MATCHES Ticket #5519522 / #5543510")
                    print("         (TCP reset within 5s, zero frames). Send")
                    print("         this stdout back to Dhan support.")
                else:
                    print("         Clean stream, then disconnect.")
                break
    except KeyboardInterrupt:
        print(f"\nInterrupted (Ctrl-C). Total frames: {frames}.")
    except Exception as exc:  # noqa: BLE001
        print(f"EXCEPTION: {type(exc).__name__}: {exc}")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
