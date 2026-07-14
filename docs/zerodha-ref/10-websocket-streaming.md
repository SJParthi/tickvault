# Zerodha Kite Connect v3 — WebSocket Streaming (`wss://ws.kite.trade`) — THE feed-decision file

> **Source:** `https://kite.trade/docs/connect/v3/websocket/` (live official page — NOT directly fetchable from this sandbox; its `#quote-packet-structure` anchor is confirmed to exist via the pykiteconnect changelog, see §0)
> **Fetched-Verified:** 2026-07-13 via CLIENT-LIB-SOURCE (pykiteconnect@master `kiteconnect/ticker.py`, 863 lines, full binary parser; gokiteconnect@master `ticker/ticker.go`, 779 lines, independent cross-check) + OFFICIAL-MOCK (zerodha/kiteconnect-mocks@main `ticker_ltp.json` / `ticker_quote.json` / `ticker_quote.packet` / `ticker_full.json` / `ticker_full.packet` / `postback.json` — the two `.packet` files are base64-encoded REAL wire packet bodies, decoded and re-parsed in this sandbox at the code-derived offsets, matching the paired JSON on every WIRE-DECODED field — the derived `change` field is excluded, see §12 note) + SEARCH (search-backend extraction of the live docs page + kite.trade forum threads, for the two doc-only facts: the 1-byte heartbeat cadence and the 3000-instruments / 3-connections limits) + ARITH (epoch-semantics derivation from the mock timestamps). Direct kite.trade fetch, web.archive.org, and r.jina.ai were ALL proxy-blocked (CONNECT 403) — no ARCHIVE-DOC or MIRROR-LIVE tier was attainable for this file (see `zerodha-probe`).
> **Evidence tiers:** see README legend — Verified (CLIENT-LIB-SOURCE …) / Verified (OFFICIAL-MOCK …) / SEARCH / ARITH / Assumed / Unknown.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. Adding Zerodha as a live feed requires its own dated operator scope lock (the `gdf-third-feed-scope-2026-07-13.md` / `groww-second-feed-scope-2026-06-19.md` pattern) FIRST.
> **Related:** `07-market-quotes.md` (REST snapshot twin of the quote fields), `02-rest-conventions-errors-and-rate-limits.md`, `14-option-chain-composition.md` §5 (India-F&O/BFO coverage note — SENSEX/BFO live-tick delivery is a day-0 probe), `99-UNKNOWNS.md`.

---

## 0. Provenance note — why this file can still be high-confidence with the docs page blocked

The two official SDKs (Python + Go, both © Zerodha) each contain a COMPLETE,
independent implementation of the binary protocol, and they agree byte-for-byte
on every offset, divisor, and packet length. On top of that, the official
`kiteconnect-mocks` repo ships two REAL captured wire packets
(`ticker_quote.packet`, `ticker_full.packet`, base64-encoded) alongside the
expected parsed JSON — decoded in this sandbox, every WIRE-DECODED field at the
code-derived offsets reproduces the official JSON exactly (§12; the client-derived
`change` field is excluded — its semantics diverge three ways, see the §12 note). So the packet structure
below is effectively triple-sourced. Only two facts in this file rest solely on
SEARCH extraction of the (blocked) docs page: the heartbeat cadence wording and
the connection-count/instrument-count limits.

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `README.md` changelog):** the
official docs page carries a `#quote-packet-structure` anchor — the changelog
entry reads "Renamed ticker fields as per [kite connect doc]
(https://kite.trade/docs/connect/v3/websocket/#quote-packet-structure)"
(https://raw.githubusercontent.com/zerodha/pykiteconnect/master/README.md,
fetched 2026-07-13). This also means the SDK field names were deliberately
aligned to the docs' packet tables.

## 1. Endpoint + authentication

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master `kiteconnect/ticker.py` lines 382, 443–448; fetched 2026-07-13, https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/ticker.py):**

```
ROOT_URI = "wss://ws.kite.trade"
socket_url = "{root}?api_key={api_key}&access_token={access_token}"
```

