#!/usr/bin/env python3
"""Groww validation sidecar (LOCAL-ONLY, operator lock §32) — the PRODUCER.

VERIFIED against the official growwapi-1.5.0 SDK source
(docs/groww-ref/10-live-feed-mapping-verified.md). The live feed is
NATS-over-WebSocket + Protobuf; GrowwFeed handles transport + decode. The
callback receives the topic META; the parsed ticks are pulled via get_ltp()
(stocks) / get_index_value() (indices), each shaped
`{exchange: {segment: {exchange_token: {...}}}}`.

PR-B2i (2026-06-21): the watch set is the watch file Rust writes at boot
(crates/core/src/feed/groww/instruments.rs -> data/groww/groww-watch-<date>.json).
This sidecar reads that file and subscribes BOTH:
  - STOCK entries (kind="ltp")        via subscribe_ltp        (numeric token).
  - INDEX entries (kind="index_value") via subscribe_index_value (NSE name token
    e.g. "NIFTY"; BSE SENSEX numeric "1").
The integer `security_id` STORED for each instrument comes from the Rust watch
entry (single source of truth): for stocks it is the numeric exchange_token; for
indices (whose token may be a name) it is the Groww-native stable id Rust
derived (operator decision 2026-06-21). The sidecar NEVER re-derives it — it
looks it up by (exchange, segment, exchange_token) so the index name/token never
has to map to an integer here.

Each received tick is appended as one NDJSON line to data/groww/live-ticks.ndjson
— the EXACT schema the Rust bridge (crates/app/src/groww_bridge.rs) parses.
Index ticks are emitted with segment="IDX_I" and ltp=<index value>.
Capture-at-receipt: write + flush + fsync the instant the callback fires (durable
floor one hop downstream of the socket; lock §32.3).

Volume is Option A (price-only, operator 2026-06-20): the Groww live feed carries
NO traded volume (only ltp/value + tsInMillis), so cum_volume is always 0.

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
    # Log only the exception TYPE (consistent with the reconnect/watch handlers):
    # an import error's str can embed filesystem/site-packages paths.
    sys.exit(
        f"growwapi import failed ({type(exc).__name__}). "
        "`pip install -r requirements.txt` first."
    )

# The path the Rust bridge tails (GROWW_TICK_FILE_DEFAULT in groww_bridge.rs).
OUTPUT_PATH = os.environ.get("GROWW_TICK_FILE", "data/groww/live-ticks.ndjson")
# Directory the Rust watch-list builder writes groww-watch-<date>.json into.
WATCH_DIR = os.environ.get("GROWW_WATCH_DIR", "data/groww")
IST = timezone(timedelta(hours=5, minutes=30))

# Groww (exchange, segment) -> our canonical segment string (bridge contract),
# used for STOCK (subscribe_ltp) ticks. Verified values: NSE/BSE x CASH/FNO.
SEGMENT_MAP = {
    ("NSE", "CASH"): "NSE_EQ",
    ("NSE", "FNO"): "NSE_FNO",
    ("BSE", "CASH"): "BSE_EQ",
    ("BSE", "FNO"): "BSE_FNO",
}
# All index ticks (whatever exchange/segment Groww uses) store as IDX_I — matches
# the Dhan index convention + the bridge's segment_from_str("IDX_I").
CANONICAL_INDEX_SEGMENT = "IDX_I"

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


def load_subscriptions(watch_path: str):
    """Read the Rust watch file -> (stock_list, index_list, sid_map).

    stock_list / index_list = [{exchange, segment, exchange_token}] for the SDK
    subscribe calls. sid_map = {(exchange, segment, exchange_token): security_id}
    so emit can stamp the Rust-assigned integer security_id (the index name/token
    never has to be parsed to an int here).
    """
    with open(watch_path, "r") as fh:
        doc = json.load(fh)
    stock_list = []
    index_list = []
    sid_map = {}
    skipped_non_numeric = 0
    for entry in doc.get("entries", []):
        exchange = str(entry.get("exchange", "NSE"))
        segment = str(entry.get("segment", "CASH"))
        token = str(entry.get("exchange_token", ""))
        if not token:
            continue
        security_id = int(entry.get("security_id", 0))
        sub = {"exchange": exchange, "segment": segment, "exchange_token": token}
        kind = entry.get("kind")
        if kind == "index_value":
            index_list.append(sub)
            sid_map[(exchange, segment, token)] = security_id
        else:
            # Stocks must have a numeric token (also their security_id).
            if not token.isdigit():
                skipped_non_numeric += 1
                continue
            stock_list.append(sub)
            sid_map[(exchange, segment, token)] = security_id or int(token)
    if skipped_non_numeric:
        print(
            f"groww sidecar: skipped {skipped_non_numeric} non-numeric stock tokens",
            flush=True,
        )
    return stock_list, index_list, sid_map


def _write_record(out, security_id: int, segment: str, ts_millis: int, price) -> None:
    """Append one NDJSON tick (Rust bridge schema) + capture-at-receipt fsync."""
    rec = {
        "security_id": int(security_id),
        "segment": segment,
        "ts_ist_nanos": ms_to_ist_nanos(ts_millis) if ts_millis else 0,
        "exchange_ts_millis": ts_millis,
        "ltp": float(price),
        # Option A: Groww live feed carries no volume -> always 0.
        "cum_volume": 0,
    }
    out.write(json.dumps(rec) + "\n")
    out.flush()
    os.fsync(out.fileno())  # capture-at-receipt durability


def emit_ltp_records(out, ltp_tree: dict, sid_map: dict) -> None:
    """Flatten get_ltp() `{exchange:{segment:{token:{ltp,tsInMillis}}}}` -> NDJSON.

    Stock identity comes from the tree path; security_id from sid_map (falls back
    to the numeric token, which IS the stock security_id).
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
                token = str(token)
                security_id = sid_map.get((str(exchange), str(segment), token))
                if security_id is None:
                    security_id = int(token) if token.isdigit() else 0
                if security_id <= 0:
                    continue
                try:
                    ts_millis = int(tick.get("tsInMillis", 0))
                    _write_record(out, security_id, canonical, ts_millis, tick["ltp"])
                except (KeyError, ValueError, TypeError):
                    continue


