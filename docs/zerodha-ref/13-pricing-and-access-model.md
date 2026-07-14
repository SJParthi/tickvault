# Zerodha Kite Connect — Pricing & Access Model (reference)

> **Source:** `https://kite.trade/` (homepage) · `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis` · `https://zerodha.com/z-connect/updates/free-personal-apis-from-kite-connect` · `https://zerodha.com/products/api/` · `https://kite.trade/forum/discussion/14806/historical-data-is-now-free-with-base-kite-connect-subscription` · `https://kite.trade/forum/discussion/14868/introducing-kite-connect-personal-apis-free-apis-for-personal-use` · `https://kite.trade/forum/discussion/15015/revising-kite-connect-fees-from-2000-to-500-per-month` · `https://kite.trade/startups/` · `https://kite.trade/publisher/` · `https://developers.kite.trade/signup`
> **Fetched-Verified:** 2026-07-13 — via **SEARCH ONLY** for every pricing fact (see provenance warning below), plus **CLIENT-LIB-SOURCE** (pykiteconnect@master `README.md` — absence check) and **ARITH** (social-post ID timestamp decodes). `kite.trade`, `zerodha.com`, `support.zerodha.com`, `tradingqna.com`, `marketcalls.in`, `web.archive.org`, and `r.jina.ai` are ALL 403-blocked at the sandbox egress proxy (direct WebFetch AND curl-via-proxy both refused) — **no ARCHIVE-DOC and no MIRROR-LIVE route exists for this file's topic**.
> **Evidence tiers:** see README legend. This file additionally uses two SEARCH sub-forms, in descending strength: **SEARCH-title** (a verbatim page TITLE returned in the search-result link list — byte-exact title string, content not read) and **SEARCH-extract** (the search backend's AI-summarized extraction of a named page — substantive, URL-cited, NOT byte-verbatim, and the summarizer can itself err). Per the pack's hard rule, NOTHING search-derived is labeled bare "Verified"; the best available label here is SEARCH with a consistency count.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. No Zerodha account exists in this project; nothing here authorizes signing up, paying, or fetching.

---

## ⚠ 0. Provenance warning — read before trusting ANY number in this file

This is the pack's **highest-hallucination-risk file**: every rupee figure below is
sourced from search-result extraction because every primary page is proxy-blocked.
Mitigations applied: (a) each load-bearing number is corroborated across **multiple
independent search extractions** (count noted per claim); (b) verbatim **page titles**
(returned as-is in result link lists) are preferred over AI summaries — two of the
three key price events are stated IN the title of an official kite.trade forum thread;
(c) social-post snowflake-ID timestamp decodes (ARITH) pin the announcement windows
independently of any summary text. Single-extraction facts are labeled **Assumed**.
Before any commercial decision, an operator MUST re-read §1 from a live browser
(the §10 checklist / OPEN QUESTIONS carry the exact URLs).

---

## 1. Current pricing (as of 2026-07-13)

| Plan / app type | Price | What it includes |
|---|---|---|
| **Kite Connect Personal** | **Free** | Order placement + positions, holdings, funds — **NO market data** (neither live nor historical) |
| **Kite Connect** (full) | **₹500 / month / API key** | Everything in Personal **plus** live market data (WebSocket streaming) **plus** historical candle data at **no additional cost** |
| **Kite Publisher** | **Free** | HTML/JS trade buttons only (order execution via Kite popup); no data, no REST API surface |

- **SEARCH-extract (consistent ×4, 2026-07-13):** "Kite Connect Personal API (Free):
  You can place orders and track your positions, holdings, and funds… Kite Connect
  (Paid) API: all Personal API features plus access to live market data and
  historical data at no additional cost. This plan costs **₹500 per month per API
  key**." — extraction attributed to
  `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis`
  and `https://zerodha.com/products/api/`; the same ₹500-with-data-included figure
  recurred in four independent 2026-07-13 search extractions.
- **SEARCH-title verbatim (X post, official @zerodhaonline):** "Kite Connect Personal
  APIs: This free API lets you access and manage your Zerodha account
  programmatically. It includes all essential features of Kite Connect except for
  market data. You can place orders, track positions, holdings, and funds as long as
  you have a market data [source]" —
  `https://x.com/zerodhaonline/status/1912823918823702875`.
- **Unknown (live-currency):** whether ₹500/Personal-free is STILL the live price on
  2026-07-13 could not be byte-verified (pages blocked). One search extraction
  asserted the support page carries 2025-09/2026-05 update dates — that assertion is
  itself summarizer output → **Assumed** freshness. Day-0 live re-read required
  (§10, U-P1). GST treatment of the ₹500 (inclusive vs + 18% GST) — **Unknown** (U-P5).

**Compare: Dhan/Groww.** Dhan splits identically — "Trading APIs (free for all Dhan
users)" vs "Data APIs (additional charges)" (`docs/dhan-ref/01-introduction-and-rate-limits.md`
lines 60–63; the rupee amount of Dhan's data plan is NOT stated anywhere in the repo
ref). Groww's Trading API documents **no API-access fee at all** anywhere in
`docs/groww-ref/` (grep 2026-07-13 — no ₹/pricing/subscription-fee statement exists in
that pack). Kite's post-2025 model (free execution / ₹500 data) is therefore the
structural middle: same free-execution split as Dhan, but with a published flat data
price; Groww publishes no price at all.

