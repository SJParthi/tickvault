# Groww Trade API — Rate Limits & Capacity (the capacity-truth file)

> **Source:** https://groww.in/trade-api/docs/curl#rate-limits · https://groww.in/trade-api/docs/python-sdk (Rate Limits section) · https://groww.in/trade-api/docs/python-sdk/exceptions
> **Fetched/Verified:** 2026-07-13 (via the 2026-07-03 lossless capture + 2026-07-13 live cross-checks; groww.in direct fetch proxy-blocked from the sandbox — provenance stated honestly)
> **Evidence tiers:** Verified / Assumed / Unknown per README legend.
> **Related:** `11-historical-candles.md` §10, `14-option-chain.md` §4, `99-UNKNOWNS.md` (U-4..U-10)

---

## §1. The official rate-limit table (VERBATIM)

Verbatim from the REST intro page (capture 2026-07-03; the identical section exists on the python-sdk page):

> The rate limits are applied at the type level, not on individual APIs. This means that all APIs grouped under a type (e.g., Orders, Live Data, Non Trading) share the same limit. If the limit for one API within a type is exhausted, all other APIs in that type will also be rate-limited until the limit window resets.
>
> | Type | Requests | Limit (Per second) | Limit (Per minute) |
> | --- | --- | --- | --- |
> | Orders | Create, Modify and Cancel Order | 10 | 250 |
> | Live Data | Market Quote, LTP, OHLC | 10 | 300 |
> | Non Trading | Order Status, Order list, Trade list, Positions, Holdings, Margin | 20 | 500 |

Load-bearing facts (all Verified from the verbatim above):

- **Three families only** — Orders / Live Data / Non Trading. Family membership is enumerated ONLY as the "Requests" column shows.
- **No per-day column exists.** No burst semantics beyond the per-second number. No "Historical" family is defined.
- "until the limit window resets" is the only reset-semantics wording (fixed-window implication; window mechanics otherwise undocumented).
- Separate WS-side statement on the same page family: "For Live feed, upto 1000 subscriptions are allowed at a time." (WebSocket, not REST; per-connection vs per-account unstated — `99-UNKNOWNS.md` U-14.)
- Batch note: LTP/OHLC support "Upto 50 instruments … for each API call" (Verified live 2026-07-13). Whether a 50-symbol call bills as 1 request or 50 is undocumented (**Assumed 1** — it is one HTTP request; probe U-9).
- **Limit scope (per key vs per access token vs per account): NOT stated (Unknown)** — THE load-bearing unknown for BruteX co-tenancy (U-5). Internal empirical evidence (2026-06-30 incident, WS-GAP-09): a tickvault restart loop 429-starved BruteX's auth on the shared account — consistent with per-ACCOUNT pooling, not doc-proven.

## §2. Forensic freshness record (how much to trust §1 today)

| Evidence | Date | What it establishes |
| --- | --- | --- |
| Lossless 26-page capture (per-page heading/table/code-block counts verified 100% vs live HTML) | **2026-07-03** | The §1 table verbatim, 10 days before this write-up |
| Prior lossless capture generation | 2026-06-12 | Same table — two-point stability across 3 weeks |
| designhawk/nse-swing-trading verbatim page scrape | **2026-03-22** | Orders 10/250 · Live Data 10/300 · Non Trading 20/500 — no per-day column |
| asifrahaman13/quant-trading code comments | **2026-04-12** | Live Data 10/300 · Non Trading 20/500 |
| sppidy/janus doc mirror (freshest independent) | **2026-04-30** | Live Data 10/300 · Orders 10/250 · Non Trading 20/500 |
| arkapravasinha/groww-mcp-server README (Orders **15**/s + per-day columns) | 2025-06-10 | The circulating conflicting table — **PROVEN 2025-stale** |
| myselfshravan/wiz-hack (Orders **15**/250) | 2025-11-07 | Same stale generation |
| anil-sn/EliteTrading (Orders **15**/s·250/min) | 2025-12-16 | Same stale generation |

**Verdict (Verified via mirror dating, 2026-07-13):** every mirror carrying Orders **15/s** and/or per-day columns is dated **2025**; every **2026**-dated mirror (Mar/Apr) carries **10/250, 10/300, 20/500 with NO per-day column** — matching the 2026-07-03 capture exactly. The circulating 15/s+per-day table is stale pre-2026 text. The 2026-07-13 WebSearch backend regurgitated that stale table as if official — a documented extraction hazard.

**The caveat that cuts both ways:** the Orders limit demonstrably changed **15 → 10 between 2025-12-16 and 2026-03-22** — the numbers CAN change, and the docs site had at least one post-capture edit elsewhere in the same 10-day window class (segment-wording drift on the June python-sdk live-data page vs live). Present-tense currency of §1 is therefore certifiable only via a live re-probe (§6) — a residual, not a refutation: nothing reachable on 2026-07-13 contradicts any §1 number.

