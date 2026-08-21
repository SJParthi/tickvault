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

> **[ARCHIVED 2026-07-20]** 2026-05-15 historical body (verbatim demand, allowed-set/FORBIDS tables, reconnect parity, mechanical guards, REJECT list, re-approval protocol, auto-driver — all superseded by the 2026-07-13/2026-07-15 amendments; retained as historical audit) — moved verbatim to `docs/rules-archive/websocket-connection-scope-lock-archive.md` (context-size incident; content unchanged).

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
| **Dhan order-update WS** (`wss://api-order-update.dhan.co`) | **SPAWN RETIRED 2026-07-14 (module RETAINED DORMANT)** — supersedes the Q4-i functional-dormant KEEP | Per the §A.1 2026-07-14 subsection below: the `dhan_rest_stack` Phase 5a spawn is DELETED — no process opens this socket anymore. The core module `crates/core/src/websocket/order_update_connection.rs` (+ its unit tests) is RETAINED DORMANT for the future live-trading re-wire: re-spawning it OR deleting the module each requires a fresh dated operator quote HERE first. Historical Q4-i context: it was rewired into `dhan_rest_stack` by PR-C1 (2026-07-13), connected + authenticated, events counted-then-DISCARDED (no WAL, no OMS) — a daily socket to a demonstrably RST-flaky Dhan endpoint that protected nothing while dry_run=true, and the stack's ONLY HIGH-page noise source (WS-GAP-10). Its WAL replay staging (`ws_type=order_update`) is process-global and unaffected. |
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
>
> **2026-07-13 PR-C1 note (Q4-i rewire SHIPPED):** `dhan_rest_stack` now spawns the
> order-update WS (functional-dormant — Phase 5a, after the family-claim tripwire), so a
> dhan-off boot opens **≤1 Dhan WS (order-update only)**; the legacy fast-arm/lane spawn
> sites remain dead code until the Phase C2 deletion, after which the stack is the sole
> call site. **Dormancy honesty (2026-07-13, PR-C1 round-2):** while functionally
> dormant, incoming order-update frames are parsed, counted
> (`tv_order_update_dormant_events_total`) and DISCARDED — no WAL capture, no OMS
> consumer; durable order-event capture returns with live trading (the OMS wiring), and
> boot-staged order-update WAL segments remain undrained on dhan-off boots (pre-existing
> Phase A residual, C2 target).
>
> **2026-07-14 Amendment (order-runtime dry-run PR — SOCKET-FREE under the same-day
> §A.1 noise lock):** with `[order_runtime].enabled = true` (base.toml ON; the serde
> default stays OFF) the dhan-OFF REST stack spawns the DRY-RUN ORDER RUNTIME
> (`.claude/rules/project/order-runtime-dryrun.md`) — a paper OMS + RiskEngine fed by
> paper fills and Groww marks, `dry_run` hard-true, ZERO live orders. It opens NO Dhan
> WebSocket and performs NO order-update WAL capture/drain: the runtime's order-update
> broadcast channel is created with ZERO producers, honoring the §A.1 spawn retirement.
> The LIVE RE-ARM is one quoted follow-up unit — (1) the order-update socket spawn with
> the runtime consumer wired, (2) durable WAL frame capture + the boot drain/conditional
> confirm, (3) the two CloudWatch order-update alarms §A.1 deleted — re-armed together
> only after a fresh dated operator quote lands in
> `dhan-rest-only-noise-lock-2026-07-14.md` §3 + §A.1 here. Ratchets:
> `test_rest_stack_spawns_no_order_update_ws_and_no_canary` (the socket ban) +
> `test_rest_stack_wires_order_runtime` (the socket-free/WAL-free runtime shape).

### §A.1 — 2026-07-14 subsection: Dhan REST-only NOISE lock (order-update spawn retired; Dhan alert surface narrowed to 4)

**The verbatim operator demand (2026-07-14, relayed verbatim via the coordinator
session — preserve exactly, expletives included):**

> "for Dhan except spot 1m and option chain nothing else should work… these fucking
> issues of mid profile and all other fucking issues of Dhan should be entirely
> removed… always make the telegram messages/notifications cleaner, always mention
> precisely which broker."

This quote is exactly the class of fresh dated quote the pre-2026-07-14 §D REJECT row
("Removes the order-update WS instead of rewiring it…") demanded — it SUPERSEDES the
Q4-i functional-dormant ruling for the SPAWN:

1. **The `dhan_rest_stack` Phase 5a order-update spawn is RETIRED** (with its dormant
   drain task, auth-Telegram listener, ws_event_audit consumer wiring, and the two
   CloudWatch alarms `tv-<env>-order-update-ws-inactive` +
   `tv-<env>-order-update-reconnect-storm`). Zero Dhan WebSocket connections exist on
   any boot path until live trading re-wires it.
2. **The core module `order_update_connection.rs` is RETAINED DORMANT** (its unit
   tests stay) — the live-trading re-wire restores the spawn from git history. A fresh
   dated quote is required HERE first to re-spawn it OR to delete the module.
3. The full Dhan noise contract (the 4-item alert set, the profile/canary/no-tick/
   fast-boot-validation/token-gauge deletions) lives in
   `.claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md`.

The §A table's order-update row above is edited in place per house style; the
"AFTER the Phase C rewire: ≤1" total in the paragraph below the table reads **0**
as of 2026-07-14 (the rewire's spawn is retired; the module is dormant code).

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
`instrument_snapshot::is_valid_trading_date`, `presence_registration::ist_day_from_date` *(retired 2026-07-18, stage-4 — caller-less after the presence registry deleted; the scoreboard derives the IST day itself)*,
`storage::lifecycle_reconciler::classify_transition`, the `instrument_lifecycle` /
`index_constituency` / `instrument_fetch_audit` TABLES (SEBI never-delete), the ts-pin
migration, the Groww `shared_master_writer`, the scoreboard, `feed_presence`, and the
constants `INDEX_CONSTITUENCY_BASE_URL` / `GROWW_INSTRUMENT_CSV_URL` /
`SPOT_1M_REST_INDICES` / `DHAN_OPTION_CHAIN_*`.

### §C. Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: Groww capture
> keeps the full bounded zero-tick-loss chain (WAL-before-broadcast → ring → NDJSON spill
> → DLQ; ring constant retired 2026-07-18 with the dead tick chain — the live absorption
> tier is the 200,000-seal ring, `SEAL_BUFFER_CAPACITY`/`seal_ring.rs`); the Dhan REST stack keeps lock-before-mint +
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
  *(2026-07-14 note — PARTIALLY SUPERSEDED by §A.1, the house §37.6-precedent in-place
  annotation: the operator's 2026-07-14 Dhan noise directive RETIRED the functional-dormant
  SPAWN itself — `dhan_rest_stack` Phase 5a no longer opens the socket, so "spawns it
  anywhere OTHER than dhan_rest_stack" now reads "spawns it ANYWHERE at all" pending the
  live-trading re-wire quote. The MODULE-DELETION half of this row STANDS unchanged:
  deleting `order_update_connection.rs` remains REJECT — the dormant module is the
  live-trading re-wire target.)*
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

## 2026-07-15 Amendment — Groww live WS retired; live market-data WS count 1 → 0 (REST-only runtime)

> **Operator directive 2026-07-15 (received directly in this session):** Q1: *"remove the whole Groww live feed; keep only spot 1m and option chain for both brokers; go."*
> Approval Q2 (typos preserved): *"go aehad approv ed dude"*.

Effects: the Groww live NATS-over-WS feed — the SOLE live market-data feed per the 2026-07-13 amendment —
is RETIRED. **Total live market-data WebSocket connections: 0.** Market data is REST-only for BOTH brokers:
the Dhan §8 spot-1m + option-chain pulls and the Groww §9/§38 spot-1m + option-chain (+ bounded contract)
pulls (`no-rest-except-live-feed-2026-06-27.md`). Order/position live-push channels remain a SEPARATE,
authorized surface per the operator's 2026-07-15 order-side directive (recorded by the order-side session;
see the cluster-A rule updates) — market data = per-minute REST pull, order/position events = live push;
the dormant `order_update_connection.rs` module ruling in §A.1 is UNCHANGED. The GDF lock
(`gdf-third-feed-scope-2026-07-13.md`) is UNTOUCHED — it is the ONLY path to any future live market-data
WebSocket. Where the 2026-07-13 amendment's §A table names Groww "THE SOLE LIVE MARKET-DATA FEED", this
amendment supersedes that row.

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
### 2026-07-16 — Groww + Dhan order/position/trade-update PUSH channels authorized (operator directive)

Operator Parthiban, 2026-07-16, verbatim (event 38df2073-eecb-43cf-876d-a4a809dde269):
> "Build real-time order, position and trade-update WebSockets for both Dhan and Groww, paper mode / off by default, no live orders yet. Edit the scope-lock rule files to allow it and use the socket-token the Groww channel needs. Everything's staged on branch claude/groww-order-position-push and PR #1597 — continue from there."
Confirmed after a permission prompt about this rule-file write (event 157f7cd0-dfdf-4c4e-b93a-9f9aff3317c2): OK to record the instruction here and open the two sockets, paper mode / off by default.

