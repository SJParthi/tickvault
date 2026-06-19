#!/usr/bin/env python3
"""Groww validation sidecar (LOCAL-ONLY, operator lock §32) — the PRODUCER.

VERIFIED against the official growwapi-1.5.0 SDK source
(docs/groww-ref/10-live-feed-mapping-verified.md). The live feed is
NATS-over-WebSocket + Protobuf; GrowwFeed handles transport + decode. The
callback receives the topic META; the parsed tick is pulled via get_ltp(),
shaped `{exchange: {segment: {exchange_token: {ltp, tsInMillis, volume, ...}}}}`.

Each received tick is appended as one NDJSON line to data/groww/live-ticks.ndjson
— the EXACT schema the Rust bridge (crates/app/src/groww_bridge.rs) parses.
Capture-at-receipt: write + flush + fsync the instant the callback fires (durable
floor one hop downstream of the socket; lock §32.3).

Usage:
    export GROWW_API_KEY=...   GROWW_TOTP_SECRET=...
    python3 groww_sidecar.py
"""
import json
import os
import sys
import time
from datetime import datetime, timezone, timedelta

import pyotp

try:
    from growwapi import GrowwAPI, GrowwFeed
except Exception as exc:  # pragma: no cover
    sys.exit(f"growwapi import failed ({exc}). `pip install -r requirements.txt` first.")

# The path the Rust bridge tails (GROWW_TICK_FILE_DEFAULT in groww_bridge.rs).
OUTPUT_PATH = os.environ.get("GROWW_TICK_FILE", "data/groww/live-ticks.ndjson")
IST = timezone(timedelta(hours=5, minutes=30))

# Your validation set. exchange_token comes from Groww's instrument.csv.
# 2885 = RELIANCE (NSE CASH). Add more dicts to watch more instruments (<=1000).
WATCH = [{"exchange": "NSE", "segment": "CASH", "exchange_token": "2885"}]

# Groww (exchange, segment) -> our canonical segment string (bridge contract).
# Verified values: exchange NSE/BSE, segment CASH/FNO.
SEGMENT_MAP = {
    ("NSE", "CASH"): "NSE_EQ",
    ("NSE", "FNO"): "NSE_FNO",
    ("BSE", "CASH"): "BSE_EQ",
    ("BSE", "FNO"): "BSE_FNO",
}


def ms_to_ist_nanos(ts_millis: int) -> int:
    """Groww `tsInMillis` (UTC epoch ms) -> IST epoch nanoseconds.

    Mirrors the Dhan "store IST wall-clock directly" rule (data-integrity.md):
    convert UTC ms -> IST wall clock, then to nanos. Keeps ms precision.
    """
    dt_utc = datetime.fromtimestamp(ts_millis / 1000.0, tz=timezone.utc)
    dt_ist = dt_utc.astimezone(IST)
    return int(dt_ist.replace(tzinfo=timezone.utc).timestamp() * 1_000_000_000)


def emit_records(out, ltp_tree: dict) -> None:
    """Flatten get_ltp() `{exchange:{segment:{token:{...}}}}` and append NDJSON.

    Verified tick keys: `ltp`, `tsInMillis` (ms), `volume` (cumulative).
    Instrument identity comes from the tree path (exchange/segment/token), NOT
    the tick body.
    """
    if not isinstance(ltp_tree, dict):
        return
    for exchange, segs in ltp_tree.items():
        if not isinstance(segs, dict):
            continue
        for segment, tokens in segs.items():
            canonical = SEGMENT_MAP.get((str(exchange), str(segment)))
            if canonical is None or not isinstance(tokens, dict):
                continue
            for token, tick in tokens.items():
                if not isinstance(tick, dict) or "ltp" not in tick:
                    continue
                try:
                    ts_millis = int(tick.get("tsInMillis", 0))
                    rec = {
                        "security_id": int(token),
                        "segment": canonical,
                        "ts_ist_nanos": ms_to_ist_nanos(ts_millis) if ts_millis else 0,
                        "exchange_ts_millis": ts_millis,
                        "ltp": float(tick["ltp"]),
                        "cum_volume": int(tick.get("volume", 0)),
                    }
                except (KeyError, ValueError, TypeError):
                    continue
                out.write(json.dumps(rec) + "\n")
                out.flush()
                os.fsync(out.fileno())  # capture-at-receipt durability


def main() -> None:
    api_key = os.environ.get("GROWW_API_KEY")
    totp_secret = os.environ.get("GROWW_TOTP_SECRET")
    if not api_key or not totp_secret:
        sys.exit("Set GROWW_API_KEY and GROWW_TOTP_SECRET in the environment.")

    os.makedirs(os.path.dirname(OUTPUT_PATH) or ".", exist_ok=True)
    out = open(OUTPUT_PATH, "a", buffering=1)  # line-buffered append
    print(f"groww sidecar → appending NDJSON to {OUTPUT_PATH}", flush=True)

    # Reconnect loop — never give up (lock: not a single received tick missed).
    while True:
        try:
            totp = pyotp.TOTP(totp_secret).now()
            access_token = GrowwAPI.get_access_token(api_key=api_key, totp=totp)
            groww = GrowwAPI(access_token)
            feed = GrowwFeed(groww)

            def on_update(_meta) -> None:
                # Pull the full parsed LTP tree and emit whatever changed.
                emit_records(out, feed.get_ltp())

            feed.subscribe_ltp(WATCH, on_update)
            print("subscribed — streaming…", flush=True)
            feed.consume()  # blocking
        except KeyboardInterrupt:
            print("stopping.", flush=True)
            break
        except Exception as exc:  # noqa: BLE001 - reconnect on any error
            print(f"groww sidecar error: {exc!r} — reconnecting in 5s", flush=True)
            time.sleep(5)


if __name__ == "__main__":
    main()
