#!/usr/bin/env python3
"""
Dhan 200-Level Full Market Depth — Diagnostic Test Script
==========================================================

Tests BOTH URL paths (root `/` and `/twohundreddepth`) to determine which
one actually works on your account. Logs the exact WebSocket URL, subscription
JSON, and server response for debugging.

Usage:
    # Option 1: Pass token from the running app's token cache
    python3 scripts/test-dhan-200depth.py

    # Option 2: Pass token explicitly
    DHAN_ACCESS_TOKEN="eyJ..." DHAN_CLIENT_ID="1106656882" python3 scripts/test-dhan-200depth.py

    # Option 3: Use a specific SecurityId
    DHAN_SECURITY_ID="63424" python3 scripts/test-dhan-200depth.py

Prerequisites:
    pip install dhanhq==2.2.0rc1

What this script does:
    1. Reads token from token cache file OR environment variable
    2. Tests 200-level depth with ROOT PATH (/) — what Python SDK uses
    3. Tests 200-level depth with /twohundreddepth — what Dhan support recommended
    4. Prints exact URL, subscription JSON, and result for each test
    5. If either works, captures the first few depth frames as proof

Ticket: #5519522 (third consecutive day of TCP resets)
"""

import asyncio
import json
import os
import struct
import sys
import time

try:
    import websockets
except ImportError:
    print("ERROR: websockets not installed. Run: pip install websockets")
    sys.exit(1)


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# Read from env or token cache file
TOKEN_CACHE_PATH = "data/cache/tv-token-cache"

def load_credentials():
    """Load credentials from env vars or token cache file."""
    client_id = os.environ.get("DHAN_CLIENT_ID", "")
    access_token = os.environ.get("DHAN_ACCESS_TOKEN", "")

    if client_id and access_token:
        print(f"[OK] Credentials from environment variables (client_id={client_id})")
        return client_id, access_token

    # Try token cache file
    if os.path.exists(TOKEN_CACHE_PATH):
        try:
            with open(TOKEN_CACHE_PATH, "r") as f:
                cache = json.load(f)
            access_token = cache.get("access_token", "")
            client_id = cache.get("client_id", "")
            if access_token and client_id:
                print(f"[OK] Credentials from token cache ({TOKEN_CACHE_PATH})")
                print(f"     client_id={client_id}, token={access_token[:20]}...{access_token[-10:]}")
                return client_id, access_token
        except Exception as e:
            print(f"[WARN] Failed to read token cache: {e}")

    print("ERROR: No credentials found.")
    print("Set DHAN_ACCESS_TOKEN and DHAN_CLIENT_ID environment variables,")
    print("or ensure the app has written data/cache/tv-token-cache.")
    sys.exit(1)


EXCHANGE_SEGMENT = "NSE_FNO"  # String, not numeric — confirmed by SDK comparison

# NIFTY spot SecurityId (IDX_I segment) for LTP lookup
NIFTY_SPOT_SID = 13
BANKNIFTY_SPOT_SID = 25