## 2. Pricing timeline (2015 → 2026)

| When | Event | Evidence |
|---|---|---|
| 2015 | Kite Connect launched alongside Kite, aimed at startups building on Zerodha's infrastructure | SEARCH-extract ×2 ("In 2015, when we launched Zerodha Kite, we also built Kite Connect…"), attributed to `https://zerodha.com/z-connect/kite/kite-connect-apis-for-programmatic-access` + `https://kite.trade/startups/` |
| pre-2025 (long-standing) | **₹2,000 / month per app** for Kite Connect + **₹2,000 / month historical-data add-on** (separate subscription) | SEARCH-extract ×4 ("charging ₹2000 for data and ₹2000 for historical data"; "2000 fixed credits get deducted per month per app and if you subscribe for historical API add-on additional 2000 credits"); corroborated by the two forum-thread titles below whose subject matter is retiring exactly these two charges |
| **2025-02-08** | Historical-data add-on charge **removed** — historical bundled into the base Connect subscription; existing apps auto-granted the permission | SEARCH-title verbatim: "Historical data is now free (with base Kite Connect subscription)" — `https://kite.trade/forum/discussion/14806/...`; effective date "February 8, 2025" appeared in 2 independent extractions (attributed to the forum post + `https://support.zerodha.com/...historical-data-and-live-market-data-payment-plan`) |
| **2025-03 (likelier) / 04** | **Kite Connect Personal launched — FREE** order placement + account APIs for personal use (no market data). Extractions consistently say execution APIs "free since March 2025" (support article + marketcalls framing of "two major announcements in February and March 2025") — **March is the likelier launch month**; an official X post PROMOTING Personal decodes to **2025-04-17** and Zerodha's LinkedIn post to **2025-04-24** (both likely later promotion, not the announcement instant) | SEARCH-title verbatim: "Introducing Kite Connect Personal APIs — Free APIs for personal use" — `https://kite.trade/forum/discussion/14868/...` + "Free personal APIs from Kite Connect – Z-Connect by Zerodha" — `https://zerodha.com/z-connect/updates/free-personal-apis-from-kite-connect`; **ARITH:** X status 1912823918823702875 → snowflake `(id>>22)+1288834974657` = 2025-04-17T11:02Z; LinkedIn activity 7321070649689485312 → top-41-bit ms = 2025-04-24T07:21Z (derivations shown; LinkedIn decode is a community convention → treat that one date as Assumed-grade) |
| **2025-05/06** | Full Connect fee **revised ₹2,000 → ₹500 / month**, with live + historical data included; extractions tie the cut to NSE's retail-algo operational circular reducing regulatory risk, and quote the rationale that ₹500 "ensures that only users with serious intent sign up… streaming data comes with significant bandwidth cost" | SEARCH-title verbatim: "Revising Kite Connect fees from ₹2000 to ₹500 per month" — `https://kite.trade/forum/discussion/15015/...`; third-party coverage title "Zerodha Slashes API Data Charges to ₹500/month After Regulatory Green Signal from NSE" (`marketcalls.in`); **ARITH:** LinkedIn activity 7340813674103607298 → 2025-06-17T18:52Z (same Assumed-grade decode — likely a later repost, not the announcement instant); one extraction said "announced May 2025", and a marketcalls extraction (adversarial re-source 2026-07-13, single source for the exact day) states the price reduction "**started May 6, 2025**" → effective ~2025-05-06 (Assumed-grade, single extraction), month window May–June 2025 |
| 2026-07-13 | Current state believed unchanged from the 2025-06 structure (§1) | SEARCH-extract consistency only; live byte-verification impossible from sandbox → freshness **Assumed** (U-P1) |

