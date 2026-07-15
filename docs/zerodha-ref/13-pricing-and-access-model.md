# Zerodha Kite Connect — Pricing & Access Model (reference)

> **Source:** `https://kite.trade/` (homepage) · `https://zerodha.com/products/api/` · `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis` · `https://support.zerodha.com/...articles/historical-data-and-live-market-data-payment-plan` · `https://support.zerodha.com/...articles/can-i-subscribe-to-historical-api-without-subscribing-to-kite-connect-api` · `https://support.zerodha.com/...articles/how-do-i-sign-up-for-kite-connect` · `https://support.zerodha.com/...articles/invoice-kite-connect-api` · `https://support.zerodha.com/...articles/kite-connect-api-faqs` · `https://zerodha.com/z-connect/updates/free-personal-apis-from-kite-connect` · `https://zerodha.com/z-connect/kite/kite-connect-apis-for-programmatic-access` · `https://kite.trade/forum/discussion/14806/...` · `https://kite.trade/forum/discussion/14868/...` · `https://kite.trade/forum/discussion/15015/...` · `https://kite.trade/startups/` · `https://kite.trade/publisher/` · `https://kite.trade/docs/connect/v3/publisher/` · `https://developers.kite.trade/`
> **Fetched-Verified:** 2026-07-14 — **live-verbatim via GH-runner 2026-07-14 (UTC timestamps in 00-COVERAGE-MANIFEST.md)**. Every URL above was fetched HTTP-200 by the LIVE-DOC runner (fetch-log carries per-URL sha256 + timestamp); claims below labeled **Verified (LIVE-DOC runner 2026-07-14)** are read byte-verbatim from those pages. The 2026-07-13 SEARCH-only provenance (sandbox egress 403-blocked) is superseded for everything the live pages cover; SEARCH/ARITH labels are retained only as cross-check notes or where the live pages are silent. **`https://developers.kite.trade/` is LOGIN-GATED** — the live fetch returned the public login page only ("Login / Kite Connect developer — E-mail / Password"); the developer console (create-app form, Billing tab) behind it is NOT publicly fetchable, so console-internal details keep their weaker tiers.
> **Evidence tiers:** see README legend. Top tier for this file: **Verified (LIVE-DOC runner 2026-07-14)**. Legacy sub-forms **SEARCH-title** / **SEARCH-extract** appear only in residual notes.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. No Zerodha account exists in this project; nothing here authorizes signing up, paying, or fetching.

---

## 0. Provenance status — 2026-07-14 live upgrade

The 2026-07-13 version of this file was the pack's highest-hallucination-risk file
(every rupee figure search-derived). That is no longer true: on **2026-07-14** the
LIVE-DOC runner fetched every primary pricing page (kite.trade root, products/api,
the support charges + payment-plan articles, all three announcement forum threads
with their exact OP timestamps, both z-connect posts, startups/, publisher/).
**Every load-bearing number below is now live-byte-verified**, and the search-era
timeline was confirmed with ONE correction (the launch year — §2). Residual
non-live areas: the developer console (login-gated — credits denomination, exact
create-app form), and GST treatment (stated nowhere on the fetched pages).

---

## 1. Current pricing (Verified LIVE-DOC runner 2026-07-14)

| Plan / app type | Price | What it includes |
|---|---|---|
| **Kite Connect Personal** | **Free** | "Full-fledged order, GTT, alerts management · Margin computation, portfolio management, etc. · **No historical or real-time data included**" |
| **Kite Connect** (paid) | **₹500 / month per API key** (charges article words it "₹500 per app each month") | "Everything in Personal plus: Real-time data via WebSockets · Historical candle data" — historical + live data at **no additional cost** |
| **Kite Publisher** | **Free** ("No, Kite Publisher is available free of charge") | HTML/JS trade buttons; order execution via inline Kite popup; no data, no REST surface |
| **Startups (mass-retail platforms)** | **Free** | "For startups working on mass retail products: Kite Connect APIs are free" (§7) |