def fetch_atm_security_id(access_token, client_id):
    """
    Fetches the ATM (At-The-Money) NIFTY CE SecurityId from Dhan's option chain API.
    This proves we're testing with a real, liquid, current-market contract.

    Steps:
        1. GET /v2/marketfeed/ltp to get NIFTY spot LTP
        2. POST /v2/optionchain/expirylist to get nearest expiry
        3. POST /v2/optionchain to get the chain
        4. Find the strike closest to spot LTP
        5. Return the CE SecurityId for that strike
    """
    import requests

    headers = {
        "access-token": access_token,
        "client-id": client_id,
        "Content-Type": "application/json",
    }

    # Step 1: Get NIFTY spot LTP
    print("[ATM] Fetching NIFTY spot LTP...")
    ltp_resp = requests.post(
        "https://api.dhan.co/v2/marketfeed/ltp",
        headers=headers,
        json={"IDX_I": [NIFTY_SPOT_SID]},
    )
    if ltp_resp.status_code != 200:
        print(f"[ATM] LTP API failed: {ltp_resp.status_code} {ltp_resp.text[:200]}")
        return None, None, None
    ltp_data = ltp_resp.json()
    spot_ltp = ltp_data.get("data", {}).get("IDX_I", {}).get(str(NIFTY_SPOT_SID), {}).get("last_price", 0)
    if spot_ltp <= 0:
        print(f"[ATM] Could not get NIFTY spot LTP. Response: {ltp_data}")
        return None, None, None
    print(f"[ATM] NIFTY spot LTP: {spot_ltp}")

    # Step 2: Get nearest expiry
    print("[ATM] Fetching expiry list...")
    expiry_resp = requests.post(
        "https://api.dhan.co/v2/optionchain/expirylist",
        headers=headers,
        json={"UnderlyingScrip": NIFTY_SPOT_SID, "UnderlyingSeg": "IDX_I"},
    )
    if expiry_resp.status_code != 200:
        print(f"[ATM] Expiry list API failed: {expiry_resp.status_code} {expiry_resp.text[:200]}")
        return None, None, None
    expiry_data = expiry_resp.json()
    expiries = expiry_data.get("data", [])
    if not expiries:
        print(f"[ATM] No expiries returned. Response: {expiry_data}")
        return None, None, None
    nearest_expiry = expiries[0]
    print(f"[ATM] Nearest expiry: {nearest_expiry}")

    # Step 3: Get option chain
    print("[ATM] Fetching option chain...")
    chain_resp = requests.post(
        "https://api.dhan.co/v2/optionchain",
        headers=headers,
        json={
            "UnderlyingScrip": NIFTY_SPOT_SID,
            "UnderlyingSeg": "IDX_I",
            "Expiry": nearest_expiry,
        },
    )
    if chain_resp.status_code != 200:
        print(f"[ATM] Option chain API failed: {chain_resp.status_code} {chain_resp.text[:200]}")
        return None, None, None
    chain_data = chain_resp.json()
    oc = chain_data.get("data", {}).get("oc", {})
    if not oc:
        print(f"[ATM] Empty option chain. Response keys: {chain_data.get('data', {}).keys()}")
        return None, None, None

    # Step 4: Find ATM strike (closest to spot LTP)
    best_strike = None
    best_diff = float("inf")
    best_ce_sid = None

    for strike_str, strike_data in oc.items():
        try:
            strike_price = float(strike_str)
        except ValueError:
            continue
        diff = abs(strike_price - spot_ltp)
        ce_data = strike_data.get("ce")
        if ce_data and diff < best_diff:
            best_diff = diff
            best_strike = strike_price
            best_ce_sid = ce_data.get("security_id")

    if not best_ce_sid:
        print("[ATM] Could not find ATM CE SecurityId in chain")
        return None, None, None

    label = f"NIFTY-{nearest_expiry}-{int(best_strike)}-CE"
    print(f"[ATM] ATM strike: {best_strike} (diff from spot: {best_diff:.2f})")
    print(f"[ATM] ATM CE SecurityId: {best_ce_sid} ({label})")

    return str(best_ce_sid), label, spot_ltp


# ---------------------------------------------------------------------------
# Test Functions
# ---------------------------------------------------------------------------

async def test_200_depth(url_base, client_id, access_token, security_id, label):
    """
    Test a 200-level depth WebSocket connection.
    Returns True if frames were received, False otherwise.
    """
    url = f"{url_base}?token={access_token}&clientId={client_id}&authType=2"

    # Redact token in log
    safe_url = url.replace(access_token, f"{access_token[:8]}...REDACTED")
    print(f"\n{'='*70}")
    print(f"TEST: {label}")
    print(f"{'='*70}")
    print(f"URL:  {safe_url}")

    subscribe_msg = json.dumps({
        "RequestCode": 23,
        "ExchangeSegment": EXCHANGE_SEGMENT,
        "SecurityId": security_id,
    })
    print(f"SUB:  {subscribe_msg}")
    print(f"SID:  {SECURITY_ID} (segment: {EXCHANGE_SEGMENT})")

    try:
        print(f"[...] Connecting...")
        ws = await asyncio.wait_for(
            websockets.connect(url),
            timeout=10.0,
        )
        print(f"[OK]  WebSocket connected!")

        # Send subscription
        await ws.send(subscribe_msg)
        print(f"[OK]  Subscription sent")

        # Wait for frames (up to 15 seconds)
        frames_received = 0
        start = time.time()
        timeout_secs = 15

        print(f"[...] Waiting for binary depth frames (up to {timeout_secs}s)...")

        while time.time() - start < timeout_secs:
            try:
                data = await asyncio.wait_for(ws.recv(), timeout=5.0)

                if isinstance(data, bytes):
                    frames_received += 1
                    # Parse the 12-byte header
                    if len(data) >= 12:
                        header = struct.unpack('<hBBiI', data[0:12])
                        msg_len = header[0]
                        response_code = header[1]
                        exchange_seg = header[2]
                        sec_id = header[3]
                        rows_or_seq = header[4]

                        code_name = {41: "BID", 51: "ASK", 50: "DISCONNECT"}.get(response_code, f"UNKNOWN({response_code})")
                        print(f"[FRAME #{frames_received}] code={code_name} seg={exchange_seg} sid={sec_id} "
                              f"len={msg_len} rows/seq={rows_or_seq} raw_bytes={len(data)}")

                        if response_code == 50:
                            # Disconnect packet
                            if len(data) >= 14:
                                reason = struct.unpack('<H', data[12:14])[0]
                                print(f"[DISCONNECT] Reason code: {reason}")
                            else:
                                print(f"[DISCONNECT] No reason code (only {len(data)} bytes)")
                            break

                        # If we got 3+ real frames, that's proof it works
                        if frames_received >= 4:
                            print(f"\n[SUCCESS] Received {frames_received} depth frames!")
                            await ws.close()
                            return True
                    else:
                        print(f"[FRAME #{frames_received}] raw_bytes={len(data)} (too short for header)")

                elif isinstance(data, str):
                    print(f"[TEXT] {data[:200]}")

            except asyncio.TimeoutError:
                elapsed = time.time() - start
                print(f"[...] No frame in 5s (elapsed: {elapsed:.1f}s, frames so far: {frames_received})")

        elapsed = time.time() - start
        if frames_received > 0:
            print(f"\n[PARTIAL] {frames_received} frames in {elapsed:.1f}s")
            await ws.close()
            return frames_received > 0
        else:
            print(f"\n[FAIL] Zero frames received in {elapsed:.1f}s")
            await ws.close()
            return False

    except asyncio.TimeoutError:
        print(f"[FAIL] Connection timed out (10s)")
        return False
    except websockets.exceptions.ConnectionClosedError as e:
        print(f"[FAIL] Connection closed: {e}")
        return False
    except Exception as e:
        error_str = str(e)
        if "ResetWithoutClosingHandshake" in error_str or "reset" in error_str.lower():
            print(f"[FAIL] TCP RESET — Protocol(ResetWithoutClosingHandshake)")
            print(f"       Server actively rejected this connection.")
        else:
            print(f"[FAIL] {type(e).__name__}: {e}")
        return False


