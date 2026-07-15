# Zerodha Kite Connect v3 — Official SDKs, API versioning, and the official Changelog

> **Source:** `https://kite.trade/docs/connect/v3/sdks/` ("Libraries and SDKs") · `https://kite.trade/docs/connect/v3/changelog/` ("Changelog") — both fetched live.
> **Fetched-Verified:** 2026-07-14 — **live-verbatim via GH-runner 2026-07-14 (UTC timestamps in 00-COVERAGE-MANIFEST.md)**; both URLs are in the runner fetch-log (per-URL sha256 + timestamp in the manifest). These two docs-nav sections were MISSING from the 2026-07-13 pack; this file + `16-apps-publisher-and-basket.md` close the 5-section gap the live nav revealed.
> **Evidence tiers:** see README legend + the new top tier **Verified (LIVE-DOC runner 2026-07-14)**. CLIENT-LIB-SOURCE / OFFICIAL-MOCK stay as cross-check evidence.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code.
> **Related:** `01-overview-auth-and-token-lifecycle.md` (auth header the 3.0 changelog introduced), `02-rest-conventions-errors-and-rate-limits.md` (rate limits — the changelog's 2021-09-28 row links to the exceptions page's `#api-rate-limit` anchor), `03-orders.md` (MTF/iceberg/TTL/auction dating), `04-gtt.md`, `07-market-quotes.md` (the 500/1000/1000 caps stated on this page), `08-historical-candles.md` (OI column dating), `10-websocket-streaming.md` (WS address + 3.0 protocol change), `15-alerts.md` (Alerts API dating), `99-UNKNOWNS.md`.

---

## 1. The headline facts

