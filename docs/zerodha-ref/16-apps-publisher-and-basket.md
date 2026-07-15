# Zerodha Kite Connect v3 — Mobile/Desktop apps, Offsite basket execution, Publisher JS plugin (+ app-creation lifecycle)

> **Source:** `https://kite.trade/docs/connect/v3/apps/` ("Mobile and Desktop apps") · `https://kite.trade/docs/connect/v3/basket/` ("Offsite order execution") · `https://kite.trade/docs/connect/v3/publisher/` ("Publisher JS Plugin") · `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/how-do-i-sign-up-for-kite-connect` · `https://support.zerodha.com/category/trading-and-markets/general-kite/kite-api/articles/kite-connect-api-faqs` — all fetched live.
> **Fetched-Verified:** 2026-07-14 — **live-verbatim via GH-runner 2026-07-14 (UTC timestamps in 00-COVERAGE-MANIFEST.md)**; every cited URL is in the runner fetch-log (per-URL sha256 + timestamp in the manifest). These three docs-nav sections were MISSING from the 2026-07-13 pack (the U-1 "16th undiscovered section" risk realized — the live nav revealed 5 uncovered sections; this file covers 3 of them, `17-sdks-and-changelog.md` the other 2).
> **Evidence tiers:** see README legend + the new top tier **Verified (LIVE-DOC runner 2026-07-14)** — claim confirmed against the runner-fetched live page. CLIENT-LIB-SOURCE / OFFICIAL-MOCK stay as cross-check evidence.
> **Scope note:** REFERENCE ONLY — docs pack, not a feed-scope grant. No credentials, no code. The Publisher/basket surface is an ORDER-EXECUTION surface; nothing here authorizes order code (CLAUDE.md read-only-reference pattern).
> **Related:** `01-overview-auth-and-token-lifecycle.md` (the login flow these pages extend), `03-orders.md` (the order-param family the basket reuses), `05-portfolio.md` §6 (eDIS — the `authHoldings()` JS method's REST half), `13-pricing-and-access-model.md` §5 (Connect vs Personal vs Publisher app types + pricing — the canonical app-types table; this file carries the wire/flow detail), `17-sdks-and-changelog.md`, `99-UNKNOWNS.md`.

---

## 1. The headline facts

1. **The docs `apps/` section is NOT an "app types" page** — its live title is **"Mobile and Desktop apps"** and it documents exactly one thing: how a desktop/mobile app without a server backend completes the login flow via an embedded **webview** — **Verified (LIVE-DOC runner 2026-07-14, `…/v3/apps/`)**. App TYPES (Connect vs Personal vs Publisher) live on the pricing/support surfaces — canonical table in `13 §5`; the app-creation lifecycle is §3 below.
2. **Offsite order execution (`basket/`)** = a payment-gateway-style redirect: POST a JSON basket (form field `data` + `api_key`) to `https://kite.zerodha.com/connect/basket`; the user confirms orders on Kite's **exchange-approved** order page and is redirected back with `status` + `request_token` — no prior login API call needed — **Verified (LIVE-DOC runner 2026-07-14, `…/v3/basket/`)**.
3. **Publisher JS plugin (`publisher/`)** = one-click trade buttons on any webpage via `https://kite.trade/publisher.js?v=3`: branded `<kite-button>` tags, `data-*` attributes on arbitrary elements, or a dynamic JS basket API (`KiteConnect.ready()` / `add()` / `renderButton()` / `link()` / `finished()`), basket **maximum 10** entries — **Verified (LIVE-DOC runner 2026-07-14, `…/v3/publisher/`)**.
4. **The plugin is dead in iOS WebViews** — official warning: Safari's cookie policy blocks cross-domain cookies in iframes, so the JS plugin "will not function in the iOS WebView"; Zerodha now "recommend[s] using offsite order execution for **all** Kite Publisher use cases" — **Verified (LIVE-DOC runner 2026-07-14, the `publisher/` page's Warning box)**.
5. **API key lifecycle:** developer account at `developers.kite.trade/signup` → choose plan (Personal free / Connect ₹500/mo per key) → My Apps → Create New App (app name, Zerodha Client ID, redirect URL, optional postback URL, description) → receive **API Key + API Secret**. No sandbox environment exists — **Verified (LIVE-DOC runner 2026-07-14, both support articles)**.

**Compare: Dhan/Groww** — neither has ANY analog to this whole file: no webview-login flow (both mint tokens via API/TOTP — `docs/dhan-ref/02`, `groww-shared-token-minter-2026-07-02.md`), no offsite basket redirect, no publisher trade buttons. This surface exists because Kite's individual-user auth is interactive-login-only (`01 §7`).