Cross-check **Verified (CLIENT-LIB-SOURCE gokiteconnect@master `ticker/ticker.go` lines 152–155, 289–293):** `tickerURL = url.URL{Scheme: "wss", Host: "ws.kite.trade"}` with `q.Set("api_key", …)` + `q.Set("access_token", …)`.

- Auth is **entirely in the URL query string** — `api_key` + `access_token` (the token minted by the login flow, see `01-overview-auth-and-token-lifecycle.md`). No auth frame after connect.
- **Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` lines 505–507):** the Python SDK additionally sends an HTTP header `X-Kite-Version: 3` on the WS handshake ("For version 3"). The Go SDK sends NO extra header on its dial (`d.Dial(t.url.String(), nil)`) and works — so the header appears optional on the WS surface; **Assumed:** the `?api_key&access_token` query params alone select v3 behaviour. (The REST surface, by contrast, requires the header — `02-rest-conventions…`.)
- **Compare: Dhan** puts the token in the WS URL too (`wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` — `docs/dhan-ref/03-live-market-feed-websocket.md`); **Groww** does in-band NATS CONNECT auth with a per-session socket token (`docs/groww-ref/README.md` [R#27]).

## 2. Connection limits (SEARCH tier — doc/forum only, re-verify live)

**SEARCH (search-backend extraction of https://kite.trade/docs/connect/v3/websocket/ + kite.trade forum threads incl. https://kite.trade/forum/discussion/15708/ and /11179/, fetched 2026-07-13):**

- "You can subscribe for up to **3000 instruments** on a single WebSocket connection and receive live quotes for them."
- "Single API key can have upto **3 websocket connections**" (forum wording: "a soft limit of up to 3 WebSocket connections per API account") → ≈ 9,000 instruments max per api_key (**ARITH:** 3 × 3000).
- Forum-reported behaviour on exceeding 3000: the over-limit subscription errors while the existing connection stays up (forum thread 5271 title: "Error: Can't subscribe to more than 3000 instruments."). Wire shape of that error: **Unknown** (presumably a `type: "error"` text frame, §13 — not sourced).

Neither number appears anywhere in either SDK source (grep-verified over `ticker.py` + `ticker.go`) — these are server-side limits documented only on the (blocked) docs page. **Treat both as re-verify-on-live-doc.**

**Compare: Dhan** allows 5 WS connections × 5,000 instruments (batches of 100 per subscribe message) — `docs/dhan-ref/03-live-market-feed-websocket.md`. **Groww** documents "up to 1000 instruments at a time" with per-connection-vs-per-account scope unstated, and MEASURABLY runs an undocumented adaptive connection cap — `docs/groww-ref/README.md` [R#7], [R#23].

## 3. The two channels: binary frames (ticks) vs text frames (everything else)

**Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` `_on_message` lines 668–679 + gokiteconnect `readMessage` lines 458–471):**

| Frame type | Content | Handling |
|---|---|---|
| **Binary** | one or more quote packets (the multi-packet container of §6) | parsed by `_parse_binary` / `parseBinary` |
| **Binary, length < 2 bytes** | **heartbeat** | ignored by both SDKs (`if len(bin) < 2: return []`) |
| **Text (UTF-8 JSON)** | order updates (postbacks) + error messages | parsed by `_parse_text_message` / `processTextMessage` (§13) |

- Heartbeat cadence — **SEARCH (docs-page extraction, 2026-07-13):** "The API sends a **1 byte 'heartbeat' every couple seconds** to keep the connection alive, and this can be safely ignored." The SDKs' `< 2 bytes` guard is the code-level confirmation that a 1-byte binary frame is expected traffic.
- The Python SDK only attempts tick-parsing on binary payloads `> 4` bytes (`ticker.py` line 674) — anything 2–4 bytes would be silently dropped; no such frame is documented. **Robustness wart (Verified, adversarial re-read 2026-07-13):** the Go SDK has NO such gate — a malformed 2–4-byte binary frame with a nonzero packet count would panic `splitPackets`' slicing. A native parser should copy py's strict guards, not go's.
- **Compare: Dhan** has no heartbeat packet in the data plane (WS-level ping/pong every 10s, handled by the WS library); **Groww** uses NATS `PING`/`PONG` protocol frames plus server-push messages.

