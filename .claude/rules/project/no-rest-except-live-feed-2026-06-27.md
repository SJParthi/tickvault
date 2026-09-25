# No REST Except Live Feed — Market-Data REST Lock (Operator Lock 2026-06-27)

> **⚠ LOCK INVERTED 2026-07-15 — NO live market-data feed exists (operator Q1, received directly in this
> session: *"remove the whole Groww live feed; keep only spot 1m and option chain for both brokers; go."*;
> approval Q2, typos preserved: *"go aehad approv ed dude"*):** with the Groww live WS retired (after the
> 2026-07-13 Dhan retirement), the §8 (Dhan) + §9 (Groww) scheduled-pull KEEP classes ARE the entire
> market-data surface — §1's "only the live-feed WebSocket carries market data" no longer names a running
> feed. The Groww master-CSV + token-read KEEP rows STAND (the watch set supplies the REST legs' identity;
> the SEBI master continuity keeps writing). The live-feed-AUTH KEEP class now serves only the REST stacks +
> the order-side channels (order/position live push is a separate, permanently-kept surface per the
> operator's 2026-07-15 order-side directive — recorded by the order-side session; see the cluster-A rule
> updates — market data = per-minute REST pull, order/position events = live push). GDF
> (`gdf-third-feed-scope-2026-07-13.md`) remains the only path to a future live market-data feed.