- **Verified (LIVE-DOC runner 2026-07-14, support charges article):** "Personal (Free):
  Full-fledged order, GTT, alerts management; Margin computation, portfolio
  management, etc.; No historical or real-time data included. Connect - ₹500/month:
  Everything in Personal plus: Real-time data via WebSockets; Historical candle
  data; Available for retail users at ₹500 per app each month" —
  `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis`
  (in fetch-log).
- **Verified (LIVE-DOC runner 2026-07-14, payment-plan article):** "Kite Connect Personal
  API (Free): This plan allows you to place orders and track your positions,
  holdings, and funds. It does not include live market data or historical data.
  Kite Connect (Paid) API: You get all Personal API features plus access to live
  market data and historical data at no additional cost. This plan costs **₹500
  per month per API key**." —
  `https://support.zerodha.com/...articles/historical-data-and-live-market-data-payment-plan`.
- **Verified (LIVE-DOC runner 2026-07-14, kite.trade homepage pricing block):** "Pricing —
  Start for free, scale as you grow. **Personal (Free)** Full fledged order, GTT,
  alerts management. Margin computation, portfolio management etc. **Connect** Full
  suite of APIs with realtime WebSocket streaming and historical candle data for
  **₹ 500 / month**" — `https://kite.trade/`. `https://zerodha.com/products/api/`
  serves a byte-length-identical, semantically identical page (same 30,127-byte
  length in the fetch-log; distinct sha256s — the two bodies differ only in
  Cloudflare email-obfuscation nonces).
- The old official X-post citation (`x.com/zerodhaonline/status/1912823918823702875`,
  SEARCH-title 2026-07-13) remains as cross-check evidence for the Personal scope
  wording ("as long as you have a market data [source]") — the live forum OP (§2)
  now carries the same wording verbatim.
- **GST treatment of the ₹500 (inclusive vs + 18% GST) — Unknown** (U-P5): none of
  the fetched live pages states it; the invoice exists (§6) but its line items are
  behind the login-gated console.

**Compare: Dhan/Groww.** Dhan splits identically — "Trading APIs (free for all Dhan
users)" vs "Data APIs (additional charges)" (`docs/dhan-ref/01-introduction-and-rate-limits.md`
lines 60–63; the rupee amount of Dhan's data plan is NOT stated anywhere in the repo
ref). Groww's Trading API documents **no API-access fee at all** anywhere in
`docs/groww-ref/` (grep 2026-07-13 — no ₹/pricing/subscription-fee statement exists in
that pack). Kite's post-2025 model (free execution / ₹500 data) is therefore the
structural middle: same free-execution split as Dhan, but with a published flat data
price; Groww publishes no price at all.

## 2. Pricing timeline (2016 → 2026) — dated, live-verified

