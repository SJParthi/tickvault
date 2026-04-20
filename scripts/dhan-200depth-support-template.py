#!/usr/bin/env python3
"""
Dhan 200-Level Depth — Support Template (1:1 copy of Dhan's `200latest.py`)

This is the EXACT template Dhan support ships in their official sample file.
When the Dhan team asks "run this on your end and paste output", paste your
three values into the placeholders below and run — the rest of the code is
unchanged so output format matches what Dhan expects.

Setup (one-time)
----------------
    python3.12 -m venv .venv-dhan
    source .venv-dhan/bin/activate
    pip install dhanhq==2.2.0rc1

Usage
-----
1. Edit the 3 PLACEHOLDER lines below.
2. Save.
3. Run:
       source .venv-dhan/bin/activate
       python scripts/dhan-200depth-support-template.py

Shareable with Dhan
-------------------
- Paste the full stdout back to the support ticket / email.
- Do NOT paste the token value — redact `Access Token` before sharing.
"""

from dhanhq import DhanContext, FullDepth

# ------------------------------------------------------------
# PLACEHOLDERS — edit these 3 lines (everything else is verbatim Dhan)
# ------------------------------------------------------------
CLIENT_ID    = "Client ID"       # e.g. "1106656882"
ACCESS_TOKEN = "Access Token"    # your 24h JWT — from data/cache/tv-token-cache
SECURITY_ID  = "63424"           # e.g. "63434" for current ATM NIFTY CE
# ------------------------------------------------------------

dhan_context = DhanContext(CLIENT_ID, ACCESS_TOKEN)  # Replace with your valid client ID and access token

instruments = [(2, SECURITY_ID)]  # NIFTY 21 APR 24150 CALL
depth_level = 200

# NIFTY 21 APR 24150 CALL

# 20 or 200, default 20 in case this is not passed
try:
    response = FullDepth(dhan_context, instruments, depth_level)  # depth_level is non mandatory for 20 depth
    response.run_forever()

    while True:
        response.get_data()

        if response.on_close:
            print("Server disconnection detected. Kindly try again.")
            break

except Exception as e:
    print(e)
