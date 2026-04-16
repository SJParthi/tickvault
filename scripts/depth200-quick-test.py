#!/usr/bin/env python3
"""
QUICK 200-Level Depth Test — Run During Google Meet with Dhan
==============================================================

BEFORE THE CALL:
    pip install websockets requests

USAGE (pick one):
    # Easiest — set env vars and run:
    export DHAN_ACCESS_TOKEN="<YOUR_TOKEN>"
    export DHAN_CLIENT_ID="<YOUR_CLIENT_ID>"
    python3 scripts/depth200-quick-test.py

    # Or pass inline:
    DHAN_ACCESS_TOKEN="<YOUR_TOKEN>" DHAN_CLIENT_ID="<YOUR_CLIENT_ID>" python3 scripts/depth200-quick-test.py

    # Override SecurityId manually (if Dhan gives you one during the call):
    DHAN_SECURITY_ID="63424" python3 scripts/depth200-quick-test.py

WHAT IT DOES:
    1. Fetches NIFTY spot LTP from REST API (proves token works)
    2. Fetches ATM option SecurityId from option chain (proves data APIs work)
    3. Connects to 200-level depth WebSocket
    4. Subscribes to the ATM contract
    5. Waits 60s for depth frames
    6. Prints clear PASS/FAIL with full detail for Dhan to see

OUTPUT: Share your terminal screen on the Meet. That's it.
"""

import asyncio
import json
import os
import struct
import sys
import time
from datetime import datetime

# ---------------------------------------------------------------------------
# Dependency check
# ---------------------------------------------------------------------------
try:
    import websockets
except ImportError:
    print("ERROR: Run this first:  pip install websockets requests")
    sys.exit(1)

try:
    import requests
except ImportError:
    print("ERROR: Run this first:  pip install websockets requests")
    sys.exit(1)


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
CLIENT_ID = os.environ.get("DHAN_CLIENT_ID", "1106656882")
ACCESS_TOKEN = os.environ.get("DHAN_ACCESS_TOKEN", "")
OVERRIDE_SID = os.environ.get("DHAN_SECURITY_ID", "")

NIFTY_SPOT_SID = 13  # IDX_I segment

# Both URL paths to test
URLS = [
    ("wss://full-depth-api.dhan.co/twohundreddepth", "/twohundreddepth"),
    ("wss://full-depth-api.dhan.co/",                "/ (root path)"),
]


def banner(text):
    w = 72
    print(f"\n{'=' * w}")
    print(f"  {text}")
    print(f"{'=' * w}")


def timestamp():
    return datetime.now().strftime("%Y-%m-%d %H:%M:%S IST")


# ---------------------------------------------------------------------------
# Step 1: Load credentials
# ---------------------------------------------------------------------------
def load_credentials():
    global ACCESS_TOKEN, CLIENT_ID

    if ACCESS_TOKEN:
        print(f"[OK] Token from env var (client_id={CLIENT_ID})")
        return

    # Try token cache from running app
    cache_path = "data/cache/tv-token-cache"
    if os.path.exists(cache_path):
        try:
            with open(cache_path) as f:
                cache = json.load(f)
            ACCESS_TOKEN = cache.get("access_token", "")
            CLIENT_ID = cache.get("client_id", CLIENT_ID)
            if ACCESS_TOKEN:
                print(f"[OK] Token from cache file ({cache_path})")
                return
        except Exception as e:
            print(f"[WARN] Cache read failed: {e}")

    print("ERROR: No access token found.")
    print("")
    print("Set it like this:")
    print('  export DHAN_ACCESS_TOKEN="eyJ..."')
    print('  export DHAN_CLIENT_ID="1106656882"')
    print("  python3 scripts/depth200-quick-test.py")
    sys.exit(1)


