# Groww LIVE-Feed Request/Response Mapping — VERIFIED against the official SDK

> **Status:** ✅ **Verified** — extracted directly from the official `growwapi-1.5.0`
> Python SDK source (the working reference the operator pointed to), 2026-06-19.
> Not guessed, not hallucinated. Every field below is read from real SDK code.
> **Source files (official SDK):** `groww/feed.py`, `groww/constants.py`,
> `groww/proto/proto_parser.py`, `groww/proto/stocks_socket_response_pb2.py`,
> `groww/client.py`.
> **Operator lock:** §32 (brutex/SDK are REFERENCE for the protocol; our production
> path stays native tickvault Rust + the authorized local Python validation sidecar).

---

## 0. The headline (what the operator asked to cross-verify)

| Question | Verified answer |
|---|---|
| How do we REQUEST a live instrument? | Subscribe to a **NATS topic** built from `{exchange, segment, exchange_token}` |
| What URL / transport? | **`wss://socket-api.groww.in`** — **NATS-over-WebSocket + Protobuf** (NOT plain JSON) |
| Where does the tick TIMESTAMP live, and is it ms? | Field **`tsInMillis`** — **YES, millisecond** epoch (this is the Groww advantage) |
| Where is the price? | Field **`ltp`** (last traded price) |
| Where is volume? | Field **`volume`** (cumulative day volume) |
| Where is the instrument identity in the tick? | **NOT in the tick body** — it comes from the **subscription topic/meta** (`exchange`, `segment`, `feed_key=exchange_token`) |

---

## 1. AUTH — two separate tokens (both verified)

### 1a. Access token (REST) — `client.py::get_access_token`
```
POST https://api.groww.in/v1/token/api/access
Headers:
  Authorization: Bearer <api_key>
  Content-Type: application/json
  x-api-version: 1.0
  x-client-id: growwapi
  x-request-id: <uuid4>
Body:  {"key_type":"totp","totp":"<6-digit TOTP>"}
200 -> {"token":"<access_token>"}   # SDK returns response.json()["token"]
```
✅ Our native Rust `crates/core/src/feed/groww/auth.rs` already matches this EXACTLY.

### 1b. Socket token (live feed) — `feed.py` + `client.py::generate_socket_token`
```
POST https://api.groww.in/v1/api/apex/v1/socket/token/create/
  (nkey/NaCl-signed; the SDK generates an ed25519 user keypair per session)
200 -> {"token":"<socket_jwt>", "subscriptionId":"<sub_id>"}
```
The live NATS connection authenticates with this socket JWT + the ed25519 seed
(`nkeys`). This is the part a native Rust client must implement (NATS-over-WS +
nkey) — which is why the validation sidecar uses the SDK first (lock §32).

---

## 2. REQUEST — subscribe an instrument (`feed.py` + `constants.py`)

The SDK `GrowwFeed.subscribe_ltp(instrument_list, on_data_received)` takes a list of
**instrument dicts**, each EXACTLY:

```python
{"exchange": "NSE", "segment": "CASH", "exchange_token": "2885"}
```

| Key | Constant | Allowed values (verified from `client.py`) |
|---|---|---|
| `exchange` | `FeedConstants.EXCHANGE` | `NSE`, `BSE` (also MCX/MCXSX/NCDEX/GLOBAL/US in the proto enum) |
| `segment` | `FeedConstants.SEGMENT` | `CASH` (equity), `FNO` (derivatives) (+ CURRENCY/COMMODITY in proto) |
| `exchange_token` | `FeedConstants.EXCHANGE_TOKEN` | Groww instrument token (string) from `instrument.csv` |

**Up to 1000 instruments per subscribe.** Each instrument maps to one NATS topic:

| Feed | Topic prefix (`constants.py`) | Example (token=2885) |
|---|---|---|
| Equity LTP NSE | `/ld/eq/nse/price.` | `/ld/eq/nse/price.2885` |
| Equity LTP BSE | `/ld/eq/bse/price.` | `/ld/eq/bse/price.<token>` |
| F&O LTP NSE | `/ld/fo/nse/price.` | `/ld/fo/nse/price.<token>` |
| Index value NSE | `/ld/indices/nse/price.` | `/ld/indices/nse/price.<token>` |
| Market depth NSE eq | `/ld/eq/nse/book.` | `/ld/eq/nse/book.<token>` |