def emit_index_records(out, index_tree: dict, sid_map: dict) -> None:
    """Flatten get_index_value() `{exchange:{segment:{token:{value,tsInMillis}}}}`.

    Index value field is `value` (not `ltp`); stored as ltp with segment IDX_I.
    security_id MUST come from sid_map (the token may be a NAME) — no fallback.
    """
    if not isinstance(index_tree, dict):
        return
    for exchange, segs in index_tree.items():
        if not isinstance(segs, dict):
            continue
        for segment, tokens in segs.items():
            if not isinstance(tokens, dict):
                continue
            for token, tick in tokens.items():
                if not isinstance(tick, dict) or "value" not in tick:
                    continue
                security_id = sid_map.get((str(exchange), str(segment), str(token)))
                if security_id is None or security_id <= 0:
                    continue
                try:
                    ts_millis = int(tick.get("tsInMillis", 0))
                    _write_record(
                        out, security_id, CANONICAL_INDEX_SEGMENT, ts_millis, tick["value"]
                    )
                except (KeyError, ValueError, TypeError):
                    continue


def wait_for_subscriptions():
    """Block until the Rust watch file exists with >=1 entry; return its lists."""
    while True:
        watch_path = latest_watch_file(WATCH_DIR)
        if watch_path is not None:
            try:
                stocks, indices, sid_map = load_subscriptions(watch_path)
            except (OSError, ValueError) as exc:
                print(
                    f"groww sidecar: watch file unreadable ({type(exc).__name__}); retrying",
                    flush=True,
                )
                stocks, indices, sid_map = [], [], {}
            if stocks or indices:
                print(
                    f"groww sidecar: loaded {len(stocks)} stock + {len(indices)} index "
                    f"subscriptions from {os.path.basename(watch_path)}",
                    flush=True,
                )
                return stocks, indices, sid_map
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

    stock_list, index_list, sid_map = wait_for_subscriptions()

    # Reconnect loop — never give up (lock: not a single received tick missed).
    while True:
        try:
            totp = pyotp.TOTP(totp_secret).now()
            access_token = GrowwAPI.get_access_token(api_key=api_key, totp=totp)
            groww = GrowwAPI(access_token)
            feed = GrowwFeed(groww)

            if stock_list:
                def on_ltp(_meta) -> None:
                    emit_ltp_records(out, feed.get_ltp(), sid_map)

                feed.subscribe_ltp(stock_list, on_data_received=on_ltp)
            if index_list:
                def on_index(_meta) -> None:
                    emit_index_records(out, feed.get_index_value(), sid_map)

                feed.subscribe_index_value(index_list, on_data_received=on_index)

            print(
                f"subscribed {len(stock_list)} stocks + {len(index_list)} indices "
                f"— streaming…",
                flush=True,
            )
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
