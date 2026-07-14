# 00 — Coverage Manifest (per-URL fetch ledger)

> **Status date:** 2026-07-13.
> **Honesty header — read first:**
> (a) The kite.trade docs site, web.archive.org, AND r.jina.ai (and every other relay tried) are **ALL blocked from this research sandbox** — every request dies at the egress proxy with CONNECT 403. Consequently the page inventory below is **DERIVED**, not authoritative: it was built from the official SDK route tables (`pykiteconnect` `_routes`, `gokiteconnect`), the cross-references cited throughout this pack, and the operator-paste URL list in `99-UNKNOWNS.md` — **NOT yet from the site's own navigation**. A docs section that none of those sources mention would be invisible here (the Alerts family, file 15, was exactly such a near-miss — see U-1).
> (b) **Zero pages are live-fetched-verbatim yet.** Every row below is FAILED-from-sandbox today.
> (c) A GitHub-Actions runner fetch (`docs-fetch.yml`, `workflow_dispatch` with a URL-list input — being added by a sibling session) will replace this derived inventory with the site's OWN nav plus per-URL live-fetched-verbatim timestamps. Until that lands, treat this manifest as a fetch TODO ledger, not a coverage proof.

---

## Per-URL ledger

Status legend for today: every row reads
`FAILED — egress proxy CONNECT 403 (archive.org + r.jina.ai mirrors also blocked); pending live runner fetch` — abbreviated below as **FAILED-403-pending-runner** (the full literal, once). No row is live-fetched.

### Primary docs-nav pages (derived nav order)

| # | Live URL | Section | Status |
|---|---|---|---|
| 1 | `https://kite.trade/docs/connect/v3/` | Root / overview + TOC (the U-1 diff source) | FAILED-403-pending-runner |
| 2 | `https://kite.trade/docs/connect/v3/user/` | User — auth & token exchange, session, funds & margins | FAILED-403-pending-runner |
| 3 | `https://kite.trade/docs/connect/v3/response-structure/` | Response structure (DERIVED URL — may be a root-page section, not a standalone page) | FAILED-403-pending-runner |
| 4 | `https://kite.trade/docs/connect/v3/orders/` | Orders — varieties, place/modify/cancel, statuses, AMO, iceberg | FAILED-403-pending-runner |
| 5 | `https://kite.trade/docs/connect/v3/gtt/` | GTT (Good Till Triggered) triggers | FAILED-403-pending-runner |
| 6 | `https://kite.trade/docs/connect/v3/portfolio/` | Portfolio — holdings, positions, conversion, holdings auth | FAILED-403-pending-runner |
| 7 | `https://kite.trade/docs/connect/v3/margins/` | Margins & charges — order/basket margins, virtual contract note | FAILED-403-pending-runner |
| 8 | `https://kite.trade/docs/connect/v3/market-quotes/` | Market quotes — /quote, /quote/ohlc, /quote/ltp + per-call caps | FAILED-403-pending-runner |
| 9 | `https://kite.trade/docs/connect/v3/market-data-and-instruments/` | Market data & instruments (DERIVED URL — may alias/merge with market-quotes in the live nav) | FAILED-403-pending-runner |
| 10 | `https://kite.trade/docs/connect/v3/historical/` | Historical candle data — per-interval caps table, intervals, continuous, oi | FAILED-403-pending-runner |
| 11 | `https://kite.trade/docs/connect/v3/websocket/` | WebSocket streaming — binary packet structures, modes, limits, heartbeat | FAILED-403-pending-runner |
| 12 | `https://kite.trade/docs/connect/v3/postbacks/` | Postbacks / webhooks — payload + checksum | FAILED-403-pending-runner |
| 13 | `https://kite.trade/docs/connect/v3/mutual-funds/` | Mutual funds — orders, SIPs, holdings, instruments | FAILED-403-pending-runner |
| 14 | `https://kite.trade/docs/connect/v3/alerts/` | Alerts (ASSUMED URL — section proven to exist, URL never confirmed; see U-1/U-41) | FAILED-403-pending-runner |
| 15 | `https://kite.trade/docs/connect/v3/exceptions/` | Exceptions & API rate limits table | FAILED-403-pending-runner |

### Pricing / product / console pages

| # | Live URL | Section | Status |
|---|---|---|---|
| 16 | `https://kite.trade/` | Kite Connect landing — pricing block | FAILED-403-pending-runner |
| 17 | `https://zerodha.com/products/api/` | Zerodha product page — API pricing/plans | FAILED-403-pending-runner |
| 18 | `https://kite.trade/startups/` | Kite Connect startups program | FAILED-403-pending-runner |
| 19 | `https://kite.trade/publisher/` | Kite Publisher (JS button product) | FAILED-403-pending-runner |
| 20 | `https://developers.kite.trade/` | Developer console (signup / apps / billing pages behind login) | FAILED-403-pending-runner |
| 21 | `https://zerodha.com/z-connect/kite/kite-connect-apis-for-programmatic-access` | Z-Connect launch post (pricing timeline) | FAILED-403-pending-runner |
| 22 | `https://zerodha.com/z-connect/updates/free-personal-apis-from-kite-connect` | Z-Connect free-personal-APIs post | FAILED-403-pending-runner |

