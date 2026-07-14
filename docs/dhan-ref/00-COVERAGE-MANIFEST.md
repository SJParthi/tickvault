# Dhan Docs Coverage Manifest — 2026-07-14 runner crawl

> **Purpose**: the completeness record for the 2026-07-13/14 live verification of
> `docs/dhan-ref/*` — derived from the SITES' OWN sitemaps/nav, not from this repo's file
> structure. Answers: "of everything Dhan publishes, what did we fetch, when, and what did we
> deliberately not fetch?"
> **Crawl bundle**: branch `docs-fetch/dhan-2026-07-13` of the crawl repo
> (`crawl-out/fetch-log.tsv` — columns url | status | bytes | sha256 | fetched_at_utc |
> attempts | discovered; `SUMMARY.md`; `discovered-urls.txt`; `pages/`). Runs: run 1
> 2026-07-13T19:35–19:37Z (GH-Actions run 29279025906), run 2 2026-07-14T00:05–00:07Z
> (29294952698), run 3 2026-07-14T07:57–08:06Z (29316310511 — the CANONICAL run cited below:
> it covers classic + portal in one 191-row manifest, 191/191 HTTP 200, 0 failures).
> Classic-page sha256s are content-identical across all three runs.
> **Companion**: `verification-2026-07-13.md` (the verification record; its 2026-07-14
> section supersedes the search-index tiers).

---

## 1. Classic surface — `dhanhq.co/docs/v2/*` (22 pages fetched)

Universe per the site's own `https://dhanhq.co/sitemap-api-docs.xml` (fetched
2026-07-14T00:07:15Z, 19 URLs) **plus 4 nav-discovered pages NOT in that sitemap**
(authentication, conditional-trigger, expired-options-data, full-market-depth — real,
200-serving pages the sitemap omits). All fetched pages returned HTTP 200 with distinct
sha256s and real server-rendered prose (zero WAF markers). Timestamps below = run 3;
runs 1–2 fetched the identical content (same sha256 per page).

| Live URL | Status | sha256 (8) | Fetched (UTC) | Verdict |
|---|---|---|---|---|
| https://dhanhq.co/docs/v2/ | 200 | `d777e139` | 2026-07-14T07:57:28Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/authentication/ † | 200 | `1c2546f0` | 2026-07-14T07:57:34Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/live-market-feed/ | 200 | `6bd6ebc3` | 2026-07-14T07:57:37Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/full-market-depth/ † | 200 | `6fdc8b09` | 2026-07-14T07:57:41Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/historical-data/ | 200 | `5ef184e4` | 2026-07-14T07:57:44Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/option-chain/ | 200 | `88a50d44` | 2026-07-14T07:57:47Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/orders/ | 200 | `e09e187d` | 2026-07-14T07:57:50Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/super-order/ | 200 | `4a04b343` | 2026-07-14T07:57:53Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/forever/ | 200 | `91bc414b` | 2026-07-14T07:57:56Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/conditional-trigger/ † | 200 | `f3ddbe28` | 2026-07-14T07:58:00Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/edis/ | 200 | `b3300a8d` | 2026-07-14T07:58:03Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/postback/ | 200 | `1ce54e4e` | 2026-07-14T07:58:06Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/annexure/ | 200 | `1a9330a1` | 2026-07-14T07:58:09Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/instruments/ | 200 | `36a9b707` | 2026-07-14T07:58:12Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/order-update/ | 200 | `424a29f2` | 2026-07-14T07:58:15Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/market-quote/ | 200 | `505e0ef6` | 2026-07-14T07:58:18Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/portfolio/ | 200 | `afbf4321` | 2026-07-14T07:58:21Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/funds/ | 200 | `4244823f` | 2026-07-14T07:58:23Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/statements/ | 200 | `f25a5deb` | 2026-07-14T07:58:26Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/traders-control/ | 200 | `a1f3da12` | 2026-07-14T07:58:29Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/releases/ | 200 | `54116c5c` | 2026-07-14T07:58:32Z | fetched-verbatim |
| https://dhanhq.co/docs/v2/expired-options-data/ † | 200 | `1b970d47` | 2026-07-14T07:58:35Z | fetched-verbatim |

† = fetched but NOT in the site's sitemap-api-docs.xml (nav-discovered).

