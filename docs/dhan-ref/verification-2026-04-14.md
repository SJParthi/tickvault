# Dhan API Verification Report — 2026-04-14

**Session:** S8 / Phase 6.3
**Author:** Claude (automated WebFetch-based re-verification)
**Scope:** Cross-check `.claude/rules/dhan/*.md` and `docs/dhan-ref/*.md`
against the DhanHQ Python SDK (`dhan-oss/DhanHQ-py` v2.2.0) as
the secondary source of truth. The primary source
(`https://dhanhq.co/docs/v2/*`) is **blocked** by the sandbox
egress layer (HTTP 403 from Cloudflare).

## WebFetch availability in this sandbox

| Source | Works? | Why |
|---|---|---|
| `https://dhanhq.co/docs/v2/*` | **NO (403)** | Cloudflare block on the egress IP. Primary source UNAVAILABLE. |
| `https://raw.githubusercontent.com/dhan-oss/DhanHQ-py/*` | **YES** | GitHub raw content is whitelisted. Secondary source ACCESSIBLE. |
| `https://api.github.com/repos/*` | **NO (403)** | GitHub API blocked. Directory listing impossible. |

**Implication:** this verification is based SOLELY on the Python SDK source code. The primary Dhan docs were not fetchable in this session. Any drift between SDK and Dhan docs is UNDETECTABLE from this sandbox. Parthiban's manual cross-check on a non-sandbox environment is recommended for full coverage.

---

## Verified invariants (PASS — our rules match the SDK)

### 1. REST API base URL and auth header

Source: `src/dhanhq/dhan_http.py`

| Invariant | Our rule | SDK value | Status |
|---|---|---|---|
| Base URL | `https://api.dhan.co/v2/` (api-introduction.md rule 2) | `https://api.dhan.co/v2` | **PASS** |
| Auth header name | `access-token` (api-introduction.md rule 3) | `access-token` | **PASS** |

### 2. Binary packet sizes (live-market-feed.md rule 5)

Source: `src/dhanhq/marketfeed.py`

| Packet | Our rule (bytes) | SDK struct format | SDK size | Status |
|---|---|---|---|---|
| Ticker (code 2) | 16 | `<BHBIfI` | 16 | **PASS** |
| Quote (code 4) | 50 | `<BHBIfHIfIIIffff` | 50 | **PASS** |
| OI (code 5) | 12 | `<BHBII` | 12 | **PASS** |
| PrevClose (code 6) | 16 | `<BHBIfI` | 16 | **PASS** |
| MarketStatus (code 7) | 8 | `<BHBI` | 8 | **PASS** |
| Full (code 8) | 162 | `<BHBIfHIfIIIIIIffff100s` | 162 | **PASS** |
| Disconnect (code 50) | 10 | `<BHBIH` | 10 | **PASS** |

**Header layout** (bytes 0-7, shared by all packets):

| Offset | Our rule | SDK format-derived | Status |
|---|---|---|---|
| 0 | u8 response code | `B` (byte 0) | PASS |
| 1-2 | u16 LE message length | `H` (bytes 1-2) | PASS |
| 3 | u8 exchange segment | `B` (byte 3) | PASS |
| 4-7 | u32 LE security ID | `I` (bytes 4-7) | PASS |

All 7 packet types dispatch correctly on code + size. Our parser
and the SDK would produce byte-identical output given the same
wire bytes. Locked facts test
(`dhan_locked_facts::test_locked_packet_sizes`) continues to pass.

### 3. Feed response codes (annexure-enums.md rule 4)

Source: `src/dhanhq/marketfeed.py`

| Code | Our rule | SDK value | Status |
|---|---|---|---|
| 2 | Ticker | Ticker Data | PASS |
| 3 | MarketDepth (v1 legacy) | Market Depth | PASS (SDK still handles v1) |
| 4 | Quote | Quote Data | PASS |
| 5 | OI | OI Data | PASS |
| 6 | PrevClose | Previous Close | PASS |
| 7 | MarketStatus | Market Status | PASS |
| 8 | Full | Full Packet | PASS |
| 50 | Disconnect | Server Disconnection | PASS |

### 4. Feed request codes (annexure-enums.md rule 3)

Source: `src/dhanhq/marketfeed.py`

| Code | Our rule | SDK value | Status |
|---|---|---|---|
| 11 | Connect | Authorization (v1) | PASS (same operation, different label) |
| 12 | Disconnect | Disconnect | PASS |
| 15 | SubscribeTicker | Ticker subscription | PASS |
| 17 | SubscribeQuote | Quote subscription | PASS |
| 21 | SubscribeFull | Full subscription (v2) | PASS |
| 23 | SubscribeFullDepth | Not in SDK list | N/A (v2 feature, SDK may lag) |
| 25 | UnsubscribeFullDepth | Not in SDK list | N/A (v2 feature) |

**Note on code 19:** SDK lists `19 = Depth subscription (v1)`. Our rules do not reference code 19 — we use code 23 for v2 FullDepth subscription per the v2 API. SDK is correct for v1, our rules are correct for v2. Both can coexist because we never emit code 19.

### 5. 20-level and 200-level depth (full-market-depth.md)

Source: `src/dhanhq/fulldepth.py`

