# 00 — Coverage Manifest (per-URL fetch ledger)

> **Status date:** 2026-07-14 — **LIVE-VERIFIED**.
> **Provenance:** every row below was fetched live-verbatim by the GitHub-Actions `docs-fetch` runner —
> **run 29316744448** on branch **`docs-fetch/zerodha-2026-07-14`**, results committed as **`d28f3cb4`**
> (`d28f3cb4ea39f9aa0d46ca3343252dbbaff71a53`), crawl completed **2026-07-14T08:11:00Z**. The runner recorded a
> per-URL manifest (`fetch-log.tsv`: url | http_status | bytes | **sha256** | fetched_at_utc | attempts | discovered)
> plus the raw HTML of all 148 fetches; the sha256 prefixes and UTC timestamps below are copied from that log verbatim.
> Pack files 01–17 cite the live URLs with the **Verified (LIVE-DOC runner 2026-07-14)** tier; this manifest is the
> integrity anchor those citations resolve against.
> **Completeness mandate:** the section list in §1 is the live site's OWN primary nav (not a derived guess) — every
> nav section must map to a pack file; an unmapped nav section is a completeness failure.
> **History (compressed):** the 2026-07-13 pack was built SDK/mock/search-derived because the research sandbox's egress
> proxy CONNECT-403-blocked kite.trade, archive.org, and every relay tried — 0/45 URLs were live-fetched then.
> The runner fetch replaced that derived inventory in full; the old FAILED-403 ledger is superseded by this file.

---

## 1. The site's OWN docs nav (authoritative section list)

Extracted from the live v3 root page (`https://kite.trade/docs/connect/v3/`, fetched 2026-07-14T08:04:49Z,
sha256 `5edfd9a6…`): the mkdocs-material primary sidebar carries **19 nav entries**, in document order below.
**All 19 fetched HTTP 200** — no live-nav section is missing from the evidence bundle or the pack.
All timestamps below are 2026-07-14 UTC; status format: `fetched-verbatim (UTC time, sha256 prefix, runner)`.

| # | Nav section (live label) | Live URL (`…/docs/connect/v3/` +) | Pack file covering it | Status |
|---|---|---|---|---|
| 1 | Introduction | `.` (root) | `01` (overview/auth) + `02` (transport conventions) | fetched-verbatim (08:04:49Z, `5edfd9a6…`, runner) |
| 2 | Libraries and SDKs | `sdks/` | `17-sdks-and-changelog.md` | fetched-verbatim (08:08:04Z, `9f25efa6…`, runner) |
| 3 | Response structure | `response-structure/` | `02-rest-conventions-errors-and-rate-limits.md` | fetched-verbatim (08:04:54Z, `14e8cf28…`, runner) |
| 4 | Exceptions and errors | `exceptions/` | `02-rest-conventions-errors-and-rate-limits.md` | fetched-verbatim (08:05:23Z, `a51b5b88…`, runner) |
| 5 | User | `user/` | `01-overview-auth-and-token-lifecycle.md` (+ funds in `06`) | fetched-verbatim (08:04:52Z, `1573d3c4…`, runner) |
| 6 | Orders | `orders/` | `03-orders.md` | fetched-verbatim (08:04:57Z, `919e7f90…`, runner) |
| 7 | GTT orders | `gtt/` | `04-gtt.md` | fetched-verbatim (08:04:59Z, `f43a9820…`, runner) |
| 8 | Alerts | `alerts/` | `15-alerts.md` | fetched-verbatim (08:05:21Z, `e5a98a41…`, runner) |
| 9 | Portfolio | `portfolio/` | `05-portfolio.md` | fetched-verbatim (08:05:01Z, `55312b2a…`, runner) |
| 10 | Market quotes and instruments | `market-quotes/` | `07-market-quotes.md` + `09-instruments-master.md` (+ `14`) | fetched-verbatim (08:05:06Z, `a9908760…`, runner) |
| 11 | WebSocket streaming | `websocket/` | `10-websocket-streaming.md` (+ order-update frames in `11`) | fetched-verbatim (08:05:14Z, `c750c431…`, runner) |
| 12 | Historical candle data | `historical/` | `08-historical-candles.md` | fetched-verbatim (08:05:12Z, `7ff795c1…`, runner) |
| 13 | Postbacks / WebHooks | `postbacks/` | `11-postbacks-and-order-updates.md` | fetched-verbatim (08:05:16Z, `c603c9c9…`, runner) |
| 14 | Mutual funds | `mutual-funds/` | `12-mutual-funds.md` | fetched-verbatim (08:05:19Z, `7ed998f9…`, runner) |
| 15 | Margin calculation | `margins/` | `06-margins-and-charges.md` | fetched-verbatim (08:05:04Z, `7a60762b…`, runner) |
| 16 | Publisher - Offsite orders | `basket/` | `16-apps-publisher-and-basket.md` | fetched-verbatim (08:07:00Z, `ae5d0c1a…`, runner) |
| 17 | Publisher JS plugin | `publisher/` | `16-apps-publisher-and-basket.md` | fetched-verbatim (08:07:58Z, `8bfead1d…`, runner) |
| 18 | Mobile and Desktop apps | `apps/` | `16-apps-publisher-and-basket.md` | fetched-verbatim (08:06:48Z, `a391f990…`, runner) |
| 19 | Changelog | `changelog/` | `17-sdks-and-changelog.md` | fetched-verbatim (08:07:02Z, `5e0dc081…`, runner) |

