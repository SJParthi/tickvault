#!/usr/bin/env python3
"""
200-Level Depth LIVE VIEWER — Pretty-prints the order book in real-time
========================================================================

Use this DURING the Google Meet when Dhan enables 200-level on your account.
It connects, subscribes, and continuously prints the order book with
bid/ask levels in a nice table format.

USAGE:
    export DHAN_ACCESS_TOKEN="<YOUR_TOKEN>"
    export DHAN_CLIENT_ID="<YOUR_CLIENT_ID>"
    python3 scripts/depth200-live-viewer.py

    # With specific SecurityId:
    DHAN_SECURITY_ID="63424" python3 scripts/depth200-live-viewer.py

    # With specific URL path:
    DHAN_URL_PATH="/twohundreddepth" python3 scripts/depth200-live-viewer.py
    DHAN_URL_PATH="/" python3 scripts/depth200-live-viewer.py

Press Ctrl+C to stop.
"""

import asyncio
import json
import os
import struct
import sys
import time
from datetime import datetime

try:
    import websockets
except ImportError:
    print("ERROR: pip install websockets requests")
    sys.exit(1)

try:
    import requests
except ImportError:
    print("ERROR: pip install websockets requests")
    sys.exit(1)


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
CLIENT_ID = os.environ.get("DHAN_CLIENT_ID", "1106656882")
ACCESS_TOKEN = os.environ.get("DHAN_ACCESS_TOKEN", "")
OVERRIDE_SID = os.environ.get("DHAN_SECURITY_ID", "")
URL_PATH = os.environ.get("DHAN_URL_PATH", "/twohundreddepth")
NIFTY_SPOT_SID = 13


def load_token():
    global ACCESS_TOKEN, CLIENT_ID
    if ACCESS_TOKEN:
        return
    cache_path = "data/cache/tv-token-cache"
    if os.path.exists(cache_path):
        try:
            with open(cache_path) as f:
                cache = json.load(f)
            ACCESS_TOKEN = cache.get("access_token", "")
            CLIENT_ID = cache.get("client_id", CLIENT_ID)
            if ACCESS_TOKEN:
                return
        except Exception:
            pass
    print("ERROR: Set DHAN_ACCESS_TOKEN and DHAN_CLIENT_ID env vars.")
    sys.exit(1)


def fetch_atm_sid():
    """Get ATM NIFTY CE SecurityId from option chain."""
    headers = {
        "access-token": ACCESS_TOKEN,
        "client-id": CLIENT_ID,
        "Content-Type": "application/json",
    }

    # Get spot
    r = requests.post(
        "https://api.dhan.co/v2/marketfeed/ltp",
        headers=headers,
        json={"IDX_I": [NIFTY_SPOT_SID]},
        timeout=10,
    )
    if r.status_code != 200:
        return None, None, None
    spot = r.json().get("data", {}).get("IDX_I", {}).get(str(NIFTY_SPOT_SID), {}).get("last_price", 0)
    if spot <= 0:
        return None, None, None

    # Get nearest expiry
    r = requests.post(
        "https://api.dhan.co/v2/optionchain/expirylist",
        headers=headers,
        json={"UnderlyingScrip": NIFTY_SPOT_SID, "UnderlyingSeg": "IDX_I"},
        timeout=10,
    )
    if r.status_code != 200:
        return None, None, spot
    expiries = r.json().get("data", [])
    if not expiries:
        return None, None, spot
    nearest = expiries[0]

    time.sleep(1)

    # Get chain
    r = requests.post(
        "https://api.dhan.co/v2/optionchain",
        headers=headers,
        json={"UnderlyingScrip": NIFTY_SPOT_SID, "UnderlyingSeg": "IDX_I", "Expiry": nearest},
        timeout=10,
    )
    if r.status_code != 200:
        return None, None, spot
    oc = r.json().get("data", {}).get("oc", {})
    if not oc:
        return None, None, spot

    best_strike, best_diff, best_sid = None, float("inf"), None
    for strike_str, data in oc.items():
        try:
            strike = float(strike_str)
        except ValueError:
            continue
        diff = abs(strike - spot)
        ce = data.get("ce")
        if ce and diff < best_diff:
            best_diff = diff
            best_strike = strike
            best_sid = ce.get("security_id")

    if not best_sid:
        return None, None, spot
    label = f"NIFTY-{nearest}-{int(best_strike)}-CE"
    return str(best_sid), label, spot