Subscribe methods: `subscribe_ltp` (LTP), `subscribe_index_value` (indices),
`subscribe_market_depth` (5-level book). Each has an `unsubscribe_*` twin.

---

## 3. RESPONSE — the live tick (`proto_parser.py` + `stocks_socket_response_pb2.py`)

The NATS message payload is a **Protobuf** `StocksSocketResponseProtoDto`. For an LTP
feed, `get_data_dict(data, "ltp")` returns the inner **`stockLivePrice`** message
(`StocksLivePriceProto`), converted with
`MessageToDict(preserving_proto_field_name=True)` — so the dict keys are the proto
field names below **verbatim** (camelCase as defined):

| Tick dict key | Meaning | Type |
|---|---|---|
| **`ltp`** | Last traded price | number |
| **`tsInMillis`** | **Timestamp — epoch MILLISECONDS** | int64 |
| **`volume`** | Cumulative day volume | int |
| `open` | Day open | number |
| `high` | Day high | number |
| `low` | Day low | number |
| `close` | Prev/day close | number |
| `value` | Traded value | number |
| `bidQty` | Best bid qty | int |
| `offerQty` | Best offer qty | int |
| `avgPrice` | Average price | number |
| `openInterest` | OI (F&O) | int |
| `highPriceRange` / `lowPriceRange` | Circuit band | number |
| `highTradeRange` / `lowTradeRange` | Traded range | number |

**CRITICAL — instrument identity is NOT in the tick body.** `stockLivePrice` carries
NO symbol/token. The identity comes from the SUBSCRIPTION: the `on_data_received`
callback receives the topic **meta** (`exchange`, `segment`, `feed_key`=the
`exchange_token`, `feed_type`); you then pull the parsed values via
`feed.get_ltp()` (which returns `{exchange: {segment: {token: {ltp, tsInMillis, ...}}}}`).

The wrapper `StocksSocketResponseProtoDto` fields: `symbol`, `segment`
(`StockSegmentProto`), `exchange` (`StockExchangeProto`), `stockLivePrice`,
`stocksMarketDepth`, `stocksLiveIndices`, `livePoint`.

---

## 4. Corrections this verification forced (vs our earlier `# VERIFY` guesses)

| Our earlier guess (sidecar) | Verified truth | Fix |
|---|---|---|
| Timestamp key `tick_timestamp_millis` | **`tsInMillis`** | sidecar field map |
| Token key `exchange_token` IN the tick | identity from **subscription meta**, not the tick | sidecar callback uses `get_ltp()` + meta |
| `on_data(message)` = the tick (JSON) | callback gets **meta**; data pulled via `get_ltp()`; transport is NATS+protobuf | sidecar uses `GrowwFeed` properly |
| LTP key `ltp` | ✅ `ltp` (correct) | none |
| Volume key `volume` | ✅ `volume` (correct) | none |
| segment `CASH`/`FNO`, exchange `NSE`/`BSE` | ✅ correct | none |
| REST auth `/v1/token/api/access` + TOTP | ✅ correct (our Rust `auth.rs` matches) | none |

---

## 5. What this means for our bridge contract (`groww_bridge.rs`)

Our NDJSON bridge contract is `{security_id, segment, ts_ist_nanos, exchange_ts_millis, ltp, cum_volume}`.
The sidecar maps the verified SDK dict → this contract:
- `exchange_ts_millis` ← **`tsInMillis`** (already ms — no precision loss)
- `ltp` ← `ltp`
- `cum_volume` ← `volume`
- `security_id` ← the subscription's `exchange_token` (from meta, NOT the tick)
- `segment` ← meta `exchange`+`segment` → our canonical (`NSE`+`CASH`→`NSE_EQ`, etc.)
- `ts_ist_nanos` ← `tsInMillis` → IST epoch nanos (Groww ms is UTC epoch; convert)

The Rust bridge is unchanged; only the sidecar field extraction is corrected to the
verified names. Native-Rust live client (NATS-over-WS + nkey + this proto) remains the
production target per lock §32; the verified field list above is its decode spec.