## §3. UNASSIGNED FAMILIES (load-bearing)

**`/v1/historical/*` (candles, candle/range, expiries, contracts) and `/v1/option-chain/*` appear in NO documented rate-limit family.** Verified-by-omission across ALL 26 pages of the 2026-07-03 capture (grep 2026-07-13): the §1 table's Live Data row enumerates exactly the three `/live-data/{quote,ltp,ohlc}` endpoints; option chain shipped Nov 2025 and the table gained no row through 2026-07-03; even the stale third-party tables carry no historical/chain row. Endpoint-by-endpoint:

| Endpoint prefix | Family per docs |
| --- | --- |
| `/live-data/quote`, `/live-data/ltp`, `/live-data/ohlc` | **Live Data (Verified)** |
| `/historical/candles`, `/historical/candle/range` | **NOT ENUMERATED — Unknown** |
| `/historical/expiries`, `/historical/contracts` | **NOT ENUMERATED — Unknown** |
| `/option-chain/exchange/{ex}/underlying/{ul}` | **NOT ENUMERATED — Unknown** (path outside `/live-data/`) |
| `/live-data/greeks/...` | under the `/live-data/` path — plausibly Live Data (**Assumed**); not named in the table |
| `/order-advance/*` (Smart Orders) | **NOT ENUMERATED — Unknown** (plausibly Orders) |
| `/user/detail`, `/margins/*` | **NOT ENUMERATED — Unknown** (plausibly Non Trading — "Margin" is listed) |