## 2. Mobile and Desktop apps (`apps/`) — the webview login pattern

**Verified (LIVE-DOC runner 2026-07-14, `https://kite.trade/docs/connect/v3/apps/`)** — the page's entire content:

- The login flow (`01 §5`) ends with a redirect to the registered `redirect_url` carrying `request_token`. With no server backend, the app must open a **webview (browser view)** pointed at `https://kite.zerodha.com/connect/login?api_key=xxx` and let the whole login happen inside it.
- The app monitors the webview's **location (URL)** via a change event or a poll timer; when it changes to `https://yoursite.com/kite-redirect?request_token=yyy&status=zzz`, extract `request_token` and `status`, close the webview. (Page wart: the sentence renders as "extract the request_token and status_from the URL" — the italicized `status` merged with "from".)
- **Cookie note (official "Note" box):** "Don't forget to enable cookie (and 3rd party cookie) support in your webview or the login may not work."
- The `redirect_url` "can be a blank page even"; **"For personal desktop apps, you can run a local web server and use `127.0.0.1` as the redirect_url's host"** — the documented local-loopback pattern.
- **Secret-handling rule (verbatim intent):** "If you intent on distributing your mobile or desktop application to the public, **do not embed the api_secret** in the application. Use a server backend to do the token exchange on behalf of your application." (Note the page's own "intent" typo.) — the api_secret is server-side-only for distributed apps.

## 3. App types + API key lifecycle (support surface; canonical pricing table in `13 §5`)

**Verified (LIVE-DOC runner 2026-07-14, the sign-up article + the Kite Connect FAQ article):**

| Step | Detail |
|---|---|
| 1. Developer account | `developers.kite.trade/signup` — email (preferably the Zerodha-account one), name, password, phone, state of residence, T&C |
| 2. Choose plan | **Kite Connect Personal API (Free)** — "place orders and track your positions, holdings, and funds… does not include live market data and historical data"; **Kite Connect (Paid)** — all Personal features "plus access to live market data and historical data at no additional cost… ₹500 per month per API key" |
| 3. Create app | My Apps → Create New App: **App Name**, **Zerodha Client ID** (the app is bound to a trading account), **Redirect URL** ("you can use localhost during testing, such as `http://127.0.0.1:8000`"), **Postback URL** ("optional — postbacks trigger when you modify open orders or receive partial fills"), **Description** |
| 4. Credentials issued | **API Key** ("your unique identifier for API access") + **API Secret** ("your private authentication key (keep this secure)") |

- FAQ extras, same tier: Personal includes "Full order, GTT… and **alerts** management" + "Margin computation and portfolio management"; startups building mass-retail products get the APIs free (write to Zerodha — `13 §6`); **"No. Zerodha does not offer a sandbox environment for Kite Connect"**; no phone/ticket API support — the community forum is the support channel; "Kite Connect and Kite Publisher form part of Rainmatter's initiative".
- **Publisher app creation** happens from the same developer console (`13 §5` — "Create +" a Publisher app to obtain the `data-kite` api_key) — **Verified (LIVE-DOC runner 2026-07-14, kite.trade/publisher/ product page, per 13)**.
- What the live pages still do NOT show: the logged-in developer-console screens themselves (`developers.kite.trade` fetched but **LOGIN-GATED** — manifest §b), so per-app billing/credits quanta and the key-rotation/regeneration UI remain **Unknown** (U-42 half-open; the app-creation FORM half is now closed by the article above).

## 4. Offsite order execution (`basket/`)

**Verified (LIVE-DOC runner 2026-07-14, `https://kite.trade/docs/connect/v3/basket/`)** throughout this section.

### 4a. Purpose + mechanics

- "Redirect your users to Kite's **exchange approved** order page where they place orders and come back to your application seamlessly, like a payment gateway… you do not have to build, maintain, and get exchange approvals for order execution screens. The Kite Publisher program utilizes offsite order execution to provide embeddable Javascript+HTML trade buttons that do not require any API integrations."
- Wire form: an HTML form `POST https://kite.zerodha.com/connect/basket` with hidden fields `api_key` and `data` = the JSON **array** of order objects. "This is a browser / mobile (webview) request and has to happen at the user's end, although the basket preparation can happen in the backend" — the documented pattern is a hidden form auto-submitted by JS.
- Multiple orders → "a shopping basket like interface" for user confirmation.
- **No login prerequisite (official Note):** "You do not have to initiate a login using the login API to do offsite order execution. If a user is not already logged in, they'll be asked to login, otherwise, they'll be taken to the order basket directly. Either way, in the end, you will receive `status` and `request_token` at your `redirect_url` like in the login flow."
- **Session bridging:** after order placement the basket redirects back to the registered redirect URL with a token you may exchange for a Kite Connect session "or disregard it altogether". **Page wart, verbatim:** this paragraph says "redirect back to your registered **redirect_login** along with a **request_key**, exactly like login" — `redirect_login`/`request_key` contradict the page's own Note (`redirect_url`/`request_token`) and the login docs; read them as typos for `redirect_url`/`request_token` (**recorded, not silently normalized**).

### 4b. Basket entry parameters (the live table, verbatim semantics)

| parameter | live description |
|---|---|
| `variety` | "Order variety (regular, amo, co. Defaults to regular)" — note: NO iceberg/auction on the offsite surface (vs the REST varieties in `03 §2`) |
| `tradingsymbol` / `exchange` | instrument identity (exchange:tradingsymbol convention, `09`) |
| `transaction_type` | BUY or SELL |
| `order_type` | "MARKET, LIMIT etc." |
| `quantity` | quantity to transact |
| `product` | "Margin product to use for the order (margins are blocked based on this)" |
| `price` / `trigger_price` | for LIMIT / "for SL, SL-M etc." |
| `disclosed_quantity` | equity only |
| `validity` | order validity |
| `readonly` | "Default is false. If set to true, the UI does not allow the user to edit values such as quantity, price etc., and they can only review and execute." |
| `tag` | "An optional tag to apply to an order to identify it (alphanumeric, **max 20 chars**)" — ⚠ the publisher page's otherwise-identical table says **max 8 chars** for the same field; live-site self-conflict, recorded (Z-PB-2). `03`'s REST tag cap is 20. |

- The page's own example basket shows a regular NSE MARKET buy, an NFO LIMIT sell (`NIFTY15DECFUT` — stale 2015 example symbol), and a `co` variety entry with `product: MIS` + `trigger_price` + `readonly: true`.

## 5. Publisher JS plugin (`publisher/`)

**Verified (LIVE-DOC runner 2026-07-14, `https://kite.trade/docs/connect/v3/publisher/`)** throughout this section.

- **iOS WebView warning (official, top of page):** "The publisher JS plugin will not function in the iOS WebView due to Safari's new cookie policy, which blocks cross-domain cookie management in iframes. You will need to implement offsite order execution. We recommend using offsite order execution for all Kite Publisher use cases."
- Include ONCE, before `</body>`: `<script src="https://kite.trade/publisher.js?v=3"></script>`. "It works like a basket combined with a payment gateway, where an inline popup opens on your webpage, guides the user through a trade, and lands the user back on your page." The `request_token` from this flow can seed a Kite Connect session (§4a).
- **Basket cap: "one or more stocks to the basket (maximum 10)"** via the JS plugin, "or embed simple static buttons using plain HTML".
- Three integration styles: (1) branded `<kite-button>` HTML5 tag; (2) `data-*` attributes on ANY element (`data-kite="api_key"`, `data-exchange`, `data-tradingsymbol`, `data-transaction_type`, `data-quantity`, `data-order_type`, plus `data-price`/`data-variety`/`data-product`/`data-trigger_price` — the CO example uses all); (3) the dynamic JS API below.
- Parameter table = the same family as §4b (same `readonly` semantics; `tag` "max **8** chars" here — the §4b conflict).
- Assets load async — custom code MUST run inside `KiteConnect.ready()` and be included after `publisher.js`.

### 5a. JS API methods (the live methods table)

| method | args | live semantics |
|---|---|---|
| `KiteConnect.ready()` | `function()` | "Safe wrapper for all API calls that waits asynchronously for all assets to load" |
| `new KiteConnect("api_key")` | — | instance; "You can initialize multiple instances if you need" |
| `add()` | entry | add one order object literal (§4b params) to the basket |
| `get()` / `count()` | — | array of added entries / entry count |
| `setOption()` | key, value | "Sets the value for certain supported keys"; documented key: **`redirect_url`** — "can be a single '#', a 127.0.0.1 url for testing, or a fully qualified URL belonging to the **same domain** as the registered URL" |
| `renderButton()` | element_selector | render a branded Kite button in the target; click → "the transaction in an overlay popup" |
| `link()` | element_selector | link the basket to any existing element |
| `html()` | — | "Returns a serialized HTML form with the necessary hidden fields and the basket payload" (i.e. the §4a offsite form, generated) |
| `finished()` | `function(status, request_token)` | callback "triggered after order placement is finished" |
| `authHoldings()` | `request_id, callback(data)` | "Request ID from the **holdings authorisation** call along with an optional callback… when the holdings authorisation flow finishes" — the browser half of the eDIS `POST /portfolio/holdings/authorise` flow (`05 §6`); upgrades that flow's web-view leg to LIVE-DOC |

## 6. Cross-file confirmations from these pages

- The login URL `https://kite.zerodha.com/connect/login?api_key=xxx` and the `request_token`+`status` redirect contract restated verbatim on `apps/` — corroborates `01 §5` **[Z#1]** at LIVE-DOC tier.
- `readonly` + the offsite param family are Publisher/basket-only; nothing here adds REST order params (`03` unchanged).
- The basket/publisher flows are the ONLY documented order path that works **without** an existing API session — relevant to the `13 §5` Publisher (free) app type: order execution with zero REST/data entitlement.

---

## OPEN QUESTIONS (for 99-UNKNOWNS)

- **Z-PB-1 — developer console internals (U-42 residue).** The app-creation FORM is now LIVE-DOC (§3 — RESOLVED-in-part by live fetch: the sign-up article closes the "create-app form text" half of U-42); the LOGGED-IN console (billing/credits tab, API-secret regeneration/rotation UI, per-app plan switching) remains unfetchable (`developers.kite.trade` is login-gated — manifest §b). Paste after login. Matters: key-rotation ops planning.
- **Z-PB-2 — `tag` length: 20 vs 8.** The basket page says max 20 chars, the publisher page says max 8 for the same parameter (both LIVE-DOC 2026-07-14). Which does the server enforce on the offsite surface? Probe with a 9-char tag via a Publisher button. Matters: only if tags are used for order attribution through postbacks (`11`).
- **Z-PB-3 — `request_key`/`redirect_login` wording.** Confirm the §4a warts are typos (expected) — check whether the post-basket redirect query param is literally `request_token`. One live basket round-trip settles it. Matters: session-bridging code.
- **Z-PB-4 — offsite variety set.** The basket/publisher tables list only `regular, amo, co`. Is `iceberg` (REST-supported, `03 §2`) truly unavailable offsite, or just undocumented? Matters: none for this repo (recording only).

## CLAIMS (for README reconciled table)

- The docs `apps/` section = the mobile/desktop **webview login** pattern: open `kite.zerodha.com/connect/login` in a webview, poll the URL for `request_token`+`status`, 3rd-party cookies required, `127.0.0.1` redirect_url sanctioned for personal desktop apps, api_secret must NEVER be embedded in distributed apps (server-side token exchange) — **Verified (LIVE-DOC runner 2026-07-14, `…/v3/apps/`)**.
- Offsite order execution: form-POST `api_key` + JSON basket (form field `data`) to `https://kite.zerodha.com/connect/basket`; exchange-approved order UI; no prior login needed; returns `status` + `request_token` at the redirect_url (usable to mint a session, or ignorable); entry params = regular/amo/co varieties + the standard order fields + `readonly` + `tag` — **Verified (LIVE-DOC runner 2026-07-14, `…/v3/basket/`)**; page carries `redirect_login`/`request_key` typos + a 20-vs-8 `tag`-cap conflict with the publisher page (recorded, Z-PB-2/3).
- Publisher JS plugin: `https://kite.trade/publisher.js?v=3`, `<kite-button>` / `data-*` / dynamic-JS styles, basket **max 10**, `KiteConnect.ready()` mandatory, `setOption("redirect_url", …)` override (same-domain / `#` / 127.0.0.1), `finished(status, request_token)` callback, `authHoldings()` = the eDIS browser leg — **Verified (LIVE-DOC runner 2026-07-14, `…/v3/publisher/`)**.
- The Publisher JS plugin does NOT work in iOS WebViews (Safari iframe-cookie policy); Zerodha officially recommends offsite order execution for all Publisher use cases — **Verified (LIVE-DOC runner 2026-07-14)**.
- API key lifecycle: developers.kite.trade signup → plan (Personal free / Connect ₹500/mo/key) → Create New App (name, Zerodha Client ID, redirect URL, optional postback URL) → API Key + API Secret issued; **no sandbox environment**; no phone/ticket API support (forum only) — **Verified (LIVE-DOC runner 2026-07-14, support sign-up + FAQ articles)**; logged-in console internals still Unknown (Z-PB-1).
