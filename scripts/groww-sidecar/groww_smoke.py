#!/usr/bin/env python3
"""Groww smoke test (LOCAL-ONLY) — de-risk the tick-object shape.

Authenticates with the official `growwapi` SDK (api_key + TOTP), subscribes to a
single instrument's live feed, and PRINTS the first few raw tick objects so we can
lock the exact field names before finalizing `groww_sidecar.py`.

HONEST: written against the Groww docs; lines that depend on the exact SDK shape
are marked `# VERIFY`. Run this FIRST and paste the printed objects back.

Usage:
    export GROWW_API_KEY=...   GROWW_TOTP_SECRET=...
    python3 groww_smoke.py
"""
import json
import os
import sys

import pyotp

try:
    # VERIFY: confirm these import names against your installed growwapi.
    from growwapi import GrowwAPI, GrowwFeed
except Exception as exc:  # pragma: no cover - environment dependent
    sys.exit(f"growwapi import failed ({exc}). `pip install -r requirements.txt` first.")


def main() -> None:
    api_key = os.environ.get("GROWW_API_KEY")
    totp_secret = os.environ.get("GROWW_TOTP_SECRET")
    if not api_key or not totp_secret:
        sys.exit("Set GROWW_API_KEY and GROWW_TOTP_SECRET in the environment.")

    totp = pyotp.TOTP(totp_secret).now()
    # VERIFY: docs show `GrowwAPI.get_access_token(api_key=..., totp=...)`.
    access_token = GrowwAPI.get_access_token(api_key=api_key, totp=totp)
    groww = GrowwAPI(access_token)
    feed = GrowwFeed(groww)

    seen = {"n": 0}

    def on_data(message) -> None:
        # Print the RAW object EXACTLY as the SDK delivers it, so we can read the
        # real field names (ltp / volume / timestamp / exchange_token / segment).
        print(json.dumps(message, indent=2, default=str), flush=True)
        seen["n"] += 1
        if seen["n"] >= 5:
            print("--- got 5 ticks; paste the above back to lock the mapping ---", flush=True)
            os._exit(0)

    # VERIFY: the subscribe call + its argument shape. The Groww docs show
    # `GrowwFeed` subscribe helpers (e.g. subscribe_live_data / subscribe_ltp).
    # Replace the instrument with a liquid one (e.g. NIFTY 50 / RELIANCE).
    feed.subscribe_live_data(  # VERIFY method name + args
        [{"exchange": "NSE", "segment": "CASH", "exchange_token": "2885"}],  # VERIFY token (RELIANCE example)
        on_data,
    )
    print("subscribed — waiting for the first 5 ticks (Ctrl-C to stop)…", flush=True)
    feed.consume()  # VERIFY: blocking consume loop


if __name__ == "__main__":
    main()