GROWW order/position/trade PUSH channel: ONE NEW dedicated NATS-over-WS connection (`wss://socket-api.groww.in`) carrying ORDER / POSITION / TRADE events ONLY — never market data (market data stays REST-only per the 2026-07-15 amendment). Config key `order_push_enabled` under `[groww_orders]` (serde default OFF); module tree `crates/trading/src/oms/groww/push/`; error codes GROWW-PUSH-01..04; `WsType::GrowwOrderUpdate`. Receive-only, paper mode; `GROWW_ORDER_LIVE_FIRE` stays false; no live orders.

2026-07-16 (operator directive above, events 38df2073 + 157f7cd0): the dormant `order_update_connection.rs` is authorized for re-spawn as a PAPER-MODE, receive-only, DEFAULT-OFF channel from `dhan_rest_stack` Phase 5a, gated on `[dhan_order_push] enabled = false`, with `notifier: None` (Telegram-silent — the Dhan 4-item noise-lock family unchanged, the 2 deleted CloudWatch alarms stay deleted). Events are consumed into `order_audit` rows feed='dhan'/mode='paper'. Module DELETION remains REJECT; live order fire remains locked (dry_run untouched).

### 2026-07-19 — Static IP / EIP ruling: release APPROVED for the no-real-orders period (Dhan static-IP whitelist dormant until live re-enable)

Operator Parthiban, 2026-07-19, verbatim (preserve EXACTLY, typos included):
> "until or unless we flip the real orders static ip is not needed due okay?"

Effects (docs-only record; NO terraform flip ships with this note): the Elastic IP
(`13.234.145.177` — the Dhan static-IP whitelist address) is release-APPROVED for the
no-real-orders period. The operator's safety order — VERIFY outbound-without-EIP FIRST,
release SECOND — was executed as a live verification (coordinator session, 2026-07-19,
live describe evidence): a STANDALONE release is UNSAFE (ephemeral-public-IP assignment is
a launch-time ENI attribute; the live ENI `eni-01fdeec2412f55587` can never mint one, so
release-today = no public IPv4 = no SSM/feeds/deploys). Execution is therefore BUNDLED
with the erase-window instance RECREATE per `docs/runbooks/eip-release.md` (recreate →
prove the fresh ENI mints an ephemeral IP → merge the `enable_eip=false` terraform PR —
the path-filtered terraform-apply auto lane is the sanctioned release mechanism).
The Dhan static-IP surface stays consistent with the existing retirements: the Step 6a
boot IP gate + Step 5.5 IP verification already retired with the Dhan live-WS lane (PR-C2,
2026-07-13 — `ip_verifier` has zero production callers, Verified 2026-07-19), and the Dhan
DATA REST pulls (§8 spot-1m/chain) carry no static-IP requirement — only the ORDER APIs do.
**RE-ENABLE PROTOCOL (live trading):** ≥7 days BEFORE the first live order (Dhan's static-IP
modify cooldown): fresh dated operator quote HERE + daily-universe §7 → `enable_eip=true`
terraform PR (allocates a NEW address — the old one is gone forever) → Dhan
`POST /v2/ip/setIP` (PRIMARY) registration → update the EIP literal consumers
(`downsize-instance.yml` `EXPECTED_EIP`, SSM `/tickvault/<env>/network/static-ip`) → re-wire
the boot IP gate before live fire. A PR that flips `enable_eip` in EITHER direction without
the matching dated note here + §7 = REJECT.

### 2026-07-21 — PAPER order-push ACTIVATION prep (DRAFT PR; do not merge without the operator's go)

Prep lane (coordinator-routed, 2026-07-21): `config/base.toml` flips
`[groww_orders] order_push_enabled` and `[dhan_order_push] enabled` to `true` so the
already-BUILT receive-only PAPER order-push channels (authorized 2026-07-16 — events
38df2073-eecb-43cf-876d-a4a809dde269 + 157f7cd0-dfdf-4c4e-b93a-9f9aff3317c2: the Groww
order/position/trade NATS-over-WS channel and the Dhan order-update re-spawn from
`dhan_rest_stack`) begin capturing broker events. Serde defaults stay OFF; `dry_run`
stays true; `GROWW_ORDER_LIVE_FIRE` stays false — zero live orders; market data stays
REST-only (this activates the ORDER-side push surface the 2026-07-15 amendment
explicitly kept separate). The operator's go for the merge lands here verbatim:
**Operator go (2026-07-31, typed directly in-session — verbatim, typos preserved):**
> "Okay then merge all these PRs dude okay? see dont mereg speartely make it a s a
> clubbed PR and mereg dud eokay?"
>
> "just as a single clubbed PR dud eokay?"

Merged 2026-07-31 as part of the single clubbed integration PR (this PR), together
with the two dependabot bumps and the second-scale frame diet. `dry_run` stays true;
`GROWW_ORDER_LIVE_FIRE` stays false; serde defaults stay OFF — capture only, zero
live orders.

### 2026-08-09 — DHAN LIVE MAIN-FEED WS REVIVAL AUTHORIZED (reverses the 2026-07-13 retirement)

**The verbatim operator demand (2026-08-09, typed directly in-session — preserve EXACTLY, typos included):**

> "do thsi dude opkay? but ensiure to sue oen and onl yRUST O(1) dude okay?C — revive Dhan live WS	r8g.xlarge justified	~₹5,824–7,382	Needs a dated quote reversing your 13 July retirement — and that retirement was because Dhan's live data didn't match its own historical record."

Given in direct response to a presented three-option table in which **Option C** read
verbatim *"revive Dhan live WS · r8g.xlarge justified · ~₹5,824–7,382 · Needs a dated
quote reversing your 13 July retirement — and that retirement was because Dhan's live
data didn't match its own historical record."* The operator selected C, quoting the row
back including its warning, and added the standing **Rust-O(1)-only** constraint.

**This is the fresh dated quote the §D REJECT row demands.** It authorizes
re-introducing the Dhan main-feed market-data WebSocket, reversing the 2026-07-13
retirement (Q1/Q2/Q3 of the "2026-07-13 Amendment" above).