# ---------------------------------------------------------------------------
# Step 2: Verify token with REST API + get ATM SecurityId
# ---------------------------------------------------------------------------
def get_atm_security_id():
    """Returns (security_id, label, spot_ltp) or exits on failure."""
    headers = {
        "access-token": ACCESS_TOKEN,
        "client-id": CLIENT_ID,
        "Content-Type": "application/json",
    }

    # 2a: Get NIFTY spot LTP
    print("\n[1/3] Fetching NIFTY spot LTP (proves token + REST works)...")
    try:
        r = requests.post(
            "https://api.dhan.co/v2/marketfeed/ltp",
            headers=headers,
            json={"IDX_I": [NIFTY_SPOT_SID]},
            timeout=10,
        )
    except Exception as e:
        print(f"  FAILED: {e}")
        sys.exit(1)

    if r.status_code != 200:
        print(f"  FAILED: HTTP {r.status_code} — {r.text[:300]}")
        print("  Your token may be expired. Generate a new one.")
        sys.exit(1)

    ltp_data = r.json()
    spot = ltp_data.get("data", {}).get("IDX_I", {}).get(str(NIFTY_SPOT_SID), {}).get("last_price", 0)
    if spot <= 0:
        print(f"  FAILED: NIFTY spot LTP is 0. Response: {ltp_data}")
        sys.exit(1)
    print(f"  NIFTY Spot LTP: {spot}")

    # 2b: Get nearest expiry
    print("[2/3] Fetching nearest expiry...")
    try:
        r = requests.post(
            "https://api.dhan.co/v2/optionchain/expirylist",
            headers=headers,
            json={"UnderlyingScrip": NIFTY_SPOT_SID, "UnderlyingSeg": "IDX_I"},
            timeout=10,
        )
    except Exception as e:
        print(f"  FAILED: {e}")
        return None, None, spot

    if r.status_code != 200:
        print(f"  FAILED: HTTP {r.status_code}")
        return None, None, spot

    expiries = r.json().get("data", [])
    if not expiries:
        print("  FAILED: No expiries returned")
        return None, None, spot

    nearest = expiries[0]
    print(f"  Nearest expiry: {nearest}")

    # 2c: Get option chain + find ATM strike
    print("[3/3] Fetching option chain for ATM strike...")
    time.sleep(1)  # respect 1-req-per-3s rate limit
    try:
        r = requests.post(
            "https://api.dhan.co/v2/optionchain",
            headers=headers,
            json={
                "UnderlyingScrip": NIFTY_SPOT_SID,
                "UnderlyingSeg": "IDX_I",
                "Expiry": nearest,
            },
            timeout=10,
        )
    except Exception as e:
        print(f"  FAILED: {e}")
        return None, None, spot

    if r.status_code != 200:
        print(f"  FAILED: HTTP {r.status_code}")
        return None, None, spot

    oc = r.json().get("data", {}).get("oc", {})
    if not oc:
        print("  FAILED: Empty option chain")
        return None, None, spot

    # Find ATM
    best_strike = None
    best_diff = float("inf")
    best_sid = None

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
        print("  FAILED: Could not find ATM CE")
        return None, None, spot

    label = f"NIFTY-{nearest}-{int(best_strike)}-CE"
    print(f"  ATM strike: {best_strike} (diff from spot: {best_diff:.2f})")
    print(f"  ATM CE SecurityId: {best_sid}")
    print(f"  Contract: {label}")

    return str(best_sid), label, spot


# ---------------------------------------------------------------------------
# Step 3: Test 200-level depth WebSocket
# ---------------------------------------------------------------------------
def parse_depth_frame(data):
    """Parse a binary depth frame. Returns dict with parsed info."""
    result = {"raw_bytes": len(data), "ok": False}
    if len(data) < 12:
        result["error"] = f"too short ({len(data)} bytes, need 12)"
        return result

    # 12-byte header: msg_len(i16) + response_code(u8) + segment(u8) + sid(i32) + rows(u32)
    msg_len, resp_code, segment, sid, rows = struct.unpack('<hBBiI', data[:12])
    result["msg_len"] = msg_len
    result["response_code"] = resp_code
    result["segment"] = segment
    result["security_id"] = sid
    result["rows_or_seq"] = rows
    result["side"] = {41: "BID", 51: "ASK", 50: "DISCONNECT"}.get(resp_code, f"UNKNOWN({resp_code})")

    if resp_code == 50:
        result["type"] = "DISCONNECT"
        if len(data) >= 14:
            result["reason_code"] = struct.unpack('<H', data[12:14])[0]
        return result

    if resp_code not in (41, 51):
        result["error"] = f"unknown response code {resp_code}"
        return result

    # Parse depth levels
    row_count = rows
    if row_count > 200:
        result["error"] = f"row_count {row_count} > 200"
        return result

    expected_size = 12 + (row_count * 16)
    if len(data) < expected_size:
        result["error"] = f"packet {len(data)}B < expected {expected_size}B for {row_count} rows"
        return result

    levels = []
    for i in range(min(row_count, 200)):
        offset = 12 + (i * 16)
        price = struct.unpack('<d', data[offset:offset+8])[0]       # f64
        qty = struct.unpack('<I', data[offset+8:offset+12])[0]      # u32
        orders = struct.unpack('<I', data[offset+12:offset+16])[0]  # u32
        levels.append({"price": price, "qty": qty, "orders": orders})

    result["levels"] = levels
    result["row_count"] = row_count
    result["ok"] = True
    return result


