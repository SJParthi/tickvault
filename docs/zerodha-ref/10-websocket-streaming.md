# Zerodha Kite Connect v3 — WebSocket Streaming (`wss://ws.kite.trade`) — THE feed-decision file

> **Source:** `https://kite.trade/docs/connect/v3/websocket/` (live official page — FETCHED VERBATIM by the GH-runner crawl 2026-07-14, HTTP 200, sha256 `c750c431…` in the crawl fetch-log; the `#quote-packet-structure` / `#index-packet-structure` / `#market-depth-structure` / `#message-types` anchors all confirmed present in the live HTML)
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE + OFFICIAL-MOCK + SEARCH + ARITH; **live-verbatim via GH-runner 2026-07-14 (UTC timestamps in 00-COVERAGE-MANIFEST.md)** — the official websocket docs page (and forum thread 15708) are now in the live evidence bundle, so every doc-table fact below is upgraded to the top tier. Cross-check evidence RETAINED: CLIENT-LIB-SOURCE (pykiteconnect@master `kiteconnect/ticker.py`, 863 lines, full binary parser; gokiteconnect@master `ticker/ticker.go`, 779 lines, independent cross-check) + OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main `ticker_ltp.json` / `ticker_quote.json` / `ticker_quote.packet` / `ticker_full.json` / `ticker_full.packet` / `postback.json` — the two `.packet` files are base64-encoded REAL wire packet bodies, decoded and re-parsed in this sandbox at the code-derived offsets, matching the paired JSON on every WIRE-DECODED field — the derived `change` field is excluded, see §12 note) + ARITH (epoch-semantics derivation from the mock timestamps).
> **Evidence tiers:** see README legend — **Verified (LIVE-DOC runner 2026-07-14)** is the new top tier / Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / ARITH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Adding Zerodha as a live feed requires its own dated operator scope lock (the `gdf-third-feed-scope-2026-07-13.md` / `groww-second-feed-scope-2026-06-19.md` pattern) FIRST.
> **Related:** `07-market-quotes.md` (REST snapshot twin of the quote fields), `02-rest-conventions-errors-and-rate-limits.md`, `14-option-chain-composition.md` §5 (India-F&O/BFO coverage note — SENSEX/BFO live-tick delivery is a day-0 probe), `99-UNKNOWNS.md`.

---

## 0. Provenance note — the live page is now IN HAND; the packet structure is quadruple-sourced

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/):**
the official page was fetched verbatim by the GH-runner crawl. Its packet tables
(message structure, quote packet, index packet, market depth), mode table, limits
paragraph, heartbeat paragraph, and message-types table all match the SDK-derived
layout below — every former SEARCH-tier fact is upgraded, and one Assumed fact is
resolved (index bytes 24–27 = "Price change", §9).

The cross-check chain STAYS: the two official SDKs (Python + Go, both © Zerodha)
each contain a COMPLETE, independent implementation of the binary protocol, and
they agree byte-for-byte on every offset, divisor, and packet length. On top of
that, the official `kiteconnect-mocks` repo ships two REAL captured wire packets
(`ticker_quote.packet`, `ticker_full.packet`, base64-encoded) alongside the
expected parsed JSON — decoded in this sandbox, every WIRE-DECODED field at the
code-derived offsets reproduces the official JSON exactly (§12; the client-derived
`change` field is excluded — its semantics diverge three ways, see the §12 note).
So the packet structure below is now quadruple-sourced: live docs table + two
independent official SDK parsers + a real decoded wire packet.

**⚠ Correction (live fetch 2026-07-14):** the earlier SEARCH corroboration
sentence "The data is transmitted in big endian format" does NOT appear anywhere
on the live page (grep of the fetched HTML: zero hits for "endian"). Endianness
is therefore SDK+mock-verified only (still rock-solid — the mock packet decodes
correctly ONLY as big-endian) but it is no longer claimed as docs-corroborated.

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `README.md` changelog):** the
docs page's `#quote-packet-structure` anchor is also referenced by the changelog
entry "Renamed ticker fields as per [kite connect doc]
(https://kite.trade/docs/connect/v3/websocket/#quote-packet-structure)"
(https://raw.githubusercontent.com/zerodha/pykiteconnect/master/README.md,
fetched 2026-07-13) — the anchor's existence is now directly confirmed in the
live HTML (`id="quote-packet-structure"`). Note (corrected 2026-07-14): the live
table's field labels are DESCRIPTIVE long names ("Last traded quantity",
"Average traded price", …), not the short JSON keys of the mocks — the earlier
inference that "the docs tables use the short names" was wrong; see §10.

## 1. Endpoint + authentication

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/#connecting-to-the-websocket-endpoint):** "The WebSocket endpoint is `wss://ws.kite.trade`. To establish a connection, you have to pass two query parameters, `api_key` and `access_token`." The page's own JS example: `new WebSocket("wss://ws.kite.trade?api_key=xxx&access_token=xxxx")`.