**The 20th primary docs page (legacy, OUT of the live nav):** `market-data-and-instruments/` was in the original
requested list and still serves HTTP 200, but the live nav's slug is `market-quotes/`. It is a provably **stale
mirror** (pre-3.1 field names, 250-instrument caps) — kept as evidence with a poison warning in
`07-market-quotes.md` (source-header note + the bold "Legacy field-name divergence" bullet) and `09-instruments-master.md` §11:

| Legacy page | Pack file | Status |
|---|---|---|
| `market-data-and-instruments/` | `09-instruments-master.md` §11 (+ stale-mirror warning in `07`) | fetched-verbatim (08:05:09Z, `0b76eb0d…`, runner) — legacy/out-of-nav |

Non-nav outbound links on the root page (context, not sections): `https://developers.kite.trade/login`
(LOGIN-GATED console), forum FAQ thread `discussion/4732` (NOT fetched this crawl), and one support-portal
TOTP-setup article (fetched — see §2c).

---

## 2. Full per-URL ledger (all 148 runner fetches)

Column values are copied verbatim from `fetch-log.tsv` (sha256 truncated to 12 hex chars; full digests live in the
runner artifact / commit `d28f3cb4`). `discovered` = `list` (in the dispatched URL list) or `nav-discovered`
(found by the runner's nav crawl). Every `200` row = **fetched-verbatim (runner)**.

### 2a. Primary docs pages (20 = 19 live-nav sections + 1 legacy)

| URL | HTTP | bytes | sha256 | fetched_at_utc | source |
|---|---|---|---|---|---|
| `https://kite.trade/docs/connect/v3/` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:04:49Z | list |
| `https://kite.trade/docs/connect/v3/alerts/` | 200 | 74055 | `e5a98a412673…` | 2026-07-14T08:05:21Z | list |
| `https://kite.trade/docs/connect/v3/apps/` | 200 | 14445 | `a391f9904373…` | 2026-07-14T08:06:48Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/basket/` | 200 | 25305 | `ae5d0c1ab354…` | 2026-07-14T08:07:00Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/changelog/` | 200 | 24580 | `5e0dc08176fd…` | 2026-07-14T08:07:02Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/exceptions/` | 200 | 18620 | `a51b5b88ed1c…` | 2026-07-14T08:05:23Z | list |
| `https://kite.trade/docs/connect/v3/gtt/` | 200 | 62261 | `f43a98209a0e…` | 2026-07-14T08:04:59Z | list |
| `https://kite.trade/docs/connect/v3/historical/` | 200 | 30035 | `7ff795c16b16…` | 2026-07-14T08:05:12Z | list |
| `https://kite.trade/docs/connect/v3/margins/` | 200 | 72997 | `7a60762b534c…` | 2026-07-14T08:05:04Z | list |
| `https://kite.trade/docs/connect/v3/market-data-and-instruments/` | 200 | 39306 | `0b76eb0da1e1…` | 2026-07-14T08:05:09Z | list |
| `https://kite.trade/docs/connect/v3/market-quotes/` | 200 | 45878 | `a990876014d9…` | 2026-07-14T08:05:06Z | list |
| `https://kite.trade/docs/connect/v3/mutual-funds/` | 200 | 80613 | `7ed998f908ca…` | 2026-07-14T08:05:19Z | list |
| `https://kite.trade/docs/connect/v3/orders/` | 200 | 157324 | `919e7f90baae…` | 2026-07-14T08:04:57Z | list |
| `https://kite.trade/docs/connect/v3/portfolio/` | 200 | 95783 | `55312b2a7c1f…` | 2026-07-14T08:05:01Z | list |
| `https://kite.trade/docs/connect/v3/postbacks/` | 200 | 26130 | `c603c9c96b1a…` | 2026-07-14T08:05:16Z | list |
| `https://kite.trade/docs/connect/v3/publisher/` | 200 | 35161 | `8bfead1d1fcc…` | 2026-07-14T08:07:58Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/response-structure/` | 200 | 16821 | `14e8cf28f3d7…` | 2026-07-14T08:04:54Z | list |
| `https://kite.trade/docs/connect/v3/sdks/` | 200 | 17222 | `9f25efa660e4…` | 2026-07-14T08:08:04Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/user/` | 200 | 50544 | `1573d3c47efc…` | 2026-07-14T08:04:52Z | list |
| `https://kite.trade/docs/connect/v3/websocket/` | 200 | 32567 | `c750c431e724…` | 2026-07-14T08:05:14Z | list |

### 2b. Pricing / product pages (7)

`developers.kite.trade/` is classified **LOGIN-GATED**: the fetch returned real HTML (200, 3,915 bytes) but it is
only the public login page — the developer-console dashboard (app management, billing/credits) is not publicly
reachable. All other rows are full public pages.

| URL | HTTP | bytes | sha256 | fetched_at_utc | source |
|---|---|---|---|---|---|
| `https://developers.kite.trade/` | 200 | 3915 | `124cec3d9a78…` | 2026-07-14T08:05:36Z | list |
| `https://kite.trade/` | 200 | 30127 | `fbc439f5d062…` | 2026-07-14T08:05:26Z | list |
| `https://kite.trade/publisher/` | 200 | 4715 | `6ffbaa807413…` | 2026-07-14T08:05:33Z | list |
| `https://kite.trade/startups/` | 200 | 6654 | `29b0bafc3ca1…` | 2026-07-14T08:05:30Z | list |
| `https://zerodha.com/products/api/` | 200 | 30127 | `c943532ff614…` | 2026-07-14T08:05:28Z | list |
| `https://zerodha.com/z-connect/kite/kite-connect-apis-for-programmatic-access` | 200 | 1322555 | `bfb13891a12c…` | 2026-07-14T08:05:38Z | list |
| `https://zerodha.com/z-connect/updates/free-personal-apis-from-kite-connect` | 200 | 49836 | `4afaf47e2d9b…` | 2026-07-14T08:05:41Z | list |

### 2c. Support-portal articles (31 = 6 original + 25 nav-discovered, incl. the 14 newly discovered Kite-API articles and the 2 previously-ellipsized slugs recovered)

The two URLs the 2026-07-13 pack only knew in ellipsized form were recovered by the nav crawl and fetched:
`…/can-i-subscribe-to-historical-api-without-subscribing-to-kite-connect-api` and
`…/historical-data-and-live-market-data-payment-plan`.

| URL | HTTP | bytes | sha256 | fetched_at_utc | source |
|---|---|---|---|---|---|
| `https://support.zerodha.com/category/trading-and-markets/alerts-and-nudges/kite-error-messages/articles/order-rate-limits-on-kite` | 200 | 68124 | `04b7c99a9c3a…` | 2026-07-14T08:05:46Z | list |
| `https://support.zerodha.com/category/trading-and-markets/charts-and-orders/gtt/articles/what-is-the-good-till-triggered-gtt-feature` | 200 | 74478 | `8c38eb2b5ad2…` | 2026-07-14T08:05:54Z | list |
| `https://support.zerodha.com/category/trading-and-markets/charts-and-orders/order/articles/market-price-protection-on-the-order-window` | 200 | 69276 | `b23bb175709e…` | 2026-07-14T08:05:56Z | list |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/api-sandbox` | 200 | 67226 | `3d3c916e9d63…` | 2026-07-14T08:09:34Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/can-i-subscribe-to-historical-api-without-subscribing-to-kite-connect-api` | 200 | 67913 | `8deabe55f043…` | 2026-07-14T08:09:36Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/can-i-use-historical-and-live-data-taken-from-kite-connect-api-on-other-platforms` | 200 | 67480 | `957cb9a7a360…` | 2026-07-14T08:09:39Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/difference-between-net-and-day-positions-api` | 200 | 72396 | `029fc0e8ef6f…` | 2026-07-14T08:09:42Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/enable-desktop-mode` | 200 | 67255 | `f957500344c2…` | 2026-07-14T08:09:44Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/historical-data-and-live-market-data-payment-plan` | 200 | 66893 | `09d16139e4b2…` | 2026-07-14T08:09:47Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/how-do-i-sign-up-for-kite-connect` | 200 | 70553 | `be726296a4af…` | 2026-07-14T08:09:49Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/invoice-kite-connect-api` | 200 | 67093 | `1bc04c392cbc…` | 2026-07-14T08:09:51Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/kite-connect-api-faqs` | 200 | 78494 | `ccb438f87256…` | 2026-07-14T08:05:43Z | list |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/static-ip` | 200 | 68767 | `31bc94f66a5c…` | 2026-07-14T08:09:54Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/unsubscribe-unlink-trading-account-kite-api` | 200 | 69231 | `41d798263954…` | 2026-07-14T08:09:57Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis` | 200 | 68153 | `7ecbe6e74bad…` | 2026-07-14T08:05:48Z | list |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-is-algo-trading` | 200 | 67905 | `f90455143843…` | 2026-07-14T08:09:59Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-is-kite-connect-api-and-who-is-it-for` | 200 | 68982 | `2b79c1c67f23…` | 2026-07-14T08:10:02Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/why-am-i-getting-the-error-maximum-allowed-order-request-exceeded` | 200 | 67956 | `fc30de6da622…` | 2026-07-14T08:10:04Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/will-zerodha-help-me-code-my-strategies-using-kite-api` | 200 | 67063 | `f622acc990f2…` | 2026-07-14T08:10:07Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/login-credentials-of-trading-platforms/articles/time-based-otp-setup` | 200 | 78039 | `659da06d1f3c…` | 2026-07-14T08:10:09Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/kite-features/auctions/articles/participation-in-the-auction` | 200 | 73139 | `7ef40be2d264…` | 2026-07-14T08:10:14Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/kite-features/gtt/articles/what-is-the-validity-of-a-gtt-order` | 200 | 68243 | `fd6c1dd110dd…` | 2026-07-14T08:05:51Z | list |
| `https://support.zerodha.com/category/trading-and-markets/kite-web-and-mobile/kite-mw/articles/what-does-the-average-price-on-kite-3-market-depth-mean` | 200 | 67903 | `45dbae16f2f2…` | 2026-07-14T08:10:17Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/margins/margin-trading-facility/articles/buy-stocks-using-mtf` | 200 | 69820 | `21179d2350ae…` | 2026-07-14T08:10:21Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/product-and-order-types/order/articles/iceberg-orders` | 200 | 73758 | `71bc221ae474…` | 2026-07-14T08:10:24Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/product-and-order-types/order/articles/what-are-cover-orders-and-how-to-use-them` | 200 | 68818 | `666706fd9b12…` | 2026-07-14T08:10:27Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/product-and-order-types/order/articles/what-are-stop-loss-orders-and-how-to-use-them` | 200 | 70394 | `aba2c9b3f955…` | 2026-07-14T08:10:30Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/product-and-order-types/product/articles/what-does-cnc-mis-and-nrml-mean` | 200 | 69471 | `9aaa62161ae9…` | 2026-07-14T08:10:33Z | nav-discovered |
| `https://support.zerodha.com/category/your-zerodha-account/your-profile/articles/how-do-i-place-a-complaint-at-zerodha` | 200 | 66031 | `6e0d090c122c…` | 2026-07-14T08:10:53Z | nav-discovered |
| `https://support.zerodha.com/category/your-zerodha-account/your-profile/ticket-creation/articles/how-do-i-create-a-ticket-at-zerodha` | 200 | 69112 | `a16630e2da20…` | 2026-07-14T08:10:56Z | nav-discovered |
| `https://support.zerodha.com/category/your-zerodha-account/your-profile/ticket-creation/articles/track-complaints-or-tickets` | 200 | 68773 | `de3dda5209a5…` | 2026-07-14T08:10:58Z | nav-discovered |

### 2d. Support-portal category index pages (39, nav-discovered except the kite-api index which was in the list)

Category indexes were crawled for article discovery; only the `kite-api` index carries pack-relevant listings.

| URL | HTTP | bytes | sha256 | fetched_at_utc | source |
|---|---|---|---|---|---|
| `https://support.zerodha.com/category/account-opening` | 200 | 94748 | `b382cb8cbff9…` | 2026-07-14T08:08:19Z | nav-discovered |
| `https://support.zerodha.com/category/account-opening/company-partnership-and-huf-account-opening` | 200 | 76702 | `cf27a3645589…` | 2026-07-14T08:08:22Z | nav-discovered |
| `https://support.zerodha.com/category/account-opening/glossary` | 200 | 71988 | `ab9de65c816d…` | 2026-07-14T08:08:24Z | nav-discovered |
| `https://support.zerodha.com/category/account-opening/minor` | 200 | 68694 | `120ecf27ea50…` | 2026-07-14T08:08:27Z | nav-discovered |
| `https://support.zerodha.com/category/account-opening/nri-account-opening` | 200 | 82027 | `1c2bf74d8f02…` | 2026-07-14T08:08:29Z | nav-discovered |
| `https://support.zerodha.com/category/account-opening/resident-individual` | 200 | 94748 | `57aca18530c3…` | 2026-07-14T08:08:32Z | nav-discovered |
| `https://support.zerodha.com/category/console` | 200 | 78708 | `0740db6cd91e…` | 2026-07-14T08:08:35Z | nav-discovered |
| `https://support.zerodha.com/category/console/corporate-actions` | 200 | 79344 | `086061091da7…` | 2026-07-14T08:08:38Z | nav-discovered |
| `https://support.zerodha.com/category/console/ledger` | 200 | 64242 | `e7fcc69e6b75…` | 2026-07-14T08:08:40Z | nav-discovered |
| `https://support.zerodha.com/category/console/portfolio` | 200 | 78708 | `2f7348190e1f…` | 2026-07-14T08:08:42Z | nav-discovered |
| `https://support.zerodha.com/category/console/profile` | 200 | 71889 | `12ddec1a8574…` | 2026-07-14T08:08:45Z | nav-discovered |
| `https://support.zerodha.com/category/console/reports` | 200 | 75712 | `8fb0eac01e4d…` | 2026-07-14T08:08:47Z | nav-discovered |
| `https://support.zerodha.com/category/console/segments` | 200 | 64563 | `3548304d9b5d…` | 2026-07-14T08:08:50Z | nav-discovered |
| `https://support.zerodha.com/category/funds` | 200 | 72735 | `54c4148b6ce5…` | 2026-07-14T08:08:53Z | nav-discovered |
| `https://support.zerodha.com/category/funds/adding-bank-accounts` | 200 | 69588 | `80af582d417d…` | 2026-07-14T08:08:56Z | nav-discovered |
| `https://support.zerodha.com/category/funds/adding-funds` | 200 | 72735 | `6f2d2c13fea6…` | 2026-07-14T08:08:58Z | nav-discovered |
| `https://support.zerodha.com/category/funds/fund-withdrawal` | 200 | 70627 | `e208d76b4b58…` | 2026-07-14T08:09:00Z | nav-discovered |
| `https://support.zerodha.com/category/funds/mandate` | 200 | 68506 | `07579036fc21…` | 2026-07-14T08:09:03Z | nav-discovered |
| `https://support.zerodha.com/category/mutual-funds` | 200 | 80605 | `88c795e5aa96…` | 2026-07-14T08:09:06Z | nav-discovered |
| `https://support.zerodha.com/category/mutual-funds/coin-general` | 200 | 68548 | `b9d7aad9b170…` | 2026-07-14T08:09:08Z | nav-discovered |
| `https://support.zerodha.com/category/mutual-funds/features-on-coin` | 200 | 75490 | `0acb290a2b77…` | 2026-07-14T08:09:11Z | nav-discovered |
| `https://support.zerodha.com/category/mutual-funds/fixed-deposits` | 200 | 63389 | `893218582e74…` | 2026-07-14T08:09:14Z | nav-discovered |
| `https://support.zerodha.com/category/mutual-funds/nps` | 200 | 67631 | `be30127f0aa7…` | 2026-07-14T08:09:17Z | nav-discovered |
| `https://support.zerodha.com/category/mutual-funds/payments-and-orders` | 200 | 72455 | `b885a622c919…` | 2026-07-14T08:09:20Z | nav-discovered |
| `https://support.zerodha.com/category/mutual-funds/understanding-mutual-funds` | 200 | 80605 | `ea205e0936cb…` | 2026-07-14T08:09:22Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets` | 200 | 80477 | `2f46dbd1ea8c…` | 2026-07-14T08:09:25Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/alerts-and-nudges` | 200 | 83391 | `84f727c729ff…` | 2026-07-14T08:09:27Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/charts-and-orders` | 200 | 103127 | `22f3edf71450…` | 2026-07-14T08:09:29Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite` | 200 | 128393 | `73dbe4eb627d…` | 2026-07-14T08:09:32Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api` | 200 | 67397 | `9ad209c24840…` | 2026-07-14T08:06:39Z | list |
| `https://support.zerodha.com/category/trading-and-markets/ipo` | 200 | 80477 | `7342c609b299…` | 2026-07-14T08:10:12Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/margins` | 200 | 84177 | `2e9dcee6b743…` | 2026-07-14T08:10:19Z | nav-discovered |
| `https://support.zerodha.com/category/trading-and-markets/trading-faqs` | 200 | 115111 | `f9b054b48618…` | 2026-07-14T08:10:36Z | nav-discovered |
| `https://support.zerodha.com/category/your-zerodha-account` | 200 | 88965 | `f8eafaae3013…` | 2026-07-14T08:10:38Z | nav-discovered |
| `https://support.zerodha.com/category/your-zerodha-account/account-modification-and-segment-addition` | 200 | 73509 | `2f90043ecfd3…` | 2026-07-14T08:10:41Z | nav-discovered |
| `https://support.zerodha.com/category/your-zerodha-account/dp-id-and-bank-details` | 200 | 65217 | `c068211158d8…` | 2026-07-14T08:10:43Z | nav-discovered |
| `https://support.zerodha.com/category/your-zerodha-account/nomination-process` | 200 | 64372 | `6cbfe8a80704…` | 2026-07-14T08:10:46Z | nav-discovered |
| `https://support.zerodha.com/category/your-zerodha-account/transfer-of-shares-and-conversion-of-shares` | 200 | 84043 | `9f7656f61306…` | 2026-07-14T08:10:49Z | nav-discovered |
| `https://support.zerodha.com/category/your-zerodha-account/your-profile` | 200 | 88965 | `4365432fbafe…` | 2026-07-14T08:10:51Z | nav-discovered |

### 2e. Kite forum threads (17 — staff-answered limit/policy/pricing evidence cited across the pack)

| URL | HTTP | bytes | sha256 | fetched_at_utc | source |
|---|---|---|---|---|---|
| `https://kite.trade/forum/discussion/11108/` | 200 | 24991 | `3da5d2ea57be…` | 2026-07-14T08:06:15Z | list |
| `https://kite.trade/forum/discussion/11460/kite-historical-data-interval-date-range` | 200 | 25368 | `bfbc5bbd1a6d…` | 2026-07-14T08:06:17Z | list |
| `https://kite.trade/forum/discussion/12851/changes-to-per-day-order-limits` | 200 | 74872 | `ff24ef3ef299…` | 2026-07-14T08:06:20Z | list |
| `https://kite.trade/forum/discussion/13397/rate-limits` | 200 | 32019 | `d9676c3f3cda…` | 2026-07-14T08:06:22Z | list |
| `https://kite.trade/forum/discussion/14149/historical-data-retention-policy` | 200 | 30336 | `99437804f216…` | 2026-07-14T08:06:25Z | list |
| `https://kite.trade/forum/discussion/14806/historical-data-is-now-free-with-base-kite-connect-subscription` | 200 | 122933 | `5e30b515e91d…` | 2026-07-14T08:06:27Z | list |
| `https://kite.trade/forum/discussion/14868/introducing-kite-connect-personal-apis-free-apis-for-personal-use` | 200 | 103270 | `63a4592fcf84…` | 2026-07-14T08:06:29Z | list |
| `https://kite.trade/forum/discussion/15015/revising-kite-connect-fees-from-2000-to-500-per-month` | 200 | 86664 | `edd735eba48e…` | 2026-07-14T08:06:32Z | list |
| `https://kite.trade/forum/discussion/15111/historical-data-limit-60-days` | 200 | 25518 | `9ee6a5232ff8…` | 2026-07-14T08:06:34Z | list |
| `https://kite.trade/forum/discussion/15708/` | 200 | 36729 | `be5b4336e709…` | 2026-07-14T08:06:36Z | list |
| `https://kite.trade/forum/discussion/2354/what-are-the-limits-of-kite-api` | 200 | 54743 | `914a0a6b025c…` | 2026-07-14T08:05:59Z | list |
| `https://kite.trade/forum/discussion/2875/is-there-any-limitation-on-getting-historical-data` | 200 | 40881 | `078158fa8af8…` | 2026-07-14T08:06:01Z | list |
| `https://kite.trade/forum/discussion/3018/how-to-get-historical-data-for-expired-futures` | 200 | 85172 | `4d8de845bf80…` | 2026-07-14T08:06:03Z | list |
| `https://kite.trade/forum/discussion/3059/do-i-need-to-download-instrument-list-daily-do-you-change-instrument-token-daily` | 200 | 30512 | `d30db8680579…` | 2026-07-14T08:06:06Z | list |
| `https://kite.trade/forum/discussion/6292/historical-data-for-expired-contracts` | 200 | 28171 | `98ebeaa8c365…` | 2026-07-14T08:06:08Z | list |
| `https://kite.trade/forum/discussion/8577/api-rate-limits` | 200 | 36328 | `e440872bbd00…` | 2026-07-14T08:06:10Z | list |
| `https://kite.trade/forum/discussion/8722/is-intraday-1-minute-historical-data-available-until-2015` | 200 | 39183 | `d3e4f81aa166…` | 2026-07-14T08:06:12Z | list |

### 2f. Other nav-discovered assets (34: stylesheets, `.`/`..`/`./` self-referential nav-link duplicates of docs pages, and the single 404)

The `..`/`.`/`./` rows are byte-identical duplicates of the root/section pages (matching sha256s prove it).
The **only non-200 in the entire crawl** is the favicon asset row below (404, 2 attempts) — an image asset,
not a documentation page; zero doc-page fetch failures.

| URL | HTTP | bytes | sha256 | fetched_at_utc | source |
|---|---|---|---|---|---|
| `https://kite.trade/docs/connect/v3/.` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:06:41Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/alerts/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:06:44Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/alerts/./` | 200 | 74055 | `e5a98a412673…` | 2026-07-14T08:06:46Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/assets/stylesheets/main.26e3688c.min.css` | 200 | 113505 | `26e3688c3464…` | 2026-07-14T08:06:50Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/assets/stylesheets/main.8b42a75e.min.css` | 200 | 91394 | `a5e0c38f2af1…` | 2026-07-14T08:06:53Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/assets/stylesheets/palette.3f5d1f46.min.css` | 200 | 10608 | `4098df2195f8…` | 2026-07-14T08:06:55Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/assets/stylesheets/palette.ecc896b0.min.css` | 200 | 12245 | `ecc896b06a48…` | 2026-07-14T08:06:57Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/exceptions/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:05Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/exceptions/./` | 200 | 18620 | `a51b5b88ed1c…` | 2026-07-14T08:07:07Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/gtt/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:10Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/gtt/./` | 200 | 62261 | `f43a98209a0e…` | 2026-07-14T08:07:12Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/historical/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:14Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/historical/./` | 200 | 30035 | `7ff795c16b16…` | 2026-07-14T08:07:16Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/images/favicon` | 404 | 146 | `55f7d9e99b8e…` | 2026-07-14T08:07:24Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/margins/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:26Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/margins/./` | 200 | 72997 | `7a60762b534c…` | 2026-07-14T08:07:29Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/market-data-and-instruments/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:31Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/market-quotes/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:33Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/market-quotes/./` | 200 | 45878 | `a990876014d9…` | 2026-07-14T08:07:35Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/mutual-funds/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:38Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/mutual-funds/./` | 200 | 80613 | `7ed998f908ca…` | 2026-07-14T08:07:40Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/orders/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:42Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/orders/./` | 200 | 157324 | `919e7f90baae…` | 2026-07-14T08:07:45Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/portfolio/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:47Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/portfolio/./` | 200 | 95783 | `55312b2a7c1f…` | 2026-07-14T08:07:50Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/postbacks/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:07:52Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/postbacks/./` | 200 | 26130 | `c603c9c96b1a…` | 2026-07-14T08:07:55Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/response-structure/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:08:00Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/response-structure/./` | 200 | 16821 | `14e8cf28f3d7…` | 2026-07-14T08:08:02Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/static/style.css` | 200 | 1928 | `3d2e15acdc43…` | 2026-07-14T08:08:07Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/user/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:08:10Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/user/./` | 200 | 50544 | `bea61798efcb…` | 2026-07-14T08:08:12Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/websocket/..` | 200 | 17583 | `5edfd9a6b40e…` | 2026-07-14T08:08:14Z | nav-discovered |
| `https://kite.trade/docs/connect/v3/websocket/./` | 200 | 32567 | `c750c431e724…` | 2026-07-14T08:08:17Z | nav-discovered |

---

## 3. Coverage fraction

**19/19 live docs-nav sections fetched-verbatim (20/20 primary docs pages incl. the legacy out-of-nav mirror).
148/148 inventoried URLs attempted; 147/148 HTTP 200; the single non-200 is the `images/favicon` 404 (asset, not a
doc page). 46 list URLs + 102 nav-discovered. `developers.kite.trade/` fetched but LOGIN-GATED (public login page
only). Every live-nav section maps to a pack file (files 16–17 were created for the 5 sections the derived
2026-07-13 inventory missed: apps/ basket/ changelog/ publisher/ sdks/).**

---

## 4. Verification protocol — **COMPLETE (2026-07-14)**

The 2026-07-13 five-step protocol has been executed in full:

| Step | Outcome |
|---|---|
| 1. Flip statuses honestly | DONE — all 148 rows carry live statuses from `fetch-log.tsv` verbatim, incl. the one 404 and the LOGIN-GATED classification. |
| 2. Replace the derived nav with the site's own | DONE — §1 is rebuilt from the live root page's sidebar (19 entries). The 5 sections the derived inventory missed (apps/basket/changelog/publisher/sdks) got new pack files 16–17; `alerts/` URL confirmed real; `market-data-and-instruments/` demoted to legacy mirror. |
| 3. Re-verify every pack claim | DONE — files 01–17 re-verified against the live HTML; 200+ claims upgraded to **Verified (LIVE-DOC runner 2026-07-14)**, 17 corrected in place with dated notes (token expiry 6 AM, orders/min 400, orders/day 5,000, iceberg_legs 2–50, 20-vs-19 caps pages, Kite Connect launch 2016, endianness attribution, etc.). |
| 4. Close the paired UNKNOWNS | DONE — each file's OPEN QUESTIONS rewritten with "RESOLVED by live fetch" notes; `99-UNKNOWNS.md` rebuild consumes them. |
| 5. Adjudicate the two numeric conflicts | DONE — orders/min = **400** and orders/day = **5,000** per the live exceptions page + both live support articles (superseded figures kept as history). |

**Next refresh:** re-run the `docs-fetch.yml` workflow (same URL list + nav crawl) on a fresh
`docs-fetch/zerodha-<date>` branch; diff the new `fetch-log.tsv` sha256s against this ledger — any changed digest
means the live page changed and the citing pack file(s) must be re-verified. Any NEW nav entry in the root page's
sidebar = a new topic file (or an explicit N/A row) before tiers stay current. Known follow-ups for the next run:
forum thread `discussion/4732` (root-page FAQ link, never fetched) and the login-gated developer-console surfaces
(need an authenticated session, not a crawler).
