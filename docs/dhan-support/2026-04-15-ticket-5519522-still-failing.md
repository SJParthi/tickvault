**Subject:** Re: 200-Level Full Market Depth WebSocket — Still Resetting After Applying Your Fix (ATM Strike + /twohundreddepth path) — Client ID: 1106656882 — [#5519522]

---

Hi Trishul / Dhan API Team,

Thank you for your reply on April 10, 2026 under Ticket #5519522. I have now applied **both** corrections you recommended:

1. Switched the WebSocket URL to the explicit `/twohundreddepth` path (previously root path from the Python SDK).
2. Switched the `SecurityId` from far-OTM strikes to the **ATM (At-The-Money)** strike of the current-month expiry, selected live from the Option Chain API using the underlying's spot LTP.

**The 200-level Full Market Depth WebSocket is STILL resetting within 3–5 seconds of subscription**, with the exact same error as before (`Protocol(ResetWithoutClosingHandshake)`). Meanwhile, **the 20-level depth, Live Market Feed, and Live Order Update WebSockets on the same account, same token, same process all work perfectly throughout the same session.**

Requesting urgent investigation on your side — this is now purely a server-side issue, as our request is byte-for-byte what your own support email recommended.

---

### Account Details
| Field | Value |
|---|---|
| **Client ID** | 1106656882 |
| **Name** | Parthiban Subramanian |
| **UCC** | NWXF17021Q |
| **Segments** | Equity, F&O, Currency, Commodities |
| **Data Plan** | Active |
| **Prior Ticket** | #5519522 (response received 2026-04-10) |

---

### What We Changed After Your April 10 Reply

| Item | Previous (Failing) | Current (Still Failing) |
|---|---|---|
| URL path | `wss://full-depth-api.dhan.co/` (SDK root path) | **`wss://full-depth-api.dhan.co/twohundreddepth`** |
| Security ID | Far-OTM option (e.g. 72966) | **ATM CE / ATM PE selected live from `/v2/optionchain` using current spot LTP** |
| Underlyings | Mixed | **NIFTY + BANKNIFTY only** (4 connections, within 5-connection limit) |
| Code reference | — | `crates/common/src/constants.rs:986`, `crates/core/src/websocket/depth_connection.rs:357-504`, `crates/app/src/main.rs:2121-2254` |

---

### Precise Request Being Sent

**Connection URL:**
```
wss://full-depth-api.dhan.co/twohundreddepth?token=***REDACTED***&clientId=1106656882&authType=2
```

**HTTP Handshake:**
```
GET /twohundreddepth?token=***REDACTED***&clientId=1106656882&authType=2 HTTP/1.1
Host: full-depth-api.dhan.co
Upgrade: websocket
Connection: Upgrade
Sec-WebSocket-Version: 13
```

**Handshake Result:** HTTP 101 Switching Protocols — succeeds cleanly.

**Subscription message (sent as a single JSON text frame immediately after handshake, per your support email format):**
```json
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"<ATM_CE_OR_PE_SID>"}
```

Exactly one instrument per connection. 4 parallel connections total:
- NIFTY ATM CE (current-week expiry)
- NIFTY ATM PE (current-week expiry)
- BANKNIFTY ATM CE (current-week expiry)
- BANKNIFTY ATM PE (current-week expiry)

The ATM strike is re-derived every 60 seconds from live spot LTP and rebalanced on drift, so the SecurityId is always at the money — exactly what you asked us to test with.

---

### What Happens (Observed)

1. TLS handshake → succeeds
2. WebSocket upgrade (HTTP 101) → succeeds
3. Subscription JSON frame → sent successfully
4. **Server sends TCP RST within 3–5 seconds**
5. Zero binary depth frames received before the reset
6. Our code's exponential-backoff reconnect kicks in, the cycle repeats indefinitely

**Error captured (tokio-tungstenite):**
```
Protocol(ResetWithoutClosingHandshake)
```

This is identical to what we were seeing before your fix was applied. No WebSocket Close frame, no 8xx disconnect code in the Dhan envelope — a raw TCP reset from the server side.

---

### Live Logs — Today, 2026-04-15, IST (after market open)

Both NIFTY and BANKNIFTY ATM CE/PE are failing repeatedly. Attempt counters are already at **4 and 6** within the first 2 minutes of the trading session. Below is the exact, per-instrument, per-attempt timeline from our structured JSON logs. Every single row is a TCP RST from your server — zero binary frames received on any of these connections.

#### Per-instrument reset timeline (2026-04-15, IST)

| # | Timestamp (IST) | Instrument (Connection Label) | Underlying | Option Type | Strike | Attempt # | URL | Error |
|---|---|---|---|---|---|---|---|---|
| 1 | `09:39:20.125754` | `depth-200lvl-BANKNIFTY-ATM-CE` | BANKNIFTY | CE | ATM (current-week, live-derived) | 4 | `wss://full-depth-api.dhan.co/twohundreddepth` | `Protocol(ResetWithoutClosingHandshake)` |
| 2 | `09:39:20.129789` | `depth-200lvl-NIFTY-ATM-PE`      | NIFTY     | PE | ATM (current-week, live-derived) | 4 | `wss://full-depth-api.dhan.co/twohundreddepth` | `Protocol(ResetWithoutClosingHandshake)` |
| 3 | `09:39:20.129800` | `depth-200lvl-BANKNIFTY-ATM-PE` | BANKNIFTY | PE | ATM (current-week, live-derived) | 4 | `wss://full-depth-api.dhan.co/twohundreddepth` | `Protocol(ResetWithoutClosingHandshake)` |
| 4 | `09:39:20.129814` | `depth-200lvl-NIFTY-ATM-CE`      | NIFTY     | CE | ATM (current-week, live-derived) | 4 | `wss://full-depth-api.dhan.co/twohundreddepth` | `Protocol(ResetWithoutClosingHandshake)` |
| 5 | `09:40:21.455422` | `depth-200lvl-BANKNIFTY-ATM-CE` | BANKNIFTY | CE | ATM (current-week, live-derived) | 6 | `wss://full-depth-api.dhan.co/twohundreddepth` | `Protocol(ResetWithoutClosingHandshake)` |
| 6 | `09:40:21.457279` | `depth-200lvl-NIFTY-ATM-CE`      | NIFTY     | CE | ATM (current-week, live-derived) | 6 | `wss://full-depth-api.dhan.co/twohundreddepth` | `Protocol(ResetWithoutClosingHandshake)` |
| 7 | `09:40:21.457318` | `depth-200lvl-BANKNIFTY-ATM-PE` | BANKNIFTY | PE | ATM (current-week, live-derived) | 6 | `wss://full-depth-api.dhan.co/twohundreddepth` | `Protocol(ResetWithoutClosingHandshake)` |
| 8 | `09:40:21.459613` | `depth-200lvl-NIFTY-ATM-PE`      | NIFTY     | PE | ATM (current-week, live-derived) | 6 | `wss://full-depth-api.dhan.co/twohundreddepth` | `Protocol(ResetWithoutClosingHandshake)` |

**Pattern (very consistent):**
- All 4 ATM contracts fail in a tight **~4 ms window**, twice in a row (09:39:20 and 09:40:21 IST).
- Exactly the 4 contracts you asked us to test — live ATM, current expiry, major indices, NSE_FNO segment.
- Gap between attempt 4 and attempt 6 is ~61 seconds, which matches the backoff schedule (attempt 5 also fails silently in between — we omitted it for brevity, but we have it on record).
- **8 confirmed TCP resets in 61 seconds, zero binary depth frames received across any of the 4 connections.**

#### Raw JSON log lines (verbatim, for your trace correlation)

```
{"timestamp":"2026-04-15T09:39:20.125754+05:30","level":"WARN","fields":{"message":"depth-200lvl-BANKNIFTY-ATM-CE: connection failed — will reconnect","attempt":4,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection"}
{"timestamp":"2026-04-15T09:39:20.129789+05:30","level":"WARN","fields":{"message":"depth-200lvl-NIFTY-ATM-PE: connection failed — will reconnect","attempt":4,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection"}
{"timestamp":"2026-04-15T09:39:20.129800+05:30","level":"WARN","fields":{"message":"depth-200lvl-BANKNIFTY-ATM-PE: connection failed — will reconnect","attempt":4,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection"}
{"timestamp":"2026-04-15T09:39:20.129814+05:30","level":"WARN","fields":{"message":"depth-200lvl-NIFTY-ATM-CE: connection failed — will reconnect","attempt":4,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection"}
{"timestamp":"2026-04-15T09:40:21.455422+05:30","level":"WARN","fields":{"message":"depth-200lvl-BANKNIFTY-ATM-CE: connection failed — will reconnect","attempt":6,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection"}
{"timestamp":"2026-04-15T09:40:21.457279+05:30","level":"WARN","fields":{"message":"depth-200lvl-NIFTY-ATM-CE: connection failed — will reconnect","attempt":6,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection"}
{"timestamp":"2026-04-15T09:40:21.457318+05:30","level":"WARN","fields":{"message":"depth-200lvl-BANKNIFTY-ATM-PE: connection failed — will reconnect","attempt":6,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection"}
{"timestamp":"2026-04-15T09:40:21.459613+05:30","level":"WARN","fields":{"message":"depth-200lvl-NIFTY-ATM-PE: connection failed — will reconnect","attempt":6,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection"}
```

#### Same process, same token, same session — working WebSockets (for contrast)

While the 4 rows above are failing, the **exact same Rust process, on the exact same JWT access token, on the exact same machine/static-IP**, is successfully streaming data on every other Dhan WebSocket. Timestamps are from the *same log file, same minute window* so you can cross-verify:

| # | Timestamp (IST) | Component | Evidence | Status |
|---|---|---|---|---|
| 1 | `09:39:18.796015` | 20-level depth writer (NIFTY book) | `deep depth batch flushed to QuestDB flushed_rows=506` | ✅ Streaming |
| 2 | `09:39:18.796980` | 20-level depth writer (BANKNIFTY book) | `deep depth batch flushed to QuestDB flushed_rows=519` | ✅ Streaming |
| 3 | `09:39:18.824848` | 20-level depth persistence | `depth batch flushed to QuestDB flushed_snapshots=200` | ✅ Streaming |
| 4 | `09:39:18.925851` | Live Market Feed (tick pipeline) | `tick batch flushed to QuestDB flushed_rows=740` | ✅ Streaming |
| 5 | `09:39:18.994014` | Order Book Imbalance (NIFTY) | `OBI batch flushed to QuestDB flushed_rows=100 underlying=NIFTY` | ✅ Streaming |
| 6 | `09:39:19.977669` | Live Market Feed (tick pipeline) | `tick batch flushed to QuestDB flushed_rows=683` | ✅ Streaming |
| 7 | `09:40:20.919368` | 20-level depth persistence | `depth batch flushed to QuestDB flushed_snapshots=200` | ✅ Streaming |
| 8 | `09:40:20.967179` | Top movers / Live Market Feed | `stock movers snapshot computed total_tracked=239` | ✅ Streaming |

**The only thing failing on this account is 200-level Full Market Depth — at exactly the same wall-clock time when Live Feed, 20-level Depth, Order Updates, Option Chain, and every other API are all healthy.**

---

### What Is Working On The Same Account / Same Token / Same Process

This is the critical piece — the account, data plan, token, and static IP are all healthy, because every other WebSocket Dhan offers is streaming fine in the same process right now:

| WebSocket | Endpoint | Status Today (2026-04-15) |
|---|---|---|
| **Live Market Feed** (v2) | `wss://api-feed.dhan.co` | ✅ Working — ~740 ticks/batch flushed to QuestDB every ~1s |
| **20-Level Depth** | `wss://depth-api-feed.dhan.co/twentydepth` | ✅ Working — 200 depth snapshots/batch flushed every ~1s |
| **Live Order Update** | `wss://api-order-update.dhan.co` | ✅ Working |
| **200-Level Depth** | `wss://full-depth-api.dhan.co/twohundreddepth` | ❌ **TCP RST within 3–5s, every attempt, zero frames** |

Supporting evidence from the same log window:

```
2026-04-15T09:39:18.925851+05:30 DEBUG tick batch flushed to QuestDB  flushed_rows=740
2026-04-15T09:39:19.580636+05:30 DEBUG depth batch flushed to QuestDB flushed_snapshots=200   ← 20-level: fine
2026-04-15T09:39:19.977669+05:30 DEBUG tick batch flushed to QuestDB  flushed_rows=683
2026-04-15T09:40:20.919368+05:30 DEBUG depth batch flushed to QuestDB flushed_snapshots=200   ← 20-level: still fine
```

The `deep_depth_persistence` lines visible in our logs are the **20-level depth writer path** — they are NOT 200-level frames. The 200-level writer `tv_depth_200lvl_frames_received` counter remains at **0** because no binary frame ever arrives before the reset.

---

### Specific Questions For Your Team

1. Can you please check your server-side logs for `clientId=1106656882` on the `full-depth-api.dhan.co` / `/twohundreddepth` endpoint between **09:39:00 and 09:41:00 IST on 2026-04-15**? There should be at least 8 reset events on our side; we'd like to know what the server decided to reject.

2. Is 200-level Full Market Depth enabled at the **account level** for Client ID 1106656882 specifically? Your April 10 reply confirmed "subscribed to Data APIs", but Data APIs cover LTP / Quote / 20-level / historical too. Is 200-level a **separately enabled** feature?

3. Are there any **per-account, per-IP, or per-session caps** on `/twohundreddepth` that might be tripping the reset? Our static IP is registered for Order APIs and is healthy; `GET /v2/ip/getIP` returns `ordersAllowed=true`.

4. Does 200-level Full Market Depth have a **market-hours gating** on your side that silently TCP-RSTs subscriptions at certain times, even though 20-level keeps streaming? The reset happens continuously during the main trading session (we're seeing it right now at 9:39 IST, well within NSE hours 09:15–15:30).

5. Can you provide a **working curl / wscat / Postman test script** with a sample NIFTY ATM SecurityId that your internal team successfully connects to `/twohundreddepth`? If your team can connect from their network, we will adapt and re-test from our end. If they get the same RST, then the root cause is server-side.

6. If 200-level Full Market Depth requires a separate paid subscription on top of Data APIs, please tell us explicitly and we will subscribe today. We simply need an unambiguous answer — paid vs account-enabled vs server bug.

---

### What We Need From Dhan

- Server-side packet capture / reset reason for the 4 connections listed above between **09:39 and 09:41 IST on 2026-04-15**
- Confirmation whether 200-level Full Market Depth is enabled on Client ID 1106656882 at the account / data-plan level
- Either: a working end-to-end test from your infrastructure, or a clear statement that the feature is disabled for this account and the steps required to enable it

This has now been failing for over a week with your recommended fix applied, and it's the only missing piece in an otherwise fully-working DhanHQ v2 integration. We are ready to supply a packet capture (pcap), full source lines, or run any test script your team provides.

Thank you for your continued support.

Best regards,
**Parthiban Subramanian**
Client ID: 1106656882
UCC: NWXF17021Q

---

### Appendix A — Source Reference (public repo structure, for traceability)

| Concern | File | Line |
|---|---|---|
| 200-level URL constant | `crates/common/src/constants.rs` | 986 |
| 200-level URL builder / handshake | `crates/core/src/websocket/depth_connection.rs` | 437–490 |
| 200-level subscription JSON | `crates/core/src/websocket/depth_connection.rs` | 370–375 |
| 200-level boot wiring (NIFTY + BANKNIFTY ATM CE/PE) | `crates/app/src/main.rs` | 2121–2254 |
| ATM strike selector (live from option chain) | `crates/core/src/instrument/depth_strike_selector.rs` | 56–100 |
| ATM rebalancer (60s spot drift tracker) | `crates/core/src/instrument/depth_rebalancer.rs` | — |

### Appendix B — Exact Subscription Frames On Wire (from our serializer)

```json
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"<NIFTY_ATM_CE_current_week>"}
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"<NIFTY_ATM_PE_current_week>"}
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"<BANKNIFTY_ATM_CE_current_week>"}
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"<BANKNIFTY_ATM_PE_current_week>"}
```

Each frame is sent on its own TCP/TLS connection (1 instrument per connection, as per your spec). The SecurityIds are re-derived from the live option chain every boot and every 60 seconds. On request, we can share the exact 4 SecurityIds used at the 09:39 IST reset window from today's run.
