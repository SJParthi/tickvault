**Subject:** Re: 200-Level Full Market Depth — Third Consecutive Day of TCP Resets, All 4 ATM Contracts — Client ID: 1106656882 — [#5519522]

---

Hi Trishul / Dhan API Team,

Following up on Ticket #5519522 (your reply April 10, our detailed report April 15). Today is the **third consecutive trading day** where 200-level Full Market Depth connections are being TCP-reset by your server. No changes on our side since April 15. All other WebSockets remain healthy.

---

### Account Details

| Field | Value |
|---|---|
| **Client ID** | 1106656882 |
| **Name** | Parthiban Subramanian |
| **UCC** | NWXF17021Q |
| **Segments** | Equity, F&O, Currency, Commodities |
| **Data Plan** | Active |
| **Prior Ticket** | #5519522 (your reply 2026-04-10, our follow-up 2026-04-15) |
| **Date of Incident** | 2026-04-16 (Wednesday, today — third day) |
| **Time Window** | 11:36:58 – 11:38:14 IST (ongoing, repeats every ~25s) |

---

### What We Are Sending (unchanged since April 15)

**Connection URL:**
```
wss://full-depth-api.dhan.co/twohundreddepth?token=***REDACTED***&clientId=1106656882&authType=2
```

**Subscription JSON (one per connection, flat structure per your spec):**
```json
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"65866"}
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"65869"}
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"66454"}
{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"66457"}
```

**4 connections total (within 5-connection limit), 1 instrument per connection.**

### Exact Contracts Failing Today

| # | Underlying | Option | Strike | Expiry | SecurityId | Segment |
|---|---|---|---|---|---|---|
| 1 | NIFTY | CE (ATM) | 17400 | Apr 2026 | **65866** | `NSE_FNO` |
| 2 | NIFTY | PE (ATM) | 17400 | Apr 2026 | **65869** | `NSE_FNO` |
| 3 | BANKNIFTY | CE (ATM) | 42500 | May 2026 | **66454** | `NSE_FNO` |
| 4 | BANKNIFTY | PE (ATM) | 42500 | May 2026 | **66457** | `NSE_FNO` |

These are ATM strikes selected live from `POST /v2/optionchain` using current spot LTP, exactly as you instructed on April 10. The SecurityIds changed from yesterday (Apr 15) because the ATM strike shifted — confirming our code dynamically selects ATM, not hardcoded.

---

### Per-Attempt Reset Timeline — 2026-04-16, IST

All 4 contracts fail in a tight <4ms window on every attempt. Zero binary depth frames received before any reset. The server completes the TLS+WebSocket handshake (HTTP 101), accepts our subscription JSON, then sends TCP RST within 3–5 seconds.

| # | Timestamp (IST) | Contract Label | SecurityId | Attempt | Error |
|---|---|---|---|---|---|
| 1 | `11:36:58.839674` | `BANKNIFTY-May2026-42500-CE` | **66454** | 0 | `Protocol(ResetWithoutClosingHandshake)` |
| 2 | `11:36:58.839722` | `NIFTY-Apr2026-17400-CE` | **65866** | 0 | `Protocol(ResetWithoutClosingHandshake)` |
| 3 | `11:36:58.842668` | `NIFTY-Apr2026-17400-PE` | **65869** | 0 | `Protocol(ResetWithoutClosingHandshake)` |
| 4 | `11:36:58.842759` | `BANKNIFTY-May2026-42500-PE` | **66457** | 0 | `Protocol(ResetWithoutClosingHandshake)` |
| 5 | `11:37:23.483525` | `NIFTY-Apr2026-17400-PE` | **65869** | 1 | `Protocol(ResetWithoutClosingHandshake)` |
| 6 | `11:37:23.483601` | `NIFTY-Apr2026-17400-CE` | **65866** | 1 | `Protocol(ResetWithoutClosingHandshake)` |
| 7 | `11:37:23.483779` | `BANKNIFTY-May2026-42500-PE` | **66457** | 1 | `Protocol(ResetWithoutClosingHandshake)` |
| 8 | `11:37:23.484092` | `BANKNIFTY-May2026-42500-CE` | **66454** | 1 | `Protocol(ResetWithoutClosingHandshake)` |
| 9 | `11:37:49.584663` | `NIFTY-Apr2026-17400-PE` | **65869** | 2 | `Protocol(ResetWithoutClosingHandshake)` |
| 10 | `11:37:49.584775` | `BANKNIFTY-May2026-42500-CE` | **66454** | 2 | `Protocol(ResetWithoutClosingHandshake)` |
| 11 | `11:37:49.586757` | `NIFTY-Apr2026-17400-CE` | **65866** | 2 | `Protocol(ResetWithoutClosingHandshake)` |
| 12 | `11:37:49.586788` | `BANKNIFTY-May2026-42500-PE` | **66457** | 2 | `Protocol(ResetWithoutClosingHandshake)` |
| 13 | `11:38:13.994724` | `NIFTY-Apr2026-17400-CE` | **65866** | 3 | `Protocol(ResetWithoutClosingHandshake)` |
| 14 | `11:38:13.994791` | `BANKNIFTY-May2026-42500-CE` | **66454** | 3 | `Protocol(ResetWithoutClosingHandshake)` |
| 15 | `11:38:13.997214` | `NIFTY-Apr2026-17400-PE` | **65869** | 3 | `Protocol(ResetWithoutClosingHandshake)` |
| 16 | `11:38:13.997280` | `BANKNIFTY-May2026-42500-PE` | **66457** | 3 | `Protocol(ResetWithoutClosingHandshake)` |

**Pattern:** 4 resets per cycle × 4 cycles in 75 seconds = **16 TCP resets, zero depth frames received**. Reconnect backoff is ~25 seconds between attempts (exponential with jitter). This repeats indefinitely.

---

### What Works On Same Account, Same Token, Same Process, Same Minute

| WebSocket | Endpoint | Status Today (2026-04-16) | Evidence |
|---|---|---|---|
| **Live Market Feed** (v2) | `wss://api-feed.dhan.co` | ✅ Streaming | Tick gaps detected = normal F&O illiquidity, not connection loss |
| **20-Level Depth** | `wss://depth-api-feed.dhan.co/twentydepth` | ✅ Streaming | Flushing to QuestDB continuously |
| **Live Order Update** | `wss://api-order-update.dhan.co` | ✅ Connected | Auth succeeded |
| **200-Level Depth** | `wss://full-depth-api.dhan.co/twohundreddepth` | ❌ **TCP RST every attempt, zero frames, 3 days running** |

The tick feed gap warnings visible in our logs (security_ids 87034, 67560, 71331, etc.) are **normal F&O illiquidity gaps** — those instruments simply have no trades for 30–120 seconds. The Live Market Feed WebSocket itself is healthy and streaming continuously. This is NOT related to the 200-level depth issue.

---

### Three Days of Evidence

| Date | Contracts Tested | SecurityIds | Result | Our Email |
|---|---|---|---|---|
| **2026-04-14** | NIFTY/BANKNIFTY ATM CE/PE | (different SIDs — ATM shifted) | ❌ TCP RST | — |
| **2026-04-15** | NIFTY/BANKNIFTY ATM CE/PE | (different SIDs — ATM shifted) | ❌ TCP RST | `2026-04-15-ticket-5519522-still-failing.md` |
| **2026-04-16** | NIFTY-17400-CE/PE, BANKNIFTY-42500-CE/PE | 65866, 65869, 66454, 66457 | ❌ TCP RST | **This email** |

Three different sets of ATM SecurityIds across three days. Same result every time. This rules out any SecurityId-specific issue.

---

### Verbatim Log Lines (for server-side grep correlation)

```json
{"timestamp":"2026-04-16T11:36:58.839674+05:30","level":"WARN","fields":{"message":"depth-200lvl-BANKNIFTY-May2026-42500-CE: connection failed — will reconnect","security_id":66454,"label":"BANKNIFTY-May2026-42500-CE","attempt":0,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection","filename":"crates/core/src/websocket/depth_connection.rs","line_number":590,"threadId":"ThreadId(7)"}
{"timestamp":"2026-04-16T11:36:58.839722+05:30","level":"WARN","fields":{"message":"depth-200lvl-NIFTY-Apr2026-17400-CE: connection failed — will reconnect","security_id":65866,"label":"NIFTY-Apr2026-17400-CE","attempt":0,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection","filename":"crates/core/src/websocket/depth_connection.rs","line_number":590,"threadId":"ThreadId(13)"}
{"timestamp":"2026-04-16T11:36:58.842668+05:30","level":"WARN","fields":{"message":"depth-200lvl-NIFTY-Apr2026-17400-PE: connection failed — will reconnect","security_id":65869,"label":"NIFTY-Apr2026-17400-PE","attempt":0,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection","filename":"crates/core/src/websocket/depth_connection.rs","line_number":590,"threadId":"ThreadId(4)"}
{"timestamp":"2026-04-16T11:36:58.842759+05:30","level":"WARN","fields":{"message":"depth-200lvl-BANKNIFTY-May2026-42500-PE: connection failed — will reconnect","security_id":66457,"label":"BANKNIFTY-May2026-42500-PE","attempt":0,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection","filename":"crates/core/src/websocket/depth_connection.rs","line_number":590,"threadId":"ThreadId(5)"}
{"timestamp":"2026-04-16T11:37:49.584775+05:30","level":"WARN","fields":{"message":"depth-200lvl-BANKNIFTY-May2026-42500-CE: connection failed — will reconnect","security_id":66454,"label":"BANKNIFTY-May2026-42500-CE","attempt":2,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection","filename":"crates/core/src/websocket/depth_connection.rs","line_number":590,"threadId":"ThreadId(12)"}
{"timestamp":"2026-04-16T11:38:13.994791+05:30","level":"WARN","fields":{"message":"depth-200lvl-BANKNIFTY-May2026-42500-CE: connection failed — will reconnect","security_id":66454,"label":"BANKNIFTY-May2026-42500-CE","attempt":3,"err":"ConnectionFailed { url: \"wss://full-depth-api.dhan.co/twohundreddepth\", source: Protocol(ResetWithoutClosingHandshake) }"},"target":"tickvault_core::websocket::depth_connection","filename":"crates/core/src/websocket/depth_connection.rs","line_number":590,"threadId":"ThreadId(7)"}
```

---

### Requests to Dhan Engineering

1. **Server-side logs for `clientId=1106656882` on `full-depth-api.dhan.co`** between **11:36:50 and 11:38:20 IST on 2026-04-16**. There are 16 TCP resets in this 90-second window — your server must have rejection logs explaining why.

2. **Is 200-level Full Market Depth enabled at the account level** for Client ID 1106656882? This is the third time we are asking. We need a definitive yes/no. If it requires a separate subscription beyond "Data APIs", please tell us the exact steps to enable it.

3. **Can your engineering team successfully connect to `/twohundreddepth`** from their own infrastructure with any NIFTY or BANKNIFTY ATM SecurityId? If yes, please share the exact `wscat` / `curl` / Postman command so we can replicate. If they also get TCP RST, then this is a server-side outage affecting all clients.

4. **Is there a per-account connection cap** on the `/twohundreddepth` endpoint that is separate from the documented 5-connection limit? We are only opening 4 connections, well within spec.

5. **Timeline for resolution.** This has been failing since at least April 14, now 3 consecutive trading days. We have applied every fix you recommended (ATM strikes, `/twohundreddepth` path). The 200-level depth is the only missing piece in our otherwise complete DhanHQ v2 integration.

---

### What We Need

- **A definitive answer** on whether 200-level depth works for this account or not.
- If it works: a working example we can test against.
- If it doesn't work: what needs to happen to enable it.
- If it's a known server-side issue: an ETA for the fix.

We are ready to supply packet captures (tcpdump/pcap), run any test script you provide, or test from a different IP/machine. We simply need direction from your engineering team.

Thank you,
**Parthiban Subramanian**
Client ID: 1106656882
UCC: NWXF17021Q

---

### Appendix — Source Reference

| Concern | File | Line |
|---|---|---|
| 200-level URL constant | `crates/common/src/constants.rs` | 986 |
| 200-level connect + subscribe | `crates/core/src/websocket/depth_connection.rs` | 611–876 |
| 200-level subscription JSON builder | `crates/core/src/websocket/depth_connection.rs` | 522–528 |
| ATM strike selector | `crates/core/src/instrument/depth_strike_selector.rs` | 56–100 |
| ATM rebalancer (60s spot drift) | `crates/core/src/instrument/depth_rebalancer.rs` | — |