async def run_all_tests():
    """Run tests on both URL paths using live ATM SecurityId."""
    client_id, access_token = load_credentials()

    # Fetch ATM SecurityId from live option chain
    atm_sid, atm_label, spot_ltp = fetch_atm_security_id(access_token, client_id)

    # Fallback to env or Dhan's suggested SID
    override_sid = os.environ.get("DHAN_SECURITY_ID", "")
    if override_sid:
        security_id = override_sid
        label = f"manual-override-SID-{override_sid}"
    elif atm_sid:
        security_id = atm_sid
        label = atm_label
    else:
        security_id = "63424"
        label = "Dhan-suggested-63424 (ATM fetch failed)"
        print("[WARN] ATM fetch failed — falling back to Dhan's suggested SID 63424")

    print(f"\n{'='*70}")
    print(f"Dhan 200-Level Depth Diagnostic")
    print(f"{'='*70}")
    print(f"Client ID:    {client_id}")
    print(f"Security ID:  {security_id} ({label})")
    print(f"Segment:      {EXCHANGE_SEGMENT}")
    if spot_ltp:
        print(f"NIFTY Spot:   {spot_ltp}")
    print(f"Timestamp:    {time.strftime('%Y-%m-%d %H:%M:%S IST')}")

    # Test 1: Root path (what Python SDK uses)
    root_ok = await test_200_depth(
        "wss://full-depth-api.dhan.co/",
        client_id, access_token, security_id,
        f"ROOT PATH (/) — Python SDK default — {label}"
    )

    # Small delay between tests
    await asyncio.sleep(2)

    # Test 2: /twohundreddepth (what Dhan support recommended in Ticket #5519522)
    explicit_ok = await test_200_depth(
        "wss://full-depth-api.dhan.co/twohundreddepth",
        client_id, access_token, security_id,
        f"/twohundreddepth — Dhan support Ticket #5519522 — {label}"
    )

    # Summary
    print(f"\n{'='*70}")
    print(f"SUMMARY")
    print(f"{'='*70}")
    print(f"Security ID tested:     {security_id} ({label})")
    if spot_ltp:
        print(f"NIFTY Spot at test:     {spot_ltp}")
    print(f"Root path (/)           : {'WORKS' if root_ok else 'FAILED'}")
    print(f"/twohundreddepth        : {'WORKS' if explicit_ok else 'FAILED'}")

    if root_ok and not explicit_ok:
        print(f"\nRECOMMENDATION: Switch to root path (/)")
        print(f"Dhan support gave wrong advice on Ticket #5519522.")
    elif explicit_ok and not root_ok:
        print(f"\nRECOMMENDATION: Keep /twohundreddepth (current)")
    elif root_ok and explicit_ok:
        print(f"\nBOTH WORK — keep /twohundreddepth (more explicit)")
    else:
        print(f"\nBOTH FAILED — issue is NOT the URL path or the SecurityId.")
        print(f"200-level depth is likely not enabled on account {client_id}.")
        print(f"Tested with live ATM SecurityId — this rules out stale/OTM contracts.")
        print(f"Escalate to Dhan: 'Enable 200-level Full Market Depth on our account.'")

    print(f"\nCopy this ENTIRE output and share with Dhan support.")


# ---------------------------------------------------------------------------
# Entry Point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    asyncio.run(run_all_tests())