## 4. Client → server messages (text JSON)

**Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` lines 392–396, 567–628 + gokiteconnect `ticker.go` lines 489–552):** all client messages are TEXT frames of the shape `{"a": <action>, "v": <value>}`:

| Action | Message | Notes |
|---|---|---|
| Subscribe | `{"a": "subscribe", "v": [738561, 5633]}` | `v` = array of NUMERIC instrument_tokens (from the instruments CSV) |
| Unsubscribe | `{"a": "unsubscribe", "v": [738561]}` | |
| Set mode | `{"a": "mode", "v": ["full", [738561]]}` | `v` = 2-element array: mode string, then token array |

- Tokens are numbers on the wire, NOT strings (both SDKs marshal plain ints / `[]uint32`). **Compare: Dhan** requires STRING SecurityIds in its subscribe JSON (`docs/dhan-ref/03-live-market-feed-websocket.md` rule 15) — the exact opposite convention.
- **Default mode after bare subscribe:** the Python SDK bookkeeps freshly-subscribed tokens as `quote` mode (`ticker.py` line 579), while the Go SDK records them as `modeEmpty` and never assumes. **Assumed (from the py SDK's bookkeeping + the mock naming):** the server default mode on subscribe is `quote` — the docs page presumably states this; re-verify (99-UNKNOWNS).
- There is a `_message_code = 11` constant in `ticker.py` (line 393) that is **never used anywhere** in the file — a vestige; no numeric message codes exist in the v3 protocol.
- Resubscribe-after-reconnect is CLIENT-side: both SDKs replay `subscribe` + `mode` for all tracked tokens on reconnect (`ticker.py` `resubscribe()` lines 630–647; `ticker.go` `Resubscribe()` lines 555–590). The server does NOT restore subscriptions.

## 5. The three streaming modes

**Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` lines 384–387 + gokiteconnect `ticker.go` lines 95–100):** `ltp`, `quote`, `full` — set per-token via the `mode` message. Mode determines the packet SIZE the server emits for that token (§7). Mode strings on the wire are exactly `"ltp"`, `"quote"`, `"full"`.

## 6. Binary message framing (the multi-packet container)

**Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` `_split_packets` lines 848–863 + `_unpack_int` lines 844–846; gokiteconnect `splitPackets` lines 635–651):**

```
byte 0-1   : int16 (big-endian, unsigned)  number of packets in this message
per packet:
  2 bytes  : int16 (big-endian, unsigned)  length of the following packet body
  N bytes  : packet body (N = the length just read)
```

- **ALL integers in the protocol are BIG-ENDIAN unsigned** — `struct.unpack(">I"/">H")` in Python, `binary.BigEndian.Uint32/Uint16` in Go. Corroborated by SEARCH (docs extraction 2026-07-13): "The data is transmitted in big endian format."
- One binary WS message batches ticks for MANY instruments (each with its own length prefix).
- **Compare: Dhan** is the opposite: LITTLE-endian, one fixed 8-byte header per packet (code u8, length u16 LE, segment u8, security_id u32 LE), no outer packet-count container, and prices as `f32` floats — Kite has NO floats on the wire at all (§7). **Compare: Groww** is protobuf over NATS — no hand-rolled binary at all.

## 7. Packet dispatch: length IS the type; segment; price divisors; tradability

**Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` `_parse_binary` lines 719–842; gokiteconnect `parsePacket` lines 654–765 + constants lines 126–130):** there is NO packet-type byte. The parser dispatches on **packet body length**:

| Body length | Meaning | Go constant |
|---|---|---|
| 8 bytes | LTP mode (any instrument) | `modeLTPLength = 8` |
| 28 bytes | INDEX instrument, quote mode | `modeQuoteIndexPacketLength = 28` |
| 32 bytes | INDEX instrument, full mode | `modeFullIndexLength = 32` |
| 44 bytes | tradable instrument, quote mode | `modeQuoteLength = 44` |
| 184 bytes | tradable instrument, full mode | `modeFullLength = 184` |

