#!/usr/bin/env python3
"""Groww validation sidecar (LOCAL-ONLY, operator lock §32) — the PRODUCER.

VERIFIED against the official growwapi-1.5.0 SDK source
(docs/groww-ref/10-live-feed-mapping-verified.md). The live feed is
NATS-over-WebSocket + Protobuf; GrowwFeed handles transport + decode. The
callback receives the topic META; the parsed tick is pulled via get_ltp(),
shaped `{exchange: {segment: {exchange_token: {ltp, tsInMillis, ...}}}}`.

PR-B2 (2026-06-21): the watch set is no longer hardcoded — it is the watch file
Rust writes at boot (crates/core/src/feed/groww/instruments.rs ->
data/groww/groww-watch-<date>.json). This sidecar reads that file and subscribes
the STOCK entries (kind="ltp") via subscribe_ltp. INDEX entries (kind=
"index_value") are NOT subscribed here yet: Groww NSE indices use a NAME token
(e.g. "NIFTY"), which cannot map to the bridge's integer security_id without a
name->SID identity decision (PR-B2i, pending a live Groww index row). They are
counted + logged, never silently dropped.

Each received tick is appended as one NDJSON line to data/groww/live-ticks.ndjson
— the EXACT schema the Rust bridge (crates/app/src/groww_bridge.rs) parses.
Capture-at-receipt: write + flush + fsync the instant the callback fires (durable
floor one hop downstream of the socket; lock §32.3).

Volume is Option A (price-only, operator 2026-06-20): the Groww live LTP feed
carries NO traded volume (only ltp + tsInMillis), so cum_volume is always 0.

Usage (auto-launched by the Rust supervisor; no manual run needed):
    export GROWW_API_KEY=...   GROWW_TOTP_SECRET=...
    python3 groww_sidecar.py
"""
import glob
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
# Directory the Rust watch-list builder writes groww-watch-<date>.json into.
WATCH_DIR = os.environ.get("GROWW_WATCH_DIR", "data/groww")
IST = timezone(timedelta(hours=5, minutes=30))

# Groww (exchange, segment) -> our canonical segment string (bridge contract).
# Verified values: exchange NSE/BSE, segment CASH/FNO.
SEGMENT_MAP = {
    ("NSE", "CASH"): "NSE_EQ",
    ("NSE", "FNO"): "NSE_FNO",
    ("BSE", "CASH"): "BSE_EQ",
    ("BSE", "FNO"): "BSE_FNO",
}

# How long to wait between checks for the Rust-built watch file at startup.
WATCH_POLL_SECS = 5


def ms_to_ist_nanos(ts_millis: int) -> int:
    """Groww `tsInMillis` (UTC epoch ms) -> IST epoch nanoseconds.

    Mirrors the Dhan "store IST wall-clock directly" rule (data-integrity.md):
    convert UTC ms -> IST wall clock, then to nanos. Keeps ms precision.
    """
    dt_utc = datetime.fromtimestamp(ts_millis / 1000.0, tz=timezone.utc)
    dt_ist = dt_utc.astimezone(IST)
    return int(dt_ist.replace(tzinfo=timezone.utc).timestamp() * 1_000_000_000)


def latest_watch_file(watch_dir: str):
    """Return the path of the most recent groww-watch-*.json, or None."""
    matches = sorted(glob.glob(os.path.join(watch_dir, "groww-watch-*.json")))
    return matches[-1] if matches else None


def load_stock_subscriptions(watch_path: str):
    """Read the Rust watch file -> (stock_list, index_count).

    stock_list = [{exchange, segment, exchange_token}] for kind=="ltp" entries
    (numeric exchange_token only — the bridge security_id is an integer).
    Index entries (kind=="index_value") are counted but NOT subscribed (PR-B2i).
    """
    with open(watch_path, "r") as fh:
        doc = json.load(fh)
    stock_list = []
    index_count = 0
    skipped_non_numeric = 0
    for entry in doc.get("entries", []):
        kind = entry.get("kind")
        if kind == "index_value":
            index_count += 1
            continue
        token = str(entry.get("exchange_token", ""))
        # Stocks must have a numeric token (maps to the bridge integer security_id).
        if not token.isdigit():
            skipped_non_numeric += 1
            continue
        stock_list.append(
            {
                "exchange": entry.get("exchange", "NSE"),
                "segment": entry.get("segment", "CASH"),
                "exchange_token": token,
            }
        )
    if skipped_non_numeric:
        print(
            f"groww sidecar: skipped {skipped_non_numeric} non-numeric stock tokens",
            flush=True,
        )
    return stock_list, index_count


def emit_records(out, ltp_tree: dict) -> None:
    """Flatten get_ltp() `{exchange:{segment:{token:{...}}}}` and append NDJSON.

    Verified tick keys: `ltp`, `tsInMillis` (ms). The Groww LTP feed has NO
    traded volume (Option A) -> cum_volume is always 0. Instrument identity comes
    from the tree path (exchange/segment/token), NOT the tick body.
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
                        # Option A: Groww live feed carries no volume -> always 0.
                        "cum_volume": 0,
                    }
                except (KeyError, ValueError, TypeError):
                    continue
                out.write(json.dumps(rec) + "\n")
                out.flush()
                os.fsync(out.fileno())  # capture-at-receipt durability


def wait_for_stock_subscriptions():
    """Block until the Rust watch file exists and yields >=1 stock; return it."""
    while True:
        watch_path = latest_watch_file(WATCH_DIR)
        if watch_path is not None:
            try:
                stocks, index_count = load_stock_subscriptions(watch_path)
            except (OSError, ValueError) as exc:
                print(
                    f"groww sidecar: watch file unreadable ({type(exc).__name__}); retrying",
                    flush=True,
                )
                stocks, index_count = [], 0
            if stocks:
                print(
                    f"groww sidecar: loaded {len(stocks)} stock subscriptions + "
                    f"{index_count} index entries (indices deferred to PR-B2i) "
                    f"from {os.path.basename(watch_path)}",
                    flush=True,
                )
                return stocks
        print(
            f"groww sidecar: waiting for Rust watch file in {WATCH_DIR} …",
            flush=True,
        )
        time.sleep(WATCH_POLL_SECS)


def main() -> None:
    api_key = os.environ.get("GROWW_API_KEY")
    totp_secret = os.environ.get("GROWW_TOTP_SECRET")
    if not api_key or not totp_secret:
        sys.exit("Set GROWW_API_KEY and GROWW_TOTP_SECRET in the environment.")

    os.makedirs(os.path.dirname(OUTPUT_PATH) or ".", exist_ok=True)
    out = open(OUTPUT_PATH, "a", buffering=1)  # line-buffered append
    print(f"groww sidecar → appending NDJSON to {OUTPUT_PATH}", flush=True)

    stock_list = wait_for_stock_subscriptions()

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

            feed.subscribe_ltp(stock_list, on_data_received=on_update)
            print(f"subscribed {len(stock_list)} stocks — streaming…", flush=True)
            feed.consume()  # blocking
        except KeyboardInterrupt:
            print("stopping.", flush=True)
            break
        except Exception as exc:  # noqa: BLE001 - reconnect on any error
            # Log ONLY the exception type name, never repr/str: a Groww SDK HTTP
            # error can embed the response body (and thus the access token /
            # api_key) in its repr, which the Rust supervisor captures from stdout
            # (security-review MEDIUM 2026-06-19). The type name is enough to
            # triage; full detail stays out of the log.
            print(
                f"groww sidecar error: {type(exc).__name__} — reconnecting in 5s",
                flush=True,
            )
            time.sleep(5)


if __name__ == "__main__":
    main()