### Support-portal articles (limits / charges / feature semantics)

| # | Live URL | Section | Status |
|---|---|---|---|
| 23 | `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/kite-connect-api-faqs` | Kite Connect API FAQs (rate limits, order caps) | FAILED-403-pending-runner |
| 24 | `https://support.zerodha.com/category/trading-and-markets/alerts-and-nudges/kite-error-messages/articles/order-rate-limits-on-kite` | Order rate limits on Kite | FAILED-403-pending-runner |
| 25 | `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis` | Charges for Kite APIs | FAILED-403-pending-runner |
| 26 | `https://support.zerodha.com/category/trading-and-markets/kite-features/gtt/articles/what-is-the-validity-of-a-gtt-order` | GTT validity | FAILED-403-pending-runner |
| 27 | `https://support.zerodha.com/category/trading-and-markets/charts-and-orders/gtt/articles/what-is-the-good-till-triggered-gtt-feature` | GTT feature overview | FAILED-403-pending-runner |
| 28 | `https://support.zerodha.com/category/trading-and-markets/charts-and-orders/order/articles/market-price-protection-on-the-order-window` | Market price protection | FAILED-403-pending-runner |

### Kite forum threads (staff-answered limit/policy evidence cited in the pack)

| # | Live URL | Section | Status |
|---|---|---|---|
| 29 | `https://kite.trade/forum/discussion/2354/what-are-the-limits-of-kite-api` | API limits (early-era) | FAILED-403-pending-runner |
| 30 | `https://kite.trade/forum/discussion/2875/is-there-any-limitation-on-getting-historical-data` | Historical limits | FAILED-403-pending-runner |
| 31 | `https://kite.trade/forum/discussion/3018/how-to-get-historical-data-for-expired-futures` | Expired-futures historical | FAILED-403-pending-runner |
| 32 | `https://kite.trade/forum/discussion/3059/do-i-need-to-download-instrument-list-daily-do-you-change-instrument-token-daily` | Instrument-token stability | FAILED-403-pending-runner |
| 33 | `https://kite.trade/forum/discussion/6292/historical-data-for-expired-contracts` | Expired-contracts historical | FAILED-403-pending-runner |
| 34 | `https://kite.trade/forum/discussion/8577/api-rate-limits` | Rate limits (staff answer) | FAILED-403-pending-runner |
| 35 | `https://kite.trade/forum/discussion/8722/is-intraday-1-minute-historical-data-available-until-2015` | Minute-data depth (from ~2015) | FAILED-403-pending-runner |
| 36 | `https://kite.trade/forum/discussion/11108/` | (bare thread id cited in-pack; slug unknown) | FAILED-403-pending-runner |
| 37 | `https://kite.trade/forum/discussion/11460/kite-historical-data-interval-date-range` | Historical per-interval date ranges | FAILED-403-pending-runner |
| 38 | `https://kite.trade/forum/discussion/12851/changes-to-per-day-order-limits` | Per-day order-limit change (July 2023) | FAILED-403-pending-runner |
| 39 | `https://kite.trade/forum/discussion/13397/rate-limits` | Rate limits (staff answer) | FAILED-403-pending-runner |
| 40 | `https://kite.trade/forum/discussion/14149/historical-data-retention-policy` | Historical retention policy | FAILED-403-pending-runner |
| 41 | `https://kite.trade/forum/discussion/14806/historical-data-is-now-free-with-base-kite-connect-subscription` | Historical bundled free (pricing timeline) | FAILED-403-pending-runner |
| 42 | `https://kite.trade/forum/discussion/14868/introducing-kite-connect-personal-apis-free-apis-for-personal-use` | Personal APIs free (pricing timeline) | FAILED-403-pending-runner |
| 43 | `https://kite.trade/forum/discussion/15015/revising-kite-connect-fees-from-2000-to-500-per-month` | Fee revision ₹2000→₹500 (pricing timeline) | FAILED-403-pending-runner |
| 44 | `https://kite.trade/forum/discussion/15111/historical-data-limit-60-days` | Minute-interval 60-day cap | FAILED-403-pending-runner |
| 45 | `https://kite.trade/forum/discussion/15708/` | (bare thread id cited in-pack; slug unknown) | FAILED-403-pending-runner |

> Two support-portal citations appear in the pack only in ellipsized form
> (`https://support.zerodha.com/...can-i-subscribe-to-historical-api-without-subscribing-to-kite-connect-api`,
> `https://support.zerodha.com/...historical-data-and-live-market-data-payment-plan`) — the full URLs were never
> captured, so they are NOT rows above; the runner fetch of the support-portal Kite-API category index should
> recover them.

---

## Coverage fraction