**Planning assumption (conservative for per-minute math, stated as an Assumption):** historical + chain calls bill to **Live Data (10/s · 300/min)**. This is NOT conservative against the existence of a smaller undocumented endpoint bucket (the Dhan chain's 1-per-3s rule is the industry existence proof of that pattern) — nothing suggests one exists at Groww, and nothing rules it out. **Unknown until probed** (§6, U-4).

## §4. The per-minute-pipeline arithmetic — honestly split three ways

Workload under assessment: per minute close in session (09:15–15:30 IST), ~3 index spot 1m-candle calls + ~3 option-chain calls + up to ~12 per-contract FNO candle calls ≈ **≤18 req/min** (at the §36.7 futures envelope of ≤24 contracts, worst case ≈ 30 req/min), on a Groww account SHARED with BruteX.

### (a) DOCS-VERIFIED (arithmetic against the §1 numbers alone)

- TickVault steady state: **≤18 req/min = 6% of the 300/min Live Data budget** (≈10% at the ≤24-contract envelope). Trivially inside the per-minute limit under the §3 family assumption.
- The per-SECOND limit (10/s) is the binding constraint even standalone: 18 boundary calls fired inside one second would alone exceed 10/s. **Burst pacing at the minute boundary is required regardless of any co-tenant.**

### (b) EMPIRICAL ANCHOR (BruteX co-tenancy — both readings shown)

- **The anchor:** BruteX sustains **~3 req/sec** against this account when running (operator-relayed, 2026-07-13). BruteX's bulk Groww pulls are **NIGHTLY** per the coordinator brief (its documented output is post-close/off-hours CSV production), so **in-session contention is expected LOW** — but BruteX's in-session REST timing is **NOT independently verified** by any evidence in the research pack. Both readings therefore stand:
- **Reading 1 — BruteX idle in-session:** TickVault alone ≈ 18/300 = **~6% of the minute budget**; the 10/s ceiling is effectively all TickVault's, and boundary pacing is needed only against its own burst.
- **Reading 2 — BruteX active co-tenancy:** ~180 req/min (BruteX) + ≤18 (TickVault) = ~198/300 = **~66% of the minute budget** (~34% headroom) — still inside the per-minute limit; but the shared **10/s ceiling** becomes the hard constraint: BruteX's ~3/s floor leaves ~7/s, so **minute-boundary burst pacing to ≤6/s is MANDATORY** (e.g. 18 calls spread over ≥3 s), else a ≥11/s combined instant 429s the shared family for BOTH systems ("If the limit for one API within a type is exhausted, all other APIs in that type will also be rate-limited").
- **Unresolved internal contradiction (recorded, not resolved):** one research reading treats ~3 req/s as BruteX's consumption WITHIN a real 10/s budget; another treats ~3 req/s as the account's REAL sustainable ceiling (docs' 10/s theoretical). Under the ceiling reading, ≤6/s pacing would still 429 daily and the boundary burst needs ≥6 s even with BruteX idle. Neither reading is evidenced; measure before trusting any contention math.

### (c) LIVE-PROBE-ONLY (what no document answers)

The numbers only the first live session answers: which family the historical/chain calls actually bill to; whether limits pool per key or per account; whether an undocumented per-day cap exists above BruteX's observed volume; the real sustainable per-second rate (3/s vs 10/s); whether any REST-side adaptive/penalty mechanism engages under sustained bursts. §6 is the measurement plan.

### Per-day note

TickVault ≈ 2,250–6,750 req/day; BruteX plausibly tens of thousands/day. **No per-day limit exists in the official table (Verified-absence)**; the stale third-party 5,000/day figure is empirically implausible against BruteX's sustained volume (which has drawn no daily-cap rejects). Residual: an undocumented per-day cap above that volume cannot be excluded.

## §5. 429 / throttle behaviour

- **HTTP 429 → `GrowwAPIRateLimitException`** (Verified, SDK 1.5.0 `_ERROR_MAP`). The SDK raises it with a fixed message, extracts nothing from the body, reads no `Retry-After`, and performs no retry/backoff.
- Official guidance, verbatim (exceptions page, capture 2026-07-03) — the ONLY retry guidance anywhere in the docs:

> This exception is raised when the rate limit for the Groww API is exceeded.
>
> Expect this exception to handle scenarios where the SDK makes too many requests to the API within a short period, indicating a need to **throttle the request rate**.

- **No documented `Retry-After` header, no documented 429 body shape** (Verified-absence; the raw 429 wire shape is probe U-7).
- **Grep record (run by this file's author, 2026-07-13, over ALL 26 pages of the 2026-07-03 lossless capture — closing the earlier partial-corpus caveat):** terms `fair use`, `abuse`, `ban`, `block`, `cooldown`, `retry-after`, `retry after`, `penalty`, `suspension`, `throttl`. Result: **zero policy hits** — `ban`/`block` matched only instrument-symbol substrings (BANKNIFTY/BANKINDIA), capture-header boilerplate ("code blocks match") and SDK code comments ("blocking call"); `throttl` matched exactly once, the exceptions-page sentence quoted above. **No ban / cooldown / fair-use / SLA / capacity / adaptive-throttle language exists anywhere in the REST docs.**
- **The contrast that mandates live-probe humility:** the docs' silence is NOT evidence that adaptive control is absent. On the WebSocket surface, Groww MEASURABLY runs an undocumented account-level ADAPTIVE cap — 33 connections ACKed on fresh account state vs 7 after churn, with a ~35–40 min penalty window (internal measurement 2026-07-06, `groww-scale-aws-lockout-2026-07-06.md`). A vendor demonstrably running adaptive server-side controls on one surface may do so on REST too; assume fixed-window-only at your peril. Design consequence: back OFF on `GrowwAPIRateLimitException`, cap retries, never storm — a reject storm may extend a penalty window.

## §6. Live-probe measurement plan (what the build session's FIRST live session MUST capture)

| # | Measurement | How |
| --- | --- | --- |
| 1 | Per-endpoint response-time histograms | spot 1m candle (`/historical/candles`, 3 index SIDs), chain (`/option-chain/...`, 3 underlyings), per-contract FNO candle — record full latency distributions per endpoint per minute cycle |
| 2 | Every 429, forensically | timestamp (ms), WHICH endpoint drew it, response headers (any `Retry-After`?), raw body shape, and recovery time until the same family accepts again |
| 3 | Just-closed-minute availability latency | poll-until-present ladder: at seconds T≈1,2,3,5,8… of minute M+1, request 1minute candles ending at minute M; record when candle M first appears, whether an in-progress M+1 candle ever appears, and whether candle M's OHLCV mutates on re-poll within 60 s |
| 4 | Chain payload size | `len(strikes)` + raw byte size per underlying (NIFTY, BANKNIFTY on NSE; SENSEX on BSE), current expiry — repeat on an expiry day for the worst case |
| 5 | Per-second burst tolerance at the minute boundary | fire the boundary batch at increasing pace (4/s → 6/s → 8/s → 11/s) on separate days; find the first 429 rate |
| 6 | Family-membership inference | after a historical/chain 429, immediately issue one `get_ltp` — a 429 on it proves shared Live Data billing; clean 200 suggests a separate bucket. Symmetrically: exhaust Live Data with LTP bursts and test whether chain 429s |
| 7 | Live table re-read | from the unblocked box: `curl -s https://groww.in/trade-api/docs/curl | grep -A30 -i 'rate limit'` — re-verify §1 verbatim (the 15→10 history proves the numbers can move) |
| 8 | BruteX in-session rate | measure BruteX's ACTUAL Groww REST request rate and timing during 09:15–15:30 before trusting any co-tenancy arithmetic (resolves the §4b contradiction) |
