# Zerodha Kite Connect v3 — API Reference Pack

> **Vendor:** Zerodha (Kite Connect v3 — `api.kite.trade` REST + `wss://ws.kite.trade` streaming), India's largest retail broker's official trading/data API.
> **Fetched:** 2026-07-13 (all research performed this date).
> **Purpose:** research-only reference pack so a future implementation session needs ZERO re-research. Mirrors the `docs/dhan-ref/` + `docs/groww-ref/` + `docs/gdf-ref/` patterns.
>
> **🎯 SCOPE BANNER — REFERENCE ONLY.** This pack is documentation, NOT a feed-scope grant: **no feed code, no order code, no credentials, no Zerodha account exists in this project.** It mirrors the read-only DhanHQ-skill pattern (CLAUDE.md — "READ-ONLY API reference … never used to place orders"). Adding Zerodha as a feed/broker requires its own dated operator scope lock per `.claude/rules/project/websocket-connection-scope-lock.md` re-approval protocol + `no-rest-except-live-feed-2026-06-27.md` FIRST.
>
> **Single source of open questions:** `99-UNKNOWNS.md` (U-1 … U-49) — including the **ordered operator-paste URL list** (the proxy blocks every kite.trade page; the operator must paste them from a normal browser, the gdf-ref pattern).
> **Per-URL coverage ledger:** `00-COVERAGE-MANIFEST.md` — one row per known/derived docs page with fetch status (all FAILED-from-sandbox today; each row flips to fetched-verbatim when the `docs-fetch.yml` runner artifact lands).

---

## ⚠ ACCESS CONSTRAINT — read before trusting any tier label

**Every kite.trade / zerodha.com / support.zerodha.com page was PROXY-BLOCKED from the research sandbox on 2026-07-13** (direct fetch = egress-proxy CONNECT 403; web.archive.org = blocked; r.jina.ai and every relay = blocked — full route log in the probe record). Therefore:

- **NOTHING in this pack is byte-verbatim from an official docs page.** The ARCHIVE-DOC and MIRROR-LIVE tiers are defined below but **UNUSED — unattainable, not skipped.**
- The strongest attainable evidence — and the pack's backbone — is **official Zerodha source code + official Zerodha mock data**, both fetched raw on 2026-07-13: the two official SDKs (`zerodha/pykiteconnect@master`, `zerodha/gokiteconnect@master`, + `zerodha/kiteconnectjs@e366e45` for the postback validator) and the official `zerodha/kiteconnect-mocks@main` repo (64 files incl. two base64-encoded REAL captured WebSocket wire packets, which were **decoded and re-parsed in-sandbox, reproducing the official paired JSON on every wire-decoded field** — the pack's one piece of empirical protocol proof).
- Docs-page prose and every published LIMIT (rate limits, caps, token-flush time, pricing) reached at most **SEARCH** tier (search-backend extraction of the named pages — substantive, URL-cited, NOT byte-verbatim, and the summarizer can itself err). Every such number carries an operator-paste row in `99-UNKNOWNS.md`. **Do not contract on a SEARCH-tier number.**
- An independent adversarial review ran the same day (5 reviewer passes: WS protocol re-derivation + mock re-decode, limits re-sourcing, auth/pricing re-verification, completeness matrix, hostile label audit). All CRITICAL/HIGH findings were fixed in place; the surviving posture is what you read here.

## Evidence-tier legend (header block on every file)

| Tier | Meaning | Status in this pack |
|---|---|---|
| `ARCHIVE-DOC` | web.archive.org capture of the official page | **UNUSED (blocked)** |
| `MIRROR-LIVE` | live official page via a relay/mirror | **UNUSED (blocked)** |
| `CLIENT-LIB-SOURCE` | read from official Zerodha SDK source (pykiteconnect / gokiteconnect / kiteconnectjs @ pinned commits, fetched 2026-07-13) | the backbone — authoritative for wire behaviour |
| `OFFICIAL-MOCK` | read from `zerodha/kiteconnect-mocks@main` c7a8123 (canonical response shapes; incl. the two real `.packet` wire captures) | the backbone — authoritative for response shapes |
| `SEARCH` | search-backend extraction of a named live kite.trade/zerodha page, 2026-07-13; sub-Verified — **never carries a bare "Verified" prefix** (pack hard rule) | all docs-page limits/prose |
| `ARITH` | derived arithmetic, derivation shown (token maths, epoch decodes, mock reconciliations) | supporting |