> **⚠ 2026-07-12 NOTE — BruteX cross-verify S3 read is a KEEP class:** per the operator's 2026-07-12 directive recorded verbatim in `groww-second-feed-scope-2026-06-19.md` §37, TickVault reads BruteX-produced backtest CSVs from OUR OWN S3 bucket (`s3://tv-prod-cold/crossverify/*`) via `aws-sdk-s3` GetObject/ListObjectsV2. That is an INTERNAL artifact transfer from our own infrastructure — the same class as the S3 cold-archive surface — NOT a market-data REST endpoint of Dhan or Groww. A new **KEEP** row is added to the §3 inventory below. This lock's ban on Dhan/Groww market-data REST pulls is UNCHANGED.
>
> **⚠ 2026-07-12 NOTE (SECOND same-day directive) — per-minute REST pipeline is a scheduled-pull KEEP class:** a SECOND 2026-07-12 operator directive (relayed verbatim via the coordinator session, quote preserved in §8.0 below) adds a narrow **scheduled-pull KEEP class**: the per-minute **spot-1m intraday fetch** (`POST /v2/charts/intraday`, interval `"1"`, exactly 3 IDX_I SIDs — NIFTY=13, BANKNIFTY=25, SENSEX=51) + the per-minute **option-chain fetch** (`POST /v2/optionchain` + `POST /v2/optionchain/expirylist`, the same 3 underlyings' current expiry, config-gated DEFAULT-OFF pending the first-live-boot entitlement probe) — **see the new §8**. Two new KEEP rows join the §3 inventory; the matching legacy REMOVE rows are annotated, never deleted. The ban on all OTHER market-data REST pulls is UNCHANGED.
>
> **⚠ 2026-07-13 NOTE — GROWW per-minute REST pipeline is a scheduled-pull KEEP class:** a 2026-07-13 operator directive (relayed verbatim via the coordinator session, quotes preserved in §9.0 below + `groww-second-feed-scope-2026-06-19.md` §38) extends the §8 scheduled-pull KEEP class to GROWW: the per-minute **spot-1m fetch** (`GET api.groww.in/v1/historical/candles`, `candle_interval="1minute"`, 3 Groww spot indices) + the per-minute **option-chain fetch** (`GET api.groww.in/v1/option-chain/...`, the same 3 underlyings' current expiry) + a bounded **per-contract 1m fetch** (same candles endpoint, `segment=FNO`, selected option contracts) — **see the new §9**. Two new KEEP rows join the §3 inventory. The ban on all OTHER market-data REST pulls (incl. any BULK Groww historical sweep) is UNCHANGED.
>
> **⚠ 2026-07-13 NOTE (THIRD same-period directive) — Dhan live WS retired; KEEP-row repurposing:** per the operator's 2026-07-13 retirement directive (verbatim in `websocket-connection-scope-lock.md`'s "2026-07-13 Amendment"), the Dhan LIVE-FEED half of §1's one-liner no longer exists — for Dhan, market data is now EXCLUSIVELY the §8 scheduled-pull KEEP class (spot-1m + option-chain + historical); the live-feed WS in §1 now means the GROWW feed (and, when its trial goes live, GDF per `gdf-third-feed-scope-2026-07-13.md`). Three §3 row effects: **(1) the Dhan "live-feed AUTH" KEEP rows (generateAccessToken / RenewToken / getIP-setIP) are KEPT with a repurposed rationale** — they now produce the token/gate for the Dhan REST stack (spot-1m/chain/historical) + the functional-dormant order-update WS inside `dhan_rest_stack`, no longer for a market-data WS; **(2) the Dhan Detailed CSV instrument-master KEEP row is RETIRED** (operator Q3: *"hereafter no Dhan instrument download/parsing — just direct hardcoded security IDs passed to spot 1m and option chain"*; the Phase B dependency map proved ZERO surviving consumers — the Groww watch build reads its OWN master CSV, never the Dhan one); **(3) the niftyindices NTM row and the Groww master CSV row are UNCHANGED** (their surviving consumer is the Groww watch build via its own hardened client). Row annotations below are edited in place per house style (rows annotated, never deleted).
>
> **⚠ 2026-07-14 NOTE — Groww ORDER-SIDE is a gated KEEP class (build authorized, live fire LOCKED):** a 2026-07-14 operator directive (verbatim in §10.0 below + `groww-second-feed-scope-2026-06-19.md` §39) authorizes BUILDING + INTEGRATING the Groww order-side (orders / smart orders / portfolio / margin / user, per `docs/groww-ref/16-orders-margins-portfolio.md`) **entirely behind a 4-gate live-fire lattice** — see the new **§10**. Order MUTATIONS stay hard-locked until a separate future dated live-orders enable; order-side READ-ONLY GETs are per-area config-gated DEFAULT-OFF + market-hours-only. Quote/LTP REST and bulk historical fetch stay BANNED (§1/§2/§33). Two-plus new KEEP-GATED rows join the §3 inventory; the ban on all OTHER market-data REST pulls is UNCHANGED.
>
> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > `daily-universe-scope-expansion-2026-05-27.md` §3 > `groww-second-feed-scope-2026-06-19.md` > this file > defaults.
> **Scope:** PERMANENT once confirmed. Every Phase. Every PR. Every future Claude/Cowork session. Applies to BOTH Dhan (feed #1) and Groww (feed #2).
> **Operator-locked:** 2026-06-27 (verbatim quote below).
> **Status:** **PENDING OPERATOR CONFIRMATION** of the market-data-only scope (§2). The companion plan `.claude/plans/active-plan-dual-feed-monday-open.md` (P0b) tracks this confirmation. Until confirmed, this file is the authoritative interpretation; no code PR may strip a REST call before the §2 scope is acknowledged.
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demand (preserve exactly, do not paraphrase)

**Quote (2026-06-27):**
> "As of now except live feed any rest urls should never ever be used anywhere in dhan or groww dude okay?"

---

## §1. The rule (one line)

**Only the live-feed WebSocket carries market data. ALL REST market-data pulls are FORBIDDEN for both Dhan and Groww — no REST prices, OHLCV, quotes, option-chain, or profile polling. Market data comes from the live WS stream and nowhere else.**

---

## §2. Honest hard-constraint — the allowed exceptions (NO HALLUCINATION)

A **literal** reading ("kill ALL REST") is self-contradictory: it would also kill the live-feed AUTH and the static instrument-master CSVs — and **without those the live feed cannot connect or map a single tick**, so the literal lock destroys the very feed it wants to keep. Therefore the only coherent interpretation is **market-data-only**, and TWO REST classes are EXPLICITLY KEPT as the structural prerequisites of the live feed:

| Kept REST class | Why it is NOT "market data" and MUST stay |
|---|---|
| **Live-feed AUTH** — Dhan `generateAccessToken` / `RenewToken` / static-IP gate. *(Groww's former `/v1/token` mint call was DELETED 2026-07-02 — the Groww token now arrives via a READ-ONLY SSM read of the bruteX-Lambda-minted `/tickvault/<env>/groww/access-token`; see `groww-shared-token-minter-2026-07-02.md`.)* | Produces the token the live WS needs to connect. No auth ⇒ no live feed at all. |
| **Static instrument-master CSVs** — Dhan Detailed CSV, niftyindices NTM list, Groww master CSV | Public, daily, STATIC reference files (not prices). They build the `security_id`↔instrument map + the Groww ISIN→token map. No master ⇒ arriving ticks cannot be mapped/subscribed. This matches `daily-universe-scope-expansion-2026-05-27.md` §3 which already REQUIRES the static Detailed CSV while BANNING per-segment REST + `/marketfeed/ltp` price pulls. |

**This lock is consistent with the prior daily-universe lock** — it only adds the removal of the remaining market-data REST endpoints (§3 REMOVE rows). It is scoped, pending operator confirmation, to **market-data pulls only**.

---

## §3. KEEP / REMOVE REST inventory

| Endpoint | File:line | Class | Verdict |
|---|---|---|---|
| `POST auth.dhan.co/.../generateAccessToken` | `constants.rs:1246` | Dhan AUTH — since 2026-07-13 serves the REST stack (§8 spot-1m/chain + historical) + the dormant order-update WS, not a market-data WS | **KEEP** (rationale repurposed 2026-07-13) |
| `GET /v2/RenewToken` | `auth/types.rs:353`, `constants.rs:1250` | Dhan AUTH — REST stack + dormant order-update WS (2026-07-13) | **KEEP** (rationale repurposed 2026-07-13) |
| `GET/PUT /v2/ip/getIP` `setIP` | `constants.rs:1017,1270,1279`; `main.rs:4417…` | Dhan STATIC-IP gate — order-API prerequisite (April 2026 mandate), kept for the REST stack + future order path | **KEEP** (rationale repurposed 2026-07-13) |
| `GET images.dhan.co/api-data/api-scrip-master-detailed.csv` | ~~`csv_downloader.rs:129,209`~~ | instrument-master (static ref) — WAS the Dhan-WS map source | **RETIRED 2026-07-13** (Dhan live WS retired — Q3: no Dhan instrument download/parsing at all; hardcoded SIDs feed the §8 pulls; the Groww watch build never consumed this CSV — Phase B map, Verified; downloader deleted in the Phase C PRs) |
| `GET niftyindices.com ind_niftytotalmarket_list.csv` | `instruments.rs:646`; `main.rs:1978,2667,6806` | constituents (static ref) — the MAP source | **KEEP** |
| ~~`POST api.groww.in/v1/token/api/access`~~ | ~~`groww/auth.rs`~~ | Groww live-feed AUTH | **REMOVED 2026-07-02** — superseded by the shared token-minter SSM read (`groww-shared-token-minter-2026-07-02.md`); TickVault never mints |
| `GET GROWW_INSTRUMENT_CSV_URL` | `constants.rs:612`; `instruments.rs:639` | Groww master (static ref) — the MAP source | **KEEP** |
| `POST api.groww.in/v1/api/apex/v1/socket/token/create/` (per-session socket-token mint) | `constants.rs` (`GROWW_SOCKET_TOKEN_URL`); `feed/groww/native/socket_token.rs` | Groww live-feed AUTH — mints the per-session NATS user JWT from the SSM-read access token (NOT an access-token mint; the shared-minter lock 2026-07-02 is untouched). Recorded 2026-07-04 with the native-shadow-client authorization (`groww-second-feed-scope-2026-06-19.md` §35); the Python sidecar's SDK already makes this exact call | **RETIRED 2026-07-15** (was KEEP, added 2026-07-04) — the sole caller (the native shadow client's socket-token mint) was deleted with the Groww live feed; `GROWW_SOCKET_TOKEN_URL` deleted from constants.rs the same day. Row kept as history |
| `POST /v2/charts/intraday` (1m cross-verify) | `cross_verify_1m_boot.rs:283`; `constants.rs:1255` | MARKET-DATA pull | **REMOVE** (historical uses removed; the narrow §8 scheduled-pull use was re-authorized 2026-07-12) |
| `POST /v2/charts/historical` (prev-day OHLCV) | `prev_day_ohlcv_boot.rs:120`; `constants.rs:1259` | MARKET-DATA pull | **REMOVE** |
| `GET /v2/profile` (REST canary + mid-session watchdog) | `rest_canary_boot.rs:170`; `mid_session_watchdog.rs:30` | MARKET-DATA-adjacent poll | **REMOVE** (canary + watchdog) |
| `GET /v2/profile` (token_manager validity check) | `token_manager.rs:466` | AUTH-adjacent | **operator ruling** — auth-adjacent, not a price pull |
| `POST /v2/marketfeed/quote` / `/marketfeed/ltp` (open-price fallback) | `open_price_rest_fallback.rs:136`; `open_price_source.rs:48` | MARKET-DATA pull | **REMOVE** |
| `POST /v2/optionchain` (option-chain cache) | ~~`option_chain_cache_loader.rs`~~ | MARKET-DATA pull | **REMOVED 2026-06-28** (entire option_chain subsystem deleted per operator directive; historical uses removed — the narrow §8 scheduled-pull use was re-authorized 2026-07-12) |
| `GET /v2/positions` (orphan-position watchdog) | `orphan_position_watchdog_boot.rs:8` | TRADING (not market-data, not live-feed) | **operator ruling** — trading-adjacent, not a price pull |
| `aws-sdk-s3` GetObject/ListObjectsV2 on `s3://tv-prod-cold/crossverify/*` (BruteX cross-verify CSVs) | future `crates/app/src/brutex_crossverify_boot.rs` (code lands in the §37 follow-up PR) | internal artifact transfer from OUR OWN infrastructure (BruteX-produced CSVs in our own bucket) — NOT a market-data REST endpoint of Dhan or Groww | **KEEP** (added 2026-07-12) — authorized by the 2026-07-12 operator quote recorded in `groww-second-feed-scope-2026-06-19.md` §37 |
| `POST /v2/charts/intraday` (per-minute spot-1m scheduled pull, 3 IDX_I SIDs — §8) | future `crates/app/src/spot_1m_rest_boot.rs` + `crates/storage/src/spot_1m_rest_persistence.rs` (code lands in the §8 follow-up PR) | scheduled-pull market-data KEEP class (§8) — interval `"1"`, NIFTY=13 / BANKNIFTY=25 / SENSEX=51 spot ONLY, once per minute in-session, writes ONLY the new `spot_1m_rest` table | **KEEP** (added 2026-07-12 — §8) |
| `POST /v2/optionchain` + `POST /v2/optionchain/expirylist` (per-minute chain, config-gated — §8) | future `crates/app/src/option_chain_1m_boot.rs` + `crates/storage/src/option_chain_1m_persistence.rs` (code lands in the §8 follow-up PR) | scheduled-pull market-data KEEP class (§8) — 3 underlyings' CURRENT expiry, sequenced after the spot fetch, writes ONLY the new `option_chain_1m` table | **KEEP** (added 2026-07-12 — §8; DEFAULT-OFF pending the first-live-boot entitlement probe) |
| `GET api.groww.in/v1/historical/candles` (per-minute scheduled pull — 3 Groww spot indices + bounded selected option contracts; §9) | future `crates/app/src/groww_spot_1m_boot.rs` + `crates/app/src/groww_contract_1m_boot.rs` + `crates/storage/src/option_contract_1m_rest_persistence.rs` (code lands in the §9 follow-up PRs) | scheduled-pull market-data KEEP class (§9) — `candle_interval="1minute"`, day-granular window + client-side target-minute filter, once per minute in-session; writes ONLY `spot_1m_rest` (feed='groww') / the new `option_contract_1m_rest` table (never `ticks`/`candles_*`) | **KEEP** (added 2026-07-13 — §9) |
| `GET api.groww.in/v1/option-chain/exchange/{e}/underlying/{u}?expiry_date=...` (per-minute scheduled pull, 3 underlyings current expiry; §9) | future `crates/app/src/groww_option_chain_1m_boot.rs` (code lands in the §9 follow-up PRs) | scheduled-pull market-data KEEP class (§9) — CURRENT expiry from the already-ingested Groww instruments CSV, sequenced after the Groww spot fetch; writes ONLY `option_chain_1m` (feed='groww') (never `ticks`/`candles_*`) | **KEEP** (added 2026-07-13 — §9; DEFAULT-OFF pending first-live-session verification) |
| `POST /v1/order/create` · `/modify` · `/cancel` · `/v1/order-advance/*` (Groww order mutations) | future `crates/trading/src/oms/groww/{api_client,smart_orders}.rs` (feature `groww_orders`) | order MUTATION (behind the 4-gate live-fire lattice) | **KEEP-GATED** (added 2026-07-14 — §10; operator-live-orders-enable-ONLY, `GROWW_ORDER_LIVE_FIRE` false) |
| `GET /v1/order/{list,detail,status,status/reference,trades}` · `/v1/positions/*` · `/v1/holdings/user` · `/v1/margins/detail/*` · `/v1/user/detail` (Groww order-side reads) | future `crates/trading/src/oms/groww/{api_client,portfolio,margin,user}.rs` (feature `groww_orders`) | order-side READ (config-gated + market-hours-only) | **KEEP-GATED** (added 2026-07-14 — §10; per-area `[groww_orders]` flags DEFAULT-OFF) |
| Groww order/position-update WS (`subscribe_*_order_updates` / `subscribe_fno_position_updates`) on the existing feed connection | future `crates/trading/src/oms/groww/*` (feature `groww_orders`) | order/position update stream (no new connection) | **KEEP-GATED** (added 2026-07-14 — §10; gated with the order-side) |

**What removing the REMOVE rows costs (honest, fail-soft):** prev-day `*_pct_from_prev_day` columns read 0 (already boot-never-blocks); the 15:31 IST 1m cross-verify (the only OHLCV parity signal) goes away; REST canary + mid-session profile watchdog go away (lose early "REST died" detection); open-price fallback + option-chain cache go away. **The live feed, dedup, and mapping all keep working — nothing in the hot path or the master build breaks.**

---

## §4. What a PR that violates this lock looks like (REJECT)

- Adds or re-introduces ANY REST call that fetches PRICES / OHLCV / QUOTES / option-chain for Dhan or Groww.
- Removes a **KEEP** row (live-feed AUTH or a static instrument-master/constituent CSV) in the name of "no REST" — that breaks the feed (boot HALTS at universe build, or no token ⇒ WS can't connect).
- Strips a REST endpoint BEFORE the operator has confirmed the §2 market-data-only scope (this file is still PENDING confirmation).
- Re-adds `/v2/charts/intraday`, `/v2/charts/historical`, `/v2/marketfeed/*`, `/v2/optionchain`, or the `/v2/profile` canary/watchdog as a market-data source.
- Routes a `/v2/positions` or token-manager `/v2/profile` change without the operator ruling noted in §3.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote, only then can the PR land.

---

## §5. Auto-driver / Insta-reel explanation

> Sir, imagine the juice shop. The LIVE board on the wall shouts today's fruit prices every second — that is the only place prices come from. The new rule: stop phoning the supplier to ASK "what's the price of mango right now?" — no more price phone-calls, for Dhan OR Groww. BUT two phone-calls stay: (1) the call that gets the shop KEY to open in the morning (auth), and (2) the call for the printed list of WHICH fruits exist today (the static master list). Without the key the shop never opens; without the fruit list nobody knows which price belongs to which fruit. Those two are not "asking prices" — they are how the live board turns on at all. Everything else — the price-asking phone-calls — stop.

---

## §6. Trigger / auto-load

Always loaded. Activates on any session that:
- Edits `crates/common/src/constants.rs` (any Dhan/Groww REST URL constant)
- Edits any file under `crates/core/src/historical/`, `crates/core/src/option_chain/`, or any `*_rest_*` / `*_canary_*` / `*_watchdog_*` boot module
- Edits `crates/app/src/cross_verify_1m_boot.rs`, `prev_day_ohlcv_boot.rs`, `rest_canary_boot.rs`, `mid_session_watchdog.rs`, `open_price_rest_fallback.rs`, `option_chain_cache_loader.rs`
- Edits `crates/app/src/spot_1m_rest_boot.rs`, `crates/app/src/option_chain_1m_boot.rs`, `crates/storage/src/spot_1m_rest_persistence.rs`, `crates/storage/src/option_chain_1m_persistence.rs` (the §8 scheduled-pull modules)
- Edits `crates/app/src/groww_spot_1m_boot.rs`, `crates/app/src/groww_option_chain_1m_boot.rs`, `crates/app/src/groww_contract_1m_boot.rs`, `crates/storage/src/option_contract_1m_rest_persistence.rs` (the §9 scheduled-pull modules)
- Adds any new REST call to `api.dhan.co`, `api.groww.in`, or any market-data host
- Any file containing `charts/intraday`, `charts/historical`, `marketfeed/ltp`, `marketfeed/quote`, `optionchain`, `/v2/profile`, `generateAccessToken`, `RenewToken`, `api-scrip-master`, `niftyindices`, `GROWW_INSTRUMENT_CSV_URL`, `/v1/token`, `spot_1m_rest`, `option_chain_1m`, `v1/historical/candles`, `v1/option-chain`, `groww_spot_1m`, `groww_option_chain_1m`, `option_contract_1m_rest`, `v1/order/`, `order-advance`, `margins/detail`, `holdings/user`, `positions/user`, `v1/user/detail`, `groww_orders`, `GROWW_ORDER_LIVE_FIRE`, `oms/groww`, `broker_order_events`

---

# §8. Per-minute REST pipeline — scheduled-pull KEEP class (operator authorization 2026-07-12)

## §8.0 The verbatim operator demand (preserve exactly, do not paraphrase)

**Quote (2026-07-12, relayed verbatim via the coordinator session — typos preserved):**
> "nifty bank nifty and sensex... precisely at each and every minute close we need to fetch the one minute candle and we need to define a new table for this... let the websocket live feed generate but... based on every minute close precise close within a second... we can pull the nifty banknifty and sensex spot in a second... one min ohlcv... this is only for spot... once it is fetched successfully just save it in a new table... meanwhile for option chain also at the time of day start fetch all the [expiry] dates and at the precise time when it fetched the spot indices data then instantly in the second pull option chain also [with the] rate limiter also... so that... we have the entire options chain data of current expiry of entire nifty banknifty and sensex"

**Same-day operator additions (2026-07-12):**
1. **Sequencing** — the spot fetch fires FIRST at each minute close (~1s after the boundary); the option-chain fetch follows immediately after in the next available seconds (SEQUENCED, not simultaneous).
2. **Rate limit re-verified** — the option-chain limit was checked against the live Dhan docs on 2026-07-12 and is UNCHANGED: **1 unique request per 3 seconds**, with multiple DIFFERENT underlyings/expiries allowed concurrently inside the window (the v2.5 enhancement); Data-API budget 5/sec + 100,000/day unchanged.

## §8.1 The grant — one paragraph

Two narrowly-scoped, SCHEDULED market-data REST pulls join the KEEP set: (a) a **per-minute spot-1m fetch** — at each minute close during the NSE session, `POST /v2/charts/intraday` (interval `"1"`) for exactly THREE IDX_I spot SIDs (NIFTY=13, BANKNIFTY=25, SENSEX=51), persisting the just-closed official 1-minute OHLCV into the NEW `spot_1m_rest` QuestDB table (DEDUP UPSERT KEYS `(ts, security_id, exchange_segment, feed)` per I-P1-11 + feed-in-key); and (b) a **per-minute option-chain fetch** — one day-start `POST /v2/optionchain/expirylist` per underlying, then per minute (sequenced AFTER the spot fetch) `POST /v2/optionchain` for the same 3 underlyings' CURRENT expiry, persisting per-minute per-strike per-leg rows into the NEW `option_chain_1m` table. Both are COLD-PATH scheduled tasks in the app crate — never the tick hot path; the WebSocket candle pipeline is untouched; the 2-WS lock is untouched. Total request volume ≈ 6-9 requests/minute in-session (3 intraday + 3 optionchain + retries), trivially inside the Data-API 5/sec + 100K/day budget and the 1-unique-per-3s chain rule (3 DISTINCT underlyings may go concurrently).

## §8.2 The two KEEP endpoints — exact scope

| Endpoint | Scope (LOCKED) | Cadence | Destination table | Gate |
|---|---|---|---|---|
| `POST /v2/charts/intraday` (interval `"1"`) | 3 IDX_I spot SIDs ONLY: NIFTY=13, BANKNIFTY=25, SENSEX=51 | once per minute close, [09:15, 15:30) IST trading days | `spot_1m_rest` ONLY (never `ticks`/`candles_*`/`historical_candles`) | `[spot_1m_rest]` config; enabled in base.toml, serde default OFF |
| `POST /v2/optionchain` + `POST /v2/optionchain/expirylist` | the SAME 3 underlyings, CURRENT (nearest) expiry only; expirylist once at day start | chain once per minute, SEQUENCED after the spot fetch; 1 unique req/3s honored (3 distinct underlyings concurrent); `client-id` header required | `option_chain_1m` ONLY | `[option_chain_1m]` config — shipped **DEFAULT-OFF** pending the first-live-boot entitlement probe; **flipped ON 2026-07-13 after the probe PASSED live (§8.7)** |

## §8.3 Probe outcome (2026-07-12, recorded honestly)

The option-chain entitlement (absent June 2026 — the DH-902/806 class that got the old subsystem deleted 2026-06-28) is **UNPROBEABLE from the development sandbox**: `dhan.co` egress is 403-blocked at the proxy AND no minted access token exists in SSM (only client-id/client-secret/totp-secret; minting from the sandbox would invalidate the live prod token per `authentication.md` rule 5). Therefore the chain half ships **config-gated DEFAULT-OFF** with a **first-live-boot entitlement probe** (one bounded `POST /v2/optionchain/expirylist` call) that reports the verdict via Telegram; only then does the operator enable it per config. The spot half is INDEPENDENT and is NEVER blocked by the chain half.

## §8.4 Honest envelope (mandatory per operator-charter §F)

- **Just-closed-minute availability is UNDOCUMENTED and UNVERIFIED-LIVE** — how fast Dhan's intraday endpoint surfaces the minute that closed 1 second ago is unknown until the first live session. The fetcher does bounded in-minute retries and MEASURES the close-to-data latency with a histogram — a slow/empty response is loud (typed ErrorCode + counter), never a false-OK.
- The chain entitlement is UNPROVEN until the live boot probe (§8.3); the chain half stays OFF until proven + operator-enabled.
- Disk envelope (honest): `option_chain_1m` ≈ ~337K rows ≈ ~70 MB/day at ~150 strikes ≈ ~6.3 GB/90d (~21% of the 30 GB EBS — *2026-07-19 correction: live root verified 30 GiB by `describe-volumes`; the 2026-07-13 approved 30→50 grow was recorded but never physically applied — see `daily-universe-scope-expansion-2026-05-27.md` §7; this line read "50 GB since 2026-07-13" until 2026-07-19. Same-day ruling (Quote 9): 30 GB formally ACCEPTED, the grow CANCELLED*) — retention/partition-manager registration is a flagged follow-up in the code PRs; `spot_1m_rest` is trivial (~1,125 rows/day).
- Cold-path only; zero hot-path involvement; zero new WebSocket; §28 indicators/strategies boundary untouched; token via the existing TokenManager (never logged).

## §8.5 What a PR that violates this grant looks like (REJECT)

- Extends either endpoint to ANY other SID, underlying, segment, expiry set, or timeframe (a 4th index, stocks, non-current expiries, interval ≠ "1") without a fresh dated operator quote HERE first.
- Writes either fetch's output to `ticks`, `candles_*`, `historical_candles`, or any table other than `spot_1m_rest` / `option_chain_1m` (live-feed purity).
- Involves the tick hot path, the WS read loops, or any per-tick code in the scheduled fetch (cold-path only).
- Wires either table into strategy/indicator/risk paths (§28 boundary of `daily-universe-scope-expansion-2026-05-27.md`).
- Converts the per-minute schedule to unbounded/tighter polling, or exceeds the 1-unique-per-3s chain rule / Data-API 5-per-sec budget.
- Ships `[option_chain_1m]` DEFAULT-ON before the entitlement is proven live AND a fresh dated operator quote is recorded here.
- Re-adds any OTHER §3 REMOVE-row endpoint under cover of this grant.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this §8 FIRST with a dated quote.

## §8.6 Auto-driver / Insta-reel explanation

> Sir, the juice shop's LIVE price board keeps shouting as before — nothing changes there. NEW: once every minute, exactly when the minute hand ticks, the boy makes ONE quick phone call to the supplier for the OFFICIAL last-minute price card of just the 3 big baskets (NIFTY, BANKNIFTY, SENSEX) and files it in a brand-new drawer. Right after that call, he makes a second call for the option-coupon price sheet of the same 3 baskets — but that second phone stays UNPLUGGED until we confirm the supplier will actually answer it (last time they refused). Six-ish short calls a minute, filed in two new drawers, never touching the live board or the old drawers.

## §8.7 — 2026-07-13: entitlement probe PASSED live → `[option_chain_1m].enabled` flips to true (dated note) + spot window hotfix recorded

**Entitlement evidence (Verified, CloudWatch `/tickvault/prod/app`, quoted verbatim):** the first live boot's entitlement probe PASSED at **08:31:49 IST on 2026-07-13** —
> `"option_chain_1m: entitlement probe PASSED — chain data is available; pipeline stays OFF until the config is flipped (the Telegram body carries the plain-English action; the exact key lives HERE)","symbol":"NIFTY","expiries":18,"config_key":"[option_chain_1m].enabled"`

— i.e. `POST /v2/optionchain/expirylist` answered for the granted underlyings (18 expiries returned for NIFTY; the chain data plan is entitled).

**Authorization (VERBATIM, relayed via the coordinator session, 2026-07-13):**
> "flip [option_chain_1m] enabled=true — the boot entitlement probe PASSED this morning (08:31:49, 'chain data is available'), and the operator's standing directive demands the chain live; make sure the chain still fires via the 2.5s fallback when the spot fetch fails so one broken leg never silences the other"

Per that relayed directive (rooted in the operator's standing 2026-07-12 §8.0 grant — chain data live once entitled), and with the entitlement now PROVEN LIVE above, `config/base.toml [option_chain_1m].enabled` flips `false → true` (PR carrying this note). The 2.5 s fallback demand in the quote is VERIFIED, not assumed: the spot leg publishes its minute-done signal unconditionally after every fire (success or failure — pinned by `option_chain_1m_wiring_guard`), and the chain's `test_wait_until_chain_fire_signal_wakes_early_and_fallback_bounds` arms (a)–(e) prove the no-receiver / stale-signal / dropped-sender / pre-satisfied cases all bound to the fallback timer — one broken leg never silences the other. This satisfies the §8.5 REJECT row ("Ships `[option_chain_1m]` DEFAULT-ON before the entitlement is proven live AND a fresh dated operator quote is recorded here") — both conditions are now met and recorded HERE first, in the same PR as the flip. The serde DEFAULT stays OFF (fail-safe: an absent section still means disabled); `probe_and_report` stays true (inert while enabled; the automatic fallback canary on any rollback). Everything else in §8.2's scope table — 3 underlyings, current expiry only, per-minute cadence, `option_chain_1m` table only — is UNCHANGED.

**Same-day spot-window hotfix (recorded for §8.2 accuracy):** the spot half's FIRST live session (2026-07-13) failed every minute (`SPOT1M-01`, `ok=0/errors=0/empty=3` from 09:16 IST — Dhan answered `2xx` without the target candle) because the fetcher used a same-date `[minute open, open+60s]` request window, a shape never live-proven. The fix (same PR) switches each per-minute fire to the ONLY live-proven window shape — day-granular `fromDate = D 00:00:00, toDate = D+1 00:00:00` (the exact body the 15:31 cross-verify + prev-day fetchers use) — with client-side filtering to the exact minute, plus a previous-minute backfill on every fire AND one bounded post-session sweep (~15:33:30 IST since the same-day 429-coordination follow-up — it clears the 15:31–15:33 cross-verify 429 burst window) that repairs any session minute still missing above the per-SID persisted watermark (DEDUP-idempotent re-appends; the sweep is what gives the final 15:29 candle a repair path). Cadence, SIDs, table, and budget are UNCHANGED (still 3 requests per minute close; a full-day body is ~20 KB, far inside the 2 MiB cap and the Data-API budget) — this is a request-SHAPE correction inside the existing §8 grant, not a scope change.

**2026-07-14 pacing pointer:** the §8 legs (spot-1m + option-chain, incl. ladder/sweep/probes) now pass through the shared self-tuning Dhan Data-API rate limiter (3 rps cap, 2 rps floor — operator pacing directive 2026-07-14; design + honesty envelope in `rest-1m-pipeline-error-codes.md` §2f); the §8 grant's scope, cadence, tables and budgets are UNCHANGED.

---

# §9. Groww per-minute REST pipeline — scheduled-pull KEEP class (operator authorization 2026-07-13)

> **⚠ §9 IS RETIRED 2026-08-21 — the entire Groww REST pipeline is REMOVED by operator
> directive.** Verbatim: *"Dude along with all these see clealry note whqtber is entirely
> related to groww feed remove everything entilrey dude okay? Do you understand what I'm
> asking dude"*. Full dated authorization + REJECT list:
> `websocket-connection-scope-lock.md` §"2026-08-21 (THIRD quote of the day)". Effects:
> every §9 KEEP row (the Groww spot-1m candles pull, the Groww option-chain pull, the
> bounded per-contract 1m pull) moves KEEP → **REMOVED**, and the matching §3 inventory
> rows are annotated rather than deleted, per house convention. The §10 Groww ORDER-SIDE
> KEEP-GATED rows are RETIRED with them (the gates never opened; no live Groww order was
> ever placed). **§8 — the DHAN spot-1m + option-chain KEEP class — is UNTOUCHED and
> still binds**, as does §11's re-assertion that those two Dhan legs are enabled classes.
> The §1/§2 market-data-REST ban is otherwise unchanged, and the shared tables
> (`spot_1m_rest`, `option_chain_1m`, `rest_fetch_audit`) survive as Dhan-only writers —
> they are feed-generic by design and the `feed` column stays in every DEDUP key so the
> historical `feed='groww'` rows remain readable. Sections below are retained as
> historical audit; where they conflict with this banner, the banner wins.

## §9.0 The verbatim operator demand (preserve exactly, do not paraphrase — typos included)

**Quote 1 (2026-07-13, the directive — relayed verbatim via the coordinator session):**
> "can we implement the same Groww one min fetch which is precisely very similar to the same Dhan — REST api pull ohlcv entirely and even then instantly option chain api also... for Groww live feed and now we planned to add this live REST which is very similar to Dhan. That's it."

**Quote 2 (2026-07-13, latency visibility):**
> "always clearly note within a second — or within how many seconds precisely — we are fetching this live real OHLCV, along with the option chain API."

(Full authorization record incl. the verbatim-intent fill-model context lives in
`groww-second-feed-scope-2026-06-19.md` §38 — the §33 partial-supersession edit this lock's
§4 protocol requires.)

## §9.1 The grant — one paragraph

The §8 scheduled-pull KEEP class extends to GROWW, same shape: (a) a **per-minute spot-1m
fetch** — at each in-session minute close, `GET https://api.groww.in/v1/historical/candles`
(`candle_interval="1minute"`) for the 3 Groww spot indices (`NSE-NIFTY`, `NSE-BANKNIFTY`
segment CASH exchange NSE; `BSE-SENSEX` segment CASH exchange BSE), one DAY-granular
`start_time`/`end_time` window per fire with client-side target-minute filtering (the
Dhan-#1499 lesson baked in from day one), persisting into the EXISTING `spot_1m_rest` table
tagged `feed='groww'` (feed already in the DEDUP key); (b) a **per-minute option-chain
fetch** — `GET /v1/option-chain/exchange/{e}/underlying/{u}?expiry_date=...` for the SAME 3
underlyings' CURRENT expiry (resolved from the already-ingested daily Groww instruments CSV
— no new expiry endpoint, zero rate cost), sequenced after the Groww spot leg, persisting
into the EXISTING `option_chain_1m` table tagged `feed='groww'`; (c) a bounded
**per-contract 1m fetch** — the same candles endpoint with `segment=FNO` for a capped
selected set of active option contracts (the fill-model leg), persisting into ONE new
`option_contract_1m_rest` table with `feed` in its DEDUP key. All three are COLD-PATH
scheduled tasks — never the tick hot path; the Groww live WS capture and the Dhan §8 legs
are untouched. The §33 bulk-history/backtest-fetch ban otherwise stands: no multi-day
sweeps, no past-day backfills beyond the one-minute-lookback + 15:31 post-session sweep
patterns (the Dhan PR #1499 pattern, pending merge).

## §9.2 The KEEP endpoints — exact scope

| Endpoint | Scope (LOCKED) | Cadence | Destination table | Gate |
|---|---|---|---|---|
| `GET api.groww.in/v1/historical/candles` (spot) | 4 Groww spot indices ONLY (was 3; INDIA VIX added 2026-07-13 per `groww-second-feed-scope-2026-06-19.md` §38.7 — runtime-resolved from the day's Groww master, SPOT ONLY): `NSE-NIFTY` / `NSE-BANKNIFTY` / `BSE-SENSEX` + the resolved INDIA VIX groww_symbol, segment CASH; `candle_interval="1minute"`; day-granular window + client-side minute filter; `Authorization: Bearer <shared-minter SSM read-only token>` + `x-api-version: 1.0` | once per minute close, [09:15, 15:30) IST trading days, + one bounded 15:31 sweep (the Dhan PR #1499 pattern, pending merge) | `spot_1m_rest` ONLY, `feed='groww'` (never `ticks`/`candles_*`/`historical_candles`) | `[groww_spot_1m]` config, serde default OFF, base.toml opts in |
| `GET api.groww.in/v1/option-chain/exchange/{e}/underlying/{u}?expiry_date=...` | the SAME 3 underlyings, CURRENT (nearest ≥ today) expiry only, from the already-ingested Groww instruments CSV | ~~once per minute, SEQUENCED after the Groww spot fetch; own min-gap pacing~~ **SUPERSEDED 2026-07-14 by the §9.7 auto-ladder:** once per minute on the chain leg's OWN minute-boundary timer (close + 300 ms), the 3 underlyings CONCURRENT within the wave (min-gap deleted) | `option_chain_1m` ONLY, `feed='groww'` | `[groww_option_chain_1m]` config — **DEFAULT-OFF** pending first-live-session verification + a dated note; **flipped ON in base.toml 2026-07-13 after the live probe PASSED (§38.6 of the groww-scope file; the serde DEFAULT stays OFF)** |
| `GET api.groww.in/v1/historical/candles` (contracts) | a BOUNDED selected set of active option contracts (`segment=FNO`, `groww_symbol` like `NSE-NIFTY-04Jan24-19200-CE`; envelope cap per minute; selection fed by the chain snapshot / instruments master) | once per minute, after the chain leg | NEW `option_contract_1m_rest` ONLY (`feed` in DEDUP key; retention registered) | config-gated, serde default OFF |

## §9.3 Honest envelope (mandatory per operator-charter §F)

- **Just-closed-minute freshness is UNDOCUMENTED and UNVERIFIED-LIVE** — Groww documents no
  availability delay for the sealing minute; the first live session is the probe. The
  fetchers MEASURE close-to-data latency (per-row column + `tv_groww_spot1m_close_to_data_ms`
  / `tv_groww_chain1m_close_to_data_ms` histograms + a plain-English daily digest line) and
  NEVER assert "within a second" — the measured number is always shown (Quote 2).
- **Rate-bucket unknown:** the docs' type-level table (Live Data 10/sec + 300/min; no
  per-day cap) does not NAME the `/historical/*` or `/option-chain/*` buckets — conservatively
  ASSUMED Live Data. Our ~6–12 requests/min is far inside either reading; 429s are counted,
  never out-polled. Capacity verdict (2026-07-13 docs research): the buckets are TYPE-LEVEL
  POOLED (exhausting one API throttles the whole type; Orders changed 15→10 between Dec'25
  and Mar'26 — the numbers CAN change, re-verify on the box), and Groww documents NO
  Retry-After/ban/cooldown for 429 while the SDK ships ZERO client-side throttling — pacing
  (minute-boundary bursts spread to ≤6 req/s against the shared 10/s ceiling) and timeouts
  are entirely ours; worst-case ~18 req/min ≈ 6% of the 300/min budget solo, ~66% with
  in-session BruteX co-tenancy — still inside.
- **Shared-account budget note:** whether Groww scopes rate limits per API key, per token,
  or per account is officially UNSTATED (Unknown). Both BruteX and TickVault present the ONE
  shared daily minter token, so the bucket is EFFECTIVELY shared for us either way — an
  Assumed inference from the single shared token, not a documented fact. BruteX's bulk
  historical pulls are nightly / post-market — TIME-DISJOINT from our in-session per-minute
  pulls (Assumed; coordinate before relying on tighter headroom).
- V2 candle timestamp type (string vs epoch) is UNVERIFIED-LIVE — defensive dual-format
  parse; zero-trade option-strike minutes may be legitimately absent (counted, never
  fabricated).
- Cold-path only; zero hot-path involvement; zero new WebSocket; token READ-ONLY from SSM
  (never minted — `groww-shared-token-minter-2026-07-02.md`); §28 indicators/strategies
  boundary untouched.
- **Burst arithmetic (2026-07-14 auto-ladder — supersedes the "≤6 req/s" pacing line
  above for the Groww legs; full contract in §9.7; numbers RECOMPUTED the same day for
  the fix-round HIGH-1 rung jitter):** the shipped `two_wave` tier bursts ≤ 4 req/s at
  the boundary (3 chain at close+300 ms, 4 spot at close+1,350 ms — the 1,050 ms wave
  separation keeps every rolling second single-wave, and both legs compute the wake from
  the MILLISECOND clock so the separation holds exactly — fix-round CRITICAL-1; the
  original whole-second else-branch could collapse it to ~51 ms); the probe-gated
  `seven_concurrent` tier bursts 7 req/s. Each spot target's re-poll rung schedule is
  jittered by slot × 150 ms in ALL tiers (fix-round HIGH-1 — the Dhan-leg precedent), so
  the honest vendor-lag worst case (jittered rung stragglers inside one rolling second)
  is 6 req/s solo for the rungs, 8 with the contract leg's 2/s (pre-jitter: 8 and 10 —
  AT the ceiling), and 9 for the seven tier's initial-wave-plus-rungs (pre-jitter 11,
  above the ceiling) — all const-asserted; still zero margin over the assumed BruteX
  ~3/s co-tenancy, which is why any live 429 AUTO-DEMOTES the tier down the §9.7(3)
  ladder (one coded warn per level + `tv_groww_rest_burst_demoted_total{level}`), never
  out-polled. LOW-2 honest note (same fix round): the contract leg now starts ~2–6 s
  earlier than pre-Stage-1 (the chain leg completes faster), so its 2/s stream stacks
  INSIDE the spot rung windows — counted inside the solo const-assert, zero co-tenant
  margin beyond it, accepted by design. Per-minute totals stay ≤ 55 (30 contract + 20
  spot + 3 chain + 2 warm-up) ≈ 18% of the 300/min budget — const-asserted in
  `crates/common/src/constants.rs`.
- **Decision-freshness gate (2026-07-13, recorded with PR-4 — the operator's verbatim
  intent: "we cannot rely on backfill — within the particular second or few seconds it
  should definitely be pulled for TRADING DECISIONS; we need precise filling"):**
  backfill/sweep-repaired rows in `spot_1m_rest` / `option_chain_1m` /
  `option_contract_1m_rest` are RECORD-COMPLETENESS data (backtest parity, cross-verify,
  audit) — NEVER trading-decision inputs. Any future strategy consumer MUST fail closed on
  staleness (a row older than a configured freshness threshold ⇒ no trade that minute);
  stale rows are mechanically distinguishable today (`close_to_data_ms ≥ 60000` on
  backfilled rows vs ~1-2 s own-fire; the `rest_fetch_audit` outcome names the recovery
  path). No strategy code without its own dated operator scope (§28 boundary). Full rule:
  `groww-second-feed-scope-2026-06-19.md` §38.8.

## §9.4 What a PR that violates this grant looks like (REJECT)

- Any BULK Groww historical fetch (multi-day sweeps, past-day backfills beyond the
  one-minute-lookback / 15:31-sweep patterns) — §33 of the groww-scope file stands.
- Extends any leg to other symbols/segments/expiries/timeframes, or lifts the contract-set
  envelope cap, without a fresh dated operator quote HERE (and in §38) first.
- Writes any leg's output to `ticks`, `candles_*`, `historical_candles`, or any table other
  than `spot_1m_rest` / `option_chain_1m` / `option_contract_1m_rest` (live-feed purity).
- Converts the per-minute schedule to unbounded/tighter polling, or exceeds the shared Live
  Data 10/sec + 300/min budget share.
- Mints a Groww token, caches one past an auth failure, or reads credential params
  (token-minter lock 2026-07-02).
- Ships `[groww_option_chain_1m]` DEFAULT-ON before first-live-session verification AND a
  fresh dated quote recorded here.
- Involves the tick hot path, the WS read loops, or any strategy/indicator/risk path.
- Re-adds any OTHER §3 REMOVE-row endpoint (Dhan or Groww) under cover of this grant.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator
must update this §9 FIRST with a dated quote.

## §9.5 Auto-driver / Insta-reel explanation

> Sir, remember the two new drawers we opened for supplier Dhan's official minute-price cards?
> The operator now says: do the EXACT same thing with supplier Groww's phone. Once every
> minute the boy makes one quick call to Groww for the official last-minute price card of the
> same 3 big baskets, files it in the SAME drawers (with a "from Groww" stamp so the two
> suppliers' cards never mix up), then a second call for Groww's option-coupon sheet, and a
> third short call for the price cards of just the FEW coupons we actually care about — those
> go in one small new drawer. And on every card the boy writes HOW MANY SECONDS after the
> minute ended the card arrived — we never brag "within a second", we show the stopwatch. What
> he must NEVER do: phone Groww asking for last month's whole record book — that call is still
> banned.

## §9.6 Trigger (auto-loaded)

Covered by the §6 trigger list (extended 2026-07-13 with the `groww_spot_1m` /
`groww_option_chain_1m` / `option_contract_1m_rest` / `v1/historical/candles` /
`v1/option-chain` strings and the §9 module paths).

## §9.7 — 2026-07-14: rate-safe AUTO-LADDER replaces the sequenced-after-spot cadence (dated amendment)

**Operator authorization (2026-07-14, relayed verbatim via the coordinator
session):** *"approved and go ahead with the recommendation"* — the
recommendation being the rate-safe auto-ladder rebuild of the per-minute Groww
REST legs so worst-case DECISION data lands < 5 s after minute close, with the
operator's stated preference for the 7-concurrent burst held PROBE-GATED
behind the off-hours rate probe (the shipped default is the safer two-wave
shape).

**What changes (the §9.1/§9.2 "sequenced after the Groww spot fetch" +
"own min-gap pacing" cadence contract is SUPERSEDED for the Groww legs):**

1. **Self-imposed pacing DELETED.** The chain leg no longer waits for the
   spot leg's minute-done signal nor the 2.5 s fallback timer — it fires on
   its OWN minute-boundary timer; the 1,000 ms inter-underlying chain min-gap
   is deleted; the spot targets fetch CONCURRENTLY instead of sequentially.
   (~4.5 s of self-imposed latency removed; measured chain round-trip is
   ~0.2 s per the 2026-07-13 live probe.)
2. **Two burst tiers (`[groww_rest_burst] tier`):**
   - `two_wave` (the SHIPPED default; serde default too): 3 chain requests
     concurrently at close + 300 ms; 4 spot requests concurrently at
     close + 1,350 ms. The 1,050 ms wave separation keeps every rolling
     1-second window single-wave ⇒ boundary burst ≤ 4 req/s.
   - `seven_concurrent` (the operator preference — PROBE-GATED, never the
     default): all 7 requests concurrently at close + 300 ms (7/10 of the
     documented ceiling solo; zero margin at the assumed BruteX ~3/s
     co-tenancy anchor). Promotion = a config flip + a fresh dated note HERE
     after the §9.7(5) probe passes.
3. **AUTO-DEMOTE (a LADDER since the same-day fix round — review
   MEDIUM-1):** any HTTP 429 on any Groww REST leg (spot / chain /
   contract) steps the session ONE shape down
   `seven_concurrent → two_wave → two_wave + 350 ms intra-wave stagger →
   fully-sequential-within-wave` (the sequential floor spaces wave slots
   a whole per-slot hard budget apart — the pre-Stage-1 one-at-a-time
   pacing shape; a 429 at the floor is a no-op). One coded warn per
   demotion LEVEL (SPOT1M-01 / CHAIN-02 `stage="burst_demoted"` — no new
   ErrorCode variants) + `tv_groww_rest_burst_demoted_total{level}`; boot
   resets to the configured tier.
4. **WARM-UP (`[groww_rest_burst] warm_up`, base.toml ON):** at minute
   boundary − 4 s each leg sends ONE lightweight UNAUTHENTICATED GET on its
   own client (response discarded; the 401/400 still re-establishes an
   idle-closed TLS/H2 connection before the critical window), bounded by its
   own 3 s timeout so a fire can never start late. ≤ 2 requests in a second
   that never overlaps the waves — counted in the budget arithmetic.
5. **OFF-HOURS RATE PROBE (env-gated `TICKVAULT_GROWW_RATE_PROBE=1`, default
   OFF — OPERATOR-TRIGGERED, never scheduled):** escalating bursts
   4 → 6 → 8 → 11 req/s (one second each, ~10 s pause between steps, max 2
   rounds, ~58 requests total) against the granted candles endpoint;
   REFUSED inside the [08:30, 16:00) IST wall-clock blackout window on ANY
   day — trading day or not (same-day fix round, SECURITY-MEDIUM: the
   original `trading_day && [09:00, 15:35)` gate depended solely on the
   trading calendar, so a stale holiday list could fire the burst
   mid-session; the unconditional wall-clock window has no calendar lookup
   to fail-open on). CO-TENANCY (review MEDIUM-2): BruteX's bulk pulls are
   nightly/post-market (§9.3) and the blackout cannot see BruteX's
   schedule — the operator MUST coordinate the probe run with BruteX's
   nightly window before arming the env var. Per-step 429 count + latency
   as structured log lines + counters; writes NO data tables. Its verdict
   is the ONLY gate that can promote `seven_concurrent`. (The probe's
   11 req/s top step over-tests: with the fix-round rung jitter the
   seven-tier lag-worst shape is 9 req/s — the 11 step is kept as
   deliberate margin.)
6. **What does NOT change:** the KEEP endpoints, destination tables, DEDUP
   keys, token discipline (SSM read-only, never minted), the bounded spot
   re-poll ladder + backfill + 15:31 sweep (never-skip capture is a hard
   constraint — rungs landing after the decision window are record-only),
   the per-leg escalation edges, the Dhan legs (completely untouched), the
   chain→contract sequencing, and the §38.8 decision-freshness gate.
   Timeouts TIGHTEN: chain 10 s → 4.5 s, spot 5 s → 3.5 s (per-leg budgets
   re-derived and const-asserted).

---

# §10. Groww ORDER-SIDE REST/WS — gated KEEP class (operator authorization 2026-07-14)

## §10.0 The verbatim operator authorization (2026-07-14 — preserve exactly)

**Operative authorization — the operator's DIRECT dated in-thread confirmation (2026-07-14, typed by Parthiban into this session's thread, event `321350d4-cf70-4a1c-88fc-36584527c8fd`), verbatim:**

> "confirm — apply the Groww order scope-unlock PR-0. Build only, behind the OFF switch, no live orders"

**Originating build directive (same day, relayed verbatim via the coordinator session):**

> "build design architect and even integrate everything entirely… only when I turn on/off or enable live orders then live orders should be placed."

## §10.1 The grant — one paragraph

The market-data-only ban of §1/§2 is NARROWLY extended to authorize the Groww
ORDER-SIDE surface — but ONLY behind the 4-gate live-fire lattice of
`groww-second-feed-scope-2026-06-19.md` §39. NO Groww quote/LTP REST pull is
authorized (that stays BANNED — §3 REMOVE class); NO bulk historical/backtest
fetch is authorized (§33 stands). Order MUTATIONS (create/modify/cancel, smart
orders) are HARD-LOCKED until the operator's explicit future dated live-orders
enable (§39.2 Gates 1–4 all aligned). Order-side READ-ONLY GETs (order/trade
list+detail+status, positions, holdings, margins, user profile) are per-area
config-gated DEFAULT-OFF + market-hours-only — the cold-path scheduled-read
discipline, no order placed. The order-update / position-update WS subscriptions
ride the EXISTING single Groww feed connection (no new connection). All calls
carry the shared-minter SSM READ-ONLY token (never mint — token-minter lock
2026-07-02). Cold-path only; the tick hot path + the live feed + the §8/§9 REST
legs are untouched.

## §10.2 KEEP-GATED endpoint table (all gated per §39.2 — nothing runs by default)

| Endpoint / function | Transport | Class | Verdict |
|---|---|---|---|
| `POST /v1/order/create` · `/modify` · `/cancel` | REST | order MUTATION | **KEEP-GATED** — operator-live-orders-enable-ONLY (Gates 1–4; `GROWW_ORDER_LIVE_FIRE` false) |
| `/v1/order-advance/create` · `/modify/{id}` · `/cancel/{...}` (smart orders) | REST | order MUTATION | **KEEP-GATED** — operator-live-orders-enable-ONLY |
| `GET /v1/order/list` · `/detail/{id}` · `/status/{id}` · `/status/reference/{ref}` · `/trades/{id}` | REST | order-side READ | **KEEP-GATED** — `[groww_orders] orders_read`, default-OFF + market-hours-only |
| `GET /v1/positions/user` · `/positions/trading-symbol` | REST | portfolio READ | **KEEP-GATED** — `[groww_orders] portfolio_read`, default-OFF + market-hours-only |
| `GET /v1/holdings/user` | REST | portfolio READ | **KEEP-GATED** — `[groww_orders] portfolio_read`, default-OFF + market-hours-only |
| `GET /v1/margins/detail/user` · `POST /v1/margins/detail/orders` (margin calculator) | REST | margin READ | **KEEP-GATED** — `[groww_orders] margin_read`, default-OFF + market-hours-only |
| `GET /v1/user/detail` | REST | user READ | **KEEP-GATED** — `[groww_orders] user_read`, default-OFF + market-hours-only |
| order-update WS (`subscribe_equity_order_updates` / `subscribe_fno_order_updates`) + `subscribe_fno_position_updates` (on the EXISTING feed connection) | WS | order/position update stream | **KEEP-GATED** — gated with the order-side (no new connection) |

## §10.3 What a violating PR looks like (REJECT)

- ANY Groww order-side call site OUTSIDE `crates/trading/src/oms/groww/` or not
  behind the 4-gate lattice (§39.2 / Gate 5).
- ANY Groww quote/LTP REST pull — stays BANNED (§3 REMOVE class; §1/§2).
- ANY bulk Groww historical/backtest fetch — stays BANNED (§33).
- A Groww MUTATING order request while any of Gates 1–3 is unaligned.
- Minting a Groww token, caching one past an auth failure, or reading the
  credential params (token-minter lock 2026-07-02).
- Flipping a read-only gate ON without market-hours gating, or DEFAULT-ON, or a
  live-fire gate without a fresh dated quote editing §39 + §10 first.

Any such PR MUST be rejected in review even if the operator approves verbally —
the operator must update THIS §10 first with a fresh dated quote.

## §10.4 Honest envelope (mandatory per operator-charter §F)

> "The order-side ships DARK behind §39's four build-failure-ratcheted gates —
> a default build has no Groww order code and no request fires until a dated
> operator enable. Read-only GETs are market-hours-gated and place no order.
> NOT claimed: live Groww order behaviour (PAPER-MODE tested vs
> `docs/groww-ref/16` only, UNVERIFIED-LIVE) or that our key carries order
> ENTITLEMENT (UNPROBED). Quote/LTP REST + bulk history stay banned."

## §10.5 Auto-driver / Insta-reel explanation

> Sir, the "no price-phone-calls" rule still holds — no ringing Groww to ask a
> price. NEW: we may build an ORDERING desk with a four-lock safe on the "place
> order" button. Reading the order book / holdings / margin is allowed but ONLY
> during shop hours and ONLY when that little switch is on. The real "place a
> live order" button stays behind all four locks until you, on a dated day, open
> them yourself.

## §10.6 Trigger (auto-loaded)

Covered by the §6 trigger list (extended 2026-07-14 with `v1/order/`,
`order-advance`, `margins/detail`, `holdings/user`, `positions/user`,
`v1/user/detail`, `groww_orders`, `GROWW_ORDER_LIVE_FIRE`, `oms/groww`, and the
`broker_order_events` seam).

---

# §11 — 2026-07-14 operator re-assertion: Dhan spot-1m + option-chain REST are ENABLED classes

## §11.0 The verbatim operator quotes (preserve exactly, do not paraphrase — expletives included)

**Quote 1 (2026-07-14, verbatim):**
> "dhan option chain should be enabled why the fuck this is disabled ... only live feed is disabled"

**Quote 2 (2026-07-14, verbatim):**
> "market feed ... it should be clearly historical data api for spot 1m"

## §11.1 The re-assertion — one paragraph

`POST /v2/charts/intraday` (interval `"1"` — the spot-1m leg) and
`POST /v2/optionchain` (+ `POST /v2/optionchain/expirylist`) are **KEEP /
RE-AUTHORIZED** classes. This section RE-ASSERTS the existing §8 grant
(2026-07-12; the chain half flipped ON 2026-07-13 after the live entitlement
probe PASSED — §8.7) with tonight's quotes, so the §3 REMOVE-row HISTORY
(the pre-§8 removals of `charts/intraday` / `optionchain`) can never again be
misread as "these endpoints are disabled". The §3 rows already carry the
re-authorization annotations; this section is the dated, quote-backed record
that they are ENABLED classes, permanently.

## §11.2 The factual config truth (Verified, 2026-07-14)

- **Both legs are `enabled = true` in `config/base.toml` and running in
  prod** — `[spot_1m_rest]` (the §8 spot half) and `[option_chain_1m]`
  (flipped ON 2026-07-13 per §8.7; the serde DEFAULT stays OFF, fail-safe).
- **The ONLY disabled Dhan surface is the live WebSocket feed** — the
  2026-07-13 retirement (`websocket-connection-scope-lock.md` "2026-07-13
  Amendment"; operator Quote 1 above: "only live feed is disabled").
- **The apparent "disabled" symptom is NOT a config state:** it is the
  `charts/intraday` VENDOR-SIDE zero-candle serving issue — the SPOT1M-01
  `empty` class, running ~14 days (2xx responses carrying no same-day
  candles), diagnosed by the #1543/#1524 raw-body serving-delay probes at
  the next 09:16 IST fire (`rest-1m-pipeline-error-codes.md` §1, the
  2026-07-14 empty-class split + one-shot probes). A vendor not serving
  data is a measured degrade, never a reason to call the leg "disabled".

**2026-07-17 truth-sync:** the statement above ('Both legs are enabled = true
in config/base.toml and running in prod') describes the pre-cadence state. As
of 2026-07-17 the four legacy per-minute leg sections are `enabled = false` in
base.toml and the SAME pulls fire from the cadence scheduler lane
(`[cadence] enabled = true`, real broker executors — see
`cadence-error-codes.md` §0c). Endpoints, SIDs, tables, DEDUP keys and budgets
are unchanged; only the fire-timing owner moved. The §8/§9 grants continue to
govern the cadence executors' calls.

## §11.3 What stays REMOVED (explicit, per Quote 2)

**`POST /v2/marketfeed/quote` and `POST /v2/marketfeed/ltp` REMAIN REMOVED**
(the §3 REMOVE row stands). Per Quote 2, the spot-1m data source is
"clearly historical data api" — any cadence-scheduler spot slot MUST use
`charts/intraday` under the Data-API 5/sec bucket, NEVER the marketfeed
quote endpoints (Quote-API 1/sec bucket). Re-adding a marketfeed caller
requires a fresh dated quote HERE first.

## §11.4 Cross-references + honest envelope

Consistent with `dhan-rest-only-noise-lock-2026-07-14.md` §1 (same-day
operator lock: "for Dhan except spot 1m and option chain nothing else should
work") and with this PR's cross-source-chain-coverage rule. Honest envelope:
this section changes ZERO code and ZERO config — it is the dated record that
the two legs are authorized + enabled; whether Dhan actually SERVES same-day
intraday candles remains the open vendor-side question the §11.2 probes
measure (the chain leg is serving normally — 735/735 on 2026-07-14).

## §11.5 Trigger (auto-loaded)

Covered by the §6 trigger list (the `charts/intraday`, `optionchain`,
`spot_1m_rest`, `option_chain_1m` strings already activate this file).
(2026-07-16, operator directive events 38df2073 + 157f7cd0) SUPERSEDED wording: "on the existing feed connection (no new connection)" predates the 2026-07-15 Groww live-feed retirement. There is no live Groww feed connection anymore, so the authorized order/position/trade PUSH channel opens ONE NEW dedicated NATS-over-WS connection (`wss://socket-api.groww.in`, order/position/trade events only). Config `order_push_enabled`, module `oms/groww/push/`, codes GROWW-PUSH-01..04.

(2026-07-16) RE-KEPT as live-feed-AUTH class for the order-push channel — `POST api.groww.in/v1/api/apex/v1/socket/token/create/` mints the per-session NATS user JWT the order channel needs (operator: "use the socket-token the Groww channel needs"). The ACCESS-token minter lock (groww-shared-token-minter-2026-07-02.md) is UNTOUCHED: the access token stays SSM read-only, never minted by TickVault.

### §10 addendum — 2026-07-21: order-push PAPER ACTIVATION prep (DRAFT PR; operator-go pending)

Prep lane (coordinator-routed, 2026-07-21): the two order-push gates flip to `true` in
`config/base.toml` (`[groww_orders] order_push_enabled` + `[dhan_order_push] enabled`),
activating the §10 KEEP-GATED order/position/trade PUSH channels in receive-only PAPER
mode (operator 2026-07-16, events 38df2073 + 157f7cd0). The Groww per-session
socket-token mint stays the sanctioned live-feed-AUTH class; the ACCESS-token minter
lock is untouched (SSM read-only, never minted). Order MUTATIONS stay hard-locked
(Gates 1–4; `GROWW_ORDER_LIVE_FIRE` false); the read-only GET areas stay OFF — this PR
flips nothing else. The operator's go for the merge lands here verbatim:
<OPERATOR-GO-HERE>

---

# §12 — SOCKETS-ONLY: every remaining market-data REST pull is REMOVED (operator directive 2026-09-16)

> **Authority:** this section SUPERSEDES §8 (the Dhan per-minute spot-1m +
> option-chain scheduled-pull KEEP class), §11 (the 2026-07-14 re-assertion that
> those two legs are ENABLED classes), and the market-data half of §3's KEEP
> rows. It also reverses the `websocket-connection-scope-lock.md` 2026-08-11
> SECOND-quote REJECT row *"Stands down, disables, or starves ANY per-minute
> REST leg for Dhan or Groww in the name of the live lane"* — that row was
> written when the Dhan live WebSocket had been retired and REST was the only
> market data there was. It is not that any more.
>
> **Recorded BEFORE any code change, per the rule-file-first law.**

## §12.0 The verbatim operator demand (preserve EXACTLY, typos included)

> "Dude except sockets remove all the entire remaining rest api call related implementations ddue okay?"

## §12.1 What "except sockets" means, decided against this file's OWN §2

§2 of this file already answers the boundary question, and it was written for
exactly this trap. It records that a LITERAL "kill ALL REST" is
**self-contradictory**, in its own words: it *"would also kill the live-feed
AUTH and the static instrument-master CSVs — and **without those the live feed
cannot connect or map a single tick**, so the literal lock destroys the very
feed it wants to keep."*

That is not an interpretation invented to soften the directive. It is the
standing definition in the file the directive edits, and it is **VERIFIED in
source today, twice**:

| Verified | Evidence |
|---|---|
| A socket CANNOT dial without AUTH REST | `websocket/connection.rs:1326` calls `build_feed_url(endpoint, base_url, &token, &client_id)`; the URL is `wss://api-feed.dhan.co/?version=2&token=<JWT>&clientId=<ID>&authType=2`. `dhan_feed_stack.rs::current_feed_token` reads the `TokenManager` handle, whose only two writers are `POST auth.dhan.co/app/generateAccessToken` and `GET /v2/RenewToken`. Its own docblock: *"there is no second credential path."* |
| A socket CANNOT name a contract | `ParsedTick` (`tick_types.rs:15-29`) carries `security_id` + `exchange_segment_code` + prices. **No symbol, strike, expiry, leg or lot size.** A socket can only echo ids you already subscribed. Contract identity must come from outside the socket. |

So the sockets keep exactly the two classes §2 already names as their structural
prerequisites, and **everything else that fetches market data over HTTP is
removed.**

## §12.2 The disposition table (every row Verified in source 2026-09-16)

| Class | Endpoints | Verdict | Why |
|---|---|---|---|
| **AUTH** | `POST auth.dhan.co/app/generateAccessToken`, `GET /v2/RenewToken`, the `dhan-token-minter` Lambda | **KEEP** | produces the JWT the socket URL embeds — §2 class 1; removing it dials nothing |
| **INSTRUMENT IDENTITY** | `GET images.dhan.co/api-data/api-scrip-master-detailed.csv`, `GET niftyindices.com/IndexConstituent/<slug>.csv` | **KEEP** | produces the `security_id` map the sockets subscribe with — §2 class 2; static daily reference files, not prices. §1 of this file bans *"REST prices, OHLCV, quotes, option-chain"* and these are none of those |
| **MARKET DATA** | `POST /v2/charts/intraday` (spot-1m), `POST /v2/optionchain`, `POST /v2/optionchain/expirylist` | **REMOVE** | this is the class the directive names. The 16 sockets carry the same instruments live |
| **VERIFICATION** | `POST /v2/charts/intraday` in `dhan_live_crossverify.rs` (15:41 compare) | **REMOVE — see §12.4, this is the one that costs something** | |
| **DEAD** | `ip_verifier.rs` (5 fns), `ip_monitor.rs` (2 fns), `DHAN_CHARTS_HISTORICAL_PATH`, `DHAN_NSE_HOLIDAY_CROSS_CHECK_URL` | **REMOVE** | **zero production call sites**, Verified by workspace scan. Free |
| **NOT broker REST** | QuestDB `/exec` + ILP (our own database over HTTP), Telegram `sendMessage`, AWS SDK (SSM/S3/SNS/CloudWatch/EC2) | **KEEP** | none of these is a market-data REST call to a broker |
| **ORDER SIDE** | 51 `api.dhan.co/v2/*` URLs in `oms/api_client.rs`, `/v2/positions`, margin gate | **UNTOUCHED by this directive** | the directive says *market-data*-adjacent "rest api call related implementations"; the order surface is separately locked (`dry_run: true` hardcoded, §39 four-gate lattice) and removing it needs its own dated quote |

## §12.3 The depth question, and the stale prose that would have answered it wrongly

The obvious objection is that `websocket-connection-scope-lock.md:625` (the
2026-08-11 SECOND quote) calls the option-chain REST pull *"the sanctioned depth
instrument source"*, so deleting it should blind the 10 depth sockets.

**That prose is STALE, and the code has not depended on it since the same day.**
The THIRD quote of 2026-08-11, 50 lines below in the same file, REVERSED the
instrument-master ban that forced the chain to be the source (*"Q3 IS REVERSED:
the daily Dhan master CSV … is ORDERED BACK"*). The code followed the reversal;
the prose at `:625` was never annotated.

Verified in source 2026-09-16:

- `dhan_depth_universe.rs:1097` — `load_depth_candidates`, the per-minute
  steering path, opens with `read_contract_artifact(date_ist)`. On failure it
  returns `Vec::new()` — **it does not query the chain.**
- `dhan_feed_stack.rs:10151` — boot attach calls `load_depth_universe_from_master`
  FIRST; the `option_chain_1m` SQL is only the `None` fallback at `:10161`.
- `depth_seed.rs:28` validates every seeded id against **today's contract
  artifact**, never the chain.
- The master is strictly RICHER: `ContractRow` carries security_id, segment,
  class, expiry, strike (paise), CE/PE, underlying and **lot_size** for
  **121,674** contracts, against the chain's ~1,250 INDEX-option legs
  (`contract_underlying_map.rs:20`, measured) — which cannot represent a stock
  option at all.

So depth loses its boot FALLBACK, not its source. **A doc line at `:625` is
being annotated in lockstep with this section**, because a future session
trusting it would conclude that deleting the chain kills depth — the
false-finding class this repository records for `day_ohlc_tracker` (2026-08-12).

## §12.4 What is LOST — stated plainly, not buried (Rule 11)

1. **The 15:41 cross-verification dies, and it is the ONLY ground truth the
   revived Dhan feed has.** It compares our captured candles against Dhan's own
   official minute tape. Every other signal in the lane reports whether the
   machinery RAN; this is the only one that reports whether the numbers are
   RIGHT. After this, "the feed works" rests on internal consistency alone.
   Removing it also retires the two alarms
   `errcode-ws-gap-03-xverify-{vacuous,failed}` (§2.3f of the noise lock) and
   the `-diverged` alarm (§2.3k), since a filter whose emit site is gone is a
   permanently-green dead monitor.
2. **`spot_1m_rest` and `option_chain_1m` stop receiving rows.** The TABLES are
   retained (data already captured stays; they are not SEBI tables but deleting
   history buys nothing). Any consumer reading them for a *current* value gets
   the last row before this change — so every read site must be removed with the
   writer, not left pointing at a frozen table.
3. **The spot-price QuestDB backstop keeps working and is NOT this class.**
   `dhan_contract_universe.rs:1531` queries `FROM ticks` — our own database,
   our own socket-captured rows. It is not a broker REST pull.

## §12.5 ⚠ A FALSE-OK this directive exposes, and it is worth more than the removal

`config/base.toml` reads `[spot_1m_rest] enabled = false` (:697) and
`[option_chain_1m] enabled = false` (:750). **Both legs fire anyway.**
`[cadence] enabled = true` (:1029), and `dhan_cadence_executor.rs:341-343`
builds `intraday_url` / `chain_url` / `expirylist_url` and calls the same
`*_fetch_once_unpaced` functions. The legacy legs were stood down 2026-07-17
under RS3 mutual exclusion and **the HTTP moved, it did not stop.**

So anyone reading base.toml today concludes these two REST pulls are off. They
are not. That is the false-OK class rule 11 forbids, and it means the removal
must land in the **cadence executor**, not in a config flag — flipping
`[cadence] enabled` would stand down every other cadence lane with it.

## §12.6 What a PR that violates §12 looks like (REJECT)

- Removes the AUTH REST calls or the token minter — every socket goes dark
  (Verified at `connection.rs:1326`).
- Removes the daily master CSV or the niftyindices constituent fetch — the live
  universe collapses to the 4 hardcoded `SPOT_1M_REST_INDICES` and BOTH depth
  pools open zero sockets. That is the 99.98% collapse
  `tv-<env>-errcode-ws-gap-03-universe-collapse` exists to page on.
- Removes a leg's HTTP while leaving its ALARM, its EMF metric name, or its
  metric filter in place — a filter whose emit site is gone reads permanently
  green (the `ws-reinject-01` / `tick-conserve-01` precedent, retired twice).
- Removes a WRITER and leaves a READER pointed at the now-frozen table.
- Stands the legs down by flipping `[cadence] enabled = false` — that kills
  every other cadence lane as collateral (§12.5).
- Deletes `spot_1m_rest` / `option_chain_1m` / `rest_fetch_audit` TABLE ROWS —
  the writer is authorized for removal, the history is not.
- Touches the order-side REST surface, `dry_run`, or the §28 frozen
  indicator/strategy area under cover of this directive.
- Claims the feed is verified after the cross-verification is gone.

## §12.7 Trigger

Covered by the §6 trigger list. Reinforced on any session editing
`dhan_cadence_executor.rs`, `spot_1m_rest_boot.rs`, `option_chain_1m_boot.rs`,
`dhan_live_crossverify.rs`, `ip_verifier.rs`, `ip_monitor.rs`, or any file
containing `charts/intraday`, `optionchain`, `expirylist`, or `SOCKETS_ONLY`.

---

## ⚠ §12.8 — CORRECTED 2026-09-16 (same day, hours later): §12 names FOUR things wrongly, and two of them would break production

**No new authorization is claimed.** §12 above is the operator's directive and
its disposition stands unchanged. What follows are FACTUAL corrections to the
manifest §12 gives, found by three parallel mapping agents commissioned to
produce the deletion manifest BEFORE any code moved. Each is verified in source
at the cited file:line. §12 is left standing per house convention; where it and
§12.8 conflict, §12.8 wins.

### (a) The table names in §12 are the LEGACY names. The wire names are different.

| §12 / module name | REAL QuestDB table | Constant |
|---|---|---|
| `spot_1m_rest` | **`rest_spot_1m`** | `spot_1m_rest_persistence.rs:40` |
| `option_chain_1m` | **`rest_option_chain_1m`** | `option_chain_1m_persistence.rs:61` |
| `option_contract_1m_rest` | **`rest_option_contract_1m`** | `option_contract_1m_rest_persistence.rs:60` |
| `rest_fetch_audit` | `rest_fetch_audit` (unchanged) | `rest_fetch_audit_persistence.rs:51` |

Both persistence modules carry BOTH constants — e.g.
`LEGACY_SPOT_1M_REST_TABLE = "spot_1m_rest"` at `:44` — so the names §12 uses
exist in source and grep cleanly. **That is exactly what makes this dangerous.**

§12.6's REJECT row reads *"Removes a WRITER and leaves a READER pointed at the
now-frozen table."* A removal PR that greps the §12 names finds **zero readers**,
passes its own review against this file, and leaves every reader in place. The
checkable rule was written against names the readers do not use.

**This is the same class as the 2026-09-12 `top_volume` rename** recorded in
`CLAUDE.md`'s storage map: a stale name inside a *runnable* check costs more than
a stale name in prose, because the check reports success. Here the check is a
REJECT row rather than a SQL statement, and it fails the same way.

### (b) ⚠ THE TRAP §12 DOES NOT NAME — a permanently-RED alarm

`tv_rest_1m_fire_heartbeat` has exactly three producers, and **all three are
inside the removal set**: `dhan_cadence_executor.rs:605`,
`spot_1m_rest_boot.rs:2370`, `spot_1m_rest_boot.rs:3146`.

It is the SOLE metric behind `tv-<env>-market-hours-liveness-missing`
(`market-hours-liveness-alarm.tf:186`), and that alarm is
**`treat_missing_data = "breaching"`** (`:193`) — the file's own header at `:75`
states it verbatim: *"MISSING data PAGES during the gated window."*

**Deleting the producers without retiring or re-pointing that alarm pages the
operator every gated market window, every trading day, forever.** That is worse
than the dead-monitor class this file records repeatedly: a permanently-GREEN
monitor is ignored, but a permanently-RED one gets MUTED — and muting it
silently disables the app-liveness signal, which is a real one.

The alarm's own header already records one `metric_name` swap performed for this
reason (`:170-171`). The socket-era candidate is `tv_dhan_feed_last_tick_age_secs`,
already EMF-selected. **Re-pointing it is a DECISION, not a deletion**, and it is
NOT authorized by §12 — it needs its own dated line here.

**BINDING, added to the §12.6 REJECT list:** a PR that deletes any producer of
`tv_rest_1m_fire_heartbeat` without retiring or re-pointing that alarm — and
removing its gate membership from the `ALARM_NAMES` join in `market-hours-liveness-alarm.tf`
— in the SAME change is a REJECT.

### (c) `SPOT_1M_REST_INDICES` is NOT a REST constant. It is the live-universe fallback.

`constants.rs:1807`. Its name says REST; its load-bearing consumer is
`dhan_live_universe.rs:212`, where the four ids are the fallback the live
universe collapses to when the master artifact is unreadable.

**Deleting it dials zero sockets** — the 99.98% collapse that
`tv-<env>-errcode-ws-gap-03-universe-collapse` exists to page on. §12.6 already
bans removing the master CSV for that reason; this constant is the same hazard
wearing a REST name. **KEEP.**

### (d) The scope is larger than §12 implies: the cadence scheduler becomes DEAD

`CadenceExecutor` (`crates/core/src/cadence/executor.rs`) declares exactly three
methods — `fetch_chain`, `fetch_spot`, `fetch_expiry_list` — and **all three ARE
the removed legs**. The Groww executor was deleted 2026-08-21. So after this
removal `spawn_cadence_scheduler` (`cadence_boot.rs:72`) has no executor to
build, and `crates/core/src/cadence/{runner,schedule,gate,ladder,decision,assembly,expiry}.rs`
plus `main.rs:2410` are unreachable.

That is a further ~15,600 lines of `src/` across ~46 files, not the narrow leg
removal §12.2's table suggests. Recorded so the PR is scoped honestly rather
than discovered mid-review.

### (e) §12.4's loss list omits a third table, and understates the finality

§12.4 names the 15:41 cross-verification. Its module also owns
**`dhan_rest_1m_tape`** (`dhan_live_crossverify_persistence.rs:76`) — the raw
vendor tape — alongside the two audit tables. All three are in the Lambda's SEBI
keep-list (`operator_control_action_commands.rs:88,163`), so the wipe command
protects them; deleting the writer leaves them frozen, and DROPPING them is a
separate decision this file does not authorize.

**And the finality is worse than §12.4 states.** An exhaustive search for any
other comparator found none: `tf_consistency_boot` recomputes our own timeframes
from our own candles; `rest_candle_fold` is a writer and is disabled;
`volume_semantics_probe` is QuestDB-only and has zero callers; and
`brutex_crossverify` / `spot_crossverify` / `cross_verify_1m` /
`groww_cross_verify_1m` were all deleted in 2026-07-15 and 2026-08-21.

**After this removal there is ZERO mechanism anywhere in this workspace that
compares captured market data against any external record.** The India feed
carries no sequence number and no snapshot-on-subscribe, so nothing else can
answer *"are the numbers right"* — only *"did the machinery run"*.
`websocket-connection-scope-lock.md` states that a non-zero `compared` is *"the
ONLY evidence this repository can offer that the feed works"*. That evidence
source ends here.

### (f) Retention: the retained tables are NOT exempt

`RETENTION_EXEMPT_TABLES` lists the LEGACY names — DELIBERATELY, and its own doc
comment says why (the rename consumed them, and the coverage guard demands a decision about the surviving constant). That is NOT a defect and must not be "fixed". What it means is narrower: the LIVE names are in `DAY_PARTITIONED_TABLES` and ARE swept:

| Table | Class | Window |
|---|---|---|
| `rest_spot_1m` | Standard | 90 days |
| `rest_fetch_audit` | Standard | 90 days |
| `rest_option_chain_1m` | **MarketData** | **15 days** |
| `rest_option_contract_1m` | **MarketData** | **15 days** |

So §12's "the TABLES are retained" is true of the rows and NOT of their local
availability: retained chain history leaves EBS after 15 days. It is archived to
S3 fail-closed, never destroyed — but it stops being locally queryable.
Whether to move the three live names into the exempt list is an **operator
decision**, not a consequence of §12.

### (g) The DDL callers all live inside the deleted modules

`ensure_spot_1m_rest_table`, `ensure_option_chain_1m_table` and
`ensure_rest_fetch_audit_table` are called only from `cadence_boot.rs:158,171,177`
and the two boot modules. If the tables are retained, **a DDL caller must be
re-homed into a surviving boot step** — otherwise a future wipe leaves the
retained readers erroring on "table does not exist" instead of returning empty.

### What §12.8 does NOT change

The operator's directive; the KEEP classes (auth REST, instrument identity REST);
the REMOVE classes; the order-side carve-out; the SEBI never-delete rule; and
§12.4's honest loss, which is confirmed and widened rather than softened.

### The reusable half

Three of these seven — the table names, the heartbeat alarm, and
`SPOT_1M_REST_INDICES` — share one shape: **a name that describes what something
was called, not what it does now.** §12 was written from the module names and the
config section headers, both of which are accurate about the code's HISTORY and
wrong about its wiring. The rule-file-first law got the ordering right; what it
cannot do on its own is verify that the names in the rule are the names in the
running system. Mapping the manifest BEFORE the code is what caught it, and that
step is now part of the removal, not optional preparation for it.

---

## ⚠ §12.9 — CORRECTED 2026-09-16 (same day, hours later): §12.8 has two FALSE claims and one WRONG decision, and the removal has a CRITICAL blocker §12 never named

**No new authorization is claimed.** §12.0 is the operator's directive and its
disposition stands. What follows corrects §12.8 — which was itself written to
correct §12 — and records the one finding that changes the removal from a
deletion into a redesign. Every item below is verified in source at the cited
symbol. §12.8 is left standing per house convention; where it and §12.9
conflict, §12.9 wins.

### (a) ⚠ §12.8(a) states the reader-discovery hazard BACKWARDS

§12.8(a) says a removal PR that greps the **legacy** table names *"finds zero
readers, passes its own review against this file, and leaves every reader in
place."*

**That is the inverse of the truth, and following it would do the damage it
warns about.** The MODULE name and the CONSTANT name both carry the legacy
string, so every live reader's import line contains it:

```
crates/app/src/rest_candle_fold.rs       use ...::spot_1m_rest_persistence::SPOT_1M_REST_TABLE;
crates/app/src/tf_consistency_boot.rs    use ...::spot_1m_rest_persistence::SPOT_1M_REST_TABLE;
crates/app/src/feed_scoreboard_boot.rs   use ...::spot_1m_rest_persistence::SPOT_1M_REST_TABLE;
crates/app/src/market_ram_store_boot.rs  use ...::option_chain_1m_persistence::OPTION_CHAIN_1M_TABLE;
crates/app/src/dhan_depth_universe.rs    use ...::option_chain_1m_persistence::OPTION_CHAIN_1M_TABLE;
crates/storage/src/partition_archive.rs      crate::option_chain_1m_persistence::OPTION_CHAIN_1M_TABLE,
```

MEASURED: **29 files** outside the three persistence modules carry the legacy
string. Readers build their SQL through the CONSTANT
(`FROM {SPOT_1M_REST_TABLE}`), never through the wire literal — so grepping the
**NEW** names (`rest_spot_1m`, `rest_option_chain_1m`) is what returns almost
nothing but test assertions and one operator-console string.

**So the dangerous grep is the NEW wire name, and §12.8(a) told the next
session to trust exactly that one.** The REJECT row it added is still right in
substance (do not leave a reader pointed at a frozen table); its stated method
was wrong and is replaced: **find readers by the MODULE/CONSTANT path, never by
the table literal.**

### (b) ⚠ §12.8(c) cited a COMMENT; the conclusion was right for a weaker reason

§12.8(c) keeps `SPOT_1M_REST_INDICES` on the grounds that
`dhan_live_universe.rs:212` is the live-universe fallback. **Line 212 is a
comment.** That file never imports the constant.

The real mechanism is two files away and is stronger than the one cited:
`dhan_feed_stack.rs::hardcoded_index_universe()` maps the constant into
`Vec<SubscribeInstrument>`, `main.rs` passes it as `index_universe`, and
`dhan_live_universe::select_live_universe` returns it when the master artifact
is `None`.

**And the constant cannot be deleted at all without failing the build**, which
is the decisive reason §12.8(c) missed: `crates/common/src/constants.rs` carries
two `const _: () = assert!(...)` pins on it —
`SPOT_1M_REST_LADDER_JITTER_SLOTS == SPOT_1M_REST_INDICES.len()`, and
`CHAIN_1M_UNDERLYINGS` as its SID prefix. **KEEP stands; the evidence is
replaced.**

### (c) ⚠ DECISION CHANGED — RE-POINT the liveness alarm, do NOT retire it

§12.8(b) offers "retire or re-point". A session note then chose RETIRE on the
grounds that `dhan-no-ticks-flowing` already covers it and nothing is lost.
**Measured, that is false — retiring doubles the detection latency on the one
failure class it exists for:**

| alarm | metric | eval x period | time to page |
|---|---|---|---|
| `market-hours-liveness-missing` | `tv_rest_1m_fire_heartbeat` | 5 x **60 s** | **~5 min** |
| `dhan-no-ticks-flowing` | `tv_dhan_feed_last_tick_age_secs` | 2 x 300 s | ~10 min |
| `dhan-live-lane-down` | `tv_dhan_feed_stack_up` | 2 x 300 s | ~10 min |
| `app-log-ingestion-silent` | `IncomingLogEvents` | 3 x 300 s | ~15 min |

The liveness alarm is **the only 60-second-period alarm in the entire gated
set**. Both are `treat_missing_data = "breaching"` and both sit in the same
`ALARM_NAMES` gate, so coverage is equivalent and only latency differs.

**The re-point costs nothing and keeps the 5 minutes.**
`tv_dhan_feed_last_tick_age_secs` is published on the 30-second silence timer
(`SILENCE_SCAN_INTERVAL_SECS = 30`), unconditionally and before the
market-hours gate, and it is already EMF-selected. So the change is a
`metric_name` swap with `period = 60` and `evaluation_periods = 5` UNCHANGED —
the same shape this alarm's own header records performing twice before
(2026-07-13 SLO score, 2026-07-15 Groww bridge).

The two alarms then become complementary rather than duplicative: the
re-pointed one is a 5-minute **missing-data** detector (the exporter or the
process stopped publishing at all), and `dhan-no-ticks-flowing` stays the
10-minute **stale-data** detector (the gauge is climbing).

### (d) 🆘 THE BLOCKER §12 NEVER NAMED: the 15:41 cross-verification is a BLOCKING BOOT FLOOR for all sixteen sockets

This is the finding that changes the shape of the work, and it is verified in
source at `dhan_feed_stack.rs`, in the block whose own comment reads
*"the verification floor"*:

```rust
let Some(crossverify) = spawn_daily_crossverify(&params.main_feed_instruments) else {
    error!( ... "REFUSING to open any socket ..." );
    report_unfolded_wal_frames(&params.wal_replay_live_feed, "crossverify_deps_missing");
    return;
};
```

`spawn_daily_crossverify` returns `None` unless `install_crossverify_deps` ran
at boot, and that call site constructs `DhanLiveCrossverifyConfig` — a type in
the module the removal deletes. **So the implementer is forced to touch it, and
the obvious edit makes `run_dhan_feed_stack` return before any dial.**

Both gates are OPEN in production today (`dhan_enabled = true` in base and
production config, `TICKVAULT_DHAN_LIVE_FEED=1` in the systemd unit), so this
is a live path. The consequence of getting it wrong is **zero ticks, zero
depth, all sixteen sockets dark for the session** — the precise opposite of the
standing no-tick-loss mandate.

The code comment records WHY the refusal is real: until 2026-08-11 this block
called the comparator and discarded the result one line below a comment
asserting the check was blocking. Both halves were false, and the lane opened
all sixteen sockets while capturing data it had no way to verify.

**Therefore removing the cross-verifier is a REDESIGN of the live lane's boot
contract, not a deletion, and the first question is what replaces the
verification floor — not which files to delete.** §12.8(e) already records that
after this removal **nothing anywhere compares captured data against any
external record**; this section adds that the lane is currently built to refuse
to run in that state.

### (e) ⚠ The paper order book loses its only mark producer, silently

`mark_forward(` has exactly **two** production call sites, both in
`dhan_cadence_executor.rs` (the spot close and the chain leg price). Everything
else is `benches/` or `tests/`.

The silent half is the sharp part. `order_runtime.rs` has three arms for a
closed mark channel, including a producer-less-boot `warn!` — **all three
require the channel to CLOSE.** The sender is *moved* out of `main`'s frame
into `spawn_cadence_scheduler`; delete that call and the binding stays alive in
`main` for the process lifetime, so the channel never closes, the receiver
blocks forever, and there is **no log line, no counter and no page**. Setting
the forwarder to `None` reaches the same place by a different route (the REST
stack leaks a dummy sender on purpose so the arm idles).

With `[order_runtime] enabled = true` and `paper_fill = true`, the paper book
and the daily-loss halt run permanently unmarked and report healthy. That is
the false-OK class rule 11 forbids, and no existing gate catches it.

### What §12.9 does NOT change

The operator's directive; the KEEP classes (auth REST, instrument identity
REST); the REMOVE classes; the order-side carve-out; the SEBI never-delete
rule; and §12.4's honest loss, which is confirmed and widened rather than
softened.

### The reusable half

§12.8's own closing paragraph says three of its seven findings *"share one
shape: a name that describes what something was called, not what it does now"*
— and then two of its own items repeated that shape one level up. §12.8(a)
reasoned about grep behaviour from the table names without running the grep,
and §12.8(c) cited a line number without checking whether the line was code.
**Mapping the manifest before the code was the right instinct; the step it was
missing is that a claim about what a COMMAND returns has to be made by running
the command, and a claim about a LINE has to be made by reading the line.**

## What a PR that violates §12.9 looks like (REJECT)

- Finds readers by grepping the wire table names (`rest_spot_1m`,
  `rest_option_chain_1m`, `rest_option_contract_1m`) — that grep is near-empty
  by construction and will miss every live query builder.
- Deletes `SPOT_1M_REST_INDICES` (two `const` asserts and ~14 non-REST
  consumers; the build fails).
- RETIRES `market-hours-liveness-missing` instead of re-pointing it, or
  re-points it while changing `period` or `evaluation_periods` (that is the
  5-minute detection this section exists to keep).
- Removes the cross-verifier without deciding what replaces the verification
  floor — a naive deletion makes the live lane refuse to open any socket.
- Deletes the cadence scheduler without giving `mark_forward` a home, or
  relies on the producer-less `warn!` to report it (that arm cannot fire on
  this path).

---

# §12.10 — SCOPE NARROWED 2026-09-16: two items only, and the verification floor is REMOVED rather than replaced

> **Authority:** this section NARROWS §12.0's directive and RESOLVES the blocker
> §12.9(d) raised. It is recorded BEFORE any code change, per the
> rule-file-first law. §12, §12.8 and §12.9 stand as the manifest and its
> corrections; where their SCOPE conflicts with this section, this section wins.

## §12.10.0 The verbatim operator demand (preserve EXACTLY, typos included)

> "Bro just remove per minute price falls and 3.41 pm accuracy check alone dude okay"

Given in DIRECT response to a report that laid out §12's disposition as three
REMOVE classes — **MARKET DATA** (the per-minute `charts/intraday` spot pull,
`optionchain`, `expirylist`), **VERIFICATION** (the 15:41 cross-check), and
**DEAD** (`ip_verifier`, `ip_monitor`, two unreferenced constants) — alongside
the KEEP classes and the §12.9(d) blocker. The operator selected two of the
three and wrote **"alone"**, which is the narrowing.

Reaffirmed in the next message, after the blocker was put to him in full:

> "So once everything is entirely fi ed and resolved then this will be merged and deployed right dude"

## §12.10.1 What this SELECTS and what it DROPS

| §12.2 class | §12.10 disposition |
|---|---|
| **MARKET DATA** — `POST /v2/charts/intraday` (spot-1m), `POST /v2/optionchain`, `POST /v2/optionchain/expirylist` | **REMOVE** — "per minute price pulls" |
| **VERIFICATION** — the 15:41 `charts/intraday` compare in `dhan_live_crossverify.rs` | **REMOVE** — "3.41 pm accuracy check" |
| **DEAD** — `ip_verifier.rs`, `ip_monitor.rs`, `DHAN_CHARTS_HISTORICAL_PATH`, `DHAN_NSE_HOLIDAY_CROSS_CHECK_URL` | **OUT OF SCOPE** — "alone". These have zero production call sites and cost nothing to keep; deleting them is free but was not asked for, and a removal PR that widens its own scope is the smuggling this file's REJECT lists exist to stop. |
| **AUTH REST** (`generateAccessToken`, `RenewToken`, the minter Lambda) | **KEEP** — unchanged (§12.1: the socket URL embeds the JWT; removing it dials nothing) |
| **INSTRUMENT IDENTITY REST** (Dhan master CSV, niftyindices constituents) | **KEEP** — unchanged (§12.1: `ParsedTick` carries no symbol, strike, expiry, leg or lot size) |
| **ORDER SIDE**, QuestDB, Telegram, AWS SDK | **UNTOUCHED** — unchanged |

## §12.10.2 ⚠ The one genuine ambiguity, decided rather than left open

"Per minute price pulls" reads two ways, and the difference is a real fork:

| Reading | What it removes | Consequence |
|---|---|---|
| NARROW — the spot leg only | `fetch_spot` | the cadence scheduler SURVIVES to keep firing the chain leg; ~15,600 lines stay |
| **WIDE — the per-minute market-data trio** (taken) | `fetch_spot` + `fetch_chain` + `fetch_expiry_list` | `CadenceExecutor` declares **exactly these three methods** (§12.8(d)), so the trait has no methods left and the scheduler becomes unreachable |

**The WIDE reading is taken**, for two reasons stated so they are checkable:
(a) the operator's selection was of the CLASS as presented, and the MARKET DATA
class named all three endpoints in the message he answered; (b) the option-chain
response is literally a per-minute price pull — it returns LTP, bid/ask, OI and
greeks per leg — so excluding it would leave "price pulls" running under a
different file name.

Narrowing back to spot-only remains available and is a smaller change, not a
larger one; it needs its own dated line here.

## §12.10.3 THE VERIFICATION FLOOR — removed, not replaced, and the cost is stated

§12.9(d) recorded the blocker: `run_dhan_feed_stack` REFUSES to open any socket
unless `spawn_daily_crossverify` returns `Some`, and the code's own comment
calls that floor BLOCKING because *"the main feed has no snapshot-on-subscribe
and no sequence number, so packet loss is invisible at the protocol level and
this comparator is the only ground truth the lane has."* §12.9 closed with
*"the first question is what replaces the verification floor — not which files
to delete."*

**The answer is that NOTHING replaces it, and the floor is removed with the
comparator it gates on.** This is not a shortcut around the blocker; it is the
only coherent reading of the instruction:

1. A refusal floor that gates on a component the operator has ordered removed
   cannot stand. Keeping it means the lane refuses to open all sixteen sockets,
   every session, forever — the precise failure §12.9(d) warned about.
2. Gating the floor on something ELSE would be inventing a boot precondition
   the operator did not ask for, on the live trading lane, in a removal PR.
   That is worse than either option, and it is outside this section's scope.

**Therefore, binding on the removal PR:**

- The `spawn_daily_crossverify` binding, its `else` arm, and the
  `report_unfolded_wal_frames(.., "crossverify_deps_missing")` call are deleted
  **together**, so `run_dhan_feed_stack` proceeds to plan and dial.
- **The OTHER refusal floors in the same function are UNTOUCHED.** The WAL
  floor in particular stays exactly as it is. Removing one floor is authorized;
  weakening the boot contract generally is not.
- Every downstream use of the `crossverify` binding is removed with it — a
  binding deleted while a later `.await` on it survives does not compile, and a
  compile error is the cheap failure here; the expensive one is a lane that
  dials nothing.

## §12.10.4 ⚠ What is LOST (Rule 11 — no false-OK)

§12.4 and §12.8(e) already state this and it is repeated here because it is the
whole cost of the section:

**After this change there is ZERO mechanism anywhere in this workspace that
compares captured market data against any external record.** An exhaustive
search found no other comparator: `tf_consistency_boot` recomputes our own
timeframes from our own candles, `rest_candle_fold` is a disabled writer,
`volume_semantics_probe` is QuestDB-only with zero callers, and every
cross-broker comparator was deleted in 2026-07-15 and 2026-08-21.

The India feed carries **no sequence number and no snapshot-on-subscribe**, so
nothing else can answer *"are the numbers right"* — only *"did the machinery
run"*. What survives answers the second question only, and must never be quoted
as answering the first:

| Survives | Answers |
|---|---|
| `dhan-no-ticks-flowing` (`tv_dhan_feed_last_tick_age_secs`) | is data arriving at all |
| the `dropped == spilled` equality on both loss families | did we keep what we received |
| `tv_aggregator_tick_refused_total` | are the vendor's timestamps inside the session |
| `tv_dhan_feed_depth_total{outcome="ghost"}` | is the vendor honouring an unsubscribe |

`websocket-connection-scope-lock.md` states that a non-zero `compared` is *"the
ONLY evidence this repository can offer that the feed works"*. **That evidence
source ends here**, and no claim that the feed is verified may be made after it.

Also lost, and smaller: `rest_spot_1m`, `rest_option_chain_1m`,
`rest_option_contract_1m`, `rest_fetch_audit` and `dhan_rest_1m_tape` stop
receiving rows. The TABLES and their history are RETAINED (§12.6 forbids
deleting rows); only the writers go.

## §12.10.5 Binding requirements carried forward (each is a REJECT if missed)

These are §12.8's and §12.9's findings, restated as obligations on the removal
PR rather than as observations:

1. **Find readers by the MODULE/CONSTANT path, never the wire table name**
   (§12.9(a)). The wire names return almost nothing; the legacy constant names
   appear in **29** files.
2. **RE-POINT `tv-<env>-market-hours-liveness-missing`, do not retire it**
   (§12.9(c)). All three producers of `tv_rest_1m_fire_heartbeat` are inside the
   removal set, and the alarm is `treat_missing_data = "breaching"` — so a
   naive deletion pages **every gated market window, every trading day,
   forever**, and a muted alarm silently disables a real liveness signal. The
   swap is `metric_name` only: `period = 60` and `evaluation_periods = 5` stay,
   because this is the only 60-second-period alarm in the gated set and its
   ~5-minute detection is what the re-point exists to keep. Its gate membership
   in the market-hours Lambda's `ALARM_NAMES` must move in lockstep.
3. **`SPOT_1M_REST_INDICES` is KEPT** (§12.9(b)). Its name says REST; it is the
   live-universe fallback via `hardcoded_index_universe()`, and two
   `const _: () = assert!(...)` pins fail the build on its deletion.
4. **`mark_forward` must not be orphaned silently** (§12.9(e)). Its only two
   production call sites are in the cadence executor, and every "no mark
   producer" arm in `order_runtime.rs` requires the channel to CLOSE — which a
   sender still owned by `main.rs`'s frame never does. A removal that leaves the
   paper book unmarked and reporting healthy is the false-OK class this file
   forbids.
5. **Retire every alarm, metric filter and EMF selector entry whose producer is
   deleted, in the SAME change.** A filter with no emit site reads permanently
   green — the `ws-reinject-01` / `tick-conserve-01` precedent, retired twice.
6. **Re-home a DDL caller for every RETAINED table**, or a future wipe leaves
   readers erroring on "table does not exist" instead of returning empty
   (§12.8(g)).
7. **Decide the retention question explicitly** (§12.8(f)): the live names sit
   in `DAY_PARTITIONED_TABLES`, and `rest_option_chain_1m` /
   `rest_option_contract_1m` are `MarketData` at a **15-day** window — so
   retained history leaves local disk after 15 days. Moving them to the exempt
   list is an operator decision, not a consequence of this section.

## §12.10.6 What a PR that violates §12.10 looks like (REJECT)

- Removes the DEAD class, the AUTH REST, the instrument-identity REST, the
  order-side surface, `dry_run`, or the §28 frozen area under cover of this
  narrowing — "alone" is the operative word.
- Keeps the crossverify boot floor while deleting the comparator (zero sockets),
  or gates that floor on a newly-invented precondition.
- Weakens or removes the WAL floor, or any other refusal floor, in the same
  change.
- Deletes a SEBI or audit table row, or DROPs any retained table.
- Retires the liveness alarm, or re-points it while changing `period` or
  `evaluation_periods`.
- Deletes `SPOT_1M_REST_INDICES`.
- Lands with `mark_forward` unproduced and no counter, log or config change
  saying so.
- Claims the feed is verified, or cites a `compared` count, after this lands.

## §12.10.7 — MEASURED 2026-09-16: the manifest, verified in source before any code moved

Eight parallel mapping passes, every row re-verified by reading the cited
line. This is the evidence behind §12.10.5, and it CHANGES two of those
requirements rather than merely illustrating them.

### (a) ⚠ THE SILENT ONE — `mark_forward` is worse than §12.9(e) stated, and the fix is one line

§12.9(e) said the paper book "loses its only mark producer, silently". The
ownership chain is now proven end to end, and the silence is total:

| Step | Site |
|---|---|
| channel created (gated on `order_runtime.enabled`) | `main.rs:693-696` |
| the ONE `Sender`, wrapped — never cloned anywhere | `main.rs:704-708` |
| bound as `order_runtime_mark_forwarder` inside `async_main` | `main.rs:689` |
| **moved by value into the cadence spawn — the ONLY consumer of that binding** | `main.rs:2422` |
| receiver arm fires only when EVERY sender is dropped | `order_runtime.rs:1333` |
| the "no live mark producer" `warn!` | `order_runtime.rs:1366-1374` |

Delete the spawn at `main.rs:2410-2425` and the binding is never moved out.
It lives in `async_main`'s frame until the process ends, so the channel
**never closes**, `recv()` pends forever, and the arm at `:1366` — the one
written for exactly this condition — **cannot fire**. No warn, no error, no
counter. `unused_variables` is a warning, and `main.rs` denies only the
clippy print/unwrap/expect lints, so nothing fails the build either.

**The contrast is the tell, and it is what makes this a real trap:**
`[cadence] enabled = false` DOES close the channel, because
`cadence_boot.rs:108-111` returns `None` **before** the by-value parameter is
consumed — so the config-off path is LOUD and the code-deletion path is
SILENT. The two look identical from outside and behave oppositely.

**What actually breaks:** marks go **STALE, not zero and not an error**.
`RiskEngine::update_market_price_in_segment` (`risk/engine.rs:567`) has exactly
one production caller (`order_runtime.rs:1587`), so `market_prices` keeps its
last value forever, `total_unrealized_pnl` freezes, and
`evaluate_daily_loss_halt` (`engine.rs:639`) decides on a stale price
indefinitely. Pending `PAPER-n` orders never fill — the filler is mark-driven.
**There is NO alternative producer**: `SpotPriceStore` has no wiring into
`MarkUpdate` or the risk engine, and `update_market_price` has zero production
callers. `[order_runtime] enabled = true` and `paper_fill = true`
(`config/base.toml:860-861`), so this is live.

**BINDING — the fix is to drop the binding explicitly, not to invent a
producer.** At the point the cadence spawn is removed, `main.rs` must
explicitly drop `order_runtime_mark_forwarder` so the channel CLOSES and the
existing `order_runtime.rs:1366` warn fires. That converts a silent stall into
the loud, already-written, already-tested signal the code was designed around.
Wiring a NEW mark producer from the tick drain is real work on the order path
and is **out of this section's scope**; leaving the forwarder undropped is a
REJECT.

### (b) Depth loses its FALLBACK — not its source, and the dead branch must go with it

§12.3 is CONFIRMED for the per-minute steering path: `load_depth_candidates`
(`dhan_depth_universe.rs:1090`) opens with `read_contract_artifact` and returns
`Vec::new()` on failure — it never queries the chain.

But `load_depth_universe` (`:1244`) DOES query `rest_option_chain_1m` via
`build_depth_candidate_query` (`:813`), and it is reached at
`dhan_feed_stack.rs:10161` as the **`None` fallback** when
`load_depth_universe_from_master` (`:10151`) cannot read the master artifact.
Its own error text reads *"depth-20 and depth-200 will open ZERO sockets this
session"*.

So: on a morning when the daily master rider had a bad night, depth currently
falls back to the chain table and still dials. After removal that query bounds
on `ts >= today` against a table nothing writes, returns empty, and **depth
opens zero sockets** — the same outcome the fallback exists to prevent.

**BINDING:** the fallback branch at `dhan_feed_stack.rs:10159-10163` is deleted
WITH the leg. A fallback that can only ever return empty is a dead monitor
written in code, and leaving it means a silent "fell back, got nothing" path
where an honest "the master artifact is unreadable" error belongs.

### (c) Two RAM structures become producer-less — recorded, not fixed

| Structure | After removal |
|---|---|
| `pipeline::chain_snapshot` registry | **no producer AND no consumer.** Sole producer `option_chain_1m_boot.rs:802` (via `publish_chain_moneyness_snapshot`, called from `:2061` and `dhan_cadence_executor.rs:1089`); sole production reader `cadence/assembly.rs:353,465`. Both sides are inside the removal set. The MODULE stays — `chain_day_store` uses its types. |
| `pipeline::chain_day_store` | **installed at boot, never written, read only for gauges.** Installed `market_ram_store_boot.rs:307`; sole writer `option_chain_1m_boot.rs:800`; readers `market_ram_store_boot.rs:599` (rehydrate) and `:782` (`tv_ram_store_chain_minutes_resident`). |

This is the `spot_bar_store` shape CLAUDE.md already records — *"allocated at
boot, never written, and read only to publish residency gauges."* Both are
recorded here so the next reader does not mistake a flat gauge for a defect.
Neither is fixed in the removal PR: deciding whether to stop installing them is
a separate change.

### (d) Exactly ONE alarm dies RED. Everything else dies green.

| | |
|---|---|
| **RED** | `tv-<env>-market-hours-liveness-missing` (`market-hours-liveness-alarm.tf:173`). All **three** producers of `tv_rest_1m_fire_heartbeat` are inside the removal set (`spot_1m_rest_boot.rs:2370`, `:3146`, `dhan_cadence_executor.rs:605`), and it is `treat_missing_data = "breaching"` (`:193`) and gate-armed as the **first** entry of the market-hours Lambda's `ALARM_NAMES` (`:357`). Deleting the producers pages every gated window, every trading day, forever. §12.10.5 #2 stands: **re-point `metric_name` only**, keep `period = 60` (`:188`) and `evaluation_periods = 5` (`:182`). |
| **GREEN** | 4 metric-filter alarms (`spot1m-01-escalation` `error-code-alarms.tf:403`, `chain-02-escalation` `:416`, `chain-01` `:430`, `chain-04-warmup` `:448`), plus `ws-gap-03-xverify-vacuous` `:756` and `-failed` `:766` — whose producers are `dhan_feed_stack.rs:13916,13930,14100`, inside the crossverify path being removed. 12 EMF selector entries and 12 dashboard widgets lose their producers. |

Every one must be retired in the SAME change. A filter with no emit site reads
permanently green, which is the `ws-reinject-01` / `tick-conserve-01` class
this repository has already retired twice.

**Recorded because it inverts an assumption:** the cross-verification's own
four metrics (`tv_dhan_live_xverify_{runs,findings,query_failures,audit_rows_discarded}_total`)
and its error code `DHAN-LIVE-XVERIFY-01` are in **no** EMF selector and **no**
alarm. The only ground truth the lane has was never itself monitored.

### (e) Hand-edits in surviving files (module deletion is not enough)

| Site | What |
|---|---|
| `main.rs:2410-2425` | the cadence spawn — plus the explicit forwarder drop, per (a) |
| `main.rs:2787` / `:2809` | `install_crossverify_deps` + `DhanLiveCrossverifyConfig::default()` |
| `main.rs:3814` | `ensure_dhan_live_crossverify_tables` |
| `main.rs:4201-4206` | the boot report's `spot_1m_enabled` / `chain_1m_enabled` `\|\|` operands |
| `main.rs:4359` | `notify_cadence_shutdown()` |
| `dhan_feed_stack.rs:12290` + `:13566` | the crossverify boot floor and `spawn_daily_crossverify` |
| `dhan_feed_stack.rs:10159-10163` | the depth fallback, per (b) |
| `dhan_rest_stack.rs:1036-1081` | the legacy spot/chain/probe spawn gates |
| `config.rs:3501-3505` | the `cadence.enabled` ⟂ `spot_1m_rest.enabled \|\| option_chain_1m.enabled` mutual exclusion |

`crates/app/src/cadence_escalation.rs` imports the edge trackers from BOTH boot
files and belongs to the removal set. `dhan_live_crossverify.rs:1814` calls
`spot_1m_rest_boot::fetched_at_ist_nanos_now` — both sides are being removed, so
nothing needs re-homing there.

### (f) Retention is a LOCKSTEP requirement, not a follow-up

`crates/storage/tests/partition_retention_coverage_guard.rs` extracts every
`const …_TABLE…: &str` in `crates/storage/src` and asserts each appears in
`partition_manager.rs`. So deleting a table constant **requires** deleting its
literal from the retention lists in the same change, and vice versa — the guard
fails either way round. `partition_manager.rs:932-935` separately enforces that
no name is in both an exempt list and a partitioned list.

The §12.10.5 #7 decision, with the measured windows: `rest_spot_1m` (DAY,
`Standard`, **90d**), `rest_fetch_audit` (DAY, `Standard`, 90d),
`dhan_rest_1m_tape` (DAY, `Standard`, 90d), `rest_option_chain_1m` (DAY,
`MarketData`, **15d**), `rest_option_contract_1m` (DAY, `MarketData`, 15d).
Retained chain history therefore leaves local disk after 15 days. Moving them to
`RETENTION_EXEMPT_TABLES` is an operator decision and is NOT taken here.

`ensure_option_contract_1m_rest_table` already has **zero** production callers
(`option_contract_1m_rest_persistence.rs:192`) — that table is orphaned today,
before this change.

### (g) One reader is left pointed at a frozen table

`[rest_candle_fold]` is `enabled = false` (`config/base.toml:929`) and its
`main.rs` read sites (`:2944, :3901, :3928, :3934, :3939`) SURVIVE — but what it
folds is `rest_spot_1m` bars. After removal it is a disabled reader of a table
nothing writes. §12.6's REJECT row ("Removes a WRITER and leaves a READER
pointed at the now-frozen table") applies: it must be removed or explicitly
recorded as inert in the same change.

`[cross_verify]` in `config/base.toml:649-680` has **no Rust consumer at all**
(stated at `:645-646`) — it is already dead config and is not load-bearing
either way.

### (h) Honest scale

`dhan_cadence_executor.rs` 1,626 + `spot_1m_rest_boot.rs` 5,260 +
`option_chain_1m_boot.rs` 4,242 + `crates/core/src/cadence/` 8,558 ≈ **19,700
lines of `src/`**, before the storage modules, the crossverify pair, the guards
and the terraform. `CadenceExecutor` declares exactly three methods
(`cadence/executor.rs:291,298,309`) and all three are the removed legs, which is
why the scheduler becomes unreachable rather than merely quieter.

### (i) What did NOT change

The operator's directive; the KEEP classes; the DEAD class staying out of scope;
the SEBI never-delete rule; §12.10.3's decision that the verification floor is
removed rather than replaced; and §12.10.4's loss statement, which this pass
confirms and widens — the comparator was the only external ground truth AND it
was itself unmonitored.

## §12.10.8 — MEASURED 2026-09-16 (same pass): three findings that CHANGE §12.10.5, not merely illustrate it

### (a) §12.10.5 #1 named ONE surviving reader. There are FIVE.

Found by the module/constant path, exactly as §12.9(a) prescribes. Every one
survives the removal and every one then reads a table nothing writes:

| Reader | Site | Behaviour on the frozen table |
|---|---|---|
| `market_ram_store_boot.rs` (chain rehydrate) | `:58` import, `:435` `FROM {OPTION_CHAIN_1M_TABLE}` | **silent** — parses to an empty vec, records 0 rehydrated minutes, indistinguishable from "market closed" |
| `rest_candle_fold.rs` | `:114` import, `:1246`, `:1260` | **silent** — no SIDs discovered, no catch-up fold, not counted a failure |
| `tf_consistency_boot.rs` | `:90` import, `:858` | **downgrades an alarm** — with zero spot rows the verdict becomes `NoData` (Info) instead of `Blind` |
| `feed_scoreboard_boot.rs` | `:69` import, `:2444`; plus a LITERAL `rest_fetch_audit` read at `:2431` | **silent** — percentiles emit the `-1` no-sample sentinel, digest renders "no data" |
| `dhan_depth_universe.rs` | `:46` import, `:828` | **loud** — `record_depth_failure("empty_selection")` + *"depth-20 and depth-200 will open ZERO sockets this session"* (this is the (b) fallback) |

**Four of the five fail SILENTLY.** None crashes; none assumes rows exist. The
risk is zero-coverage that reads exactly like a quiet market — which is the
false-OK class, arriving five times at once.

**Also outside the Rust constant path entirely**, and therefore invisible to any
constant-rename: `crates/aws-lambdas/src/operator_control_commands.rs:50-54,68,106-111`
(SSM diagnostic SQL), `operator_control_console.html:202` (the console's DEFAULT
query is `SELECT * FROM rest_spot_1m ORDER BY ts DESC LIMIT 50`), and
`operator_control_action_commands.rs:44,51,88,163` (the wipe and SEBI-protect
allowlists, which name all five wire names AND the three crossverify tables).
The console default must not be left pointing at a dead table.

### (b) The error codes do NOT split the way a file-scoped reading suggests

`error_code_paging_filter_drift_guard.rs` fails any coded terraform filter whose
code has **zero** production emit sites. So the four filters
(`error-code-alarms.tf:403, 416, 430, 448`) and the emit sites must move in
lockstep. The fate of each code, after resolving every "surviving" site against
the actual removal set:

| Code | Verdict | Why |
|---|---|---|
| `CHAIN-01`, `CHAIN-03`, `CADENCE-01/02/03/05`, `DHAN-LIVE-XVERIFY-01` | **dies entirely** | every emit site is inside the set |
| `SPOT1M-01` | **dies entirely** | its one "surviving" site is `cadence_escalation.rs:422,529` — which imports the edge trackers from both boot files and is itself in the set |
| `CHAIN-02` | **dies entirely** | same: `cadence_escalation.rs:469,558` |
| `CHAIN-04` | **verify before deleting** | its surviving site is `dhan_rest_stack.rs:1088`, inside the chain-probe block the removal also takes (`:1074-1088`) |
| `SPOT1M-02` | **SURVIVES — must NOT be deleted** | `option_contract_1m_rest_persistence.rs:208,250,318,331`, a module outside the set |

The last row is the one a file-scoped sweep gets wrong in the expensive
direction: deleting `SPOT1M-02` breaks a module that is not being removed.

Two further lockstep guards: `error_code_rule_file_crossref.rs:184` requires
every surviving variant to be mentioned in `.claude/rules/**` or
`docs/error-runbooks/**` — so deleting a variant means deleting its mentions,
and keeping one means keeping them. `runbook_cross_link_guard.rs:102` requires
every repo path cited INSIDE a runbook to resolve, so deleting a source file
breaks any runbook that names it.

### (c) THREE xverify alarms, not two — and an exact-count guard sits on them

§12.10.7(d) listed `ws-gap-03-xverify-vacuous` and `-failed`. There is a third:
**`ws-gap-03-xverify-diverged`** (`error-code-alarms.tf:791-798`, pattern
`$.source = "xverify_diverged"`). All three producers are
`dhan_feed_stack.rs:13916, 13930, 14100`, inside the removed path.

`crates/common/tests/cloudwatch_app_alarms_wiring.rs:2882,2894` asserts an
**exact count** of ws-gap-03 alarms, and
`crates/aws-lambdas/tests/alarm_phrase_coverage_guard.rs:9-10` maps alarm names
to Telegram phrases. Both fail unless the count and the phrase map move in the
same change.

### (d) Good news, verified: the boot floor orphans NOTHING

The binding from the floor is used exactly ONCE after `dhan_feed_stack.rs:12290`
— at `:12307`, `let _crossverify = crossverify;`, a hold-to-prevent-drop that is
never read again anywhere in `:12307-13427`. So §12.10.3's deletion is clean:
there is no later `.await` to strand.

The six refusal floors of `run_dhan_feed_stack`, in source order, with the one
being removed marked:

| # | Floor | Guard | `return` |
|---|---|---|---|
| 1 | plan-build failure | `:12213` | `:12228` |
| 2 | exclusivity — `rest_fold_writes_dhan_candles` | `:12257` | `:12268` |
| 3 | **verification (REMOVED)** | `:12290` | `:12303` |
| 4 | capture / WAL floor | `:12314` | `:12325` |
| 5 | token-manager / client_id | `:12363` | `:12374` |
| 6 | dual-instance lock | `:12386` | `:12397` |

Five floors remain and are UNTOUCHED, per §12.10.3. **Floor 2's message text at
`:12263-12264` names the cross-verification** and must be re-worded rather than
left asserting a check that no longer exists.

### (e) One naming correction, so the next reader is not misled

This file, the terraform and the Telegram copy call it the **15:41** check;
the code computes `RUN_SECS_OF_DAY_IST = SESSION_CLOSE_SECS_OF_DAY_IST + 60`
(`dhan_live_crossverify.rs:132`) and its own comments say **15:31**. The
operator's phrase is "3.41 pm", so 15:41 is the name used throughout §12; the
literal minute follows `TICK_PERSIST_END_SECS_OF_DAY_IST` and is not a constant
anyone should quote from memory. Recorded because a wrong minute in a runbook is
the copy-pasteable-claim class this file already records once.

### (f) A fixture that will break, and is easy to miss

`spot_1m_rest_boot.rs:4932-4953` carries a `crossverify_day_window` byte-equality
probe that reuses the cross-verification's own request shape as its fixture and
reads a `crossverify` local at `:4949`. Both sides are in the removal set, so it
goes with them — but it is the kind of cross-module fixture a per-file deletion
walks past.

---

## §12.11 — RESOLVED 2026-09-17: §12.10.5 #6's DDL re-homing is satisfied by REMOVING the readers, not by creating writer-less tables

> **This section RESOLVES a binding requirement differently from the way it is
> worded, and is recorded BEFORE the code, per the rule-file-first law.** It
> authorizes nothing new; it records a decision inside work the operator already
> authorized (§12.10.0), and it names what the alternative would have cost.

### What §12.10.5 #6 says, and the premise it rests on

> "**Re-home a DDL caller for every RETAINED table**, or a future wipe leaves
> readers erroring on 'table does not exist' instead of returning empty
> (§12.8(g))."

§12.8(g) states the premise in as many words: *"`ensure_spot_1m_rest_table`,
`ensure_option_chain_1m_table` and `ensure_rest_fetch_audit_table` are called
only from `cadence_boot.rs:158,171,177` and the two boot modules."* That
sentence assumes the ensure fns SURVIVE the removal and only lose their callers.

**They do not.** The four persistence modules were deleted whole —
`spot_1m_rest_persistence.rs`, `option_chain_1m_persistence.rs`,
`rest_fetch_audit_persistence.rs` and `dhan_live_crossverify_persistence.rs` —
because each held its writer, its DDL and its table constant in one file. A
workspace `grep` for `fn ensure_spot_1m_rest_table` now returns **zero**. So
"re-home a caller" is not an edit; it is *resurrect a CREATE TABLE for a table
nothing will ever write again*, and that is a different decision deserving a
different answer.

### The decision: satisfy the PURPOSE, which is "no reader left erroring"

The requirement's stated purpose is the `or` clause — do not leave readers
erroring. Two shapes satisfy it:

| | (a) create the empty table | **(b) remove the readers** |
|---|---|---|
| Reader gets | `0 rows` | nothing — the query is gone |
| What that reads as | *"the leg ran and captured nothing today"* | *"there is no leg"* |
| Monitors | `feed_scoreboard_boot`'s REST arm reports a measured zero **every day, forever** | the arm goes with the leg |
| New code | 3 resurrected CREATE TABLE statements + a boot caller | none |

**(b) is taken.** (a) manufactures the exact misreading this repository has
already paid for, and the precedent is dated, in-repo, and about one of these
very tables.

### The precedent that decides it — `operator_control_commands.rs`, 2026-09-03

`rest_option_contract_1m` reached this state one removal earlier (its DDL
entry point lost its last caller when the 2026-08-21 Groww removal deleted
`groww_contract_1m_boot`). The house response is recorded at that file's own
docblock and it was to **delete the queries, not create the table** — with
two measured reasons:

- **MEASURED on the box:** `SELECT count() FROM rest_option_contract_1m`
  returned `table does not exist`, and QuestDB logged **61 errors in 30
  minutes** naming it, about once a minute, for a leg with no writer.
- **The larger half, in that docblock's own words:** `curl -f` swallowed the
  400, the field arrived EMPTY, and the console rendered a blank "contracts"
  bar — *"which reads as 'the per-contract leg captured nothing today' when
  the truth is 'there is no per-contract leg'. An operator cannot tell a
  broken leg from an absent one."*

An empty table produces that second failure by construction and removes the
first, which is the worse trade: the log noise is a nuisance an operator
notices; a confidently blank chart is a false-OK an operator believes.

### What this obliges, and it is MORE work than the table would have been

Removing the readers is the larger job and every one is named here so none is
quietly skipped:

| Reader | Table | Disposition |
|---|---|---|
| `feed_scoreboard_boot.rs` — the REST-leg digest arm | `rest_fetch_audit`, `rest_spot_1m` | **REMOVE** the arm; a permanently-zero REST section in the daily scoreboard is the dead-monitor class — **DONE 2026-09-17.** The arm, both day queries, `aggregate_rest_leg_day`, the `tv_rest_leg_close_to_data_p99_ms` gauge (LOCAL `/metrics` only, so no alarm or EMF entry is owed), the `main.rs` threading, the `RestLegScoreLine` type + `render_pulls_per_leg` + the two event fields, the `rest_leg_digest_wiring_guard.rs` ratchet and 20 tests are gone; the now-orphaned `SPOT_1M_REST_TABLE` const went with them. Runbook `dual-feed-scoreboard-error-codes.md` §2b carries the dated RETIRED banner, which also corrects §12.9(a)'s "renders 'no data'" — it rendered ABSENCE (`None` → segment omitted), never a fabricated zero. **⚠ What this leaves unwatched:** the daily card no longer answers the operator's 2026-07-13 Quote 2 (*"within how many seconds precisely"*) at all, because there is no per-minute fetch left to time. |
| `dhan_depth_universe.rs` — the boot `None` fallback | `rest_option_chain_1m` | **REMOVE** (already required by §12.10.7(b) — a fallback that can only return empty is a dead monitor written in code) |
| `volume_semantics_probe.rs` | `rest_option_chain_1m` | zero production callers, before and after; recorded, not deleted here — **disposition closed 2026-09-17 in the module header: its QUESTION was settled on 2026-09-09 by the Track 2 monotonicity SELECT (9,879,724 ticks, 157 violations = 0.0016%, from `ticks` alone and needing no second source), and its oracle is now frozen. Prefer the Track 2 shape if the question ever re-opens.** |
| `rest_candle_fold.rs` | `rest_spot_1m` | `enabled = false`; §12.10.7(g) already requires it removed or explicitly recorded inert |
| `operator_control_commands.rs` — `CHAIN_TODAY`, `CHAIN_BY_FEED`, `REST_AUDIT`, `REST_LAT_HOUR`, `REST_LATENCY_SQL` | all three | **REMOVE**, exactly as the 2026-09-03 pair was removed — **DONE 2026-09-17**, and see §12.12 below: this row named FIVE commands and the file had EIGHT of the same shape, so `SPOT_TODAY` / `SPOT_BY_FEED` / `DEDUP_KEYS` were RE-POINTED at the live tables rather than left reading a frozen one. |
| `operator_control_console.html:202` default query | `rest_spot_1m` | **RE-POINT** — a console whose default query errors on open is the first thing an operator sees — **DONE 2026-09-17** (§12.12) |

### §12.12 — 2026-09-17: the operator console's Data tab is RE-POINTED at the live tables, not emptied

**No new authorization is claimed.** §12.11's disposition row above named five
of `operator_control_commands.rs`'s REST-table reads. The file carried
**eight** of the same shape, and the three it did not name —
`SPOT_TODAY`, `SPOT_BY_FEED`, `DEDUP_KEYS` — are the console's **hero
number, its per-broker split, and its schema shield**. A list that names some
reads exactly like a list that audited the file; this is that class again, in
the disposition table itself.

**Why RE-POINT and not REMOVE.** Removing them empties the operator's primary
surface: the hero counter renders `—` forever and the Data tab shows no bars.
Leaving them is worse — every one is scoped to `today()` against a table with
no writer, so each returns a permanent zero, and this file's own
`operator_control.rs` docstring already states the rule that settles it:
*"a zero an operator cannot distinguish from a real outage is worse than an
absent field."* The third option is the one §12.9(c) took for
`market-hours-liveness-missing` in this same removal series, for the same
reason, and is taken here.

| Command | Was | Now |
|---|---|---|
| `SPOT_TODAY` | `count() FROM rest_spot_1m WHERE ts IN today()` | **`TICKS_TODAY`** — `ticks` |
| `CHAIN_TODAY` | `rest_option_chain_1m` | **`DEPTH_TODAY`** — `market_depth`, the other live socket table |
| `SPOT_BY_FEED` | `rest_spot_1m` GROUP BY feed | **`TICKS_BY_FEED`** — `ticks` GROUP BY feed |
| `CHAIN_BY_FEED` | `rest_option_chain_1m` GROUP BY feed | removed (one live by-feed split is the question, not two) |
| `DEDUP_KEYS` | upsert-key count on `rest_spot_1m`, expected **4** | upsert-key count on `ticks`, expected **5** |
| `REST_AUDIT` · `REST_LAT_HOUR` · `REST_LATENCY_SQL` | `rest_fetch_audit` | **REMOVED** — see below |

**The field names MOVE with the queries** (`SPOT_TODAY` → `TICKS_TODAY`). A
re-pointed query under its old name is the stale-name trap §12.8(a) and
§12.9(a) each record: an operator reading "spot" would believe they were
looking at the REST spot leg.

**The dedup expectation moves 4 → 5**, and that is not cosmetic: `ticks` is
`DEDUP UPSERT KEYS(ts, security_id, segment, capture_seq, feed)` — five,
pinned by `questdb_init_script_guard.rs::test_init_script_ticks_ddl_is_final_
schema_with_5_key_dedup`. A re-point that kept the 4 would have shown a red
"DEDUP disabled / schema drift" shield on a perfectly healthy box, every day.

**⚠ What is NOT re-pointed, and why.** `REST_AUDIT`, `REST_LAT_HOUR` and
`REST_LATENCY_SQL` all measured **`close_to_data_ms` — how many seconds after
a minute closed its data arrived.** That question has no surviving equivalent:
there is no per-minute fetch left to time. The live lane's
`tv_dhan_feed_last_tick_age_secs` measures ARRIVAL freshness, which is a
different question, and presenting it as an answer to this one would be the
false-OK this file exists to stop. The console's pull line and latency button
are therefore REMOVED, not replaced.

**⚠ What this leaves unwatched, stated rather than implied:** the console no
longer answers the operator's 2026-07-13 Quote 2 (*"within how many seconds
precisely"*), and nothing else does.

**REJECT:** re-pointing a query while keeping its old field name; keeping the
4-column dedup expectation against `ticks`; presenting tick-arrival age as
minute-close latency; or re-adding any `rest_fetch_audit` read without its
writer coming back first.

#### ⚠ CORRECTED 2026-09-17 (same day, hours later) — it was NINE reads, not eight, and the ninth is the LATENCY tab

The section above opens by correcting §12.11 for naming five reads where the
file had eight. **It then made the same error one level down: the file has
NINE.** The ninth is `LATENCY_COMMANDS[6]` — a `curl` that aggregates
`count()`, `approx_percentile(close_to_data_ms, 0.5)` and `(…, 0.99)` from
`rest_fetch_audit` for `today()`, grouped by `(feed, leg)`, and re-emits each
row as a `RESTLAT_ROW=` line.

**How it was missed, because the mechanism is the reusable part.** The audit
that produced the eight-row table was of the `VIEW`/`FEEDS` command arrays —
the Data tab. `LATENCY_COMMANDS` is a different array serving a different tab,
so it was outside the thing being read, and its query is embedded INSIDE a
shell string rather than named by a `const` a reader would grep. The standalone
`REST_LATENCY_SQL` const WAS found and retired; the identical query inlined
twenty lines above it was not. **A grep for the const name finds the const; only
a grep for the TABLE finds both** — and the table grep is the one this file's
own §12.9(a) already prescribes ("find readers by the MODULE/CONSTANT path,
never by the table literal" — here the inverse, and both directions are needed).

The count is now MEASURED rather than asserted:
`awk '/^#\[cfg\(test\)\]/{exit}' crates/aws-lambdas/src/operator_control_commands.rs |
grep -cE 'rest_fetch_audit|rest_spot_1m|rest_option_chain_1m'` returns the live
reads; after this change it is **0**, every remaining hit being a tombstone.

**Disposition: REMOVED, not re-pointed, and for the reason §12.12 already
gives.** It measures `close_to_data_ms` — *how many seconds after a minute
closed did its data arrive* — which is the exact question the paragraph above
records as having no surviving equivalent. Re-pointing it at
`tv_dhan_feed_last_tick_age_secs` would present ARRIVAL freshness as
minute-close latency, which that same paragraph names as the false-OK this file
exists to stop. Left in place it returns an empty result for `today()` every
day, forever — a latency table that is permanently blank, on the tab an
operator opens to ask whether the box is slow.

Removed with it, as one unit: `REST_LAT_QUERY_MAX_SECS` (the 4-second budget
that existed only for that curl), `parse_rest_lat_row`, the `RESTLAT_ROW=` arm
of `parse_latency`, the `rest_latency` JSON field, and the console's REST-pull
latency table. `LATENCY_TIMEOUT_SECS` is re-derived downward in the same change
— a timeout sized for a query that no longer runs is a number that no longer
means anything.

**⚠ What this leaves unwatched, and it is narrower than it looks:** the
latency tab keeps every BOX-WIDE probe it had — QuestDB round-trip (`QDB=`),
clock skew (`SKEW=`), the dormant order-placement histogram, and the
per-socket live WebSocket delivery lag (`WSLAT_RAW=`, sixteen sockets). What
goes is the per-(feed, leg) REST pull percentile, and nothing replaces it,
consistent with the paragraph above.

**REJECT, in addition to the list above:** re-adding any `rest_fetch_audit`
read in any command array; re-pointing the latency table at a tick-age gauge;
or claiming this file has been audited for retired-table reads without running
the table-literal grep over the production region of every command array.

#### ⚠ TWO RESIDUALS the removal itself left, both found by counting rather than reading

Acting on the correction above produced two defects of its own. Both are fixed
in the same change and are recorded because each would read as deliberate to a
later session, and neither was visible by reading the diff.

**1. A dangling `#[test]`.** The retirement tombstone for
`test_failed_bucket_excludes_never_attempted_minutes` was written UNDER the
attribute rather than over it, so the attribute fell through onto the next
function and `test_errors_render_verbatim_through_esc` carried TWO `#[test]`s.
It compiled, it ran, and every suite stayed green — a duplicate attribute is
not an error. It surfaced only when the ratchet delta (−10) failed to match
the named removals (−11) and the missing one had to be hunted: the annotation
count and the `fn test_*` count disagreed by exactly one.

**The reusable half:** a retirement tombstone replaces an ITEM, and an item
starts at its first attribute, not at its `fn`. Writing the tombstone below a
surviving attribute silently re-parents it.

**2. A dead labeled-line scan.** `parse_feeds_view` built a
`HashMap<String, String>` from every non-marker line on every call. Its only
two inputs were ever `REST_AUDIT=` and `REST_LAT_HOUR=`; with those gone,
`FEEDS_VIEW_COMMANDS` emits marker-delimited JSON and nothing else, so the map
had no input — and after the classification block was retired it had no reader
either. Clippy does not flag it: a `HashMap` that is `insert`ed into is
"used". Removed, with its two rules restated at the site, because re-adding a
labeled line means re-adding the scan and both rules are easy to miss the
second time: it must skip BETWEEN the BEGIN/END markers (a `=` inside a JSON
body is not a field), and any field read that way owes the three-way
ABSENT / UNPARSEABLE / GENUINELY-EMPTY split.

The same pass rewrote the unreachable arm's comment, which still explained the
2026-09-08 incident in terms of a third field the reader can no longer find.
The lesson is kept and re-attached to the two fields that survive.

### ⚠ What is NOT claimed

- **This does not preserve the history's queryability.** The ROWS are retained
  (§12.6 forbids deleting them, and the SEBI allowlists at
  `operator_control_action_commands.rs:88,163` are untouched), and on the
  CURRENT volume the tables exist and can still be queried by hand. What goes
  is the machinery that queried them automatically. On a FRESH volume the
  tables will not exist at all — and that is the honest state, because on a
  fresh volume they would hold nothing either way.
- **`rest_option_contract_1m` is deliberately not in scope.** It has been
  orphaned since 2026-08-21, its console queries are already gone, and adding
  a DDL for it now would be a scope expansion nobody asked for.
- **Retention is unchanged.** All five live names stay in
  `DAY_PARTITIONED_TABLES` with their existing classes (§12.10.7(f)); this
  section touches no retention list, and the `partition_retention_coverage_guard`
  lockstep still binds.

### §12.13 — 2026-09-17: `rest_candle_fold` is RECORDED INERT, mechanically, and NOT removed

§12.10.7(g) left one open item: `[rest_candle_fold]` is `enabled = false` and
its `main.rs` read sites survive, but what it folds is `rest_spot_1m` bars, so
after the removal it is *"a disabled reader of a table nothing writes"* — and
§12.6's REJECT row applies: it *"must be removed or explicitly recorded as
inert in the same change."* This is that disposition.

#### Why NOT removed

The operator's narrowing named two classes and said **"alone"**. The fold is
neither: it is a downstream CONSUMER of one of them. §12.10.6's own REJECT list
bans removing anything else under cover of this quote, and removing the fold
would additionally take the live lane's **exclusivity floor** with it — the
floor that refuses to open a socket while the fold is writing the same
`candles_<tf>` rows. §12.10.3 says in as many words that the other refusal
floors are UNTOUCHED, and §12.10.6 makes weakening any of them a REJECT.

So the module (4,028 lines), its `[rest_candle_fold]` config section, its
runbook, and floor 2 are all left exactly as they are.

#### Why "recorded as inert" had to be mechanical, not prose

Both of the fold's inputs are gone:

| Input | State |
|---|---|
| The live inlet (`send_confirmed_bars`) | **ZERO production callers.** Its only producer was `spot_1m_rest_boot.rs`, calling it at the two flush-ok arms — the persist-CONFIRMED contract that let a bar fold only after its ILP ACK. That file went with the per-minute legs. |
| The boot catch-up | re-folds from `rest_spot_1m`, RETAINED but **FROZEN** — real history, zero rows for any day after 2026-09-16. |

A comment recording that would not have been enough, because the failure it
prevents is silent: flipping the gate to `true` installs the channel, spawns
the task, logs a clean **ARMED** line, and then receives nothing for the whole
session — no error, no counter. That is the producer-less-channel shape
§12.9(e) had to close for `mark_forward`, and it **cannot be closed the same
way**: the inlet is a first-wins `OnceLock` install, so its emptiness is never
observable at runtime. An inert thing that reports itself armed is recorded as
WORKING.

#### What shipped

`rest_candle_fold::LIVE_INLET_HAS_PRODUCER` (`false`), read by `main.rs`
INSIDE the existing config gate. Enabled-but-producerless now emits a coded
`FOLD-01` error with `stage = "no_producer"` naming both dead inputs and the
two ways out, and arms nothing. The disabled path is unchanged; the seal-writer
ordering is unchanged; no floor moves.

**The const is DERIVED, not asserted.** A const is only worth more than a
comment if it cannot go stale, so
`the_inert_const_matches_whether_a_producer_actually_exists` scans every
production file in `crates/app/src` except the fold module and requires the
const to be `true` exactly when one of them calls `send_confirmed_bars`.
Restore a producer and forget the const → fails. Flip the const with no
producer → fails. Bite-proven in both directions, plus a third arm requiring
`main.rs` to actually consult it (a const nothing reads is a comment with a
type).

#### ⚠ NOT claimed

- That the fold works, or could be made to work by a config flip. Re-arming
  needs a producer, a live catch-up source, and the const flipped in the same
  change — and floor 2 still refuses to open a socket while it is enabled, so
  it is a scope decision, not a setting.
- That this reduces any surface. It adds one const, one branch and one test;
  4,028 lines stay. What it buys is that the one way to use them wrongly is now
  loud.

#### What a PR that violates §12.13 looks like (REJECT)

- Flips `LIVE_INLET_HAS_PRODUCER` to `true` without restoring a
  `send_confirmed_bars` producer in the same change (the guard fails; do not
  weaken it to pass).
- Deletes the fold module, its config section, its runbook, or the live lane's
  exclusivity floor citing this section — the narrowing said "alone".
- Replaces the refusal with an `info!`, or drops `stage = "no_producer"`: an
  operator who enabled the fold has to be able to find out why it did not arm.
- Re-arms the fold while `rest_spot_1m` still has no writer, so the boot
  catch-up silently re-folds an empty window.

### What a PR that violates §12.11 looks like (REJECT)

- Creates a CREATE TABLE for a table with no writer so a reader returns empty
  instead of erroring — that is the blank-bar false-OK, restated.
- Removes a reader's QUERY while leaving its alarm, metric filter or EMF name
  in place (the permanently-green dead-monitor class §12.10.5 #5 already bans).
- Deletes a retained table's ROWS, or edits either SEBI allowlist.
- Leaves the operator console's DEFAULT query pointed at a table that may not
  exist on a fresh volume.
- Cites §12.10.5 #6 to justify resurrecting a deleted persistence module: the
  requirement's premise (that the ensure fns survive) is measured false above.

---

## §12.14 — 2026-09-17: the removal's loudest survivor was on the operator's PHONE

**No authorization is claimed.** This records a defect found while closing
§12.11's terraform rows, fixed the same day, and one finding escalated rather
than fixed.

### The defect

Every trading morning the boot Telegram sent, verbatim:

> ✅ Dhan per-minute price capture — armed (fires 9:16 AM to 3:30 PM IST on
> trading days): Dhan spot candles for N indices + Dhan option chain for M
> indices

for legs removed on 2026-09-16. `spot_1m_rest_boot.rs`,
`option_chain_1m_boot.rs`, `dhan_cadence_executor.rs`, `cadence_boot.rs` and
`crates/core/src/cadence/` are all deleted.

**It survived because §12.10.7(e) named the site and the reason, and the reason
was the trap.** That row records *"`main.rs:4201-4206` the boot report's
`spot_1m_enabled` / `chain_1m_enabled` `||` operands"* as a required hand-edit.
The operands read
`spot_1m_rest.enabled || (cadence.enabled && cadence.dhan_lane)` — correct logic,
evaluating flags for modules that no longer compile. `[cadence] enabled` and
`dhan_lane` BOTH still read `true` in `config/base.toml`, so both operands
returned TRUE.

### Why this one outranks every other stale claim in the sweep

Not severity — **placement**. Every other item §12.8–§12.13 corrected sits in a
file somebody has to open: a comment, a docstring, an output description, a
console default. This one is DELIVERED. A green checkmark, on the operator's
phone, once per trading day, asserting a capability that does not exist.

A stale comment costs the next reader a grep. A stale *page* costs the operator
his trust in every page.

### A second false claim in the same block, a month old

Every arm of that message ended `(Groww per-minute legs report separately)`. The
entire Groww feed was removed **2026-08-21**. So the boot Telegram has also been
pointing the operator at a broker's alerts that cannot arrive, since before this
removal began.

### ⚠ And a TEST was pinning both false claims in place

`test_startup_complete_per_minute_capture_wording_arms` asserted the rendered
message **must contain** `"Groww per-minute legs report separately"`. For a
month it would have FAILED any attempt to remove that sentence.

That is the durable half of this section. This file has recorded vacuous guards
(a scan that cannot fail) and stale guards (a claim that went untrue). This is a
third shape and the worst of them: **a test that asserts a message says
something false does not merely miss the bug — it defends it, and it turns
fixing the bug into a test failure.** When a wording assertion and reality
disagree, check which one moved before assuming the test is right.

The replacements assert the two properties that are checkable and that matter —
never claims armed, never names the removed feed — rather than freezing one
exact sentence.

### The fix shape, and why config was not re-gated

Constant-folded to `DHAN_PER_MINUTE_LEGS_EXIST`, not re-gated on a flag. **A
flag cannot arm a module that is not compiled**, so reading config here can only
ever produce a wrong answer, and flipping `[cadence] enabled` back would
silently restore the page. Same shape and same reason as
`rest_candle_fold::LIVE_INLET_HAS_PRODUCER` (§12.13).

`boot_report_capture_claim_guard.rs` does not stamp a `false` — it DERIVES the
expected value from whether a producing module exists on disk, so the const
cannot go stale in either direction.

The event's shape is read by 43 sites, so it is unchanged: the three "armed"
arms are kept and annotated unreachable rather than collapsed. They still carry
the dead Groww clause; that is deliberate and recorded at the site, because they
cannot render and the guard refuses to let them become reachable.

### ⚠ NOT fixed — escalated instead

`crates/app/tests/depth20_apply_properties.rs::re_planning_after_the_real_apply_is_quiet`
is **intermittently red on `main`**, and will break a merge at random. Found
while running the app suite for this change; it is pre-existing and unrelated to
the removal.

Its `layout()` generator has **no distinctness constraint** while its `wire()`
sibling does — and `wire()`'s carries a written justification (*"the dial
deduplicates before subscribing, so a connection cannot hold a repeat"*). So
`want` can ask for one contract on TWO sockets at once; the apply satisfies it on
one, the other still wants it, and re-planning re-asks. That is exactly the
`second > first` the property forbids.

Deliberately not fixed here: the within-socket case is confidently unreachable by
`dedup_subscribe_set`, but the **cross-socket** case — the one the failing seed
hits — needs a reachability claim about `depth20_name_board.rs`, which is outside
the operator's "alone" narrowing. Constraining a generator without proving the
input unreachable would hide a real convergence bug, and convergence is
load-bearing: it is what stops the planner burning a connection's whole swap
budget on churn while its counters read healthy.


#### ✅ RESOLVED 2026-09-18 — and the MECHANISM recorded above is WRONG

The paragraphs above say the residual needs "a reachability claim about
`depth20_name_board.rs`". That claim was made, and it holds exactly:

| Builder | one running `seen` set | flat list then `chunks()` |
|---|---|---|
| `depth20_layout.rs::build_depth20_layout` | `:207`, claimed at `:233` / `:321` / `:327` (pair-atomic, `:331` undoes a half-claim) | `:341` |
| `depth20_name_board.rs::build_name_layout` | `:1061`, claimed via `claim_instrument` `:458-469` | `:1197` |

Those two are the ONLY producers of a `want` — the production call sites are
`depth_rebalance.rs:1891` (name board) and `:1983` (layout fallback), and
`layout.sockets.push` has **zero** occurrences outside those files, so nothing
mutates a layout after it is built. `claim_instrument` short-circuits its
`seen.insert` when the socket buffer is full, so a DROPPED instrument never
falsely claims a key. A globally-distinct `want` is therefore what production
emits, and constraining the generator to match it hides nothing.

**⚠ The MECHANISM this section records is not the one that fires.** The text
above says the apply "satisfies it on one, the other still wants it, and
re-planning re-asks" — deferred work re-asked. That shape was tested directly
and **converges** (1 swap, then 1). What actually fires is a **PAIRING FLIP**:
`match_sockets_by_overlap` pairs a wire socket to a layout socket by key
overlap, so acquiring a duplicated key CHANGES a socket's overlap profile and
the next minute pairs it against a DIFFERENT layout socket — and the
re-pairing costs MORE swaps than the first plan. `second > first` comes from
the re-pairing, never from a deferral.

**Measured, not argued** (temporary instrumentation, tree restored clean):

| Check | Result |
|---|---|
| Convergence, globally-distinct generator | **100,000 cases PASS** |
| Multi-minute fixpoint (0 swaps), 20,000 cases | **0 failures; settles in ≤2 minutes** |
| Same fixpoint with a DUPLICATED `want` | **3.675% never settle** — the excluded shape is genuinely non-convergent |
| Per-socket-only dedup (the `wire()` shape), 50,000 cases | **FAILS** — cross-socket repeat is the sole trigger, so the global constraint is minimal rather than a sledgehammer |

**Honest cost of the constraint:** average non-empty sockets 1.72 → 1.62,
average instruments per case 6.08 → 4.36, and 60.1% of cases still produce a
non-quiet plan. The narrowing is real and is bounded; the removed slice is the
shape production cannot emit.

**Also corrected, because it was load-bearing in the reasoning:** `wire()`'s
justification cites `dedup_subscribe_set` (`dhan_feed_stack.rs:720`), which is
reached only from `build_feed_stack_plan` — the BOOT dial. The per-MINUTE
guarantee actually comes from the arrivals filter `!have.contains(..)`
(`depth20_track.rs:342`). The property is sound; the stated reason was half of
it.

**⚠ NOT claimed:** that the three retained `cc` regression seeds still bite.
They replay a recorded RNG *seed* against a CHANGED generator, so all three now
pass — they are inert, and the live proof is the new bite-proven unit test
(neuter `depth20_track.rs:368`'s `arrivals.retain` and it fails), not the seed
file. The seed file is retained unedited so that "the seeds were not touched"
stays checkable.
**What a PR that violates §12.14 looks like (REJECT):** re-gates the boot
report's capture fields on config; deletes `DHAN_PER_MINUTE_LEGS_EXIST` instead
of flipping it; makes one of the three unreachable arms reachable without
correcting its Groww clause; re-adds a test assertion requiring an
operator-facing message to state something false; or silences the depth-20
proptest by deleting its seed, loosening its assertion, or marking it `#[ignore]`.

---

## §12.15 — 2026-09-24: the 1-MINUTE ACCURACY CHECK IS RESTORED, and S3 archival waits for it

> **Authority:** this section PARTIALLY REVERSES §12.10 for the VERIFICATION class
> only. The per-minute price pulls stay removed. The socket boot floor stays
> removed (§12.10.3 stands). Recorded BEFORE any code, per the rule-file-first law.

### §12.15.0 The verbatim operator demands (preserve EXACTLY, typos included)

**Quote 1 (2026-09-24):**
> "See one and only when our cross verification of one min us fully finished alone right then our s3 archival should happened entilrey right dude"

**Quote 2 (2026-09-24, the authorization):**
> "Bro use the 1 min cross verification alone as whatever I have discussed it to you as the requirement provide it dude okay? Note only one min cross verification dude don't need to rebuild internal timeframes for this cross verification dude then go ahead with this S3 also dude okay? Fix and resoleve everything dude and then merge and deploy it dude okay? Always achieve O(1) everywhere."

Quote 2 rejects the alternative this session offered (gating S3 on the internal
`tf_consistency` rebuild). The gate is the EXTERNAL comparison: our `candles_1m`
(`feed='dhan'`) against Dhan's own 1-minute `charts/intraday` tape.

### §12.15.1 What comes back, and what does not

| Surface | Disposition |
|---|---|
| `POST /v2/charts/intraday`, interval `"1"`, ONCE per trading day after 15:41 IST, for the main-feed spot/index targets | **RESTORED — verification only.** Never written to `ticks` / `candles_*`. The raw vendor answer goes to `dhan_rest_1m_tape` only |
| `dhan_live_crossverify_cell_audit`, `dhan_live_crossverify_daily`, `dhan_rest_1m_tape` writers | **RESTORED** (the tables were retained; only the writers were deleted) |
| The comparator (`dhan_live_crossverify.rs`) | **RESTORED** from `53ea4b6b6^`, adapted: a local sequential pacer replaces the deleted shared Data-API limiter |
| The three `ws-gap-03-xverify-{vacuous,failed,diverged}` log-filter alarms | **RESTORED** (dated row in `dhan-rest-only-noise-lock-2026-07-14.md` §2.5) |
| Per-minute spot-1m pull, per-minute option-chain pull, expirylist | **STAY REMOVED** |
| The boot floor that refused to open sockets without the verifier | **STAYS REMOVED.** The verifier is its own task; the live lane never waits for it |
| An internal timeframe rebuild as the gate | **NOT used** (Quote 2) |

### §12.15.2 The S3 gate (LOCKED)

| Aspect | Locked value |
|---|---|
| What is held | the DAILY post-market archive leg's partitions of trading day D |
| Held until | a `dhan_live_crossverify` day marker exists for D. A marker is written only on a MEASURED verdict (clean or diverged). A vacuous or failed run writes no marker |
| Non-trading day | treated as verified — there is nothing to compare |
| Hold ceiling | `MAX_CROSSVERIFY_HOLD_DAYS`. Past it the partition archives anyway, with a loud coded error — the disk must never fill waiting on a check that cannot finish |
| Disk-pressure leg | **NOT gated.** Losing the box to a full disk costs more than archiving an unverified day. It logs that it archived unverified data |
| Held partitions | counted (`held_unverified`), never counted as failures, and never block the daily verdict |

### §12.15.3 Honest envelope

> "The check compares every in-scope minute of the spot/index targets at
> integer-paise precision, once per trading day, and holds that day's S3
> archive until it has run. **NOT claimed:** coverage of F&O contracts — their
> Dhan `instrument` string is not derivable from the segment, so they are
> counted as unverifiable and skipped, as before. **NOT claimed:** that the
> vendor tape is ground truth — it is an independent record, and a divergence
> means the two disagree, not which one is wrong. **NOT claimed:** that the
> hold never expires — past the ceiling the day archives unverified, loudly."

### §12.15.4 What a PR that violates §12.15 looks like (REJECT)

- Re-adds any per-minute REST pull under cover of this section.
- Makes the live lane wait for, or refuse to start without, the verifier.
- Writes a marker on a vacuous or failed run.
- Gates the disk-pressure archive leg.
- Holds a partition past `MAX_CROSSVERIFY_HOLD_DAYS` without a loud coded error.
- Writes the vendor tape into `ticks` or `candles_*`.

### §12.15.5 — 2026-09-24: a failed check RETRIES THE SAME DAY instead of waiting until tomorrow

**The verbatim operator demand (2026-09-24, typed directly in-session — preserve EXACTLY, typos included):**

> "fix the same-day retry gap and merge deploy it dude.Always achieve O(1) everywhere."

Recorded before the code, per the rule-file-first law. It changes WHEN the §12.15 check runs, not WHAT it compares.

**The gap.** When the 15:41 IST run failed (no token, a run error, zero minutes compared, rows not persisted, or an incomplete run), the loop slept until the next day. The day never got its marker, so §12.15.2 held that day's S3 archive for the full `MAX_CROSSVERIFY_HOLD_DAYS` and then archived it unverified.

| Aspect | Locked value |
|---|---|
| Attempts per trading day | at most `XVERIFY_MAX_ATTEMPTS_PER_DAY` = **4**, the first included |
| Gap between attempts | `XVERIFY_RETRY_INTERVAL_SECS` = **900 s** |
| Longest attempt | token wait (300 s) + `run_budget_secs` (600 s default) + 60 s persist margin = **960 s** |
| Last start | an attempt is started only if it can finish by the **17:30 IST** evening stop |
| Worst case with the default budget | all 4 attempts fit before 17:30 exactly (pinned by test) |
| Page | ONCE per day, after the last attempt: `xverify_vacuous` or `xverify_failed`. Each attempt only logs a `warn!` with a source no filter matches |
| Divergence page | fires on the first attempt that measures it; a retry never pages it again |
| Marker rule | unchanged — `should_write_marker && run_is_complete`, now expressed by `classify_attempt` and proven equal across every outcome |
| Counter | `tv_dhan_xverify_retries_total{reason}` — local `/metrics` only, no EMF name, no alarm |

**What does NOT change:** the alarms and their filters, the S3 hold and its ceiling, the disk-pressure leg, the live lane (it never waits for this check), and every §12.15.4 REJECT row.

**⚠ Honest limit.** A boot catch-up that starts after about 17:14 IST gets no retry, because its next attempt could not finish before 17:30. That day falls back to the §12.15.2 hold ceiling, as before.

**What a PR that violates §12.15.5 looks like (REJECT):** pages `xverify_failed` or `xverify_vacuous` on every attempt; starts an attempt that could still be running at 17:30; raises the attempt count or shortens the interval without re-checking the evening-stop fit; writes the marker on any condition other than `classify_attempt` returning `Ok`.

### §12.15.6 — 2026-09-25: the depth-held OPTION contracts are checked too, in a separate pass

**The verbatim operator authorization (2026-09-25, typed directly in-session):**

> "go ahead with all of these dude okay?"

Given in DIRECT response to a list whose second item was: *"An after-close check of the day's depth-held OPTION contracts against Dhan's own 1-minute record. A dated rule amendment must land first."* That is the §28.2/§28.3 authorization shape: a general go-ahead answering an ENUMERATED list selects the enumerated work. Recorded HERE before the code, per the rule-file-first law.

**The gap.** §12.15 compares spot and index targets only. F&O contracts were skipped because `dhan_intraday_instrument_for` cannot derive a Dhan `instrument` string from the segment alone (§12.15.3). So the contracts the depth sockets watched all day — the ones the operator cares most about — were never checked against any external record.

**Why it is derivable now.** A depth-held contract is always an option (depth-20 and depth-200 carry stock and index options, never futures — 2026-09-18 THIRD). The contract map already classifies every option as `OptionFamily::Index` or `OptionFamily::Stock` from the daily master, which is exactly `OPTIDX` or `OPTSTK`. No guessing.

| Aspect | Locked value |
|---|---|
| Scope | contracts held by EITHER depth pool at any publish during the IST day, segment `NSE_FNO` only |
| Instrument string | `OPTIDX` for `OptionFamily::Index`, `OPTSTK` for `OptionFamily::Stock`, from the contract map. A contract the map cannot resolve is COUNTED and skipped, never guessed |
| Cap | `XVERIFY_MAX_OPTION_TARGETS` = **300**, taken in `security_id` order; the rest are counted as truncated |
| When | ONCE per trading day, after the spot check's outcome is final (success or last attempt), same task |
| Budget | its own `run_budget_secs` (`XVERIFY_OPTION_PASS_BUDGET_SECS` = **150**); the pass starts only if it can finish by **17:30 IST** |
| Pacing | the existing 334 ms REST pacer — ≤ 3 requests/s, ~100 s for 300 contracts |
| Persisted | cell and tape rows only, into the same audit tables. **NO daily row** — the daily DEDUP key `(ts, trading_date_ist, feed, outcome)` would collide with the spot row |
| Marker / S3 hold | **UNAFFECTED.** The option pass never writes, blocks or delays the §12.15.2 day marker, and never changes the retry verdict |
| Page | **NONE.** A catastrophic divergence logs one `warn!` with `source = "xverify_options_diverged"`; no filter matches it |
| Counter | `tv_dhan_xverify_option_pass_total{outcome}` — local `/metrics` only, no EMF name, no alarm |
| `oi` | stays `false` in the request body, as for the spot check |

**⚠ Honest limits (Rule 11).**
- The held-today set lives in RAM. A restart forgets contracts held only before it; those are not checked that day.
- It checks the MAIN-FEED CANDLES of the depth-held contracts, not the depth books. Dhan publishes no historical depth record to compare against.
- A thin option can have legitimate `missing_live` / `missing_rest` minutes; they are reported, not treated as divergence.
- No live run has happened yet. The first session with this build is the measurement.

**What a PR that violates §12.15.6 looks like (REJECT):** lets the option pass affect the day marker or the retry verdict; appends a daily row from the option pass; guesses an instrument string for an unresolved contract; checks futures or `BSE_FNO` under cover of this section; raises the cap or the budget without re-checking the 17:30 fit; adds an alarm or EMF name for the option pass without its own dated row and a lever.
