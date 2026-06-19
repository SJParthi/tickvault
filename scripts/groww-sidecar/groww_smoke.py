#!/usr/bin/env python3
"""Groww smoke test (LOCAL-ONLY) — confirm the live-feed tick shape on a real run.

VERIFIED against the official growwapi-1.5.0 SDK source (see
docs/groww-ref/10-live-feed-mapping-verified.md). The live feed is
NATS-over-WebSocket + Protobuf; the SDK's GrowwFeed handles transport/decode.
The callback receives the topic META; you pull the parsed tick via get_ltp().

This script subscribes one instrument's LTP and PRINTS the parsed tick dict +
its meta so we can eyeball the real field names (expected: ltp, tsInMillis,
volume, open, high, low, close, ...).

Usage:
    export GROWW_API_KEY=...   GROWW_TOTP_SECRET=...
    python3 groww_smoke.py
"""
import json
import os
import sys

import pyotp

try:
    from growwapi import GrowwAPI, GrowwFeed
except Exception as exc:  # pragma: no cover - environment dependent
    sys.exit(f"growwapi import failed ({exc}). `pip install -r requirements.txt` first.")

# A liquid instrument to watch. exchange_token comes from Groww's instrument.csv
# (https://growwapi-assets.groww.in/instruments/instrument.csv). 2885 = RELIANCE
# (NSE). Replace with any token you want to watch.
WATCH = [{"exchange": "NSE", "segment": "CASH", "exchange_token": "2885"}]


def main() -> None:
    api_key = os.environ.get("GROWW_API_KEY")
    totp_secret = os.environ.get("GROWW_TOTP_SECRET")
    if not api_key or not totp_secret:
        sys.exit("Set GROWW_API_KEY and GROWW_TOTP_SECRET in the environment.")

    # 1a. REST access token (verified: POST /v1/token/api/access, key_type=totp).
    totp = pyotp.TOTP(totp_secret).now()
    access_token = GrowwAPI.get_access_token(api_key=api_key, totp=totp)
    groww = GrowwAPI(access_token)

    # Live feed: NATS-over-WS + protobuf, all handled by GrowwFeed (1b socket token).
    feed = GrowwFeed(groww)

    seen = {"n": 0}

    def on_update(meta) -> None:
        # The callback receives the topic META (exchange/segment/feed_key/feed_type),
        # NOT the tick. Pull the parsed tick dict via get_ltp().
        ltp_data = feed.get_ltp()
        print(json.dumps({"meta": meta, "ltp": ltp_data}, indent=2, default=str), flush=True)
        seen["n"] += 1
        if seen["n"] >= 5:
            print("--- got 5 updates; the field names above are the verified shape ---", flush=True)
            os._exit(0)

    # subscribe_ltp(instrument_list, on_data_received) — verified signature.
    feed.subscribe_ltp(WATCH, on_update)
    print("subscribed — waiting for the first 5 LTP updates (Ctrl-C to stop)…", flush=True)
    feed.consume()  # blocking — joins the NATS consume thread


if __name__ == "__main__":
    main()