**Segment extraction (Verified, both SDKs):** `segment = instrument_token & 0xFF` — the LOW byte of the 4-byte instrument_token encodes the exchange segment:

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

**Price divisors (Verified, both SDKs — py lines 728–734, go `convertPrice` lines 769–778):** every price field on the wire is an unsigned int32; convert by segment:

| Segment | Divisor | Decimals |
|---|---|---|
| CDS (3) | **10,000,000** | 4 (currency derivatives, NSE) |
| BCD (6) | **10,000** | 4 (BSE currency) |
| everything else (incl. indices) | **100** | 2 (paise → rupees) |

SEARCH corroboration (docs extraction 2026-07-13): "All prices are in paise … for currencies, the int32 price values should be divided by 10000000 to obtain four decimal places." Note the docs snippet only surfaces the 1e7 divisor; the BCD 1e4 divisor is SDK-sourced (both SDKs, consistent) — re-verify the BCD wording against the live docs table.

**Tradability (Verified, both SDKs):** `tradable = (segment != 9)` — indices are flagged non-tradable in the emitted tick.

**Dispatch-strictness divergence (Verified, adversarial re-read 2026-07-13):** py `_parse_binary` is a strict `if len==8 / elif len in (28,32) / elif len in (44,184)` chain — any OTHER body length is silently dropped (no else). go `parsePacket` checks 8, then 28||32, then **falls through to the quote layout with NO `len == 44` check** (only `len == 184` upgrades to full) — an unexpected length would be MISPARSED as a quote packet (or panic on slicing if < 44 bytes). Correct for real traffic; a native implementer must copy py's strict-drop behaviour, not go's fall-through.

**Compare: Dhan** carries the segment as an explicit header byte (numeric enum, gap at 6) and the SecurityId separately; Kite folds segment INTO the token's low byte. Dhan prices are IEEE-754 `f32` (precision-lossy above ~8.4M paise); Kite's scaled-int32 encoding is exact to the tick.

## 8. LTP packet — 8 bytes

**Verified (CLIENT-LIB-SOURCE py `ticker.py` lines 740–746; go lines 663–671):**

| Bytes (0-based) | Type | Field |
|---|---|---|
| 0–3 | uint32 BE | instrument_token |
| 4–7 | uint32 BE | last_price (÷ divisor) |

Emitted tick: `{tradable, mode:"ltp", instrument_token, last_price}` — matches OFFICIAL-MOCK `ticker_ltp.json` exactly (`{"mode":"ltp","tradable":true,"instrument_token":408065,"last_price":1573.15}`).

## 9. INDEX packets — 28 bytes (quote) / 32 bytes (full)

**Verified (CLIENT-LIB-SOURCE py `ticker.py` lines 748–778; go lines 674–701):**

| Bytes (0-based) | Type | Field |
|---|---|---|
| 0–3 | uint32 BE | instrument_token (low byte = 9) |
| 4–7 | uint32 BE | last_price (÷ 100) |
| 8–11 | uint32 BE | **high** |
| 12–15 | uint32 BE | **low** |
| 16–19 | uint32 BE | **open** |
| 20–23 | uint32 BE | **close** (previous close — used for change computation) |
| 24–27 | uint32 BE | **NOT READ by either SDK** — see wart below |
| 28–31 (full only) | uint32 BE | exchange_timestamp (UNIX epoch seconds, §12) |

