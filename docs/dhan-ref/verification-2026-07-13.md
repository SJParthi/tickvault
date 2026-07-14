# Dhan API Verification Report — 2026-07-13

**Session:** operator-directed full live re-crawl of dhanhq.co/docs/v2 against `docs/dhan-ref/*`
**Author:** Claude (multi-worker sweep A+B+C + hostile review + integrator, sandbox session)
**Honest outcome up front:** a full live-page re-crawl was **IMPOSSIBLE from this sandbox** —
every Dhan-related host is 403-blocked at the agent proxy CONNECT stage. What ran instead was a
**search-index spot-check** (WebSearch-relayed snippets of the live-indexed pages) across ALL 21
`dhan-ref` files, plus **direct-fetch** verification of the official Python SDK
(raw.githubusercontent.com) and PyPI release metadata + the released 2.3.0rc1 sdist
(pypi.org / files.pythonhosted.org). **ZERO hard contradictions** of repo facts were found;
several sub-facts remain Unknown-unchallenged; a handful of drift FLAGS are recorded below with
dated notes added to the affected files.

---

## 1. Connectivity — what was blocked, what worked

All of the following returned **CONNECT-stage 403 from the agent proxy** ("policy denial or
upstream failure") for both `curl` and WebFetch. These are SANDBOX egress blocks, NOT evidence
about the target sites:

| Host / route | Result |
|---|---|
| `dhanhq.co` (all paths, incl. /docs/v2/*) | CONNECT 403 |
| `www.dhanhq.co` | CONNECT 403 |
| `docs.dhanhq.co` (the NEW docs portal) | CONNECT 403 |
| `api.dhan.co` | CONNECT 403 |
| `dhan.co` (incl. support pages) | CONNECT 403 |
| `images.dhan.co` (instrument CSVs — HEAD also 403) | CONNECT 403 |
| `madefortrade.in` (community forum bodies) | CONNECT 403 |
| `dhan.freshdesk.com` | CONNECT 403 |
| `web.archive.org` (incl. save-page-now — no archive escape hatch) | CONNECT 403 |
| `r.jina.ai` (reader-proxy escape hatch) | CONNECT 403 |
| `api.github.com` (raw curl; MCP GitHub tools still work) | CONNECT 403 |

**Working routes:**

| Route | Nature of evidence |
|---|---|
| WebSearch | live-INDEXED snippets/summaries of dhanhq.co / docs.dhanhq.co pages. **Index-lag-prone**: the live page may have changed after the last crawl; summaries are paraphrase-quality, not raw page text. This was the only "live-ish" route. |
| `raw.githubusercontent.com` (dhan-oss/DhanHQ-py source) | direct fetch — genuinely Verified for what the SDK contains (Secondary relative to Dhan docs) |
| `pypi.org` + `files.pythonhosted.org` (dhanhq package metadata + sdists) | direct fetch — genuinely Verified for release dates/contents |
| `github.com` web pages via WebFetch (e.g. DhanHQ-py issue #108) | direct fetch — Verified for the page content (Secondary) |

**Re-crawl options for the operator:** run the crawl from the prod box (unrestricted egress),
request proxy allowlisting for `dhanhq.co` + `docs.dhanhq.co`, or walk the §3 paste list in a
browser.

---

## 2. Per-file verdict table (all 21 dhan-ref files)

**Tier legend:**
- **Verified-snippet** — live-indexed dhanhq.co/docs.dhanhq.co text quoted in a search result, unambiguous (still index-lag-prone).
- **Verified-snippet (quote text not preserved — unaudited)** — sweep A's findings; A's worker did not preserve the quoted text, so its snippet claims could not be re-audited by the hostile review.
- **Secondary** — non-Dhan source (official SDK source, PyPI, GitHub issues, community).
- **Unknown** — no usable evidence surfaced; repo statement stands unchallenged.

**The honest claim for every NO-CONTRADICTION row is: "no contradiction found via
search-index spot-check on 2026-07-13" — NEVER "verified current".**

| # | File | Live URL | Verdict | Tier | Notes |
|---|---|---|---|---|---|
| 1 | `01-introduction-and-rate-limits.md` | https://dhanhq.co/docs/v2/ | NO-CONTRADICTION | Verified-snippet (quote text not preserved — unaudited) | Canonical rate table stands (Order 7,000/day; Data 100,000/day). The 2026-07-03 LLM-export snapshot's `22-rate-limits.md` TRANSPOSES the two daily values — isolated capture defect, see §4 flag 1. |
| 2 | `02-authentication.md` | https://dhanhq.co/docs/v2/authentication/ (+ https://docs.dhanhq.co/api/v2/authentication/renew-token) | NO-CONTRADICTION + 3 RenewToken flags | Verified-snippet + Secondary (SDK direct fetch) | generateAccessToken/consent-flow/profile sample JSON all matched byte-level snippets. RenewToken: semantics + web-only now stated plainly upstream; verb DISPUTED — see §4 flag 3 + the dated note appended to the file. |
| 3 | `03-live-market-feed-websocket.md` | https://dhanhq.co/docs/v2/live-market-feed/ | NO-CONTRADICTION | Verified-snippet (endpoint, 5 conns, 5000/conn, binary, 8B header); Secondary (packet sizes 16/50/162, 20B depth level, 100/msg — SDK struct strings, arithmetic re-checked) | No new response codes / packet types surfaced. SDK 2.3.0rc1's Global Stocks feed is a SEPARATE US-stocks feed, not a change here. |
| 4 | `04-full-market-depth-websocket.md` | https://dhanhq.co/docs/v2/full-market-depth/ | NO-CONTRADICTION | Verified-snippet | Live-indexed page shows `/twohundreddepth` — consistent with the file's OWN 2026-07-03 note; this proves only "no NEW flip observed in the search index", NOT today's live state. Flip-flop stays UNRESOLVED exactly as recorded. 12-byte depth header re-confirmed. Depth remains FORBIDDEN at runtime. |
| 5 | `05-historical-data.md` | https://dhanhq.co/docs/v2/historical-data/ | NO-CONTRADICTION | Verified-snippet (intraday endpoint, intervals 1/5/15/25/60, 90-day cap, 5-year window); Unknown (daily endpoint `/v2/charts/historical` — never quoted in any snippet; toDate non-inclusivity; epoch-seconds format) | Candle-availability latency for the just-closed minute: **still UNDOCUMENTED anywhere search-indexed** — the repo's measure-don't-assume stance (rest-1m-pipeline-error-codes.md §1) is CONFIRMED still correct. |
| 6 | `06-option-chain.md` | https://dhanhq.co/docs/v2/option-chain/ | NO-CONTRADICTION | Verified-snippet | Request PascalCase fields, `oc` decimal-strike keys, greeks example values matched byte-for-byte the 2026-03-13 extraction. |
| 7 | `07-orders.md` | https://dhanhq.co/docs/v2/orders/ | NO-CONTRADICTION | Verified-snippet (endpoints/enums) + Verified-snippet (quote text not preserved — unaudited) for MPP mechanism; Unknown (30-char correlationId cap, >30% disclosedQuantity, modify=TOTAL-qty semantics — snippets too shallow) | MPP mechanism = MATCH; the YEAR in "March 21, 2026" = **Assumed** (no snippet carried the full dated string). |
| 8 | `07a-super-order.md` | https://dhanhq.co/docs/v2/super-order/ | NO-CONTRADICTION | Verified-snippet (legs/modify rules); Secondary (trail jump); Unknown (cancel-entry-cancels-all) | Flag: dhan.co support (Secondary) describes "Super Trail" — BOTH SL and target trailing, each with its own trail jump — richer than the file's single `trailingJump` model. See §4 flag 6. |
| 9 | `07b-forever-order.md` | https://dhanhq.co/docs/v2/forever/ | NO-CONTRADICTION | Verified-snippet (OCO price1/triggerPrice1/quantity1); Secondary (CNC/MTF) | — |
| 10 | `07c-conditional-trigger.md` | https://dhanhq.co/docs/v2/conditional-trigger/ | NO-CONTRADICTION | Verified-snippet | Equities/Indices-only, /alerts/orders, comparison types + operators matched exactly. |
| 11 | `07d-edis.md` | https://dhanhq.co/docs/v2/edis/ | NO-CONTRADICTION | Verified-snippet | T-PIN form, bulk flag, "ALL" all matched. |
| 12 | `07e-postback.md` | https://dhanhq.co/docs/v2/postback/ | NO-CONTRADICTION | Verified-snippet | `filled_qty` snake_case, status list, loopback-URL restriction all matched. |
| 13 | `08-annexure-enums.md` | https://dhanhq.co/docs/v2/annexure/ | NO-CONTRADICTION | Verified-snippet (ExpiryCode 1/2/3); Unknown (currency-segment removal — enum text never surfaced in snippets); Secondary partial (error codes DH-901/905, Data 805/807/813) | ExpiryCode 1=Near/2=Next/3=Far corroborates the file's existing 2026-07-03 §(b) note; the live-probe requirement STANDS. Currency removal: the repo's 2026-07-03 direct compare remains the freshest observation, nothing contradicts it. Secondary datapoint: a live DH-905 reject carried errorMessage "Invalid IP" (static-IP rejects may surface as DH-905, not only DH-911). |
| 14 | `09-instrument-master.md` | https://dhanhq.co/docs/v2/instruments/ | NO-CONTRADICTION | Verified-snippet (detailed CSV URL, per-segment REST); Unknown (compact CSV URL verbatim; column drift) | CSV HEAD probes 403'd at OUR proxy — not evidence of a dead URL. |
| 15 | `10-live-order-update-websocket.md` | https://dhanhq.co/docs/v2/order-update/ | NO-CONTRADICTION | Verified-snippet | Endpoint + MsgCode-42 auth JSON (individual AND partner forms) matched verbatim. |
| 16 | `11-market-quote-rest.md` | https://dhanhq.co/docs/v2/market-quote/ | NO-CONTRADICTION | Verified-snippet | 3 endpoints, 1000-instrument cap, access-token + client-id headers, integer-SecurityId request body all matched. |
| 17 | `12-portfolio-positions.md` | https://dhanhq.co/docs/v2/portfolio/ | NO-CONTRADICTION | Verified-snippet (endpoints + exit-all); Unknown (convertQty wire type) | Exit-all "close all your open positions and open orders" now in indexed text — strengthens the rule file's conservative reading. |
| 18 | `13-funds-margin.md` | https://dhanhq.co/docs/v2/funds/ (+ https://docs.dhanhq.co/api/v2/funds/calculate-multi-margin) | NO-CONTRADICTION + 1 DRIFT flag | Verified-snippet + Verified (PyPI sdist direct fetch) | `availabelBalance` typo STILL LIVE in indexed v1+v2 funds pages. DRIFT flag: multi-margin wire naming — see §4 flag 4 + the dated note appended to the file. |
| 19 | `14-statements-trade-history.md` | https://dhanhq.co/docs/v2/statements/ | NO-CONTRADICTION | Verified-snippet (endpoints, page default 0); Unknown (string debit/credit) | — |
| 20 | `15-traders-control.md` | https://dhanhq.co/docs/v2/traders-control/ | NO-CONTRADICTION | Verified-snippet + Verified (PyPI: `set_pnl_exit` product_type ["INTRADAY","DELIVERY"]) | Low-confidence flag: one search summary called `killSwitchStatus` a "header parameter" vs the repo's query param — paraphrase-quality, see §4 flag 5. |
| 21 | `16-release-notes.md` | https://dhanhq.co/docs/v2/releases/ | **PARTIAL** | Verified-snippet + Verified (PyPI) | Latest INDEXED releases entry = Feb 09 2026 (our "v2.5"); no newer entry found (absence = Unknown, index lag). The file has an internal provenance inconsistency (header "Extracted: 2026-03-13" vs its own "Version 2.5.1 — Mar 17, 2026" section) — see §4 flag 2 + the dated note appended to the file. |

---

## 3. OPERATOR PASTE LIST — walk these in a browser (or curl from the prod box) and paste back

The prod box has direct egress; a browser works too. For each page: does the page still exist,
and does anything contradict the matching `dhan-ref` file / the flags in §4?

- [ ] https://dhanhq.co/docs/v2/ (rate-limit table — confirm Order 7,000/day + Data 100,000/day)
- [ ] https://dhanhq.co/docs/v2/authentication/ (RenewToken curl — GET or explicit POST?)
- [ ] https://dhanhq.co/docs/v2/live-market-feed/
- [ ] https://dhanhq.co/docs/v2/full-market-depth/ (200-level URL: root path or /twohundreddepth?)
- [ ] https://dhanhq.co/docs/v2/historical-data/ (does /v2/charts/historical still appear? any note on just-closed-candle latency?)
- [ ] https://dhanhq.co/docs/v2/option-chain/
- [ ] https://dhanhq.co/docs/v2/orders/ (MPP dated string — confirm "March 21, 2026")
- [ ] https://dhanhq.co/docs/v2/super-order/ (does the page now document dual SL+target trailing / per-leg trail jump?)
- [ ] https://dhanhq.co/docs/v2/forever/
- [ ] https://dhanhq.co/docs/v2/conditional-trigger/
- [ ] https://dhanhq.co/docs/v2/edis/
- [ ] https://dhanhq.co/docs/v2/postback/
- [ ] https://dhanhq.co/docs/v2/annexure/ (ExchangeSegment enum list — currency segments present/absent? ExpiryCode numbering?)
- [ ] https://dhanhq.co/docs/v2/instruments/ (compact CSV URL still api-scrip-master.csv?)
- [ ] https://dhanhq.co/docs/v2/order-update/
- [ ] https://dhanhq.co/docs/v2/market-quote/
- [ ] https://dhanhq.co/docs/v2/portfolio/
- [ ] https://dhanhq.co/docs/v2/funds/ (multi-margin request example — includeOrder/includeOrders? scripts/scripList? availabelBalance still misspelled?)
- [ ] https://dhanhq.co/docs/v2/statements/
- [ ] https://dhanhq.co/docs/v2/traders-control/ (killSwitchStatus — query param or header?)
- [ ] https://dhanhq.co/docs/v2/releases/ (any entry newer than Feb 09 2026? was there ever a Mar 17 2026 entry?)
- [ ] https://docs.dhanhq.co/api/v2/authentication/renew-token (NEW portal — HTTP verb shown? "expires your current token"?)
- [ ] https://docs.dhanhq.co/api/v2/funds/calculate-multi-margin (NEW portal — same endpoint as /margincalculator/multi or a new one?)
- [ ] https://dhanhq.co/docs/v2/expired-options-data/ (no dhan-ref file; snapshot-only coverage)

---

## 4. Flags & follow-ups

1. **`dhanhq-v2-upstream-2026-07-03/22-rate-limits.md` daily values TRANSPOSED** (CONFIRMED —
   independently reproduced by the hostile review): lines 20–21 read Order 100,000/day + Data
   7,000/day, versus BOTH the canonical `01-introduction-and-rate-limits.md:46-51` AND the older
   snapshot `dhanhq-v2-upstream-2026-06-02/01_introduction.md:41-46` (Order 7,000/day, Data
   100,000/day). The same file shows a `RATE_LIMIT_ERROR`/`RL001` error shape found NOWHERE else
   (canonical is DH-904). Verdict: isolated capture-quality defect in Dhan's docs.dhanhq.co
   "Export .md for LLMs" generator — NOT evidence of a real limit change. **The whole
   `dhanhq-v2-upstream-2026-07-03/` LLM-export snapshot dir is flagged SUSPECT wholesale** — do
   not treat it as authoritative over the hand-extracted `dhan-ref` files without cross-checking.
   A dated banner was prepended to the snapshot file (snapshot body untouched).
2. **`16-release-notes.md` internal provenance inconsistency:** header "Extracted: 2026-03-13"
   vs its own "Version 2.5.1 — Mar 17, 2026" section. The 2.5.1 label is our internal mapping
   compiled from dated announcements and may never have been a releases-page entry. Dated note
   appended to the file; header NOT rewritten (house style).
3. **RenewToken verb + semantics** (dated note appended to `02-authentication.md` +
   `.claude/rules/dhan/authentication.md` rule 5):
   - Semantics (2 independent sources → note written): live-indexed docs.dhanhq.co renew-token
     summaries say "This API **expires your current token and provides you with a new token**
     with another 24 hours of validity" and "You can use this **only for tokens generated from
     Dhan Web**"; the official SDK docstring returns "the **new** access token". The repo's
     "extends validity" wording is a superseded reading; the one-active-token invariant is
     unaffected. UNVERIFIED-LIVE.
   - Verb (evidence COLLAPSED — the POST claim did NOT meet the 2-source bar): one search
     summary claimed "Method: POST"; a second summary of the SAME page said the curl example
     (no `-X`) indicates GET; the official SDK — main branch AND the released 2.3.0rc1 sdist
     (direct PyPI fetch) — calls `requests.get` on /v2/RenewToken; a community POST attempt
     (DhanHQ-py issue #108, 2025-10-25) got HTTP 400. Net: GET remains best-supported; the
     repo's GET stands. Live-probe BOTH verbs from the prod box before any code change.
4. **Multi-margin request naming** (dated note appended to `13-funds-margin.md`): live-indexed
   docs summary + the OFFICIAL SDK 2.3.0rc1 sdist (direct fetch — wire body keys
   `includePosition` / `includeOrder` / `scripList` to POST /margincalculator/multi) both point
   AWAY from the repo's earlier `includeOrders`+`scripts` choice (13-funds-margin.md:118).
   UNVERIFIED-LIVE; no runtime impact today (no multi-margin caller exists in crates/);
   live-probe before any multi-margin call is trusted.
5. **killSwitchStatus placement** (LOW confidence — verification-record-only, no doc note): one
   search summary called it a "header parameter" vs the repo's query param. Paraphrase-quality
   summary; historical v2 docs used the query string. Probe on next live crawl.
6. **Super Trail** (Secondary — verification-record-only): dhan.co support describes dual SL +
   target trailing with per-leg trail jump values; `07a-super-order.md` models a single
   `trailingJump`. Check /v2/super-order docs on the next full crawl before modeling.
7. **New docs portal `docs.dhanhq.co`** exists alongside `dhanhq.co/docs/v2` (structured paths:
   api/v2/guides/*, api/v2/authentication/renew-token, api/v2/funds/calculate-multi-margin).
   All dhan-ref Source headers cite only dhanhq.co/docs/v2/*. Future crawls should check BOTH —
   they could diverge.
8. **Candle-availability latency** for the just-closed 1m candle: CONFIRMED still UNDOCUMENTED
   anywhere search-indexed. The repo's measure-don't-assume stance (per-row latency +
   `tv_spot1m_close_to_data_ms` histogram) stands as the only honest source.
9. **SDK 2.3.0rc1 extras** (Secondary, out of tickvault scope, future drift sources): Global
   Stocks (US) incl. a separate live feed; Conditional Orders; P&L Based Exit; Multi-leg Margin
   Calculator; "Enhanced FullDepth callback handling" (SDK-side). Also an SDK marketfeed
   request-code list including Depth (19) — an SDK-level mode not in the repo's feed-code table;
   curiosity only (no depth subscription exists at runtime).

---

## 5. What this record MAY and MAY NOT claim

This record MAY claim: "as of 2026-07-13, a search-index spot-check of all 21 dhan-ref files
found no contradiction of any repo fact, and the specific flags above." All "live" evidence here
is **search-index-mediated** — snippets and summaries of pages as last crawled by the search
engine, subject to index lag and paraphrase error. This record may NOT claim: that any live page
was read directly (none was — every Dhan host is proxy-blocked); that the live pages are
byte-identical to the repo extractions ("verified current" is NOT claimable); or anything about
actual API BEHAVIOR (no API call was made — no token exists in this sandbox, and minting one
would invalidate the prod token). The direct-fetch items (PyPI upload timestamps + sdist
contents, SDK source on raw.githubusercontent.com, GitHub issue text) are genuinely Verified for
what those SOURCES contain — which is Secondary evidence about Dhan's API, not primary.

Full sweep detail (scratchpad, session-local, not committed): findings-B.md + findings-C.md;
sweep A's findings survive only as the hostile-review summary (quote text not preserved).

---

## 2026-07-14 — Runner crawl SUPERSEDES the search-index tiers above

**What changed:** the day after this record was written, a reusable GitHub-Actions runner
workflow (`docs-fetch.yml`; runner IPs are not blocked by Dhan) fetched the raw
server-rendered pages directly — three runs: 29279025906 (2026-07-13T19:35–19:37Z),
29294952698 (2026-07-14T00:05–00:07Z), 29316310511 (2026-07-14T07:57–08:06Z, the canonical
191-row manifest: classic pages + the NEW portal's `/markdown/api/v2/**.md` exports +
docs-export.md + the OpenAPI yaml). Classic-page sha256s were content-identical across all
three runs. Consequences for THIS record:

1. **The §2 verdict tiers are superseded**: every "Verified-snippet"/"Unknown" row now has a
   Verified-live backing (or a dated drift note) — the per-file 2026-07-14 "Upstream Update"
   sections appended to `01`, `02`, `04`, `05`, `07`, `07a`, `07b`, `07c`, `08`, `09`, `10`,
   `12`, `13`, `14`, `15`, `16` (+ the `dhanhq-v2-upstream-2026-07-03/22-rate-limits.md`
   banner update) are the authoritative per-file records. Coverage manifest:
   `00-COVERAGE-MANIFEST.md`.
2. **The §3 operator paste list is MOOT** (retained above for history): the runner crawl
   fetched every listed page verbatim, including both NEW-portal probe targets (real content
   via the `/markdown/` export routes — the portal's HTML routes are a client-rendered SPA
   shell).
3. **§4 flag outcomes:** flag 1 (transposed rate limits) — now proven LIVE portal corruption
   on `guides/rate-limits.md` + the yaml intro, canonical table confirmed verbatim on both
   surfaces' primary pages; flag 2 (2.5.1 provenance) — Verified-live NOT a Dhan release;
   flag 3 (RenewToken) — all three claims CONFIRMED (GET; new-token semantics; web-only; the
   POST claim refuted); flag 4 (multi-margin) — upgraded to a three-artifact split (classic
   page internally split; portal markdown vs yaml disagree; still live-probe-gated); flag 5
   (killSwitchStatus) — the header-vs-query self-contradiction is REAL on the live page;
   flag 6 (Super Trail) — the v2 API surface has exactly ONE trail field on both surfaces
   (the support-page claim stays a Secondary platform claim); flag 7 (two portals) — CONFIRMED
   and now central: the two surfaces DISAGREE on the annexure enums (see
   `08-annexure-enums.md` "2026-07-14 Upstream Update" — the 2026-07-03 comparison had read
   the PORTAL export and misattributed cross-surface divergence as cross-time drift); flag 8
   (candle latency) — confirmed undocumented on the full 191-page corpus; flag 9 — unchanged.
4. **The honest envelope of the superseding evidence** (hostile-review-2, verbatim):

> Every "Verified-current"/"Verified-live" claim in this sync is backed by verbatim
> server-rendered HTML fetched by a GitHub-Actions runner from `dhanhq.co/docs/v2/*`
> (runs 1–3, 2026-07-13T19:35Z–2026-07-14T00:07Z, content-identical sha256 across runs,
> 24/24 distinct page hashes, zero WAF markers) plus — NEW in run 3
> (2026-07-14T07:58–08:01Z) — the new portal's markdown-export routes
> (`docs.dhanhq.co/markdown/api/v2/**.md`), which returned real per-page content;
> the portal's HTML routes remain a client-rendered SPA shell and are NOT covered.
> Claims were verified comment-aware (content hidden in HTML comments counted as
> non-rendered; absences checked inside comments too). Where the two official surfaces
> disagree (annexure currency segments, ExpiryCode numbering, instrument-type count,
> depth unsubscribe code, multi-margin request shape), BOTH readings are recorded and
> neither is asserted as API wire truth — wire behavior for contested enums remains
> UNVERIFIED-LIVE and gated on live probes; getIP response fields are wire-observed
> (prod boot gate + support-ticket records) but doc-unbacked on both surfaces. No live
> API endpoint was probed by this crawl.