## 3. The "2023-09 free announcement" hypothesis — NOT FOUND

The task brief hypothesized a **September 2023** Zerodha announcement making Kite
Connect free for individual retail users. **Four targeted searches on 2026-07-13
found ZERO source for any 2023 free-API event** (queries combining "September 2023",
2023 news-outlet coverage, TechCrunch/Moneycontrol/Business-Standard framings, and
Nithin Kamath announcement phrasings). Every dated source places the
free-for-personal-use event in **2025** (§2 rows 3-4), and the search backend itself
twice volunteered "the specific September 2023 announcement you referenced was not
found."

**Verdict: Unknown / unsubstantiated-as-stated.** Do NOT cite a 2023 free date
anywhere in tickvault. If a genuine 2023 announcement existed (e.g. an
intent/roadmap statement that was later superseded), only an operator-pasted dated
2023 article can establish it → OPEN QUESTIONS U-P2. Until then the authoritative
free-API date is the 2025-03/04 Personal launch.

## 4. The historical-data add-on (retired 2025-02-08)

- **Old model (pre-2025-02-08):** historical candle data was a **separate ₹2,000/month
  add-on subscription per app**, on top of the ₹2,000/month Connect fee — full stack
  cost ₹4,000/month/app. SEARCH-extract ×3 + the §2 forum-title evidence.
- **New model:** no separate historical product exists — historical is a bundled
  permission of the (now ₹500/month) Connect subscription, auto-enabled on new apps.
  SEARCH-title ("Historical data is now free (with base Kite Connect subscription)") +
  SEARCH-extract ×2.
- One extraction claimed the bundled historical scope is "up to 10 years of intraday
  data for NSE/BSE" — **single extraction → Assumed**; the actual depth window
  belongs to the historical-API section file, not here (cross-ref that file; U-P4).
- A support article exists titled "Can I subscribe to historical API without
  subscribing to Kite Connect API?"
  (`https://support.zerodha.com/...can-i-subscribe-to-historical-api-without-subscribing-to-kite-connect-api`)
  — its ANSWER was not extractable; historically the add-on required a base Connect
  subscription, but post-2025 the question is moot (no separate add-on). Answer text
  → **Unknown** (U-P4).
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
| **Personal** | Free | Orders + positions/holdings/funds; **NO live or historical market data** ("as long as you have a market data [source]" — the official X wording, §1) | Individual retail users automating their own account |
| **Publisher** | Free | Embeddable HTML/JS trade buttons; clicking opens a Kite popup where the order executes; **order placement only** — no data, no account API | Websites/content platforms offering "trade this" buttons |

- Publisher evidence: SEARCH-title verbatim "HTML/Javascript trading buttons - Kite
  Connect Trading APIs" (`https://kite.trade/publisher/`) + SEARCH-extract ×2 ("Kite
  Publisher is available free of charge… only to place orders"; inline popup flow),
  incl. the official forum thread "Difference between Kite publisher and connect"
  (`https://kite.trade/forum/discussion/11108/...`).
- Whether "Personal" is a distinct app TYPE selectable at app-creation vs a billing
  tier of a Connect app — **Unknown** (U-P3); extractions describe it as a plan
  ("Choose your plan: Personal (Free) or paid Connect") but the developer-console
  create-app form could not be viewed.
- **Compare: Dhan/Groww.** Neither Dhan nor Groww has a Publisher-button analogue or
  a multi-app-type model in the repo refs; both are single-surface API products
  keyed to the user's own account. Kite's per-APP (not per-user) pricing matters for
  cost modeling: N apps = N × ₹500/mo.

## 6. Developer signup + billing flow (developers.kite.trade)

Flow as reconstructed from search evidence — **treat step details as Assumed** where
noted:

1. Sign up at `https://developers.kite.trade/signup` (page exists — SEARCH-title
   verbatim "Signup / Kite Connect developer"; login at `/login`).
2. Billing is denominated in **credits, 1 credit = ₹1**, bought via the console's
   Billing tab (SEARCH-extract ×2 + official forum-thread titles "Credits per App",
   "How are credits debited?", "How Credits are used?" — threads 837/7757/582).
