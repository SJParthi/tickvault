# WebSocket Connection Scope Lock — Operator Lock 2026-05-15

> **⚠ DHAN LIVE WS RETIRED 2026-07-13 (operator directive — Phase A banner; FULL AMENDMENT: the "2026-07-13 Amendment" §-section below):** the Dhan main-feed live WebSocket is RETIRED. Operator verbatim: *"now remove this entire Dhan live websocket feed instruments subscription even entire live websocket feed itself... As of now only Groww and Dhan historical api pull as we discussed last night along with option chain."* Rationale verbatim: *"when we checked the live websocket feed candles and historical data api candles for Dhan has a massive major mismatches... that's why I want to remove this. For Groww let us have live websocket feed api as of now."* (Both operator 2026-07-13, relayed verbatim via the coordinator session.) Effect: `dhan_enabled = false` in base + production config; the PR-E runtime-toggle **ON-half is REVOKED** (a runtime Dhan enable is refused API-side with 409 — re-enable requires a config change + restart + a fresh dated quote); **Groww is the sole live feed**; Dhan is retained for REST pulls only (`spot_1m_rest` / `option_chain_1m` / historical per `no-rest-except-live-feed-2026-06-27.md` §8) **plus the order-update WS — KEPT functional-dormant, rewired into `dhan_rest_stack` (operator Q4-i "agreed dude" ruling, 2026-07-13 — supersedes this banner's original "pending a separate operator decision" wording; the rewire lands in Phase C — see the amendment §A below)**. Lock semantics of the Dhan REST-only stack: `dual-instance-lock-2026-07-04.md` §3.5.
>
> **⚠ ALLOWED-INSTRUMENTS SUPERSEDED 2026-05-27 by [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md):** main-feed subscription expanded from 4 IDX_I SIDs (`LOCKED_UNIVERSE`) to ~250 daily-fetched SIDs (all NSE indices + 1 BSE SENSEX + unique F&O underlyings); all in Quote mode (was Ticker for IDX_I). `SubscriptionScope::Indices4Only` retires; replaced by `SubscriptionScope::DailyUniverse`. The 2-WebSocket lock itself (1 main-feed + 1 order-update) is UNCHANGED. Contents below retained as 2026-05-15 historical audit.
>
> **⚠ SECOND-FEED EXTENSION 2026-06-19 by [`groww-second-feed-scope-2026-06-19.md`](./groww-second-feed-scope-2026-06-19.md):** the 2-**Dhan**-WebSocket lock below is UNCHANGED. The operator authorized adding **GROWW** as an independent, **default-OFF** second market-data feed (feed #2) under a per-feed enable/disable contract. Groww is **native tickvault Rust** (brutex is reference only — no code pulled) reusing the same WAL/ring/spill/DLQ/aggregator chain; it adds NO Dhan connection and touches NO Dhan code. See that file for the verbatim authorization + full contract.
>
> **⚠ DHAN RUNTIME-TOGGLE AUTHORIZED 2026-06-21 (PR-E):** the **count + scope** of Dhan connections is UNCHANGED (still exactly 1 main-feed + 1 order-update, same endpoints, same locked universe). What changed: Dhan is no longer *config+restart only* — it is now **runtime enable/disable-able** from the feed-control webpage, exactly like Groww. Operator verbatim 2026-06-21: *"if I want to switch off or on dhan also it should be accepted right dude"* + (AskUserQuestion) **"Fully disconnect Dhan"** = OFF closes the Dhan WS(es) + stops storing; ON reconnects + re-subscribes (via the existing `SubscribeRxGuard` + dormant-reconnect machinery). Implementation: an `Arc<AtomicBool> dhan_enabled` flag (sourced from `FeedRuntimeState`) is read by the Dhan connection read/reconnect loop — OFF → close + dormant-idle polling the flag; ON → reconnect. **Safety guard (operator-approved 2026-06-21):** Dhan runtime-disable is allowed ONLY while no real orders are live (`dry_run = true` / no open orders+positions); once live trading is on, the toggle REFUSES to disable Dhan so the system can never be blinded mid-trade. This preserves the original "primary trading feed" safety intent that made Dhan config+restart-only. This authorization changes ONLY the lifecycle (start/stop), NOT the 2-connection lock, the endpoints, or the universe.
>
> **⚠ 2026-07-04 OPERATOR UPDATE — FEED TOGGLE BEARER-GATED IN ALL MODES:** the mutating `POST /api/feeds/{feed}` now requires **bearer auth REGARDLESS of trading mode** — the 2026-06-23 (PR-E lineage, AskUserQuestion "tokenless toggle in dev") carve-out that made the toggle PUBLIC when `feed_toggle_public = true` (dry-run/sandbox) is **RETIRED**. Operator verbatim 2026-07-04: *"whicghever is recommended go ahea dudde okay?"* — given in direct response to the recommendation to bearer-gate the publicly-funnelled tokenless feed toggle (the 3001 Tailscale funnel made the tokenless dry-run toggle a feed-disable DoS surface on the public internet; adversarial re-review 2026-07-04, HIGH). Effect: `crates/api/src/lib.rs::build_router_with_auth` places `POST /api/feeds/{feed}` in the bearer-protected router **UNCONDITIONALLY**; the `feed_toggle_public` parameter is accepted-but-ignored (kept only to avoid an 11-call-site signature cascade — it changes NOTHING). Localhost dry-run toggling now uses the SAME token as live mode: fetch it via `aws ssm get-parameter --name /tickvault/<env>/api/bearer-token --with-decryption --query Parameter.Value --output text` and paste it into the `/feeds` page token field (the page already sends `Authorization: Bearer <token>` from sessionStorage — no UI change needed), or curl with the 0600 header-file pattern consistent with `scripts/tv-tunnel/doctor.sh`: `HDR="$(umask 077 && mktemp)"; printf 'Authorization: Bearer %s\n' "$TOK" >"$HDR"; curl -H @"$HDR" -X POST http://localhost:3001/api/feeds/groww -H 'content-type: application/json' -d '{"enabled":true}'; rm -f "$HDR"` (header FILE, never argv — no `ps`/cmdline leak). The read-only `GET /api/feeds` + `GET /api/feeds/health` stay PUBLIC (2026-06-23 "public read, authed toggle" — the READ half of that ruling is unchanged); the Dhan-disable safety gate (`can_disable_dhan`) is unchanged. Ratchets: `crates/api/src/lib.rs` tests `test_feeds_post_requires_auth_401_without_token_in_both_modes` + `test_feeds_post_with_valid_token_not_401_in_both_modes`.
>
> **⚠ FUTIDX-4 EXTENSION 2026-07-08 by
> [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md) §36:**
> the single main-feed conn additionally subscribes ALL available monthly-expiry index-futures
> contracts of the 4 underlyings (§36.7, 2026-07-10; typically ~12, envelope ≤24;
> NIFTY/BANKNIFTY/MIDCPNIFTY = NSE_FNO, SENSEX = BSE_FNO; nearest expiry first; NEVER rolls;
> Quote mode). The 2-WebSocket lock is UNCHANGED. The "Index F&O full-chain" ban below still
> holds — monthly futures serials only, never an options chain. `should_subscribe_index_derivatives`
> remains `false` FOREVER (the FUTIDX path is the DailyUniverse `IndexFuture` role, not that
> legacy gate). OPTIDX/FUTSTK/OPTSTK remain forbidden. Operator verbatim 2026-07-08: *"for both
> dhan and groww we need to add futures and those also should be subscribed along with this,
> especially only for nifty banknifty and sensex nifty midcap."* Operator verbatim 2026-07-10
> (relayed via the coordinator session): *"instead of only one current month futures contracts
> just take all the futures of these indices — I mean take all available applicable months
> futures."*
>
> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > defaults.
> **Scope:** PERMANENT. Every Phase. Every PR. Every future Claude/Cowork session.
> **Operator-locked:** 2026-05-15 (verbatim quote below).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## The verbatim operator demand (preserve exactly)

> "except [for] this 1 connection main feed websocket and order update websocket we will never ever use anything else"

And the immediate follow-up:

> "only these indices and equities will be subscribed and connected right dude?"

(Originally answered YES with 4 IDX_I + 218 NSE_EQ F&O underlying stocks = 222 SIDs. **AWS-lifecycle PR #6/#7 (2026-05-19) narrowed further:** the 218 NSE_EQ stocks dropped, leaving **only the 4 IDX_I SIDs** on the 1 main-feed connection. `SubscriptionScope` is now a single-variant enum — compile-time prevention of accidental expansion.)

---

## The rule (one line)

**This product opens exactly TWO WebSocket connections to Dhan, FOREVER: one main-feed + one order-update. No depth. No second main-feed. No new WS type. Period.**

---

## The complete allowed set

| WebSocket | Count | Endpoint | Allowed instruments | Mode |
|---|---|---|---|---|
| **Main feed** | **1** | `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` | **4 IDX_I SIDs ONLY**: NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21 (per `LOCKED_UNIVERSE` in `crates/common/src/locked_universe.rs`) | Ticker for IDX_I (16-byte packets) |
| **Order update** | **1** | `wss://api-order-update.dhan.co` | Receives order events for orders WE place; filter `Source=P` | JSON, MsgCode 42 auth |

**Total live WebSocket connections to Dhan ever: 2.**

**AWS-lifecycle PR #7 (2026-05-19) update:** the original lock allowed 4 IDX_I + 218 NSE_EQ F&O underlying stocks (= 222 SIDs). The 218 NSE_EQ stocks were dropped from the live universe in PRs #6a/#6b/#7a/#7b as the trading strategy narrowed to indices-only. `SubscriptionScope` is now a **single-variant enum** (`Indices4Only`); a new scope variant cannot be added without a rule-file edit + ratchet update.

---

## What this rule FORBIDS

| ❌ Forbidden FOREVER | Why |
|---|---|
| Depth-20 connections (`wss://depth-api-feed.dhan.co/twentydepth`) | Operator lock 2026-05-15; depth not in product scope for any phase. Modules deleted in PR #4 (#707). |
| Depth-200 connections (`wss://full-depth-api.dhan.co/twohundreddepth` per the official Full Market Depth page; an earlier citation here used the retired `/?token=` form) | Same |
| Any 2nd/3rd/4th/5th main-feed conn | 4 SIDs fit comfortably on 1 conn (Dhan cap = 5,000/conn); more conns waste token+IP budget. `effective_main_feed_pool_size` returns constant 1. |
| Any new WebSocket endpoint Dhan introduces in future | Not in scope without operator explicit re-approval |
| NSE_EQ / BSE_EQ subscriptions (including F&O underlying cash equities) | AWS-lifecycle PR #7 dropped the 218 NSE_EQ universe; `SubscriptionScope::Indices4Only` has no path to emit them. |
| BSE F&O / commodity / currency feeds (except the §36/§36.7 FUTIDX grant — the BSE_FNO SENSEX monthly futures serials on the existing main-feed conn; commodity/currency stay banned — see banner) | Same |
| Stock F&O derivative subscriptions on the main-feed conn — NO carve-out: §36 grants INDEX futures only; FUTSTK/OPTSTK stay master-only forever | Phase 2 dispatcher chain deleted in PR #5 (#708). Planner returns `false` from `should_subscribe_stock_derivatives` unconditionally. |
| Index F&O full-chain derivative subscriptions (except the §36/§36.7 FUTIDX grant — monthly futures serials of 4 underlyings only, never an options chain — see banner) | Same — planner returns `false` from `should_subscribe_index_derivatives` unconditionally. |
| Display-only indices beyond INDIA VIX (sectoral, INDIA VOL, etc.) | `is_display_index_allowed_under_scope` returns `true` only for SID 21 (INDIA VIX). |

---

## Reconnect parity (both allowed WS types)

The 2026-05-13 disconnect storm (09:16-09:29 IST) showed that 500ms first-reconnect was pure unnecessary downtime. After Phase 0 Item 4 fix (2026-05-15):

| WS type | First retry | Subsequent retries |
|---|---|---|
| **Main feed** | **0 ms instant** via `compute_reconnect_base_delay_ms(0, _, _) → 0` (`crates/core/src/websocket/connection.rs:1666`) | `initial * 2^(attempt-1)` capped at `reconnect_max_delay_ms` |
| **Order update** | **0 ms instant** via `compute_reconnect_backoff_ms(1) → 0` (`crates/core/src/websocket/order_update_connection.rs:639`, Phase 0 Item 4 fix 2026-05-15) | `initial * 2^(failures-2)` capped at `ORDER_UPDATE_RECONNECT_MAX_DELAY_MS` |

**Subscription/state preservation across reconnects:**

- Main feed: `SubscribeRxGuard` (`connection.rs:145`) — Drop reinstalls `Receiver` so post-reconnect subscribe commands reach the new socket.
- Order update: stateless protocol — auth message (MsgCode 42) is re-sent on every connect.

---

## Mechanical guards (post-PR #7b — verify each PR they remain)

| Guard | What it enforces |
|---|---|
| `SubscriptionScope` is a single-variant enum (`Indices4Only`) in `crates/common/src/config.rs` | Compile-time prevention — no new scope can be added without a rule-file edit |
| `effective_main_feed_pool_size(_, _) → PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1` constant in `crates/common/src/config.rs` | Main-feed pool always has exactly 1 conn |
| `should_subscribe_stock_derivatives(_) → false`, `should_subscribe_index_derivatives(_) → false`, `is_display_index_allowed_under_scope(_, sid)` returns true only for `INDIA_VIX_SECURITY_ID = 21` (`crates/core/src/instrument/subscription_planner.rs`) | Compile-time impossibility of derivative or sectoral subscriptions |
| Source-scan ratchet `crates/core/tests/indices4only_scope_lock_guard.rs` (3 tests) | Blocks reappearance of retired `SubscriptionScope` variants OR retired `subscribe_*` flags anywhere in `crates/` |
| Test: `test_subscription_scope_has_exactly_one_variant` (`config.rs`) | Match expression in the test must remain exhaustive over the single variant — adding a variant fails the build |
| Test: `test_effective_main_feed_pool_size_is_always_one_under_indices4only` | Pool size is constant 1, no exceptions |
| `daily_universe_scope_guard.rs::futidx_scope_*` (4 tests, §36 2026-07-08; §36.7 2026-07-10) | FUTIDX grant pinned to exactly 4 underlyings + all monthly serials `>= today` + never-roll + legacy gate still `false` |
| Test: `test_first_reconnect_attempt_is_zero_ms_instant` (order_update_connection.rs) | Order-update first retry is 0ms |
| Depth / Phase 2 / movers / greeks modules: deleted in PRs #2-#6b | No code path exists to spawn them; module-level deletion is the strongest guard |

---

## What a PR that violates this lock looks like (REJECT)

- Adds a new variant to the `SubscriptionScope` enum without a rule-file edit + dated operator quote.
- Re-introduces any of the 3 deleted `subscribe_*` config flags (`subscribe_index_derivatives`, `subscribe_stock_derivatives`, `subscribe_display_indices`).
- Adds a `spawn_twenty_depth_connection` or `spawn_two_hundred_depth_connection` call site (the modules are deleted; re-creating them would require restoring whole subtrees).
- Adds a new WebSocket type / endpoint without operator explicit re-approval recorded in this file or charter §I.
- Subscribes to derivative contracts on the main-feed conn (stock F&O, index F&O full chain) — except the §36/§36.7 FUTIDX grant (see banner).
- Subscribes to NSE_EQ / BSE_EQ / NSE_FNO / BSE_FNO / NSE_CURRENCY / MCX_COMM SIDs — except the §36/§36.7 FUTIDX grant (all monthly serials of the 4 underlyings: NSE_FNO ×3 + BSE_FNO SENSEX; see banner).
- Changes `effective_main_feed_pool_size(_, _)` to return anything other than `PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1`.

**Any such PR MUST be rejected in review even if operator explicitly approves verbally** — the operator must update this rule file FIRST, with a dated quote, and only then can the PR land. This prevents accidental scope creep through casual approvals.

---

## Operator re-approval protocol (if scope ever expands)

To add a new WS type or instrument class in a future phase:

1. Operator provides explicit verbatim quote authorizing the expansion.
2. Edit this rule file: add the new WS type to the "complete allowed set" table.
3. Edit `operator-charter-forever.md` §I to reflect the new scope.
4. Add ratchet test(s) pinning the new scope.
5. Open the actual scope-expansion PR citing the rule-file edit commit as its authority.

**No "I think the operator probably meant…" expansions.** This rule file is the single source of truth.

---

## Auto-driver / Insta-reel explanation

> "Sir, imagine your juice shop has only TWO phone lines. One phone is for customers to place juice orders (this is the order-update line). The other phone is for the daily price list from the fruit market — just 4 fruits: NIFTY, BANKNIFTY, SENSEX, INDIA VIX. That's it. We will NEVER install a third phone. No phone for vegetable prices. No phone for grocery prices. No phone for the 200 individual fruit stalls. No second phone for fruits. Two phones forever. If anyone says 'sir, let's add one more phone for spices,' you tell them: NO, this rule file AND the compiler both forbid it — the enum has only 1 slot, there is nowhere to add a 3rd phone. Two phones is enough. Two phones forever."

---

## 2026-07-13 Amendment — Dhan live main-feed RETIRED; order-update WS functional-dormant; Groww sole live feed

> **Authority for this section:** the four verbatim operator quotes of 2026-07-13
> (relayed via the coordinator session), preserved exactly:
>
> **Q1:** "now remove this entire Dhan live websocket feed instruments subscription even
> entire live websocket feed itself... As of now only Groww and Dhan historical api pull as
> we discussed last night along with option chain."
>
> **Q2:** "when we checked the live websocket feed candles and historical data api candles
> for Dhan has a massive major mismatches... that's why I want to remove this. For Groww
> let us have live websocket feed api as of now. But for Dhan as we discussed last night
> only those should be needed and included [the REST pulls: spot 1m per minute + option
> chain + historical]."
>
> **Q3:** "Just Dhan live websocket feed instruments download — I mean the entire process
> completely related to Dhan live websocket feed itself should be switched off entirely or
> removed." (+ verbatim intent: "hereafter no Dhan instrument download/parsing — just
> direct hardcoded security IDs passed to spot 1m and option chain.")
>
> **Q4:** "agreed dude" — agreement to (i) the order-update WS rewire into
> `dhan_rest_stack` (functional-dormant), (ii) tick-gap detector + WS-GAP-06 deletion
> (the Groww feed-stall watchdog owns stall detection), (iii) `SubscriptionScope` enum
> deletion via THIS rule edit.

### §A. The new LOCKED state (supersedes the 2026-05-15 "complete allowed set" table)

| Connection | State (2026-07-13) | Detail |
|---|---|---|
| **Dhan main-feed live WS** (`wss://api-feed.dhan.co`) | **RETIRED — deletion authorized** | Phase A (PR #1496) flipped `dhan_enabled = false` (base + production), revoked the PR-E runtime ON-half (API-side 409), and brought up the REST-only stack (`crates/app/src/dhan_rest_stack.rs`). The Phase C code PRs DELETE the lane: WS pool, subscription planner, `SubscriptionScope` enum, daily-universe fetch chain, tick-gap detector. Re-introduction requires a fresh dated operator quote HERE first (§D). |
| **Dhan order-update WS** (`wss://api-order-update.dhan.co`) | **KEPT — functional-dormant inside `dhan_rest_stack`** (Q4-i) | Rewired in Phase C: spawned from `dhan_rest_stack` (which owns the TokenManager — JWT + dhanClientId are its only needs; MsgCode-42 JSON auth per `live-order-update.md`; zero instrument/universe/registry dependency, map §6 Verified). "Functional-dormant" = connected + authenticated, receiving order events for any order placed via REST; it carries NO market data and is NOT a live price feed. Its WAL replay staging (`ws_type=order_update`) is process-global and unaffected. |
| **Groww live feed** (native NATS-over-WS, 1 connection) | **THE SOLE LIVE MARKET-DATA FEED** | Per `groww-second-feed-scope-2026-06-19.md` (contract unchanged) + `groww-scale-aws-lockout-2026-07-06.md` (1 connection). Same WAL→ring→spill→DLQ→aggregator chain, rows tagged `feed='groww'`. |
| **Dhan REST retained surface** | KEPT (not a WS) | Token/auth stack + per-minute `spot_1m_rest` + per-minute `option_chain_1m` (+ probe) + historical, per `no-rest-except-live-feed-2026-06-27.md` §8; SIDs are the HARDCODED `SPOT_1M_REST_INDICES` (NIFTY=13, BANKNIFTY=25, SENSEX=51 — `constants.rs`), per Q3 verbatim intent. Lock semantics: `dual-instance-lock-2026-07-04.md` §3.5. |
| **GDF (feed #3)** | Separate lock — NOT governed here | `gdf-third-feed-scope-2026-07-13.md` (default OFF, trial-first). This amendment deliberately leaves the pluggable seam clean for it: `FeedsConfig`, feed-in-key shared tables, WAL/ring/spill/aggregator are all UNTOUCHED by the Dhan deletions. |

**Total live market-data WebSocket connections: 1 (Groww).** Total Dhan WebSocket
connections: **TODAY (post-Phase-A): 0 · AFTER the Phase C rewire: ≤1** (order-update,
functional-dormant). The 2026-05-15 "two Dhan phone lines" lock text below is retained
as historical audit; THIS table is the effective contract.

> Footnote (tense honesty): the order-update WS is spawned today ONLY from the Dhan-gated
> fast crash-recovery arm + `start_dhan_lane` — both OFF with `dhan_enabled = false` — so
> the live Dhan WS count is 0 until the Phase C rewire spawns it from `dhan_rest_stack`
> (functional-dormant, ≤1).

### §B. What the Phase C deletion PRs MAY remove (authorized by Q1/Q3/Q4; consumer map Verified 2026-07-13)

Per the Phase B dependency map (`(security_id, exchange_segment)` consumer analysis, every
row Verified with file:line evidence):

1. **The Dhan main-feed WS lane:** connection pool, per-slot supervised loops, subscription
   builder/dispatcher, `SubscribeRxGuard`, the lane FSM (`LaneState` / `start_dhan_lane` /
   `stop_dhan_lane` / `run_dhan_lane_runtime`), the pool watchdog, the lane-owned SLO
   publisher wiring, WAL live-feed re-injection arms specific to the Dhan pool.
2. **`SubscriptionScope` enum + planner (Q4-iii — THIS edit is the dated rule-file
   authorization the enum's own guards demand):** `subscription_planner.rs`, the
   `SubscriptionScope` enum in `config.rs`, `LOCKED_UNIVERSE`,
   `effective_main_feed_pool_size`, and the ratchet
   `crates/core/tests/indices4only_scope_lock_guard.rs` (which exists to pin that enum).
3. **The Dhan instrument-download chain (Q3):** `csv_downloader`, `csv_parser`,
   `fno_underlying_extractor`, `daily_universe(.rs/_orchestrator/_boot)`,
   `instr_fetch_{loop,runner,retry_*}`, `today_instrument`,
   `lifecycle_reconcile_*` (app modules), `constituent_resolver`, the core
   `index_constituency/` module + the lane mapping half of `index_constituency_boot`
   (the process-global ts-pin MIGRATION half is KEPT), `instr_fetch_audit_writer`,
   `prev_day_ohlcv_boot`, `cross_verify_1m_boot` (after relocating
   `parse_intraday_1m_candles` + `MinuteCandle`, consumed by `spot_1m_rest_boot`),
   `InstrumentRegistry`, plan-snapshot files.
4. **Tick-gap detector + WS-GAP-06 (Q4-ii):** the detector, its seeding, the far-month
   alarm-gate exclusion sites, `tv_tick_gap_*` metrics — AND the CloudWatch alarm on
   `tv_tick_gap_instruments_silent`
   (`aws_cloudwatch_metric_alarm.tick_gap_instruments_silent` =
   `tv-<env>-tick-gap-instruments-silent`, `deploy/aws/terraform/app-alarms.tf`,
   including its market-hours window-gate membership + the file's alarm-count/cost
   note), which retires WITH the detector — otherwise Phase C orphans a dead monitor
   (the gauge is never written again once the detector dies). Groww stall detection is
   the FEED-level stall watchdog (`feed-stall-watchdog-error-codes.md`) — see the honest
   envelope in §C.
5. **Error codes** whose only emit sites die with the chain: INSTR-FETCH-01..04,
   NTM-CONSTITUENCY-01, PREVDAY-01, CROSS-VERIFY-1M-01/02, DHAN-LANE-01..04, WS-GAP-06
   (retirement banners in their rule files; enum variants deleted in the Phase C PRs so
   the cross-ref tests stay green in both directions).

**What Phase C MUST KEEP/REWIRE (the Groww/shared seam — scope-lock obligations):**
`index_extractor` (`NSE_INDEX_ALLOWLIST` + `canonicalize_index_symbol`),
`index_futures.rs` (the §36 selector — DE-GATED from the `daily_universe_fetcher` cargo
feature, else the Groww §36.7 futures silently drop = a scope violation),
`instrument_snapshot::is_valid_trading_date`, `presence_registration::ist_day_from_date`,
`storage::lifecycle_reconciler::classify_transition`, the `instrument_lifecycle` /
`index_constituency` / `instrument_fetch_audit` TABLES (SEBI never-delete), the ts-pin
migration, the Groww `shared_master_writer`, the scoreboard, `feed_presence`, and the
constants `INDEX_CONSTITUENCY_BASE_URL` / `GROWW_INSTRUMENT_CSV_URL` /
`SPOT_1M_REST_INDICES` / `DHAN_OPTION_CHAIN_*`.

### §C. Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: Groww capture
> keeps the full bounded zero-tick-loss chain (WAL-before-broadcast → ring → NDJSON spill
> → DLQ, `TICK_BUFFER_CAPACITY` ratcheted); the Dhan REST stack keeps lock-before-mint +
> RESILIENCE-03 (`dual-instance-lock-2026-07-04.md` §3.5); the retirement is
> config-reversible until Phase C deletes the code, and irreversible-without-a-fresh-quote
> after. NOT claimed: (a) any Dhan live tick capture — by design, per Q1/Q2 there is NONE;
> Dhan market data is the per-minute official REST candles only, so intraminute Dhan price
> movement is invisible between fetches; (b) per-SID silence detection — WS-GAP-06 and the
> tick-gap detector die with the Dhan WS (Q4-ii); Groww's stall watchdog is FEED-level
> (whole-universe last-tick), so a single silent Groww instrument is visible only via the
> scoreboard presence/coverage columns and the 15:45 scorecard, not a 30s per-SID page;
> (c) a second live feed as cross-check — until GDF (feed #3) goes live, Groww is a
> single-source live feed and the §37/§38 REST comparisons are the only independent OHLCV
> parity signals."

### §D. What a PR that violates this amendment looks like (REJECT)

- Re-introduces ANY Dhan market-data WebSocket (main-feed, depth, or a new endpoint)
  without a fresh dated operator quote added to THIS section first.
- Re-adds a `SubscriptionScope` enum, `LOCKED_UNIVERSE`, a subscription planner, or any
  Dhan instrument CSV download/parse path (Q3: hardcoded SIDs only).
- Restores the PR-E runtime Dhan-enable ON-half (the 409 refusal is the contract; a Dhan
  re-enable is config + restart + a fresh dated quote).
- Deletes or breaks the KEEP/REWIRE seam items in §B (the Groww §36 futures selector, the
  canonicalizer, the SEBI tables, the ts-pin migration, `parse_intraday_1m_candles`).
- Removes the order-update WS instead of rewiring it into `dhan_rest_stack` (Q4-i keeps
  it functional-dormant), or spawns it anywhere OTHER than `dhan_rest_stack`.
- Deletes `FeedsConfig` / feed-in-key columns / the WAL-ring-aggregator seam "because only
  one feed remains" — the pluggable contract must stay clean for GDF
  (`gdf-third-feed-scope-2026-07-13.md`).
- Weakens the Groww feed's resilience chain in the name of the Dhan deletion.

Any such PR MUST be rejected in review even if the operator approves verbally — the
operator must update this section FIRST with a dated quote.

### §E. The "why" record — quantified evidence behind Q2 (for the permanent record)

The operator's "massive major mismatches" rationale is backed by committed, quantified
evidence (all Verified, sources cited):

| # | Evidence | Value | Source |
|---|---|---|---|
| 1 | Dhan main-feed delivery lag (exchange LTT → our receive), 2026-07-06, all trading day, 776-SID Quote subscription, 10-min windows | p50 1.38 s / p90 8.50 s / p95 14.93 s / **p99 46.37 s / max 198.69 s** | `docs/dhan-support/2026-07-08-orderupdate-rst-and-feed-lag.md` (Incident 3 table + timeline row) |
| 2 | Independent comparison feed (Groww), SAME host, SAME minutes | **p99 = 562 ms** — ~82× better at p99; rules out our host/NIC/network/pipeline. Dhan's whole-second LTT quantization explains ≤ ~1 s, not 46 s | same doc, Incident 3 + "Key observation" §3 |
| 3 | Per-minute silent instruments on the Dhan feed, 2026-07-06 | **29–67 instruments/minute** with tick gaps of 300–978 s; **590 gap events** logged | same doc, timeline row "per-minute tick gaps" |
| 4 | The 15:31 IST Dhan cross-verify was **BLIND SINCE BIRTH** | The `candles_1m`-side SELECT used NANOSECOND literals against QuestDB's MICROSECOND timestamp comparison — the WHERE window sat ~year 58502 and matched ZERO rows on every run since the feature shipped; `compared=0` reported honestly as BLIND, so no mismatch page ever fired. Fixed by PR #1474 (commit `f84b4398`, merged 2026-07-11) — the first sessions with a WORKING comparison are what surfaced the live-vs-historical candle mismatches behind Q2 | PR #1474 commit body (`git show f84b4398`); `crates/app/src/cross_verify_1m_boot.rs` digit-magnitude ratchets |
| 5 | Cross-verify design expectation vs observation | `cross-verify-1m-error-codes.md` §1 documents that NON-ZERO High/Low sampling noise is expected (Dhan WS is a ~2–4 ticks/sec SAMPLED stream vs their full-tape candle API) — "track the trend, not the absolute count". The post-#1474 observed divergence + the Incident-3 lag class exceeded that expected-noise envelope in the operator's judgment (Q2: "massive major mismatches") | `.claude/rules/project/cross-verify-1m-error-codes.md` §1; operator Q2 |
| 6 | Server-side transport instability (supporting) | 2026-07-06: token invalidated server-side with ZERO mints from our box (DH-906 for 4+ hours); 39+ order-update RST-after-accepted-login cycles. 2026-07-08 13:55–14:06 IST: 7 bare-RST main-feed disconnect cycles + a ~2-min full outage — continuing the 2026-07-02 RST pattern | same support doc, Incidents 1/2/4; `docs/dhan-support/2026-07-02-mainfeed-tcp-resets.md` (the file on disk; the 2026-07-08 doc's own cross-link cites the same name) |

Honest note on row 5: the WS-sampled-vs-full-tape asymmetry means SOME candle divergence
was always expected by design; the retirement decision is the operator's judgment call on
its magnitude (Q2 verbatim) reinforced by rows 1–3 (delivery lag + silence), which are
NOT explainable by sampling. Neither side of any candle comparison is claimed as ground
truth (the §37 doctrine). Provenance honesty: the 2026-07-11 first-honest-run mismatch
COUNTS are NOT repo-quantified — they exist only in the AWS box's `cross_verify_1m_audit`
table, the day's `data/cross-verify/` CSV, and the Telegram summary; the "massive major
mismatches" magnitude is the operator's own observation of those outputs (Q2), not a
number reproducible from this repository.

### §F. Auto-driver / Insta-reel explanation

> Sir, the juice shop had two price boards. Supplier Dhan's live board kept freezing —
> some days a price took 46 seconds, once over 3 minutes, to appear, while supplier
> Groww's board on the SAME wall showed the same price in half a second. Worse, when we
> finally fixed our checking machine (it had been comparing against the wrong year for
> weeks!), Dhan's live board didn't even match Dhan's OWN official record book. So the
> owner said: take Dhan's live board DOWN. Keep Groww's live board as the only live one.
> From Dhan we now take just the official printed price card once a minute (the REST
> pulls) — and we keep one Dhan phone line plugged in but silent (the order-confirmation
> line), ready for the day we place orders again. A third supplier (GDF) is being
> auditioned separately — the wall hooks stay ready for their board.

---

## Trigger (auto-loaded paths)

Always loaded. Activates on any session that:
- Edits `crates/app/src/main.rs` (boot sequence)
- Edits any file under `crates/core/src/websocket/`
- Edits `crates/common/src/config.rs` `SubscriptionScope` or related enums
- Edits `crates/app/src/phase2_recovery.rs`
- Edits `config/base.toml` `[subscription]` or `[websocket]` sections
- Adds any new `wss://` URL constant
- Calls any `spawn_*_connection` or `spawn_*_pipeline` function
- Edits `crates/app/src/dhan_rest_stack.rs` or any file containing `SPOT_1M_REST_INDICES` (the post-retirement Dhan surface)