# ---------------------------------------------------------------------------
# Order book state
# ---------------------------------------------------------------------------
class OrderBook:
    def __init__(self):
        self.bid_levels = []  # [(price, qty, orders), ...]
        self.ask_levels = []
        self.bid_count = 0
        self.ask_count = 0
        self.last_bid_time = None
        self.last_ask_time = None
        self.total_frames = 0

    def update(self, side, levels, row_count):
        self.total_frames += 1
        now = datetime.now().strftime("%H:%M:%S.%f")[:-3]
        parsed = [(l["price"], l["qty"], l["orders"]) for l in levels if l["price"] > 0]
        if side == "BID":
            self.bid_levels = parsed
            self.bid_count = row_count
            self.last_bid_time = now
        else:
            self.ask_levels = parsed
            self.ask_count = row_count
            self.last_ask_time = now

    def display(self, security_id, label):
        """Print a nice order book view."""
        # Clear screen
        print("\033[2J\033[H", end="")

        now = datetime.now().strftime("%Y-%m-%d %H:%M:%S IST")
        print(f"200-LEVEL DEPTH LIVE VIEWER | {now}")
        print(f"SecurityId: {security_id} ({label})")
        print(f"Frames: {self.total_frames} | Bid updated: {self.last_bid_time or 'waiting'} | Ask updated: {self.last_ask_time or 'waiting'}")
        print()

        # Show top 30 levels of each side
        show = 30
        bid_show = self.bid_levels[:show]
        ask_show = self.ask_levels[:show]

        # Header
        print(f"{'':>4} {'BID (Buy)':>40}  |  {'ASK (Sell)':<40}")
        print(f"{'#':>4} {'Orders':>8} {'Qty':>10} {'Price':>12}  |  {'Price':<12} {'Qty':<10} {'Orders':<8}")
        print(f"{'─'*4} {'─'*8} {'─'*10} {'─'*12}──┼──{'─'*12} {'─'*10} {'─'*8}")

        max_rows = max(len(bid_show), len(ask_show))
        for i in range(max_rows):
            # Bid side
            if i < len(bid_show):
                bp, bq, bo = bid_show[i]
                bid_str = f"{bo:>8d} {bq:>10d} {bp:>12.2f}"
            else:
                bid_str = f"{'':>8} {'':>10} {'':>12}"

            # Ask side
            if i < len(ask_show):
                ap, aq, ao = ask_show[i]
                ask_str = f"{ap:<12.2f} {aq:<10d} {ao:<8d}"
            else:
                ask_str = f"{'':>12} {'':>10} {'':>8}"

            print(f"{i+1:>4} {bid_str}  |  {ask_str}")

        # Summary
        print()
        total_bid_qty = sum(q for _, q, _ in self.bid_levels)
        total_ask_qty = sum(q for _, q, _ in self.ask_levels)
        print(f"Total bid levels: {len(self.bid_levels):>4} ({self.bid_count} reported) | Total bid qty: {total_bid_qty:>12,}")
        print(f"Total ask levels: {len(self.ask_levels):>4} ({self.ask_count} reported) | Total ask qty: {total_ask_qty:>12,}")

        if self.bid_levels and self.ask_levels:
            spread = self.ask_levels[0][0] - self.bid_levels[0][0]
            print(f"Spread: {spread:.2f}")

        print(f"\nPress Ctrl+C to stop.")