| When | Event | Evidence |
|---|---|---|
| **2016-04-20** | Kite Connect launched — "India's first market APIs for retail clients" (Nithin Kamath announcement post). **CORRECTION vs the 2026-07-13 SEARCH version of this file, which said 2015:** the live launch post is dated April 20, 2016, and the live z-connect update post says "We launched the Kite Connect suite of trading APIs **in 2016**". (Kite, the trading PLATFORM, launched 2015 — the earlier search extraction conflated the two.) | Verified (LIVE-DOC runner 2026-07-14): `https://zerodha.com/z-connect/kite/kite-connect-apis-for-programmatic-access` (post header "April 20, 2016") + `https://zerodha.com/z-connect/updates/free-personal-apis-from-kite-connect` |
| pre-2025 (long-standing) | **₹2,000 / month per app** for Kite Connect + **₹2,000 / month historical-data add-on** (separate subscription) — ₹4,000/mo all-in for the full stack | Verified (LIVE-DOC runner 2026-07-14): z-connect update — "we were charging ₹ 2000 for data and ₹ 2000 for historical data"; forum 14806 OP — "We will no longer charge an additional ₹2000 fee for historical data" |
| **2025-02-08** (effective; announced **2025-02-10T06:43:58Z**) | Historical-data add-on charge **removed** — "With effect from February 8, 2025, we've stopped charging for the historical data add-on on Kite Connect. All existing connect apps have been granted historical data permission and new apps will come with the subscription auto-enabled." The UI option to subscribe to the add-on was removed from the developer console. | Verified (LIVE-DOC runner 2026-07-14): `https://kite.trade/forum/discussion/14806/historical-data-is-now-free-with-base-kite-connect-subscription` — OP by Matti, post `datetime="2025-02-10T06:43:58+00:00"` in the fetched HTML |
| **2025-03-14T08:39:24Z** | **Kite Connect Personal launched — FREE**: "free access to your Zerodha account with all the essential features of Kite Connect, minus the market data (both real-time and historical). You can place orders, track your positions, holdings, and funds … as long as you have a source for market data. To get started, simply create a new Personal app on the Developer Console." | Verified (LIVE-DOC runner 2026-07-14): `https://kite.trade/forum/discussion/14868/introducing-kite-connect-personal-apis-free-apis-for-personal-use` — OP by Matti, post `datetime="2025-03-14T08:39:24+00:00"` (edited 2025-03-17). The earlier ARITH decodes (X 2025-04-17, LinkedIn 2025-04-24) were indeed later promotion, as the 2026-07-13 file guessed |
| **2025-04-24** | Z-connect "Updates" post recaps the changes — snapshot of the April 2025 state: "we've made the execution APIs free, and we only charge ₹ 2000 for data" (i.e. the data fee was STILL ₹2,000 between the Personal launch and the May cut) | Verified (LIVE-DOC runner 2026-07-14): `https://zerodha.com/z-connect/updates/free-personal-apis-from-kite-connect` (post dated "24 Apr 2025") |
| **2025-05-06T11:05:25Z** | Full Connect fee **revised ₹2,000 → ₹500 / month** for the data (real-time + historical) APIs, prompted by NSE's retail-algo operational circular: "With this new regulatory clarity, our regulatory risk in offering the product is greatly reduced… The order placement and account management APIs (view holdings, positions, etc.) **have been free since March 2025**. The 500 rupee fee ensures that only users with serious intent sign up to use the APIs since streaming data comes with a significant bandwidth cost for us." The old ~2025-05-06 Assumed date is now byte-exact (a same-day commenter: "the post was released at 11.05AM"). | Verified (LIVE-DOC runner 2026-07-14): `https://kite.trade/forum/discussion/15015/revising-kite-connect-fees-from-2000-to-500-per-month` — OP by Matti, post `datetime="2025-05-06T11:05:25+00:00"` |
| **2026-07-14** | Current live state = §1: Personal free, Connect ₹500/mo per API key with live + historical data bundled, Publisher free, startups free for mass-retail platforms | Verified (LIVE-DOC runner 2026-07-14): kite.trade root + zerodha.com/products/api + both support pricing articles, all fetched 2026-07-14 |

