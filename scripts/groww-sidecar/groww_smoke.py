#!/usr/bin/env python3
"""Groww smoke test (LOCAL-ONLY) — confirm the live-feed tick shape on a real run.

VERIFIED against the official growwapi-1.5.0 SDK source (see
docs/groww-ref/10-live-feed-mapping-verified.md). The live feed is
NATS-over-WebSocket + Protobuf; the SDK's GrowwFeed handles transport/decode.
The callback receives the topic META; you pull the parsed tick via get_ltp().

This script subscribes one instrument's LTP and PRINTS the parsed tick dict +
its meta so we can eyeball the real field names (expected: ltp, tsInMillis,
volume, open, high, low, close, ...).

Usage (shared token-minter lock 2026-07-02 — this script NEVER mints):
    export GROWW_ACCESS_TOKEN=<token>                 # direct token, or:
    export GROWW_SSM_TOKEN_PARAM=/tickvault/prod/groww/access-token
    python3 groww_smoke.py
"""
import json
import os
import sys

try:
    from growwapi import GrowwAPI, GrowwFeed
except Exception as exc:  # pragma: no cover - environment dependent
    sys.exit(f"growwapi import failed ({exc}). `pip install -r requirements.txt` first.")

# A liquid instrument to watch. exchange_token comes from Groww's instrument.csv
# (https://growwapi-assets.groww.in/instruments/instrument.csv). 2885 = RELIANCE
# (NSE). Replace with any token you want to watch.
WATCH = [{"exchange": "NSE", "segment": "CASH", "exchange_token": "2885"}]


def main() -> None:
    # Shared token-minter lock 2026-07-02: read the bruteX-Lambda-minted token
    # (env override for local dev, else read-only SSM GetParameter). NEVER mint.
    access_token = os.environ.get("GROWW_ACCESS_TOKEN", "").strip()
    if not access_token:
        param = os.environ.get("GROWW_SSM_TOKEN_PARAM")
        if not param:
            sys.exit(
                "Set GROWW_ACCESS_TOKEN (direct token) or GROWW_SSM_TOKEN_PARAM "
                "(SSM path of the minter-written token). This script never mints."
            )
        import boto3

        region = os.environ.get("AWS_REGION") or "ap-south-1"
        access_token = (
            boto3.client("ssm", region_name=region)
            .get_parameter(Name=param, WithDecryption=True)["Parameter"]["Value"]
            .strip()
        )
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
