# No REST Except Live Feed — Market-Data REST Lock (Operator Lock 2026-06-27)

> **⚠ 2026-07-12 NOTE — BruteX cross-verify S3 read is a KEEP class:** per the operator's 2026-07-12 directive recorded verbatim in `groww-second-feed-scope-2026-06-19.md` §37, TickVault reads BruteX-produced backtest CSVs from OUR OWN S3 bucket (`s3://tv-prod-cold/crossverify/*`) via `aws-sdk-s3` GetObject/ListObjectsV2. That is an INTERNAL artifact transfer from our own infrastructure — the same class as the S3 cold-archive surface — NOT a market-data REST endpoint of Dhan or Groww. A new **KEEP** row is added to the §3 inventory below. This lock's ban on Dhan/Groww market-data REST pulls is UNCHANGED.
>
> **⚠ 2026-07-12 NOTE (SECOND same-day directive) — per-minute REST pipeline is a scheduled-pull KEEP class:** a SECOND 2026-07-12 operator directive (relayed verbatim via the coordinator session, quote preserved in §8.0 below) adds a narrow **scheduled-pull KEEP class**: the per-minute **spot-1m intraday fetch** (`POST /v2/charts/intraday`, interval `"1"`, exactly 3 IDX_I SIDs — NIFTY=13, BANKNIFTY=25, SENSEX=51) + the per-minute **option-chain fetch** (`POST /v2/optionchain` + `POST /v2/optionchain/expirylist`, the same 3 underlyings' current expiry, config-gated DEFAULT-OFF pending the first-live-boot entitlement probe) — **see the new §8**. Two new KEEP rows join the §3 inventory; the matching legacy REMOVE rows are annotated, never deleted. The ban on all OTHER market-data REST pulls is UNCHANGED.
>
> **⚠ 2026-07-13 NOTE — GROWW per-minute REST pipeline is a scheduled-pull KEEP class:** a 2026-07-13 operator directive (relayed verbatim via the coordinator session, quotes preserved in §9.0 below + `groww-second-feed-scope-2026-06-19.md` §38) extends the §8 scheduled-pull KEEP class to GROWW: the per-minute **spot-1m fetch** (`GET api.groww.in/v1/historical/candles`, `candle_interval="1minute"`, 3 Groww spot indices) + the per-minute **option-chain fetch** (`GET api.groww.in/v1/option-chain/...`, the same 3 underlyings' current expiry) + a bounded **per-contract 1m fetch** (same candles endpoint, `segment=FNO`, selected option contracts) — **see the new §9**. Two new KEEP rows join the §3 inventory. The ban on all OTHER market-data REST pulls (incl. any BULK Groww historical sweep) is UNCHANGED.
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
| `POST auth.dhan.co/.../generateAccessToken` | `constants.rs:1246` | live-feed AUTH | **KEEP** |
| `GET /v2/RenewToken` | `auth/types.rs:353`, `constants.rs:1250` | live-feed AUTH | **KEEP** |
| `GET/PUT /v2/ip/getIP` `setIP` | `constants.rs:1017,1270,1279`; `main.rs:4417…` | live-feed STATIC-IP gate | **KEEP** |
| `GET images.dhan.co/api-data/api-scrip-master-detailed.csv` | `csv_downloader.rs:129,209` | instrument-master (static ref) — the MAP source | **KEEP** |
| `GET niftyindices.com ind_niftytotalmarket_list.csv` | `instruments.rs:646`; `main.rs:1978,2667,6806` | constituents (static ref) — the MAP source | **KEEP** |
| ~~`POST api.groww.in/v1/token/api/access`~~ | ~~`groww/auth.rs`~~ | Groww live-feed AUTH | **REMOVED 2026-07-02** — superseded by the shared token-minter SSM read (`groww-shared-token-minter-2026-07-02.md`); TickVault never mints |
| `GET GROWW_INSTRUMENT_CSV_URL` | `constants.rs:612`; `instruments.rs:639` | Groww master (static ref) — the MAP source | **KEEP** |
| `POST api.groww.in/v1/api/apex/v1/socket/token/create/` (per-session socket-token mint) | `constants.rs` (`GROWW_SOCKET_TOKEN_URL`); `feed/groww/native/socket_token.rs` | Groww live-feed AUTH — mints the per-session NATS user JWT from the SSM-read access token (NOT an access-token mint; the shared-minter lock 2026-07-02 is untouched). Recorded 2026-07-04 with the native-shadow-client authorization (`groww-second-feed-scope-2026-06-19.md` §35); the Python sidecar's SDK already makes this exact call | **KEEP** (added 2026-07-04) |
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
- Any file containing `charts/intraday`, `charts/historical`, `marketfeed/ltp`, `marketfeed/quote`, `optionchain`, `/v2/profile`, `generateAccessToken`, `RenewToken`, `api-scrip-master`, `niftyindices`, `GROWW_INSTRUMENT_CSV_URL`, `/v1/token`, `spot_1m_rest`, `option_chain_1m`, `v1/historical/candles`, `v1/option-chain`, `groww_spot_1m`, `groww_option_chain_1m`, `option_contract_1m_rest`

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
| `POST /v2/optionchain` + `POST /v2/optionchain/expirylist` | the SAME 3 underlyings, CURRENT (nearest) expiry only; expirylist once at day start | chain once per minute, SEQUENCED after the spot fetch; 1 unique req/3s honored (3 distinct underlyings concurrent); `client-id` header required | `option_chain_1m` ONLY | `[option_chain_1m]` config — **DEFAULT-OFF** pending the first-live-boot entitlement probe |

## §8.3 Probe outcome (2026-07-12, recorded honestly)

The option-chain entitlement (absent June 2026 — the DH-902/806 class that got the old subsystem deleted 2026-06-28) is **UNPROBEABLE from the development sandbox**: `dhan.co` egress is 403-blocked at the proxy AND no minted access token exists in SSM (only client-id/client-secret/totp-secret; minting from the sandbox would invalidate the live prod token per `authentication.md` rule 5). Therefore the chain half ships **config-gated DEFAULT-OFF** with a **first-live-boot entitlement probe** (one bounded `POST /v2/optionchain/expirylist` call) that reports the verdict via Telegram; only then does the operator enable it per config. The spot half is INDEPENDENT and is NEVER blocked by the chain half.

## §8.4 Honest envelope (mandatory per operator-charter §F)

- **Just-closed-minute availability is UNDOCUMENTED and UNVERIFIED-LIVE** — how fast Dhan's intraday endpoint surfaces the minute that closed 1 second ago is unknown until the first live session. The fetcher does bounded in-minute retries and MEASURES the close-to-data latency with a histogram — a slow/empty response is loud (typed ErrorCode + counter), never a false-OK.
- The chain entitlement is UNPROVEN until the live boot probe (§8.3); the chain half stays OFF until proven + operator-enabled.
- Disk envelope (honest): `option_chain_1m` ≈ ~337K rows ≈ ~70 MB/day at ~150 strikes ≈ ~6.3 GB/90d (~20-28% of the 30 GB EBS) — retention/partition-manager registration is a flagged follow-up in the code PRs; `spot_1m_rest` is trivial (~1,125 rows/day).
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

---

# §9. Groww per-minute REST pipeline — scheduled-pull KEEP class (operator authorization 2026-07-13)

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
| `GET api.groww.in/v1/historical/candles` (spot) | 3 Groww spot indices ONLY: `NSE-NIFTY` / `NSE-BANKNIFTY` / `BSE-SENSEX`, segment CASH; `candle_interval="1minute"`; day-granular window + client-side minute filter; `Authorization: Bearer <shared-minter SSM read-only token>` + `x-api-version: 1.0` | once per minute close, [09:15, 15:30) IST trading days, + one bounded 15:31 sweep (the Dhan PR #1499 pattern, pending merge) | `spot_1m_rest` ONLY, `feed='groww'` (never `ticks`/`candles_*`/`historical_candles`) | `[groww_spot_1m]` config, serde default OFF, base.toml opts in |
| `GET api.groww.in/v1/option-chain/exchange/{e}/underlying/{u}?expiry_date=...` | the SAME 3 underlyings, CURRENT (nearest ≥ today) expiry only, from the already-ingested Groww instruments CSV | once per minute, SEQUENCED after the Groww spot fetch; own min-gap pacing | `option_chain_1m` ONLY, `feed='groww'` | `[groww_option_chain_1m]` config — **DEFAULT-OFF** pending first-live-session verification + a dated note |
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
