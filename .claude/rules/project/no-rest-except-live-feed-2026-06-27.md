# No REST Except Live Feed — Market-Data REST Lock (Operator Lock 2026-06-27)

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
| **Live-feed AUTH** — Dhan `generateAccessToken` / `RenewToken` / static-IP gate; Groww `/v1/token` | Produces the token the live WS needs to connect. No auth ⇒ no live feed at all. |
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
| `POST api.groww.in/v1/token/api/access` | `groww/auth.rs:39` | Groww live-feed AUTH | **KEEP** |
| `GET GROWW_INSTRUMENT_CSV_URL` | `constants.rs:612`; `instruments.rs:639` | Groww master (static ref) — the MAP source | **KEEP** |
| `POST /v2/charts/intraday` (1m cross-verify) | `cross_verify_1m_boot.rs:283`; `constants.rs:1255` | MARKET-DATA pull | **REMOVE** |
| `POST /v2/charts/historical` (prev-day OHLCV) | `prev_day_ohlcv_boot.rs:120`; `constants.rs:1259` | MARKET-DATA pull | **REMOVE** |
| `GET /v2/profile` (REST canary + mid-session watchdog) | `rest_canary_boot.rs:170`; `mid_session_watchdog.rs:30` | MARKET-DATA-adjacent poll | **REMOVE** (canary + watchdog) |
| `GET /v2/profile` (token_manager validity check) | `token_manager.rs:466` | AUTH-adjacent | **operator ruling** — auth-adjacent, not a price pull |
| `POST /v2/marketfeed/quote` / `/marketfeed/ltp` (open-price fallback) | `open_price_rest_fallback.rs:136`; `open_price_source.rs:48` | MARKET-DATA pull | **REMOVE** |
| `POST /v2/optionchain` (option-chain cache) | `option_chain_cache_loader.rs:9` | MARKET-DATA pull | **REMOVE** |
| `GET /v2/positions` (orphan-position watchdog) | `orphan_position_watchdog_boot.rs:8` | TRADING (not market-data, not live-feed) | **operator ruling** — trading-adjacent, not a price pull |

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
- Adds any new REST call to `api.dhan.co`, `api.groww.in`, or any market-data host
- Any file containing `charts/intraday`, `charts/historical`, `marketfeed/ltp`, `marketfeed/quote`, `optionchain`, `/v2/profile`, `generateAccessToken`, `RenewToken`, `api-scrip-master`, `niftyindices`, `GROWW_INSTRUMENT_CSV_URL`, `/v1/token`