| Invariant | Our rule | SDK value | Status |
|---|---|---|---|
| Depth header size | 12 bytes | 12 bytes (`<hBBiI`) | **PASS** |
| Depth level size | 16 bytes (f64+u32+u32) | 16 bytes (`<dII`) | **PASS** |
| 20-level packet size | 332 bytes | 332 bytes (12 + 20*16) | **PASS** |
| 200-level packet size | 3212 bytes | 3212 bytes (12 + 200*16) | **PASS** |
| Bid response code | 41 | 41 | **PASS** |
| Ask response code | 51 | 51 | **PASS** |
| 20-level URL | `wss://depth-api-feed.dhan.co/twentydepth` | `wss://depth-api-feed.dhan.co/twentydepth` | **PASS** |

### 6. Order update WebSocket (live-order-update.md)

Source: `src/dhanhq/orderupdate.py`

| Invariant | Our rule | SDK value | Status |
|---|---|---|---|
| Order update WS URL | `wss://api-order-update.dhan.co` | `wss://api-order-update.dhan.co` | **PASS** |
| MsgCode | 42 | 42 | **PASS** |
| Login message structure (Individual) | `{LoginReq: {MsgCode: 42, ClientId, Token}, UserType: SELF}` | Matches | **PASS** |

---

## DRIFT FOUND — Python SDK is stale, our rule is correct

### 1. 200-level depth WebSocket URL path

**CRITICAL finding. Our rule is correct; the Python SDK is broken.**

| Source | Value |
|---|---|
| Python SDK v2.2.0 (`fulldepth.py`) | `wss://full-depth-api.dhan.co/` (**root path**) |
| Our rule (`full-market-depth.md` rule 2) | `wss://full-depth-api.dhan.co/twohundreddepth` |
| Dhan support ticket #5519522 (2026-04-10) | `/twohundreddepth` is MANDATORY. Root path causes `Protocol(ResetWithoutClosingHandshake)` within 3-5 seconds. |

**Our codebase is correct** and mechanically locked via
`dhan_locked_facts::test_locked_200_depth_path`. This is exactly
the bug class we locked against in session 2. If we had used the
SDK as authoritative, our 200-level depth connection would be
broken.

**Action:** none — our rule wins. The SDK is ahead of Dhan's
production server in acknowledging the fix, OR the SDK is stale.
Either way, our `dhan_locked_facts` test + rule file are the
canonical truth.

---

## UNVERIFIED (SDK did not expose, primary docs blocked)

These invariants exist in our rule files but were not directly
verifiable in this session — either the SDK doesn't expose them or
the Dhan docs are blocked:

1. **Dhan error codes DH-901..910** (annexure-enums.md rule 11).
   Need to fetch `docs/v2/annexure/#trading-api-error` or find them
   in an SDK source file I didn't reach.
2. **Data API error codes 800-814** (annexure-enums.md rule 12).
   Same — likely in a constants file.
3. **Order update single-character codes** (Product: C/I/M/F/V/B,
   TxnType: B/S, etc.) (live-order-update.md rule 6). SDK
   `orderupdate.py` doesn't spell these out.
4. **OrderStatus 9 variants**, **InstrumentType 10 variants**,
   **ExpiryCode 3 values**, **AfterMarketOrder 4 values**
   (annexure-enums.md rules 6-9). SDK might expose as enum values
   in a file I didn't reach.
5. **Rate limits** (api-introduction.md rule 7). Might be in
   comments in the SDK, not in verifiable code.
6. **Static IP endpoint** (`POST /v2/ip/setIP`) contract
   (authentication.md rule 7). Not reachable via the SDK files
   I fetched.
7. **Historical data endpoint contracts** (historical-data.md):
   columnar response format, 90-day intraday max, `toDate`
   non-inclusive rule. The SDK `dhan_http.py` only exposed the
   base URL + header name.
8. **Option chain `client-id` header rule** (option-chain.md rule 3).
   Dhan docs only; SDK doesn't separate the client-id header from
   the access-token header in this version.

These do NOT mean our rules are wrong — they mean I couldn't
independently confirm them in this WebFetch-restricted sandbox.
Every one of them is already locked by a test in
`crates/common/tests/dhan_locked_facts.rs` based on Dhan support
tickets and the reference doc files.

---

## Honest conclusion

**Our rule files and code are consistent with the Python SDK for
every invariant I could fetch.** The one drift found (200-level
depth URL path) is the SDK being wrong, not our rule — and we
already have a mechanical test locking the correct value.

**What this verification does NOT prove:**
- That Dhan's live API matches the Python SDK (they can drift too)
- That the invariants I could not fetch (error codes, order update
  char codes, rate limits, etc) are still correct
- That Dhan hasn't made a breaking change in the last 7 days that
  neither we nor the SDK has caught up to

**What to do next:**
1. **Parthiban: manually cross-check** the 8 unverified items
   against `https://dhanhq.co/docs/v2/*` from a non-sandbox
   machine (5 minutes of clicking through the docs).
2. **File a GitHub issue** to track re-verification of the
   unverified items whenever `dhan_locked_facts.rs` gains a new
   test — so the cross-check becomes part of the commit that
   adds the fact.
3. **Fix the sandbox egress block** (if possible) so future
   sessions can fetch `dhanhq.co` directly. This would close the
   verification gap and let me produce a complete report in one
   session.

## Signed

Session 8 plan verified the 7 PASS items above and flagged the 1
DRIFT + 8 UNVERIFIED items. Report committed alongside the plan.