**The ONE sitemap URL not fetched:** `https://dhanhq.co/docs/v2/20-market-depth/` — a legacy
alias of the full-market-depth page (which WAS fetched). Reason: not in the crawl URL list
and not nav-linked from any fetched page. Impact ≈ 0: depth WebSockets are FORBIDDEN FOREVER
in tickvault (`websocket-connection-scope-lock.md`), and the canonical depth page was
captured. Recorded solely for manifest completeness.

**Classic coverage: 22 of 23 sitemap∪fetched URLs = 95.7% of the sitemap-defined universe
(18/19 sitemap + 4/4 nav extras).**

Also fetched: `dhanhq.co/sitemap.xml` (312 B index) + `sitemap-api-docs.xml`.

---

## 2. New portal — `docs.dhanhq.co` (sitemap universe 196; api/v2 = 149/149 fetched)

The portal's `sitemap.xml` (fetched 2026-07-14T00:07:21Z) lists **196 URLs**: 1 root +
**149 `/api`** + 14 `/cloud` + 12 `/mcp` + 19 `/skills` + 1 `/search`.

**Crucial fetch-method fact:** the portal's HTML routes are a client-rendered SPA — every
`docs.dhanhq.co/<html-route>` fetch returns the IDENTICAL 33,884-byte JS shell (sha
`41b7721e`). The REAL per-page content is served at the parallel markdown routes
`https://docs.dhanhq.co/markdown/<route>.md`, which run 3 fetched with distinct sha256s.

| Portal universe | Count | Fetched | Verdict |
|---|---|---|---|
| `/api/v2/**` markdown routes (`/markdown/api/v2/…md`) | 149 | **149/149 = 100%** | fetched-verbatim; the sitemap `/api` set vs the fetched `/markdown` set diff is an IDENTICAL 1:1 mapping |
| Machine artifacts | 4 | 4/4 | `docs-export.md` (388,707 B, 07:58:44Z) · `openapi/dhan-api-v2.yaml` (113,906 B, OpenAPI 3.1.0, 43 paths incl. 10 globalstocks, 07:58:48Z) · `llms.txt` · `sitemap.xml` |
| Rendered-HTML spot-checks (consistency sampling of the SPA shell) | 14 | 14 | `/` + `/api/v2/` + 12 rendered route pages — all the same shell sha `41b7721e`, confirming the SPA-shell finding |
| `/cloud` (14) · `/mcp` (12) · `/skills` (19) · `/search` (1) | 46 | 0 | **enumerated, out-of-scope**: DhanHQ Cloud (strategy-hosting product), a DhanHQ MCP-server product, agent skills, and site search — none is API documentation for the v2 trading/data surface tickvault references. Existence recorded; no dhan-ref counterpart needed. |

Per-section breakdown of the 149 `/api/v2` markdown routes (all fetched): schemas 43 ·
global-stocks 20 · guides 17 · orders 10 · authentication 9 · conditional-triggers 7 ·
traders-control 6 · super-orders 5 · portfolio 5 · market-quote 4 · funds 4 · edis 4 ·
statements 3 · option-chain 3 · historical-data 3 · expired-options-data 2 · trading-apis 1 ·
sandbox 1 · data-apis 1 · v2 index 1. Note: the 43 `schemas/*` pages are auto-generated
per-schema stubs (several EMPTY, e.g. `schemas/convertpositionrequest.md` is just a heading)
— the OpenAPI yaml is the real schema source.

**Portal /api/v2 coverage: 149/149 = 100%.**

---

## 3. NEW-DOCS FINDINGS — portal pages with NO dhan-ref counterpart

