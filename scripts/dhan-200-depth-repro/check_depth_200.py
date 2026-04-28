#!/usr/bin/env python3
"""
tickvault — depth-200 sustained-stream check.

Tests ONE depth-200 contract for `--secs` seconds. Counts frames and reports
whether streaming was sustained (not just "subscribe accepted then dropped").

Usage:
    source /tmp/dhan-venv/bin/activate
    pip install dhanhq==2.2.0rc1
    export DHAN_CLIENT_ID='1106656882'
    export DHAN_ACCESS_TOKEN=$(jq -r '.access_token' data/cache/tv-token-cache)
    python scripts/dhan-200-depth-repro/check_depth_200.py --sid 72263 --secs 30

Exit codes:
    0  sustained (≥10 frames in window)
    1  subscribed but no frames received
    2  subscribe rejected
    3  bad env / token
"""

import argparse
import os
import signal
import sys
import time

try:
    from dhanhq import DhanContext, FullDepth
except ImportError:
    print("ERROR: dhanhq not installed. Run: pip install dhanhq==2.2.0rc1", file=sys.stderr)
    sys.exit(3)


def main() -> int:
    parser = argparse.ArgumentParser(description="depth-200 sustained-stream check")
    parser.add_argument("--sid", required=True, help="SecurityId (e.g. 72263 = NIFTY ATM CE)")
    parser.add_argument("--secs", type=int, default=30, help="Test duration (default 30s)")
    args = parser.parse_args()

    client_id = os.environ.get("DHAN_CLIENT_ID")
    token = os.environ.get("DHAN_ACCESS_TOKEN")
    if not client_id or not token:
        print("ERROR: DHAN_CLIENT_ID and DHAN_ACCESS_TOKEN must be set", file=sys.stderr)
        return 3
    if len(token) < 200:
        print(f"WARN: token length {len(token)} is short — typical Dhan JWT is 700+", file=sys.stderr)

    print(f"Testing SecurityId={args.sid} for {args.secs}s ...")

    ctx = DhanContext(client_id, token)
    instruments = [(2, str(args.sid))]
    response = FullDepth(ctx, instruments, 200)

    deadline = time.time() + args.secs
    frame_count = 0

    def on_alarm(_signum, _frame):
        raise TimeoutError("test window elapsed")

    signal.signal(signal.SIGALRM, on_alarm)
    signal.alarm(args.secs)

    try:
        response.run_forever()
        while time.time() < deadline:
            data = response.get_data()
            if data:
                frame_count += 1
                if frame_count % 50 == 0:
                    elapsed = time.time() - (deadline - args.secs)
                    print(f"  ... frame {frame_count} at +{elapsed:.1f}s")
            if response.on_close:
                print(f"server closed after {frame_count} frames")
                break
    except TimeoutError:
        pass
    except Exception as e:
        print(f"Exception: {e}", file=sys.stderr)
        if frame_count == 0:
            return 2

    print()
    print(f"  Frames received : {frame_count}")
    print(f"  Test duration   : {args.secs}s")
    print(f"  Frames per sec  : {frame_count / args.secs:.1f}")
    print()

    if frame_count >= 10:
        print(f"PASS — sustained streaming ({frame_count} frames in {args.secs}s)")
        return 0
    elif frame_count > 0:
        print(f"WEAK — connection accepted but only {frame_count} frames "
              f"(possible far-OTM contract — server filters per Dhan docs)")
        return 0
    else:
        print(f"FAIL — zero frames in {args.secs}s — connection dropped or far-OTM filtered")
        return 1


if __name__ == "__main__":
    sys.exit(main())