async def test_one_url(url_base, url_label, security_id, sid_label):
    """Test one URL path. Returns (success, frames_received, detail_lines)."""
    url = f"{url_base}?token={ACCESS_TOKEN}&clientId={CLIENT_ID}&authType=2"
    safe_url = url.replace(ACCESS_TOKEN, ACCESS_TOKEN[:8] + "...REDACTED")

    lines = []
    def log(msg):
        lines.append(msg)
        print(msg)

    log(f"\n--- Test: {url_label} ---")
    log(f"URL:         {safe_url}")
    log(f"SecurityId:  {security_id} ({sid_label})")
    log(f"Segment:     NSE_FNO")

    sub_msg = json.dumps({
        "RequestCode": 23,
        "ExchangeSegment": "NSE_FNO",
        "SecurityId": security_id,
    })
    log(f"Subscribe:   {sub_msg}")

    try:
        log("[...] Connecting...")
        ws = await asyncio.wait_for(websockets.connect(url), timeout=10.0)
        log("[OK]  Connected!")
    except asyncio.TimeoutError:
        log("[FAIL] Connection timed out (10s)")
        return False, 0, lines
    except Exception as e:
        if "reset" in str(e).lower():
            log("[FAIL] TCP RESET — server actively rejected connection")
        else:
            log(f"[FAIL] {type(e).__name__}: {e}")
        return False, 0, lines

    try:
        await ws.send(sub_msg)
        log("[OK]  Subscription sent")
    except Exception as e:
        log(f"[FAIL] Send failed: {e}")
        return False, 0, lines

    frames = 0
    bid_frames = 0
    ask_frames = 0
    start = time.time()
    timeout_secs = 60

    log(f"[...] Waiting for depth frames (up to {timeout_secs}s)...")

    while time.time() - start < timeout_secs:
        try:
            data = await asyncio.wait_for(ws.recv(), timeout=5.0)

            if isinstance(data, bytes):
                frames += 1
                parsed = parse_depth_frame(data)

                if parsed.get("type") == "DISCONNECT":
                    reason = parsed.get("reason_code", "?")
                    log(f"[DISCONNECT] Server disconnected us. Reason code: {reason}")
                    break

                side = parsed.get("side", "?")
                rows = parsed.get("row_count", 0)
                sid = parsed.get("security_id", "?")

                if side == "BID":
                    bid_frames += 1
                elif side == "ASK":
                    ask_frames += 1

                if parsed["ok"] and parsed.get("levels"):
                    top = parsed["levels"][0]
                    log(f"[FRAME {frames:3d}] {side:3s} | rows={rows:3d} | "
                        f"top: {top['price']:>10.2f} x {top['qty']:>6d} ({top['orders']} orders) | "
                        f"sid={sid}")
                elif parsed.get("error"):
                    log(f"[FRAME {frames:3d}] {side:3s} | ERROR: {parsed['error']}")
                else:
                    log(f"[FRAME {frames:3d}] {side:3s} | rows={rows} | {len(data)}B")

                # After enough frames, declare success
                if frames >= 10:
                    log(f"\n[SUCCESS] Got {frames} frames ({bid_frames} bid, {ask_frames} ask)")
                    try:
                        await ws.close()
                    except Exception:
                        pass
                    return True, frames, lines

            elif isinstance(data, str):
                log(f"[TEXT] {data[:200]}")

        except asyncio.TimeoutError:
            elapsed = time.time() - start
            log(f"[...] No frame in 5s (elapsed: {elapsed:.0f}s, frames so far: {frames})")

    elapsed = time.time() - start
    try:
        await ws.close()
    except Exception:
        pass

    if frames > 0:
        log(f"\n[PARTIAL] {frames} frames in {elapsed:.0f}s ({bid_frames} bid, {ask_frames} ask)")
        return True, frames, lines
    else:
        log(f"\n[FAIL] Zero frames received in {elapsed:.0f}s")
        return False, 0, lines