- **⚠ OHLC ordering wart (Verified, both SDKs agree):** index packets carry **H, L, O, C** while the 44/184-byte quote/full packets carry **O, H, L, C** (§10). Any hand-rolled parser copying one layout onto the other silently swaps fields.
- **Bytes 24–27 wart (ARITH + Assumed):** a 28-byte quote packet has 4 bytes (24–27) after `close` that NEITHER SDK reads — both COMPUTE `change` client-side from `last_price` vs `close` (py lines 764–767, go `NetChange` line 686; NOTE the two SDKs use DIFFERENT formulas — percent vs absolute — see the `change`-semantics table in §12). **Assumed:** the docs table labels bytes 24–27 as the price-change field and the SDKs ignore it in favour of computing it; **Unknown** until the live docs table is read (99-UNKNOWNS).
- Index packets have NO volume/depth/OI fields in any mode (indices don't trade).
- **Compare: Dhan** delivers index values as ordinary Ticker/Quote packets on segment `IDX_I=0`, plus a separate 16-byte prev-close packet (code 6) that exists ONLY for indices (`docs/dhan-ref/03-live-market-feed-websocket.md`); Kite instead bakes prev-close into every index/quote/full packet as the `close` OHLC field.

## 10. QUOTE packet (tradable instruments) — 44 bytes

**Verified (CLIENT-LIB-SOURCE py `ticker.py` lines 780–799; go lines 704–727) + Verified (OFFICIAL-MOCK `ticker_quote.packet` decoded in-sandbox, §12):**

| Bytes (0-based) | Type | Field (docs/mock name — py SDK name where different) |
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
| 40–43 | uint32 BE | ohlc.**close** (÷ divisor; PREVIOUS session close — SDKs compute `change` from it) |

- No timestamp of any kind in quote mode (the REST `/quote` snapshot has one; the WS quote packet does not).
- Field-name note: the official mock JSON (and per the changelog, the docs tables) use the short names (`last_quantity`, `average_price`, `volume`, `buy_quantity`, `sell_quantity`); the py SDK emits the longer aliases shown above. Same bytes either way.
- **Compare: Dhan** Quote (code 4, 50B LE) is the closest analog: LTP f32, LTQ u16, LTT, ATP f32, volume u32, sell/buy qty, then day-OHLC where `close` is also the PREVIOUS day's close (Dhan Ticket #5525125) — but Dhan's quote carries LTT while Kite's carries none.

## 11. FULL packet (tradable instruments) — 184 bytes, incl. 5+5 market depth

**Verified (CLIENT-LIB-SOURCE py `ticker.py` lines 807–840; go lines 730–762) + Verified (OFFICIAL-MOCK `ticker_full.packet` decoded in-sandbox, §12):** bytes 0–43 identical to the quote packet (§10), then:

| Bytes (0-based) | Type | Field |
|---|---|---|
| 44–47 | uint32 BE | last_trade_time (UNIX epoch seconds, §12) |
| 48–51 | uint32 BE | oi (open interest) |
| 52–55 | uint32 BE | oi_day_high |
| 56–59 | uint32 BE | oi_day_low |
| 60–63 | uint32 BE | exchange_timestamp (UNIX epoch seconds, §12) |
| 64–183 | 10 × 12 B | market depth: entries 0–4 = **buy** (bytes 64–123), entries 5–9 = **sell** (bytes 124–183) |

Each 12-byte depth entry (**Verified**, py loop `range(64, len(packet), 12)` lines 831–836; go `buyPos=64, sellPos=124` lines 739–761):

| Offset within entry | Type | Field |
|---|---|---|
| +0 – +3 | uint32 BE | quantity |
| +4 – +7 | uint32 BE | price (÷ divisor) |
| +8 – +9 | uint16 BE | orders (count of orders at this level) |
| +10 – +11 | — | **2 bytes padding** (unread by both SDKs; decoded as 0 in the official mock packet) |

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

**Epoch semantics (ARITH):** raw `1625461887` = 2021-07-05 **05:11:27 UTC** = 2021-07-05 **10:41:27 IST**, and the official mock JSON stamps exactly `2021-07-05T10:41:27+05:30`. Therefore Kite's `last_trade_time` (bytes 44–47) and `exchange_timestamp` (bytes 60–63, and 28–31 in index-full) are **STANDARD UNIX epoch seconds (UTC-anchored)** — display in IST by normal timezone conversion. Both SDKs do plain `datetime.fromtimestamp` / `time.Unix` (host-local interpretation; correct on an IST host, a footgun on a UTC host — convert explicitly).

**⚠ Compare: Dhan — the single most dangerous cross-broker difference:** Dhan's WS `LTT` fields are **IST-epoch** seconds (raw value already includes +19800s; subtract 19800 for UTC — `docs/dhan-ref/03-live-market-feed-websocket.md` rules 6/14 and `docs/dhan-ref/08-annexure-enums.md` rule 13). Kite is plain UTC epoch. A parser ported between the two without re-deriving this corrupts every timestamp by 5h30m. **Groww** `tsInMillis` is epoch **milliseconds** (`docs/groww-ref/README.md` [R#27]) — the only one of the three with sub-second resolution.

## 13. Text frames: order updates (postbacks) + errors

**Verified (CLIENT-LIB-SOURCE py `ticker.py` `_parse_text_message` lines 700–717; go `processTextMessage` lines 592–615):** text frames are JSON `{"type": …, "data": …}`:

| `type` | `data` | SDK handling |
|---|---|---|
| `"order"` | full order object (the postback payload) | `on_order_update` callback |
| `"error"` | error string | routed to `on_error` with code 0 (py) / `onError` (go) |
| anything else | — | silently ignored by both SDKs; other types (e.g. broadcast messages) **Unknown** |

- The order-update `data` payload shape = the postback payload — **Verified (OFFICIAL-MOCK `postback.json`, fetched 2026-07-13):** fields include `user_id, unfilled_quantity, app_id, checksum, placed_by, order_id, exchange_order_id, parent_order_id, status ("COMPLETE"), status_message, order_timestamp, …` (the same Order object as the REST order book; `checksum` is postback-HTTP-specific). Order updates for the connected user arrive on the SAME WebSocket as ticks — no separate order-update socket.
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
| Limits | 3000 instr/conn, 3 conns/api_key (SEARCH) | 5000 instr/conn, 5 conns, 100 instr/subscribe-msg | "1000 instruments" (scope unstated) + measured adaptive conn cap |
| Heartbeat | 1-byte binary frame, ~every couple s | server WS ping 10s | NATS PING/PONG |

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Index-packet bytes 24–27 official label** — neither SDK reads them; presumed "price change". Operator: open `https://kite.trade/docs/connect/v3/websocket/#quote-packet-structure`, paste the index-packet table. Matters if we ever parse index packets natively (field must be named, not guessed).
- **Default mode after a bare `subscribe` (no `mode` message)** — py SDK assumes `quote`, go SDK assumes nothing. Operator: paste the "modes" paragraph from `https://kite.trade/docs/connect/v3/websocket/`. Matters for a native client that subscribes without setting mode.
- **3000-instruments / 3-connections limits: current official wording + enforcement behaviour** (error frame shape on the 3001st token; is the 3-connection cap hard or "soft"?) — SEARCH-tier only here. Operator: paste the limits paragraph from `https://kite.trade/docs/connect/v3/websocket/` + any current rate note on `https://kite.trade/docs/connect/v3/`. Feed-capacity-decision blocking.
- **BCD (BSE currency) divisor 10,000** — SDK-sourced only; the docs snippet surfaced only the 1e7 currency divisor. Operator: paste the "price conversion"/divisor paragraph from the websocket docs page. Matters for exact price decoding on BCD tokens (if ever subscribed).
- **Other text-frame `type` values** (e.g. broadcast/admin "message" type) — both SDKs ignore unknown types. Operator: paste the "Postbacks and non-binary updates" section of the websocket docs page. Matters for not silently dropping operator-relevant notices.
- **Heartbeat exact cadence + whether tick-silence beyond N seconds implies a dead socket** — "every couple seconds" is SEARCH wording; a native stall watchdog threshold needs the real number (or an empirical measurement in the first live session).
- **MCXSX (segment 8) liveness** — present in both SDK segment maps; the exchange itself is defunct (MCX-SX became MSEI). Unknown whether tokens with this segment still occur in the instruments dump. Cosmetic.

## CLAIMS (for README reconciled table)

- WS endpoint `wss://ws.kite.trade?api_key=<key>&access_token=<token>`; no in-band auth frame — **Verified (CLIENT-LIB-SOURCE pykiteconnect `ticker.py` L382/L443–448 + gokiteconnect `ticker.go` L154/L289–293)** — https://raw.githubusercontent.com/zerodha/pykiteconnect/master/kiteconnect/ticker.py
- Client messages are text JSON `{"a": "subscribe"|"unsubscribe"|"mode", "v": …}` with NUMERIC instrument tokens; mode message `v = [mode, [tokens]]` — **Verified (CLIENT-LIB-SOURCE both SDKs)** — ticker.py L567–628 / ticker.go L489–552
- Binary framing: u16 BE packet count, then per-packet u16 BE length + body; ALL protocol ints big-endian unsigned — **Verified (CLIENT-LIB-SOURCE both SDKs; SEARCH corroboration)** — ticker.py L844–863 / ticker.go L635–651
- Packet type is dispatched by BODY LENGTH: 8 (ltp) / 28 (index quote) / 32 (index full) / 44 (quote) / 184 (full) — **Verified (CLIENT-LIB-SOURCE both SDKs, go named constants L126–130)**
- Segment = `instrument_token & 0xFF` (1 NSE … 9 INDICES); indices not tradable — **Verified (CLIENT-LIB-SOURCE both SDKs)** — ticker.py L359–372/L726/L737, ticker.go L85–93/L656–659
- Price divisors: CDS ÷1e7, BCD ÷1e4, all else ÷100 (scaled uint32, no floats on the wire) — **Verified (CLIENT-LIB-SOURCE both SDKs)** + partial SEARCH corroboration (÷100 paise, ÷1e7 currencies) — ticker.py L728–734, ticker.go L769–778
- Quote packet (44B) layout: token/ltp/ltq/atp/volume/buyq/sellq at 0–27, then OHLC as O,H,L,C at 28–43; `close` = previous session close — **Verified (CLIENT-LIB-SOURCE both SDKs) + Verified (OFFICIAL-MOCK `ticker_quote.packet` decoded in-sandbox, every wire-decoded field matching; the derived `change` excluded — §12 note)**
- Full packet (184B) adds last_trade_time@44, oi@48, oi_day_high@52, oi_day_low@56, exchange_timestamp@60, then 10 × 12-byte depth entries (5 buy @64, 5 sell @124; qty u32 + price u32 + orders u16 + 2B padding) — **Verified (CLIENT-LIB-SOURCE both SDKs) + Verified (OFFICIAL-MOCK `ticker_full.packet` decoded in-sandbox)**
- Index packets order OHLC as **H,L,O,C** (bytes 8–24) — opposite of the quote packet's O,H,L,C; bytes 24–27 unread by both SDKs — **Verified (CLIENT-LIB-SOURCE both SDKs)**; the 24–27 label is Assumed/Unknown
- Kite WS timestamps are STANDARD UNIX epoch seconds (UTC-anchored): mock raw 1625461887 ↔ official "2021-07-05T10:41:27+05:30" — **Verified (OFFICIAL-MOCK `ticker_full.packet`+`ticker_full.json`) + ARITH**; contrast Dhan's IST-anchored epoch (repo: `docs/dhan-ref/03-live-market-feed-websocket.md`)
- Heartbeat = 1-byte binary frame "every couple seconds"; both SDKs ignore binary frames <2 bytes — **Verified (CLIENT-LIB-SOURCE, the <2B guard) + SEARCH (cadence wording, docs-page extraction 2026-07-13)** — https://kite.trade/docs/connect/v3/websocket/
- Limits: up to 3000 instruments per connection; up to 3 WS connections per api_key (≈9000 total, ARITH) — **SEARCH (docs-page + forum extraction 2026-07-13, kite.trade/forum threads 15708/11179/5271)** — re-verify against the live page before any capacity decision
- Order updates + errors arrive as TEXT frames `{"type":"order"|"error","data":…}` on the SAME socket (postback-shaped Order payload) — **Verified (CLIENT-LIB-SOURCE both SDKs) + Verified (OFFICIAL-MOCK `postback.json`)**
- SDK keepalive policies: py pings every 2.5s / drops on >5s pong silence; go passively reconnects after 5s of message silence; both replay subscriptions client-side on reconnect — **Verified (CLIENT-LIB-SOURCE)**