3. Creating a full Connect app debits the monthly fee in credits ("you will be asked
   to verify that you will be charged … credits for creating the app"); the widely
   quoted figure "2000 credits per app per month (+2000 for the historical add-on)"
   is the PRE-2025 price — post-revision it should be 500 credits/month, but **no
   fetched source states '500 credits'** → the credits-amount today is **Assumed
   (derived from §1 ₹500 + the 1-credit=₹1 rule), flagged U-P3**.
4. One third-party extraction claims the billing page can link your Zerodha trading
   account for auto-deduction — single third-party source → **Assumed**.
5. The app gives an `api_key` + `api_secret`; the runtime login/token flow is the
   authentication section file's topic, not this file's.

## 7. Startups / partner program

- A dedicated startups page exists: SEARCH-title verbatim "Investment and trading
  APIs for startups - Kite Connect trading APIs" (`https://kite.trade/startups/`);
  the kite.trade homepage title itself is "Simple HTTP trading APIs for individual
  traders **and startups**".
- **SEARCH-extract (single):** "If a platform is targeted at the mass retail market,
  the Kite Connect APIs are available **free of cost**" (attributed to
  `kite.trade/startups/`) — a commercially load-bearing claim on ONE extraction →
  **Assumed**; exact eligibility terms **Unknown** (U-P6).
- **SEARCH-extract (single):** "Kite Connect and Kite Publisher form part of
  **Rainmatter**'s initiative to incubate innovative Indian fintech startups …
  end-to-end broking as a service" (attributed to the support "what is Kite Connect"
  article); Rainmatter is Zerodha's fintech fund (`https://rainmatter.com/about/`).
  Partnership mechanics (equity? revenue share? plain B2B?) — **Unknown** (U-P6).
- One extraction cites platform reach as "4+ million clients of Zerodha who can sign
  in and execute" — Zerodha's actual 2026 client count is certainly different from
  this page's stale marketing figure; number **Assumed-stale**, do not reuse.

## 8. What the SDK and mocks say about pricing: NOTHING (verified absence, narrow)

**Verified (CLIENT-LIB-SOURCE pykiteconnect@master README.md, fetched 2026-07-13):**
the official Python SDK README contains **no pricing, cost, ₹-amount, signup-fee, or
subscription statement** (grep over the fetched 224-line file for
`pric|cost|₹|2000|free|sign ?up|developers\.kite|subscription` → zero pricing hits).
Scope of this absence claim is the README **only** — not the whole repo. Pricing is
purely a docs/console-side concern; nothing in the client protocol changes between
Personal and paid Connect except which endpoints your key is entitled to (entitlement
behavior on an unentitled call — e.g. the exact HTTP error for market-data calls on a
Personal key — **Unknown**, belongs to the exceptions section file, U-P7).

## 9. Compare: Dhan / Groww (summary table)

| Aspect | Kite Connect (this file) | Dhan (repo ref) | Groww (repo ref) |
|---|---|---|---|
| Execution/trading APIs | Free (Personal, since 2025-03/04) | Free ("Trading APIs (free for all Dhan users)" — `docs/dhan-ref/01-introduction-and-rate-limits.md` lines 60–63) | No fee documented anywhere in `docs/groww-ref/` |
| Live + historical market data | ₹500/mo/API key, bundled | "Data APIs (additional charges)" — amount NOT stated in repo ref (same lines 60–63 span) | No fee documented; historical endpoint documented fee-free |
| Separate historical add-on | Retired 2025-02-08 (was ₹2,000/mo) | n/a (historical inside the paid Data class) | n/a |
| Per-what billing | Per **app/API key** per month | Per account data plan (see `dataPlan` field, `docs/dhan-ref/02-authentication.md`) | n/a |
| Publisher/button product | Yes, free (Kite Publisher) | none in repo ref | none in repo ref |
| Startup program | kite.trade/startups + Rainmatter (terms Unknown) | none in repo ref | none in repo ref |

## 10. Day-0 live re-verification checklist (operator, unblocked network)

1. Open `https://kite.trade/` and `https://developers.kite.trade/` — read the live
   price on the signup/create-app flow; confirm ₹500/mo and the Personal-free tier
   still stand (U-P1).
2. Open the support charges article (§ Source list) — paste the full current plan
   table into this file, replacing every SEARCH label it covers.
3. Open forum threads 14806 / 14868 / 15015 — capture the exact publish dates (the
   §2 date windows become byte-verified).