Cross-check **Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/ticker.py` lines 382, 443–448; fetched 2026-07-13, https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/ticker.py):**

```
ROOT_URI = "wss://ws.kite.trade"
socket_url = "{root}?api_key={api_key}&access_token={access_token}"
```

Cross-check **Verified (CLIENT-LIB-SOURCE gokiteconnect@master `ticker/ticker.go` lines 152–155, 289–293):** `tickerURL = url.URL{Scheme: "wss", Host: "ws.kite.trade"}` with `q.Set("api_key", …)` + `q.Set("access_token", …)`.

- Auth is **entirely in the URL query string** — `api_key` + `access_token` (the token minted by the login flow, see `01-overview-auth-and-token-lifecycle.md`). No auth frame after connect. The live page documents no other connect-time parameter or header.
- **Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` lines 505–507):** the Python SDK additionally sends an HTTP header `X-Kite-Version: 3` on the WS handshake ("For version 3"). The Go SDK sends NO extra header on its dial (`d.Dial(t.url.String(), nil)`) and works — so the header appears optional on the WS surface; the live page (2026-07-14) mentions NO header for the WS connect, only the two query params — **Assumed:** the `?api_key&access_token` query params alone select v3 behaviour. (The REST surface, by contrast, requires the header — `02-rest-conventions…`.)
- **Compare: Dhan** puts the token in the WS URL too (`wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` — `docs/dhan-ref/03-live-market-feed-websocket.md`); **Groww** does in-band NATS CONNECT auth with a per-session socket token (`docs/groww-ref/README.md` [R#27]).

## 2. Connection limits — LIVE-VERIFIED

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/), verbatim:** "You can subscribe for up to **3000 instruments** on a single WebSocket connection and receive live quotes for them. Single API key can have upto **3 websocket connections**." → ≈ 9,000 instruments max per api_key (**ARITH:** 3 × 3000).

**Enforcement behaviour — Verified (LIVE-DOC runner 2026-07-14, forum thread https://kite.trade/forum/discussion/15708/ in the same fetch-log; Zerodha staff `salim_chisty`, December 2025):**

- \>3000 on one connection: "If a subscription request exceeds the 3,000-instrument [limit] … the API will return an error for the additional tokens. However, the existing WebSocket connection will remain active and unaffected." Exact wire shape of that error frame: **Unknown** (presumably a `type: "error"` text frame, §13 — not shown in the thread).
- Connection count: "You can maintain a maximum of three concurrent WebSocket connections per API key across all Zerodha users" — but "there is no hard-enforced limit on the total number of WebSocket connections, a **soft limit** is in place. Exceeding reasonable usage thresholds may result in your application being **flagged**, which could lead to **suspension**." I.e. the 3-connection cap is policy-enforced (flag/suspend), not necessarily socket-refused; the thread's poster measurably ran slightly >3000 per connection with ticks still flowing — do NOT design capacity around exceeding either number.

Neither number appears anywhere in either SDK source (grep-verified over `ticker.py` + `ticker.go`) — these are server-side limits documented only on the docs page + staff forum answers.

**Compare: Dhan** allows 5 WS connections × 5,000 instruments (batches of 100 per subscribe message) — `docs/dhan-ref/03-live-market-feed-websocket.md`. **Groww** documents "up to 1000 instruments at a time" with per-connection-vs-per-account scope unstated, and MEASURABLY runs an undocumented adaptive connection cap — `docs/groww-ref/README.md` [R#7], [R#23].

## 3. The two channels: binary frames (ticks) vs text frames (everything else)

**Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` `_on_message` lines 668–679 + gokiteconnect `readMessage` lines 458–471):**

| Frame type | Content | Handling |
|---|---|---|
| **Binary** | one or more quote packets (the multi-packet container of §6) | parsed by `_parse_binary` / `parseBinary` |
| **Binary, length < 2 bytes** | **heartbeat** | ignored by both SDKs (`if len(bin) < 2: return []`) |
| **Text (UTF-8 JSON)** | order updates (postbacks) + error messages | parsed by `_parse_text_message` / `processTextMessage` (§13) |

- Heartbeat cadence — **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/#modes), verbatim:** "**If there is no data to be streamed** over an open WebSocket connection, the API will send a **1 byte 'heartbeat' every couple seconds** to keep the connection alive. This can be safely ignored." Note the live wording's conditional (heartbeats fill data silence, not a fixed metronome) — the earlier SEARCH extraction lacked it. Exact cadence beyond "every couple seconds" is still unstated. The SDKs' `< 2 bytes` guard is the code-level confirmation that a 1-byte binary frame is expected traffic.
- Frame-type discipline — **Verified (LIVE-DOC, same page):** "Always check the type of an incoming WebSocket messages. **Market data is always binary and Postbacks and other updates are always text.**"
- The Python SDK only attempts tick-parsing on binary payloads `> 4` bytes (`ticker.py` line 674) — anything 2–4 bytes would be silently dropped; no such frame is documented. **Robustness wart (Verified, adversarial re-read 2026-07-13):** the Go SDK has NO such gate — a malformed 2–4-byte binary frame with a nonzero packet count would panic `splitPackets`' slicing. A native parser should copy py's strict guards, not go's.
- **Compare: Dhan** has no heartbeat packet in the data plane (WS-level ping/pong every 10s, handled by the WS library); **Groww** uses NATS `PING`/`PONG` protocol frames plus server-push messages.

## 4. Client → server messages (text JSON)

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/#request-structure):** "Requests are simple JSON messages with two parameters, `a` (action) and `v` (value)" — the live page's action table lists exactly `subscribe` → `[instrument_token ...]`, `unsubscribe` → `[instrument_token ...]`, `mode` → `[mode, [instrument_token ...]]`, and its JS examples send NUMERIC tokens: `{ a: "subscribe", v: [408065, 884737] }`, `{ a: "mode", v: ["full", [408065]] }`, `{ a: "mode", v: ["ltp", [884737]] }`. Cross-check **Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` lines 392–396, 567–628 + gokiteconnect `ticker.go` lines 489–552):**

| Action | Message | Notes |
|---|---|---|
| Subscribe | `{"a": "subscribe", "v": [738561, 5633]}` | `v` = array of NUMERIC instrument_tokens (from the instruments CSV) |
| Unsubscribe | `{"a": "unsubscribe", "v": [738561]}` | |
| Set mode | `{"a": "mode", "v": ["full", [738561]]}` | `v` = 2-element array: mode string, then token array |

- Tokens are numbers on the wire, NOT strings (live JS examples + both SDKs marshal plain ints / `[]uint32`). **Compare: Dhan** requires STRING SecurityIds in its subscribe JSON (`docs/dhan-ref/03-live-market-feed-websocket.md` rule 15) — the exact opposite convention.
- **Default mode after bare subscribe:** the Python SDK bookkeeps freshly-subscribed tokens as `quote` mode (`ticker.py` line 579), while the Go SDK records them as `modeEmpty` and never assumes. **The live page (read in full 2026-07-14) does NOT state the server default either** — **Assumed (from the py SDK's bookkeeping + the mock naming):** the server default mode on subscribe is `quote`; only a live-wire probe can settle it (99-UNKNOWNS).
- There is a `_message_code = 11` constant in `ticker.py` (line 393) that is **never used anywhere** in the file — a vestige; no numeric message codes exist in the v3 protocol.
- Resubscribe-after-reconnect is CLIENT-side: both SDKs replay `subscribe` + `mode` for all tracked tokens on reconnect (`ticker.py` `resubscribe()` lines 630–647; `ticker.go` `Resubscribe()` lines 555–590). The server does NOT restore subscriptions.

## 5. The three streaming modes

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/#modes):** "There are three different modes in which quote packets are streamed" — the official mode table, verbatim:

| mode | Live-doc description |
|---|---|
| `ltp` | "LTP. Packet contains only the last traded price (**8 bytes**)." |
| `quote` | "Quote. Packet contains several fields excluding market depth (**44 bytes**)." |
| `full` | "Full. Packet contains several fields including market depth (**184 bytes**)." |

Cross-check **Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` lines 384–387 + gokiteconnect `ticker.go` lines 95–100):** same three mode strings, set per-token via the `mode` message; mode determines the packet SIZE the server emits for that token (§7). Note the live mode table gives the TRADABLE-instrument sizes; index instruments emit the shorter 28/32-byte packets of §9 (implied by the live index table's "packet ends here" markers, explicit in the SDK constants).

## 6. Binary message framing (the multi-packet container)

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/#message-structure):** "Each binary message … is a combination of one or more quote packets for one or more instruments", with the official structure table: A = "The first two bytes ([0 - 2] — SHORT or int16) represent the number of packets in the message"; B = "The next two bytes ([2 - 4] — SHORT or int16) represent the length (number of bytes) of the first packet"; C = "The next series of bytes ([4 - 4+B]) is the quote packet"; then per further packet a 2-byte length + body (`[4+B - 4+B+2]`, `[4+B+2 - 4+B+2+D]`, …). Cross-check **Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` `_split_packets` lines 848–863 + `_unpack_int` lines 844–846; gokiteconnect `splitPackets` lines 635–651):**

```
byte 0-1   : int16 (big-endian, unsigned)  number of packets in this message
per packet:
  2 bytes  : int16 (big-endian, unsigned)  length of the following packet body
  N bytes  : packet body (N = the length just read)
```

- **ALL integers in the protocol are BIG-ENDIAN unsigned** — `struct.unpack(">I"/">H")` in Python, `binary.BigEndian.Uint32/Uint16` in Go. **⚠ Corrected 2026-07-14:** the LIVE page never states endianness (zero hits for "endian" in the fetched HTML) and labels the types with SIGNED names ("SHORT or int16", "int32") — the earlier SEARCH corroboration "The data is transmitted in big endian format" is NOT on the live page and is withdrawn. Endianness + unsignedness rest on CLIENT-LIB-SOURCE (both SDKs) + OFFICIAL-MOCK (the real `ticker_full.packet` decodes to the official JSON values ONLY as big-endian unsigned — empirical proof, §12). Doc-vs-SDK discrepancy flagged: docs say "int32/int16" (signed naming), SDKs read unsigned; for real price/qty magnitudes the two are indistinguishable.
- One binary WS message batches ticks for MANY instruments (each with its own length prefix).
- **Compare: Dhan** is the opposite: LITTLE-endian, one fixed 8-byte header per packet (code u8, length u16 LE, segment u8, security_id u32 LE), no outer packet-count container, and prices as `f32` floats — Kite has NO floats on the wire at all (§7). **Compare: Groww** is protobuf over NATS — no hand-rolled binary at all.

## 7. Packet dispatch: length IS the type; segment; price divisors; tradability

**Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` `_parse_binary` lines 719–842; gokiteconnect `parsePacket` lines 654–765 + constants lines 126–130):** there is NO packet-type byte. The parser dispatches on **packet body length**:

| Body length | Meaning | Go constant | Live-doc status (2026-07-14) |
|---|---|---|---|
| 8 bytes | LTP mode (any instrument) | `modeLTPLength = 8` | LIVE-DOC ("8 bytes" in the mode table; "If mode is ltp, the packet ends here" after byte 8) |
| 28 bytes | INDEX instrument, quote mode | `modeQuoteIndexPacketLength = 28` | LIVE-DOC implied (index table: quote "ends here" after bytes 24–28) |
| 32 bytes | INDEX instrument, full mode | `modeFullIndexLength = 32` | LIVE-DOC implied (index table ends with exchange timestamp at 28–32) |
| 44 bytes | tradable instrument, quote mode | `modeQuoteLength = 44` | LIVE-DOC ("44 bytes"; "If mode is quote, the packet ends here" after byte 44) |
| 184 bytes | tradable instrument, full mode | `modeFullLength = 184` | LIVE-DOC ("184 bytes"; depth spans 64–184) |

**Segment extraction (Verified, both SDKs; NOT documented on the live websocket page — the live page names indices only by example, "indices such as NIFTY 50 and SENSEX"):** `segment = instrument_token & 0xFF` — the LOW byte of the 4-byte instrument_token encodes the exchange segment:

| Segment value | Exchange (py `EXCHANGE_MAP`, `ticker.py` lines 359–372 / go consts lines 85–93) |
|---|---|
| 1 | NSE (equity, NseCM) |
| 2 | NFO (NSE F&O) |
| 3 | CDS (NSE currency derivatives) |
| 4 | BSE (equity) |
| 5 | BFO (BSE F&O) |
| 6 | BCD (BSE currency; py alias `bsecds` deprecated) |
| 7 | MCX (commodity) |
| 8 | MCXSX |
| 9 | INDICES (index values — not tradable) |

**Price divisors — LIVE-DOC + SDK, with one flagged discrepancy.** **Verified (LIVE-DOC runner 2026-07-14, #quote-packet-structure preamble), verbatim:** "All prices are in paise. For currencies, the int32 price values should be divided by **10000000** to obtain four decimal plaes [sic]. For everything else, the price values should be divided by **100**." The SDKs implement this by segment (Verified, both SDKs — py lines 728–734, go `convertPrice` lines 769–778); every price field on the wire is an unsigned int32:

| Segment | SDK divisor | Live-doc wording | Decimals |
|---|---|---|---|
| CDS (3) | **10,000,000** | "currencies … divided by 10000000" ✓ | 4 (currency derivatives, NSE) |
| BCD (6) | **10,000** | **⚠ DISCREPANCY** — the live page's blanket "currencies ÷ 10,000,000" would cover BCD too; BOTH SDKs instead divide BCD by 10,000 | 4 (BSE currency) |
| everything else (incl. indices) | **100** | "everything else … divided by 100" ✓ | 2 (paise → rupees) |

**⚠ BCD discrepancy flagged (2026-07-14):** live docs and SDK code disagree on BSE-currency (segment 6) scaling — the docs never mention a 1e4 divisor, but pykiteconnect AND gokiteconnect both special-case BCD at ÷10,000 (independent implementations, so almost certainly reflecting the real wire scaling; the docs sentence is likely NSE-CDS-only shorthand). Recorded BOTH; only a live BCD tick can settle it (99-UNKNOWNS). Irrelevant unless BCD tokens are ever subscribed.

**Tradability (Verified, both SDKs):** `tradable = (segment != 9)` — indices are flagged non-tradable in the emitted tick.

**Dispatch-strictness divergence (Verified, adversarial re-read 2026-07-13):** py `_parse_binary` is a strict `if len==8 / elif len in (28,32) / elif len in (44,184)` chain — any OTHER body length is silently dropped (no else). go `parsePacket` checks 8, then 28||32, then **falls through to the quote layout with NO `len == 44` check** (only `len == 184` upgrades to full) — an unexpected length would be MISPARSED as a quote packet (or panic on slicing if < 44 bytes). Correct for real traffic; a native implementer must copy py's strict-drop behaviour, not go's fall-through.

**Compare: Dhan** carries the segment as an explicit header byte (numeric enum, gap at 6) and the SecurityId separately; Kite folds segment INTO the token's low byte. Dhan prices are IEEE-754 `f32` (precision-lossy above ~8.4M paise); Kite's scaled-int32 encoding is exact to the tick.

## 8. LTP packet — 8 bytes

**Verified (LIVE-DOC runner 2026-07-14, #quote-packet-structure: bytes "0 - 4 int32 instrument_token", "4 - 8 int32 Last traded price (If mode is ltp, the packet ends here)") + Verified (CLIENT-LIB-SOURCE py `ticker.py` lines 740–746; go lines 663–671):**

| Bytes (0-based) | Type | Field |
|---|---|---|
| 0–3 | uint32 BE | instrument_token |
| 4–7 | uint32 BE | last_price (÷ divisor) |

Emitted tick: `{tradable, mode:"ltp", instrument_token, last_price}` — matches OFFICIAL-MOCK `ticker_ltp.json` exactly (`{"mode":"ltp","tradable":true,"instrument_token":408065,"last_price":1573.15}`).

## 9. INDEX packets — 28 bytes (quote) / 32 bytes (full)

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/#index-packet-structure — "The packet structure for indices such as NIFTY 50 and SENSEX differ from that of tradeable instruments. They have fewer fields.") + Verified (CLIENT-LIB-SOURCE py `ticker.py` lines 748–778; go lines 674–701):**

| Bytes (0-based) | Type | Field (live-doc label) |
|---|---|---|
| 0–3 | uint32 BE | instrument_token ("Token"; low byte = 9) |
| 4–7 | uint32 BE | last_price ("Last traded price", ÷ 100) |
| 8–11 | uint32 BE | **high** ("High of the day") |
| 12–15 | uint32 BE | **low** ("Low of the day") |
| 16–19 | uint32 BE | **open** ("Open of the day") |
| 20–23 | uint32 BE | **close** ("Close of the day" — semantically the previous close during live hours; SDKs use it for change computation) |
| 24–27 | uint32 BE | **"Price change (If mode is quote, the packet ends here)"** — official live-doc label; NOT READ by either SDK, see wart below |
| 28–31 (full only) | uint32 BE | exchange_timestamp ("Exchange timestamp", UNIX epoch seconds, §12) |

- **⚠ OHLC ordering wart (Verified, LIVE-DOC + both SDKs agree):** index packets carry **H, L, O, C** while the 44/184-byte quote/full packets carry **O, H, L, C** (§10) — the live docs tables confirm the swap is real, not an SDK quirk. Any hand-rolled parser copying one layout onto the other silently swaps fields.
- **Bytes 24–27 — RESOLVED by live fetch (2026-07-14):** the official label is **"Price change"** (int32; the index QUOTE packet ends after it, hence 28 bytes). The former Assumed is upgraded to Verified (LIVE-DOC). The SDK-vs-doc gap stands and is deliberate: NEITHER SDK reads the wire field — both COMPUTE `change` client-side from `last_price` vs `close` (py lines 764–767, go `NetChange` line 686; NOTE the two SDKs use DIFFERENT formulas — percent vs absolute — see the `change`-semantics table in §12). A native parser may read the wire field, but its scaling (÷100? signed?) is undemonstrated — no mock index packet exists.
- Index packets have NO volume/depth/OI fields in any mode (indices don't trade).
- **Compare: Dhan** delivers index values as ordinary Ticker/Quote packets on segment `IDX_I=0`, plus a separate 16-byte prev-close packet (code 6) that exists ONLY for indices (`docs/dhan-ref/03-live-market-feed-websocket.md`); Kite instead bakes prev-close into every index/quote/full packet as the `close` OHLC field.

## 10. QUOTE packet (tradable instruments) — 44 bytes

**Verified (LIVE-DOC runner 2026-07-14, #quote-packet-structure table — byte-for-byte identical: "8 - 12 Last traded quantity", "12 - 16 Average traded price", "16 - 20 Volume traded for the day", "20 - 24 Total buy quantity", "24 - 28 Total sell quantity", "28 - 32 Open price of the day", "32 - 36 High price of the day", "36 - 40 Low price of the day", "40 - 44 Close price (If mode is quote, the packet ends here)") + Verified (CLIENT-LIB-SOURCE py `ticker.py` lines 780–799; go lines 704–727) + Verified (OFFICIAL-MOCK `ticker_quote.packet` decoded in-sandbox, §12):**

| Bytes (0-based) | Type | Field (mock JSON name — py SDK name where different) |
|---|---|---|
| 0–3 | uint32 BE | instrument_token |
| 4–7 | uint32 BE | last_price (÷ divisor) |
| 8–11 | uint32 BE | last_quantity — py `last_traded_quantity` |
| 12–15 | uint32 BE | average_price (÷ divisor) — py `average_traded_price` |
| 16–19 | uint32 BE | volume (cumulative day volume) — py `volume_traded` |
| 20–23 | uint32 BE | buy_quantity (total buy / bid qty) — py `total_buy_quantity` |
| 24–27 | uint32 BE | sell_quantity (total sell / ask qty) — py `total_sell_quantity` |
| 28–31 | uint32 BE | ohlc.**open** (÷ divisor) |
| 32–35 | uint32 BE | ohlc.**high** (÷ divisor) |
| 36–39 | uint32 BE | ohlc.**low** (÷ divisor) |
| 40–43 | uint32 BE | ohlc.**close** (÷ divisor; PREVIOUS session close — SDKs compute `change` from it; live-doc label is just "Close price") |

- No timestamp of any kind in quote mode (LIVE-DOC confirms: the table's timestamp rows sit AFTER the "packet ends here" marker; the REST `/quote` snapshot has one, the WS quote packet does not).
- Field-name note (corrected 2026-07-14): the live docs table uses descriptive prose labels ("Last traded quantity", "Average traded price", …), the official mock JSON uses the short keys (`last_quantity`, `average_price`, `volume`, `buy_quantity`, `sell_quantity`), and the py SDK emits the longer aliases shown above. Same bytes either way.
- **Compare: Dhan** Quote (code 4, 50B LE) is the closest analog: LTP f32, LTQ u16, LTT, ATP f32, volume u32, sell/buy qty, then day-OHLC where `close` is also the PREVIOUS day's close (Dhan Ticket #5525125) — but Dhan's quote carries LTT while Kite's carries none.

## 11. FULL packet (tradable instruments) — 184 bytes, incl. 5+5 market depth

**Verified (LIVE-DOC runner 2026-07-14, #quote-packet-structure table rows "44 - 48 Last traded timestamp", "48 - 52 Open Interest", "52 - 56 Open Interest Day High", "56 - 60 Open Interest Day Low", "60 - 64 Exchange timestamp", "64 - 184 []byte Market depth entries") + Verified (CLIENT-LIB-SOURCE py `ticker.py` lines 807–840; go lines 730–762) + Verified (OFFICIAL-MOCK `ticker_full.packet` decoded in-sandbox, §12):** bytes 0–43 identical to the quote packet (§10), then:

| Bytes (0-based) | Type | Field |
|---|---|---|
| 44–47 | uint32 BE | last_trade_time ("Last traded timestamp"; UNIX epoch seconds, §12) |
| 48–51 | uint32 BE | oi ("Open Interest") |
| 52–55 | uint32 BE | oi_day_high ("Open Interest Day High") |
| 56–59 | uint32 BE | oi_day_low ("Open Interest Day Low") |
| 60–63 | uint32 BE | exchange_timestamp ("Exchange timestamp"; UNIX epoch seconds, §12) |
| 64–183 | 10 × 12 B | market depth: entries 0–4 = **buy** (bytes 64–123), entries 5–9 = **sell** (bytes 124–183) |

Each 12-byte depth entry — **Verified (LIVE-DOC runner 2026-07-14, #market-depth-structure), verbatim:** "Each market depth entry is a combination of 3 fields, quantity (int32), price (int32), orders (int16) and there is a **2 byte padding at the end (which should be skipped)** totalling to 12 bytes. There are ten entries in succession—five **[64 - 124] bid** entries and five **[124 - 184] offer** entries." Cross-check **Verified** (py loop `range(64, len(packet), 12)` lines 831–836; go `buyPos=64, sellPos=124` lines 739–761):

| Offset within entry | Type | Field |
|---|---|---|
| +0 – +3 | uint32 BE | quantity |
| +4 – +7 | uint32 BE | price (÷ divisor) |
| +8 – +9 | uint16 BE | orders (count of orders at this level) |
| +10 – +11 | — | **2 bytes padding** (live-doc: "should be skipped"; unread by both SDKs; decoded as 0 in the official mock packet) |

- Depth is 5 bid + 5 ask levels ONLY in full mode; quote/ltp modes carry none. `orders` is a 16-bit field — the giant `orders` values in the py docstring example (e.g. 1048576) are pre-v3-rename artifacts of reading 4 bytes; the current code reads `byte_format="H"` (2 bytes) and the mock decodes to sane counts (1–7).
- **ARITH:** 64 + 10×12 = 184 ✓; buy block 64+5×12 = 124 = sell block start ✓ (the two SDKs express the same layout two different ways — py by `i >= 5` on a single 12-stride loop, go by explicit `buyPos`/`sellPos` cursors — and they agree).
- **Compare: Dhan** Full (code 8, 162B LE) also embeds 5-level depth but with 20-byte levels carrying BOTH sides per level (bid qty/ask qty/bid orders/ask orders/bid price/ask price interleaved), f32 prices, plus inline OI-day-high/low only for NSE_FNO (`docs/dhan-ref/03-live-market-feed-websocket.md` rules 10–11). **Compare: Groww** market depth is a separate subscription kind over protobuf (`docs/groww-ref/07-feed-websocket-streaming.md`).

## 12. Timestamp semantics + the in-sandbox mock verification (the empirical proof)

**Verified (OFFICIAL-MOCK + ARITH, 2026-07-13):** `ticker_quote.packet` and `ticker_full.packet` (base64 in the repo, decoding to exactly 44 and 184 bytes — single packet BODIES, no §6 container) were parsed in this sandbox using ONLY the offsets in §10–§11. Every WIRE-DECODED field matched the official paired JSON (the client-DERIVED `change` field is deliberately excluded — see the semantics note below the table):

| Field | Decoded from `.packet` at code offsets | Official `ticker_full.json` |
|---|---|---|
| instrument_token | 408065 (segment 1 = NSE) | 408065 |
| last_price | 1573.70 | 1573.7 |
| last_quantity | 7 | 7 |
| average_price | 1570.37 | 1570.37 |
| volume | 1,192,471 | 1192471 |
| buy_quantity / sell_quantity | 256,443 / 363,009 | 256443 / 363009 |
| O/H/L/C | 1569.15 / 1575.00 / 1561.05 / 1567.80 | same |
| oi / oi_day_high / oi_day_low | 0 / 0 / 0 | same |
| last_trade_time raw | 1625461887 | "2021-07-05T10:41:27+05:30" |
| exchange_timestamp raw | 1625461887 | "2021-07-05T10:41:27+05:30" |
| depth (all 10 levels) | e.g. buy[0] qty=5 price=1573.40 orders=1 | identical, level by level |

(The 44-byte quote packet equally reproduced `ticker_quote.json`: ltp 1573.15, atp 1570.33, vol 1,175,986, etc.)

**⚠ The `change` field — three divergent semantics (adversarial cross-check, 2026-07-13; NOT on the wire, do NOT validate it against the mocks blindly):** `change` is a client-DERIVED field computed from `last_price` vs `close`, and the three official artifacts disagree on the formula:

| Artifact | `change` semantics |
|---|---|
| pykiteconnect (`ticker.py` L767/L804) | **PERCENT**: `(last_price − close) × 100 / close` — both index and quote/full branches |
| gokiteconnect (`ticker.go` `NetChange` L686/L737) | **ABSOLUTE**: `lastPrice − closePrice`; NOT set at all for non-index QUOTE (44 B) ticks (only in the full branch) |
| official mock `ticker_full.json` | `"change": 5.900000000000091` = ABSOLUTE (1573.70 − 1567.80 = 5.9; the py percent formula gives 0.3763) — the mock agrees with go, NOT py |

Bonus mock inconsistency: `ticker_quote.json` carries the SAME `5.900000000000091` even though the quote packet's own values give abs 5.35 / pct 0.3412 — the quote mock's `change` matches NEITHER formula (apparently copy-pasted from the full mock). **Never validate a derived `change` against the quote mock**; a native parser should decode wire fields only and derive `change` per its own documented convention.

**Epoch semantics (ARITH; the live page is SILENT on the epoch anchor — its table just says "Last traded timestamp" / "Exchange timestamp", so this stays mock+ARITH tier):** raw `1625461887` = 2021-07-05 **05:11:27 UTC** = 2021-07-05 **10:41:27 IST**, and the official mock JSON stamps exactly `2021-07-05T10:41:27+05:30`. Therefore Kite's `last_trade_time` (bytes 44–47) and `exchange_timestamp` (bytes 60–63, and 28–31 in index-full) are **STANDARD UNIX epoch seconds (UTC-anchored)** — display in IST by normal timezone conversion. Both SDKs do plain `datetime.fromtimestamp` / `time.Unix` (host-local interpretation; correct on an IST host, a footgun on a UTC host — convert explicitly).

**⚠ Compare: Dhan — the single most dangerous cross-broker difference:** Dhan's WS `LTT` fields are **IST-epoch** seconds (raw value already includes +19800s; subtract 19800 for UTC — `docs/dhan-ref/03-live-market-feed-websocket.md` rules 6/14 and `docs/dhan-ref/08-annexure-enums.md` rule 13). Kite is plain UTC epoch. A parser ported between the two without re-deriving this corrupts every timestamp by 5h30m. **Groww** `tsInMillis` is epoch **milliseconds** (`docs/groww-ref/README.md` [R#27]) — the only one of the three with sub-second resolution.

## 13. Text frames: order updates (postbacks), errors, broker messages

**Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/#postbacks-and-non-binary-updates + #message-types):** "the WebSocket stream delivers postbacks and other updates in the text mode. These messages are JSON encoded … For order Postbacks, the payload is contained in the `data` key and has the same structure described in the Postbacks section." Message shape: `{"type": "order", "data": {}}`. The official message-types table lists THREE types — cross-check **Verified (CLIENT-LIB-SOURCE py `ticker.py` `_parse_text_message` lines 700–717; go `processTextMessage` lines 592–615):**

| `type` | `data` (live-doc description) | SDK handling |
|---|---|---|
| `"order"` | "Order Postback. The data field will contain the full order Postback payload" | `on_order_update` callback |
| `"error"` | "Error responses. The data field contain the error string" | routed to `on_error` with code 0 (py) / `onError` (go) |
| `"message"` | "Messages and alerts from the broker. The data field will contain the message string" — **RESOLVED by live fetch 2026-07-14** (was Unknown) | **silently DROPPED by both SDKs** (neither has a `message` branch) — an SDK-vs-doc gap: a native client should surface it, not copy the SDKs' drop |
| anything else | undocumented | silently ignored by both SDKs |

- The order-update `data` payload shape = the postback payload — **Verified (LIVE-DOC, "same structure described in the Postbacks section") + Verified (OFFICIAL-MOCK `postback.json`, fetched 2026-07-13):** fields include `user_id, unfilled_quantity, app_id, checksum, placed_by, order_id, exchange_order_id, parent_order_id, status ("COMPLETE"), status_message, order_timestamp, …` (the same Order object as the REST order book; `checksum` is postback-HTTP-specific). Order updates for the connected user arrive on the SAME WebSocket as ticks — no separate order-update socket. The live page intro also says "the text messages, alerts, and order updates (the same as the ones available as Postbacks) are also streamed."
- **Compare: Dhan** runs a SEPARATE order-update WebSocket (`wss://api-order-update.dhan.co`, JSON with MsgCode 42 login + single-char field codes — `docs/dhan-ref/10-live-order-update-websocket.md`); Kite multiplexes both onto one socket and needs no login frame at all.

## 14. Keepalive + reconnect behaviour (client-side, informative)

**Verified (CLIENT-LIB-SOURCE):** these are SDK policies, not server contract, but they document what the server tolerates:

- py: client sends a WS **ping every 2.5s**; a checker on a 2.5s timer drops the connection when `last_pong_diff > 2 × PING_INTERVAL` = 5s (`ticker.py` L127) — worst-case detection ≈ 7.5s. (Precision note, adversarial re-read 2026-07-13: `KEEPALIVE_INTERVAL = 5` at L33 is defined but never used; the operative check is the 2×PING_INTERVAL comparison.) Defaults: connect timeout 30s, reconnect exponential backoff capped at 60s (min settable 5s), max 50 retries (hard cap 300) (lines 374–401).
- go: sends NO pings; treats ANY inbound message (incl. the 1-byte heartbeat) as liveness, reconnecting if nothing arrives for 5s (`dataTimeoutInterval`, `ticker.go` lines 146–149, 411–436); backoff `2^attempt` seconds capped at 60s, max 300 attempts, 7s handshake timeout (lines 136–144). **SDK bug (trivia, Verified L194–201):** go's `SetReconnectMaxDelay` has an INVERTED comparison (`if val > reconnectMinDelay { return error }`) — it rejects any max-delay larger than the 5s minimum, making the setter effectively unusable; do not recommend tuning via it.
- Both SDKs re-subscribe + re-set modes from client memory after reconnect (§4).
- **Compare: Dhan** documents server pings every 10s / 40s server-side timeout and forbids manual pings (`docs/dhan-ref/03-live-market-feed-websocket.md` rule 16); Kite's server instead emits the 1-byte data-plane heartbeat and tolerates client pings.

## 15. Three-protocol comparison (Kite vs Dhan vs Groww)

| Aspect | **Kite** (this file) | **Dhan** (`docs/dhan-ref/03-live-market-feed-websocket.md`) | **Groww** (`docs/groww-ref/README.md` [R#27], `07-feed-websocket-streaming.md`) |
|---|---|---|---|
| Transport | raw WS, hand-rolled binary + JSON text | raw WS, hand-rolled binary | NATS protocol over WS, protobuf payloads |
| Endpoint | `wss://ws.kite.trade?api_key&access_token` | `wss://api-feed.dhan.co?version=2&token&clientId&authType=2` | `wss://socket-api.groww.in` (+ per-session socket-token mint) |
| Endianness / numbers | **big-endian**, scaled uint32 ints (÷100 / ÷1e4 / ÷1e7) — exact | **little-endian**, `f32` floats for prices | protobuf varints/doubles |
| Framing | outer u16 packet-count + per-packet u16 length | one packet per unit, fixed 8-byte header (code/len/segment/sid) | NATS MSG framing |
| Packet typing | by BODY LENGTH (8/28/32/44/184) | by response-code byte (2/4/5/6/7/8/50) | by NATS subject + proto type |
| Tick timestamp | UNIX epoch **seconds, UTC** | epoch seconds, **IST-anchored** (−19800 for UTC) | epoch **milliseconds** |
| Prev-day close | `close` field in every quote/full/index packet | inline in Quote/Full for EQ/FNO; separate code-6 packet for indices only | n/a (LTP stream) |
| Depth | 5 bid + 5 ask, 12B entries, full mode only | 5 levels × 20B combined bid+ask, Full packet | separate depth subscription kind |
| OI | full mode: oi + oi_day_high/low (u32) | Full: OI + highest/lowest OI (FNO); separate 12B OI packet in Quote mode | n/a |
| Order updates | same socket, `{"type":"order"}` text frames | SEPARATE WS (`api-order-update.dhan.co`, MsgCode 42) | order/position updates as separate subscription |
| Limits | 3000 instr/conn, 3 conns/api_key (LIVE-DOC 2026-07-14; 3-conn cap is soft/policy-enforced per staff forum answer) | 5000 instr/conn, 5 conns, 100 instr/subscribe-msg | "1000 instruments" (scope unstated) + measured adaptive conn cap |
| Heartbeat | 1-byte binary frame, ~every couple s | server WS ping 10s | NATS PING/PONG |

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **RESOLVED by live fetch (2026-07-14) — index-packet bytes 24–27 official label:** the live index-packet table labels them **"Price change"** (int32; index quote packet ends after it) — §9. Residual (minor, new): the field's scaling/signedness is undocumented and undemonstrated (no mock index packet; both SDKs ignore it and compute change client-side).
- **Default mode after a bare `subscribe` (no `mode` message)** — STILL OPEN after the live fetch: the live page (read in full 2026-07-14) does not state a server default; py SDK assumes `quote`, go SDK assumes nothing. Only a live-wire probe (bare subscribe, measure packet length) can settle it. Matters for a native client that subscribes without setting mode.
- **RESOLVED by live fetch (2026-07-14) — 3000-instruments / 3-connections limits:** official wording confirmed verbatim on the live page, and a Dec 2025 Zerodha staff forum answer (thread 15708, also in the live bundle) confirms enforcement: >3000 → error for the additional tokens, connection unaffected; the 3-connection cap is a SOFT limit (flag/suspension on abuse) — §2. Residual (minor): the exact wire shape of the over-limit error frame is still unshown.
- **BCD (BSE currency) divisor — NARROWED but still open (2026-07-14):** the live divisor paragraph is now in hand and does NOT mention 1e4 — its blanket "currencies ÷ 10,000,000" nominally contradicts both SDKs' BCD ÷ 10,000 special case (discrepancy recorded in §7). Only a live BCD tick settles which is right on the wire. Matters only if BCD tokens are ever subscribed.
- **RESOLVED by live fetch (2026-07-14) — other text-frame `type` values:** the live message-types table documents a third type **`message`** ("Messages and alerts from the broker", string `data`) — §13. Both official SDKs silently drop it; a native client should surface it.
- **Heartbeat exact cadence** — NARROWED by live fetch (2026-07-14): official wording confirmed ("1 byte 'heartbeat' every couple seconds", sent when there is no data to stream); the exact number of seconds remains unstated — a native stall watchdog threshold still needs an empirical measurement in the first live session.
- **MCXSX (segment 8) liveness** — present in both SDK segment maps; the exchange itself is defunct (MCX-SX became MSEI); the live websocket page doesn't enumerate segments. Unknown whether tokens with this segment still occur in the instruments dump. Cosmetic.
- **Index `close` field label vs semantics (new, minor, 2026-07-14):** the live index table labels bytes 20–23 "Close of the day" while both SDKs (and the tradable-packet convention) treat `close` as the PREVIOUS session close for change computation — during live hours the exchange "close of the day" IS the prior close, so no contradiction is asserted; flag only if a native parser needs post-close semantics.

## CLAIMS (for README reconciled table)

- WS endpoint `wss://ws.kite.trade?api_key=<key>&access_token=<token>`; the two query params are the whole connect-time auth, no in-band auth frame — **Verified (LIVE-DOC runner 2026-07-14, https://kite.trade/docs/connect/v3/websocket/#connecting-to-the-websocket-endpoint) + Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` L382/L443–448 + gokiteconnect `ticker.go` L154/L289–293)**
- Client messages are text JSON `{"a": "subscribe"|"unsubscribe"|"mode", "v": …}` with NUMERIC instrument tokens; mode message `v = [mode, [tokens]]` — **Verified (LIVE-DOC runner 2026-07-14, #request-structure action table + JS examples) + Verified (CLIENT-LIB-SOURCE both SDKs)** — ticker.py L567–628 / ticker.go L489–552
- Modes are exactly `ltp` (8 B) / `quote` (44 B) / `full` (184 B) for tradable instruments — **Verified (LIVE-DOC runner 2026-07-14, #modes table) + Verified (CLIENT-LIB-SOURCE both SDKs)**
- Binary framing: u16 packet count, then per-packet u16 length + body — **Verified (LIVE-DOC runner 2026-07-14, #message-structure table) + Verified (CLIENT-LIB-SOURCE both SDKs)**; big-endian + unsigned is **CLIENT-LIB-SOURCE + OFFICIAL-MOCK only** (⚠ corrected 2026-07-14: the live page never states endianness and names the types "int16"/"int32" — the earlier "big endian" docs-corroboration sentence is NOT on the live page and was withdrawn; the real mock packet decodes correctly only as BE unsigned)
- Packet type is dispatched by BODY LENGTH: 8 (ltp) / 28 (index quote) / 32 (index full) / 44 (quote) / 184 (full) — **Verified (LIVE-DOC runner 2026-07-14 — 8/44/184 explicit in the mode table, 28/32 implied by the index table's end markers) + Verified (CLIENT-LIB-SOURCE both SDKs, go named constants L126–130)**
- Segment = `instrument_token & 0xFF` (1 NSE … 9 INDICES); indices not tradable — **Verified (CLIENT-LIB-SOURCE both SDKs)**; not on the live websocket page — ticker.py L359–372/L726/L737, ticker.go L85–93/L656–659
- Price divisors: "All prices are in paise", currencies ÷1e7, everything else ÷100 — **Verified (LIVE-DOC runner 2026-07-14, #quote-packet-structure preamble) + Verified (CLIENT-LIB-SOURCE both SDKs)**; ⚠ BCD (segment 6) ÷1e4 is **CLIENT-LIB-SOURCE only and DISAGREES with the live blanket "currencies ÷ 1e7"** — both recorded, discrepancy flagged (§7), live-wire probe needed
- Quote packet (44B) layout: token/ltp/ltq/atp/volume/buyq/sellq at 0–27, then OHLC as O,H,L,C at 28–43; quote mode carries NO timestamp; `close` = previous session close (live label "Close price") — **Verified (LIVE-DOC runner 2026-07-14, #quote-packet-structure table, byte-for-byte) + Verified (CLIENT-LIB-SOURCE both SDKs) + Verified (OFFICIAL-MOCK `ticker_quote.packet` decoded in-sandbox, every wire-decoded field matching; the derived `change` excluded — §12 note)**
- Full packet (184B) adds last_trade_time@44, oi@48, oi_day_high@52, oi_day_low@56, exchange_timestamp@60, then 10 × 12-byte depth entries (5 bid @64–124, 5 offer @124–184; qty int32 + price int32 + orders int16 + 2B padding "which should be skipped") — **Verified (LIVE-DOC runner 2026-07-14, #quote-packet-structure + #market-depth-structure) + Verified (CLIENT-LIB-SOURCE both SDKs) + Verified (OFFICIAL-MOCK `ticker_full.packet` decoded in-sandbox)**
- Index packets order OHLC as **H,L,O,C** (bytes 8–24) — opposite of the quote packet's O,H,L,C; bytes 24–27 = **"Price change"** (official live label; index quote ends at 28, full adds exchange timestamp at 28–32); both SDKs leave the wire field unread and compute change client-side — **Verified (LIVE-DOC runner 2026-07-14, #index-packet-structure table) + Verified (CLIENT-LIB-SOURCE both SDKs)** — the 24–27 label was Assumed, now RESOLVED
- Kite WS timestamps are STANDARD UNIX epoch seconds (UTC-anchored): mock raw 1625461887 ↔ official "2021-07-05T10:41:27+05:30" — **Verified (OFFICIAL-MOCK `ticker_full.packet`+`ticker_full.json`) + ARITH** (live page silent on the epoch anchor); contrast Dhan's IST-anchored epoch (repo: `docs/dhan-ref/03-live-market-feed-websocket.md`)
- Heartbeat = 1-byte binary frame "every couple seconds", sent when there is no data to stream, "can be safely ignored"; both SDKs ignore binary frames <2 bytes — **Verified (LIVE-DOC runner 2026-07-14, #modes note) + Verified (CLIENT-LIB-SOURCE, the <2B guard)**
- Limits: up to 3000 instruments per WS connection; up to 3 WS connections per api_key (≈9000 total, ARITH); >3000 → error for the extra tokens with the connection unaffected; the 3-conn cap is soft/policy-enforced — **Verified (LIVE-DOC runner 2026-07-14, docs page verbatim + forum thread 15708 staff answer Dec 2025, both in the fetch-log)** — UPGRADED from SEARCH
- Text frames on the SAME socket carry `{"type":"order"|"error"|"message","data":…}` — order = full postback-shaped Order payload, error = error string, `message` = broker messages/alerts (string) — **Verified (LIVE-DOC runner 2026-07-14, #message-types table) + Verified (CLIENT-LIB-SOURCE both SDKs for order/error) + Verified (OFFICIAL-MOCK `postback.json`)**; ⚠ both SDKs silently drop `type:"message"` (SDK-vs-doc gap)
- SDK keepalive policies: py pings every 2.5s / drops on >5s pong silence; go passively reconnects after 5s of message silence; both replay subscriptions client-side on reconnect — **Verified (CLIENT-LIB-SOURCE)**; the live page documents no server-side keepalive contract beyond the heartbeat