# ---------------------------------------------------------------------------
# WebSocket loop
# ---------------------------------------------------------------------------
async def run_viewer(security_id, label):
    url_base = f"wss://full-depth-api.dhan.co{URL_PATH}"
    url = f"{url_base}?token={ACCESS_TOKEN}&clientId={CLIENT_ID}&authType=2"

    safe_url = url.replace(ACCESS_TOKEN, ACCESS_TOKEN[:8] + "...REDACTED")
    print(f"Connecting to: {safe_url}")
    print(f"SecurityId: {security_id} ({label})")
    print(f"URL path: {URL_PATH}")
    print()

    sub_msg = json.dumps({
        "RequestCode": 23,
        "ExchangeSegment": "NSE_FNO",
        "SecurityId": security_id,
    })

    book = OrderBook()
    reconnect_count = 0
    max_reconnects = 10

    while reconnect_count < max_reconnects:
        try:
            ws = await asyncio.wait_for(websockets.connect(url), timeout=10.0)
            print(f"[OK] Connected! Sending subscription...")
            await ws.send(sub_msg)
            print(f"[OK] Subscribed. Waiting for depth data...\n")

            while True:
                try:
                    data = await asyncio.wait_for(ws.recv(), timeout=45.0)

                    if isinstance(data, bytes) and len(data) >= 12:
                        msg_len, resp_code, seg, sid, rows = struct.unpack('<hBBiI', data[:12])

                        if resp_code == 50:
                            reason = struct.unpack('<H', data[12:14])[0] if len(data) >= 14 else "?"
                            print(f"\n[DISCONNECT] Reason: {reason}")
                            break

                        side = {41: "BID", 51: "ASK"}.get(resp_code)
                        if not side:
                            continue

                        row_count = min(rows, 200)
                        levels = []
                        for i in range(row_count):
                            offset = 12 + (i * 16)
                            if offset + 16 > len(data):
                                break
                            price = struct.unpack('<d', data[offset:offset+8])[0]
                            qty = struct.unpack('<I', data[offset+8:offset+12])[0]
                            orders = struct.unpack('<I', data[offset+12:offset+16])[0]
                            levels.append({"price": price, "qty": qty, "orders": orders})

                        book.update(side, levels, row_count)

                        # Refresh display every frame (both sides arrive quickly)
                        if book.total_frames % 2 == 0:
                            book.display(security_id, label)

                    elif isinstance(data, str):
                        print(f"[TEXT] {data[:200]}")

                except asyncio.TimeoutError:
                    print(f"[WARN] No data for 45s — connection may be dead")
                    break

        except asyncio.TimeoutError:
            print(f"[ERROR] Connection timed out")
        except KeyboardInterrupt:
            print(f"\n\nStopped by user. Total frames: {book.total_frames}")
            return
        except Exception as e:
            if "reset" in str(e).lower():
                print(f"[ERROR] TCP RESET — server rejected connection")
            else:
                print(f"[ERROR] {type(e).__name__}: {e}")

        reconnect_count += 1
        if reconnect_count < max_reconnects:
            wait = min(reconnect_count * 2, 10)
            print(f"[...] Reconnecting in {wait}s (attempt {reconnect_count}/{max_reconnects})...")
            await asyncio.sleep(wait)

    print(f"[FAIL] Max reconnects reached. 200-level depth is not working.")


async def main():
    load_token()

    sid = OVERRIDE_SID
    label = f"manual-{sid}" if sid else ""

    if not sid:
        print("Fetching ATM SecurityId from option chain...")
        sid, label, spot = fetch_atm_sid()
        if sid:
            print(f"ATM: {label} (SID={sid}, spot={spot})")
        else:
            sid = "63424"
            label = "fallback-63424"
            print(f"ATM fetch failed, using fallback: {sid}")

    print()
    await run_viewer(sid, label)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("\nStopped.")