| Portal page | What it is | Relevance to tickvault |
|---|---|---|
| `sandbox.md` (08:02:56Z) | **NEW: a real sandbox environment** — base URL `https://sandbox.dhan.co/v2`, sandbox tokens minted at `developer.dhanhq.co`, same `access-token` header, prod/sandbox tokens NOT interchangeable; ~23 sandbox-enabled endpoints (orders CRUD, slicing, trade book, holdings, positions, convert, killswitch, eDIS, fundlimit, margincalculator, ledger, trade history, charts/intraday, charts/historical) | A test environment for order-path code exists; dhan-ref has nothing on it. Candidate future reference file if the order path is ever re-activated. |
| `guides/errors.md` | NEW error taxonomy (`VALIDATION_ERROR`/`E001`/`RL001` + HTTP codes) that CONFLICTS with both annexures' DH-901..910 + 800-814 catalogue | Treat as generated/aspirational — do NOT adopt E-codes; the DH-9xx catalogue stands on both annexures. |
| `guides/rate-limits.md` | Dedicated rate-limit guide — carries the TRANSPOSED daily values (see `01-introduction-and-rate-limits.md` 2026-07-14 note) + best practices + a rate-limit error JSON | Demonstrably corrupt on the dailies; trust the primary tables. |
| `guides/sdks/python.md` | Official Python SDK reference (DhanContext pattern, method list) | dhan-ref has no SDK doc (the separate `dhanhq` skill covers it). |
| `data-apis.md` / `trading-apis.md` | Pure index pages grouping the endpoint families (the trading index omits Forever Orders — nav-value only) | None. |
| `guides/getting-started.md` | Quickstart ("Browse all 31+ API endpoints") | No new facts. |
| `conditional-triggers/place-multi-order.md` | Multi-order alerts endpoint `/alerts/multi/orders` (also in the yaml) | The repo's conditional-trigger ref predates this route; no consumer exists. |
| `authentication/partner-generate-consent.md` / `partner-consume-consent.md` | Partner OAuth flow split into own pages | Repo covers partner auth briefly in 02; unused by tickvault. |
| `global-stocks/*` (20 routes + 10 yaml paths) | US-stocks trading API (orders, holdings, funds, market status, transEstimate…) | OUT OF SCOPE for tickvault; existence noted. |

Also re-confirmed on the FULL 191-page crawl: **no page on either surface documents how
quickly a just-closed intraday candle becomes available** — the `spot_1m_rest`
measure-don't-assume envelope stands.

---

## 4. Fetch method + honest envelope

- **Method:** a GitHub-Actions runner (the reusable `docs-fetch.yml` workflow; runner IPs are
  NOT blocked by Dhan) fetched raw HTTP responses and committed them with a
  sha256-per-URL manifest. No JS execution, no browser — which is exactly why the portal's
  HTML routes yielded only the SPA shell and the `/markdown/*.md` routes were used for real
  portal content.
- Every "Verified-live"/"fetched-verbatim" claim = verbatim server response bytes at the
  logged timestamp. Diffs against dhan-ref were **comment-aware** (content inside HTML
  comments counted as non-rendered; absences also checked inside comments).
- **NOT covered:** the portal's rendered (post-JS) HTML pages — if a portal page's rendered
  content diverges from its own `/markdown/` export, that divergence is invisible to this
  crawl. The `/cloud` `/mcp` `/skills` `/search` sections were enumerated but not fetched
  (out of scope). The classic legacy alias `20-market-depth/` was not fetched (§1).
- **No live API endpoint was probed by this crawl** — wire behavior for every contested enum
  (ExpiryCode numbering, depth unsubscribe code, multi-margin request shape, getIP response
  shape) remains UNVERIFIED-LIVE and gated on live probes from the prod box.
- 8 stale page files from earlier runs were identified and EXCLUDED from the run-3 manifest
  accounting (superseded fetch attempts of renew-token / calculate-multi-margin /
  rate-limits.md HTML routes + llms-full.txt).

---

## 5. Coverage fractions (plain statement)

| Surface | Universe (site-defined) | Fetched | Fraction |
|---|---|---|---|
| Classic `dhanhq.co/docs/v2` | 23 (19-URL sitemap ∪ 4 nav extras) | 22 | **95.7%** (the 1 miss = the legacy `20-market-depth/` alias; impact ≈ 0) |
| Portal `/api/v2` markdown routes | 149 | 149 | **100%** |
| Portal machine artifacts | 4 | 4 | 100% |
| Portal `/cloud` + `/mcp` + `/skills` + `/search` | 46 | 0 | 0% — deliberately out of scope (not v2 API docs) |
| Run-3 manifest total | 191 fetch attempts | 191 × HTTP 200 | 0 failures |