# ---------------------------------------------------------------------------
# Step 4: Also test 20-level depth (to prove it works)
# ---------------------------------------------------------------------------
async def test_20_depth_quick(security_id, sid_label):
    """Quick test of 20-level depth to prove THAT works."""
    url = f"wss://depth-api-feed.dhan.co/twentydepth?token={ACCESS_TOKEN}&clientId={CLIENT_ID}&authType=2"

    print(f"\n--- Control test: 20-level depth (should WORK) ---")
    print(f"SecurityId: {security_id} ({sid_label})")

    sub_msg = json.dumps({
        "RequestCode": 23,
        "InstrumentCount": 1,
        "InstrumentList": [{
            "ExchangeSegment": "NSE_FNO",
            "SecurityId": security_id,
        }],
    })

    try:
        ws = await asyncio.wait_for(websockets.connect(url), timeout=10.0)
        await ws.send(sub_msg)

        frames = 0
        start = time.time()
        while time.time() - start < 15:
            try:
                data = await asyncio.wait_for(ws.recv(), timeout=5.0)
                if isinstance(data, bytes) and len(data) >= 12:
                    frames += 1
                    header = struct.unpack('<hBBiI', data[:12])
                    side = {41: "BID", 51: "ASK", 50: "DISC"}.get(header[1], "?")
                    print(f"  [20-lvl FRAME {frames}] {side} | sid={header[3]} | {len(data)}B")
                    if frames >= 4:
                        break
            except asyncio.TimeoutError:
                pass

        await ws.close()
        if frames > 0:
            print(f"  [OK] 20-level depth WORKS ({frames} frames)")
            return True
        else:
            print(f"  [FAIL] 20-level depth also got zero frames")
            return False

    except Exception as e:
        print(f"  [FAIL] 20-level depth error: {e}")
        return False


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
async def main():
    banner(f"Dhan 200-Level Depth Test — {timestamp()}")
    print(f"Client ID:  {CLIENT_ID}")
    print(f"Script:     depth200-quick-test.py")
    print(f"Purpose:    Prove 200-level depth is not working on this account")

    load_credentials()
    print(f"Token:      {ACCESS_TOKEN[:12]}...{ACCESS_TOKEN[-8:]}")

    # Get ATM SecurityId
    sid, label, spot = get_atm_security_id()

    if OVERRIDE_SID:
        sid = OVERRIDE_SID
        label = f"manual-override-{OVERRIDE_SID}"
        print(f"\n[OVERRIDE] Using SecurityId {sid} from DHAN_SECURITY_ID env var")

    if not sid:
        sid = "63424"
        label = "fallback-63424 (ATM fetch failed)"
        print(f"\n[WARN] ATM fetch failed, using fallback SID {sid}")

    banner("REST API Status (all working)")
    print(f"  NIFTY Spot LTP:    {spot}")
    print(f"  ATM SecurityId:    {sid}")
    print(f"  Contract:          {label}")
    print(f"  Token valid:       YES (REST APIs responded)")

    # Test 20-level first (control — should work)
    banner("Control Test: 20-Level Depth")
    twenty_ok = await test_20_depth_quick(sid, label)

    await asyncio.sleep(1)

    # Test 200-level on both URLs
    banner("200-Level Depth Tests")
    results = {}
    for url_base, url_label in URLS:
        ok, frame_count, detail = await test_one_url(url_base, url_label, sid, label)
        results[url_label] = (ok, frame_count)
        await asyncio.sleep(2)

    # Final summary
    banner("FINAL SUMMARY")
    print(f"Timestamp:       {timestamp()}")
    print(f"Client ID:       {CLIENT_ID}")
    print(f"NIFTY Spot:      {spot}")
    print(f"SecurityId:      {sid} ({label})")
    print(f"Token valid:     YES")
    print()
    print(f"  20-level depth:           {'WORKS' if twenty_ok else 'FAILED'}")
    for url_label, (ok, count) in results.items():
        status = f"WORKS ({count} frames)" if ok else "FAILED (0 frames)"
        print(f"  200-level {url_label:20s}: {status}")

    all_200_failed = all(not ok for ok, _ in results.values())
    if all_200_failed and twenty_ok:
        print()
        print("DIAGNOSIS: 20-level works, 200-level does NOT.")
        print("           Same token, same SecurityId, same account.")
        print("           200-level depth is NOT ENABLED on this account.")
        print()
        print(f"REQUEST: Please enable 200-level Full Market Depth")
        print(f"         for Client ID {CLIENT_ID}.")
    elif all_200_failed and not twenty_ok:
        print()
        print("DIAGNOSIS: BOTH 20-level and 200-level depth failed.")
        print("           May be outside market hours or general depth issue.")
    elif not all_200_failed:
        print()
        print("200-LEVEL DEPTH IS WORKING!")
        for url_label, (ok, count) in results.items():
            if ok:
                print(f"  Working URL: {url_label}")

    print()
    print("=" * 72)
    print("  Share this terminal output with Dhan on the Google Meet.")
    print("=" * 72)


if __name__ == "__main__":
    asyncio.run(main())