**0/15 primary docs-nav pages (0/45 inventoried URLs overall) live-fetched from this sandbox; API surface reconstructed from official SDK sources (pykiteconnect, gokiteconnect) + Zerodha's official kiteconnect-mocks (64 files) + convergent search extraction — see README evidence legend.**

---

## Runner dispatch input (bare URL list — paste into the `docs-fetch.yml` workflow_dispatch input)

```text
https://kite.trade/docs/connect/v3/
https://kite.trade/docs/connect/v3/user/
https://kite.trade/docs/connect/v3/response-structure/
https://kite.trade/docs/connect/v3/orders/
https://kite.trade/docs/connect/v3/gtt/
https://kite.trade/docs/connect/v3/portfolio/
https://kite.trade/docs/connect/v3/margins/
https://kite.trade/docs/connect/v3/market-quotes/
https://kite.trade/docs/connect/v3/market-data-and-instruments/
https://kite.trade/docs/connect/v3/historical/
https://kite.trade/docs/connect/v3/websocket/
https://kite.trade/docs/connect/v3/postbacks/
https://kite.trade/docs/connect/v3/mutual-funds/
https://kite.trade/docs/connect/v3/alerts/
https://kite.trade/docs/connect/v3/exceptions/
https://kite.trade/
https://zerodha.com/products/api/
https://kite.trade/startups/
https://kite.trade/publisher/
https://developers.kite.trade/
https://zerodha.com/z-connect/kite/kite-connect-apis-for-programmatic-access
https://zerodha.com/z-connect/updates/free-personal-apis-from-kite-connect
https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/kite-connect-api-faqs
https://support.zerodha.com/category/trading-and-markets/alerts-and-nudges/kite-error-messages/articles/order-rate-limits-on-kite
https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis
https://support.zerodha.com/category/trading-and-markets/kite-features/gtt/articles/what-is-the-validity-of-a-gtt-order
https://support.zerodha.com/category/trading-and-markets/charts-and-orders/gtt/articles/what-is-the-good-till-triggered-gtt-feature
https://support.zerodha.com/category/trading-and-markets/charts-and-orders/order/articles/market-price-protection-on-the-order-window
https://kite.trade/forum/discussion/2354/what-are-the-limits-of-kite-api
https://kite.trade/forum/discussion/2875/is-there-any-limitation-on-getting-historical-data
https://kite.trade/forum/discussion/3018/how-to-get-historical-data-for-expired-futures
https://kite.trade/forum/discussion/3059/do-i-need-to-download-instrument-list-daily-do-you-change-instrument-token-daily
https://kite.trade/forum/discussion/6292/historical-data-for-expired-contracts
https://kite.trade/forum/discussion/8577/api-rate-limits
https://kite.trade/forum/discussion/8722/is-intraday-1-minute-historical-data-available-until-2015
https://kite.trade/forum/discussion/11108/
https://kite.trade/forum/discussion/11460/kite-historical-data-interval-date-range
https://kite.trade/forum/discussion/12851/changes-to-per-day-order-limits
https://kite.trade/forum/discussion/13397/rate-limits
https://kite.trade/forum/discussion/14149/historical-data-retention-policy
https://kite.trade/forum/discussion/14806/historical-data-is-now-free-with-base-kite-connect-subscription
https://kite.trade/forum/discussion/14868/introducing-kite-connect-personal-apis-free-apis-for-personal-use
https://kite.trade/forum/discussion/15015/revising-kite-connect-fees-from-2000-to-500-per-month
https://kite.trade/forum/discussion/15111/historical-data-limit-60-days
https://kite.trade/forum/discussion/15708/
```

---

## Verification protocol (when the runner artifact arrives)

1. **Flip statuses honestly:** each row above flips to `fetched-verbatim (<UTC timestamp>, runner)` ONLY when the runner artifact contains that URL's full HTML/text. A 404/redirect/robots-block on the runner is recorded as its own status — never silently upgraded.
2. **Replace the derived nav with the site's own:** parse the fetched `https://kite.trade/docs/connect/v3/` sidebar/TOC and rebuild the "Primary docs-nav pages" table from IT. Any page in the site's nav that is absent from this pack's inventory (an unexplained absence) = a **completeness failure** — a new topic file (or an explicit N/A row with reason) is mandatory before the pack's tiers upgrade.
3. **Re-verify every pack claim:** every SEARCH/Assumed claim in files 01–15 + the [Z#1–60] reconciled claims table is re-checked against the live HTML; tier labels upgrade to the live-fetched-verbatim tier (or are CORRECTED in place with a dated note where the live page disagrees — the refuted-claims section grows, never shrinks).
4. **Close the paired UNKNOWNS:** each resolved row cross-references and closes its `99-UNKNOWNS.md` U-row (paste-class rows only; probe-class rows still require a live api_key session).
5. The two deliberately-unresolved numeric conflicts (orders/min 200-vs-400; orders/day 3,000-vs-5,000) are adjudicated ONLY by the fetched exceptions/support pages — until then both numbers stay flagged.
