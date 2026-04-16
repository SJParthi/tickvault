**Subject:** Re: 200-Level Full Market Depth — All Tests Failed Including Your SDK & SID — Client ID: 1106656882 — [#5519522]

Hi Shubham,

Thank you for the Python script and SDK suggestion. We tested everything you recommended — all tests fail on our account.

**What we tested (2026-04-16, 13:23 IST, during market hours):**

Test 1: Raw WebSocket, SID 63422 (ATM NIFTY 24100 CE, spot 24124), root path (/), segment "NSE_FNO" — FAILED, TCP reset, zero frames

Test 2: Raw WebSocket, SID 63422, /twohundreddepth path, segment "NSE_FNO" — FAILED, zero frames then reset

Test 3: Raw WebSocket, SID 63422, /twohundreddepth path, segment as numeric 2 — FAILED, zero frames then reset

Test 4: Raw WebSocket, SID 63424 (your suggested SID), root path (/), segment "NSE_FNO" — FAILED, zero frames then reset

Test 5: Your SDK dhanhq==2.2.0rc1 — SDK crashes on Python 3.9 due to `match` statement in `_super_order.py` line 54 (requires Python 3.10+)

**What WORKS on the same account, same token, same moment:**

- 20-level depth — streaming perfectly (4 connections, all green)
- Live Market Feed — ~3K ticks/sec
- Order Update WebSocket — connected
- All REST APIs — healthy

**Conclusion:** 200-level Full Market Depth appears to be not enabled at the account level for Client ID 1106656882. We have exhausted every variable — URL path, SecurityId (ATM + your suggested SID), segment format (string + numeric), and your own Python SDK.

**Request:** Please check on your server whether 200-level depth is enabled for our account and enable it if not.

Happy to connect on Google Meet — available today 2-4 PM IST or tomorrow 9:30-11 AM IST. I'll run the diagnostic live on screen share. Please have someone from engineering who can check server-side logs for clientId=1106656882 on full-depth-api.dhan.co.

Thank you,
**Parthiban Subramanian**
Client ID: 1106656882