1. **Six official SDKs** are listed: Python, Go, Java, PHP, Node.js, .NET/C# — each with a GitHub repo, examples link, and hosted docs — **Verified (LIVE-DOC runner 2026-07-14, `…/v3/sdks/`)**. No Rust SDK exists (a native implementation would be hand-rolled, as with Groww).
2. **Versioning restated on the live page:** "The current major stable version of the API is 3"; root endpoint `https://api.kite.trade`; version selected via the `X-Kite-version: v` header where v "is the version number, major or full (eg: 1 or 1.3 or 3)" — full (minor) versions are requestable — **Verified (LIVE-DOC runner 2026-07-14)**. Upgrades **[Z#1]**/**[Z#2]**'s endpoint+header facts to LIVE-DOC tier.
3. **The changelog is an official dated history**: Kite Connect 3.0 (2018-01-17) and 3.1 (2019-12-10) change sets + a dated table running to **2025-06-12 (Alerts API)** — **Verified (LIVE-DOC runner 2026-07-14, `…/v3/changelog/`)**. It dates several pack claims that were previously SEARCH-tier or undated (§5).
4. **The 3.0 endpoint table states the quote-family caps verbatim**: `/quote` "up to **500** instruments", `/quote/ohlc` "up to **1000**", `/quote/ltp` "up to **1000**" — the first official-page confirmation of **[Z#16]** (which was SEARCH-tier with a refuted stray "250"). Caveat: this is the 3.0-LAUNCH table (2018-era statement on a live page); the market-quotes live page is the current-value authority — **Verified (LIVE-DOC runner 2026-07-14)**.
5. **The changelog page has data-quality warts** (recorded, not normalized): the dated table is NOT sorted (2016/2017 rows appear after the 2018 row), the `2023-07-18 Added virtual contract API` row appears **twice**, and bulk-quote introduction appears under both `2017-09-11` and `2017-09-12` — **Verified (LIVE-DOC runner 2026-07-14)**.

**Compare: Dhan/Groww** — Dhan's release notes are feature-dated without formal version numbers (`.claude/rules/dhan/release-notes.md` — our v2.0–v2.5 labels are internal); Groww documents API-type-level changes without a public changelog page. Kite is the only one of the three with header-selectable API versions (`X-Kite-Version`) and a single official changelog URL worth polling for surface drift.

## 2. Official SDK list (`sdks/`)

**Verified (LIVE-DOC runner 2026-07-14)** — the live table's rows with the exact linked URLs (from the page's hrefs):

| Language | Repository | Examples | Hosted docs |
|---|---|---|---|
| Python | `github.com/zerodha/pykiteconnect` | `…/tree/master/examples` | `kite.trade/docs/pykiteconnect/v4/` |
| Go | `github.com/zerodha/gokiteconnect` | `…/tree/master/examples` | `pkg.go.dev/github.com/zerodha/gokiteconnect/v4` |
| Java | `github.com/zerodha/javakiteconnect` | `…/tree/master/sample/src` | `kite.trade/docs/javakiteconnect/v3/` |
| PHP | `github.com/zerodha/phpkiteconnect` | `…/tree/master/examples` | `kite.trade/docs/phpkiteconnect/v3/classes/KiteConnect-KiteConnect.html` |
| Node.js | `github.com/zerodha/kiteconnectjs` | `…/blob/master/examples` | `kite.trade/docs/kiteconnectjs/v3/` |
| .NET/C# | `github.com/zerodha/dotnetkiteconnect` | `…/tree/master/KiteConnectSample` | `kite.trade/docs/kiteconnectdotnet/v3/` |

- Framing sentence, verbatim intent: "client libraries… allow you to interact with the APIs without having to make raw HTTP calls".
- **Library-version vs API-version disambiguation (official Note):** "This version is a KiteConnect **backend API** version and should not be confused with the specific **library release** version." — i.e. pykiteconnect v5.x still speaks API v3 (`01 §1`'s stale-README refutation stands; note the hosted py docs dir is still `/v4/` — Z-SDK-1).
- The pack's three backbone SDKs (py/go/js) are all on this official list; Java/PHP/.NET were NOT mined for the pack — cross-SDK divergences beyond py/go/js (e.g. more Go-only families like `15`'s alerts) are **Unknown** in those three.

## 3. Version and API endpoint (`sdks/` §2)

**Verified (LIVE-DOC runner 2026-07-14):**

- Root: `https://api.kite.trade` — LIVE-DOC confirmation of **[Z#1]**'s REST root.
- "All requests go to it [v3] by default. It is recommended that a specific version be requested explicitly for production applications as **major releases may break older implementations**."
- Header: `X-Kite-version: v` — major (`3`) or FULL (`1.3`) version pinning. (Header rendered `X-Kite-Version: 3` on the changelog page, `X-Kite-version` here — HTTP headers are case-insensitive; both live.) LIVE-DOC confirmation of **[Z#2]**/**[Z#9]**'s header-based versioning.
- The changelog page links `kite.trade/docs/connect/v1` — the v1 docs are still served (historical reference only).

## 4. Kite Connect 3.0 changes (2018-01-17) and 3.1 changes (2019-12-10)

**Verified (LIVE-DOC runner 2026-07-14, `…/v3/changelog/`)** — historical statements on the live page; current-value authority remains each topic's own live page.

### 4a. 3.0 (major upgrade from API v1, dated 2018-01-17 in §5's table)

- **Header opt-in:** same route `api.kite.trade`; the 3.0 backend selected by sending `X-Kite-Version: 3` on every request.
- **Endpoint changes (the table):** New — `/quote` ("complete market quotes (including depth) for up to 500 instruments in one go"), `/quote/ohlc` (up to 1000), `/quote/ltp` (up to 1000), `/user/profile`. Removed — `/market/instruments/:exchange/:tradingsymbol` ("Replaced by the new /quote API").
- **Login:** `v=3` query param added — `https://kite.zerodha.com/connect/login?v=3&api_key=xxx` (LIVE-DOC confirmation of the login URL in **[Z#1]**).
- **Authentication:** the old `api_key`+`access_token` QUERY-PARAM auth replaced by the header `Authorization: token api_key:access_token` (LIVE-DOC confirmation of **[Z#2]**'s composite lowercase-`token` scheme).
- **WebSocket:** new address `wss://ws.kite.trade` (LIVE-DOC confirmation of **[Z#32]**); binary protocol changed to add **Open Interest, OI Day High, OI Day Low, Last trade timestamp, Exchange timestamp** (the `full`-packet fields decoded in `10 §11`); WS now also delivers realtime order **Postbacks and other message types** (the text-frame channel, `10 §13`/**[Z#43]**); connection auth reduced to `api_key` + `access_token` query params (the old `api_key, user_id, public_token` trio dropped); **subscription cap raised 200 → 1000 instruments** — a 2018 value; the CURRENT documented cap (3,000/conn, **[Z#42]**, SEARCH) postdates this page and is NOT contradicted by it (this entry is history, not the live limit).
- **Response-shape changes:** `/session/token` gained `api_key` + `refresh_token` (LIVE-DOC dating of the `refresh_token` field's existence — its emptiness for individuals is `01 §8`), renamed `order_type→order_types` / `exchange→exchanges` / `product→products`, removed `password_reset`/`member_id`; `/portfolio/positions` gained the six `day_buy_*`/`day_sell_*` fields (`05`); `/trades` + `/orders/:order_id/trades` gained `fill_timestamp` and REMOVED `order_timestamp` (`03 §13`).

### 4b. 3.1 (released 2019-12-10 per §5's table — "minor feature updates")

- **Orders:** added `exchange_update_timestamp` and `meta` to the order response (`03` order-row family).
- **GTT:** "Added support placing, updating, and deleting for GTT orders" — LIVE-DOC dating of the GTT family (`04`).
- **Quote:** "Added Circuit Limits to quote call response" — dates `07 §4`'s `lower/upper_circuit_limit`.
- **Historical:** "Added OI to historical candle data response" — dates `08 §5`'s 7th array element.

## 5. The dated changelog table (verbatim rows, most-recent first as printed)

**Verified (LIVE-DOC runner 2026-07-14).** Reproduced in the page's own (unsorted) order; anchor links the page attaches are noted:

| Date | Change (verbatim) | Pack cross-ref / note |
|---|---|---|
| 2025-06-12 | Added Alerts API (links `…/v3/alerts/`) | **dates [Z#54]** — explains why pykiteconnect (older releases) lacks it (`15`); the Alerts docs URL the pack Assumed is confirmed real |
| 2025-01-17 | Added MTF orders | dates the `MTF` product value in `03` (**[Z#46]**) |
| 2023-07-18 | Added virtual contract API (links `…/v3/margins/#virtual-contract-note`; **row printed TWICE**) | `06 §5` |
| 2023-07-11 | Added basket margin API (links `…/v3/margins/#basket-margins`) | `06 §4` |
| 2022-12-22 | Added holdings auction list API and auction order params (links `…/v3/portfolio/#holdings-auction-list`) | `03`/`05` auctions |
| 2022-03-19 | Added iceberg and TTL orders | dates `03`'s iceberg variety + `validity_ttl` |
| 2021-09-28 | Updated new APIs rate limit (links `…/v3/exceptions/#api-rate-limit`) | the rate-limit TABLE lives on the exceptions page (`02 §6a`); this row carries NO numbers — the 2021 change content itself is not on this page (Z-CL-1) |
| 2021-03-12 | Added holdings authorisation API (links `…/v3/portfolio/#holdings-authorisation`) | dates eDIS (**[Z#60]**, `05 §6`) |
| 2020-08-24 | Added margin calculator APIs (links `…/v3/margins/#margin-calculation`) | `06 §3` |
| 2019-12-10 | Released minor feature updates - Kite Connect 3.1 | §4b |
| 2018-01-17 | Major version upgrade to Kite Connect 3 from API v1. | §4a |
| 2017-09-11 | Introduced bulk quote APIs | duplicate-ish with the 2017-09-12 row below (wart) |
| 2016-05-07 | /orders call and bracket order modification and cancellation now involve the new parent_order_id parameter | BO-era history (`03 §2` — BO discontinued; `parent_order_id` survives for CO) |
| 2016-07-02 | Several new fields added to the Webhooks payload (exchange_timestamp, order_type, product, unfilled_quantity, validity) | `11` postback payload |
| 2017-09-11 | Added support for granular timestamp from and to queries to historical data APIs | dates `08 §2`'s `HH:MM:SS`-granular from/to |
| 2017-09-12 | Added bulk quote fetch APIs | see 2017-09-11 wart |
| 2017-11-13 | Added support for 'UPDATE' Postbacks | `11` — the UPDATE postback type |

### 5a. What the changelog does NOT carry (Verified-absence, scope = the live changelog page, 2026-07-14)

- **No removals/deprecations are logged**: bracket-order discontinuation (~2020), MF order-placement disablement (~2020, `12 §2`), and the 2025 pricing changes (₹2,000→₹500, historical bundled — `13 §2`) are ALL absent — the changelog tracks API-surface ADDITIONS only. Do not treat absence-from-changelog as evidence a removal didn't happen.
- **No rate-limit numbers, no WS-cap updates after 2018** (the 3,000/conn era is unlogged), no token-lifecycle changes. The pack's U-2/U-11/U-12 paste items remain open; this page does not answer them.

## 6. Monitoring guidance (why this page matters operationally)

- The changelog URL is the single official poll-point for NEW API surface (the 2025 rows prove it is maintained). Any future Kite integration session should re-fetch `…/v3/changelog/` FIRST and diff against §5 — the same "read release notes before implementing" discipline as `.claude/rules/dhan/release-notes.md`.
- Version pinning: production callers should send `X-Kite-Version: 3` explicitly (the live page's own recommendation, §3) — both official SDKs already do (`02 §1`).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Z-SDK-1 — hosted-docs version drift.** The live SDK table links pykiteconnect docs at `…/pykiteconnect/v4/` while the py repo's own changelog shows v5.x releases (`01 §1`). Is the hosted v4 doc stale, or the current-major doc line? Check the hosted page's version banner. Matters: none for wire behaviour (the repo source is the pack's authority); recording only.
- **Z-CL-1 — the 2021-09-28 "Updated new APIs rate limit" content.** The row links `exceptions/#api-rate-limit` but carries no numbers; what changed in 2021 vs today's table is unknown from this page. The exceptions live page (file `02`'s LIVE-DOC pass) is the current authority; the 2021 delta itself is historical-curiosity only. Matters: none beyond dating U-12's table.
- **Z-CL-2 — Java/PHP/.NET SDK divergences.** Three officially-listed SDKs were never mined (the pack's backbone is py/go/js). Do they carry additional Go-style exclusive families or divergent defaults (`02 §5`'s class)? Spot-grep their route lists once. Matters: only if a future session uses one of them.
- **Z-CL-3 — changelog completeness.** §5a proves removals/pricing/limits are unlogged; whether ALL additions are logged is unprovable (the alerts row exists, but e.g. `/user/profile/full` — Go-only, `01` — has no row). Treat the changelog as necessary-not-sufficient for surface drift; the U-1 TOC diff remains the completeness check. *(U-1 note: RESOLVED-in-part by live fetch — the live nav enumerated 19 sections (20 primary docs pages counting the out-of-nav legacy `market-data-and-instruments/` mirror) and the 5 previously-missing ones are now covered by files 16–17; the residual U-1 value is future re-diffs, not the original unknown.)*

## CLAIMS (for README reconciled table)

- Six official SDKs: Python, Go, Java, PHP, Node.js, .NET/C# (repos under github.com/zerodha; hosted docs on kite.trade; go docs on pkg.go.dev v4) — **Verified (LIVE-DOC runner 2026-07-14, `…/v3/sdks/`)**. No Rust.
- API version = backend-header-selectable: root `https://api.kite.trade`, `X-Kite-version` header takes major (`3`) or full (`1.3`) values; explicit pinning officially recommended for production; the version is distinct from library release versions — **Verified (LIVE-DOC runner 2026-07-14)**. **Upgrades [Z#1]/[Z#2]/[Z#9] (endpoint + header + header-based versioning) to LIVE-DOC.**
- The 3.0 changelog states the quote-family caps 500 (`/quote`) / 1000 (`/quote/ohlc`) / 1000 (`/quote/ltp`) on an official live page — **first LIVE-DOC corroboration of [Z#16]** (2018-launch-table caveat; current authority = the market-quotes live page); the refuted "250" stays refuted.
- 3.0 (2018-01-17): `Authorization: token api_key:access_token` replaced query-param auth; login gained `v=3`; WS moved to `wss://ws.kite.trade` with OI/OI-hi/OI-lo/LTT/exchange-timestamp added to the binary protocol + text-frame postback delivery; WS cap 200→1000 (2018 value — not the current cap); `/session/token` gained `api_key`+`refresh_token`; trades swapped `order_timestamp`→`fill_timestamp` — **Verified (LIVE-DOC runner 2026-07-14)**; **upgrades [Z#32] and corroborates [Z#2]/[Z#43] at LIVE-DOC**.
- 3.1 (2019-12-10): order response gained `exchange_update_timestamp`+`meta`; GTT family added; circuit limits added to quotes; OI added to historical candles — **Verified (LIVE-DOC runner 2026-07-14)**; dates `04`/`07`/`08` facts.
- Dated changelog rows: Alerts API **2025-06-12** (dates [Z#54]), MTF orders **2025-01-17** (dates [Z#46]'s MTF), virtual contract 2023-07-18, basket margin 2023-07-11, auctions 2022-12-22, iceberg+TTL 2022-03-19, rate-limit update 2021-09-28 (no numbers on-page), holdings authorisation 2021-03-12 (dates [Z#60]), margin calculators 2020-08-24 — **Verified (LIVE-DOC runner 2026-07-14)**.
- The changelog logs ADDITIONS only — BO discontinuation, MF placement disablement, and all pricing changes are absent; the table is unsorted and carries a duplicated 2023-07-18 row + 2017-09-11/12 near-duplicates — **Verified / Verified-absence (LIVE-DOC runner 2026-07-14, scope = the changelog page)**.