Confidence overlay: **Verified** (official artifact, or multiple consistent artifacts) / **Verified-absence** (grep-proven absent from a named complete artifact — scope always stated) / **Assumed** (single source or inference, stated) / **Unknown** (→ `99-UNKNOWNS.md`).

## File index

| File | Contents |
|---|---|
| `01-overview-auth-and-token-lifecycle.md` | product, base URLs, headers, login flow + checksum, session shape, **daily token lifecycle**, refresh-token reality, logout, profile |
| `02-rest-conventions-errors-and-rate-limits.md` | envelopes, form-vs-JSON bodies, exceptions taxonomy, HTTP codes, **the rate-limit tables + recorded conflicts** |
| `03-orders.md` | full order surface, varieties, autoslice, iceberg, market protection, states, trades, error classes |
| `04-gtt.md` | GTT single/two-leg, wire format (JSON-strings in form fields), statuses, expiry, triggered≠executed |
| `05-portfolio.md` | holdings (+summary/compact), positions {net,day}, conversion, auctions, eDIS authorise |
| `06-margins-and-charges.md` | funds/margins per segment, order/basket margin calculators, virtual contract note, the shared charges object |
| `07-market-quotes.md` | `/quote` family, `i=` param, per-call caps, full-quote shape incl. 5-level depth, trigger_range |
| `08-historical-candles.md` | the historical endpoint, **per-interval range caps**, candle arrays, +0530 timestamps, continuous/oi, freshness Unknown |
| `09-instruments-master.md` | gzipped CSV master, 12 columns (NO ISIN), token=exch_token×256+segment, exchanges, index tokens |
| `10-websocket-streaming.md` | **THE feed-decision file** — binary protocol (empirically decoded against official packets), modes, framing, divisors, timestamps, reconnect |
| `11-postbacks-and-order-updates.md` | HTTP postback + checksum formula, WS text-frame order updates, delivery caveats |
| `12-mutual-funds.md` | compact MF surface (placement-disabled caveat up front) |
| `13-pricing-and-access-model.md` | ₹500/mo model, Personal free, pricing timeline 2015→2026, credits, startups — highest-hallucination-risk file, read its §0 |
| `14-option-chain-composition.md` | **no chain endpoint** — the composition recipe + §5 the consolidated **India-F&O/BFO coverage note** |
| `15-alerts.md` | Alerts API + ATO (Go-SDK + 7 official mocks; absent from pykiteconnect) |
| `99-UNKNOWNS.md` | consolidated open questions U-1…U-49 + **the ordered operator-paste URL list** |

## Reconciled master claims table (the load-bearing facts)