Related live facts from the 2025-05-06 thread (Matti replies, same page — cross-refs
for other pack files): **no-refund policy** on the downward price revision
("we have a no refund policy… very difficult to do any downward revision of pricing
if we're expected to retroactively refund"); **static IP requirement** for the order
place/modify/cancel endpoints only (not other APIs), "expected to go live by August 1
[2025]" — the static-IP topic's authoritative treatment is the support `static-ip`
article (auth/orders files' remit).

## 3. The "2023-09 free announcement" hypothesis — REFUTED by the live timeline

The original task brief hypothesized a **September 2023** Zerodha announcement making
Kite Connect free for individual retail users. Four targeted searches on 2026-07-13
found zero source, and the **2026-07-14 live pages now positively refute it**: the
z-connect update post (24 Apr 2025, Verified LIVE-DOC) states Zerodha "were charging
₹2000 for data and ₹2000 for historical data" right up until the 2025 changes, and
the 2025-05-06 forum OP states the execution APIs "have been free **since March
2025**" — i.e. nothing was free before 2025. **Verdict: no 2023 free-API event
existed.** Do NOT cite a 2023 free date anywhere in tickvault; the authoritative
free-API date is the Personal launch, **2025-03-14**. (U-P2 — RESOLVED by live fetch,
closed as refuted.)

## 4. The historical-data add-on (retired 2025-02-08) — live-verified

- **Old model (pre-2025-02-08):** historical candle data was a **separate ₹2,000/month
  add-on subscription per app**, on top of the ₹2,000/month Connect fee — full stack
  ₹4,000/month/app. Verified (LIVE-DOC runner 2026-07-14): forum 14806 OP + z-connect update
  (§2).
- **New model:** no separate historical product exists — historical is a bundled
  permission of the (now ₹500/month) Connect subscription, auto-enabled on new apps;
  the add-on subscribe option was removed from the developer console. Verified
  (LIVE-DOC 2026-07-14): forum 14806 OP; payment-plan article: "you do not need to
  pay separately for historical data and live market data… Both … are included at no
  additional cost with your paid subscription."
- **Historical still requires the base Connect subscription** (a Personal/free key
  gets no historical): support FAQ page, Verified (LIVE-DOC runner 2026-07-14) — "Can I
  subscribe to historical API without subscribing to Kite Connect API? **No, you
  must subscribe to Kite Connect API first to subscribe to historical API.**"
  (resolves the old U-P4 answer-text unknown). Same statement by staff on the 14806
  thread: "The add-on historical data subscription is free but you will still need
  the base Kite Connect subscription to fetch historical data."
- The "up to 10 years of intraday data" scope claim from the search era remains
  **NOT covered by these live pages** — the depth window is the historical-API
  section file's topic (its own live sources govern; still open there).
- Bonus live facts from the 14806 thread (staff replies, forum tier): historical API
  throttle "up to 3 requests per second"; expired FUTURES instruments get day-candle
  data only; NO historical for expired OPTION contracts. (Cross-ref the historical +
  rate-limits files.)
- **Compare: Dhan/Groww.** Dhan classes Historical Data inside the paid "Data APIs"
  group (`docs/dhan-ref/01-introduction-and-rate-limits.md:63`); Groww's
  `GET /v1/historical/candles` carries no documented fee (`docs/groww-ref/11-historical-candles.md`
  — no pricing statement in the pack). Kite is now the same shape as Groww here
  (historical included with API access), differing from Dhan's separately-charged
  data class.

## 5. App types: Connect vs Personal vs Publisher

| App type | Cost | Capability | Audience |
|---|---|---|---|
| **Connect** | ₹500/mo/API key (§1) | Full REST + WebSocket surface: orders, portfolio, live streaming data, historical | Startups/platforms + individuals who need data |
| **Personal** | Free | Orders + positions/holdings/funds, GTT, alerts, margin computation; **NO live or historical market data** ("as long as you have a source for market data" — forum 14868 OP, Verified LIVE-DOC) | Individual retail users automating their own account |
| **Publisher** | Free | Embeddable HTML/JS trade buttons; click opens an inline Kite popup where the trade executes; basket of up to 10 stocks via the JS plugin; **order execution only** — no data, no account API | Websites/content platforms offering "trade this" buttons |

- **Personal IS a selectable plan/app type** (old U-P3, now largely resolved):
  the sign-up article (Verified LIVE-DOC runner 2026-07-14) says "Choose your Kite Connect
  plan — You can select from two available plans: Kite Connect Personal API (Free)…
  Kite Connect (Paid) API…", and the 14868 OP says "create a new **Personal app** on
  the Developer Console". Personal apps also get an `api_key` + `api_secret` (the
  launch-week missing-secret bug was fixed in-thread, March 2025).
- Publisher evidence — Verified (LIVE-DOC runner 2026-07-14): product page
  `https://kite.trade/publisher/` ("Is there any fees? **No, Kite Publisher is
  available free of charge.** … There are no approvals required. When a user clicks
  on a Kite Publisher button … an inline popup opens with Kite … where the trade
  happens"); docs page `https://kite.trade/docs/connect/v3/publisher/` (JS plugin
  `https://kite.trade/publisher.js?v=3`, `<kite-button>` tags, basket max 10, and
  note that the flow's `request_token` can be captured to start a Kite Connect
  session — "offsite order execution"). Publisher apps are created from the same
  developer console ("use the 'Create +' button to create a Publisher app and obtain
  API keys").
- **Compare: Dhan/Groww.** Neither Dhan nor Groww has a Publisher-button analogue or
  a multi-app-type model in the repo refs; both are single-surface API products
  keyed to the user's own account. Kite's per-APP (not per-user) pricing matters for
  cost modeling: N apps = N × ₹500/mo.

## 6. Developer signup + billing flow (developers.kite.trade)

`https://developers.kite.trade/` is **LOGIN-GATED**: the live fetch (2026-07-14)
returned only the public login page ("Login / Kite Connect developer — E-mail /
Password / Forgot password? / Signup"). Everything console-internal below is from the
live SUPPORT articles, not the console itself:

1. **Sign up** at `developers.kite.trade/signup` — registration form: Kite Connect
   email (preferably the Zerodha-linked one), name, password, phone, state of
   residence, T&C checkbox. Verified (LIVE-DOC runner 2026-07-14):
   `https://support.zerodha.com/...articles/how-do-i-sign-up-for-kite-connect`.
2. **Choose plan:** Personal (Free) or paid Connect (₹500/month per API key). Same
   article, Verified (LIVE-DOC runner 2026-07-14).
3. **Create app** (My Apps → Create New App): App Name, Zerodha Client ID, Redirect
   URL (localhost OK for testing, e.g. `http://127.0.0.1:8000`), optional Postback
   URL, description → issues an **API Key + API Secret**. Same article, Verified
   (LIVE-DOC 2026-07-14).
4. **Payment & invoices:** paid either "directly using the payment gateway on Kite
   Connect (**Razorpay**)" — invoice downloadable from the billing page's payment
   history — or "debited from your **Zerodha account**" — invoice from Console.
   Verified (LIVE-DOC runner 2026-07-14):
   `https://support.zerodha.com/...articles/invoice-kite-connect-api`. This also
   confirms the search-era claim that the trading account can be linked for
   deduction; the billing page has an **Unlink** action for the linked Kite account
   (unsubscribe article, Verified LIVE-DOC runner 2026-07-14).
5. **Cancel / delete:** Cancel stops billing but preserves the app config
   (reactivatable); Delete (type `I UNDERSTAND`) immediately invalidates the API
   key, secret, and all access tokens. Verified (LIVE-DOC runner 2026-07-14):
   `https://support.zerodha.com/...articles/unsubscribe-unlink-trading-account-kite-api`
   + FAQ article.
6. **Credits denomination — unresolved:** the search-era "1 credit = ₹1, debited
   monthly per app" model (old forum threads 837/7757/582) appears on NONE of the
   2026-07-14 live pages — the live articles describe direct Razorpay/account-debit
   payment instead. Whether a credits ledger still exists inside the login-gated
   console is **Unknown** (U-P3-residual); the operative fact (₹500/mo per app) is
   live-verified regardless.
7. **No sandbox:** "No. Zerodha does not offer a sandbox environment for Kite
   Connect." Verified (LIVE-DOC runner 2026-07-14): FAQ article + the dedicated
   `api-sandbox` support article.

## 7. Startups / partner program (live-verified)

- **Verified (LIVE-DOC runner 2026-07-14, `https://kite.trade/startups/`):** "What about
  fees? **If your platform is targeted at the mass retail market, the Kite Connect
  APIs are available free of cost.**" (was single-extraction Assumed; now
  live-verbatim — U-P6 core claim RESOLVED). Revenue sharing: "Please write to
  **talk@rainmatter.com** to learn more" (email decoded from the page's
  Cloudflare-obfuscated `data-cfemail` — same address appears on the kite.trade
  homepage business blurb and the support charges article). Approvals: "Yes. Once
  the platform is ready, Zerodha can assist in obtaining necessary regulatory
  approvals." Getting started: Zerodha trading account + Kite Connect developer
  account + write in for free access. Detailed partnership terms (equity? plain
  B2B?) are still only available by email — **Unknown**.
- The support charges article confirms the same: "For startups working on mass
  retail products: Kite Connect APIs are free — Reach out … to learn more."
  Verified (LIVE-DOC runner 2026-07-14). For billing/account help the forum staff address
  is `kiteconnect@zerodha.com` (forum 15015, decoded cfemail).
- **Rainmatter linkage** — Verified (LIVE-DOC runner 2026-07-14, support "what is Kite
  Connect" article + FAQ): "Kite Connect and Kite Publisher form part of
  **Rainmatter's** initiative to incubate innovative Indian fintech startups."
- Stale marketing figures, quoted as-printed: startups page says "4+ million clients
  of Zerodha"; publisher page says "2+ million"; the kite.trade homepage says "Let
  **16+ million** clients of Zerodha seamlessly access your platform" — the
  startups/publisher pages are visibly older copy; use the homepage figure if any.
- Related live restriction (data licensing): "you cannot display data from Kite
  Connect APIs on other platforms, as this violates the exchange's data vending
  policies. Kite Connect API is primarily an execution suite, not a data vending
  service." Verified (LIVE-DOC runner 2026-07-14):
  `https://support.zerodha.com/...articles/can-i-use-historical-and-live-data-taken-from-kite-connect-api-on-other-platforms`.

## 8. What the SDK and mocks say about pricing: NOTHING (verified absence, narrow)

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master README.md, fetched 2026-07-13):**
the official Python SDK README contains **no pricing, cost, ₹-amount, signup-fee, or
subscription statement** (grep over the fetched 224-line file for
`pric|cost|₹|2000|free|sign ?up|developers\.kite|subscription` → zero pricing hits).
Scope of this absence claim is the README **only** — not the whole repo. Pricing is
purely a docs/console-side concern; nothing in the client protocol changes between
Personal and paid Connect except which endpoints your key is entitled to. The
entitlement-failure shape now has FORUM-tier evidence (user reports on the live-fetched
14868 thread, March 2025): quote/LTP calls on a Personal key return
**"Insufficient permission for that call"** — official confirmation + exact HTTP
status still belong to the exceptions file (U-P7).

## 9. Compare: Dhan / Groww (summary table)

| Aspect | Kite Connect (this file) | Dhan (repo ref) | Groww (repo ref) |
|---|---|---|---|
| Execution/trading APIs | Free (Personal, since 2025-03-14 — LIVE-DOC) | Free ("Trading APIs (free for all Dhan users)" — `docs/dhan-ref/01-introduction-and-rate-limits.md` lines 60–63) | No fee documented anywhere in `docs/groww-ref/` |
| Live + historical market data | ₹500/mo/API key, bundled (LIVE-DOC) | "Data APIs (additional charges)" — amount NOT stated in repo ref (same lines 60–63 span) | No fee documented; historical endpoint documented fee-free |
| Separate historical add-on | Retired 2025-02-08 (was ₹2,000/mo) — LIVE-DOC | n/a (historical inside the paid Data class) | n/a |
| Per-what billing | Per **app/API key** per month ("₹500 per app each month" — LIVE-DOC) | Per account data plan (see `dataPlan` field, `docs/dhan-ref/02-authentication.md`) | n/a |
| Publisher/button product | Yes, free (Kite Publisher) — LIVE-DOC | none in repo ref | none in repo ref |
| Startup program | kite.trade/startups: free for mass-retail platforms + Rainmatter (`talk@rainmatter.com`) — LIVE-DOC | none in repo ref | none in repo ref |

## 10. Residual verification items (operator, logged-in console only)

The day-0 unblocked-network checklist from the 2026-07-13 version is DONE (items 1–3
were executed by the 2026-07-14 LIVE-DOC runner: live price read, charges article
pasted, forum publish dates captured byte-exact). What remains needs a **logged-in**
`developers.kite.trade` session, which no fetch runner can do:

1. Screenshot the create-app form + Billing tab — resolves whether the legacy credits
   ledger still exists and the exact per-app debit line (U-P3-residual).
2. Read one paid invoice — resolves GST-inclusive vs +18% (U-P5).
3. First live session with a Personal key: capture the exact HTTP status/error body
   for a market-data call — entitlement-failure shape (U-P7).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **U-P1 — RESOLVED by live fetch (2026-07-14).** Current price live-verified:
  Personal free / Connect **₹500 per month per API key** with live + historical data
  bundled — read byte-verbatim from `https://kite.trade/`,
  `https://zerodha.com/products/api/`, and both support pricing articles (all in the
  fetch-log). The 99-UNKNOWNS rebuilder can close the matching U-row.
- **U-P2 — RESOLVED by live fetch (refuted).** No 2023 free-API event existed: the
  live z-connect update (24 Apr 2025) says ₹2000+₹2000 was still being charged into
  2025, and the 2025-05-06 forum OP says execution APIs are "free since March 2025"
  (§3). Close the U-row as refuted.
- **U-P3 (narrowed) — console form + credits residual:** Personal IS a selectable
  plan/app-type and the create-app flow is live-verified (§5/§6 — that half is
  RESOLVED by live fetch); still open: does the legacy credits ledger (1 credit = ₹1)
  still exist inside the login-gated console, and what is the exact debit line?
  Paste: the `developers.kite.trade` Billing tab (logged-in screenshot). Why: cost
  modeling nicety only — the ₹500/mo per-app quantum is already live-verified.
- **U-P4 — RESOLVED by live fetch (the answer-text half).** "Can I subscribe to
  historical API without subscribing to Kite Connect API? **No**" — live-verbatim
  (§4). The bundled-historical DEPTH window ("10 years intraday"?) was never this
  file's fact — it stays with the historical-API section file's own live sources.
  Close this file's U-row; the depth question lives under the historical file.
- **U-P5 — GST:** is ₹500 inclusive or + 18% GST? Stated on NONE of the 2026-07-14
  live pages. Paste: a real invoice line from the login-gated billing page. Why: the
  repo's cost-envelope convention (`aws-budget.md`) quotes everything GST-inclusive.
- **U-P6 — RESOLVED by live fetch (core claim).** "If your platform is targeted at
  the mass retail market, the Kite Connect APIs are available free of cost" is now
  live-verbatim from `https://kite.trade/startups/` (§7), with the contact
  `talk@rainmatter.com`. Residual (keep as note, not a blocking unknown): the
  detailed partnership/revenue-share terms are only available by email.
- **U-P7 — Entitlement failure shape (still open, evidence improved):** forum-user
  reports on the live-fetched 14868 thread show quote/LTP on a Personal key returns
  "Insufficient permission for that call" (March 2025) — but the exact HTTP status
  code + error_type is unconfirmed officially. Probe: first live session with a
  Personal key. Why: tickvault's error taxonomy needs the reject class to
  distinguish "unentitled" from "auth-dead" (the Dhan DH-902/806 precedent).

## CLAIMS (for README reconciled table)

- Current paid plan: Kite Connect = **₹500/month per API key** ("₹500 per app each
  month"), including live WebSocket market data AND historical candle data at no
  additional cost — **Verified (LIVE-DOC runner 2026-07-14)**: kite.trade root +
  zerodha.com/products/api (identical page) + support charges article +
  payment-plan article + FAQ article.
- **Kite Connect Personal = free**: orders, GTT, alerts, margin computation,
  positions/holdings/funds — NO market data (live or historical) — **Verified
  (LIVE-DOC 2026-07-14)**: charges article + payment-plan article + forum 14868 OP
  ("as long as you have a source for market data").
- Historical-data add-on (**₹2,000/mo**) **removed effective 2025-02-08** (announced
  2025-02-10T06:43:58Z); historical bundled into base Connect, auto-enabled,
  console option removed — **Verified (LIVE-DOC runner 2026-07-14)**: forum 14806 OP.
- Connect fee **revised ₹2,000 → ₹500/month on 2025-05-06** (OP timestamp
  2025-05-06T11:05:25Z), citing NSE's retail-algo circular; "order placement and
  account management APIs have been free since March 2025"; no-refund policy on the
  revision — **Verified (LIVE-DOC runner 2026-07-14)**: forum 15015 OP + Matti replies.
- Personal launch = **2025-03-14** (OP timestamp 2025-03-14T08:39:24Z; the earlier
  ARITH X/LinkedIn dates of 2025-04-17/24 were later promotion, as suspected) —
  **Verified (LIVE-DOC runner 2026-07-14)**: forum 14868 OP.
- Pre-2025 pricing was **₹2,000/mo per app + ₹2,000/mo historical add-on** (₹4,000
  all-in) — **Verified (LIVE-DOC runner 2026-07-14)**: z-connect update (24 Apr 2025) +
  forum 14806 OP.
- **Kite Connect launched 2016-04-20** ("India's first market APIs for retail
  clients", Nithin Kamath) — **Verified (LIVE-DOC runner 2026-07-14)**; **CORRECTION**: the
  2026-07-13 SEARCH version of this file said 2015 (conflation with the Kite
  platform's launch) — the live page wins.
- **NO 2023 free-API announcement existed** — now REFUTED (not just unsourced): the
  live 2025 posts state ₹2000+₹2000 was charged until Feb/May 2025 and execution
  went free only in March 2025 — **Verified (LIVE-DOC runner 2026-07-14)** (§3).
- Kite Publisher (HTML/JS trade buttons via inline Kite popup, basket ≤10, order
  execution only, no approvals needed) is **free** — **Verified (LIVE-DOC
  2026-07-14)**: kite.trade/publisher/ + docs publisher page.
- Signup = developers.kite.trade/signup → choose plan (Personal free / Connect
  ₹500) → create app → api_key + api_secret; payment via Razorpay gateway or
  Zerodha-account debit (invoices from billing page / Console); cancel preserves
  config, delete invalidates all credentials; **no sandbox environment** —
  **Verified (LIVE-DOC runner 2026-07-14)**: sign-up + invoice + unsubscribe + FAQ +
  api-sandbox support articles. The legacy credits model (1 credit = ₹1) appears on
  NO live page — Unknown whether it persists inside the login-gated console
  (U-P3-residual).
- Startups page: Kite Connect APIs **free of cost for platforms targeted at the
  mass retail market**; approvals assisted by Zerodha; revenue-share by email
  `talk@rainmatter.com`; Rainmatter incubation linkage — **Verified (LIVE-DOC
  2026-07-14)**: kite.trade/startups/ + charges article + what-is-Kite-Connect
  article.
- Kite Connect data may NOT be displayed/redistributed on other platforms (exchange
  data-vending policy; "primarily an execution suite, not a data vending service")
  — **Verified (LIVE-DOC runner 2026-07-14)**: support article.
- `https://developers.kite.trade/` is **LOGIN-GATED** — the live fetch returned the
  public login page only — **Verified (LIVE-DOC runner 2026-07-14)** (in fetch-log).
- The official SDK carries no pricing content — Verified (CLIENT-LIB-SOURCE
  pykiteconnect@master README.md, fetched 2026-07-13; absence claim scoped to that
  file only).
