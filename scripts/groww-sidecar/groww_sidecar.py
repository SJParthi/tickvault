#!/usr/bin/env python3
"""Groww validation sidecar (LOCAL-ONLY, operator lock §32) — the PRODUCER.

Authenticates with the official `growwapi` SDK (api_key + TOTP), subscribes to the
live feed, and appends each received tick as one NDJSON line to
`data/groww/live-ticks.ndjson` — the EXACT schema the Rust bridge
(`crates/app/src/groww_bridge.rs`) parses. Capture-at-receipt: write + flush +
fsync the instant the callback fires (durable floor one hop downstream of the
socket; see lock §32.3).

HONEST: written against the Groww docs; every line that depends on the exact SDK
tick shape is marked `# VERIFY`. Run `groww_smoke.py` FIRST to confirm the field
names, then adjust `tick_to_record()` below if needed.

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
    from growwapi import GrowwAPI, GrowwFeed  # VERIFY import names
except Exception as exc:  # pragma: no cover
    sys.exit(f"growwapi import failed ({exc}). `pip install -r requirements.txt` first.")

# The path the Rust bridge tails (GROWW_TICK_FILE_DEFAULT in groww_bridge.rs).
OUTPUT_PATH = os.environ.get("GROWW_TICK_FILE", "data/groww/live-ticks.ndjson")
IST = timezone(timedelta(hours=5, minutes=30))

# Map Groww's exchange+segment to our canonical segment string (bridge contract).
# VERIFY the Groww exchange/segment values against the smoke output.
SEGMENT_MAP = {
    ("NSE", "CASH"): "NSE_EQ",
    ("NSE", "FNO"): "NSE_FNO",
    ("BSE", "CASH"): "BSE_EQ",
    ("BSE", "FNO"): "BSE_FNO",
    ("NSE", "INDEX"): "IDX_I",
    ("BSE", "INDEX"): "IDX_I",
}


def ms_to_ist_nanos(ts_millis: int) -> int:
    """Groww millisecond timestamp -> IST epoch nanoseconds.

    VERIFY whether Groww's ms is UTC or IST. This assumes the SDK gives a UTC
    epoch ms (most common) and converts to IST wall-clock nanos to match how the
    Rust side stores `ts`. If the smoke output shows IST already, drop the +IST
    offset. Confirm against one known tick's wall-clock time.
    """
    dt_utc = datetime.fromtimestamp(ts_millis / 1000.0, tz=timezone.utc)
    dt_ist = dt_utc.astimezone(IST)
    # IST wall-clock seconds-since-epoch * 1e9, mirroring the Dhan "store IST
    # directly" rule (data-integrity.md). Keep ms precision.
    ist_epoch_ns = int(dt_ist.replace(tzinfo=timezone.utc).timestamp() * 1_000_000_000)
    return ist_epoch_ns


def tick_to_record(tick: dict) -> dict | None:
    """Map one raw Groww tick object to the bridge's NDJSON record.

    VERIFY every key against the smoke output — these are best-guess names.
    Returns None if the tick is malformed (skipped, not crashed).
    """
    try:
        exchange = str(tick.get("exchange", "NSE"))            # VERIFY
        seg_raw = str(tick.get("segment", "CASH"))             # VERIFY
        segment = SEGMENT_MAP.get((exchange, seg_raw))
        if segment is None:
            return None
        ts_millis = int(tick["tick_timestamp_millis"])          # VERIFY key name
        return {
            "security_id": int(tick["exchange_token"]),         # VERIFY key name
            "segment": segment,
            "ts_ist_nanos": ms_to_ist_nanos(ts_millis),
            "exchange_ts_millis": ts_millis,
            "ltp": float(tick["ltp"]),                          # VERIFY key name
            "cum_volume": int(tick.get("volume", 0)),           # VERIFY key name
        }
    except (KeyError, ValueError, TypeError):
        return None


def main() -> None:
    api_key = os.environ.get("GROWW_API_KEY")
    totp_secret = os.environ.get("GROWW_TOTP_SECRET")
    if not api_key or not totp_secret:
        sys.exit("Set GROWW_API_KEY and GROWW_TOTP_SECRET in the environment.")

    os.makedirs(os.path.dirname(OUTPUT_PATH) or ".", exist_ok=True)
    out = open(OUTPUT_PATH, "a", buffering=1)  # line-buffered append
    print(f"groww sidecar → appending NDJSON to {OUTPUT_PATH}", flush=True)

    def on_data(message) -> None:
        # A message may be one tick or a batch — handle both (VERIFY shape).
        ticks = message if isinstance(message, list) else [message]
        for tick in ticks:
            if not isinstance(tick, dict):
                continue
            rec = tick_to_record(tick)
            if rec is None:
                continue
            out.write(json.dumps(rec) + "\n")
            out.flush()
            os.fsync(out.fileno())  # capture-at-receipt durability

    # Reconnect loop — never give up (lock: not a single received tick missed).
    while True:
        try:
            totp = pyotp.TOTP(totp_secret).now()
            access_token = GrowwAPI.get_access_token(api_key=api_key, totp=totp)  # VERIFY
            groww = GrowwAPI(access_token)
            feed = GrowwFeed(groww)
            feed.subscribe_live_data(  # VERIFY method + args; subscribe your validation set
                [{"exchange": "NSE", "segment": "CASH", "exchange_token": "2885"}],  # VERIFY
                on_data,
            )
            print("subscribed — streaming…", flush=True)
            feed.consume()  # VERIFY blocking consume
        except KeyboardInterrupt:
            print("stopping.", flush=True)
            break
        except Exception as exc:  # noqa: BLE001 - reconnect on any error
            print(f"groww sidecar error: {exc!r} — reconnecting in 5s", flush=True)
            time.sleep(5)


if __name__ == "__main__":
    main()