4. Screenshot the create-app form — resolves the Personal-as-app-type and
   credits-per-app questions (U-P3).

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **U-P1 — Is ₹500/mo (+ free Personal) the live price on the current date?** Paste: `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/what-are-the-charges-for-kite-apis` and `https://kite.trade/` (pricing block). Why: every current-price figure in this file is search-derived; prices demonstrably moved twice in 2025 alone — cost modeling for any Kite feed needs the day-0 number.
- **U-P2 — Did ANY 2023 free-API announcement exist?** Paste: a dated 2023 article or `https://zerodha.com/z-connect/` archive listing for 2023. Why: the task hypothesis says 2023-09; zero sources found; if a 2023 statement existed and was superseded, the timeline in §2 needs a row.
- **U-P3 — App-creation mechanics today:** is "Personal" a distinct app type on the create-app form, and is the monthly debit now 500 credits? Paste: the `https://developers.kite.trade/apps/new` form (screenshot text) + the Billing tab. Why: per-app billing quantum + app-type choice drive how many keys a two-lane (data + orders) design needs.
- **U-P4 — Bundled historical scope + the old add-on FAQ answer:** paste `https://support.zerodha.com/...can-i-subscribe-to-historical-api-without-subscribing-to-kite-connect-api` and the historical-data section of the charges page. Why: "10 years intraday" is single-extraction Assumed; the historical section file needs the authoritative depth window.
- **U-P5 — GST:** is ₹500 inclusive or + 18% GST? Paste: the billing page's invoice line or charges article footnote. Why: the repo's cost-envelope convention (`aws-budget.md`) quotes everything GST-inclusive.
- **U-P6 — Startup/free-for-mass-retail terms:** paste `https://kite.trade/startups/` in full. Why: "free of cost for mass-retail platforms" is a single-extraction claim that would change the commercial calculus entirely if real and applicable.
- **U-P7 — Entitlement failure shape:** what exact HTTP status/error does a Personal (data-less) key get on `GET /quote` or WS connect? Probe: first live session with a Personal key. Why: tickvault's error taxonomy needs the reject class to distinguish "unentitled" from "auth-dead" (the Dhan DH-902/806 precedent).

## CLAIMS (for README reconciled table)

- Current paid plan: Kite Connect = **₹500/month per API key**, including live WebSocket market data AND historical data at no additional cost — SEARCH-extract (consistent ×4, 2026-07-13; support charges article + zerodha.com/products/api) — freshness Assumed pending day-0 live read (U-P1).
- **Kite Connect Personal = free**: orders + positions/holdings/funds, NO market data (live or historical) — SEARCH-title verbatim (official X post `x.com/zerodhaonline/status/1912823918823702875`) + SEARCH-extract ×4.
- Historical-data add-on (**₹2,000/mo**) **removed effective 2025-02-08**; historical bundled into base Connect — SEARCH-title verbatim (`kite.trade/forum/discussion/14806`) + SEARCH-extract ×2 (date ×2).
- Connect fee **revised ₹2,000 → ₹500/month**, effective ~2025-05-06 (single marketcalls extraction, Assumed-grade; window May–June 2025), in the wake of NSE's retail-algo circular — SEARCH-title verbatim (`kite.trade/forum/discussion/15015` — the fact is IN the title) + ARITH (LinkedIn decode 2025-06-17, likely a later repost).
- Personal launch = **2025-03 (likelier) / 04** (extractions consistently say "free since March 2025"; an X post promoting it decodes to 2025-04-17 via snowflake ARITH; LinkedIn 2025-04-24 — both likely later promotion) — SEARCH-title (`kite.trade/forum/discussion/14868` + z-connect post URL) + ARITH.
- Pre-2025 pricing was **₹2,000/mo per app + ₹2,000/mo historical add-on** (₹4,000 all-in) — SEARCH-extract ×4, corroborated by the two forum titles that retire exactly these charges.
- **NO 2023 free-API announcement could be sourced** (4 targeted searches, 2026-07-13) — the "2023-09 free" hypothesis is unsubstantiated; authoritative free date is the 2025 Personal launch — SEARCH (absence) / Unknown (U-P2).
- Kite Publisher (HTML/JS trade buttons, order-execution only) is **free** — SEARCH-title (`kite.trade/publisher/`) + SEARCH-extract ×2.
- Developer billing is credit-denominated, **1 credit = ₹1**, debited monthly per app via `developers.kite.trade` Billing — SEARCH-extract ×2 + official forum-thread titles (837/7757/582); today's per-app credit quantum (500?) Assumed (U-P3).
- A startups page (`kite.trade/startups/`) + Rainmatter linkage exists; "free of cost for mass-retail-targeted platforms" is a SINGLE-extraction claim — Assumed / Unknown terms (U-P6).
- The official SDK carries no pricing content — Verified (CLIENT-LIB-SOURCE pykiteconnect@master README.md, fetched 2026-07-13; absence claim scoped to that file only).