**Why the operator needs it (recorded):** with TrueData explicitly excluded from this
instance (operator, 2026-08-09: *"as of now dont add or implement anm.y truedata i said
this isntance is one and onlmy for dhan dude okay?"*) and both Groww and GDF off, the
Dhan live WS is the **ONLY tick source the account can reach**. Without it there are
zero ticks, so the 5 second-scale timeframes (1s/5s/10s/15s/30s) of the 13-timeframe
requirement are unachievable and the r8g.xlarge 32 GiB has nothing to hold. Reviving it
is what makes the Quote 13 instance sizing coherent.

**⚠ WHAT THIS DOES NOT FIX — the retirement reason is UNADDRESSED (Rule 11, no false-OK).**
The 2026-07-13 retirement was not arbitrary; §E of this file quantifies it, and reviving
the lane changes NONE of it because every cause is Dhan-side:

| Measured on 2026-07-06 (776-SID Quote subscription, all trading day) | Value |
|---|---|
| Delivery lag (exchange LTT → our receive) | p50 1.38 s · p90 8.50 s · p95 14.93 s · **p99 46.37 s · max 198.69 s** |
| Groww, SAME host, SAME minutes | **p99 562 ms — ~82× better** (rules out our host/NIC/pipeline) |
| Silent instruments per minute | **29–67**, gaps 300–978 s, **590 gap events** |
| Live-vs-historical candle agreement | operator verdict: *"massive major mismatches"* |

The operator has accepted this knowingly by selecting an option whose own text named the
reason. **The revival must therefore ship the mismatch DETECTION, not a claim that the
mismatch is gone:** the 15:31 cross-verify (fixed 2026-07-11, PR #1474 — it had been
BLIND SINCE BIRTH on a nanosecond-vs-microsecond literal bug) must be live from day one,
and its divergence counts are the honest measure of whether this feed is usable.

**O(1) status (Verified, not assumed):** the Dhan binary parser SURVIVED the July
deletions in full — `crates/core/src/parser/{header,ticker,quote,full_packet,oi,previous_close,disconnect,dispatcher,market_status,read_helpers}.rs`
are all present. That is the fixed-offset `from_le_bytes` hot path with its DHAT
zero-alloc tests, so the O(1) core needs **no rebuild** and the operator's
Rust-O(1)-only constraint is met by existing, already-gated code.

**What the revival MUST rebuild (all deleted 2026-07-13/17 — Verified by file scan):**
main-feed connection + reconnect/pool, subscription builder (hardcoded SIDs per Q3 —
**NOT** the CSV download chain), `MultiTfAggregator` (tick→timeframe), `tick_persistence.rs`
+ `DEDUP_KEY_TICKS` as a **const** (never an inline literal — an inline key evades the
feed-in-key allowlist guard), the tick-gap detector, and the WAL→ring→spill→DLQ wiring.

**What stays FORBIDDEN even under this revival** (unchanged from §D unless separately
quoted): the Dhan instrument CSV download/parse chain (Q3 stands — hardcoded SIDs only);
depth-20 / depth-200 / any additional Dhan WS endpoint; live order fire (`dry_run` stays
true); the §28 indicator/strategy boundary.

**Companion plan:** `.claude/plans/proposals/2026-08-09-dhan-live-ws-revival.md`
(DRAFT — implementation may not start until the operator flips it to APPROVED, per the
design-first wall).

### 2026-08-09 (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS + depth-20/depth-200 AUTHORIZED

**The verbatim operator demand (2026-08-09, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

> "what the fuck bro our idea is toa dd live feed so oevrall 16 websocket
> conbections rigth what na why the fuck puir arhcietctrue design plan fixes
> solutions ntohign is hsown or applciabel here mtoehrfucke rhwy?"

Reaffirmed moments later: *"whta the fuck we have deisgne dveeyhtign in PR 1731
right?"*

Given in direct response to a plan that had WRONGLY listed the feed choice as an
open operator decision, when the revival had already been authorized earlier the
same day by the quote in the section above. The operator is stating the intended
SHAPE of that already-authorized revival: **a live feed totalling 16 WebSocket
connections.**

**This is the fresh dated quote that
`.claude/plans/proposals/2026-08-09-dhan-16-connection-architecture.md` (PR #1731)
names as its own precondition.** That proposal states it "cannot be implemented as
written without a dated operator quote covering (1) depth-20 / depth-200, which
that file currently lists as FORBIDDEN, and (2) a 5-connection main-feed pool,
since the existing lock is 1 connection." Both are granted here.

**What this quote authorizes, precisely:**

| Surface | Before | Now |
|---|---|---|
| Main-feed connections | 1 | **up to 5** |
| depth-20 (`depth-api-feed.dhan.co/twentydepth`) | FORBIDDEN | **ALLOWED, up to 5** |
| depth-200 (`full-depth-api.dhan.co`) | FORBIDDEN | **ALLOWED, up to 5** |
| Order-update WS | 1 (dormant module) | unchanged, 1 |
| **Total live WebSocket connections** | 0 | **≤ 16** |

The 16 figure is the operator's stated target and is consistent with Dhan's own
limits: Dhan confirmed 2026-04-06 that the 5-connection cap applies **per endpoint
type independently**, so 5 + 5 + 5 + 1 = 16. The binding constraint was never
Dhan's — it was this file's own lock, and that lock is lifted to 16 here.

The "What stays FORBIDDEN even under this revival" row four paragraphs above listed
"depth-20 / depth-200 / any additional Dhan WS endpoint" — the depth-20 and
depth-200 half of that row is SUPERSEDED by this quote (house convention: annotate
in place, never rewrite). "Any ADDITIONAL Dhan WS endpoint" beyond these four
stands FORBIDDEN.

**What this quote does NOT authorize (unchanged, still REJECT):**

- More than 16 total live WebSocket connections, or any endpoint beyond
  main-feed / depth-20 / depth-200 / order-update.
- Live ORDER FIRE. `dry_run` stays true and the §39 four-gate lattice is untouched
  — this is a MARKET-DATA authorization only.
- The Dhan instrument CSV download/parse chain (Q3 of the 2026-07-13 amendment
  stands — hardcoded SIDs only, no daily master fetch).
- Any edit to the §28 indicator/strategy frozen area beyond the recorded lifts.

**The honest envelope carried over from the revival section above — NOT weakened by
this quote:** the 2026-07-13 retirement reason remains UNADDRESSED, because every
cause was Dhan-side. Measured 2026-07-06: p99 delivery lag 46.37 s (max 198.69 s)
against Groww's 562 ms on the SAME host in the SAME minutes; 29–67 silent
instruments per minute; live-vs-historical candle mismatches. Reviving the lane
repairs none of that. Additionally, per PR #1731's protocol findings, the India
feed has **no snapshot-on-subscribe** (documented only for the US global-stocks
socket, feed code 29) and **no sequence number**, so packet loss is undetectable at
the protocol level. The 15:31 REST cross-verification is therefore the ONLY
available ground truth and must be live from day one — not a supplementary check.

**Companion plans, both unblocked by this quote:**
`.claude/plans/proposals/2026-08-09-dhan-live-ws-revival.md` and
`.claude/plans/proposals/2026-08-09-dhan-16-connection-architecture.md` (PR #1731).

### 2026-08-11 — THE DEFAULT IS FLIPPED ON (the lane goes live, not just buildable)

**The verbatim operator demand (2026-08-11, typed directly in-session — preserve
EXACTLY, typos included):**

> "switch the dhan feed on espeic llay to cpature all tehs eirght dude am i irght dude?"

This is the **change of DEFAULT** that the 2026-08-09 quotes deliberately did not
make. Those quotes authorized the CODE and the 16-socket budget; the lane still
shipped dark behind a double gate. This quote opens both gates:

| Gate | Before | After |
|---|---|---|
| `[feeds] dhan_enabled` (base.toml + production.toml) | `false` | **`true`** |
| `TICKVAULT_DHAN_LIVE_FEED` env opt-in | unset | **`=1`** in `deploy/systemd/tickvault.service` |

It is also the "fresh dated operator quote" that
`crates/app/tests/dhan_live_off_phase_a_guard.rs` and
`crates/common/tests/production_config_wiring.rs` name as the precondition for
flipping the flag; both guards are INVERTED in the same PR to pin the ON state,
so the flag can never silently drift back OFF either.

**The coupled change this forces — `[rest_candle_fold]` goes OFF.** The live lane
and the REST candle fold both seal into the same `candles_<tf>` tables stamped
`feed='dhan'`, and the dedup key `(ts, security_id, segment, feed)` has no column
that separates them: one silently overwrites the other, and the 15:31
cross-verification would compare the REST record against itself and agree every
time. The lane's exclusivity floor already REFUSES to open a socket while the fold
is on, so leaving the fold enabled would have made this flip a no-op wearing a
success message. The fold is therefore disabled here. **Honest cost:** its 35-day
`catchup_days` backfill of historical minute candles stops running; live capture
replaces it going forward but does NOT backfill the past. Re-enabling it means
turning the live lane off again, until a source discriminator is added to the
candle key — a schema decision, deliberately not taken here.

**⚠ WHAT THIS QUOTE CANNOT DELIVER — 11 of the 16 sockets stay shut, and not for
lack of code.** The operator's words are "capture all these", so this must be said
plainly rather than left to be discovered:

- **Main feed: 1 socket of the 5 granted.** The universe is
  `SPOT_1M_REST_INDICES` — NIFTY, BANKNIFTY, SENSEX, INDIA VIX. Four instruments
  fit one connection; the pool shards by need, so four more sockets are authorized
  and unused. Widening the universe is blocked by Q3 of the 2026-07-13 amendment
  ("hardcoded security IDs only, no instrument download/parsing"), which this
  quote does not touch.
- **depth-20 and depth-200: 0 sockets of the 10 granted.** Both instrument lists
  are empty, and `plan_pool` opens nothing for an empty set. This is a **rule
  conflict, not a gap**: depth needs a tradeable order book, indices do not have
  one, and reaching real option/future contracts requires either the instrument
  master download (forbidden by Q3) or a hardcoded contract list that expires
  every week. Populating depth needs its own dated quote resolving that conflict —
  it is not a config flip.
- **Ticks are captured; the 5 second-scale timeframes are not yet proven.** The
  13-timeframe requirement (Quote 13, 2026-08-08) is what the r8g.xlarge was sized
  for; this flip starts the tick flow that feeds it.

So the accurate one-line summary of this change is: **the Dhan live feed goes from
zero sockets to one, carrying four index instruments** — a real and necessary
first step, and materially less than "all these".

**Everything else stays REJECT** exactly as the 2026-08-09 sections state: no live
order fire (`dry_run` stays true), no CSV download, no fifth endpoint type, no
edit to the §28 frozen area.

### 2026-08-11 (SAME DAY, SECOND QUOTE) — ALL 16 SOCKETS ORDERED OPEN; per-minute REST KEPT RUNNING ALONGSIDE

**The verbatim operator demand (2026-08-11, typed directly in-session — preserve
EXACTLY, typos included):**

> "bro fix all tehse issues whatevr is mentioend dude see emanhwiel ensure to enable connect estbalish al lteh 16 ocnenctions defintitley ddue okay? Meanwhile elt the current rest api hit of evry minute for btoh dhan and groww shodu land let ir run dude okay? do youu nderstand whayt imasnkign ddue okay?"

Given in DIRECT response to a message that stated the opposite of what the
operator wanted and named the blocker: that 11 of the 16 authorized sockets were
shut, that depth-20 and depth-200 sat at ZERO because depth needs a tradeable
order book which an index does not have, and that reaching real contracts
"requires either the instrument master download (forbidden by Q3) or a hardcoded
contract list that expires every week … it is not a config flip." The operator
read that and answered **"definitely"**. This section is the dated quote the
2026-08-09 sections' own REJECT rows demand before that shape changes.

**What this quote authorizes, precisely:**

| Surface | Before this quote | Now |
|---|---|---|
| Main feed | 5 authorized, **1** open | 5 authorized, **open as many as the instrument set needs** |
| depth-20 | 5 authorized, **0** open — no instrument list | 5 authorized, **ORDERED OPEN** |
| depth-200 | 5 authorized, **0** open — no instrument list | 5 authorized, **ORDERED OPEN** |
| Order-update | 1, paper-mode receive-only | unchanged, 1 |
| Per-minute REST (Dhan + Groww) | running | **explicitly ORDERED to keep running** alongside the live lane |
| **Total** | 1 socket carrying data | **16** |

**The REST half is an explicit KEEP, not an afterthought.** The operator's
second sentence — *"let the current rest api hit of every minute for both dhan
and groww … let it run"* — makes the per-minute REST legs a KEPT surface that
the live lane must COEXIST with, never replace. Any change that stands a REST
leg down "because the live feed covers it now" is a REJECT under this quote.
The two write DIFFERENT tables (`ticks` / `candles_<tf>` for the live lane;
`spot_1m_rest` / `option_chain_1m` / `option_contract_1m_rest` for the REST
legs), which is what makes coexistence structurally safe — and is exactly why
the earlier same-day `[rest_candle_fold]` stand-down was correct and is NOT
touched by this quote: that fold wrote into `candles_<tf>`, the live lane's own
table, under a key that cannot separate them. REST legs that write their own
tables coexist; a REST fold that writes the live lane's table does not.

**⚠ THE CONSTRAINT THIS QUOTE DOES *NOT* LIFT (Rule 11, no false-OK).**
The operator ordered the sockets open. He did NOT authorize an instrument-master
CSV download, and **Q3 of the 2026-07-13 amendment stands** (*"hereafter no Dhan
instrument download/parsing — just direct hardcoded security IDs"*). Depth needs
tradeable contract security-ids, and there are exactly three ways to obtain them:

| Source | Rule status | Automation status |
|---|---|---|
| Dhan instrument-master CSV | **FORBIDDEN** by Q3 — not lifted by this quote | would be automatic |
| A hardcoded contract list in Rust | permitted by Q3's letter | **FAILS** the operator's own standing "no manual intervention" mandate — option contracts expire weekly, so a hardcoded list needs a human edit every week and silently goes stale between edits |
| **An already-authorized live source that carries contract security-ids** | permitted — no new fetch class | automatic, self-rolling |

Only the third satisfies BOTH this quote and the operator's standing
zero-manual-intervention rule at the same time. **The implementation MUST use
the third form.** A depth lane fed by a stale hardcoded list would subscribe
expired contracts, receive nothing, and report healthy: the exact false-OK class
this file exists to prevent.

#### The third form EXISTS for OPTIONS — resolved 2026-08-11, same day

The already-authorized, already-running per-minute Dhan option-chain pull
(`POST /v2/optionchain`, the §8 grant of
`no-rest-except-live-feed-2026-06-27.md`) returns a **per-leg
`security_id`** — the tradeable contract's own Dhan id. It is already parsed
(`crates/app/src/option_chain_1m_boot.rs:431`, `ParsedLeg.contract_security_id`)
and already persisted every minute (`option_chain_1m.contract_security_id LONG`,
`crates/storage/src/option_chain_1m_persistence.rs:776`). The vendor doc states
it outright: *"gives you the SecurityId of each option contract directly, no
instrument master lookup needed for subscriptions"*
(`docs/dhan-ref/06-option-chain.md:195`).

**This is the sanctioned depth instrument source.** It costs no new fetch class,
adds no REST call, breaks no rule, and self-rolls: when the expiry changes the
chain returns the new contracts and the depth set follows automatically — the
zero-manual-intervention property the hardcoded-list option cannot provide.

Two things this source does NOT give, both recorded rather than papered over:

1. **The contract's EXCHANGE SEGMENT is absent from the response.** The stored
   `exchange_segment` is the UNDERLYING's (`IDX_I`, hardcoded at
   `option_chain_1m_persistence.rs:108`). Depth subscription needs the
   CONTRACT's segment (`NSE_FNO` = 2 for NIFTY/BANKNIFTY, `BSE_FNO` = 8 for
   SENSEX). That mapping is deterministic from the underlying but is OUR
   assumption, not vendor-supplied — it must be a named, tested, single-source
   mapping, never an inline literal, and it must fail closed on an unknown
   underlying rather than guessing a segment.
2. **`contract_security_id` populated-in-practice is UNVERIFIED-LIVE.** The
   parser defaults it to `0` when the field is absent, and the field is marked
   "added v2.5" upstream. One query settles it —
   `SELECT count(*) FROM option_chain_1m WHERE contract_security_id = 0` — and
   the implementation MUST treat a `0` id as REFUSED-and-counted, never
   subscribed. A zero id would otherwise subscribe instrument 0 and look fine.

#### FUTURES depth is NOT reachable — stated plainly, not silently dropped

There is **no path from any authorized Dhan source to a FUTIDX `security_id`**.
`/v2/optionchain` returns `ce`/`pe` legs only; the expiry-list endpoint returns
DATES, not ids; and `index_futures.rs::select_index_future_expiries` is a pure
date filter fed exclusively by the **Groww** master CSV, whose ids are a
different id space entirely (`exchange_token`, not Dhan `security_id`).

So depth on index FUTURES needs the forbidden CSV, a monthly-expiring hardcoded
list, or its own fresh operator quote. **It is therefore OUT of this quote's
deliverable**, and any claim that "all 16 sockets carry data" must not be read as
including futures depth. Option depth is what this quote can actually deliver.

**What a PR that violates this section looks like (REJECT):**

- Revives the Dhan instrument-master CSV download/parse chain (Q3 stands; this
  quote does not lift it).
- Hardcodes an expiring option/future contract list as the depth instrument
  source (breaks the standing no-manual-intervention mandate and goes silently
  stale).
- Stands down, disables, or starves ANY per-minute REST leg for Dhan or Groww
  in the name of the live lane (the explicit KEEP above).
- Opens a fifth Dhan endpoint type, or exceeds 16 total live connections.
- Reports depth as "enabled" when its instrument set is empty — an empty set
  opens zero sockets, and calling that success is the false-OK this file forbids.
- Flips `dry_run`, touches the §28 frozen area, or arms live order fire — none
  of which this quote mentions.

### 2026-08-11 (SAME DAY, THIRD QUOTE) — Q3 IS REVERSED: the daily Dhan master CSV + NSE India indices download is ORDERED BACK

**The verbatim operator demand (2026-08-11, typed directly in-session — preserve
EXACTLY, typos included):**

> "yes go ahead dude we ened to downlaod the dhan master csv evry day startign right dude espeiclaly to udpate the mappigns and its data entirley always a swell right dude menahwiel see from nse india websote alwyas evryday mornign you need to downlaod all teh indices as well right i mean. to find the rpeicse mappigns between nse india websoite nse idncies csv data with our daily downlaoded new master instruemnts scirpt csv fiel dtaa rigth ddue am i irght dude tell me dude okay?"

Given in DIRECT response to a message that laid out the two options side by side
— keep Q3 and accept that futures depth is impossible, or reverse Q3 and rebuild
the deleted download chain — and named the rebuild cost. The operator chose the
rebuild, and named the JOIN between the two files as the actual deliverable.

**This quote REVERSES Q3 of the 2026-07-13 amendment.** That directive read
*"hereafter no Dhan instrument download/parsing — just direct hardcoded security
IDs passed to spot 1m and option chain"*, and it is the authority every "no CSV
download" REJECT row in this file cites — including the two rows written earlier
TODAY (the 2026-08-11 first and second quotes). Those rows are superseded to
exactly the extent stated here and no further.

#### What is authorized

| Surface | Before this quote | Now |
|---|---|---|
| Dhan instrument-master CSV | FORBIDDEN (Q3); every module DELETED | **DAILY DOWNLOAD ORDERED** |
| NSE India (niftyindices) index constituent lists | one list (NIFTY Total Market) fetched as a Groww watch-build input | **ALL index lists, every morning, as a first-class pipeline** |
| The ISIN join between them | did not exist | **THE DELIVERABLE** — precise constituent → Dhan `security_id` mapping |
| Live-lane universe | 4 hardcoded index SIDs | may be sourced from the rebuilt master (a SEPARATE step; see below) |
| Everything else | — | UNCHANGED |

#### What this quote does NOT authorize (Rule 11 — no scope smuggling)

- **Live order fire.** `dry_run` stays true. Not mentioned, not touched.
- **A fifth Dhan WS endpoint type**, or more than 16 total connections.
- **Any edit to the §28 frozen indicator/strategy area.**
- **Standing down the per-minute REST legs** — the second 2026-08-11 quote's
  explicit KEEP stands and is reinforced, not replaced, by this one.
- **Automatically widening the live subscription set.** The download produces a
  MAPPING; pointing the live lane at it changes what we subscribe and is its own
  decision with its own bandwidth and cost consequences. Building the pipeline is
  ordered here; re-pointing the lane is not, and must not be smuggled in.

#### The mapping contract is ALREADY LOCKED — build to it, do not reinvent it

`daily-universe-scope-expansion-2026-05-27.md` §31.1 (operator-confirmed
2026-06-06) already specifies precisely the join this quote asks for, and it
stands unamended:

1. **PRIMARY KEY = ISIN.** Match the NSE list's `ISIN Code` against the Dhan
   master's `ISIN`, filtered to `EXCH_ID == NSE AND SEGMENT == E AND SERIES == EQ`.
   The matched row's `SECURITY_ID` is the answer.
2. **SECONDARY / cross-check = `(Symbol, Series=EQ, NSE, Equity)`.** Symbol-ALONE
   is BANNED as a primary key — tickers are reused and renamed, so a symbol join
   can silently map to the WRONG security, which is worse than failing.
3. **O(1) build.** One `HashMap<ISIN, (security_id, ExchangeSegment)>` built once
   from the Dhan NSE-EQ rows; each constituent is then an O(1) lookup. Never a
   per-constituent scan of the master.
4. **Fail-closed.** An unresolved constituent is COUNTED and LOGGED BY NAME,
   never silently dropped. Past the tolerance, REJECT the whole build.
5. **Dedup** by the I-P1-11 composite `(security_id, exchange_segment)`.
6. **Role tagging** so `index_constituent` vs `fno_underlying` is an O(1) filter.

The §18 downloader hardening contract (redirect policy `none`, 50 MB body cap,
content-type assertion, cache-path validation, 10s connect / 60s read timeouts,
never log the URL) is likewise already locked and binds this rebuild verbatim.

#### The honest envelope

- **Two tolerances, deliberately different, and they must not be merged:** the
  NSE membership-list tolerance (2%, raised from 0.5% after the 2026-06-08 live
  boot degraded the universe over 5 stragglers out of 748) and the order-critical
  Dhan-master F&O dangling guard (0.5%, unchanged). Collapsing them into one
  number breaks one of the two.
- **A same-wrong-on-both-sides input is invisible by construction.** The join
  detects disagreement between the two files; it cannot detect two files that are
  consistently wrong. Nothing here claims otherwise.
- **Derivative security_ids are documented by Dhan as unstable across days.** The
  mapping is therefore a POINT-IN-TIME artifact per trading day — which is why
  the SEBI tables are append-with-history and never overwritten in place.

#### What a PR that violates this section looks like (REJECT)

- Joins on SYMBOL as the primary key, or drops the ISIN cross-check.
- Silently skips unresolved constituents instead of counting + naming them.
- Merges the 2% membership tolerance and the 0.5% F&O dangling tolerance.
- Ships the downloader without the §18 hardening (a redirect-following client,
  an uncapped body, or no content-type assertion is a REJECT on its own).
- Logs the CSV URL with query parameters, or writes outside the validated cache
  directory.
- Re-points the live subscription set at the new master without its own dated
  quote (see "does NOT authorize" above).
- Presents a build that resolved zero constituents as success — a zero-row join
  passing a "no mismatches" check is the false-OK class this file exists to stop.

### 2026-08-11 (FOURTH QUOTE) — master-sourced live universe authorized to be BUILT, shipped DEFAULT-OFF

**The verbatim operator demand (2026-08-11, typed directly in-session — preserve
EXACTLY, typos included):**

> "Just go ahead and fix everything dude okau"

**Read this section before treating that quote as broader than it is.** It is a
GENERAL reaffirmation, not a targeted instruction about the subscription set.
What makes it usable here is what it answered: the immediately preceding message
named this work explicitly — *"Fork B (widening the live subscription)"* — and
stated its magnitude, *"from 4 instruments to ~25,000"*, alongside the
recommendation that it be built but not activated before the first live probe.
The operator read that and said fix everything.

That is the same authorization shape §28.2 and §28.3 of
`daily-universe-scope-expansion-2026-05-27.md` already accept ("go ahead and fix
and implement eveuthign dude okay>" and "fi everyhtugn dude oaky?"), where a
general go-ahead selected work the preceding message had enumerated. It is
recorded HERE, before any implementation, because this file's own third-quote
section requires exactly that.

#### The tension with the THIRD quote, stated rather than glossed

The third quote of the same day carved this out in as many words: *"the rider
emits a mapping; it does NOT re-point the live subscription set at it… Building
the pipeline is ordered here; re-pointing the lane is not, and must not be
smuggled in."*

A general "fix everything" does not obviously overturn a specific carve-out, and
this section does not pretend that it does. It resolves the tension the only way
that is safe in both directions:

| | |
|---|---|
| **Authorized here** | BUILDING the master-sourced universe path, and landing it in the tree |
| **NOT authorized here** | Any change to what we actually subscribe |
| **Mechanism** | The path ships **DEFAULT-OFF**. Nothing is re-pointed; the live set stays the 4 hardcoded index SIDs until a human flips the flag |

So the carve-out is honoured in substance — the thing it protects is the live
subscription set, and that set does not move. What lands is code that *can* move
it, sitting behind an off switch.

#### Flipping the default needs its own explicit go, and should wait for the probe

Not merely as protocol. **2026-08-12 is this lane's first live session since the
2026-07-13 retirement** — it has never received a Dhan tick. Taking that session
from 4 instruments to ~25,000 means any failure arrives as an unreadable pile
instead of a diagnosable signal. The 4-index probe first, then widen, is the only
ordering that produces an answer.

The recorded reasons for the retirement are also still unrepaired, because they
were never ours to repair: p99 delivery lag 46.37 s (max 198.69 s) against
Groww's 562 ms on the same host in the same minutes, and 29–67 silent
instruments per minute. Widening the universe multiplies whatever that feed
actually does; it does not improve it.

#### What a PR that violates this section looks like (REJECT)

- Ships the master-sourced universe **enabled by default**, in any config file,
  env var, deploy script, or serde default.
- Widens the live set without the boot-time envelope check, so a master that
  returns more SIDs than the authorized 5 main-feed connections can carry takes
  the WHOLE lane down (`plan_pool` refuses the entire pool, not just the excess).
- Subscribes an instrument the master did not resolve, or one whose segment was
  inferred rather than read.
- Presents an empty or partial master-sourced set as "widened" — an empty set
  silently falls back to the index universe, and reporting that as success is the
  false-OK this file exists to stop.
- Flips the default ON without a fresh dated quote in THIS section recording the
  operator's explicit go AFTER a live probe.

### 2026-08-12 — the probe RAN, the lane was BROKEN, and the master-sourced universe is now ON

**The verbatim operator demand (2026-08-12, typed directly in-session — preserve
EXACTLY, typos included):**

> "i need all 16 sow hatevr is needed fix and impelemnt dude okay>"

This is the fresh dated quote the REJECT row immediately above requires, and it
is recorded HERE before the config flip, per this file's own rule-file-first
law. It follows the same-day operator instruction to fix and implement whatever
the 16 connections need.

#### The probe ran. It did not validate the feed — it found a bug.

The fourth quote conditioned the flip on "a live probe". **2026-08-12 was that
probe: the lane's first live session since the 2026-07-13 retirement.** It must
not be reported as a pass, because it was not one. What it produced:

| Evidence (CloudWatch `/tickvault/prod/app`, 2026-08-12) | Value |
|---|---|
| Main-feed dial attempts | **12, all failed** — `WS-GAP-03`, `"HTTP error: 400 Bad Request"`, every 30s from 09:29:28 IST |
| Daily cross-verification | `outcome: Degraded`, **`compared: 0`, `missing_live: 373`**, `missing_rest: 0` |
| depth-200 subscribe | `WS-GAP-02` at 09:06:49 — `"... got: BSE_FNO"`, socket opened and torn down |
| Sockets planned vs carrying data | 8 planned; **only the order-update socket alive** |

So the live lane produced **zero candles for the entire 373-minute session**,
and the main feed never completed a handshake even once.

**Both causes were found and fixed the same day, and neither was Dhan's:**

1. **A missing `/`.** `build_feed_url` appended `?` directly to a pathless base
   URL, so tungstenite — which writes `GET {path_and_query} HTTP/1.1` — put
   `GET ?version=2&token=… HTTP/1.1` on the wire. An origin-form request-target
   must begin with `/` (RFC 9112 §3.2.1), so Dhan's edge answered 400 before the
   upgrade was ever considered. The main feed was the ONLY endpoint without a
   path, which is why it was the only one failing.
2. **BSE_FNO in the depth sets.** SENSEX options are `BSE_FNO` and Dhan serves
   depth on NSE only, so a SENSEX depth socket can only ever die on connect
   (depth-200) or sit live and silent (depth-20, which had no builder check at
   all). Refused at selection time now, and counted.

#### What is flipped, and the honest risk

`[dhan_universe] live_subscription_from_master` moves `false → true` in
`config/base.toml`. The **serde default stays `false`** — an absent section
still means the 4 hardcoded index SIDs, so the fail-safe is unchanged and the
REJECT row above ("enabled by default … or serde default") is not breached.

Why the flip is needed at all: 16 sockets is arithmetically impossible without
it. The main feed spreads its set across the 5 authorized connections one shard
each, and the index universe is **4 instruments** — four sockets, and an empty
fifth is refused by design (an empty subscribe is `EmptyBatch`). Reaching the
fifth main-feed socket requires a fifth instrument, and the master is the only
authorized source of one.

**The honest risk, stated rather than buried.** The fourth quote's own warning
still applies and is not retired by this section: this widens the set from 4
instruments to whatever today's master resolves (**4,565** on 2026-08-12) on a
lane that has **never successfully received a single tick**. If something is
still wrong after the two fixes above, it now arrives across 4,565 instruments
instead of 4. That is a real cost and it was accepted deliberately, on the
operator's explicit instruction, against a box (r8g.xlarge, 32 GiB) sized for
exactly this load.

Three things bound it, none of which is a promise that it works:
- **Fail-soft, loudly.** An unreadable or unparseable mapping artifact falls
  back to the 4 index SIDs with a coded `error!` saying master sourcing was
  REQUESTED and is NOT in effect — never a silent partial widening.
- **Fail-closed on the envelope.** A master resolving more SIDs than the 5
  connections can carry refuses the WHOLE pool rather than truncating.
- **Reversible in one line.** Setting the flag back to `false` restores the
  4-SID universe with a restart.

#### What is still NOT delivered, at 16 sockets or otherwise

- **Futures depth remains unreachable.** No authorized Dhan source yields a
  FUTIDX `security_id` (§ the 2026-08-11 second quote). Unchanged.
- **The 2026-07-13 retirement reasons remain unrepaired** — p99 46.37s delivery
  lag, 29–67 silent instruments/minute — because every one of them is Dhan-side.
- **Nothing here proves the feed works.** The 400 fix explains and removes a
  handshake failure; it does not demonstrate tick delivery. **The measurement
  that settles it is the same cross-verification line that exposed the outage:
  a non-zero `compared`.** Until a session reports one, "the Dhan live feed is
  working" is not a claim this repository can make.

### 2026-07-24 — TrueData live market-data WS authorized as feed #4 (default-OFF, trial-first)

Operator Parthiban, 2026-07-24 (verbatim quotes preserved in
`.claude/rules/project/truedata-feed-scope-2026-07-24.md` §0): authorized preparing
**TrueData** as a fourth, **default-OFF** live-tick market-data feed for a trial
("our plan is to do the trial version with true data … the truedata websocket live feed
entirely … not even miss even a single tick … entirely RUST O(1)").

Effect on this lock: the 2026-07-15 amendment's "total live market-data WebSocket
connections: 0; GDF is the ONLY path to any future live market-data WebSocket" is AMENDED
— TrueData (`wss://push.truedata.in:<port>`, feed='truedata') is a SECOND sanctioned
live-market-data-WS path, **default-OFF** (`feeds.truedata_enabled = false`, serde
default). When `truedata_enabled = true` the live-market-data-WS count is 1 (TrueData);
default remains 0. Groww/Dhan live WS stay retired; the Dhan/Groww order-push channels are
unaffected. Native Rust only (TrueData SDK reference-only). Full contract + the 90-byte
tick layout + the zero-tick-loss / O(1) / instance / reversibility envelope:
`truedata-feed-scope-2026-07-24.md`. Companion plan:
`.claude/plans/active-plan-truedata-feed.md` (DRAFT — operator flips to APPROVED before any
implementation PR).

### 2026-08-15 — FULL-MODE, FULL-UNIVERSE SUBSCRIPTION SCOPE (the ~24,600-instrument set)

**The verbatim operator demand (2026-08-15, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

> "bro see clelary note our entire NTM and entire nse idncies indices and thee ntire ntm and nse indices neitre futures of all the expirires shdou lbe fully subscribed dude wiht full mdoe dude and tehn see one and onl yfor indcies for nifty and banknifty entire options contarcts shodu lbe fully subscribed dude that too alwyas one and onl yfor the current expiry dude okay? then onely fir stocks the max atm plus or minus 25 for btoh call and put should be entirley susbcribed that toto here also current expireis alone right dude alwyas full mdoe right due am i irgth dude tell me dude okay? see emanwhiel what ahpepend to this fuckign 250 dpeth 20 and 5 depth 200 and its precise tabels also ddue okay?"

**The authorization (2026-08-15, same session, in direct response to a message
that laid out the counted scope, the three blockers, and the depth finding):**

> "fix all of thes eneitrley dude okay?"

That second quote is the go. It was given AFTER the reply that named
`MAX_DAILY_UNIVERSE_SIZE = 1200`, the Quote-mode scope lock, and the fact that
depth has no table — so it authorizes those three specifically, not a vague
"do more".

#### The authorized subscription set

| Class | Mode | Expiry scope | Count |
|---|---|---|---|
| All NSE indices | **Full** | n/a | ~30 |
| NTM constituents (spot) | **Full** | n/a | ~750 |
| Index futures | **Full** | **ALL expiries** | ~21 (Assumed 7 × 3) |
| Stock futures | **Full** | **ALL expiries** | ~660 (Assumed 220 × 3) |
| NIFTY + BANKNIFTY options | **Full** | **current expiry ONLY** | ~700 (Assumed) |
| Stock options, **ATM ± 25 both legs** | **Full** | **current expiry ONLY** | 22,440 (220 × 51 × 2) |
| **TOTAL** | | | **~24,600 of 25,000** |

The set is sized to the 5 × 5,000 main-feed capacity with ~400 spare. Only the
stock-option row is arithmetic; every other count is **Assumed** until the
master resolves live.

#### What this quote CHANGES (each needs its own lockstep edit)

1. **`MAX_DAILY_UNIVERSE_SIZE` 1,200 → 25,000.** The authorized set is 20×
   the current boot-halt envelope. Without this the lane refuses to boot.
2. **`DEFAULT_MAIN_FEED_MODE` Quote → Full.** 50 B → 162 B per packet, 3.24×
   the bytes. The test `test_the_default_feed_mode_is_the_scope_locked_quote_mode`
   pins the old value and must be re-blessed in the SAME change.
3. **Universe builder gains contracts.** It resolves SPOT only today (4,565).
   Options and futures selection does not exist and must self-roll at expiry —
   a hardcoded contract list is a REJECT (it goes stale weekly, breaching the
   standing no-manual-intervention mandate).
4. **Depth gets tables, or depth stops dialing.** See below.

#### The depth finding this quote responds to (Verified in source, 2026-08-15)

The operator asked what happened to the 250 depth-20 and 5 depth-200 and their
tables. Traced end to end:

- The drain routes `Depth20 | Depth200` frames to a `depth_unconsumed` counter.
  Its own comment: *"Captured durably in the WAL, counted here, and NOT folded:
  no depth consumer exists yet."*
- `ls crates/storage/src/ | grep -i depth` → **NONE**. `market_depth` and
  `deep_market_depth` were deleted with the earlier live-feed retirements.
  Only the packet-layout constants survive in `constants.rs`.

So today depth-200 pulls 512 KiB frames and **discards every one**. That is the
largest wasted bandwidth in the design and the reason the 2026-08-14 ring split
(`MAIN_FEED_RING_MAX_BYTES` 3:1) exists — a discarded stream must not evict the
kept one.

**BINDING:** depth sockets may not be dialed against instrument sets whose
frames have no consumer. Either the vertical (parser call → DDL → writer →
dedup keys → retention → alarms) lands, or the depth pools stay at zero
instruments. Opening 255 sockets that discard everything is strictly worse
than opening none, and reporting them as "connected" is the false-OK this file
exists to stop.

#### Honest envelope

- **Full mode already carries 5 levels of bid/ask** inside its 162 bytes. Full
  mode on 25,000 instruments therefore overlaps much of what depth-20 is for;
  whether depth-20 is still wanted on the SAME instruments is an open operator
  question, not a settled part of this authorization.
- **CPU at this scale is UNMEASURED.** ~12,500 packets/sec at the open × (decode
  + 24-timeframe fold + ILP append) has never run. Memory fits 32 GiB per the
  Quote-13 sizing; CPU has no such analysis.
- **Nothing here makes the feed work.** No Dhan tick has been received since
  2026-07-13. The measurement that settles it is a non-zero `compared` from the
  15:31 cross-verification.

#### What a PR that violates this section looks like (REJECT)

- Raises the universe cap without raising it in lockstep across the constant,
  this file, and the ratchet that pins it.
- Flips the feed mode without re-blessing the scope-lock test in the same PR.
- Sources option/future contracts from a hardcoded list (goes stale weekly).
- Dials depth sockets whose frames still have no consumer, or reports an
  instrument-less depth pool as "enabled".
- Claims any of this is working before a non-zero `compared`.

#### 2026-08-19 — APPLIED: the two constants this section's REJECT list names

The 2026-08-15 authorization above listed four things it changes. **Two were
recorded and never applied**, which is worth stating plainly: for four days the
rule file said Full mode and 25,000 while the code said Quote and 1,200. This
subsection closes that gap and is the lockstep record its own REJECT rows demand.

| Constant | Was | Now | Ratchet re-blessed in the same change |
|---|---|---|---|
| `connection.rs::DEFAULT_MAIN_FEED_MODE` | `FeedMode::Quote` (code 17, 50 B) | **`FeedMode::Full`** (code 21, 162 B) | `test_the_default_feed_mode_is_the_scope_locked_full_mode` + `test_the_main_feed_payload_carries_the_full_request_code` |
| `constants.rs::MAX_DAILY_UNIVERSE_SIZE` | `1200` | **`25_000`** | `max_daily_universe_size_pinned_at_25000` + the rule-file cross-ref |

The second test on the mode row is the one that matters: a constant changed
while `subscription_builder` still emitted 17 would subscribe Quote packets
while every document claimed Full, and the only symptom would be a quiet
`unparseable` counter. It asserts 21 is present **and** 17 is gone.

**What the mode flip does NOT deliver (Rule 11).** Every Full packet carries 5
levels of bid/ask, and the drain discards them — `ParsedFrame::TickWithDepth(tick, _)`
folds the tick and drops the depth, because no depth writer and no depth table
exist. So the lane now pays **3.24× the bandwidth** and consumes the tick half.
That is deliberate, not an oversight: the same section's second quote binds
depth to "either the vertical lands, or the depth pools stay at zero
instruments", and a writer must exist before anything is claimed as captured.
The 5-level depth inside Full is the *cheapest* future source for that writer —
it arrives on a socket already open — but wiring it is its own unit of work.

**Neither constant makes the feed work, and neither is the measurement.** The
universe cap in particular enforces nothing at all — see the corrected §2
envelope note in `daily-universe-scope-expansion-2026-05-27.md`, which records
that the live lane ran at 4,565 SIDs for a week against a stated 1,200 "cap"
with no halt, because the enforcing function was deleted on 2026-07-13. The
measurement that settles whether any of this works remains a non-zero
`compared` from the 15:31 cross-verification.

### 2026-08-15 (SAME DAY, SECOND QUOTE) — DEPTH IS CAPTURED IN FULL, INTO ONE COMMON TABLE; NOTHING IS DROPPED

**The verbatim operator demand (2026-08-15, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

> "nope mtoehrufcker we ened depth 20 and dpeth 200 shdou lbe shwon and vsisibil in one common atbek dude we need all of them eevry ticks dude okay? we need everyhtign we cnanot miss or hdi or wipe fof nayhtign dude okay?"

Given in DIRECT response to a message that had recommended the opposite — it
reported that depth-200 persistence "fills 100 GB in 1.6–6.5 days" and offered
top-5 levels, derived aggregates, or sampled snapshots as alternatives. The
operator read that and rejected all three. **This section overrules that
recommendation.** It is recorded here rather than argued, because the cost is
the operator's to accept and he has accepted it explicitly.

#### What this authorizes

| Surface | Before this quote | Now |
|---|---|---|
| depth-20 frames | captured to WAL, counted `depth_unconsumed`, **discarded** | **PERSISTED IN FULL** — all 20 levels, both sides, every update |
| depth-200 frames | same — captured then discarded | **PERSISTED IN FULL** — all 200 levels, both sides, every update |
| Destination | none — the writers were deleted with the earlier live-feed retirements | **ONE COMMON TABLE** carrying both depths together |
| Sampling / truncation / top-N | recommended | **FORBIDDEN.** "we cannot miss or hide or wipe off anything" |

#### The DEDUP key finding — the one thing that would have silently "wiped off" data

A single table holding both depths is only safe if the DEDUP key distinguishes
them. depth-20 and depth-200 both emit a level 5 bid for the same instrument at
the same timestamp, and those are DIFFERENT observations from DIFFERENT sockets.
A key of `(ts, security_id, exchange_segment, feed, side, level)` makes them
collide, and QuestDB's UPSERT semantics mean **one silently overwrites the
other** — the exact "wipe off" this quote forbids, produced by the schema rather
than by any code path.

**BINDING:** the common depth table's DEDUP key MUST carry a depth-kind
discriminator (`d20` / `d200`) alongside the I-P1-11 composite and `feed`. A PR
that ships this table without it is a REJECT, regardless of how the writer
behaves.

#### The honest cost — arithmetic, not a warning

> **⚠ CORRECTED 2026-08-15 (same day), operator-caught.** The first version of
> this table said **~70 GB/day** and **~350 GB/day**. Both were wrong by
> **3.4×**, and the reason matters more than the numbers: the rate was
> multiplied by **86,400 seconds — a full calendar day**. Depth frames only
> arrive while the sockets are up, which is the persistence window 09:00–15:40
> IST = **24,000 seconds**. Costing a market-data stream over a 24-hour day
> credits it for the 62,400 seconds the exchange is shut. The row width was
> also carried as a round "≈68 B" rather than derived; it is 72 B, shown below.

Row width, derived: 4 SYMBOL columns (`feed`, `segment`, `depth_kind`, `side`)
at 4 B of interned key each = 16 B, plus 7 eight-byte columns (`security_id`,
`level`, `price`, `quantity`, `orders`, `capture_seq`, `ts`) = 56 B → **72 B**.
depth-20 emits 40 rows/update (20 levels × 2 sides); depth-200 emits 400.
Session = 24,000 s.

| Pool | Instruments | Rows/update | At 1 update/s | At 5 updates/s |
|---|---|---|---|---|
| depth-20 | 250 | 40 | 17.3 GB/day | 86 GB/day |
| depth-200 | 5 | 400 | 3.5 GB/day | 17 GB/day |
| **Total** | | | **≈ 21 GB/day** | **≈ 104 GB/day** |

**The 250-instrument depth-20 pool dominates, not depth-200.** That inverts the
intuition the earlier recommendation was built on: the 5 deep sockets are the
cheap half — and it survives the correction, because both pools scaled by the
same wrong factor. The update rate is the unmeasured multiplier and still swings
the answer 5×; it is **Assumed**, and the first live session measures it.

Against a **100 GB root** that is **~4.8 days at the low estimate and ~23 hours
at the high one** — not the ~1.4 days and ~7 hours the wrong figure implied. gp3
grows online in one command and `variables.tf` permits up to 200 GB.

A further **~17% is available and deliberately NOT taken**: `level` (≤200),
`quantity` and `orders` are `u32` on the wire and stored as `LONG`, costing
12 B/row. Narrowing them to `INT` needs an i64→INT ILP coercion for which this
crate has no existing precedent to copy, and getting that wrong fails every
depth write. Measure it against a live QuestDB before shipping it.

#### How "nothing is dropped" is actually delivered

Capturing everything and keeping everything on EBS are different requirements,
and only the first is what the operator asked for. The mechanism that satisfies
this quote without a disk that cannot exist:

**Same-day S3 archival.** `partition_archive.rs` already implements
archive → verify → drop against the S3 cold bucket; today it runs at >90 days.
The depth table registers with a same-day (or same-hour) partition policy: every
row is written, verified in S3, and only then dropped from EBS. **Nothing is
missed, hidden, or wiped — the hot window on local disk is simply short.** A
drop that has not been verified in S3 first is a REJECT.

Storage cost is real and belongs in front of the operator rather than inside a
follow-up. **Recomputed on the corrected 21 GB/day** (22 trading days, not 30
calendar days — the same session-vs-calendar error, applied to the month):
~**460 GB/month**. In S3 Standard that is ~**$10.6/mo**; in Glacier Instant
Retrieval ~**$1.8/mo**; in Deep Archive ~**$0.45/mo**. The storage-class choice
still changes the bill by ~24×, but at this corrected volume even the most
expensive tier is a fraction of the AWS ceiling rather than half of it — which
materially weakens the case for aggressive tiering and strengthens the case for
keeping the data readily queryable. At the 5-updates/sec end every figure
multiplies by five (~2.3 TB/mo, ~$53 Standard).

*(The pre-correction text said "~2.1 TB/month … ~$48/mo … over half the current
AWS ceiling on its own". That framing drove the original
don't-persist-depth-200 recommendation the operator overruled. He was right on
both counts: the recommendation AND the number behind it.)*

#### What this quote does NOT change

- The 16-connection budget and the four endpoint types. Unchanged.
- `dry_run` stays true; no live order fire; the §28 frozen area is untouched.
- The per-minute REST legs keep running (the 2026-08-11 second-quote KEEP).
- The finding that **depth needs tradeable contract security-ids**, and that the
  only sanctioned source is the already-running option-chain pull's per-leg
  `security_id` (2026-08-11 second quote). Persisting depth does not create
  instruments to subscribe; that constraint stands.

#### What a PR that violates this section looks like (REJECT)

- Ships the common depth table without a depth-kind discriminator in the DEDUP
  key (silent overwrite between the two pools).
- Persists top-N levels, sampled snapshots, or derived aggregates INSTEAD of the
  full book — explicitly rejected by this quote.
- Drops an EBS partition that has not been verified present in S3 first.
- Opens depth sockets before the writer exists (the previous section's binding
  rule stands — a captured-then-discarded frame is still a discarded frame).
- Reports depth as "captured" while `depth_unconsumed` is still incrementing.

### 2026-08-20 — MAIN-FEED PACKS ITS FIRST PASS (the 2026-08-12 spread directive, amended so it can actually be delivered)

**The verbatim operator demand (2026-08-20, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

> "See without building our entire plan why the fuck you stopped motjerfuxker youcfinaih everything entirely fully motherucker rokah"

Given in DIRECT response to a report that said 6 of the 16 authorized sockets
carry data, that contracts do not dial, and that closing it "needs your decision:
pack the spot universe onto fewer sockets to leave room for contracts, or keep it
spread across all 5." The operator's answer is that there was no decision to
escalate — finish it. This section is the dated record that house law requires
before the code changes.

#### What this amends

The 2026-08-12 directive told `plan_pool` to **SPREAD** across the authorized
connections rather than pack into the fewest. That was correct for the shape it
was written for: the main feed carried ONLY the ~4,565 spot universe, and four
authorized sockets sat idle.

The main feed is now dialed in **two passes** — spots at boot, and the ~20,000
option/future contracts once post-open prices exist. Under spread, pass 1 takes
`min(5, 4565)` = **all five** connections, so pass 2 is refused by the stateful
`pool.admit`; and because `MainFeed` is the first endpoint in
`build_feed_stack_plan`'s loop, that refusal aborted **Depth20 and Depth200
planning as well**. Spreading the small first pass is precisely what starved the
sockets the spread directive existed to fill.

| pass | PACKED | SPREAD |
|---|---|---|
| boot spots (4,565) | 1 connection | 5 connections |
| contracts (~20,000) | 4 connections | 0 — REFUSED |
| **main-feed sockets carrying data** | **5** | **1** |
| **depth planned at all?** | yes | **no — aborted by the main-feed refusal** |

#### The amendment (narrow)

**The MAIN FEED packs: `connections_to_use = ceil(len / cap).min(available)`.
DEPTH continues to SPREAD, unchanged.**

Depth must keep spreading and this is not a detail: depth-200 admits ONE
instrument per connection, so packing it would open a single socket and strand
four. The 2026-08-12 reasoning — failure isolation, head-of-line blocking,
decode parallelism — stands verbatim for depth and stands for the main feed too
once both passes have run, because the end state is the same five sockets.

**At the 25,000 target the two policies converge exactly**
(`ceil(25000/5000)` = 5 = `min(5, 25000)`), so this changes nothing at full
scale and everything today.

#### What this does NOT change

The 16-connection budget; the four endpoint types; `dry_run` stays true; no live
order fire; the §28 frozen indicator/strategy area; the per-minute REST KEEP; the
Q3 ban on a hardcoded expiring contract list. Contract security-ids continue to
come from the already-running option-chain leg, which self-rolls at expiry.

#### What a PR that violates this section looks like (REJECT)

- Packs DEPTH (strands four depth-200 sockets on one connection).
- Lets `main_feed_connections_for` and `plan_pool`'s main-feed arm disagree —
  that disagreement is the exact defect this amends, and it cost depth an entire
  session while reporting a depth problem that did not exist.
- Reports main-feed capacity without clamping to what the pool actually has
  free (`.min(available)`), which asks for room that does not exist and earns a
  refusal of the WHOLE pool.
- Claims sockets are "carrying data" on the strength of a dial rather than a
  received frame (see the same-day up-gauge correction).

### 2026-08-21 — FULL MODE EVERYWHERE, and the ONE segment that cannot take it

**The verbatim operator demands (2026-08-21, typed directly in-session — preserve
EXACTLY, typos included):**

> "see evrythgin enitlrey it shdou lbe always full mode with depth 5 dude okay? why ti fialed bro what ahppened what si the issue we need evryhtign in palce and shdou lwork roght dude?"

> "yes fix evrythgin. dude see i need the entire websokcet coenctions of 16 websocket coennections shdou lebe fully started to recieve the ticks startign from 9 am pre makret price and even market price startign 91.5 am also right dude am i irght dude tlem e dude okay?  so fix an dreoslev evrythgind dude okay?"

Given in DIRECT response to a report that the 119 subscribed NSE indices had
produced ZERO ticks since being subscribed, while 8,868 tradeable instruments
were flowing normally at 17.5M ticks/session. This dated section is the
rule-file-first record the 2026-08-15 §"APPLIED" subsection's own REJECT list
requires before `DEFAULT_MAIN_FEED_MODE` behaviour changes.

**Full mode with 5-level depth is CONFIRMED and is NOT changed.** Measured on
the box the same day: 163,934 packets walked frame-by-frame out of the live
capture log were **every one** `code 8` (Full, 162 bytes, 5 depth levels) —
135,892 NSE_FNO and 28,042 NSE_EQ. The operator's requirement is already met
for every instrument that has an order book, and this change does not touch it.

**What this section authorizes is narrow: `IDX_I` — and ONLY `IDX_I` —
subscribes in Quote (17) instead of Full (21).**

| Segment class | Mode | Depth levels |
|---|---|---|
| NSE_EQ, NSE_FNO, BSE_EQ, BSE_FNO — everything with a book | **Full (21)** — unchanged | 5 |
| **IDX_I only** | **Quote (17)** | none exist |

**Why this is not a downgrade of anything real.** An index is a computed
number, not a traded instrument. NIFTY has no bids, no asks, no order book —
there is no depth-5 for Dhan to send. Asking for it requests something that
does not exist, and Dhan's answer is not an error but SILENCE, which is
indistinguishable from a quiet instrument. Quote still carries LTP plus day
open/high/low/close at fixed offsets. Nothing is lost, because indices have no
depth to lose.

**The evidence (all measured 2026-08-21, not inferred):**

| Finding | Value |
|---|---|
| IDX_I packets in 163,934 captured frames | **0** |
| `code 6` PrevClose for IDX_I — which Dhan support CONFIRMED (Ticket #5525125) is emitted for IDX_I on ANY subscription in ANY mode | **0** |
| `RISK-GAP-03` never-ticked count | **119** — exactly the index count (it was **4** on 2026-08-20, when four seeds were subscribed) |
| IDX_I rows in `ticks`, any day on record | **0** |
| Index IDs correct? | yes — master-sourced, segment 0, NIFTY=13 / BANKNIFTY=25 present in `instrument_lifecycle` |

A subscription that never draws even its one guaranteed packet was never
accepted. This is not a parser, registry or persistence fault — nothing arrived
to parse.

**This RESTORES a partition that already existed and was lost in the rebuild.**
`docs/rules-archive/live-market-feed-subscription.md:279` records the
pre-retirement design: *"the `WebSocketConnection` constructor pre-sorts IDX_I
instruments into a separate Quote-mode subscription batch (`connection.rs`, the
`idx_instruments` partition)"*, chosen so the exchange-computed day OHLC came
from the packet rather than being tracked app-side. The lane was hard-deleted
2026-07-17 and rebuilt in the 2026-08-09 revival with ONE global mode and no
partition — `idx_instruments` has zero occurrences on the tree. The 2026-08-19
flip to Full and the 2026-08-20 arrival of the 119 master index ids then made
the loss visible for the first time.

**⚠ UNVERIFIED-LIVE, and deliberately not claimed (Rule 11).** Whether Quote
(17) is SERVED for IDX_I on this account is not settled in this repository:
`docs/dhan-support/2026-05-18-idx-i-quote-full-mode-support.md` asked Dhan this
exact question and **no answer is recorded**, and an older uncited note claimed
Dhan forces Ticker (15) for indices. If Quote is also refused the failure is
identical in shape — silence — so the verification is explicit: the
`RISK-GAP-03` never-ticked count for the index set must fall to **zero** on the
first session after this lands, and `SELECT count() FROM ticks WHERE
segment='IDX_I'` must be non-zero. If it does not, `IDX_I_FEED_MODE` is the one
line to change and `Ticker` is the next value to try. **Nothing here claims the
indices now work; it claims the request we send is no longer one Dhan is known
to answer with silence.**

**What this section does NOT authorize:** any change to the mode of any other
segment; a fifth endpoint type; more than 16 connections; any universe
widening; any live order fire (`dry_run` stays true); or any edit to the §28
frozen indicator/strategy area.

**What a PR that violates this section looks like (REJECT):** subscribes IDX_I
in Full or Ticker without a fresh dated quote here; applies the index override
to a segment that has an order book (that would silently drop all ~24,600
tradeable instruments off depth-5); sends a mixed-segment batch as ONE message
(one Dhan message carries exactly one RequestCode, so one half would get the
wrong mode); or reports the index set as covered while its never-ticked count
is non-zero.