Adjudicated 2026-07-13 across the drafting pass + 5 adversarial review passes, built from the per-file CLAIMS appendices (which remain in each file with full citations — this table is the index, the appendices are the detail). Cite rows as **[Z#n]**.

| # | Claim | Tier | Source file |
|---|---|---|---|
| 1 | REST root `https://api.kite.trade`; login `https://kite.zerodha.com/connect/login?api_key=…&v=3` (both official SDKs point at kite.zerodha.com, NOT kite.trade) | Verified (CLIENT-LIB-SOURCE ×2) | 01 §2 |
| 2 | Every authenticated request: `X-Kite-Version: 3` + `Authorization: token <api_key>:<access_token>` (composite, lowercase `token` scheme — a third scheme vs Dhan's `access-token` and Groww's `Bearer`) | Verified (CLIENT-LIB-SOURCE ×2) | 01 §3, 02 §1 |
| 3 | Token exchange: `POST /session/token`, form-encoded, `checksum = sha256_hex(api_key + request_token + api_secret)`; renewal checksum swaps in `refresh_token` | Verified (CLIENT-LIB-SOURCE ×2, byte-exact) | 01 §5, §8 |
| 4 | The session response carries NO expiry/TTL field (only `login_time`, 19-char naive string) — the client must KNOW the daily-flush rule; materially worse than Dhan/Groww which both return machine-readable expiry | Verified (OFFICIAL-MOCK) + Verified-absence | 01 §6 |
| 5 | **Access token valid ~one whole day; flushed daily ~07:30 AM IST; a token minted after ~07:35 lasts the whole day** | SEARCH (forum, convergent; TOP open question U-2) | 01 §7 |
| 6 | **NO unattended token path for individuals:** daily interactive login on kite.zerodha.com is exchange-mandated; `refresh_token` is EMPTY for individuals (platform-only feature); no programmatic request_token exists | Verified-absence (SDKs+mock) + SEARCH | 01 §5/§7/§8 |
| 7 | One-active-token-per-key (second mint kills the first) | Assumed (weak SEARCH; probe U-4 — the Dhan dual-instance lesson) | 01 §7 |
| 8 | Session-death wire signature: HTTP **403** + `error_type == "TokenException"` (fires pykiteconnect's `session_expiry_hook`) | Verified (CLIENT-LIB-SOURCE) | 01 §4, 02 §5c |
| 9 | Versioning is HEADER-based (no `/v3/` in any path); POST/PUT bodies are form-urlencoded EXCEPT exactly 3 JSON routes (`/margins/orders`, `/margins/basket`, `/charges/orders`); GTT nests JSON as STRINGS inside form fields | Verified (CLIENT-LIB-SOURCE) | 02 §1–§2, 04 §3 |
| 10 | Envelopes: `{"status":"success","data":…}` / `{"status":"error","error_type","message","data"}` — NO numeric machine error codes (vs Dhan's DH-9xx); dispatch is by `error_type` NAME with unknown names degrading to GeneralException | Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK) | 02 §3–§5 |
| 11 | Exception surface: 8 py classes (Token/Permission 403, Input 400, Order/General 500, Data 502, Network 503); Go adds `UserException`/`TwoFAException` strings + divergent defaults (Order→400, Data→504); `MarginException` is attested on the wire by the autoslice MOCK ONLY (in neither SDK — py raises GeneralException for it) | Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK) | 02 §5, 03 §14 |
| 12 | REST timestamps are naive IST strings `yyyy-mm-dd hh:mm:ss` (19-char) | SEARCH + Verified (SDK len==19 parse) / Assumed (IST) | 02 §3c |
| 13 | **API rate limits: Quote 1/s · Historical 3/s · Order placement 10/s · everything else 10/s — per api_key; no daily cap outside orders; breach → HTTP 429** | SEARCH (convergent ×3 passes; paste U-12) | 02 §6a |
| 14 | Order RMS caps: 10/s; **200-vs-400/min CONFLICT** (400 = currently-extracted, 200 = superseded FAQ-era); **3,000-vs-5,000/day CONFLICT** (5,000 = the live-FAQ figure, 3,000 tied to a 2023 change); pooling scope wording conflicts (account-level vs "single user/API key"); 25 modifications/order | SEARCH (conflicts recorded, unresolved; U-13/U-14) | 02 §6b |
| 15 | NEITHER official SDK retries, backs off, rate-limits, or reads `Retry-After` — pacing is entirely caller-owned | Verified-absence (grep, both SDKs) | 02 §2/§6e |
| 16 | Quote-family per-call caps: **500** instruments on `/quote`, **1000** on `/quote/ohlc` + `/quote/ltp` (a stray "250" extraction failed a targeted disconfirmation pass) | SEARCH (paste U-15) | 07 §3 |
| 17 | Full `/quote` payload: OHLC, volume, OI (+day high/low), circuit limits, **5-level bid/ask depth** — keyed by the requested `exchange:tradingsymbol` string, repeated `i=` query params | Verified (OFFICIAL-MOCK + CLIENT-LIB-SOURCE) | 07 §2/§4 |
| 18 | `ohlc.close` in quotes = PREVIOUS session's close (the Dhan Ticket #5525125 semantics class) | Assumed (mock-consistent; paste U-18) | 07 §4 |
| 19 | Historical endpoint: `GET /instruments/historical/{instrument_token}/{interval}` with `from`/`to` (`yyyy-mm-dd HH:MM:SS`), `continuous`, `oi` — instrument identity + interval both in the PATH | Verified (CLIENT-LIB-SOURCE ×2) | 08 §1–§2 |
| 20 | Historical response: positional arrays `[ts, o, h, l, c, volume(, oi)]`, 6 elements (7 with `oi=1`); timestamps ISO-8601 with explicit `+0530` offset — the least timezone-ambiguous of the three brokers | Verified (OFFICIAL-MOCK + both SDK parsers) | 08 §5–§6, §8 |
| 21 | Interval set: `minute, 3minute, 5minute, 10minute, 15minute, 30minute, 60minute, day` (no `2minute`); no SDK-side enum — all validation server-side | SEARCH + Verified-absence (no INTERVAL constants) | 08 §3 |
| 22 | **Per-request range caps: minute 60d · 3/5/10minute 100d · 15/30minute 200d · 60minute 400d · day 2000d** — the pack's most load-bearing numeric table | SEARCH (×3 independent agreeing extractions; NEVER byte-verbatim — paste U-6 is the pack's #1 item) | 08 §4 |
| 23 | **1-minute history available from ~2015-02-02**; day candles further back; ONE conflicting "3-year rolling retention" forum claim exists | SEARCH (conflict recorded; U-10) | 08 §9 |
| 24 | Just-closed-minute availability/latency + forming-candle behaviour: UNDOCUMENTED everywhere — the gating fact for any Kite per-minute REST leg | Unknown (probe ladder specified; U-7) | 08 §10 |
| 25 | `continuous=1` = expired FUTURES contracts' candles via the live contract's token, `day` interval only (SDK docstring says "futures and options" — discrepancy flagged) | SEARCH + Verified (docstring) / Unknown (authority) | 08 §7 |
| 26 | Historical data now included with the base subscription: the ₹2,000/mo historical add-on was REMOVED effective **2025-02-08** | SEARCH-title verbatim (forum 14806) ×2 dates | 08 §11, 13 §2/§4 |
| 27 | Instruments master: `GET /instruments` (+ `/instruments/{exchange}`) returns a **gzipped CSV** (not JSON), 12 columns exactly, **NO ISIN column** — cross-vendor joins need a symbol bridge (unlike Dhan/Groww masters) | Verified (OFFICIAL-MOCK + Go struct) / SEARCH (gzip wording) | 09 §2–§3 |
| 28 | **`instrument_token = exchange_token × 256 + segment_id`** — low byte = WS segment (nse 1, nfo 2, cds 3, bse 4, bfo 5, bcd 6, mcx 7, mcxsx 8, indices 9); proven over 99/99 mock rows, hard-coded by both SDKs | Verified (CLIENT-LIB-SOURCE ×2) + ARITH | 09 §5 |
| 29 | Dump generated once daily (`last_price` stale by design); fetch once ~08:30 AM and store; derivative tokens churn daily — NEVER hardcode tokens | SEARCH (advisory; publish time unpinned U-25) | 09 §6 |
| 30 | Index tokens: `NSE:NIFTY 50` = 256265, `NSE:NIFTY BANK` = 260105, `BSE:SENSEX` = 265 (all ×256+9-consistent) — forum-sourced; resolve from the daily CSV at runtime regardless | SEARCH + ARITH | 09 §8 |
| 31 | Exchange enum: NSE, BSE, NFO, BFO, CDS, BCD, MCX (BFO = BSE F&O is a first-class constant in both SDKs) | Verified (CLIENT-LIB-SOURCE) | 09 §4 |
| 32 | WS endpoint `wss://ws.kite.trade?api_key=<key>&access_token=<token>` — auth entirely in the query string, no auth frame | Verified (CLIENT-LIB-SOURCE ×2) | 10 §1 |
| 33 | Client WS messages: text JSON `{"a": "subscribe"/"unsubscribe"/"mode", "v": …}` with NUMERIC tokens (opposite of Dhan's string SecurityIds); modes `ltp`/`quote`/`full`; resubscribe-after-reconnect is CLIENT-side | Verified (CLIENT-LIB-SOURCE ×2) | 10 §4–§5 |
| 34 | Binary framing: u16 BE packet-count + per-packet u16 BE length; **ALL protocol integers BIG-endian unsigned**; NO floats on the wire — prices are scaled uint32 (÷100; CDS ÷1e7; BCD ÷1e4) | Verified (CLIENT-LIB-SOURCE ×2 + mock decode) | 10 §6–§7 |
| 35 | **Packet type = body LENGTH: 8 (ltp) / 28 (index quote) / 32 (index full) / 44 (quote) / 184 (full)**; segment = `token & 0xFF`; indices non-tradable | Verified (CLIENT-LIB-SOURCE ×2) | 10 §7 |
| 36 | Quote packet (44B): token/ltp/ltq/atp/volume/buyq/sellq @0–27, OHLC as **O,H,L,C** @28–43 (`close` = prev session); NO timestamp in quote mode | Verified + empirical mock decode | 10 §10 |
| 37 | Full packet (184B): + ltt@44, oi@48, oi_day_high@52, oi_day_low@56, exchange_timestamp@60, 10 × 12-byte depth entries (5 buy @64, 5 sell @124; qty u32 + price u32 + orders u16 + 2B pad) | Verified + empirical mock decode (all 10 levels reproduced) | 10 §11–§12 |
| 38 | **Index packets order OHLC as H,L,O,C** (opposite of quote packets) and leave bytes 24–27 unread by both SDKs (presumed change — unlabeled) | Verified (CLIENT-LIB-SOURCE ×2) / Unknown (24–27 label) | 10 §9 |
| 39 | **Kite WS timestamps are STANDARD UNIX epoch seconds (UTC-anchored)** — mock raw 1625461887 ↔ official "2021-07-05T10:41:27+05:30". THE most dangerous cross-broker difference: Dhan's WS epoch is IST-anchored (−19800 for UTC); porting a parser between them corrupts every timestamp by 5h30m | Verified (OFFICIAL-MOCK) + ARITH | 10 §12 |
| 40 | The derived `change` field has THREE divergent semantics (py = percent, go = absolute, official mock = absolute; the quote mock's value matches NEITHER formula) — not on the wire, never validate it against the mocks | Verified (adversarial cross-check) | 10 §12 note |
| 41 | Heartbeat = 1-byte binary frame "every couple seconds" (both SDKs ignore <2-byte frames); py pings 2.5s/drops ~5–7.5s, go passively reconnects after 5s silence | Verified (SDK guards) + SEARCH (cadence) | 10 §3/§14 |
| 42 | **WS caps: 3,000 instruments/connection × 3 connections/api_key (≈9,000)** — the 3-conn cap is described by forum staff as a SOFT limit with abuse-flagging; neither number exists in any SDK | SEARCH (paste U-11) | 10 §2, 02 §6d |
| 43 | Order updates + errors arrive as TEXT frames `{"type":"order"/"error","data":…}` on the SAME WS as binary ticks (no separate order socket — vs Dhan's dedicated order-update WS); payload = the standard order-row family | Verified (CLIENT-LIB-SOURCE ×3 + OFFICIAL-MOCK postback.json) | 10 §13, 11 §5 |
| 44 | Postback (HTTP webhook) checksum = `sha256(order_id + order_timestamp + api_secret)` — Kite is the only broker of the three with an authenticated webhook; only the JS SDK ships the validator | Verified (CLIENT-LIB-SOURCE kiteconnectjs) | 11 §4 |
| 45 | Coverage asymmetry: HTTP postbacks fire ONLY for the registering api_key's orders; WS order updates cover ALL the user's orders from any platform | SEARCH (consistent; paste U-39) | 11 §1 |
| 46 | Order surface: `POST/PUT/DELETE /orders/{variety}(/{order_id})`; varieties `regular/amo/co/iceberg/auction` (Go ships legacy `bo` — discontinued); autoslice via `autoslice=true` with mixed-outcome `children[]`; `MTF` is a live wire product value | Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK) | 03 |
| 47 | Order states are OPEN-ENDED (8-state mock transition list incl. MODIFY interims; `TRIGGER PENDING` for resting SL — SEARCH); only COMPLETE/CANCELLED/REJECTED are terminal — vs Dhan's closed 9-value enum | Verified (OFFICIAL-MOCK) + SEARCH | 03 §12 |
| 48 | GTT: `/gtt/triggers` family; types `"single"`/`"two-leg"`; **`triggered` ≠ executed** — the official mock shows a triggered leg whose order was REJECTED (circuit limit); execution truth lives in `orders[].result.order_result` | Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK) | 04 §6 |
| 49 | GTT validity ≈ 1 year (mock: expires_at exactly created_at+1y — but a second mock is NOT +1y); max active GTTs 500 vs older 250/100 — conflicting | SEARCH + ARITH (conflicts recorded; U-34) | 04 §8 |
| 50 | Positions = `{net: [], day: []}` with SIGNED `quantity` (no LONG/SHORT enum); conversion = `PUT /portfolio/positions`, plain-int quantity → `data: true` (vs Dhan's string convertQty + 202); NO pledge and NO exit-all endpoint exists in either SDK | Verified (OFFICIAL-MOCK + CLIENT-LIB-SOURCE + absence greps) | 05 |
| 51 | Margin calculators take JSON ARRAYS and return per-order margin components + a FULL statutory charges breakdown (`transaction_tax` stt/ctt, exchange/SEBI charges, brokerage, stamp, GST split) — unique among the three brokers; the basket's top-level `charges` key is silently DROPPED by the official Go struct | Verified (OFFICIAL-MOCK ×3 + CLIENT-LIB-SOURCE) | 06 §3–§6 |
| 52 | **Kite Connect has NO option-chain, expiry-list, or greeks endpoint** — the chain is COMPOSED client-side (daily NFO/BFO master → filter → batched `/quote` ≤500 legs at 1 call/s → ~1 Hz for one index chain); greeks must be self-computed (staff-confirmed) — vs Dhan's and Groww's single-call chains with greeks | Verified-absence (complete `_routes` + full Go grep) + SEARCH (staff forum) | 14 |
| 53 | **India-F&O coverage:** parser/enum plumbing for NSE indices, NFO, SENSEX and BFO is Verified at SDK level (segment slots 2/5/9, `EXCHANGE_BFO`, index packet layout), but NO fetched artifact demonstrates live SENSEX/BFO delivery end-to-end, and index/BFO HISTORICAL coverage is undocumented — day-0 probes | Verified (SDK plumbing) / Unknown (live + historical; U-9/U-22) | 14 §5 |
| 54 | An official **Alerts API** exists (`/alerts` family, UUID identity, `simple` + **`ato`** alert-triggered-orders with full order-param baskets) — in gokiteconnect + 7 official mocks, ABSENT from pykiteconnect | Verified (CLIENT-LIB-SOURCE go + OFFICIAL-MOCK ×7) + Verified-absence (py) | 15 |
| 55 | MF surface exists end-to-end in SDKs+mocks, but MF order PLACEMENT was reportedly disabled ~2020 — code surface and live entitlement disagree | Verified (surface) / SEARCH + Unknown (liveness) | 12 §2 |
| 56 | **Current pricing (2026-07-13, search-tier): Kite Connect = ₹500/month per API key incl. live WebSocket + historical data; Kite Connect Personal = FREE (orders/portfolio, NO market data); Kite Publisher = free trade buttons** | SEARCH-extract ×4 + SEARCH-title (official X post); freshness Assumed (U-42) | 13 §1 |
| 57 | Pricing timeline: pre-2025 = ₹2,000/mo/app + ₹2,000/mo historical add-on; add-on removed 2025-02-08; Personal launched free 2025-03 (likelier)/04; fee cut ₹2,000→₹500 effective ~2025-05-06 (single dated extraction) — tied to NSE's retail-algo circular | SEARCH-titles (official forum threads 14806/14868/15015) + ARITH decodes | 13 §2 |
| 58 | Developer billing is credit-denominated (1 credit = ₹1) per app per month at developers.kite.trade; today's per-app quantum (500?) unconfirmed | SEARCH ×2 / Assumed (U-42) | 13 §6 |
| 59 | `trigger_range` (CO-legacy): the two official SDKs ship DIFFERENT route shapes; official mock exists; liveness unknown | Verified (CLIENT-LIB-SOURCE + OFFICIAL-MOCK) / Unknown | 07 §10 |
| 60 | eDIS: `POST /portfolio/holdings/authorise` → `{request_id}` + a kite.zerodha.com web-view authorisation URL (Go-SDK + mock only; not in pykiteconnect) | Verified (CLIENT-LIB-SOURCE go + OFFICIAL-MOCK) | 05 §6 |

## Refuted / commonly-misstated claims (do NOT resurrect)

| Refuted / misstated claim | Truth |
|---|---|
| "Zerodha made Kite Connect free in **September 2023**" | NOT FOUND — 4 targeted searches returned zero 2023 sources; every dated source places the free-Personal event in 2025 (13 §3). Do not cite a 2023 date anywhere. |
| "The OHLC/LTP quote endpoints cap at **250** instruments" | A stray extraction; a targeted 2026-07-13 disconfirmation pass failed to reproduce it and re-returned 1000/1000/500 (07 §3). Still SEARCH — but 250 is the weaker reading by far. |
| "`MarginException` is a pykiteconnect exception class" | exceptions.py has exactly 8 classes, no margin variant — `MarginException` is a MOCK-attested wire `error_type` that pykiteconnect dispatches to GeneralException (03 §14). |
| "Monthly SIP `instalment_day` ∈ {1, 5, 10, 15, 20, 25}" | The official `mf_sips.json` mock itself carries monthly rows with `instalment_day: 30` — the SEARCH enum is stale/partial/wrong (12 §4). |
| "The mock `change` values validate the SDK formula" | The full-mock `change` is ABSOLUTE (matches go, not py's percent); the quote-mock `change` matches NEITHER formula (copy-paste artifact). Never validate derived `change` against the mocks (10 §12). |
| "Kite WS timestamps are IST-anchored epoch (like Dhan)" | ARITH-refuted: raw 1625461887 = the official JSON's `10:41:27+05:30` only under STANDARD UTC epoch semantics. Dhan is the IST-anchored one (10 §12). |
| "Bracket orders (`bo`) are placeable" | BO was discontinued (~2020); the gokiteconnect `VarietyBO` constant is legacy residue — pykiteconnect dropped it (03 §2). |
| "The official Python client is v4" | The README H1 title is stale; the changelog in the same file shows v5/v5.2 (01 §1). |
| "WS root is `wss://websocket.kite.trade`" | A kiteconnectjs JSDoc wart; the code default in all three SDKs is `wss://ws.kite.trade` (11 §5). |
| "`holdings.json` shows the same ISIN listed on both exchanges" | The holdings mock contains zero duplicate ISINs; the per-exchange-token fact rides on CROSS-mock evidence (SBIN NSE=779521 in postback.json vs BSE=128028676 in holdings.json) (05 §2 — fixed after the label audit). |

## Compare-at-a-glance (the three-broker deltas that bite)

| Dimension | Kite | Dhan (`docs/dhan-ref/`) | Groww (`docs/groww-ref/`) |
|---|---|---|---|
| Token life | ~1 day, ~07:30 IST flush (SEARCH); NO unattended mint for individuals | 24h JWT, TOTP API mint | daily 06:00 IST expiry, API mint |
| WS binary | BIG-endian scaled uint32, length-typed packets | LITTLE-endian, f32 prices, code-typed packets | protobuf over NATS |
| WS tick timestamp | UTC epoch seconds | IST-anchored epoch seconds | epoch milliseconds |
| Option chain | NONE — compose it yourself, no greeks | 1 call/underlying (1-per-3s), greeks incl. | 1 call/underlying+expiry, greeks incl. |
| Quote budget | 1/s × 500 (full, with depth) | 1/s × 1000 | 10/s+300/min × 50 (LTP/OHLC) |
| Historical | 1 endpoint, path-typed interval, +0530 ISO rows, per-interval day caps | POST, columnar arrays, epoch secs, 90d intraday | GET, row arrays, 30/90/180d caps, from-2020 |
| Master ISIN | **absent** | present | present |

---

*Assembled 2026-07-13. Per-file CLAIMS appendices carry the full citation detail behind every row above; `99-UNKNOWNS.md` carries everything this pack could NOT establish, with the exact operator-paste URLs.*
