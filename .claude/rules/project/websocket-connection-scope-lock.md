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

> **⚠ CORRECTED 2026-08-22 — 4,565 is the MAPPING-ROW count, not the
> instrument count, and the real live set is roughly 870.**
>
> `join_constituents` dedups on `(index_name, security_id, segment)` — scoped
> to the INDEX — so a stock belonging to twelve of the forty-six downloaded
> NSE India index lists produces TWELVE resolved rows. The artifact's
> `mappings` array is that sum across all 46 lists WITH the duplicates, plus
> one row per NSE index. 4,565 is what that array holds.
>
> The subscribe path has always deduped on the I-P1-11 composite key
> (`dhan_live_universe::select_live_universe`, `seen: HashSet<(SecurityId,
> ExchangeSegment)>`), so the number of instruments the lane DIALS is the
> distinct count: about **120 NSE indices + ~750 unique NIFTY Total Market
> stocks ≈ 870**. NTM is the broadest basket and contains the other lists'
> members, which is why the distinct total lands near its own size rather than
> near the sum.
>
> Pinned by `dhan_live_universe.rs::a_stock_repeated_across_many_index_lists_
> is_subscribed_once`, which feeds exactly 4,565 rows in and asserts the
> instrument count out, so this cannot be re-derived wrongly a third time.
>
> **What the wrong number changed downstream, so each is checked rather than
> assumed:**
>
> * The packing table below reads "boot spots (4,565) → 1 connection". The
>   CONCLUSION is unchanged and is now stronger: at ~870 the boot pass takes
>   one connection with far more room to spare, and `ceil(870/5000) = 1` the
>   same as `ceil(4565/5000) = 1`.
> * `dhan-rest-only-noise-lock-2026-07-14.md` uses 4,565 to price
>   per-instrument CloudWatch metrics at ~$1,369/mo. At ~870 that is ~$261/mo
>   — still far above what the budget can absorb, so the decision to dimension
>   latency per CONNECTION rather than per instrument stands unchanged.
> * Sizing arguments that treat the boot pass as "already spending ~4,565 of
>   the 25,000 slots" are working from the row count. The spot pass spends
>   about 870, leaving correspondingly more headroom for contracts.
>
> No live figure is claimed here: `distinct_instruments` and
> `tv_dhan_universe_distinct_instruments` were added the same day and publish
> the measured value from the next 08:30 build onward. Until one lands, ~870
> is what the code must produce, not what the box reported.

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

### 2026-08-25 — THE DAY OPEN IS THE PRE-OPEN EQUILIBRIUM PRICE, NOT OUR FIRST OBSERVED TICK

**The verbatim operator demand (2026-08-25, typed directly in-session — preserve
EXACTLY, typos included):**

> "firts tell me whtehr all thsi sorted ourt meanwhiel whatve rthe 9.12 am ckose rpcie shdou lbe eth 9.15 am open price rigth ddue see we need to make the rpe market price as 9.15 am open price always right dude am i irgth ddue see emanhwiel alogn with this to fidn each and evry one minute of high low tarckign cpaturing also we beed to tarck pature and do it irtght dude am i irght dude tell me dude okay?"

**Authorization to implement (same session, immediately after the plan was
listed back with this item marked as needing his decision):**

> "yes dude fix evrythgin dud ehwtevr i have stared and discusse ddude oaky?"

This section is the dated record the rule-file-first law requires, written
BEFORE the code change. It REVERSES a behaviour that is currently deliberate
and test-defended, so it cannot land on a verbal reading alone.

#### What the code does TODAY, and why it was written that way

`crates/app/src/day_ohlc_orchestrator.rs` states it in its own header:

> *"`day_open` is the first live SESSION tick (not a pre-market value) … the
> 09:15:00 tick IS the day open"*

`day_ohlc_session_accepts` REFUSES every pre-open timestamp, and
`test_preopen_tick_never_arms_day_open_then_0915_tick_is_the_open` exists
specifically to keep it that way. That was a defensible choice when the
pre-market buffer had just been deleted and the only alternative was a
half-removed code path.

#### Why the operator is right, and why the fix is not what it first looks like

On NSE the 09:15 opening price **is** the pre-open equilibrium price,
discovered in the 09:08–09:12 matching window. So "the 09:12 close is the
09:15 open" is not a preference — it is how the exchange defines the open.

Taking "the first tick we happen to observe at or after 09:15:00" is a
DIFFERENT number for any instrument that does not trade in the first moments
of the session. For a thin stock option that first prints at 09:31, our
recorded `open` was a 09:31 price wearing the day's opening label. Across
20,268 stock options that is a systematic error, not an edge case.

**The fix is therefore NOT to synthesise an open from pre-open ticks.** The
exchange already sends its own value: `ParsedTick.day_open`, present in every
Quote and Full packet (`crates/common/src/tick_types.rs:38` — *"Day open price
(from Quote/Full; 0.0 for Ticker)"*). The authoritative open is already on the
wire and is being discarded in favour of a derived one.

#### The contract

| Aspect | Locked value |
|---|---|
| Source of `day_open` | the EXCHANGE field `ParsedTick.day_open` when finite and > 0 |
| Fallback | first in-session tick LTP, exactly as today — Ticker-mode packets carry `day_open = 0.0` and must not be broken |
| Pre-open gate | UNCHANGED for high/low/close. This section changes where OPEN comes from; it does not admit pre-open ticks into the day's range |
| Per-minute high/low | UNCHANGED — already folded per tick into the 1m bucket and 23 other frames |

#### What a PR that violates this section looks like (REJECT)

- Synthesises a day open from a pre-open tick LTP instead of reading the
  exchange's own `day_open` field.
- Lets a `day_open` of `0.0` (the documented Ticker-mode absent sentinel)
  overwrite a real open with zero.
- Admits pre-open ticks into day HIGH/LOW/CLOSE under cover of this quote —
  it authorizes the OPEN only.
- Removes the first-tick fallback, which is the only source for a Ticker-mode
  instrument.



### 2026-08-26 (SECOND) — DEPTH-200 IS NIFTY + BANKNIFTY ATM, AND IT MUST TRACK ATM ALL SESSION

**The verbatim operator demand (2026-08-26, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

> "see meanwhile clealry ntoe for evry one minute alwyas espeiclaly for dpeth 200 esppeiclaly as of now nifty atm ce atm pe always for every one minute resusbcribe dude okay even for bancknfity atm ce atm pe also dude okay are you doign this dude as of now or nto dude okay"

**The authorization (same session, in direct response to the three open items
being listed back to him):**

> "fix and resolve evryhtign dude okay?"

#### What was actually happening — measured, not inferred

The five depth-200 sockets on 2026-08-26 carried:

| Slot | Contract | Rows in the 09:20 minute |
|---|---|---|
| 1 | NIFTY Sep-26 24150 CE | 100,800 |
| 2 | FINNIFTY Sep-26 26250 CE | **800** |
| 3 | FINNIFTY Sep-26 26250 PE | **800** |
| 4 | MIDCPNIFTY Sep-26 15000 CE | 100,000 |
| 5 | MIDCPNIFTY Sep-26 15000 PE | 100,800 |

**BANKNIFTY got ZERO sockets**, and NIFTY got one lone leg. Two of the five
went to FINNIFTY strikes delivering ~125x less than the others.

**The cause is `atm_distance`**, which returns `|strike - spot|` — a RAW
ABSOLUTE price distance — and `select_depth_universe` sorts one `pair_pool`
mixing every underlying by that number. Absolute rupee distance is not
comparable across underlyings with different price levels and strike
spacings: a FINNIFTY strike 50 points from a ~26,200 spot outranks a
BANKNIFTY strike 100 points from a ~57,500 spot, though the BANKNIFTY strike
is nearer in every sense that matters and vastly more liquid.

That mis-ranking also caused a second, separate fault: connections 12 and 13
redialled **112 and 210 times** (against 19 each for the other three) because
the 50-second idle-silence watchdog reads a sparse deep book as a dead socket.
Putting liquid ATM contracts in those slots removes the churn as a side
effect — one fix, two faults.

#### The contract (LOCKED)

| Aspect | Locked value |
|---|---|
| Priority | **NIFTY first, then BANKNIFTY.** Their ATM CE/PE pairs claim four of the five sockets before any other underlying is considered |
| Ranking within an underlying | nearest-ATM, unchanged |
| Ranking ACROSS underlyings | **normalised**, never raw rupees — absolute distance is not comparable between a 24,000 index and a 57,000 one |
| 5th socket | unchanged: the next-nearest lone leg, with `depth_200_lone_leg` still reporting it |
| Budget | UNCHANGED — 5 instruments, 5 sockets. This changes WHICH five, never how many |
| ATM tracking | the selected strike must follow spot through the session, not freeze at the boot-time value |

#### What this reverses, stated rather than quietly applied

`dhan_feed_stack` skips re-selection once `depth_done`, on the recorded
grounds that "re-running two QuestDB queries a minute for a set that is
already subscribed buys nothing". That was true of a set chosen by identity
and false of a set chosen by ATM: an ATM contract picked at 09:10 is not ATM
at 14:00 if the index moved, so the boot-time choice decays all session. The
operator's instruction is that it must track.

**Edge-triggered, NOT unconditional.** "Re-subscribe every minute" taken
literally is ~375 re-subscribes per socket per session, on the very feed whose
reconnect churn this same change is fixing. The re-subscribe fires only when
the resolved ATM strike actually CHANGES, which yields the operator's outcome
at a fraction of the cost. A no-op minute must cost no socket action.

#### What a PR that violates this section looks like (REJECT)

- Ranks depth-200 candidates across underlyings by raw `|strike - spot|`.
- Lets any underlying outside NIFTY/BANKNIFTY take a socket while a NIFTY or
  BANKNIFTY ATM pair is available.
- Changes the socket or instrument budget (5 remains 5).
- Re-subscribes unconditionally every minute rather than on an ATM change.
- Hardcodes contract security-ids — they expire; the chain leg is the source.
### 2026-08-26 — THE OPEN RULE RE-AFFIRMED, AND WHAT WAS ACTUALLY MISSING

**The verbatim operator demand (2026-08-26, typed directly in-session — preserve
EXACTLY, typos included):**

> "Always ensure the finalised pre open 9.12 close price as 9.15 am open price dude menawhile fix and resolve evryhrinf else Dus eokay"

This is the THIRD statement of the same requirement (2026-08-25 twice, now
again), and it is recorded because the operator restated it, not because
anything about the contract changed. **The §"2026-08-25" contract above is
UNCHANGED and remains the authority**: `day_open` comes from the EXCHANGE field
`ParsedTick.day_open` when finite and plausible, with the first in-session tick
LTP as the fallback, and the pre-open gate on HIGH/LOW/CLOSE is untouched.

**This quote does NOT authorize folding pre-open ticks into candles.** The
operator's words are about the OPEN PRICE, which is a different mechanism from
the `out_of_session` ingest gate (402,549 ticks refused on 2026-08-26). The
2026-08-25 section already says so in as many words — *"it authorizes the OPEN
only"* — and that carve-out stands. Reading a re-affirmation of a rule as an
expansion of it is the scope-smuggling this file's REJECT lists exist to stop.

#### VERIFIED WORKING, live, before this note was written

Queried on the prod box mid-session, 2026-08-26 — `ticks`, security_id 13
(NIFTY):

| IST | `ltp` | `open` |
|---|---|---|
| 09:00:02 | 24035.25 | **NULL** — pre-open; Dhan sends no day-open field yet |
| 09:15:00 | 24343.05 | **24341.95** — the exchange's equilibrium open |

Two things this proves, and both matter:

1. **The exchange's open is what we store**, and it DIFFERS from the 09:15 LTP
   (24341.95 vs 24343.05). So "the first tick at or after 09:15" would ALSO
   have been wrong; only the vendor's own field is right.
2. Every one of the 101,348 null-`open` rows that day was `IDX_I`, and they sit
   before 09:15. Indices legitimately have no exchange open during pre-open.

#### THE GAP THAT WAS REAL — a ratchet, not the behaviour

The behaviour was already correct: `update_tick_with_exchange_open` assigns
`self.day_open = exchange_day_open` unconditionally on every plausible open, so
a late-arriving exchange open CORRECTS a fallback taken from a pre-open print.
What did not exist was a test for that path. The one test on this rule
(`the_exchange_open_replaces_the_first_tick_we_happened_to_see`) covers the
open arriving WITH the first tick — not the shape indices hit every single
morning, where the first several ticks carry no open at all.

So a refactor that made the adoption first-write-wins would have left NIFTY's
recorded day open at **24035.25, a pre-open price, wrong by ~307 points on the
headline index every day**, and every existing test would still have passed.

Added, using the live numbers above as the fixture:
`a_late_exchange_open_corrects_the_preopen_price_we_fell_back_to` and
`many_preopen_prints_still_end_on_the_exchange_open`
(`crates/trading/src/in_mem/day_ohlc_tracker.rs`).

#### What a PR that violates this section looks like (REJECT)

- Makes the exchange-open adoption first-write-wins, or otherwise lets a
  pre-open print survive as the day open once the exchange has published one.
- Deletes or weakens either new test.
- Treats this re-affirmation as authorization to fold pre-open ticks into
  candles, or into day HIGH/LOW/CLOSE — it is not, and the 2026-08-25 carve-out
  binds.
- Uses "the first tick at or after 09:15" as the open instead of the exchange's
  own field (measured above: they differ).

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

> ### ✅ SETTLED 2026-09-06 — the Assumed multiplier is now MEASURED, and it is the HIGH column
>
> The paragraph above closes by saying the update rate "is **Assumed**, and the
> first live session measures it." That session happened, and the measurement has
> been sitting in `depth_persistence.rs`'s own module header since 2026-08-24
> without ever being carried back to this table.
>
> | | rows/session |
> |---|---:|
> | This table's model at **1 update/s** | 288,000,000 |
> | This table's model at **5 updates/s** | 1,440,000,000 |
> | **MEASURED, 2026-08-24** | **1,530,651,649** |
>
> Implied rate: **5.31 updates/second** — slightly ABOVE this table's own high
> column. At 72 B/row that is **110.2 GB/session** of logical depth rows: the
> high column (104 GB) was right to within 6%, and **the low column (21 GB) is
> understated 5.2×**.
>
> **That matters because the LOW column is the one the sizing decisions quote.**
> `daily-universe-scope-expansion-2026-05-27.md` reasons that a 100 → 200 GB grow
> "buys roughly +4.8 days at the low estimate and +1 day at the high one." Only
> the second half was ever true. The same paragraph in this file says a 100 GB
> root is "~4.8 days at the low estimate and ~23 hours at the high one" — it is
> ~23 hours, full stop.
>
> **And 110 GB is not the disk burn.** The measured consumption is **~307 GB per
> session** (booted 2026-09-01 at ~309.6 GB free, ended at 2.4 GB), i.e. **2.8×**
> the logical depth rows once ticks, 24 candle frames, the raw-frame WAL, the
> spill tiers and QuestDB's own write amplification are counted. A row-width
> model is a floor on disk consumption, never an estimate of it — and this table
> has been read as the latter.
>
> **What it cost.** On 2026-09-03 the volume reached 0 bytes free; on 2026-09-04
> the box booted onto a full disk, captured **zero** ticks all day, and dropped
> **2,000,238** frames before the write-ahead log — permanent loss, because a WAL
> that cannot append cannot rescue. A 600 GB volume against a 307 GB session is
> under two sessions of room, not the ~28 days the low column implies.
>
> Nothing about the SCOPE changes here — this is the measurement the table asked
> for, recorded where the table is, so the next disk decision starts from the
> right column.

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

### 2026-08-21 — SPOT UNIVERSE NARROWED to indices + F&O underlyings (the change that makes the authorized contract set fit)

**The verbatim operator demands (2026-08-21, typed directly in-session — preserve EXACTLY, typos included):**

> "See for nifty and banknifty indices alone only enifr cuurent options of current expiry right dude but for entire stocks options of fno alone only w ehsoud always pull current expiries of atm plus minus 25 alone right dude oaky."

> "Go ahead and fix evrfhbrin fuxd eokay?"

The first quote SPECIFIES the contract shape; the second authorizes the work.
Both are recorded here BEFORE any code, per this file's own rule-file-first law.

#### What the first quote CONFIRMS (already true — no change authorized or needed)

| Operator's words | Code today | Status |
|---|---|---|
| NIFTY + BANKNIFTY: "entire options of current expiry" | `FULL_CHAIN_INDEX_UNDERLYINGS = ["NIFTY","BANKNIFTY"]`, full chain, current expiry, NO ATM window (`dhan_contract_universe.rs:223,655`) | **already correct** |
| F&O stocks: "current expiries of atm plus minus 25 alone" | `STOCK_OPTION_ATM_STRIKES_EACH_SIDE = 25` (`constants.rs:995`) | **already correct** |

This retires the executor's earlier recommendation to CAP the index chains. The
operator has now explicitly ruled the opposite: index chains stay UNCAPPED. That
is a decision on the record, not a loose end — and it means index-chain depth
(measured 542 contracts on 2026-07-13, and 2,037 for three indices on
2026-04-25) is a vendor-controlled swing the design accepts.

#### What the second quote AUTHORIZES (the actual change)

**The spot universe narrows from the master-sourced constituent set to NSE
indices + F&O stock underlyings only.**

The arithmetic, on MEASURED figures:

| Component | Slots | Source |
|---|---|---|
| NSE indices (spot) | 119 | MEASURED — never-ticked count, 2026-08-21 |
| F&O stock spots (required to compute ATM) | 216 | MEASURED — live QuestDB, 2026-04-25 (`constants.rs:981-983`) |
| NIFTY+BANKNIFTY current-expiry options, full chain | 542–~1,200 | MEASURED range |
| NIFTY+BANKNIFTY futures, all expiries | 6 | 2 × 3 |
| F&O stock futures, all expiries | 648 | 216 × 3 |
| F&O stock options, ATM ± 25 | 22,042 | MEASURED (`constants.rs:981-983`) |
| **TOTAL** | **23,573–24,231** | **fits, 769–1,427 spare** |

Against the CURRENT spot universe (`live_subscription_from_master = true`,
measured **4,565** SIDs) the same contract set totals **~27,800–28,500** —
over by ~3,000. The code says so itself at `dhan_feed_stack.rs:4855-4862`:
*"leaving 4 whole connections plus ~435 spare ≈ 20,435 for contracts. The
authorized contract set is ~23,820, so it ALREADY does not fit."*

**So the spot universe is the ONLY lever left, and narrowing it is what makes
the operator's stated design fit.** Nothing about the contract shape changes.

#### The mechanical contract

| Aspect | Locked value |
|---|---|
| New spot set | NSE indices (all, as today) + the F&O stock UNDERLYING set derived from `FUTSTK`/`OPTSTK` rows of the daily master |
| Derivation point | artifact build time — `dhan_universe.rs` already holds `&[MasterRow]` with `InstrumentClass` (`:276`), so no new fetch and no new parse pass |
| Artifact shape | a SEPARATE artifact listing F&O underlying ids. The existing mapping artifact is **not** re-shaped — an additive file cannot break a consumer that never reads it |
| Default | **OFF.** Serde default false; an absent section keeps today's behaviour byte-for-byte |
| Fail-soft | an unreadable/absent F&O artifact falls back to today's master-sourced set with a coded error — never a silent narrowing, and never an empty spot set |
| Unchanged | the 16-connection budget, the 4 endpoint types, `dry_run`, the §28 frozen area, the per-minute REST legs, and the contract selection in every respect |

#### What a PR that violates this section looks like (REJECT)

- Ships the narrowed spot universe **enabled by default**, in any config file, env var, deploy script, or serde default.
- Caps, windows, or otherwise narrows the NIFTY/BANKNIFTY option chains — the operator explicitly ruled them UNCAPPED in the first quote above.
- Changes `STOCK_OPTION_ATM_STRIKES_EACH_SIDE` away from 25.
- Derives the F&O underlying set from anything other than the daily master's own `FUTSTK`/`OPTSTK` rows (a hardcoded list goes stale weekly — the standing no-manual-intervention mandate).
- Lets an unreadable F&O artifact produce a SILENT fallback, or an empty spot set.
- Re-shapes the existing mapping artifact rather than adding a separate one.

#### Honest envelope

The 22,042 stock-option figure is **2026-04-25** at **216** stocks. If today's
F&O list or ladder depth has grown, the 769–1,427 spare shrinks accordingly —
and at the top of the measured index-chain range the margin is already under
800. `scripts/count-current-expiry-universe.sh` exists to replace April's
number with a measured one; until it has been run on the box, **this section's
"fits" verdict rests on a four-month-old measurement** and is stated as such.

### 2026-08-21 (THIRD quote of the day) — THE ENTIRE GROWW FEED IS ORDERED REMOVED

**The verbatim operator demand (2026-08-21, typed directly in-session — preserve
EXACTLY, typos included):**

> "Dude along with all these see clealry note whqtber is entirely related to groww feed remove everything entilrey dude okay? Do you understand what I'm asking dude"

This is the fresh dated quote that this file's own §"2026-08-11 (SAME DAY, SECOND
QUOTE)" REJECT list demands before any Groww surface is stood down. It is recorded
HERE, BEFORE any code change, per the rule-file-first law.

#### What this REVERSES — stated plainly, because it is a reversal

The 2026-08-11 second quote said, verbatim: *"Meanwhile elt the current rest api
hit of evry minute for btoh dhan and groww shodu land let ir run dude okay?"* —
and this file turned that into a REJECT row reading **"Stands down, disables, or
starves ANY per-minute REST leg for Dhan or Groww in the name of the live lane"**.

Ten days later the operator has ordered the opposite for the Groww half. The later
instruction governs; the earlier one is recorded here rather than quietly
overwritten, so the reversal is auditable. **The DHAN half of that KEEP is
UNTOUCHED** — Dhan's per-minute spot-1m and option-chain legs keep running exactly
as they do today. Nothing in this quote mentions Dhan.

#### What is authorized

| Surface | Disposition |
|---|---|
| Groww live NATS-over-WS market-data feed | already RETIRED 2026-07-15 — nothing to remove |
| Groww per-minute REST legs (spot-1m, option-chain, per-contract 1m) | **REMOVED** — reverses the 2026-08-11 KEEP |
| Groww order/position/trade PUSH channel + the §39 order-side lattice | **REMOVED** |
| Groww daily master CSV / watch build / universe rider | **REMOVED** as a live path |
| Groww cadence-scheduler executor arm | **REMOVED** |
| Groww CloudWatch alarms, dashboard widgets, EMF metric names | **REMOVED in lockstep** — see the dead-monitor rule below |
| Groww SSM token READ (`fetch_groww_access_token`) | **REMOVED** — tickvault never minted it (the bruteX Lambda owns minting; that Lambda is not ours and is unaffected) |

#### ⚠ What this quote CANNOT mean, and must not be read as (SEBI)

**`instrument_lifecycle`, `instrument_lifecycle_audit` and `index_constituency`
rows carrying `feed='groww'` are NEVER deleted.** §5/§6/§25 of
`daily-universe-scope-expansion-2026-05-27.md` bind them to SEBI 5-year
point-in-time retention, and that obligation does not depend on whether we still
consume the feed that produced them. Removing the WRITER is authorized; deleting
the ROWS is not, and no PR under this quote may issue a `DROP`, `DELETE` or
`TRUNCATE` against those tables. This is the single most important line in this
section: "remove everything related to Groww" is a CODE instruction, and reading
it as a DATA instruction would destroy regulatory history that cannot be rebuilt.

#### ⚠ What is LOST by this removal (Rule 11 — no false-OK)

1. **Dhan becomes single-source with no independent parity check.** The §37/§38
   cross-verification compares our data against a Groww-derived record. With Groww
   gone there is no second vendor to disagree with us, so a Dhan-side error becomes
   undetectable by comparison — only by internal consistency. The 15:31 REST
   cross-verify against Dhan's OWN historical API remains, and that is a weaker
   signal: it can only catch us disagreeing with Dhan, never Dhan being wrong.
2. **Any comparator left pointing at a feed that no longer publishes will report a
   vacuous pass** — `compared = 0` rendering as "no mismatches". That is the
   false-OK class this repo has retired twice. Every such comparator must be
   removed or made to fail loudly on a zero-row comparison, in the SAME change.
3. **Alarms on `tv_groww_*` metrics become permanently-green dead monitors** the
   moment nothing publishes them. They must be deleted in lockstep, not left
   sitting green.

#### What a PR that violates this section looks like (REJECT)

- Deletes, drops or truncates any `feed='groww'` row from the SEBI tables.
- Stands down, disables or starves any **DHAN** per-minute REST leg under cover of
  this quote — the 2026-08-11 KEEP still binds the Dhan half.
- Leaves a `tv_groww_*` alarm, dashboard widget or EMF metric name in place after
  its producer is gone (a permanently-green dead monitor).
- Leaves a cross-verify or parity path that now compares against nothing and
  renders `compared = 0` as success.
- Removes shared, feed-generic machinery (the cadence scheduler, the `Feed` enum,
  `spot_1m_rest` / `option_chain_1m` / `rest_fetch_audit` tables and their
  writers) because "only Dhan is left" — the pluggable seam must stay clean for
  GDF and TrueData, whose scope locks are untouched by this quote.
- Ships the removal without the ratchet re-blessing it forces (the EMF
  metric-name count, the Groww guard tests, the alarm-wiring pins).

### 2026-08-22 — SPOT UNIVERSE = NSE INDICES + NIFTY TOTAL MARKET ONLY (supersedes the 2026-08-21 F&O-underlyings narrowing)

**The verbatim operator demand (2026-08-22, typed directly in-session — preserve EXACTLY, expletives and typos included):**

> "what the fick i clelayrl otld you o skip bse and aske dyout uckign pcik only nifty toal marjet stcoks and enitre nse indices aloen rigth mtoehrfucker then whyt he fuck stills truggligj motherufcke rhwy ? see emanhwiel why the fuck you didnt do the crsoss ebrificatione ntorley with nse india real nse idncies svcsv fiels downlaod becuase onl ywhen yo uhave thsoe aloen only then you cna do the cross verifictaion of each and eveyr data to preicsley fidn nifty total amrket symbsols repsectively rigtnh dude do yoir elaly udnerstand dude because if im not worn nse idnices woudl comeb anywhere between 120 and nifty total amrket would be aorudn 750 or 755 right dude then whyt he fuck still tehse are msisin dud ehwy if you dont do that then always you mtoehrukcxer wil lawlays tell me 4500 spots onl ywrist do thes emtoherucke rokay?"

This is the dated quote the rule-file-first law requires, recorded BEFORE the
code change it authorizes.

#### What was already true, and what was actually missing

The operator's two premises were checked against source before anything moved.
One was already satisfied; the other was not, and he is right that it was not.

| Premise | Verdict |
|---|---|
| "skip BSE" | **ALREADY DONE.** The spot path is NSE-only by construction — `nse_index_mappings` and `fno_underlying_mappings` both filter `row.exch_id != "NSE"`, and two tests pin it (`"NSE cash equities only: no BSE rows, no index legs"`, `"SENSEX is BSE — the operator narrowed to NSE alone"`). |
| "you didn't do the cross-verification with the real NSE India index CSVs" | **HALF TRUE, and the half that is false matters less than the half that is true.** The download and the ISIN join DO exist and DO run: `build_once` fetches all **49** `INDEX_CONSTITUENCY_SLUGS` from `niftyindices.com` — `ind_niftytotalmarket_list` among them — and joins every constituent to the Dhan master **by ISIN** through `join_constituents`, fail-closed at a 2% unresolved tolerance and a 10% failed-list ceiling. So the cross-verification is built and gated. |
| "then why do you still tell me 4,500 spots" | **THE REAL DEFECT.** The join's dedup key is `(index_name, security_id, segment)` — scoped PER LIST. The artifact therefore carries the **UNION of all 49 lists**, and `select_live_universe` dedupes that to the recorded **~4,565** SIDs. Nifty Total Market's ~750 rows are IN there, tagged `index_name = "Nifty Total Market"`, and **nothing ever selected them**. The data was resolved and then thrown into a pile. |

So the machinery he asked for exists; what never existed is a selector that
takes the ONE list he named out of the pile.

#### What this quote changes

| Surface | Before | After |
|---|---|---|
| Spot set, as configured | union of all 49 index lists (~4,565) | **NSE indices + Nifty Total Market constituents only (~869)** |
| 2026-08-21 narrowing (indices + F&O underlyings, ~335) | the only narrowing that existed; OFF | **SUPERSEDED as the default choice**, key retained and still honoured when NTM is off |
| BSE | excluded | excluded (unchanged) |
| Contract shape | NIFTY+BANKNIFTY full current-expiry chains, stock options ATM ±25, all futures expiries | **unchanged — this quote does not touch it** |

Precedence when both narrowing keys are on: **NTM wins**, because it is the
later dated instruction. Stated in the config and pinned by a test rather than
left to the order of two `if` blocks.

#### The arithmetic this fixes (the reason the operator kept hitting a wall)

| Spot set | Spot | Contracts authorized | Total vs the 25,000 cap |
|---|---|---|---|
| Union of 49 lists (today) | ~4,565 | ~23,820 | **~28,385 — OVER by ~3,385** |
| Indices + F&O underlyings (2026-08-21) | ~335 | ~23,820 | ~24,155 — fits, 845 spare |
| **Indices + NTM (this quote)** | **~869** | ~23,820 | **~24,689 — fits, ~311 spare** |

The margin is thinner than the F&O-underlyings option and that is stated
plainly: at the top of the measured index-chain range (2,037 legs for two
underlyings) the total breaches the cap and `plan_pool` refuses the WHOLE pool
fail-closed rather than truncating. That refusal is loud and is the correct
failure — but it is a session-ending one, so the headroom here is real and
small.

#### Honest envelope

- **~119 indices and ~750 NTM constituents are EXPECTED, not measured.** The
  index count comes from the master's NSE `INDEX` rows and the NTM count from
  whatever `ind_niftytotalmarket_list` serves on the day. Neither is a constant
  in this repository and neither can be verified from a dev container. The
  operator's own figures (~120 and ~750–755) match the expectation.
- **Nothing here makes the feed work.** It changes which instruments are asked
  for. A non-zero `compared` from the 15:31 cross-verification remains the only
  evidence this repository can offer that ticks arrive at all.
- **Fail-soft direction is UNCHANGED and deliberate:** an unreadable or
  unparseable NTM artifact falls THROUGH to the full master-sourced set with a
  coded error — the session subscribes MORE than was asked for, never less. A
  silent narrowing would drop ~3,700 instruments with nothing to say so.

#### What a PR that violates this section looks like (REJECT)

- Sources the NTM set from anything other than `ind_niftytotalmarket_list`
  joined to the Dhan master **by ISIN** (symbol-alone joins are banned by
  §31.1 of `daily-universe-scope-expansion-2026-05-27.md` — tickers are reused
  and renamed).
- Hardcodes an NTM constituent list (it rebalances; a hardcoded list goes
  stale and breaches the standing no-manual-intervention mandate).
- Re-admits BSE to the spot set.
- Lets an unreadable NTM artifact produce a SILENT fallback, or an empty spot
  set.
- Changes the contract shape (chain scope, ATM ±25, futures expiries) under
  cover of this quote — it says nothing about contracts.
- Flips `dry_run`, touches the §28 frozen area, or arms live order fire.

#### 2026-08-22 (SAME DAY) — MEASURED ON THE BOX: the "~4,565 spot" figure in the section above is WRONG, and so is its capacity table

The section above was written from source and from the figures this repository
has been repeating since 2026-08-12. Both were then checked against the live
box (`aws logs filter-log-events`, `/tickvault/prod/app`, session of
2026-08-21). The correction matters more than the change it corrects.

**The live spot set is 868. It has been 868. Nothing was ever subscribing 4,565.**

```
"live universe: widened from today's resolved master"
  instruments: 868   master_entries: 4636   deduped: 3771   capacity: 25000
```

`4,565` / `4,636` is the **artifact ROW count** — one row per
(index list, stock) membership pair, because `join_constituents` dedupes per
list. `select_live_universe` then dedupes on `(security_id, segment)` and
**3,771 of those rows collapse**. The union of 49 NSE index lists is ~749
distinct stocks, which is essentially Nifty Total Market's own membership,
because NTM is the superset the other broad lists are drawn from.

**What this does to the capacity table above.** Measured the same session:

| Line | This file said | Box says |
|---|---:|---:|
| Spot | ~4,565 | **868** |
| F&O underlyings with ladders | 216 | **208** |
| Index futures | ~6 | **18** |
| Stock futures | ~648 | **640** |
| Index options (current expiry) | 542–2,037 | **1,250** |
| Stock options at ATM ±25 | 22,032 | **20,220** |
| **Total subscribed** | ~28,385 (OVER by 3,385) | **22,996 — 2,004 spare** |
| ATM window actually applied | "would silently shrink" | **25, `dropped_for_capacity: 0`** |

So the capacity problem the section above was written to solve **did not
exist**. ±25 fits at full width today with 2,004 slots spare.

**What the NTM change therefore is, honestly.** Not a capacity fix — it
changes the subscribed count by roughly nothing (868 → ~869). What it does
buy is real but narrower: the set becomes **deliberately** NSE indices + Nifty
Total Market instead of **incidentally** the union of 49 lists that happens to
dedupe to the same membership. That removes a dependency on 48 lists nobody
selected, makes the set reproducible from one named source, and shrinks the
artifact from ~4,600 rows to ~869. The commit message and the section above
claim a ~3,700-instrument saving. **That claim is withdrawn.**

**How this file came to carry a wrong number for ten days.** The 4,565 figure
entered on 2026-08-12 from a log line, was copied into `config/base.toml`'s
comments, into `dhan_feed_stack`'s own reasoning ("~4,565 spot instruments
leave ~20,435 for contracts"), into `config.rs`'s doc comment, and into the
2026-08-21 narrowing rationale — each copy citing the last. Nobody re-read the
line it came from, which reports `instruments` and `master_entries` as
separate fields. A number that is only ever quoted is not evidence.

**Still true, and unaffected by the correction:** the NTM rows really were
being resolved and never selected; BSE really is excluded; the ISIN join
really does run fail-closed. Those were checked in source, not quoted.

### 2026-08-28 — CANDLES FROM 09:00, OHLCV ON THE RECEIPT CLOCK, PER-MINUTE ATM RE-FIT, AND THE PERCENTAGE COLUMNS

**The verbatim operator demands (2026-08-28, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

> "Yes fox and resolve everything always ensure to start eelvery candles starting at 9 am dude okay"

> "Meanwhile ensure to achieve this ohlcv based on one and only received at dude okay?"

> "See clealry ensure whenever the pre marketbope ticks and candles get finished ensure to provide this fucking atm plus minus depths also dude and starting 9.16 am every one minute resubscribe also right dude of current atm and what about pre oopem marketbpercentage change and even percentage change also dude okay?"

**Preceding demands from the same session (2026-08-27), which these reaffirm:**

> "still now nowhere I can see our entire ohlcv is nowhere captured based on revived at still it is looking at Ts soclelary ensure to use received at dude okay?"

> "see meanwhile at 3.40 pm we need to pull all the entire underlying spots data right that too one and only for one minute right to do the cross verification between these and our cnaldes 1 min right whether everything is entirely mat he'd right dud eam I right dude"

> "high low shoudl be on place base don current second and current minute to precisely capture day high day low"

This dated record is written BEFORE any code, per the rule-file-first law.

#### ⚠ The reaffirmation, recorded so the decision is auditable

The receipt-clock instruction was given, measured against, reported back with
contrary evidence, and then **reaffirmed**. The measurement is recorded here in
full because it constrains HOW the instruction must be implemented, not whether:

| Measured on production, 2026-08-27, NIFTY | Exchange clock | `received_at` clock |
|---|---|---|
| Session minutes present | 385 / 385 | **351 / 385** |
| Bars exactly matching the vendor's own tape | 382 (99.2%) | 321 (83.4%) |
| Phantom bars stamped outside market hours | 0 | **4** |
| Ticks filed on the WRONG DAY | 0 | **4,319** |
| **Ticks that would change minute on the LIVE path** | — | **0 of 83,871** |

The last row is the load-bearing one. On the live path the two clocks are not
merely close, they are **identical** — the exchange stamps whole seconds and we
receive inside the same second, so the minute is never in dispute. The clocks
diverge **only** on ticks replayed from the WAL, and there `received_at` carries
the moment of REPLAY rather than the moment of receipt.

**That is a DEFECT in how `received_at` is populated, not a property of the
clock.** The true receipt instant is captured correctly and early — `FrameSink::
accept` stamps it BEFORE the WAL append — and is then discarded at the WAL record
boundary, because the record format carries `ws_type`, `frame_seq` and the frame
bytes and nothing else. Replay therefore re-stamps with `now()`.

**So the instruction is implementable and, once the defect is repaired, the
receipt clock is strictly BETTER than today**: identical on the live path, correct
on the replay path where today's exchange-clock rows are merely lucky, and it
retires the entire late-arrival/refold policy because receipt is monotone per
drain. The operator's instruction stands and is adopted.

#### What is authorized

| # | Surface | From | To |
|---|---|---|---|
| 1 | WAL record format | `TVW2` (ws_type, frame_seq, frame) | **`TVW3`** — adds an 8-byte LE `received_at_nanos`. Replay restores it instead of re-stamping `now()`. `TVW1`/`TVW2` records replay with `0`, which the persistence layer already maps to NULL — never a lying timestamp |
| 2 | Candle bucketing clock | `exchange_timestamp` | **`received_at`**, for every timeframe |
| 3 | Candle session start | 09:15:00 | **09:00:00**, for every timeframe |
| 4 | ATM re-fit cadence | once at attach, top-up until 09:30 | **every minute from 09:16**, additive-only |
| 5 | Percentage columns | four columns, all hard-coded `0.0` | **computed** — `change_pct`, `close_pct_from_prev_day`, `open_pct`, `open_gap_pct` (the pre-open gap) |
| 6 | Cross-verification | already runs 15:41 over ~868 spots | **UNCHANGED in scope** — the operator's ask is already met; only the time budget is a real gap |

#### ⚠ What must NOT move, and why (getting this wrong breaks the only ground truth)

**`MARKET_OPEN_IST_NANOS` (09:15) STAYS.** It gates tick *persistence* and the
DAY OHLC tracker — not candles. Moving it would admit pre-open ticks into the day
high/low/close, which this file's own 2026-08-25 section forbids in as many
words: *"Admits pre-open ticks into day HIGH/LOW/CLOSE under cover of this quote
— it authorizes the OPEN only."* Only the trading-crate candle constant moves.
The compile-time cross-assert that currently binds the two must be **deliberately
severed with a comment**, never silently deleted, or the day gate follows the
candle gate and the 2026-08-25 rule is breached by accident.

**The cross-verification's 09:15 / 385-minute window STAYS.** Dhan's own tape has
no pre-open minutes. Widening it would report 09:00–09:14 as `missing_rest` on
every instrument every day and destroy the only ground truth the revived feed
has. Pre-open live rows must be EXCLUDED from the comparison, never counted as
missing.

#### ⚠ MEASURED: the per-minute ATM re-fit will almost always be a no-op

Recorded because the operator asked for a mechanism, and the mechanism should be
built knowing what it will actually do:

| Measured, 210 F&O underlyings, current expiry, 2026-08-27 | Value |
|---|---|
| Median strike spacing | **2.63% of price** |
| Average intraday drift from the open | 2.20% = **0.8 strikes** |
| Worst single underlying that day (15.86%) | **6.0 strikes** |
| Window half-width | **25 strikes** |
| Ladders where ±25 ALREADY takes every strike that exists | **81 of 210 (39%)** |

The worst stock on that day moved **6 strikes inside a 25-strike buffer**, and
for 39% of underlyings there is nowhere to re-centre to because the window
already covers the entire ladder. The re-fit is therefore built **additive-only
and delta-driven**: it recomputes every minute, and on a normal day it sends
nothing. It is not built as a swap.

**Additive-only is not a preference, it is the only safe shape today**, for four
independent reasons: the transport has no `send_unsubscribe`; `SubscribeGuard`
has no removal API and its instrument set IS the reconnect replay, so an
unsubscribed instrument silently returns on the next reconnect; unsubscribe
acknowledgement is undocumented; and error 805 is a DISCONNECT, so doubling wire
traffic to reclaim slots trades bounded memory for tick loss — the wrong
direction against the operator's own no-tick-loss mandate.

**What bounds it, stated plainly:** ~2,000 spare aggregator slots against ~23,000
already subscribed. A drift of one strike per underlying per minute would exhaust
that in ~10 minutes. The measured drift is 0.8 strikes per DAY, so the headroom is
ample in practice — but the re-fit must carry a per-minute cap and must fail
CLOSED and LOUD at the ceiling: it stops adding, counts the refusal, and reports.
It must never silently narrow the window.

#### ⚠ What the pre-open candles will actually contain

Not a uniform 15 extra minutes. A bucket exists only if a tick opens it; there is
no synthetic fill and no carry-forward. Measured on 2026-08-27:

- **~120 indices** tick continuously from 09:00 → full coverage across all frames.
- **~750 equities** deliver one stale snapshot at ~08:30 (outside the window, so
  rejected) and then **nothing until the 09:07 auction print** → the 09:00–09:06
  buckets produce **no rows at all**, one bar at 09:07, then nothing until 09:15.

Any consumer assuming "N bars per session" must treat pre-open absence as NORMAL,
not as a gap. `tf_consistency` and the cross-verify comparator both make that
assumption today and must be taught the difference.

#### What a PR that violates this section looks like (REJECT)

- Moves `MARKET_OPEN_IST_NANOS`, the day-OHLC gate, or the cross-verify window to
  09:00 (breaches the 2026-08-25 rule and blinds the only ground truth).
- Deletes rather than deliberately severs the compile-time assert binding the
  candle gate to the persistence gate.
- Switches the bucketing clock WITHOUT the `TVW3` receipt-preserving WAL record —
  that is the combination measured to lose 8.8% of a session.
- Implements the ATM re-fit as a SWAP, or wires unsubscribe, without first
  building `send_unsubscribe`, a `SubscribeGuard` removal API, and aggregator slot
  release with a full cell reset (a reused slot inheriting a foreign volume
  baseline is the documented 9.2×-volume corruption class).
- Lets the re-fit run on the socket reader task, or sends full sets rather than
  deltas, or omits the 25 ms inter-batch pacing.
- Computes a percentage change without guarding a zero or non-finite previous
  close (this codebase has already been bitten by a NaN poisoning indicator state
  for the life of the process).
- Reports a pre-open minute with no ticks as a missing bar rather than as normal.

### 2026-09-06 — DEPTH IS STOCK OPTIONS ONLY, RANKED BY VOLUME, WITH THE TWO OPTION FAMILIES RANKED SEPARATELY

**The verbatim operator demands (2026-09-06, typed directly in-session — preserve
EXACTLY, typos included):**

**Quote A (the requirement):**
> "Dude clealry note for depth 20 check every 5 seconds top volume of stocks options strikes contracts alone dude I mean pick top 250 top volume aligned wirh top gainers dude okay? For depth 200 also always pick top 5 volume gainers stocks options strikes contracts dude but ensure on depth 200 top 5 should never ever be same symbols strike ddue okay?"

**Quote B (the narrowing, minutes later):**
> "See clelwry note for depth 20 and depth 200 only stocks options contracts strikes dude oaky? No underlying spot or futures or indices or indices fmo dude okay? Meanwhile we need to split this top volume gainers purely based on indices options vs stocks options dude okay?"

**Quote C (the authorization):**
> "See whatever I mentioned go ahead with that dude okay?"

Quote C was given in DIRECT response to a message that ENUMERATED exactly what was
blocked and why — removing NIFTY/BANKNIFTY from depth-20, the depth-200 change, and
the SEBI deletion — so it selects the enumerated work. That is the §28.2/§28.3
authorization shape this repository already accepts. Recorded HERE before any code,
per the rule-file-first law.

#### What this SUPERSEDES

This is a reversal of the 2026-08-26 (SECOND) depth-200 lock and of the index half
of the depth-20 layout the same day authorized. Both are recorded rather than
quietly overwritten:

| Surface | 2026-08-26 locked value | 2026-09-06 |
|---|---|---|
| depth-20 socket 1 | NIFTY ATM ±12, CE + PE (50 slots) | **stock options** |
| depth-20 socket 2 | BANKNIFTY ATM ±12, CE + PE (50 slots) | **stock options** |
| depth-20 sockets 3–5 | 37 gainers + 37 losers + 1 = 75 stocks, ATM CE/PE, ranked by PERCENT CHANGE | same shape, **ranked by VOLUME**, widened to fill 250 |
| depth-200 | NIFTY ATM CE/PE + BANKNIFTY ATM CE/PE + 1 lone mover | **top 5 stock-option contracts, distinct underlyings** |
| Ranking key | `close_pct_from_prev_day` | **cumulative day volume**, gainers as an eligibility filter |
| Cadence | once a minute at :08, edge-triggered on an ATM change | **rank every 5 s**; re-subscribe still edge-triggered (see the envelope) |

The 2026-08-26 REJECT row *"Lets any underlying outside NIFTY/BANKNIFTY take a
socket while a NIFTY or BANKNIFTY ATM pair is available"* is **RETIRED by Quote B**,
which excludes those underlyings from depth entirely. Its sibling rows — the socket
budget (5 remains 5), no hardcoded contract ids, and normalised rather than raw
cross-underlying comparison — all STAND.

#### The contract (LOCKED)

| Aspect | Locked value |
|---|---|
| Instrument class | **stock options only** (`OPTSTK`). NO spot, NO futures (stock or index), NO indices, NO index options. Quote B is explicit on all four |
| Segment | `NSE_FNO` only. **SENSEX and BANKEX can never have depth** — they are `BSE_FNO` and Dhan serves depth on NSE alone; the existing refusal stands unchanged |
| depth-20 | **250 instruments** = 5 sockets × 50, the highest-volume stock-option contracts |
| depth-200 | **5 instruments** = 5 sockets × 1, the highest-volume stock-option contracts, **each a distinct underlying** |
| Budget | UNCHANGED — 250 + 5. This changes WHICH, never how many |
| Ranking key | **cumulative day volume**, read at byte offset 22 (`QUOTE_OFFSET_VOLUME`, const-asserted; the field is shared by the Quote AND Full layouts, not Full-only, and is ABSENT — reading 0 — in Ticker mode). ✅ **PROVEN CUMULATIVE 2026-09-09, live.** This row previously read "INFERRED, not proven … the single most load-bearing unproven input in the design", and called running the Track 2 SELECT "the cheapest high-value verification available". It was run: **9,879,724 ticks across 8,675 instruments in one session, 157 monotonicity violations = 0.0016%** (`.claude/plans/research/track-2-result-2026-09-09.md`; the runbook `docs/operator/track-2-monotonicity-select.md` carries the resolved banner). A per-packet delta field would fall on roughly half of all ticks, not on one in 630,000. The 157 are arrival artefacts — intra-second transposition and the documented slow-consumer skip — and are exactly what `VolumeLeaderboard`'s monotonicity gate refuses; the measurement now SIZES that gate's workload rather than leaving it hypothetical. The FALSE citation this row once carried ("Dhan Ticket #5525125", which is about PrevClose routing and contains zero volume facts) stays retired. Also measured the same run: **IDX_I carries `volume = 0` on every one of 866,796 index ticks**, so an index can never be ranked by volume and only its option contracts can — a property the design already relies on and had never checked. |
| Gainer role | **eligibility filter, not the sort key.** An instrument qualifies if its underlying is in the day's gainers; volume then decides the order. This keeps the ordered set monotonic, and therefore stable |
| **Family split** | index options and stock options are ranked in **SEPARATE leaderboards**, never one blended list |
| Ranking cadence | every **5 seconds** |
| Re-subscribe cadence | **delta-only, edge-triggered, capped per window** — NOT a 5-second full re-subscribe (see the envelope) |
| Contract source | the daily master artifact. Hardcoding contract ids remains a REJECT — they expire |

#### Why the family split is mandatory, not stylistic

Quote B asks for the split and it is the load-bearing requirement, not a
refinement. Measured on the box 2026-08-22: **1,250 index-option contracts against
20,220 stock-option contracts**, and a single NIFTY weekly at-the-money strike
out-trades stock-option strikes by orders of magnitude.

**A single blended top-250 by raw volume returns 250 index strikes and zero stock
options.** Not approximately — the stock options this lock exists to capture would
never appear, and the selector would silently return the exact opposite of the
requirement while every counter read green. The split is what makes a stock-option
ranking exist at all.

This is the same reasoning the 2026-08-26 lock already applies one level up, where
cross-underlying ranking is normalised rather than raw rupees because a 24,000 index
and a 57,000 index are not comparable. Volume across the two option families is that
problem an order of magnitude worse.

#### ⚠ The honest envelope (mandatory per operator-charter §F)

**Three defects must ship WITH this change or the ranking is unsafe.** All three were
found by the 2026-09-06 five-agent sweep and all three are live today:

1. **`ParsedTick.volume` is `u32` with NO overflow guard on the Dhan path.** This
   file's own TrueData section records that a liquid index/future day *exceeds*
   `u32::MAX`. On wrap, cumulative volume falls from ~4.29e9 to near zero and the
   most liquid contract silently leaves the leaderboard. A saturating read with a
   refusal counter is REQUIRED.
2. **`VOLUME-MONO-01` has an error code but its `volume_monotonicity_guard` module
   was DELETED.** Cumulative volume is NOT monotonic in practice — WAL replay
   re-injects older frames after newer ones, Dhan skips a slow consumer forward with
   no sequence number, and the counter resets at 09:00. **One garbage value near the
   ceiling pins the leaderboard threshold at a value nothing can ever beat, the depth
   set freezes for the session, and every counter reads green.** A falling volume must
   be refused, counted and logged, and must never move the threshold.
3. **Percent gain divides by the previous close, which is a PROVEN NaN source** —
   the quote parser carries a test that asserts it — and `0.0` is a live sentinel. A
   non-finite comparator is non-transitive, so a heap ordered by it corrupts wholesale
   rather than in one entry. This is the §28.4 poisoning class one module over; a
   finite gate on the ingest side is REQUIRED.

**NOT claimed — the thin-book risk, which is the operator's to accept.** NIFTY and
BANKNIFTY held the depth-200 sockets because they are the only books deep enough to
fill 200 levels. The 2026-08-26 incident measured two FINNIFTY strikes delivering
**800 rows/minute against NIFTY's 100,800** — a 125× difference — and the idle
watchdog read the sparse sockets as dead and redialled them **112 and 210 times**
against 19 for the healthy ones. **Stock-option books are thinner than FINNIFTY's.**
Putting all five 200-level sockets on stock options is expected to produce
mostly-empty books and reconnect churn. That is a measurement, not an opinion; it is
recorded here so the outcome is a decision rather than a surprise, and it is
reversible by a fresh dated quote.

**NOT claimed — that a 5-second full RE-SEND is possible.** (⚠ REFRAMED by the FOURTH quote below: the operator's requirement is that the SUBSCRIBED SET be current every 5 seconds, and delta-only delivers exactly that. This row states the refusal badly; read it with the reframe.) Swaps are one-for-one with
no bulk API, each carries a 2 s wire budget on the drain task, and 250 swaps
serialise to **up to 500 s against a 5-second window**. At 5 s the session runs
**4,680** cycles (09:00–15:30 = 23,400 s), the per-socket command channel is depth 4,
Dhan closes a socket silent for 40 s, error 804 parks a socket for the session, and 805
is "Too many requests/connections — may result in user being blocked" (Dhan's own
wording, `docs/dhan-ref/08-annexure-enums.md:348` — *may*, which this line previously
asserted flatly as "an account block"). **The RANKING runs every 5 seconds (⚠ CORRECTED: this said "measured ~70 µs,
0.0014%". That was the `scan_silence` constant borrowed from a LINEAR scan and
applied to a `sort_unstable_by` — an invalid transfer, and it was the number used to
argue the heap design away. ⚠ CORRECTED AGAIN 2026-09-12: the replacement figure "900 µs, a 0.018% duty cycle" was ALSO not a measurement — that harness was timing an EMPTY sort under a constant lot stub. Re-measured with the harness repaired and asserting a filled board: 2.95 ms at the ceiling where every contract traded, 123 µs at the assumed realistic shape. The CONCLUSION is unchanged twice over — even 2.95 ms is a 0.295% duty at 1 s and the heap's threshold-poisoning is still catastrophic — but the number was first understated 15x, then overstated 3.3x, and is now real); the SUBSCRIPTION moves only the delta, edge-triggered and capped per
window**, which is the shape the existing re-fit already uses and for these reasons.

**NOT claimed — that this improves capture.** 2026-09-04 captured ZERO ticks and
dropped 2,000,238 frames before the write-ahead log because the volume was full. That
sits upstream of every word here.

#### ⚠ What this quote does NOT authorize

- **Any deletion of SEBI or audit rows.** `instrument_lifecycle`,
  `instrument_lifecycle_audit`, `index_constituency`, `order_audit`,
  `order_update_events`, `position_update_events` and `ws_event_audit` are NEVER
  deleted. A general "go ahead" is precisely the shape §5-class REJECT lists name as
  insufficient — *"even if the operator approves verbally"* — and the retention is a
  five-year regulatory obligation that cannot be rebuilt. Authorizing it needs its own
  dated quote naming those tables.
- Any change to the socket or instrument budget (250 + 5 remain).
- Any fifth Dhan endpoint type, or more than 16 total connections.
- Live order fire; `dry_run` stays true.
- Any edit to the §28 frozen indicator/strategy area.
- Depth on `BSE_FNO` — structurally impossible, unchanged.


#### 2026-09-06 (FOURTH quote, same day) — the reaffirmation, and the one thing it changes

**The verbatim operator demand (preserve EXACTLY, typos included):**

> "See for depth 20 and depth 200 never ever use the index options dude see every 5 seconds it shoudl he reussbribed to this stocks options strikes contracts alone only that too top volume gainers dude see for depth 20 pick top 250 but for depth 200 always ensure to to have top 5 as different symbols dude okay? If same symbols multiple strikes contracts mejas then have it as unique dude okay?"

Three of its four clauses restate the contract above **exactly** and change nothing:
index options never reach depth, depth-20 takes 250, depth-200 takes 5 distinct
underlyings. They are recorded because a third statement of the same requirement is
evidence about what matters to the operator, not noise — and because the last clause
sharpens the depth-200 rule into a form worth pinning by test (below).

**The fourth clause — "every 5 seconds it should be resubscribed" — is the second
time this has been asked for, and the envelope above framed the answer badly.**

##### The reframe: delta-only IS the 5-second resubscribe

The envelope reads *"NOT claimed — that a 5-second RE-SUBSCRIBE is possible"*, which
states a refusal. That is the wrong frame and it is corrected here. What the operator
is asking for is a PROPERTY of the subscribed set:

> at every 5-second boundary, the 250 instruments Dhan is streaming ARE the current
> top 250 by volume.

**Delta-only delivers exactly that property.** It is not a reduced version of the
requirement — it is the only implementation of it that survives contact with the
vendor, for a reason that is Dhan's, not ours:

| | |
|---|---|
| What a literal full re-send does | re-subscribes instruments the socket already holds |
| What we expect Dhan to answer | **804 — "Requested number of instruments exceeds limit"** (`docs/dhan-ref/08-annexure-enums.md:347`). ⚠ **The doc supports the WORDING of 804 only. That a DUPLICATE subscribe triggers it is THIS REPOSITORY'S INFERENCE** — nothing in `docs/dhan-ref/` documents duplicate-counting — and it is repeated in ~8 code comments, which is how an inference starts reading like a vendor fact. The reasoning: a duplicate counts again against the per-connection cap, so re-sending 50 held instruments to a 50-slot socket asks for 100. Sound for depth-200 (cap 1, so 2 > 1 regardless); UNPROVEN for depth-20, where it holds only if Dhan adds rather than de-duplicates |
| What 804 costs | `classify_disconnect` files it **Fatal**: the socket closes and does not re-dial. `SubscribeGuard` replays the same retained set, so a re-dial earns the identical rejection — deterministic, for the session |
| What the churn itself risks | **805 — "Too many requests/connections — may result in user being blocked"** (same table, line 348). *May*, in Dhan's own word — not a certainty, and not something to test against a live account |

So a literal 5-second full re-send does not deliver the requirement more faithfully;
it **destroys the sockets on the first cycle** and the set becomes permanently stale
at whatever it held. The delta is what keeps the property true.

**What is unchanged and what tightens:**

| | |
|---|---|
| Ranking cadence | every **5 seconds** — unchanged, and it is free (~60 µs, a 0.0012% duty cycle) |
| Set freshness | the subscribed set reflects the ranking **as of the last 5-second sweep** — this is the operator's requirement, met |
| Wire traffic | only instruments that ENTERED or LEFT the top 250. A quiet window costs **zero** socket actions |
| Per-window cap | a bounded number of swaps per window, with the refusal counted — so a churning market cannot serialise 250 two-second swaps into a 5-second window |

##### The depth-200 clause, pinned rather than assumed

*"If same symbols multiple strikes contracts means then have it as unique"* is the
precise statement of the distinct-underlying rule: when the top five by volume
contain several strikes of ONE stock, keep that stock's heaviest contract and take
the next DISTINCT underlying for the remaining sockets. Greedy over the volume order
does exactly this, and `rank_distinct_underlying` implements it — but the operator
has now stated the multi-strike case explicitly, so it is pinned by its own named
test rather than left as a property of the general case.

##### ⚠ The blocking prerequisite nobody had named

Raising the cadence multiplies one existing, UNVERIFIED risk by 12x, and it must be
probed before any cadence increase ships.

`docs/dhan-ref/08-annexure-enums.md` records the depth UNSUBSCRIBE RequestCode as a
cross-surface split — **24 vs 25** — with the weight of evidence "SHIFTED toward 24"
and the conclusion **"UNVERIFIED-LIVE both ways"**. `constants.rs:397` ships
`FEED_UNSUBSCRIBE_TWENTY_DEPTH = 25`. *(⚠ SUPERSEDED 2026-09-10 — the constant is now 24; the live session proved 25 ignored. See "2026-09-10 — THE DEPTH UNSUBSCRIBE REQUESTCODE IS SETTLED LIVE".)*

If 25 is the wrong code, every unsubscribe is a silent no-op at Dhan's side and every
swap becomes an ADD. A depth-200 socket then reaches 2 instruments against a cap of 1
— 804, Fatal, parked for the session. `send_unsubscribe` is fire-and-forget
(`connection.rs:1131`): `Ok` means bytes were written, not that Dhan removed anything,
so there is no ack to detect it from. Today that risk fires at most 5 times a minute;
at a 5-second cadence it fires twelve times more often.

**So the ordering is: probe the unsubscribe code on a live session FIRST, then raise
the cadence.** Shipping the cadence first is the one change here that could park all
five depth-200 sockets for a session with no recovery path.

**The thin-book cost is unchanged and still stated:** when the true top five ARE five
strikes of one stock, this forces four substitutions into thinner books — the shape
that measured 800 rows/minute against 100,800 on 2026-08-26. That is the operator's
call and he has now made it twice.
#### What a PR that violates this section looks like (REJECT)

- Ranks index options and stock options in ONE blended leaderboard (returns zero
  stock options — the defect this lock exists to prevent).
- Subscribes any spot, future, index or index option to a depth socket.
- Ships the volume ranking without the saturating read, the monotonicity gate, or the
  finite gate on percent gain.
- Re-subscribes unconditionally every 5 seconds rather than moving the delta.
- Sorts by percent gain rather than using it as an eligibility filter (breaks
  monotonicity and therefore stability).
- Hardcodes contract security-ids.
- Reports a depth pool as enabled while its instrument set is empty — including the
  pre-open case, where volume is zero for everything and the ranking is meaningless.
- Deletes a SEBI or audit row under cover of this quote.

### 2026-09-07 — THE RANKING KEY BECOMES LOTS TRADED IN THE WINDOW, NOT UNITS TRADED SINCE OPEN

**The verbatim operator demand (2026-09-07, typed directly in-session — preserve
EXACTLY, typos included):**

> "see i decided to put our sort by volume liek this dude see based on lot quantity size it hsodul be chekced rigth suppose lets say per lot quantity is 200 and every second if it is having somwhwre or somehtign like 20k volume means and even per 5 seocnd also if the veolume is 25k means then you need to chekc the quantiyt to volume difference rigth then you will get the precise voluem percentage rigth dude so that eaisly we can sort it out right do youu dnerstand what im even aksing dude?"

**The authorization (2026-09-07, same session, in DIRECT response to a message
that enumerated this work, named the rule-file row it changes, and said it needed
his word first):**

> "Bro fix and resolve everything dude okay?"

That is the §28.2/§28.3 authorization shape this repository already accepts — a
general go-ahead answering an ENUMERATED ask selects the enumerated work.
Recorded HERE before the code, per the rule-file-first law.

#### What this SUPERSEDES

The 2026-09-06 contract table row:

| Aspect | 2026-09-06 locked value | 2026-09-07 |
|---|---|---|
| Ranking key | **cumulative day volume** | **lots traded IN THE WINDOW** = `(units traded since the last snapshot of this cadence × 1000) ÷ lot size` |
| Unit | raw exchange units | milli-lots (integer, 3 decimal places of a lot) |
| Horizon | since 09:15 | the 1s or 5s window that just closed |

Everything else in that section STANDS unchanged and is not re-litigated here:
stock options only, `NSE_FNO` only, 250 depth-20 + 5 depth-200, the two option
families ranked in SEPARATE leaderboards, gainers as an eligibility FILTER and
never the sort key, distinct underlyings on depth-200, delta-only edge-triggered
re-subscribe, no hardcoded contract ids.

#### The operator's own arithmetic, which is the specification

His numbers define it exactly:

| His words | Meaning |
|---|---|
| "per lot quantity is 200" | lot size = 200 units |
| "every second ... 20k volume" | 20,000 units traded in that 1-second window |
| "per 5 second also ... 25k" | 25,000 units traded in that 5-second window |
| "check the quantiyt to volume difference" | divide volume by lot quantity |
| "you will get the precise voluem percentage" | 20,000 ÷ 200 = **100 lots**; 25,000 ÷ 200 = **125 lots** |
| "so that eaisly we can sort it out" | those figures are the sort key |

Both halves matter and both are changes:

1. **NORMALISE by lot size.** Without it the board compares a 15-unit lot
   against a 1,800-unit lot and the big-lot contract wins on units while
   trading fewer actual contracts. Ranking is supposed to find the BUSIEST
   book, and units are not comparable across contracts; lots are.
2. **Measure the WINDOW, not the day.** His figures are explicitly per-second
   and per-5-seconds. Cumulative-since-open answers "who has been busy today",
   which by mid-afternoon is a fact about the morning. The depth sockets should
   sit on what is busy NOW.

#### The contract (LOCKED)

| Aspect | Locked value |
|---|---|
| Sort key | `lots_milli = (delta_units as u64 * 1000) / lot_size as u64` — integer, no float anywhere on the path |
| `delta_units` | `stored_cumulative - baseline[cadence]`, saturating; a contract whose cumulative FELL is refused by the existing monotonicity gate before this is ever computed |
| Baseline | per contract, PER CADENCE — the 1s and 5s windows are independent and each keeps its own; updated in the same pass that reads it |
| First window after a contract is first tracked | baseline is seeded to the contract's CURRENT cumulative, so its first delta is **0** and it ranks nothing until it trades inside a real window |
| Lot size | from `ContractOwner.lot_size`, guaranteed non-zero by `LegRefusal::MissingLotSize` |
| Tie-break | unchanged — `security_id` then `segment`, so the order is total and stable |
| Scale | `× 1000` (milli-lots). Integer division alone would collapse every contract trading under one lot in a 1-second window to 0 and make the bottom of a 250-deep board an arbitrary tie |
| Overflow | `u32::MAX × 1000 = 4.295e12`, inside `u64`. Const-asserted, never `as` |
| Cumulative pipeline | **UNCHANGED** — the monotonicity gate, `RELATCH_AFTER_CONSECUTIVE_LOWER`, the capacity cap and the refusal counters all stay exactly as they are. They guard the INPUT; this changes only the key derived from it |

#### ⚠ The honest cost, which is a real behaviour change and not a detail

**The board stops being monotonic, and that is the point of the change rather
than a defect in it.** Today's key only ever rises, so the top-250 set is
stable by construction and the delta-only re-subscribe sends almost nothing. A
per-window rate rises AND falls, so the set will genuinely churn more, and every
entry and exit is a depth swap on a socket.

Three things bound that, and none of them is new machinery:

* The re-subscribe is already **edge-triggered and delta-only** — a contract
  that stays in the set costs nothing, however its rank moves inside the set.
* It is already **capped per window**, with the refusal counted, so a violently
  churning market cannot serialise 250 two-second swaps into a 5-second window.
* The 5-second board is inherently steadier than the 1-second one, and it is
  the 5s board that has the larger sample.

**NOT claimed:** that the churn is small. Nobody has measured it, because no
session has yet ranked on this key. `tv_depth_swaps_total` against the existing
budget-refusal counter is the measurement, and the first live session is when it
exists. If the swap budget is hit routinely, the answer is a longer window or a
hysteresis band on entry/exit — NOT reverting to cumulative, which answers a
different question.

**NOT claimed:** that a 1-second window is statistically meaningful for a thin
stock option. Many will trade zero units in any given second and rank 0, which
is correct and not a bug; the 5s board is where a thin book gets a fair reading.

#### What a PR that violates this section looks like (REJECT)

- Ranks on raw units without dividing by lot size (the defect this fixes).
- Uses a float anywhere in the sort key — a non-finite comparator is
  non-transitive and corrupts a sort wholesale, which this file already records
  once.
- Defaults a missing lot size to 1 rather than refusing the contract.
- Seeds a newly-tracked contract's baseline to 0, which makes its first window
  report the WHOLE DAY and hands it a depth socket it did not earn.
- Shares one baseline between the 1s and 5s cadences — each window must measure
  its own interval.
- Weakens or removes the monotonicity gate, the re-latch, or the capacity cap
  because "the key is a delta now". They guard the cumulative INPUT and are
  still exactly as load-bearing.
- Re-subscribes unconditionally rather than on the delta, or removes the
  per-window swap cap, on the grounds that the set churns more.

### 2026-09-08 — DEPTH-200 RANKED STEERING WIRED; depth-20 NOT, and the dial still puts index options on the wire

**No new authorization is claimed.** This is the record of the 2026-09-06 and
2026-09-07 sections above being ACTED ON, and of exactly how far. PR #1890
built the lots-in-window ranking and published its top five distinct
underlyings (`depth200_candidates`), then only LOGGED the divergence between
that ranking and what the five depth-200 sockets held — every socket kept
following the at-the-money engine the lock bans. Recorded here because the
`REJECT` list above says re-subscribing unconditionally is a violation, and the
next reader needs to know which half of the lock is on the wire and which is
not.

#### What is now on the wire (`crates/app/src/depth200_ranked_steer.rs`)

| Property the lock makes binding | Implementation |
|---|---|
| Delta-only | a socket already holding a ranked contract is never touched; only sockets holding something OFF the ranking swap, and only onto contracts held nowhere |
| Edge-triggered | a minute where holdings and ranking agree costs zero wire calls |
| Capped per window | `MAX_RANKED_SWAPS_PER_MINUTE` = 5, and never more than one swap per socket per minute (the reconcile-before-plan discipline the loop already had); refusals counted as `tv_depth200_ranked_swaps_total{outcome="capped"}` |
| Distinct underlyings | inherited from the published ranking (`distinct_underlying_over`), not re-derived at a second site |
| I-P1-11 | held-vs-ranked comparison on the `(security_id, segment)` composite; a held index option never reads as "already holding" the stock option that shares its number (pinned by test) |

The at-the-money engine is consulted only until the FIRST ranking of the
session is published; from then on it is bypassed for the rest of the session.
`Some(empty)` (the ranking ran and selected nothing) moves nothing — it is not a
fallback to the banned engine.

#### ⚠ What is NOT delivered (Rule 11)

1. ~~**depth-20 still runs the 2026-08-26 layout**~~ **DELIVERED later the same
   day — see "2026-09-08 (SECOND)" below.** The ranking layer publishes the
   gainer-eligible top set (up to 300 rows) for depth-20 and
   `depth20_ranked_steer::plan_depth20_ranked_minute` steers the five
   50-instrument sockets from it with a per-socket cap; the 2026-08-26 layout
   survives only as the pre-first-ranking fallback. The strikethrough is kept
   because this item was true when the section above it was written that
   morning.
2. **The boot dial still selects index at-the-money contracts for depth-200**
   (`select_depth_universe`). For the first minute(s) of a session, until the
   drain's first 5-second ranking exists, the five sockets carry the banned
   class. Pre-open there is no volume to rank on, so this is the honest
   starting state — but it means the lock is met from ~09:16, not 09:00.
3. **The apply cadence is one minute, not five seconds.** The ranking is
   recomputed every 5 s on the drain; the steering loop applies the latest
   ranking once a minute at :08. Applying every 5 s would put swap I/O on the
   frame drain, which this same lock forbids. The subscribed set therefore
   reflects the ranking as of the last sweep before the minute mark.
4. **The unsubscribe RequestCode (24 vs 25) is still UNVERIFIED-LIVE**, exactly
   as the FOURTH-quote section records. *(⚠ RESOLVED 2026-09-10: 25 was proven IGNORED on the wire and 24 now ships — see the 2026-09-10 section.)* Ranked steering does not raise the
   swap cadence — it is still at most one swap per socket per minute — so it
   multiplies that risk by nothing, but it does not retire it either.
5. **Churn is UNMEASURED.** `tv_depth200_ranked_swaps_total{outcome}` is the
   measurement; the first live session is when it exists. A routinely non-zero
   `capped` count is the signal the 2026-09-07 section names for a longer
   window or a hysteresis band — never for reverting the key.
6. **A socket is never emptied.** With fewer than five ranked contracts, a
   socket holding an off-ranking contract keeps it rather than being
   unsubscribed to nothing (an empty depth socket delivers nothing, and the
   unsubscribe code is unverified). A socket that never subscribed (`None`)
   cannot take a swap and is counted `socket_empty`.

#### What a PR that violates this section looks like (REJECT)

- Applies the ranking every 5 s from the frame drain (swap I/O on the hot path).
- Removes the per-minute cap, or lets one socket take two swaps in a minute.
- Falls back to the at-the-money engine after a ranking has been published.
- Empties a socket to "match" a short ranking.
- Claims depth-20 is on the volume ranking without the 2026-09-08 (SECOND) contract below — entry at 250, exit at 300, gainer filter before the cut, zero-lot rows excluded.

### 2026-09-08 (SECOND, same day) — DEPTH-20 IS NOW RANKED TOO, and what the wiring audit found on the way

**No new authorization is claimed.** This is the record of item 1 of the section
above being delivered, plus the defects an adversarial audit of the ranking
pipeline found while it was wired — each fixed in the same change and stated
here because the next reader will otherwise find the fixed shape and wonder
what the old one was.

#### What is now on the wire (`crates/app/src/depth20_ranked_steer.rs`)

| Property the lock makes binding | Implementation |
|---|---|
| Stock options only | the published list is the STOCK family's gainer-eligible top set; index options never reach it |
| Top 250 | entries are taken from the first `DEPTH20_ENTRY_RANKS` = 250 by lots-in-window |
| Delta-only, edge-triggered | a held contract inside the published list is never touched; only ranked-but-unheld contracts arrive, and only into slots freed by held-but-off-list departures |
| Capped per window | at most `DEPTH_SWAP_COMMAND_CHANNEL_DEPTH` (4) swaps per socket per minute; the rest is retried next minute and counted |
| Hysteresis | `DEPTH20_EXIT_RANKS` = 300: a held contract that slips to rank 251–300 is KEPT. The 2026-09-07 section names a hysteresis band as the remedy for a churning per-window board; this is it, at 50 ranks |
| Composite key | held-vs-ranked comparison on `(security_id, segment)`; the first draft compared on the id alone and the test that pins it fails on that draft |
| Gainer filter BEFORE the cut | the filter runs over the FULL volume-ordered population and stops once 300 gainers are collected. The first draft cut to 250 and THEN filtered, so a gainer ranked 251st by volume could never reach depth while a non-gainer above it consumed the slot |
| Zero-lot rows excluded | a contract with 0 lots in the window is not on the board. Before this the board was PADDED with every seeded contract at key 0 in `security_id` order — a depth set of "whichever low ids ticked first", which is the arbitrary tie the 2026-09-06 section forbids |
| Replay is not trading | a WAL replay re-seeds every window baseline (`rebaseline_all`) after the refold, so a 2M-frame backlog cannot read as one window's volume and hand out 250 sockets at the bell |
| Previous close from the packet | the gainer verdict needs the stock's previous close; the code-6 packet is one door, and since today the `day_close` field of a Full/Quote packet on `NSE_EQ` is the second (first write wins). Without it every verdict was `Unknown` on any morning the code-6 packet did not arrive |
| All-unknown latch | if every gainer verdict is `Unknown` on a non-empty board, one coded `error!` (`source = "gainer_verdicts_all_unknown"`, log-sink only) says so once per session, instead of two green depth pools holding the boot dial all day |
| Post-close gate | after 15:40 IST the steering loop plans nothing: the frozen last ranking was being re-planned every minute until the box stopped |
| No-ranking detection | at 09:20 IST a steering view that is still `None` logs one coded `error!` (`source = "no_ranking_by_0920"`, log-sink only) — the 2026-09-08 morning's thirty silent minutes would have been one line at 09:20 |
| Refused unsubscribe | `SubscribeGuard::undo_swap` puts the OLD instrument back when the wire REFUSES the unsubscribe (`Ok(Err)`), so the guard never names a strike the socket does not carry; a TIMEOUT is deliberately not reverted (the frame may have landed, and the redial must replay the chosen strike) |

The DHAT gate on the ingest seam now publishes a contract map before it
measures, so the per-tick `observe` runs INSIDE the measured window; until
today the gate measured a seam that skipped the ranking board, which is the
vacuous-pass shape closed in #1884 arriving one structure later.

#### ⚠ What is STILL not delivered (Rule 11)

1. **The boot dial still puts index at-the-money contracts on the depth sockets
   until the first ranking (~09:15–09:16).** Seeding the dial from the previous
   session's persisted `top_volume_rank` is designed but NOT built: yesterday's
   contract ids can be expired on an expiry rollover, so the seed must be
   validated against today's attached contract set, and the boot-time QuestDB
   read has the WAL-apply-lag exposure `SpotPriceStore` was built to escape.
   Item 2 of the section above stands.
2. **The unsubscribe RequestCode is still UNVERIFIED-LIVE.** The operator's
   Dhan pack (uploaded 2026-09-08) reads `25 = Unsubscribe — Full Market Depth`,
   which is what ships; the 24-vs-25 split recorded in the annexure is not
   retired by a document, only by a live probe. *(⚠ The live probe happened on 2026-09-10: 25 was ignored, 24 ships — see the 2026-09-10 section.)*
3. **Depth-200 has no hysteresis band.** Its planner is one contract per socket
   and swaps whenever the top five distinct underlyings change; churn is
   measured by `tv_depth200_ranked_swaps_total`, not bounded by a band.
4. **A frame arriving for an instrument the guard does not hold is not
   detected.** If the unsubscribe silently fails at Dhan's side, the socket
   keeps delivering the OLD contract and nothing counts it. Detecting it needs
   a per-socket held-set lookup on the depth decode path, and that is a hot-path
   change with its own DHAT gate — recorded, not smuggled in.
5. **The per-minute steering applies a ranking that is one 5-second window
   old.** That is the cadence the operator asked for; on a thin option it means
   a contract can leave the board because it did not trade in that one window.
   The exit band is what keeps that from being a swap.
6. **Churn is UNMEASURED** for depth-20 as it is for depth-200; the first live
   session is the measurement, and `tv_depth20_ranked_swaps_total{outcome}` is
   where it lands.

#### What a PR that violates this section looks like (REJECT)

- Filters gainers AFTER cutting to the top 250 (drops gainers below the cut).
- Ranks a zero-lot contract (pads the board with arbitrary low ids).
- Removes the exit band, or lets a held contract leave the board on a single
  window in which it did not trade while an arrival funds it.
- Reverts the guard on a TIMED-OUT unsubscribe (the frame may have landed).
- Applies the depth-20 ranking from the frame drain, or lets one socket take
  more than four swaps in a minute.
- Presents the boot dial as ranked before the first 5-second sweep exists.

### 2026-09-08 (THIRD, same day) — the boot dial is SEEDED from the previous close, depth-200 gains its band, and an ignored unsubscribe now redials the socket

**No new authorization is claimed.** This is the record of items 1, 3 and 4 of
the (SECOND) section's "STILL not delivered" list being delivered, in one
change, with the properties the lock makes binding stated per item.

#### Item 1 — the boot dial (`crates/app/src/depth_seed.rs`)

| Property | Implementation |
|---|---|
| Source | the socket HOLDINGS at the capture-window close (15:40 IST), written to `data/instrument-cache/depth-seed-latest.json` only if a ranking was published that session. Never the ranking itself: what the sockets held is what was ranked AND admitted, which is the honest "yesterday's best" |
| Validation | every seeded id is checked against TODAY's contract artifact — `OPTSTK`, CE/PE, `NSE_FNO`, expiry ≥ today. An expired or unknown id is refused per contract and counted (`tv_depth_seed_rows_total{outcome}`); a seed that survives partially fills the lead slots and the boot dial fills the rest |
| Hold | a seeded pool HOLDS STILL until the first ranking of the session (`seed_until_first_ranking`), so the index at-the-money dial no longer runs for ~15 minutes on sockets that already carry yesterday's top set |
| Fail direction | absent, unreadable, wrong-day, or all-refused seed → the existing boot dial, with an `info!` naming the outcome. Never an empty socket |
| Bounded | ≤ 250 + ≤ 5 entries by construction; the validation index is built once per boot |

The bare nuke of 2026-09-08 deleted `instrument-cache`, so the first session
after it runs on the boot dial — the seed appears from the second session.

#### Item 3 — depth-200 hysteresis (`depth200_candidates.rs`, `depth200_ranked_steer.rs`)

`DEPTH200_EXIT_UNDERLYINGS` = 5 + `DEPTH200_HYSTERESIS_RANKS` (3) = 8. The
published list is the top 8 distinct underlyings; the ENTRY set is its first 5.
A held contract anywhere in the 8 is kept; only a contract outside the 8 is
swapped, and only for an unheld contract inside the first 5. Band contracts are
never placed. Same shape as depth-20's 250/300, at the scale of five sockets.

#### Item 4 — the ghost instrument (`depth_subscription_view.rs`, `dhan_feed_stack.rs`, core `pool_supervisor.rs`)

The FOURTH-quote section named the unsubscribe RequestCode (24 vs 25) as
UNVERIFIED-LIVE and said a socket that silently keeps delivering an
unsubscribed contract "is not detected". It is now:

| Property | Implementation |
|---|---|
| Detection | each depth PACKET on a live socket is classified O(1) against the published held sets and a dropped map. Only an instrument THIS process dropped ≥ `GHOST_GRACE_SECS` (90 s) ago, still arriving, is a ghost. Never-held is `Unknown` — the swap-in window is that shape and must not redial |
| Counted | `tv_dhan_feed_depth_total{outcome="ghost" \| "unsubscribed_grace" \| "ghost_redial"}` |
| Remedy | `request_ghost_redial(connection_index)` arms a per-socket register (cooldown 180 s); the connection task takes it on its existing 1 s idle tick and redials through the normal backoff ladder as `ReconnectReason::GhostInstrument`. The replay re-subscribes the guard's CURRENT set, which excludes the ghost, so the vendor's view is rebuilt from ours |
| Rows | STILL WRITTEN. The levels arrived; capture is not suspended for an instrument we did not want. Only the verdict and the redial are new |
| Log | one `error!` per armed redial (`code = WS-GAP-02`, `source = "unsubscribe_ignored"`), log-sink only; the cooldown is the throttle |

**This is the safety net the FOURTH-quote section said must exist before the
apply cadence is raised.** If code 25 is wrong for an endpoint, every swap
becomes an add, the ghost shows within 90 s, and the socket is rebuilt within
the cooldown instead of sitting at 804 for the session. *(⚠ 2026-09-10: this is exactly what happened — no socket parked. Code 25 WAS wrong; 24 ships since the 2026-09-10 section. **⚠ The counts once given here, "20 ignored unsubscribes, 10 redials", are WRONG — measured 2026-09-11 the session carried 80 lines across all ten sockets and both endpoints; see the dated correction under the 2026-09-10 section.**)*

#### Also delivered: the two per-cadence faces of `top_volume_rank`

`top_volume_rank_1s` and `top_volume_rank_5s` (`console_views.rs`) — views over
the ONE table filtered on `tf`, joined to the instrument master. The operator's
words were "1s table and 5s tables also separately"; one stored table with two
faces gives that without writing every row twice.

#### ⚠ STILL not delivered (Rule 11)

1. **The apply cadence is one minute, not five seconds.** The ranking runs every
   5 s; the delta is applied once a minute at :08. The operator asked for the
   5-second cadence a fourth time on 2026-09-08 (*"every 5 seconds needs to be
   resubscribed … in O(1) latency"*). The FOURTH-quote section's ordering —
   probe the unsubscribe code live FIRST, then raise the cadence — still binds;
   the ghost redial above makes a wrong code SELF-HEALING rather than fatal,
   which is what makes the raise safe to do next, but the raise itself needs
   its own dated row and is not smuggled in here.
2. **A swap is not O(1) on the wire.** Ranking is O(n log n) at 123 µs realistic / 2.95 ms at the ceiling (MEASURED 2026-09-12; the "900 µs" this line carried came from a harness timing an empty sort); the
   subscription CHANGE is one unsubscribe + one subscribe per swap with a 2 s
   wire budget, serialised per socket. The honest claim is "delta-only, capped
   per socket, edge-triggered" — never "O(1) resubscribe".
3. **Churn is UNMEASURED** for both pools until the first live session with
   this build; `tv_depth20_ranked_swaps_total` / `tv_depth200_ranked_swaps_total`
   are the read-out.
4. **The unsubscribe RequestCode is still UNVERIFIED-LIVE.** The ghost counter
   is now the instrument that verifies it: a session with `ghost = 0` and
   `unsubscribed_grace > 0` is the evidence that 25 works. *(⚠ 2026-09-10: the
   session read the OPPOSITE — every code-25 unsubscribe ignored, so 25 does NOT
work and
   24 now ships; the same counter pair is the verdict instrument for 24. See
   "2026-09-10 — THE DEPTH UNSUBSCRIBE REQUESTCODE IS SETTLED LIVE".)*

#### What a PR that violates this section looks like (REJECT)

- Seeds from the ranking rather than the holdings, or applies a seed without
  validating every id against today's contract set.
- Lets a seeded pool be re-dialled by the index engine before the first ranking.
- Places a band contract into a depth-200 socket, or evicts a held contract
  that is inside the band.
- Classifies a never-held instrument as a ghost (redials healthy sockets on
  every swap).
- Drops ghost rows instead of writing them.
- Raises the apply cadence under cover of this section.

### 2026-09-09 — THE VOLUME RANKING BECOMES VISIBLE; and the 5-SECOND APPLY CADENCE IS ORDERED BUT NOT SHIPPED, with the reason

**The verbatim operator demands (2026-09-09, typed directly in-session — preserve
EXACTLY, typos included):**

> "Dude just remove this one minute swap dude what happened to new volume ranking tables dude because we have now 5second swap right that too focusing only on options strikes top right"

> "Morning will I see the precise volume ranking or not bro meanwhile Ar eyou sure about precise volume ranking vaze don its quantity percentage diff and in this also how will you always find ghe per engage change dude that's my main question because morning I need evryhhting need to be ready dude okay"

#### Part 1 — the answer to the ranking question was NO, and three defects said so

The honest answer to *"will I see the precise volume ranking"* on the build that
was live this morning is **no, the table would have been EMPTY**, and the reason
was not one bug but three, each verified in source before it was touched:

| # | Defect | Evidence | Consequence at 09:15 |
|---|---|---|---|
| 1 | The snapshot asked the previous-close store for the **CONTRACT's** own close on `NSE_FNO`. The store's ONE production write door, `record_prev_close_from_tick`, returns early unless the segment is `NseEquity` — so the probe returned `None` on every row → `NaN` → refused as `NonFiniteGain` | `dhan_feed_stack.rs` (the `project_snapshot` closure) vs `:2656`; `record_prev_close` has zero production callers | **Every row of every snapshot dropped. The table empty all session while every counter read healthy.** |
| 2 | `window_lots_milli` — **the actual sort key since the 2026-09-07 lock** — was on `RankedContract` and was NOT a column of `TopVolumeRankRow`. Only cumulative `volume` was stored | `top_volume_rank_persistence.rs` column list | Rank 1 could hold less volume than rank 40 with nothing in the row to explain the order |
| 3 | `console_views::ensure_named_views` runs inside `run_candle_ddl_at_boot`, which `main.rs` calls BEFORE `run_live_table_ddl_at_boot` creates `top_volume_rank`. The view DDL warn-fails and is never retried in-boot | `main.rs:3745` vs `:3792`; `console_views.rs:288-296` | On a fresh volume `top_volume_rank_1s` / `_5s` **do not exist for the whole session** — the operator's own words for this table were *"only using db i can see this"* |

**Fixed, in this order.** (1) `gain_pct` is now the **UNDERLYING's** percent
change, computed by `underlying_gain_pct` from the **same two RAM inputs** that
`underlying_gainer_verdict` turns into the gainer boolean — so the column and the
filter cannot disagree, and the closure is keyed on `underlying_id` so the wrong
lookup is unrepresentable rather than merely corrected. This is also what
`eligible_gain_pct` was written for: its own `MAX_PLAUSIBLE_GAIN_PCT` doc states a
deep-out-of-the-money option "can move that far" and is "not what this function is
fed", so the old call site was wrong twice. (2) `window_lots_milli LONG` joins the
table through the house `CREATE → ADD COLUMN IF NOT EXISTS → DEDUP ENABLE`
self-heal, so the order is checkable from the row rather than taken on trust.
(3) The named views are **re-ensured** after the rank table exists — additive
rather than a re-order, because the candle ordering above it is load-bearing and
every view statement is `CREATE OR REPLACE`.

**So the answer to "how will you always find the percentage change":** it is the
underlying stock's move — spot against its previous close, both from RAM, both
the gainer filter's own inputs. It is NOT the option contract's own move: nothing
in this system has ever stored a previous close for an `NSE_FNO` contract, and
inventing one would be a fabricated number in the column that decides eligibility.

#### Part 2 — the 5-second apply cadence is NOT shipped, and this is the evidence

The operator ordered the one-minute swap removed in favour of the 5-second swap.
**It is not shipped today**, and the reason is a specific, checkable fact rather
than caution:

| Fact | Evidence |
|---|---|
| Dhan error 804 is classified `DisconnectClass::Fatal` | `pool_supervisor.rs:521-543` |
| Fatal ⇒ `park(ParkReason::FatalDisconnect)`, and `allows_one_respawn()` returns **`false`** for it | `pool_supervisor.rs:1247-1256`, `:866-871` |
| `take_ghost_redial` is consulted **only** while `action == SupervisorAction::Continue`. A parked socket has LEFT that loop | `pool_supervisor.rs:4634-4638` |
| **Therefore the ghost-redial detector shipped 2026-09-08 (THIRD) structurally CANNOT recover an 804-parked socket.** Parking bypasses the redial ladder | derived from the three rows above |
| depth-200 socket capacity is **1**. If the unsubscribe RequestCode (24 vs 25, still UNVERIFIED-LIVE) is wrong, swap #1 asks Dhan for 2 > 1 ⇒ 804 on the **first swap, at any cadence** | `pool_supervisor.rs:2058`, `:2136-2146` |

The 2026-09-08 (THIRD) section claimed the ghost redial "makes a wrong code
SELF-HEALING rather than fatal, which is what makes the raise safe to do next."
**That claim is WITHDRAWN.** It is true for a socket that keeps *delivering* a
ghost, and false for one that has been *parked* — and 804 parks. The cadence raise
would therefore be session-ending, not self-healing, and 5 s gives twelve times the
chances per hour to reach it.

Three further blockers stack behind that one, each independently sufficient:
per-socket swap caps are enforced per CALL and hold no cross-call state
(`depth20_ranked_steer.rs:77,87,116`), so 5 s yields 240 swaps/socket/minute
against a documented budget of 4; `send_swap` sets `socket.pending` with no
`pending.is_some()` guard, so a second dispatch corrupts the revert target
(`depth_rebalance.rs:987-990`); the per-iteration `load_depth_candidates` +
`fetch_movers` are two QuestDB queries whose RAM-empty budget is **10 s**, longer
than a 5 s tick (`dhan_contract_universe.rs:1423-1428`); and
`depth_steering_stalled`'s 180 s threshold, written against a 60 s loop, becomes 36
missed iterations (`live-lane-alarms.tf:829`).

**The ordering the 2026-09-06 FOURTH-quote section already binds stands: probe the
unsubscribe code on a live session FIRST, then raise the cadence.** The instrument
for that probe now exists and is live — `tv_dhan_feed_depth_total{outcome="ghost"}`
against `{outcome="unsubscribed_grace"}`. A session that ends with `ghost = 0` and
`unsubscribed_grace > 0` is the evidence that code 25 works, and the cadence raise
becomes a small change the following day. *(⚠ 2026-09-10: the evidence came back the other way for 25 — see the 2026-09-10 section; the same counter pair is now the verdict instrument for 24.)*

**What a PR that violates this section looks like (REJECT):** raises the apply
cadence before that probe reads clean; ships the raise without converting the
per-call swap caps to rolling per-minute budgets, guarding `send_swap` on
`pending`, moving the two QuestDB queries off the per-iteration path, and lowering
the stall threshold in the same change; computes `gain_pct` from the CONTRACT's
previous close (nothing writes one); or drops `window_lots_milli` from the row on
the grounds that `volume` is already there — `volume` has not been the sort key
since 2026-09-07.

### 2026-09-09 — STOCK FUTURES BECOME THE PRIMARY PRICE FOR ATM ±25, AND THE WINDOW FREEZES FOR THE DAY

**The verbatim operator demand (2026-09-09, typed directly in-session — preserve
EXACTLY, typos included):**

> "dude see dotn sue futures as the fallback dude just use futures as the primary dude espeically to fidn this atm plus minus and stickign fully with that for the entire current day dude okay?"

Given in DIRECT response to a design study that recommended futures as a
**fallback** for lot size and spot price and recommended **against** using them
to centre the strike window. The operator read that recommendation and reversed
its central conclusion. **That is his call and it governs.** The study's reasoning
is preserved below rather than deleted, because the numbers in it are measured and
the next reader is entitled to see what was traded away.

#### What this authorizes

| Surface | Was | Now |
|---|---|---|
| Price used to centre the stock-option **ATM ±25** window | the stock's SPOT last-traded price | **the stock's nearest-expiry FUTURE's last-traded price, as PRIMARY** |
| Spot price | the only source | the **fallback**, used when the future has not printed |
| Window lifetime | re-fit until 09:30, then frozen | **chosen once and frozen for the whole trading day** |
| Lot size | option's own `z`, refuse if absent | unchanged by this quote — the futures lot-size fallback stays a separate decision |

**The futures are already subscribed and this costs no new connection or fetch.**
All 1,270 `FUTSTK` contracts are classified at `dhan_contract_universe.rs:813`,
pushed at priority 1–2 into `picked`, surfaced as `ContractSelection::instruments`
and dialled onto the main feed in Full mode. Their ticks already reach the drain,
are lag-recorded and folded. The ONLY thing stopping a future's price reaching the
selector is the binding pattern at `dhan_feed_stack.rs:6172-6176`, which admits
`IdxI | NseEquity | BseEquity` and nothing else. Widening it to `NseFno` is one
enum arm; `SpotPriceStore::record` is already segment-agnostic and keyed on the
I-P1-11 composite, and ~1,270 futures against a 25,000 cap with ~870 live entries
is inside the ceiling.

#### ⚠ The honest measurement, which argued the other way

Recorded because the operator overruled it knowingly and a future reader must not
mistake this for a numbers-driven decision:

| Quantity | Measured |
|---|---|
| Median strike spacing, 210 F&O underlyings, current expiry (2026-08-27) | **2.63% of price** |
| Gap needed to move the nearest-strike pick by ONE step (half a spacing) | **1.32%** |
| Typical near-month equity futures premium (cost of carry, ~1 month) | **~0.5%** — *Assumed, not derivable in-repo* |

So on the median name the futures price does not change which strike is chosen,
and when it does the ±25 window shifts by one strike — 24 of 25 per side unchanged.
**The measured benefit to centring accuracy is therefore approximately zero.**

**What the change DOES buy, and it is real:** coverage. A stock with no spot print
is refused into `underlyings_without_spot` and gets no options at all that day —
measured 2026-08-21: 725 priced, **8 without**, ≈780 option contracts absent for
the session. A future that printed when the spot did not now supplies the centre.
How many of those 8 had a futures tick is **Unknown** — no counter exists, and a
stock too illiquid to print a spot usually has an equally illiquid future.

#### The freeze is the half with real consequences, in both directions

"Sticking fully with that for the entire current day" makes the window **immutable
once chosen**. That is stricter than today, where a top-up runs until 09:30.

- **For:** the subscribed set stops moving, so a contract cannot silently leave
  depth mid-session, and the day's capture is reproducible from one decision.
- **Against, stated plainly:** if the underlying moves more than 25 strikes from
  where it was centred, the true at-the-money leaves the captured window and
  **nothing re-centres it**. Measured drift is 2.20% ≈ 0.8 strikes on an average
  day and **6.0 strikes** on the worst single underlying of 2026-08-27 — well
  inside 25, so this is a tail risk rather than a daily one. It must be COUNTED:
  a session where the live price leaves the window is exactly the case the
  operator would want to know about, and freezing removes the mechanism that
  would otherwise hide it.

#### What this quote does NOT authorize

- **Previous close from futures.** A future's previous close is not the stock's;
  feeding it to the gainer test invents a percentage change from two unrelated
  numbers. The `PrevCloseStore` gate at `dhan_feed_stack.rs:2796-2800` stays
  `IdxI | NseEquity`. This is the one row that would manufacture a confidently
  wrong answer, and it stays shut.
- Any change to `STOCK_OPTION_ATM_STRIKES_EACH_SIDE` (25), the 60% pricing quorum,
  the socket or instrument budgets, or the subscription set.
- Index options: NIFTY/BANKNIFTY full-chain selection is unchanged and takes no
  futures price.
- Live order fire; `dry_run` stays true; the §28 frozen area is untouched.

#### What a PR that violates this section looks like (REJECT)

- Uses a far-month future rather than the **nearest non-expired** expiry — a stale
  far-month print would centre a ladder on an hours-old number.
- Lets a futures price overwrite a **fresher** spot print (the store's
  later-exchange-time-wins rule and its trading-day floor both still bind).
- Widens `PrevCloseStore` to `NseFno`.
- Re-centres the window after it is frozen, or freezes it without counting the
  case where the live price leaves the window.
- Presents futures centring as an accuracy improvement — the measurement above
  says it is a coverage change, and the honest claim is the coverage one.

### 2026-09-10 — THE DEPTH UNSUBSCRIBE REQUESTCODE IS SETTLED LIVE: 25 IS IGNORED, 24 SHIPS

**The verbatim operator demand (2026-09-10, typed directly in-session — preserve
EXACTLY):**

> "Fix and resolve everything I don't want any open items dude okay?"

Given in DIRECT response to a live-health report that listed, as its first open
item, that Dhan was ignoring the depth unsubscribe on the wire. That is the
§28.2/§28.3 authorization shape this repository already accepts: a general
go-ahead answering an ENUMERATED ask selects the enumerated work. Recorded HERE
before the constant moves, per the rule-file-first law.

#### The verdict the FOURTH-quote section asked for, read from the first session that could give it

The 2026-09-06 FOURTH-quote section bound the ordering *"probe the unsubscribe
code on a live session FIRST, then raise the cadence"*, and the 2026-09-09 section
named the instrument: `tv_dhan_feed_depth_total{outcome="ghost"}` against
`{outcome="unsubscribed_grace"}`, seeded at zero on 2026-09-10 so the answer could
be read at all. The session of 2026-09-10 (build carrying #1903, first ranked
swaps from 09:16 IST) is that probe, and it answered in the OTHER direction:

| Reading, 09:16–09:46 IST, 2026-09-10 | Value |
|---|---:|
| `WS-GAP-02` / `source = "unsubscribe_ignored"` ERROR lines | **20** |
| Ghost redials armed (`outcome = "ghost_redial"`) | **10** |
| Distinct instruments streaming depth-200 | **8**, on **5** single-instrument sockets |
| Code on the wire for every one of those unsubscribes | **25** (`FEED_UNSUBSCRIBE_TWENTY_DEPTH`) |

> ### ⚠ CORRECTED 2026-09-11 — the "20" and the "10" in the table above are BOTH WRONG, and the real numbers make the verdict STRONGER, not weaker
>
> Re-queried today against the source rather than carried forward
> (`aws logs filter-log-events --log-group-name /tickvault/prod/app
> --filter-pattern '{ $.fields.source = "unsubscribe_ignored" }'`), because this
> table is the evidence a vendor support ticket will cite and a number quoted
> from a quote is not a measurement:
>
> | 2026-09-10, code 25 | table said | MEASURED 2026-09-11 |
> |---|---:|---:|
> | `unsubscribe_ignored` ERROR lines, full session | 20 | **80** |
> | …inside the table's own 09:16–09:46 window | 20 | **48** |
> | Ghost redials armed | 10 | **80** (one per line; `redials_taken` reaches **8**, the session ceiling, on every socket) |
> | Endpoints affected | depth-200 implied | **40 depth-200 AND 40 depth-20** |
> | Distinct sockets affected | 5 implied | **10** — `connection_index` 5 through 14, i.e. EVERY depth socket |
> | First / last line | — | 09:20:09.059 / 10:46:11.440 IST |
>
> **20 matches no window.** It is not the session total and it is not the
> 30-minute total; where it came from is unrecoverable, which is exactly why it
> should never have been written without the query beside it.
>
> **The same query for 2026-09-11 (code 24) returns the SAME SHAPE:** 80 lines,
> 40/40 across both endpoints, all ten sockets, `redials_taken` max 8, first
> 09:18:38.073 and last 10:21:39.074 IST, `ghost_packets` 1–24.
>
> #### Why this is the most important line in the section
>
> The two codes do not merely both fail — **they fail IDENTICALLY**: same line
> count, same even split across two different endpoints, same ten sockets, same
> exhaustion of the redial ceiling roughly an hour into the session. A code that
> was simply *wrong* would be expected to differ from another wrong code in at
> least one of those dimensions. **That both produce a byte-identical failure
> signature is evidence the problem may not be the RequestCode at all** — and it
> is the single strongest thing to put in front of Dhan engineering, which the
> "20 vs 10" framing was too small to show.
>
> > ##### ⚠ CORRECTED 2026-09-11 (same day, by an adversarial re-read) — "byte-identical" is TRUE and is NOT EVIDENCE OF ANYTHING. 80 is our own ceiling, and this section refutes itself two paragraphs down.
> >
> > The paragraph above calls the identical signature "the single strongest
> > thing to put in front of Dhan engineering." **It is the weakest, because
> > every number in it is pinned by OUR code and could not have come out any
> > other way.**
> >
> > | "signature" dimension | what actually fixes it |
> > |---|---|
> > | **80** lines | `GHOST_REDIAL_SESSION_CEILING` = 8 × **10** depth sockets = **80**. Arithmetic maximum. |
> > | **40 / 40** split | 5 depth-20 + 5 depth-200 sockets × 8 = 40 each. Forced. |
> > | **all ten** sockets | there are exactly ten. |
> > | ceiling reached ~1 h in | the line is emitted ONLY inside the `Ok(())` arm of `request_ghost_redial`, which returns `Err(SessionCeiling)` past 8. |
> >
> > So the two sessions did not produce the same number because Dhan behaved
> > the same way — **they produced the same number because a counter that
> > saturates at 80 reported 80 twice.** A vendor that honoured code 24 on
> > 30% of frames and ignored 25 entirely would still print 80/40/40 as long
> > as *any* ghost survived per socket. **A metric that can only report one
> > value is not evidence**, and the section immediately below this one says
> > so without noticing: *"every socket reaches `GHOST_REDIAL_SESSION_CEILING`
> > (8) and stands down by design."*
> >
> > **What IS non-forced, and is therefore the real evidence:** `ghost`
> > (5,345,436) and `unsubscribed_grace` (1,686,468) have no ceiling. No
> > 2026-09-10 counterparts were recorded, so the one comparison that could
> > discriminate between the two codes was never taken.
> >
> > ##### ✅ STEP 1 DONE 2026-09-11 (same evening) — the 09-10 counters WERE still recoverable, and the two days are NOT alike
> >
> > The block above says the 2026-09-10 counterparts "were never recorded, so
> > the one comparison that could discriminate between the two codes was never
> > taken." That was true of what anyone had written down, and **false of what
> > still existed**: the CloudWatch EMF group keeps the per-outcome split, and
> > `filter-log-events` can still read it. It was taken.
> >
> > **Method, and why it is trustworthy.** The EMF record carries a **delta per
> > scrape**, not a cumulative, so a session total is the SUM of ~538 samples.
> > `logs:StartQuery` is denied to `claude-code-agent`, so the aggregation is
> > client-side over `filter-log-events`. **The method validates itself:** run
> > against 2026-09-11 it reproduces every already-known figure EXACTLY — ghost
> > 5,345,436 · grace 1,686,468 · rows 793,936,960 · depth-20 swaps 7,662 ·
> > depth-200 swaps 1,799. A method that reproduces five known numbers to the
> > unit is trusted for the sixth.
> >
> > | Full session, one continuous run each | **2026-09-10 · code 25** | **2026-09-11 · code 24** | ratio |
> > |---|---:|---:|---:|
> > | boots inside the window | 1 | 1 | — |
> > | depth-20 swaps sent | 6,068 | 7,662 | 1.26x |
> > | depth-200 swaps sent | 1,712 | 1,799 | 1.05x |
> > | **total unsubscribes** | **7,780** | **9,461** | **1.22x** |
> > | depth rows stored | 226,667,920 | 793,936,960 | 3.50x |
> > | **ghost packets** | **110,114** | **5,345,436** | **48.5x** |
> > | **unsubscribed_grace** | **55,160** | **1,686,468** | **30.6x** |
> > | ghost_redial | 80 | 80 | **1.00 — the ceiling** |
> > | ghost_exhausted | 10 | 10 | **1.00 — the ceiling** |
> >
> > **The only two numbers that matched are the only two that COULD NOT differ.**
> > That is the circularity above, now demonstrated with data rather than
> > arithmetic: `ghost_redial` is 8 x 10 sockets on both days, and everything
> > without a ceiling differs by one to two orders of magnitude.
> >
> > **Three independent normalisations, because ghost scales with traffic:**
> >
> > | normalised measure | 09-10 (25) | 09-11 (24) | ratio |
> > |---|---:|---:|---:|
> > | ghost packets per unsubscribe | 14.2 | 565.0 | **40x** |
> > | ghost packets per 1M depth rows | 486 | 6,733 | **13.9x** |
> > | **ghost / grace — traffic-independent** | **1.996** | **3.170** | **1.59x** |
> > | implied mean streaming tail, `T = 90(1+r)` | **~270 s** | **~375 s** | |
> > | ratio if NEVER honoured (`510/90`) | **4.667 ⇒ 510 s** | same | |
> >
> > The `ghost / grace` row is the one to lead with: it is two counters over the
> > SAME packet stream in two different windows, so it cancels traffic by
> > construction. All three point the same way.
> >
> > **What this DOES establish:**
> > 1. **The byte-identical argument is refuted by measurement, not only by
> >    arithmetic.** The days differ enormously wherever a ceiling does not
> >    forbid it. This needs no causal claim at all.
> > 2. **Neither code is fully honoured** — ghost > 0 on both days.
> > 3. **Neither code is fully IGNORED either**, which is new: both ratios sit
> >    BELOW the never-honoured ceiling of 4.667, so some unsubscribes are
> >    taking effect. "Dhan ignores it" is too strong for either day.
> >
> > **⚠ What this does NOT establish — the cause.** Four other PRs merged
> > between the two deployed builds (`55126249b`, `ad778aa50`, `16190ce2a`,
> > `61f6e448e`, `0e6f95fc8`), and `ad778aa50` carried a "subscribed-first
> > contract map" alongside the code flip. **Depth traffic also differed 3.50x
> > between the two days and that difference is itself unexplained.** So the
> > honest statement is *"the two sessions behaved very differently and code 25
> > ghosted far less on every normalisation"*, NOT *"code 25 is better because
> > it is 25"*. The one-socket probe (step 2) is still what settles cause,
> > and it is still unrun.
> >
> > **One bounded measurement caveat, stated because it cuts the convenient
> > way.** `55126249b` ("seed the depth ghost family ... so the unsubscribe-code
> > verdict is readable") merged at 08:52 IST on 09-10, twenty-two minutes AFTER
> > that session booted — so 09-10 ran unseeded, and the agent drops the FIRST
> > sample of a series it has never seen. That under-counts **09-10**, the day
> > with fewer ghosts, so correcting it would NARROW the gap. It is bounded at
> > one sample of 538 (~0.2%), far too small to move any row above.
> >
> > **This is not a claim that 24 or 25 works.** Both were almost certainly
> > ignored. It is a claim that **the argument as recorded cannot survive
> > vendor scrutiny**, and a ticket built on it invites "we cannot reproduce;
> > send a capture."
> >
> > **What the next session must do instead, in order:**
> >
> > 1. **Record the unbounded counters on BOTH days** — those are the numbers
> >    that can differ.
> > 2. **Run the one-socket probe.** A depth-200 socket holds **exactly one**
> >    instrument, so "did the stream stop?" has nothing to mask it: send the
> >    unsubscribe, **do not re-subscribe**, watch that `connection_index` for
> >    frame silence (`FrameSilenceElapsed` already measures it). Silence ⇒ 25
> >    works and the ghost verdict is OURS. Continued delivery ⇒ vendor-side,
> >    decisively, from one socket and about four minutes.
> > 3. **Try `RequestCode 12`.** The vendor's own depth guide documents only
> >    `23` (subscribe) and `12` (disconnect) — **no per-instrument
> >    unsubscribe at all** — and `build_disconnect_message` exists with
> >    **ZERO production callers** (`FEED_REQUEST_DISCONNECT` appears only in
> >    `constants.rs` and two test files). The only stop mechanism the vendor
> >    documents has never been sent.
> > 4. **Ask the right question.** Not *"why is 25 ignored?"* but **"what is
> >    the supported way to stop a depth stream for one instrument — is 25
> >    implemented on the Indian feed, or is 12 the only mechanism?"**
> >
> > **Also not ruled out, and it is ours:** a wire-FAILED unsubscribe still
> > produces a ghost. `held` advances on `try_send` Ok — command *queued*,
> > not sent — and reconcile KEEPS that advanced belief on both wire-failure
> > arms, so the view publishes a contract as dropped that the socket was
> > never told to drop. Only `tv_dhan_ws_subscribe_failed_total{unsubscribe_*}`
> > = 0 excludes this, and that must be re-read PER REASON, not as a rollup.
> >
> > ##### ✅ STEP 2 DONE 2026-09-11 (same evening) — the wire-failure alternative is EXCLUDED, but the instrument named above is one-quarter tautology
> >
> > The paragraph above says *"Only `tv_dhan_ws_subscribe_failed_total{unsubscribe_*}`
> > = 0 excludes this, and that must be re-read PER REASON, not as a rollup."*
> > It was re-read per reason. The answer is **zero on every reason on both
> > days** — and the instruction was RIGHT to insist on per-reason, because
> > the raw EMF records carry `endpoint` and `reason` as fields even though
> > the CloudWatch METRIC folds them to `host` alone. The split is readable
> > from `filter-log-events`; it is not readable from `get-metric-statistics`.
> >
> > | endpoint × reason (8 reasons × 3 endpoints) | 10 Sep · code 25 | 11 Sep · code 24 |
> > |---|---:|---:|
> > | every one of the 24 series | **0** | **0** |
> > | samples per series | 537–538 | 537–538 |
> >
> > 537 samples at zero is a *reporting* zero, not an absent series — these
> > are seeded in `DhanSocketParams::new`, so the first-sample rule that hid
> > `tv_depth_rows_spilled_total` does not apply here.
> >
> > **But one of the four unsubscribe reasons could not have been anything
> > but zero.** `send_unsubscribe` has exactly ONE production call site
> > (`pool_supervisor.rs`, the swap) and it is wrapped in
> > `tokio::time::timeout(SWAP_WIRE_BUDGET, ..)` — **1 second** — while the
> > socket write inside `send_unsubscribe_in_mode` is bounded by
> > `SUBSCRIBE_SEND_TIMEOUT` — **10 seconds**. The outer budget always
> > elapses first and drops the inner future, so the inner timeout arm never
> > runs and `reason="unsubscribe_timeout"` **can never increment in
> > production**. Citing four zeros is citing three measurements and a
> > tautology. That is the `capped`-counter class of the same day's
> > findings list, arriving in a second file.
> >
> > **So the claim rests on three reachable reasons plus a fourth
> > instrument** — and finding that fourth is what closed the hole. The
> > outer-timeout arm sets `wire_failed` and emits a coded `WS-GAP-02` line
> > with `source = "swap_wire_failed"` (or `"swap_emptied_socket"`), and
> > `origin/main` carried that `source` field during BOTH sessions, so it
> > would have been emitted had the arm fired:
> >
> > | app-log query, both schemas | 10 Sep | 11 Sep |
> > |---|---:|---:|
> > | `$.fields.source = "swap_wire_failed"` | **0** | **0** |
> > | `$.fields.source = "swap_emptied_socket"` | **0** | **0** |
> >
> > **Every arm of the unsubscribe chain is now covered by a live instrument,
> > and every one reads zero.** Across **7,780** (code 25) and **9,461**
> > (code 24) depth unsubscribes, not one failed on our side: the payload
> > built, the socket was connected, the write succeeded, and it completed
> > inside the one-second budget. **The wire-failure alternative is
> > EXCLUDED** — the ghosts are not our unsubscribes failing to reach the
> > socket.
> >
> > **⚠ The honest limit, which is narrow and real.** `send_unsubscribe` is
> > fire-and-forget and Dhan sends no ack: `Ok` means the bytes were written
> > into the sink and flushed, not that the vendor processed them. This
> > proves our side did its job up to the socket boundary and no further. A
> > TCP-level failure on a live connection WOULD have surfaced as
> > `unsubscribe_send`, and that is zero — so the remaining gap is bytes
> > written to a healthy socket that the vendor then ignored, which is
> > precisely the vendor ticket's claim.
> >
> > **⚠ A second defect found on the way, and it is the one that could have
> > bitten silently.** The whole swap wire-outcome family —
> > `tv_dhan_ws_swap_{total,refused,failed,timeout,emptied_socket,guard_reverted}_total`
> > — is in NEITHER the EMF selector NOR seeded, so none of the six had ever
> > reached CloudWatch. Their absence was not a zero, and the arm that can
> > manufacture a FALSE ghost (the 1-second budget elapsing, where the guard
> > is deliberately NOT reverted because the frame may have landed) was
> > readable only by luck — the coded log line beside the counter. §2.3m of
> > `dhan-rest-only-noise-lock-2026-07-14.md` alarmed `swap_emptied_socket`
> > and left `swap_wire_failed` unalarmed.
> >
> > **FIXED the same evening**, free: six named consts replace the literal
> > emit sites, all six are seeded at zero in `PoolSupervisor::new`, and
> > `unsubscribe_timeout` is annotated at its seed site with why a zero there
> > proves nothing. Three guards, each bite-proven in both directions
> > (`the_swap_budget_wins_so_the_inner_unsubscribe_timeout_is_vacuous`,
> > `every_swap_wire_outcome_counter_is_seeded_from_its_own_const`). **NOT
> > fixed:** none of the six is EMF-selected, so none is alarmable — that is
> > ~$0.30/mo each against a September forecast of $142.24 and a $135
> > automatic-stop line, so §2.3n's lever requirement is unmet and is not
> > assumed.
> >
> > **The reusable half, and it is about the guard rather than the code:**
> > the seeding guard was itself VACUOUS on first write — it searched the
> > source for the seeding line and found its own assertion message, so
> > deleting the baseline left it green. Caught by bite-proving it, which is
> > the only thing that could have caught it. A guard that quotes the text it
> > searches for is a guard that cannot fail, and this file has now recorded
> > the same shape three times: a saturated ceiling, an unreachable counter,
> > and a self-satisfying scan.
> >
> > The reusable half is the one this file keeps recording, now about a
> > measurement rather than a constant: **before citing a number as evidence,
> > ask what its maximum is.** This one had been written down three times,
> > beside its own ceiling, without anyone computing 8 × 10.
>
> **Why the lines stop around 10:21–10:46 and not at the 15:40 close:** every
> socket reaches `GHOST_REDIAL_SESSION_CEILING` (8) and stands down by design.
> The ghosts kept streaming for the remaining ~5 hours with no further redial —
> which is where the session's 5,250,076 `ghost` packets come from. Silence in
> the log after 10:46 is the ceiling working, NOT the problem resolving.
>
> **NOT re-verified:** the "8 distinct instruments on 5 sockets" row. These log
> lines carry `connection_index`, `endpoint`, `ghost_packets` and
> `redials_taken` and **no instrument identifier at all** — see the flagged gap
> below. That row stands as originally recorded and was not re-checked.
>
> #### ⚠ A REAL GAP this re-query exposed: no ghost can be NAMED
>
> The successful depth-swap path logs **no `security_id`** — `depth20_track.rs`
> increments `DEPTH20_SWAPS_SENT` and logs only on the REFUSAL arms, which carry
> `socket`, not the instrument. So across two full sessions of a confirmed
> vendor-side failure, **this process cannot say which contract ghosted.** The
> operator's own Dhan-support workflow requires "precise contract labels …
> SecurityId for every contract cited", and today that requirement cannot be met
> from our telemetry. Adding `security_id` + `segment` to the ghost line is the
> prerequisite for a ticket that names contracts; it is NOT fixed here.
>
> > ##### ✅ RESOLVED 2026-09-11 (same day) — and it was TWO sites, not one
> >
> > The paragraph above names the ghost line. Acting on it found the gap has a
> > second half, and the second half is the one that mattered more:
> >
> > | Site | Logged before | What its silence cost |
> > |---|---|---|
> > | the ghost `error!` (`dhan_feed_stack.rs`) | connection, endpoint, `ghost_packets`, `redials_taken` | which contract was still arriving |
> > | the unsubscribe **SUCCESS** arm (`pool_supervisor.rs`) | **nothing at all** | which contract we asked to drop, and *when*, and *with which request code* |
> >
> > Only the REFUSAL arm named an instrument — and a refusal is the case that
> > did not happen. **All 160 ignored unsubscribes across the code-25 and
> > code-24 sessions took the silent path**, so there was no record of the ask
> > to pair the ghost against.
> >
> > Both now carry `security_id` + `segment`; the success arm additionally
> > carries `request_code`, because **25 and 24 have BOTH shipped** and a
> > session's evidence is worthless if the reader has to guess which binary
> > produced it. Pinned by
> > `crates/app/tests/ghost_instrument_named_guard.rs` (5 tests, both sites
> > bite-proven in both directions).
> >
> > **It also answers the question the log could not.** The 2026-09-10 record
> > states plainly that the log *"CANNOT distinguish (a) the same contract
> > surviving 8 reconnects from (b) 8 different contracts each newly ignored"* —
> > opposite diagnoses, and no counter separates them. A named id does.
> >
> > **NOT claimed:** that this stops a ghost, changes a request code, or makes
> > Dhan honour an unsubscribe. It makes the failure *reportable*. The vendor
> > ticket the section above calls for stays the remedy; this is the evidence
> > it needs. Cost: zero new metric, zero alarm, zero EMF name — one `info!`
> > at the measured swap rate (~9,500 lines a session, ~24 a minute) and two
> > fields on an `error!` that is already throttled to once per socket per
> > 180 s cooldown.

A socket that was told to drop a contract kept receiving it past the 90 s grace,
on every socket that swapped, every time. **Dhan did not honour a single code-25
unsubscribe.** The ghost detector did exactly what the 2026-09-08 (THIRD) section
built it for — it detected, it redialled, and the redial rebuilt the vendor's view
from ours — so nothing was lost and no socket parked. What it cannot do is make 25
mean "unsubscribe" to the vendor.

#### What changes

| Surface | Was | Now |
|---|---|---|
| `constants.rs::FEED_UNSUBSCRIBE_TWENTY_DEPTH` | 25 | **24** |
| depth-20 and depth-200 unsubscribe frames | `RequestCode: 25` | `RequestCode: 24` |
| The pin test | `test_depth_unsubscribe_code_is_25` | `test_depth_unsubscribe_code_is_24_and_is_subscribe_plus_one` |
| Every doc, rule and comment asserting "25, NOT 24" | asserted | annotated with this date |

24 is not a guess: it is the ONLY other value either vendor surface names. The
classic annexure page (stable across every crawl since 2026-06-02) lists
`24 | Unsubscribe - Full Market Depth`, and the vendor's own reference client
derives every unsubscribe as `subscribe_code + 1`, which is how Ticker (15→16),
Quote (17→18) and Full (21→22) already work in this codebase — depth (23→24) now
follows the same rule the other three have always followed. The "25" came from
the portal export alone, and the live wire has now disagreed with the portal.

#### ⚠ What this does NOT claim (Rule 11)

- **24 is UNVERIFIED-LIVE until the next session reads it.** The verdict
  instrument is unchanged: a session that ends with `ghost = 0` and
  `unsubscribed_grace > 0` is the evidence that 24 works. If 24 is ALSO ignored,
  the same detector will say so within 90 s of the first swap, the redial will
  keep every socket alive exactly as it did on 2026-09-10, and the honest next
  step is a support ticket with both codes' evidence — not a third guess.
- **The apply cadence does NOT move.** The FOURTH-quote ordering binds until 24
  reads clean on a live session; the 2026-09-09 section's blockers on the
  cadence raise (804 parks the socket and the redial cannot reach a parked
  socket; per-call swap caps; `send_swap` unguarded on `pending`; two QuestDB
  queries per iteration; the stall threshold) all stand.
- **Nothing was lost on 2026-09-10.** Ghost rows are STILL WRITTEN by design; the
  cost of the wrong code was redial churn and stale depth-200 books, not data.

#### What a PR that violates this section looks like (REJECT)

- Reverts the constant to 25 without a dated live reading showing 24 ignored AND
  25 honoured — a document is not a wire.
- Raises the apply cadence on the strength of this change before a session has
  read `ghost = 0` with `unsubscribed_grace > 0` on code 24.
- Leaves any doc, rule or comment asserting "25, NOT 24" un-annotated — a stale
  assertion in this direction sends the next reader back to the code that was
  proven ignored.

### 2026-09-10 — A TICK WHOSE EXCHANGE DAY IS NOT THE RECEIPT DAY IS REFUSED OUTRIGHT, not just kept out of candles

**The verbatim operator demand (2026-09-10, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

> "ssee just now i saw one query which is this nse fno securityid dude which had ticks recievd at is 9.15 am but why the fuck ts is last even 3.29 pm motherfucker see clealry note this motherufcker which is clealry ntoe our recievd at shodu lbe between 9.15 am till 3.49 pm for fno right meanwhiel ts also shodul be always between 9.15 am till 3.49 pm for fno right  okay can you fix this issue and assure this also dude okay? see because whenevr we manually or fix or deploy soemthigna nd if the server starts means then the incoming recivign ticks shdou lneevr ver be inegtsed right into db right do you understand my point tel me dude okay?"

The operator additionally CHOSE the refusal shape from an enumerated three-way
question — *"Refuse the row entirely"*, over stamping it with the receipt time
and over also tightening the seconds-of-day window. Both halves are recorded
because the second is what makes this a narrow change rather than a wide one.

#### The row he found, and why it exists

`security_id 66422` (NSE_FNO, `web.dhan.co/Charts?exch=NSE&seg=D&secid=66422`)
carried `received_at` = today 09:15 and `ts` = 15:29. Nothing was broken at the
receipt end; the row is the documented consequence of a decision taken here on
2026-08-26.

**Dhan sends LAST TRADE TIME, never "now".** A dormant contract snapshotted at
the 09:15 connect therefore carries the stamp of whenever it last actually
traded. `multi_tf_aggregator.rs` records the measurement in its own words:
*"measured mean 5 hours, max 34 days"*, and *"verified live as 8,898 fabricated
bars in a database created empty that same morning."*

That was found and fixed on 2026-08-26 — **for candles only.** The refusal was
made CANDLE-ONLY, and the code says so verbatim:

> *"a prior-day frame is then refused as `stale_trading_day`, which is a
> CANDLE-ONLY refusal, so its row is still written to `ticks` and only the
> bogus bar is skipped. Recovery keeps everything it could legitimately keep."*

The stated reason — *"the ROW is a real last-traded price and is kept"* — is
defensible about a price and wrong about **this table**.

#### Why keeping the row was wrong, specifically

`ts` is QuestDB's **designated timestamp**. A row stamped with a previous
session's LTT does not merely look odd next to its `received_at`: it lands in a
**previous day's partition**, silently amending a day that already closed. And
once there it is indistinguishable from a genuine 15:29 trade on that day —
nobody reading the table later can tell the back-dated snapshot from the real
tick. That is the false-OK class, written into the one table the whole system
treats as ground truth.

**The operator's second sentence names the delivery mechanism exactly.** Every
reconnect and every deploy restart draws a fresh snapshot burst, so each one
injected a batch of these rows. That is why the shape appears at 09:15 and
after every manual restart.

#### The contract (LOCKED)

| Aspect | Locked value |
|---|---|
| Gate | A tick whose IST exchange DAY differs from its IST receipt DAY is **HARD-refused** — no row in `ticks`, no candle, no leaderboard entry |
| Direction | Both. `stale_trading_day` (exchange day BEFORE receipt day) and `future_trading_day` (exchange day AFTER receipt day) are refused identically |
| Counted | `tv_dhan_feed_ingest_refused_total{reason}` with the reason preserved — a refusal that reaches no counter is a silent drop, which is what this section exists to stop |
| No receipt | `received_at_nanos <= 0` is the documented "no receipt" sentinel (pre-`TVW3` WAL frames). With no second clock there is nothing to compare, so the gate STANDS DOWN rather than guessing. Those frames are replay, not live |
| Seconds-of-day window | **UNCHANGED** at 09:00–15:40 (`TICK_PERSIST_START/END_SECS_OF_DAY_IST`) — see below |

#### ⚠ What this does NOT change, and why the operator was told before choosing

The operator's message asks for **09:15–15:49**. The window is deliberately NOT
moved, and he selected the option that leaves it alone after being shown why:

- The **09:00** start is authorized and load-bearing. The 2026-08-28 section of
  this file moved candle bucketing to a 09:00 session start to capture the
  pre-open, and `CONTINUOUS_SESSION_START_SECS_OF_DAY_IST` (09:15) exists as a
  SEPARATE, narrower constant precisely so silence detection does not page
  during the pre-open. Indices tick continuously from 09:00; moving the persist
  start to 09:15 would discard that data.
- The **15:40** end is 10 minutes past the 15:30 close, which already covers the
  closing tail. Extending to 15:49 buys nine minutes in which nothing trades.

The row that prompted this was never a window problem. `15:29` is INSIDE the
existing window on both ends — that is exactly why it passed. The defect was
always the DAY, never the time of day, and the day gate is what this section
adds.

#### ⚠ Honest cost (Rule 11)

A dormant contract now leaves **no row at all** until it genuinely trades. Before
this change it left one row per snapshot, back-dated. There is no third option
that keeps a record without either back-dating a closed partition or fabricating
a trade at the receipt instant — the operator was shown all three and chose the
refusal. If a last-traded-price record for dormant contracts is ever wanted, it
belongs in its own table with an honest schema, never back-dated into `ticks`.

**NOT claimed:** that this repairs rows already written. It does not. Existing
back-dated rows remain in their prior-day partitions and would need a separate,
operator-authorized cleanup — which is a DELETE against `ticks`, a SEBI-retention
table, and is therefore deliberately not started here.

#### What a PR that violates this section looks like (REJECT)

- Returns `stale_trading_day` or `future_trading_day` to the candle-only set.
- Writes a day-mismatched tick under a receipt-derived `ts` (the fabrication the
  operator declined).
- Refuses a day-mismatched tick without counting it.
- Applies the gate when `received_at_nanos <= 0` — that is a WAL replay frame
  with no second clock, and guessing there re-creates the 2026-08-26 defect from
  the other direction.
- Moves `TICK_PERSIST_START_SECS_OF_DAY_IST` off 09:00 under cover of this
  quote — the operator chose the option that leaves the window alone.

### 2026-09-11 — THE CODE-24 VERDICT IS IN, AND IT IS NEGATIVE: Dhan IGNORES 24 TOO

**No new authorization is claimed.** This records the live reading the
2026-09-10 section explicitly asked for, in the exact terms it set, plus one
stale claim it carries.

#### The verdict instrument, and what it read

The 2026-09-10 section set the acceptance test verbatim: *"a session that ends
with `ghost = 0` and `unsubscribed_grace > 0` is the evidence that 24 works."*

Read from the running box at 15:46 IST on 2026-09-11, the first full session on
the code-24 build:

| counter | value |
|---|---:|
| `tv_dhan_feed_depth_total{outcome="ghost"}` | **5,250,076** |
| `tv_dhan_feed_depth_total{outcome="unsubscribed_grace"}` | 1,664,266 |
| `tv_dhan_feed_depth_total{outcome="ghost_redial"}` | 80 |
| `tv_dhan_feed_depth_total{outcome="ghost_exhausted"}` | **10** |
| `tv_dhan_feed_depth_total{outcome="rows"}` | 793,936,720 |

**`ghost` is not 0. It is 5,250,076. On the stated test, code 24 is ignored.**

#### The caveat that section named is CLOSED — the box genuinely ran the 24 build

The 2026-09-08 (THIRD) section's residual 4 and the 2026-09-10 section both
leave open whether the running binary carried the flip. Verified:

| check | reading |
|---|---|
| `/tickvault/prod/deploy/binary-git-sha` | `23701dfca0cbaa02710e55003054948b57ca2ca7` |
| `git merge-base --is-ancestor ad778aa50 23701dfca` | **true** — the build CONTAINS the 25 → 24 flip |
| `systemctl show tickvault -p NRestarts` | **0** |
| process start | **08:30:49 IST**, running 7h40m at the time of reading |

One continuous session, no restarts, on a build that contains code 24.

#### The derived streaming tail

Grace is `GHOST_GRACE_SECS` = 90 s and the dropped map remembers for
`DROPPED_RETENTION_SECS` = 600 s, so the ghost window is 510 s and the
never-honoured ceiling ratio is 510/90 = 5.67. Observed ratio:
5,250,076 / 1,664,266 = **3.155**. Solving `(T − 90)/90 = 3.155` gives
**T ≈ 374 s** — the average unsubscribed contract kept streaming for about
**six minutes** after we told Dhan to stop. Approximate: it assumes a steady
packet rate and ignores contracts that re-entered the top 250 and reverted to
`Held`.

At 20 levels per depth-20 packet that is ≈ **105 million stored rows, ~13.2% of
the session's 793,936,720**, for contracts this process had unsubscribed. Waste,
never loss — `dhan_feed_stack.rs` writes them by design and says so.

#### What this means, and what it does NOT

**Two codes have now been proven ignored on two consecutive sessions**, and the
vendor's own annexure contradicts itself about which is correct. Per the
2026-09-10 section's own instruction, **the honest next step is a support ticket
with both sessions' evidence, not a third guess.** No local strategy choice
repairs it.

**NOT claimed:** that a third code exists to try. **NOT claimed:** that anything
was lost — ghost rows are written, `ghost_exhausted = 10` means ten sockets
stopped redialling after `GHOST_REDIAL_SESSION_CEILING` and kept their working
set. **The apply cadence still does NOT move**: the FOURTH-quote ordering binds
on a clean read, and this read is the opposite of clean.

#### ⚠ RE-MEASURED 2026-09-11 (same evening) — three of the numbers above are LOW, the provenance is wrong by 17 minutes, and the REDIAL BOUGHT NOTHING

The table above says *"Read from the running box at 15:46 IST."* It was read at
**15:29 IST** — the running cumulative crosses all three claimed values at that
minute — and the session had not finished: ghost kept accruing to **15:43** and
rows to **16:01**. Re-queried from `/tickvault/prod/metrics`, the EMF log group
(`tv_dhan_feed_depth_total` in CloudWatch carries only a `host` dimension — it
folds `outcome` away, so the per-outcome split is **not verifiable from
CloudWatch metrics at all** and the EMF group is the only source):

| counter | table above | MEASURED session total | low by |
|---|---:|---:|---:|
| `ghost` | 5,250,076 | **5,345,436** | 95,360 (1.8%) |
| `unsubscribed_grace` | 1,664,266 | **1,686,468** | 22,202 (1.3%) |
| `rows` | 793,936,720 | **793,936,960** | 240 |
| `ghost_redial` | 80 | 80 | — |
| `ghost_exhausted` | 10 | 10 | — |

The derived tail is unchanged in substance: 5,345,436 / 1,686,468 = 3.170 ⇒
**T ≈ 375 s** against the 374 s recorded above.

**The finding that matters is not the 1.8%.** `GHOST_REDIAL_SESSION_CEILING`
is 8 and there are ten depth sockets, so **80 is the arithmetic maximum** — the
number is a saturated ceiling, not a measurement of ghost frequency. Every
socket hit it: all five depth-20 exhausted by **09:43 IST**, all five depth-200
by **10:22 IST**, firing at the 180 s cooldown floor back to back (conn 5:
09:19:18 → 09:22:18 → 09:25:38 → 09:28:38 → …), a fresh ghost verdict at the
earliest legal instant, eight times.

| ghost packets | count | share |
|---|---:|---:|
| before the last exhaustion (10:22 IST) | 1,151,204 | 21.5% |
| **after** | **4,194,232** | **78.5%** |

**Four fifths of the session's ghosting happened with no redial budget left**,
flat across five hours (hourly IST: 09h 624,792 · 10h 1,326,090 · 11h 963,876 ·
12h 746,296 · 13h 684,950 · 14h 634,604 · 15h 364,828). The remedy shipped on
2026-09-08 to make a wrong unsubscribe code self-healing **did not heal it**;
it spent its budget in the first hour and then watched.

**⚠ A CLAIM MADE IN THIS SESSION IS REFUTED AND IS WITHDRAWN: "every ordinary
swap arms a socket re-dial."** Measured: depth-20 **7,662 swaps → 40 redials =
0.52%**; depth-200 **1,799 → 40 = 2.22%**; combined **9,461 → 80 = 0.85%**.
It is wrong by two orders of magnitude and was stated without checking the
arithmetic against the swap counters that were already in hand. **And the
refutation must not be read the other way either** — 91% of all swaps
(depth-20: 7,122 of 7,662; depth-200: 1,502 of 1,799) happened AFTER that
pool's redial budget was already spent, so they could not have armed one
whatever they did. The honest statement is that **the ghost detector was
budget-blind for most of the session**, and the swap-to-ghost rate is unknown.

**Also measured, and it is the reassuring half:** `SubscriptionRejected`,
`ParkReason`, `InstrumentsExceedLimit`, `FatalDisconnect` and `parked` each
return **0 events**; `tv_dhan_ws_park_total` is 0 on all four reasons across
8,640 EMF records; `tv_depth_rebalance_swaps_refused_total` is 0 on all five
reasons. **No socket parked.** The wrong unsubscribe code costs redial churn
and stale books, not sockets.

**Still Unknown, and the gap is the one already flagged:** no `WS-GAP-02` line
carries an instrument id (`{ $.fields.security_id = * && $.fields.code =
"WS-GAP-02" }` → **0 events**), so the per-contract tail cannot be measured
directly and the 375 s figure remains a ratio derivation assuming a steady
packet rate. Adding `security_id` + `segment` to the ghost line is the
prerequisite, and it is also what a Dhan support ticket needs.

**Two counters this session quoted are in NO CloudWatch metric at all** —
`tv_depth_rebalance_swaps_sent_total` and `tv_depth20_track_swaps_sent_total`
exist only in the EMF log group and the local exporter. They are correct
(1,799 and 7,662, verified exactly); they are simply not alarmable today.

#### ⚠ CORRECTED in the same pass — "804 parks the socket" is STALE in TWO places

The 2026-09-09 section's blocker list says *"804 parks the socket and the redial
cannot reach a parked socket"*, and the 2026-09-08 (SECOND) section says a
depth-200 over-subscribe is *"804, Fatal, parked for the session."*

**Both were true until 2026-09-10 and are now wrong.** `classify_disconnect`
maps `DisconnectCode::InstrumentsExceedLimit` to
`DisconnectClass::SubscriptionRejected`, and `ParkReason::SubscriptionRejected`
is the **only** reason in the tree whose `allows_one_respawn()` returns `true`.
Its own docblock records the reasoning: neither overflow nor credential, the
worst case of being wrong is one wasted dial on that slot alone, and *"a fresh
connection resets the vendor's count"*.

So 804 costs **one bounded respawn — riding the normal backoff ladder, the
per-slot stagger and the flap floor — and then a park**, not an instant
session-ending park.

**This matters because the overstatement has already cost work:** the
2026-09-09 section used the permanent-park reading to WITHDRAW the claim that
the ghost detector makes a wrong unsubscribe code self-healing. That withdrawal
was correct about a socket that has already parked and wrong about the blast
radius. The blocker list itself STANDS — the cadence raise is still blocked by
the per-call swap caps, the unguarded `send_swap` pending slot, the two QuestDB
queries per iteration and the stall threshold — but the 804 row in it is
smaller than written.

**Both passages are left in place per house convention and corrected here.**

#### What a PR that violates this section looks like (REJECT)

- Ships a third unsubscribe RequestCode on a guess, without a vendor answer or a
  live reading that distinguishes it.
- Raises the apply cadence citing this section — the read is negative, so the
  FOURTH-quote ordering binds harder, not less.
- Repeats "804 parks the socket for the session" without the one-respawn
  correction.
- Reports the 5,250,076 ghosts as data loss. They are written.
- Cites the 80/40/40 "byte-identical signature" as evidence about the vendor —
  it is `GHOST_REDIAL_SESSION_CEILING` × socket count and could report no other
  number (see the CORRECTED block above).

#### ⚠ 2026-09-11 — five traps in reading the NEXT session's data, found before it was read

A ghost line and an ask line are now both instrumented, but a naive join of
them (or of either against `market_depth`) returns a **clean-looking wrong
answer**. Each of these was verified in source; none is a defect in the lane,
and all five are properties of how the evidence must be QUERIED.

| # | Trap | Why a naive read is wrong |
|---|---|---|
| 1 | **`Held` short-circuits over the UNION of both pools** (`depth_subscription_view.rs`: `depth20.contains \|\| depth200.contains` is tested BEFORE `dropped`) | A contract dropped from depth-200 while depth-20 still holds it classifies `Held`, never `Ghost`, and is never logged. That file's own docblock measures depth-200's entry set as *"by construction almost always inside depth-20's top 250 — roughly 19 of every 20."* **So depth-200 unsubscribes are nearly INVISIBLE to the ghost test, and their absence reads as "depth-200 unsubscribes worked."** The short-circuit is CORRECT for its real job — never redial a socket for a contract we legitimately hold elsewhere — so it must not be "fixed"; the reading must account for it. |
| 2 | **`d5` rows are not depth-socket rows** | `market_depth` holds three `depth_kind`s, and `d5` comes from Full-mode MAIN-FEED packets. Since the 2026-09-11 FOURTH board puts SPOT on depth-20, and every spot is also a main-feed Full instrument, `d5` rows keep arriving after a spot unsubscribe — innocently, forever. **Filter `depth_kind IN ('d20','d200')` or every spot unsubscribe is unfalsifiable.** |
| 3 | **Timestamp frames differ** | `market_depth.ts` is `received_at_nanos + IST_UTC_OFFSET_NANOS` — naive IST in a UTC-typed column — while the log timer emits a real `+05:30` offset. Comparing the log stamp as an instant against `ts` as UTC is out by **5 h 30 m**, so every row reads as "after" every ask. |
| 4 | **Ring dwell back-dates rows** | `received_at_nanos = Utc::now() − frame.received_at.elapsed()`, so a row that physically arrived AFTER the ask can be stamped BEFORE it by up to the ring dwell (alarmed only at 2,000 ms, worst at the open). Early post-ask rows go missing from a strict `ts > ask` window. |
| 5 | **`top_volume_rank` cannot defeat the re-subscribe confound for the contracts under test** | It persists only the top `TOP_VOLUME_RANK_PER_FAMILY` (250), and a contract is dropped *because* it fell off the board — so it usually has **no row at all**, not a `subscribed=false` row. `subscribed` also comes from the per-MINUTE publish while snapshots write at 1 s/5 s, so a re-subscribed contract reads `false` for up to 60 s. |

**Two worst cases that are byte-identical to success, and must be excluded
before any conclusion:** (a) a session where nothing left the top 250 produces
zero asks AND zero ghosts — indistinguishable from "unsubscribe now works";
the only separator is `unsubscribed_grace > 0`. (b) The deploy not shipping
produces ghost lines with no ids — identical to 2026-09-10/11; **verify the
binary sha before trusting a null result.**

**What survives all five:** the ASK record (`depth_unsubscribe_sent`) has no
redial ceiling, so it is the only surface covering the 78.5% of ghosting that
happens after every socket exhausts its redials — and the one-socket depth-200
probe in the CORRECTED block above needs none of these joins at all.

### 2026-09-11 (SECOND) — the depth-200 hysteresis band widens 3 → 15, under the remedy the 2026-09-07 lock already prescribes

**No new authorization is claimed, and none is needed.** The 2026-09-07 section
legislates for this exact condition in advance, verbatim: *"If the swap budget
is hit routinely, the answer is a longer window or a hysteresis band on
entry/exit — NOT reverting to cumulative."* The swap budget is being hit
routinely. This is that remedy, applied to the pool that is hitting it.

#### The measurement that triggers it

Read from the box, 2026-09-11, one continuous session (process up 08:30:49 IST,
`NRestarts=0`):

| reading | value |
|---|---:|
| `tv_depth_rebalance_swaps_sent_total` — **depth-200 pool, 5 sockets** | **1,799** |
| Steering cycles in the capture window (23,100 s ÷ 60) | 385 |
| Swaps per cycle, against `MAX_RANKED_SWAPS_PER_MINUTE` = 5 | **4.67 — 93.5% of the cap** |
| Mean hold per depth-200 contract | **1.07 minutes** |
| `tv_depth_rebalance_swaps_refused_total{*}` | **0 for every reason** |

A deep socket held a contract for about one minute all session, and the pool sat
pressed against its own safety valve. Every refusal counter reads zero — the
machinery is not misbehaving; it is doing exactly what it was told, 1,799 times.

#### ⚠ A correction to this file's own arithmetic, recorded because it was quoted

The 2026-09-11 depth-subscription review quoted 1,799 as the whole system's swap
count. **It is the depth-200 pool alone.** depth-20 keeps a separate counter that
had never been read. Read the same day:

| counter | value |
|---|---:|
| `tv_depth20_track_swaps_sent_total` | **7,662** |
| `tv_depth20_ranked_swaps_total{outcome="planned"}` | 7,662 |
| `tv_depth20_ranked_swaps_total{outcome="capped"}` | **24,607** |
| `tv_depth20_ranked_swaps_total{outcome="unplaced"}` | 13,386 |
| `tv_depth20_ranked_swaps_total{outcome="unfunded_departure"}` | 121 |
| `tv_depth20_track_swaps_refused_total{*}` | 0 for every reason |

**Both pools together: 9,461 swaps performed against 32,269 wanted — the caps
refused 76% of the system's own appetite.** Each of the 250 depth-20 slots
changed contract ~31 times (mean hold ~12.6 minutes), which is far calmer than
depth-200's 1.07 minutes and is why this change touches depth-200 only.

#### What changes

`DEPTH200_HYSTERESIS_RANKS` 3 → **15**, so `DEPTH200_EXIT_UNDERLYINGS` moves
8 → **20**. Entry is unchanged at the socket budget of 5.

| | entry | exit | share of the ~208 live F&O underlyings |
|---|---:|---:|---|
| before | top 5 | top 8 | a held name lost its socket on falling out of the top **3.8%** |
| after | top 5 | top 20 | it must fall out of the top **~10%** |

**Nothing else moves.** The socket budget is 5, the instrument budget is 250 + 5,
the per-minute swap cap is unchanged, placement still comes from the ENTRY set
only, the ranking cadence is still 5 s and the apply cadence is still once a
minute. Stock-options-only stands; index options and futures remain banned.

#### Why 15, and the honest limit of the choice

**It is not derived.** Nobody has measured how far an underlying's rank drifts
between five-second windows — that is the number that would set this exactly,
and it does not exist. 15 is the first value with a defensible MEANING: *entered
as one of the five busiest names, keeps its socket until it is no longer among
the busiest tenth.* Top-8-of-208 is not a statement about a name being busy; at a
five-second sampling window on stock-option books this file already calls
*"thinner than FINNIFTY's"*, it is rank noise — and 93.5% of cap is what rank
noise looks like from the outside.

**The depth-20 ratio is deliberately NOT the model.** That pool enters at 250 and
exits at 300 — a 1.2× band — and copying the ratio would give depth-200 a band of
ONE, which is worse than today. The pools differ in the COST of being wrong, not
in proportion: losing a contract costs depth-20 one slot in 250 and costs
depth-200 **one socket in five**, on a feed with **no snapshot-on-subscribe**, so
the replacement book is silent until its next update.

**The cost of being too wide, stated plainly:** the pool can hold names ranked
16–20 while 6–15 sit unheld, because placement only happens into a socket whose
contract has left the list entirely. Every such name entered as a top-five, so
the set is "recently busiest", never arbitrary — and against a measured
one-minute hold on a book that starts silent, a continuous hold on a
recently-top-five name is very likely the better capture. **That is a judgement,
and it is labelled as one.**

#### NOT fixed by this, and it is a second churn source

The published list carries **one contract per underlying**, and the planner keeps
a socket only while that exact contract is still the list's pick for its name. If
the underlying stays busy but its busiest STRIKE moves, the held contract falls
off and the socket swaps — **even though the NAME never left the band.** Widening
the band does nothing for that case. Keying the keep-test on the underlying
rather than the contract is a semantic change to what a depth-200 socket
promises, and it deserves its own decision rather than riding along here.

#### NOT claimed

That this fixes the churn. It is the first measured step, and
`tv_depth200_ranked_swaps_total{outcome}` against the 1.07-minute mean hold is
what tunes it: still near the cap next session means still too narrow; swaps
collapsing to near zero with a stale held set means too wide. **Nothing may be
reported as fixed until those move.** It also does nothing about the
unsubscribe-code failure recorded in the section above — ghosts are a property of
Dhan ignoring the instruction, not of how often we send it, though fewer swaps
does mean fewer ghosts.

#### The ratchet

`depth200_candidates::tests::the_hysteresis_band_stays_wide_enough_to_mean_the_name_is_still_busy`
asserts a FLOOR (`DEPTH200_HYSTERESIS_RANKS >= 2 × DEPTH_200_SOCKET_BUDGET`),
never equality — tuning upward on the next measurement must not fail the build,
narrowing back toward 3 must. Bite-proven in both directions on 2026-09-11:
reverting the constant to 3 fails it; restoring 15 passes 85 depth-200 tests and
the full 2,134-test app suite.

#### What a PR that violates this section looks like (REJECT)

- Narrows the band below the ratchet floor without a measurement showing rank
  drift is small.
- Widens the band by widening the ENTRY set — entry is the socket budget and
  stays there; only the keep-test moves.
- Changes the socket or instrument budgets under cover of this change.
- Applies the same widening to depth-20, whose 50-rank band measured a 12.6-minute
  mean hold and zero refusals.
- Reports the churn as fixed before `tv_depth200_ranked_swaps_total` says so.

### 2026-09-11 — THE DARK WINDOW AFTER A SUBSCRIBE IS MEASURED FOR THE FIRST TIME

**The verbatim operator authorization (2026-09-11, typed directly in-session):**

> "go ahead and add the time to first packet measurement dude okay?"

Given in DIRECT response to a report whose "what I would do next" list opened
with, verbatim: *"**Measure time-to-first-packet after every subscribe.** One
timestamp on swap, one on the first depth frame for that contract, one
histogram."* That is the §28.2/§28.3 authorization shape this repository
already accepts — a general go-ahead answering an ENUMERATED ask selects the
enumerated work. Recorded HERE, in the same change as the code, per the
rule-file-first law.

#### The gap it closes

The operator asked how long a depth-20 resubscribe takes during a swap. Every
number this repository could offer was a **BUDGET, not a measurement**:

| Number | What it actually is |
|---|---|
| `SWAP_WIRE_BUDGET` = 1 s per side | the ceiling we WAIT before giving up |
| `SUBSCRIBE_SEND_TIMEOUT` = 10 s | the transport's own send ceiling |
| 1 swap/socket/minute | the rate the unreconciled-ack gate permits |

None answers the question. They bound how long we wait; they say nothing about
how long Dhan takes to start delivering the new book — and the India feed has
**no snapshot-on-subscribe** (the "First Tick Snapshot" is documented only on
the US global-stocks socket), so a freshly subscribed contract is not merely
late, it is **BLANK until the book next changes**, and nothing in this process
measured that window.

#### What ships

`crates/app/src/depth_first_packet.rs` — one tracker, four call sites:

| Site | Cadence | Cost |
|---|---|---|
| depth-20 dispatch, `Ok(())` arm only | ≤ 20/min | O(1) |
| depth-200 dispatch, `Ok(())` arm only | ≤ 5/min | O(1) |
| frame drain, beside the ghost check, **before the level loop** | per depth PACKET | **one relaxed atomic load** when idle; + one hash probe while a swap is outstanding |
| per-minute sweep, beside the swap-ack reconcile | 1/min | O(pending), capped at `MAX_PENDING` = 1,024 |

> **⚠ CORRECTED 2026-09-11 — the depth-20 cadence in the table above read
> “≤ 5/min” and the real figure is ≤ 20/min, four times higher.** Found by the
> same 6-agent sweep that found the three code bugs in this module, and it is
> the identical mistake this file records elsewhere in prose: the depth-200
> cap IS five a minute pool-wide (`MAX_RANKED_SWAPS_PER_MINUTE` =
> `DEPTH_200_SOCKET_BUDGET`, one per socket × five sockets, and its own
> docblock says so in those words), and that figure was carried across to the
> depth-20 row where it does not hold. depth-20 permits
> `MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE` = 4 per socket
> (`DEPTH_SWAP_COMMAND_CHANNEL_DEPTH`, const-asserted ≤ the channel depth
> because “a cap above the depth is not a cap”) across five sockets.
>
> **Read the depth-200 row as correct and unchanged.** Only the depth-20 cell
> moved.
>
> **What the wrong number understated:** the stamp rate into the pending set,
> which is the input to the `MAX_PENDING` = 1,024 ceiling and to the
> per-minute sweep's O(pending) cost. Neither conclusion changes — at 25
> stamps a minute against a 120 s lifetime the set holds on the order of 50,
> two orders of magnitude under the cap — but a reader sizing that ceiling
> from this table would have been working from a quarter of the real rate.
>
> The reusable half is the one this file keeps recording: **a per-socket
> figure and a pool-wide figure are different claims, and the two depth pools
> express their caps in different units.** Quoting one pool's number into the
> other pool's row is how they get conflated — and the two rows sitting
> adjacent with the same value is exactly what made it look checked.

Series (local `/metrics` only — see the budget row below):
`tv_depth_first_packet_latency_ms` (histogram) and
`tv_depth_first_packet_total{outcome="arrived"|"silent_window"|"refused"}`,
all three seeded at zero at boot.

#### ⚠ What the number composes — it is NOT a network round-trip

The clock starts when the steering task successfully QUEUES the swap and stops
at the first depth packet for that contract. Four terms, and only the first two
are ours: queue wait · the connection task's wire writes · Dhan applying the
subscription · **time until the contract's book next changes**. The fourth is
the market's, and on a thin stock option it can be the whole figure. It is
deliberately the UPPER bound — *how long until data flows again after we decide
to swap* — because that is the number a fill depends on.

Measuring from DISPATCH rather than from the wire write is also what keeps this
an app-crate change: the wire write happens on the connection task in `core`,
and reaching it would thread a clock through the transport for a term already
bounded by `SWAP_WIRE_BUDGET`.

#### ⚠ Why `silent_window` is not called a failure

A contract with no depth packet inside `FIRST_PACKET_WINDOW_SECS` (120 s) may
simply not have traded. Naming that "never arrived" would be a claim in the
ALARMING direction about a book doing nothing wrong — the mislabel class this
file keeps correcting. What it IS good for is the shape nobody could see
before: a swap that acknowledged and then delivered nothing at all. With the
unsubscribe code proven ignored on both 25 (2026-09-10) and 24 (2026-09-11),
the arrival half is the only half left to check.

#### ⚠ Budget: LOCAL ONLY, and that is the decision not an oversight

No EMF selector entry, no CloudWatch alarm, **$0.00/mo**. The September
forecast read live 2026-09-06 is **$142.24** against an automatic
`STOP_EC2_INSTANCES` action line of **$135.00**, and §2.3n of
`dhan-rest-only-noise-lock-2026-07-14.md` requires the next addition to arrive
with a LEVER, not a cost note. This change carries no lever, so it carries no
CloudWatch cost. It is the number an operator reads AFTER an existing page.

#### ⚠ CORRECTED 2026-09-11 (same day) — the instrument had TWO fabricated-zero paths, and both biased it toward zero

A six-agent adversarial sweep of this module, run hours after it landed and
before it had ever produced a live reading, found two independent ways for it
to record a **0 ms arrival that never happened**. Both are fixed; both are
bite-proven in each direction. They are recorded rather than quietly patched
because they are the same class as the pool-byte defect this section already
carries — and because an instrument that is wrong toward zero is wrong in the
direction that reads as good news.

| # | Defect | Why it fires | Fix |
|---|---|---|---|
| **1** | A **ghost stream answers a fresh stamp.** `Key` is `(security_id, wire_segment, pool)` — the pool byte separates depth-20 from depth-200, but **nothing separates socket 3 from socket 7 inside one pool.** | Dhan ignores the unsubscribe (proven for code 25 on 2026-09-10 and code 24 on 2026-09-11; mean ghost tail ~374 s). A dropped contract keeps streaming from the OLD socket, leaves `held_anywhere`, is legally re-taken onto a DIFFERENT socket, and the next in-flight ghost packet resolves the new stamp at ~0 ms for a socket that has delivered nothing. | `record_subscribe_at` gains `may_already_be_streaming`, answered at both dispatch sites from `DepthSubscriptionView::classify_raw`. Anything but `Unknown` means this process has a recent record of the contract on a depth socket → refuse the measurement and count `unmeasurable`. |
| **2** | A **back-dated receipt clamped to zero and counted as `arrived`.** | NOT an NTP edge case. The drain's `received_at_nanos` is deliberately back-dated by ring dwell (`Utc::now()` minus `frame.received_at.elapsed()`) while the stamp is a raw `Utc::now()`. Ring dwell has its own alarm at 2,000 ms, so under any backlog a packet received before the dispatch and drained after lands here. The old code did `.max(0)` and still counted `arrived`. | A receipt preceding its subscribe is counted `reordered` and records NO sample. The entry is still CONSUMED — the contract has delivered a packet, so it must not also age into `silent_window`. |
| **3** | A **wire-REFUSED swap aged into a false `silent_window`.** | The stamp fires on the `Ok(())` arm of `try_send`, which proves the command reached a CHANNEL, not that the connection took it. On a `NotHeld`/refused/sender-dropped ack the believed hold is reverted — but the stamp survived and was swept at 120 s as a dark window for a subscribe that never happened. | `forget()`, called from both reconcile revert arms. Idempotent. |

**Two honest limits, stated rather than left to be found.** The gate
deliberately OVER-refuses the harmless cross-pool case, because `classify_raw`
is pool-blind — depth-20 holding a contract now refuses a depth-200 stamp the
pool byte would already have protected. Over-refusing costs one sample;
under-refusing costs the series' credibility. And the module header's claim of
*"Zero allocation on every arm"* was **false in the deallocation direction**
and is corrected in place: `pin()` returns a `seize` guard whose drop runs that
collector's deferred reclamation, so the drain — which pins far more often than
the steering task — performs essentially all of this map's frees.

**Also hardened in the same change, and it closes a finding in both
directions:** `depth_first_packet_wiring_guard.rs` was the only guard of its
family scanning RAW source while ten siblings strip comments first, so a
deleted call site left behind as `// record_subscribe_at(...)` would have kept
every assertion green. The inverse is not theoretical either — the explanatory
sentence added to a dispatch site the same day moved the scan's anchor and
turned a placement test RED against correct code. It now strips comments,
anchors on the CALL rather than the bare name, carries a `guard_self_test`
bite-proving it can fail, and drops an assertion that was **vacuous**
(`level_loop > 0`, an offset that cannot be zero once the `find` succeeded).

#### ⚠ What this does NOT do (Rule 11)

- **It does not make a swap faster.** It reports how long the new contract
  stays dark. The remedies — a fixed universe that never resubscribes, or a
  vendor answer on the unsubscribe code — are unchanged.
- **It does not measure the OTHER 49 instruments' exposure.** While a swap's
  two wire calls run on the connection task, that task is not polling `recv()`,
  so every instrument on that socket queues kernel-side. That is a separate
  measurement and is NOT claimed here.
- **No live reading exists yet.** The first session with this build is the
  measurement; until then "a swap takes N ms" is not a claim this repository
  can make, and the 2 s figure remains a CEILING that has never been observed.

#### What a PR that violates this section looks like (REJECT)

- Moves the observation INSIDE the depth level loop (a depth-200 frame carries
  200 levels; the question has one answer per packet).
- Drops the `connection_index != u8::MAX` gate (a replayed WAL frame reports a
  latency against a clock that never ran).
- Stamps the clock on a REFUSED dispatch arm (ages into a false
  `silent_window`).
- Runs the O(pending) sweep on the frame drain.
- Removes the sweep (a dark contract is never counted, AND the hot-path gate
  stays above zero for the session, so every depth packet pays a probe).
- Quotes the histogram as a network round-trip, or as proof a swap is fast —
  term 4 above is the market's, not ours.
- Adds an EMF name or alarm for these series without a LEVER in the same
  change.

### 2026-09-11 (THIRD) — DEPTH-20 IS TOP 7 UNDERLYINGS BY PERCENTAGE MOVE, FUTURES + OPTIONS, FROM PRE-OPEN — and the 5-second cadence ask is WITHDRAWN

**The verbatim operator demands (2026-09-11, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

**Quote A (the withdrawal — this is what unblocks everything else):**
> "yes go ahead with thsi newer requiremente which i have sated clealry dude okay firget the fucking dpeth 20 seconds level resuscoirbe bro okay? just go ehad wiht this newer mintue level resubscrbe newer requirmeen t aloen dude okay? do you really udnerstadn ddue okay?"

**Quote B (the requirement):**
> "see its simplembro just pcik the percnetage mvoe dude to fidn the top 7 that too startign pre oprn itself shodu lbe spotted dude okay? so after evry one minute also it shodu lobe chekd dude okay? do you understand my ppioitn dude okay? nwo did you get my point dude okay? i clelaury told yo uto pick top 7 futures startign pre makret based on percnetage change dude see that too after 9.15 am also startign 9l.16 am always check the same top 7 percnetage change current expiry futures dude and its repsective options of atm plus or minus also rigth for btoh calla nd ptu dude okay? and then for index only nifty and bankn ifty futures and its repscetive options atm plus or min us 10 rigth dude now chekc this and tell me will it sit udner 250 slots and how will yo usubscibre this also dude see because evry unique fno shouslbe be subscirbe dspeartely rigth dide becuase if you need to swap with subscirbe reusbscirbe emans then tell me dude okay?"

**Quote A was given in DIRECT response to a message that had just reported the
capacity arithmetic (247/250 at ATM±5), the swap-clock blocker (23 swaps to move
one name against a 20/minute budget), and the five REJECT rows this reverses —
and that closed by asking for his words on the record before any code. He
answered by authorizing the design AND withdrawing the cadence ask that was the
hardest blocker.** Recorded HERE, before the code, per the rule-file-first law.

#### What Quote A RETIRES — and this is the most consequential line in this section

The 5-second apply cadence has been asked for four times (2026-09-06 Quote A,
the 2026-09-06 FOURTH quote, 2026-09-08, 2026-09-09) and refused four times for
reasons recorded above: the per-call swap caps hold no cross-call state, the
unguarded `send_swap` pending slot, two QuestDB queries per iteration against a
5-second tick, and the 180 s stall threshold written for a 60 s loop.

**Quote A withdraws it: "forget the … depth 20 seconds level resubscribe …
just go ahead with this newer minute level resubscribe."** The apply cadence is
therefore **ONE MINUTE, by the operator's own instruction**, and the four
blockers above are moot rather than deferred. The FOURTH-quote ordering ("probe
the unsubscribe code FIRST, then raise the cadence") is likewise moot for THIS
design: there is no raise. It stands unchanged for any FUTURE cadence proposal.

#### What this SUPERSEDES

This reverses the 2026-09-06 depth lock on three of its four axes, and the
2026-09-07 sort-key lock on one. Recorded rather than overwritten:

| Surface | 2026-09-06 / 09-07 locked value | 2026-09-11 (THIRD) |
|---|---|---|
| Instrument class | stock options ONLY (`OPTSTK`); *"No underlying spot or futures or indices or indices fmo"* | **stock options + STOCK FUTURES + INDEX futures + INDEX options** (NIFTY/BANKNIFTY only) |
| Selection unit | individual CONTRACTS, top 250 by volume | **top 7 UNDERLYINGS**, each contributing its future + ladder |
| Sort key | `window_lots_milli` (lots traded in the window) | **absolute percentage move of the UNDERLYING**, in integer basis points |
| Gainer role | eligibility FILTER, never the sort key | **the sort key itself** — and it is the ABSOLUTE move, so a faller ranks equally with a riser |
| Ranking start | 09:15 (`within_capture_window`) | **09:00 — pre-open** |
| Apply cadence | once a minute at :08 | **unchanged, once a minute** |
| depth-200 | unchanged by this quote | **UNCHANGED** — top 5 distinct underlyings by lots-in-window, band of 20 |

**depth-200 is NOT touched by this quote.** Quote B says *"this is purely
related to depth 20"* in its 2026-09-06 ancestor and says nothing about the deep
pool here. The 2026-09-11 (SECOND) band widening stands, the volume key stands,
and stock-options-only stands for depth-200.

#### The contract (LOCKED)

| Aspect | Locked value |
|---|---|
| Selection unit | **UNDERLYING**, not contract |
| Sort key | `move_bps = ((ltp_paise − prev_close_paise).abs() × 10_000) / prev_close_paise` — **i64, integer, no float anywhere on the path** |
| Direction | **ABSOLUTE.** A −8% faller and a +8% riser rank identically. This follows the operator's 2026-09-06 words *"top 7 among between top gainers losers combined"*; it is the one place this section ASSUMES rather than quotes, and it is a one-constant flip (`DEPTH20_RANK_ABSOLUTE_MOVE`) if he means gainers only |
| Inputs | `SpotPriceStore` (ltp) and `PrevCloseStore` (prev close) — both integer paise, both already live from 09:00, both already the gainer filter's own inputs |
| Stock names | **top 7** by `move_bps` |
| Per stock name | its **nearest-expiry FUTURE** (1 slot) + its options **ATM ± `DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE` = 5**, CE and PE (22 slots) = **23** |
| Index names | **NIFTY and BANKNIFTY only**, unconditionally — never ranked, never displaced |
| Per index name | its **nearest-expiry FUTURE** (1 slot) + its options **ATM ± `DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE` = 10**, CE and PE (42 slots) = **43** |
| Total | 2 × 43 + 7 × 23 = **247 of 250** |
| Ranking cadence | every minute, from **09:00** |
| Apply cadence | every minute at :08 — **UNCHANGED** (Quote A) |
| Entry / exit | enter at rank ≤ 7; **keep until rank > `DEPTH20_NAME_EXIT_RANK` = 12** |
| Budget | 250 depth-20 instruments, 5 sockets × 50. UNCHANGED |
| Segment | `NSE_FNO` only. SENSEX / BANKEX remain structurally impossible (BSE_FNO, Dhan serves depth on NSE alone) |
| Contract source | the daily master artifact. Hardcoding contract ids remains a REJECT — they expire |

#### The capacity arithmetic, and why ATM±5 is a CEILING not a preference

| Block | Contracts | Slots |
|---|---|---:|
| NIFTY future | 1 | 1 |
| BANKNIFTY future | 1 | 1 |
| NIFTY options ATM±10 | 21 strikes × CE+PE | 42 |
| BANKNIFTY options ATM±10 | 21 strikes × CE+PE | 42 |
| 7 stock futures | 7 | 7 |
| 7 stock ladders ATM±5 | 11 strikes × CE+PE × 7 | 154 |
| **Total** | | **247** |
| Ceiling (`5 × 50`) | | 250 |
| **Spare** | | **3** |

ATM±6 for stocks gives 275 — **over by 25**, and `plan_pool` refuses the WHOLE
pool fail-closed rather than truncating, which is a session-ending failure. So
±5 is the arithmetic ceiling in the operator's stated shape, not a judgement.

**⚠ The packing caveat, stated because it changes the answer.** 250 is
`5 sockets × 50` and a contract cannot straddle a socket. Under FREE packing
(contracts fill any socket) 247 fits. Under SOCKET-AFFINE packing (all of one
name's contracts on one socket, so a rotation touches one socket) NIFTY takes 43
of 50 and BANKNIFTY takes 43 of 50, stranding 14 slots, and 7 × 23 = 161 does not
fit the remaining 150 — ATM±4 (19/name → 133) does. **Free packing is what
ships**, because the wider ladder is worth more than the cheaper swap at a
one-minute cadence, and `plan_pool` already packs the main feed this way.

#### ⚠ The swap clock — the honest cost, and why the band exists

| Fact | Value |
|---|---:|
| Swaps per socket per minute | 4 (`MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE`, const-asserted = channel depth) |
| Pool-wide per minute | **20** |
| One stock name rotating out | 23 out + 23 in = **23 swaps** |
| Minutes to apply ONE name change | **2** |
| Whole board turning over | 250 ÷ 20 = **12.5 minutes** |

A percentage-move key re-orders in BOTH directions every minute, unlike
cumulative volume which only ever rises. **Without a band the board would chase a
list it can never match**, and that is not a hypothetical: the 2026-09-11 (SECOND)
section measured depth-200 at 93.5% of its swap cap with a 1.07-minute mean hold,
on a key that at least rises monotonically.

`DEPTH20_NAME_EXIT_RANK = 12` is the remedy the 2026-09-07 lock already
prescribes verbatim — *"the answer is a longer window or a hysteresis band on
entry/exit"* — applied at the NAME level: a name entered as a top-7 keeps its 23
slots until it falls out of the top 12 (~6% of the ~208 live F&O underlyings).
It is the same shape as depth-20's existing 250/300 contract band and
depth-200's 5/20 name band, and it is **not derived** — no measurement of
minute-to-minute rank drift on this key exists, because no session has ever
ranked on it. `tv_depth20_name_swaps_total{outcome}` is the read-out that tunes
it.

#### ⚠ The honest envelope (mandatory per operator-charter §F)

**Pre-open ranking is the cleanest part of this design and the operator is right
about it.** Volume is zero for everything before 09:15, which is exactly why the
2026-09-06 lock's own REJECT row calls a pre-open volume ranking *"meaningless"*.
A percentage-move ranking has no such failure mode: both its inputs are live from
09:00. The `within_capture_window` gate that starts at 09:15 exists to stop a
midnight-spanning process publishing YESTERDAY's volumes — a volume-specific
hazard — so opening the percentage ranking earlier is a narrow, reasoned unlock
and NOT a weakening of that gate, which stays exactly as it is for the volume
board.

**NOT claimed — pre-open coverage is partial, and by how much is measured.** The
2026-08-28 measurement in this file records that ~750 equities deliver one stale
snapshot at ~08:30 (rejected, outside the window) and then **nothing until the
09:07 auction print**. So between 09:00 and 09:07 most stock underlyings have no
spot price and cannot be ranked at all; the board fills from whatever HAS printed
and completes after the auction. An instrument with no spot must rank NOTHING —
never zero, which would tie it with a genuinely flat name and hand it a socket.

**NOT claimed — that this improves capture.** 2026-09-10 and 2026-09-11 both
ended with every depth socket at `GHOST_REDIAL_SESSION_CEILING` and 5,250,076
ghost packets still arriving, because Dhan ignored the unsubscribe on BOTH code
25 and code 24. **Every swap this design performs is a swap whose unsubscribe the
vendor is currently proven to ignore.** Fewer swaps means fewer ghosts, and a
name-level band means far fewer swaps than a contract-level board — so this
design is strictly better for that failure than what it replaces. It does not fix
it, and the support ticket the 2026-09-11 section calls for remains the only
real remedy.

**NOT claimed — that a thin stock has a ±5 ladder.** 81 of 210 underlyings
measured 2026-08-27 have ladders where ±25 already takes EVERY strike that
exists. ±5 is 11 strikes, well inside that, so it is almost always available —
but `fit_atm_window` returning fewer strikes than asked must fill the remainder
from the next-ranked name, never leave slots idle and never silently narrow
another name's window.

**NOT claimed — that percentage move is the right key.** It is the operator's
key. The measured argument for volume was that it finds the BUSIEST book;
percentage move finds the most MOVED name, which is a different and equally
defensible question for a depth capture. Recorded so the trade is on the record.

#### ⚠ What this quote does NOT authorize

- **Any change to depth-200.** Its key, band, budget and stock-options-only
  restriction are untouched.
- **Any deletion of SEBI or audit rows** — `instrument_lifecycle`,
  `instrument_lifecycle_audit`, `index_constituency`, `order_audit`,
  `order_update_events`, `position_update_events`, `ws_event_audit`. A general
  "go ahead" is exactly the shape §5-class REJECT lists name as insufficient.
- Any change to the socket or instrument budget (250 + 5 remain).
- Any fifth Dhan endpoint type, or more than 16 total connections.
- Raising the apply cadence — Quote A withdraws that ask outright.
- Live order fire; `dry_run` stays true.
- Any edit to the §28 frozen indicator/strategy area.
- Widening `PrevCloseStore` to `NseFno` — the 2026-09-09 section's one
  never-open row. The ranking uses the UNDERLYING's prev close, which is
  `NseEquity` and already written.
- Depth on `BSE_FNO`.

#### What a PR that violates this section looks like (REJECT)

- Uses a **float** anywhere in the sort key. The 2026-09-07 lock's reasoning
  binds unchanged: a non-finite comparator is non-transitive and corrupts a sort
  wholesale, and `prev_close = 0` is a proven NaN source in this repository.
  Integer basis points remove the failure mode rather than guard against it.
- Ranks a name whose spot or prev close is missing as 0% — that ties it with a
  genuinely flat name and hands it a socket it did not earn.
- Divides by a zero or non-positive prev close without refusing and counting.
- Removes the name-level exit band, or lets a name lose its 23 slots on a single
  minute in which it slipped to rank 8.
- Applies the ranking from the frame drain, or more than once a minute.
- Raises the apply cadence citing this section — Quote A withdraws the ask.
- Lets a stock name displace NIFTY or BANKNIFTY, which are unconditional.
- Ships ATM±6 or wider for stocks (275 > 250, and the pool is refused whole).
- Hardcodes contract security-ids.
- Reports the pool as enabled while its instrument set is empty — including the
  09:00–09:07 window, where most equities have not printed.
- Changes depth-200 under cover of this quote.

### 2026-09-11 (SECOND) — THE VENDOR DOCUMENTATION SETTLES THE UNSUBSCRIBE CODE: it is 25, and 24 exists nowhere

**The operator's instruction (2026-09-11, with four Dhan PDFs and a complete
10,263-line documentation dump attached — preserve EXACTLY, typos included):**

> "even take this also for reference and cross verification dude okay? i dont have the confidnece how wdo you assur eme dude i ene dth real tiem proevn gauarbteed assured verificatio n dude okay?"

He supplied the vendor's own current documentation and asked for cross
verification rather than assertion. This section is that cross verification, and
it **overturns the 2026-09-10 change made by this repository.**

#### What the vendor's own documentation says

Pulled from `docs.dhanhq.co` on 2026-09-11 at 9:05 PM and supplied by the
operator. The Annexure's **Feed Request Code** table, verbatim and complete:

| Code | Action |
|---|---|
| 11 | Connect Feed |
| 12 | Disconnect Feed |
| 15 / 16 | Subscribe / Unsubscribe — Ticker Packet |
| 17 / 18 | Subscribe / Unsubscribe — Quote Packet |
| 21 / 22 | Subscribe / Unsubscribe — Full Packet |
| **23** | **Subscribe — Full Market Depth** |
| **25** | **Unsubscribe — Full Market Depth** |

**The table skips 24.** And a search for the literal `24` as a request code
across **all 10,263 lines** of the complete v2 documentation returns **ZERO**.

The Full Market Depth guide itself is narrower still: it documents
`RequestCode 23` (subscribe, in two payload shapes — the 20-level LIST form and
the 200-level FLAT form) and `RequestCode 12` (**Feed Disconnect** — close the
whole socket). It shows **no unsubscribe example at all**, and its own "Response
Fields" tables list `Values: 23` and nothing else.

#### What this overturns

The 2026-09-10 section above changed the constant 25 → 24 and justified it:
*"24 is not a guess: it is the ONLY other value either vendor surface names. The
classic annexure page (stable across every crawl since 2026-06-02) lists
`24 | Unsubscribe - Full Market Depth`."* **The operator's fresh pull refutes
that.** 24 is named by no vendor surface in the documentation he supplied, and
what shipped on 2026-09-10 was therefore an **undocumented code**.

The constant is restored to **25** in the same change as this section, along
with the three rule files and the builder docblock that carried the 24 claim.

#### Why the restore is right even though 25 does not work either

Both codes are now proven ignored, and — the load-bearing fact — **they fail
IDENTICALLY**: 80 `unsubscribe_ignored` lines, split 40/40 across depth-20 and
depth-200, on all ten sockets, every socket reaching
`GHOST_REDIAL_SESSION_CEILING`, on each of two consecutive sessions. A code that
was merely *wrong* would be expected to differ from another wrong code in at
least one dimension. **Two byte-identical signatures is the evidence that the
RequestCode is not the variable.**

> **⚠ CORRECTED 2026-09-11 (same day) — the "byte-identical signature" argument
> is CIRCULAR and must not be put in a vendor ticket.** 80 = `GHOST_REDIAL_
> SESSION_CEILING` (8) × 10 depth sockets, and the 40/40 split is 5+5 sockets ×
> 8: the line is emitted only inside the `Ok(())` arm of `request_ghost_redial`,
> which refuses past the ceiling. Two saturated ceilings are identical by
> construction, whatever the vendor did. The unbounded counters (`ghost`,
> `unsubscribed_grace`) are the only discriminating numbers and were recorded
> for one day only. **The CONCLUSION may well be right; the evidence offered
> cannot support it.** Full correction, the one-socket probe that settles it in
> ~4 minutes, and the untried `RequestCode 12`: see the dated block under
> "⚠ RE-MEASURED 2026-09-11" above.

When neither value works, the value's job changes. It stops being *"make it
work"* and becomes *"make the vendor ticket unarguable"* — and
*"we send the code your own annexure documents, and you ignore it"* is
unarguable, while *"we send 24"* invites the reply *"24 is not a code"*. Shipping
the documented value is also the only position that survives Dhan implementing
the unsubscribe later without us noticing.

#### Two further facts from the same documents, recorded because they close open questions

1. **Depth breaks the `subscribe_code + 1` rule, by the vendor's own table.**
   Ticker 15→16, Quote 17→18, Full 21→22, but depth 23→**25**. That asymmetry is
   the vendor's, is now pinned by test, and must not be "corrected" back to 24 by
   a future reader restoring the pattern.
2. **Dhan documents no per-instrument depth unsubscribe in the depth guide at
   all** — only `12`, which closes the socket. That is consistent with the wire
   behaviour we measured, and it means **disconnect-and-resubscribe may be the
   only mechanism the vendor actually implements** for changing a depth socket's
   set. That bears directly on the socket-layout question and is recorded here
   rather than left to be rediscovered.

#### ⚠ What is NOT claimed

That 25 works. It does not. This change does not stop a single ghost packet, and
the depth sockets will keep receiving contracts this process unsubscribed until
Dhan honours the request or the redial rebuilds the socket. The verdict
instrument is unchanged: a session reading `ghost = 0` with
`unsubscribed_grace > 0` on `tv_dhan_feed_depth_total`.

**The apply cadence still does NOT move**, and the vendor ticket remains the
only real remedy — now with the vendor's own annexure as its first exhibit.

#### What a PR that violates this section looks like (REJECT)

- Ships 24, or any other value absent from the vendor's Feed Request Code table.
- "Restores" the `subscribe_code + 1` pattern for depth — the vendor's table
  goes 23 → 25, and a test pins the asymmetry.
- Ships a third guessed code instead of opening the vendor ticket.
- Cites the 2026-09-10 section's "the classic annexure lists 24" claim without
  re-pulling the documentation — that claim is refuted by the operator's own
  2026-09-11 full-doc pull.
- Reports the restore as a fix for the ghosts. It is a correctness fix for what
  we SEND, not a repair of what Dhan DOES.

### 2026-09-11 (FOURTH) — SIX MOVERS WITH THEIR SPOT, INDEX AT ATM ±11; AND THE SOCKET HANG-UP IS REFUSED ON MEASURED EVIDENCE

**The verbatim operator demands (2026-09-11, typed directly in-session — preserve
EXACTLY, typos included):**

**Quote A (the shape):**
> "sso can we goa head with as per your reocmmedation dude which is top 6 alone dude see in that top 6 try ot add its udnerlying spot also dude okay? so now can we can duretcly disocnenct and reocnnect the ntire socket within a seocnd rigth dude for evry minute chekc rigth dude am i rgith dude see that too frehsly you can check this precisley on tuesdya rigth dude am i rgith dude tell me dude okay? ... see meanwhile to fill uo th entire index slots can we add one which is instea dof atm plus or minus 10 can we go ahead with plus or minus 11 dude okay?"

**Quote B (the authorization):**
> "whatve ror whichevr is reocmmended from your side go ahea ddude okay?"

Quote B was given in DIRECT response to a message that ended with exactly one
enumerated question — *"drop the socket hang-up and raise the swap budget
instead — yes or no?"* — after the five findings below were put to him in full.
That is the §28.2/§28.3 authorization shape this repository already accepts: a
general go-ahead answering an ENUMERATED ask selects the enumerated work.
Recorded HERE, before the code, per the rule-file-first law.

#### What this AUTHORIZES

| Surface | 2026-09-11 (THIRD) | Now |
|---|---|---|
| Stock movers | **7** | **6** (`DEPTH20_NAME_ENTRY_RANK`) |
| Per stock name | future + options ATM±5 = **23** | **spot + future + options ATM±5 = 24** |
| Stock spot segment | banned (`NSE_FNO only`) | **`NSE_EQ` ADMITTED for the six movers** |
| Index ATM window | ±10 → 43 slots | **±11 → 47 slots** |
| Board cost | 247 of 250 | **238 of 250**, 12 spare |
| Apply cadence | once a minute | **unchanged — once a minute** |

Everything else in the (THIRD) contract STANDS unchanged: NIFTY and BANKNIFTY
unconditional and never displaced; ranking on the ABSOLUTE percentage move of
the UNDERLYING in integer basis points; the name-level hysteresis band at
`DEPTH20_NAME_EXIT_RANK = 12`; NSE_FNO for every contract; no BSE; no index
spot; no hardcoded contract ids; `dry_run` true; §28 frozen.

#### ⚠ The arithmetic makes the three changes ONE change

`slots_for_name(N) = 1 + (2N+1) × 2` (`depth20_name_board.rs:317`).

| Shape | Board cost | Verdict |
|---|---|---|
| index ±10, 7 stocks ±5 (authorized) | 2×43 + 7×23 = **247** | fits |
| **index ±11, 7 stocks kept** | 2×47 + 7×23 = **255** | ❌ **the compile-time assert at `:331` FAILS THE BUILD** |
| index ±11, 6 stocks, no spot | 2×47 + 6×23 = **232** | fits, 18 spare |
| **index ±11, 6 stocks + spot** | 2×47 + 6×24 = **238** | ✅ fits, 12 spare |
| index ±12 + future | `slots_for_name(12)` = **51** | ❌ one socket over on its own |

So ±11 cannot ship with seven names, and the freed slots cannot buy a wider
stock ladder either (6 × 27 + 94 = 256) or a seventh name (7 × 24 + 94 = 262).
**Spot is the only thing that fits in the room ±11 creates.** Six is forced by
the budget, not chosen.

**Honest note on direction:** ±11 is a step UP from the (THIRD) authorization
(±10) and a step DOWN from what is LIVE today — `depth20_layout.rs:58` runs
`DEPTH_20_INDEX_STRIKES_EACH_SIDE = 12` with **no index future** (50 option legs
filling the socket). Against the live shape this trades 2 strikes each side for
the index future that centres the ATM window under the 2026-09-09 lock.

#### ⚠ WHAT THIS REFUSES — the socket hang-up, and why

Quote A proposes replacing the vendor-ignored per-instrument unsubscribe by
CLOSING a depth socket and re-dialling it with a changed set, once a minute.
**That is REFUSED**, and the refusal is the substance of Quote B. Five findings,
all measured or in source:

| # | Severity | Finding | Evidence |
|---|---|---|---|
| 1 | **FATAL** | No deliberate-close concept exists. `ConnEvent` has 11 variants; none means "our set changed" | `pool_supervisor.rs:614-647` |
| 2 | **FATAL** | Every redial is recorded as a flap, with no way to mark one intentional — `enter_backoff` is the single site and records unconditionally | `pool_supervisor.rs:1569-1577` |
| 3 | **HIGH** | Once a minute sits at 5 of a ceiling of 6. One vendor drop that minute breaches it → forced 30 s floor, socket classed pathological | `reconnect_ladder.rs:159,182,188` |
| 4 | **HIGH** | A rebuilt socket counts as healthy only once a FRAME arrives — not on dial, not on ack. A thin book silent for 30 s makes the NEXT rebuild a short-session flap | `reconnect_ladder.rs:140,321-325` |
| 5 | **HIGH** | The blind window is NOT the 0.31 s transport redial. The India feed has **no snapshot-on-subscribe**, so a re-subscribed contract is BLANK until its book next changes; the tracker gives up at `FIRST_PACKET_WINDOW_SECS = 120`, and a 09:50 delivery cliff where no new contract delivered at all is already measured | `depth_first_packet.rs:14-17,177` |

> **⚠ CORRECTED 2026-09-12 — finding 1's variant COUNT was wrong, and it is the
> one number in this table a reader can check in a second.** `ConnEvent` had
> **12** variants when this table was written, not 11 — and has **13** since the
> probe added `ProbeCloseRequested`. Counted rather than quoted:
> `BeginDial · DialSucceeded · DialFailed · SubscribeAcked · SubscribeFailed ·
> FrameReceived · KeepAliveReceived · Disconnected · IdleElapsed ·
> FrameSilenceElapsed · GhostInstrumentDetected · ProbeCloseRequested ·
> ShutdownRequested`. The line:column citation is also stale — the enum has
> moved since.
>
> **The FINDING is UNCHANGED and is re-verified in source**: no variant meant
> "our set changed", which is what makes the refusal correct. Only the count was
> wrong, and it was wrong in the direction that looks careless rather than the
> direction that misleads — but this file has now recorded the same shape five
> times (the byte budget, the $130 ceiling, the September forecast, the
> AccessDenied flag, the 80/40/40 signature), and the lesson each time is the
> same: **a count is a measurement, and a measurement quoted from memory is not
> one.** `awk '/^pub enum ConnEvent/,/^}/'` answers it.
>
> **What the 2026-09-12 probe section changes about finding 1, precisely:** it
> ADDS the deliberate-close variant this finding says does not exist — scoped to
> the probe alone, unreachable from the steering loop, and a REJECT to widen.
> The refusal of the ROUTINE mechanism stands untouched: what it refuses is five
> sockets every minute for 375 minutes, and the probe is one close per socket per
> session, which is finding 3's own arithmetic read the other way (1 of 6, not
> 5 of 6 — and 0 of 6 once finding 2 is neutralised by the `records_flap()`
> condition INSIDE `enter_backoff`, never a bypass of it).
**And the vendor evidence points the same way, harder.** Dhan documents 805 as
*"Too many requests or connections. Further requests may result in the user
being blocked"* (`docs/dhan-ref/08-annexure-enums.md:348`), our code parks a
805'd socket PERMANENTLY (`pool_supervisor.rs:534,1285`), and
`docs/dhan-support/2026-06-01-live-feed-429-from-cloud-ip.md:52` records this
very account being refused with **HTTP 429** on the feed, our own hypothesis
being *"our reconnect logic retried too aggressively"* — with the question *"is
there a cap on new connection attempts per minute per dhanClientId?"* sent to
Dhan and **never answered**. Hanging up five sockets once a minute is ~300
connection attempts per session against that unknown.

**A PR that adds a deliberate-close-and-redial path for depth is a REJECT**
without its own fresh dated quote that engages findings 1-5 by name.

#### The REPLACEMENT, authorized in its place

The problem Quote A was solving is real and measured: **moving one name takes
six minutes.** A name is 24 contracts and the per-socket budget is
`MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE = 4`
(`depth20_ranked_steer.rs:87`), which is not a wire limit — it is const-asserted
equal to `DEPTH_SWAP_COMMAND_CHANNEL_DEPTH`, a queue depth. **Raising both so a
whole name moves in one minute is authorized**, with three binding conditions:

1. **It ships WITH the name board, never before it.** The current volume-keyed
   contract board already refuses 24,607 swaps a session against 7,662
   performed (MEASURED 2026-09-11) — raising the cap under THAT board
   multiplies ghosts fourfold for nothing.
2. **The cap stays const-asserted `<=` the channel depth.** A cap above the
   queue is not a cap.
3. **Socket-affinity is NOT adopted.** It was only ever needed to make hanging
   up cheap; with hang-up refused it actively harms, because a name confined to
   one socket draws on one 4-swap budget while a freely-packed name draws on
   several. `plan_pool`'s flat `chunks()` shard (`dhan_feed_stack.rs:848-852`)
   therefore stays as it is.

**Why more ghosts is the safe direction, MEASURED across two full sessions:**
sockets carried 50 wanted + ~25 ghosts = 75 against a documented cap of 50, for
6.4 hours, and `tv_dhan_ws_park_total` was **0 on every reason**,
`tv_dhan_ws_subscribe_failed_total` **0 on all eight reasons including all four
`unsubscribe_*`**, with zero 804 and zero 805. Ghost cost is ~13.5% of depth
rows ≈ 2.5% of the session's disk burn.

#### ⚠ Honest envelope (mandatory per operator-charter §F)

- **The 09:00 pre-open requirement CANNOT be met, and no code can meet it.**
  MEASURED (`:2072`, 2026-08-27): equities deliver one stale ~08:30 snapshot
  carrying YESTERDAY's timestamp — hard-refused since 2026-09-10 — and then
  **nothing until the 09:07 auction print**. That print carries the LTP and the
  previous close on the SAME packet, so ~208 names become rankable together at
  ~09:07, eight minutes before the bell. 09:00-09:07 the pools hold the
  previous session's validated seed (`depth_rebalance.rs:1673`, reason
  `seed_until_first_ranking`) — which is already built and already wired, and
  is the correct behaviour, not a gap.
- **A name with a missing price ranks `None`, NEVER 0%** (`:194`, pinned by
  `a_missing_price_is_not_rankable_and_is_never_zero`). Between 09:00 and 09:07
  that is most of the equity universe, and ranking them 0 would tie them with
  genuinely flat names.
- **NSE_EQ depth is UNVERIFIED-LIVE.** Dhan documents it as supported and uses
  it as their own subscribe example (`04-full-market-depth-websocket.md:13,274`
  and the `"NSE_EQ","SecurityId":"1333"` samples at `:84,:96`); our guards
  already admit it (`subscription_builder.rs:437-462`); the parser stores the
  segment byte raw and `segment` is in the depth DEDUP key. **But no session has
  ever sent one.** `IDX_I` by contrast is REFUSED at build time by our own
  guard, which is why index SPOT can never join a depth socket.
- **Stock spot depth is an INCREMENT, not new coverage.** The main feed runs
  Full mode and already persists 5 levels of every equity's book via
  `append_inline_depth` (`dhan_feed_stack.rs:6375`). This buys levels 6-20.
- **Name-board churn is UNMEASURED** — the board has never run, so how often the
  top 6 changes behind a band at rank 12 is Unknown. The swap counters are the
  read-out and **they reach no AWS surface at all** (MEASURED: absent from the
  namespace AND from the EMF group), so today they are unalarmable.
- **NOT claimed:** that any of this improves capture. The unsubscribe remains
  broken on the vendor's side; the fix is the support ticket, which is itself
  blocked because no ghost log line carries a `security_id`.

#### What a PR that violates this section looks like (REJECT)

- Adds a deliberate socket close-and-redial path for depth without a fresh dated
  quote engaging findings 1-5 by name.
- Ships index ±11 while keeping seven movers (the build fails; do not "fix" it
  by raising `DEPTH20_INSTRUMENT_BUDGET`, which is 5 sockets × 50 and is the
  vendor's number).
- Puts an `IDX_I` instrument on a depth socket, or a BSE segment.
- Raises the swap cap before the name board is wired, or above the command
  channel depth, or without keeping the const-assert.
- Adopts socket-affine packing for depth-20 on the strength of this section.
- Ranks a name with a missing price as 0%.
- Presents the board as rankable at 09:00, or reports an empty 09:00 board as a
  defect rather than as the exchange's own timetable.
- Claims NSE_EQ depth works before a session has actually delivered one.

### 2026-09-12 — THE TWO-ARMED UNSUBSCRIBE PROBE: code 25 CONFIRMED, a deliberate close is AUTHORIZED for the probe only, and a vendor report is drafted on failure

**The verbatim operator demands (2026-09-12, typed directly in-session — preserve
EXACTLY, typos included):**

**Quote A (the methodological point, which is the authority for the two-armed shape):**
> "Why bro youdidnf include so key disconnect and reconnect dude because of we don't check both of them then nowhere we will easily identify which one is working right dude"

**Quote B (the authorization, the code decision, and the vendor-report ask):**
> "Yescheck everything dude so whenever something fails especially for unsubscribe we will clearly drop an email and drop the message even in madefortrade also to them dude okay? Meanwhile lets us check all these dude see for unsubscribe clearly note dude which is only 25 dude so we need to check socket disconnect and reconnect dude okay?"

Quote B was given in DIRECT response to a message that enumerated the two-armed
probe, named its one real piece of work (the deliberate close that must not
record a flap), priced its cost at **one extra connection attempt rather than
~300**, and asked for his word before building. That is the §28.2/§28.3
authorization shape this repository already accepts. This dated section is the
rule-file edit the section above demands **by name** — its REJECT list reads
*"Adds a deliberate socket close-and-redial path for depth without a fresh dated
quote engaging findings 1-5 by name"* — and §5 of this file's own law. Recorded
BEFORE the code.

#### Part 1 — the unsubscribe RequestCode is CONFIRMED at 25

Quote B settles the decision that has blocked PR #1909: *"for unsubscribe
clearly note dude which is only 25"*. `FEED_UNSUBSCRIBE_TWENTY_DEPTH = 25`
stands, matching the vendor's own Feed Request Code table (23 subscribe → 25
unsubscribe, skipping 24), and the 2026-09-10 flip to the undocumented 24 stays
retired. **No third code may be guessed at.**

#### Part 2 — WHY the operator is right, and what the one-armed design got wrong

The executor proposed a ONE-armed probe (unsubscribe only). Quote A rejects it,
correctly: a negative result on unsubscribe alone says *"this mechanism failed"*
and cannot say *"the other one would have worked"*. Two mechanisms, one test
each, or the session produces a third negative result and no decision.

**And the executor's own prior answer was WRONG in a way this section corrects.**
A previous turn told the operator that disconnect/reconnect "does not exist" and
would not run on Tuesday. False: `ReconnectReason::GhostInstrument`
(`pool_supervisor.rs`) is exactly disconnect-and-reconnect as a removal
mechanism, it is wired, and it has already run ~80 times per session on two
sessions. What was REFUSED in the section above is disconnect/reconnect as the
**routine swap mechanism** — 5 sockets x every minute x 375 minutes, ~300
connection attempts a session against a 429 question Dhan has never answered.
Conflating a bounded test with an unbounded mechanism is what produced the wrong
answer, and the distinction is load-bearing for everything below.

#### Part 3 — the contract (LOCKED)

| Aspect | Locked value |
|---|---|
| Shape | **TWO arms, one shot each, one session.** Arm A = unsubscribe and do not re-subscribe. Arm B = remove from the retained set, deliberately close, redial so the replay excludes it |
| Scope | **depth-200 only**, because a depth-200 socket holds **exactly one** instrument, so nothing on that socket can mask the verdict. Never depth-20 |
| Sockets | **two**, one per arm, so the arms cannot contaminate each other |
| Runs | **ONCE per session**, operator-armed, never scheduled, never automatic |
| Default | **OFF.** Serde default false; an absent section means the probe does not exist |
| Restore | each arm **re-subscribes its instrument** when its watch window closes, so the pool is never left short for the session |
| Verdict | four outcomes (A silent/not x B silent/not), each recorded with the contract id, the mechanism, the baseline and the observed counts |

#### Part 4 — findings 1-5 of the refusal, engaged BY NAME as that REJECT row requires

| # | The finding (verbatim shape) | Why it does not apply at probe scale, or how it is neutralised |
|---|---|---|
| **1** | *No deliberate-close concept exists. `ConnEvent` has no variant meaning "our set changed"* | **This probe ADDS one, and its scope is the probe.** A new reason variant exists solely so a probe close is distinguishable from a fault. It does NOT become available to the steering loop, and wiring it into a swap path is a REJECT below |
| **2** | *Every redial is recorded as a flap; `enter_backoff` is the single site and records unconditionally* | **This is the one real piece of work.** The probe close must reach `enter_backoff` WITHOUT calling `record_redial`, or a deliberate close poisons the damper and the next genuine fault is mis-damped. The single-choke-point property of `enter_backoff` is PRESERVED — the flap recording becomes conditional on the reason, never a second bypassing path |
| **3** | *Once a minute sits at 5 of a ceiling of 6* | **The probe is once per SESSION, not once per minute — 1 of 6, not 5 of 6.** And with finding 2 neutralised it is 0 of 6. The arithmetic that made the routine mechanism fatal is precisely what makes the one-shot safe |
| **4** | *A rebuilt socket counts healthy only once a frame arrives; a thin book silent for 30 s makes the NEXT rebuild a short-session flap* | **Silence is the probe's SUCCESS signal, so "not proven healthy" is expected, not a defect.** The residual is real and is handled: the socket carries a not-proven-healthy state after Arm B, so the restore step must re-subscribe and the probe must not run inside the last 30 minutes of the session, where a lingering not-healthy state would meet the close |
| **5** | *The blind window is not the transport redial; with no snapshot-on-subscribe a contract is BLANK until its book next changes* | **This is the probe's central measurement hazard and it is designed for, not waved away.** Silence after the action could mean the mechanism worked OR that the book simply went quiet. Therefore each arm MUST measure a **BASELINE FIRST** — frames observed on that socket in the N seconds before the action — and a verdict is only admissible when the baseline proves the book was active. A thin book disqualifies the run, and the probe reports `inconclusive_thin_book` rather than a false positive |

**Finding 5 is the one that decides whether the probe is worth anything.** A
contract delivering 100 frames a minute before the action and 0 after is
evidence. A contract delivering 2 frames a minute is noise wearing the costume
of evidence, and reporting it as a verdict would be the false-OK class this file
exists to stop.

#### Part 5 — the vendor report on failure (Quote B)

When a run ends with a failing verdict, the operator wants Dhan told: *"we will
clearly drop an email and drop the message even in madefortrade also to them"*.

| Surface | What ships | What does NOT ship |
|---|---|---|
| Evidence | the probe writes a complete, committed support draft under `docs/dhan-support/` from the house `TEMPLATE.md`, filled from the REAL run: contract labels, SecurityId per contract, microsecond IST timestamps, the baseline and post-action frame counts, the request code sent, and the verbatim log lines | — |
| Delivery | the verdict is a coded log line and the draft is a committed file; the operator reads both and sends | **No automated send, and NO new Telegram page.** The process does NOT email Dhan, does NOT post to MadeForTrade, and does NOT add a fifth Dhan-scoped alert family |

**Why delivery is NOT automated, stated plainly rather than quietly omitted.**
Three reasons, and none of them is capability: (a) `docs/dhan-support/README.md`
mandates that every technical email is a committed markdown file shared as a
GitHub rendered link, *"never as pasted plain text in Gmail"* — an auto-send
would break the house workflow the operator himself wrote; (b) a message to a
broker's support desk and a public post in their community are **outward-facing
and irreversible**, and a false positive from a thin-book run would spam the
vendor with a defect that does not exist, which costs exactly the credibility
the ticket needs; (c) the draft is worth more than the send — the reason two
prior sessions produced no ticket is that no log line named a contract, not that
nobody could open Gmail.

**If the operator wants the send automated as well, that is its own dated quote
in this section**, and it should arrive only after the first draft has been read
and judged accurate.

#### Part 6 — honest envelope (mandatory per operator-charter §F)

> "The probe answers ONE question: on this account, on this endpoint, does an
> unsubscribe stop the stream, and does a close-and-redial stop it? Four outcomes,
> each decisive in a different direction, each recorded with the contract id and
> the baseline that makes it admissible. **NOT claimed:** that either mechanism
> works — the probe is built precisely because nobody knows. **NOT claimed:** that
> a silent socket proves the mechanism, absent a baseline that proves the book was
> active; a thin-book run reports `inconclusive_thin_book` and no verdict. **NOT
> claimed:** that one session generalises — one run on two sockets on one account
> at one time of day is one data point, and a vendor may behave differently under
> load. **NOT claimed:** that the probe fixes anything. It produces evidence and a
> draft; the remedy is still Dhan's. **NOT claimed:** that `RequestCode 12` is
> covered — the only stop mechanism the vendor's depth guide documents still has
> zero production callers and is NOT in this probe."

#### Part 7 — what a PR that violates this section looks like (REJECT)

- Makes the deliberate-close reason available to the steering loop, the swap
  path, or anything other than the probe — finding 2 of the refusal is
  neutralised **for the probe**, never lifted.
- Adds a second path that bypasses `enter_backoff` instead of making the flap
  recording conditional inside it — that destroys the single-choke-point
  property the guard pins.
- Ships the probe enabled by default, on a schedule, or more than once per
  session.
- Runs either arm on a depth-20 socket (50 instruments mask the verdict).
- Reports a verdict without a baseline, or reports a thin-book run as anything
  other than `inconclusive_thin_book`.
- Leaves an instrument unsubscribed after the watch window — each arm restores.
- Runs inside the last 30 minutes of the session (finding 4's residual).
- **Auto-sends an email to Dhan or auto-posts to MadeForTrade** without its own
  fresh dated quote here.
- Ships a support draft missing any identifier `CLAUDE.md` makes mandatory
  (Client ID, Name, UCC, per-contract SecurityId, microsecond IST timestamps).
- Guesses a third unsubscribe RequestCode.
- Changes the socket or instrument budgets, `dry_run`, or the §28 frozen area
  under cover of this quote.

**Why no Telegram page, stated rather than silently omitted.** A "draft is
ready" page would be a FIFTH Dhan-scoped alert family, and
`dhan-rest-only-noise-lock-2026-07-14.md` §3 makes that a REJECT without its own
dated quote in THAT file — *"Adds ANY new Dhan-scoped Telegram page outside the
§2 4-item set"*. It would also cost ~$0.10/mo against a September forecast of
**$142.24** and an automatic `STOP_EC2_INSTANCES` line at **$135.00**, where
§2.3n requires a LEVER and this carries none. And it buys nothing: the probe is
ONE-SHOT and OPERATOR-ARMED, so the operator already knows it ran. The four-item
Dhan family is UNCHANGED by this section.

#### Part 8 — the collision the design had to be changed for (MEASURED in source, not assumed)

The first design of Arm A would have been **silently contaminated by Arm B**,
and the mechanism is worth recording because nothing about it is visible from
the call site.

`DepthSubscriptionView` classifies an instrument this process dropped, still
arriving past `GHOST_GRACE_SECS` (**90 s**), as a ghost; the drain then calls
`request_ghost_redial(frame.connection_index, …)`. Arm A unsubscribes and
deliberately does NOT re-subscribe — which is **exactly** the shape that
classifies as a ghost. So ninety seconds into Arm A's watch window the ghost
detector would close and redial Arm A's socket, applying **Arm B's mechanism to
Arm A's measurement**, and the resulting silence would prove nothing about
either.

**The fix is a constraint, not a flag:** Arm A's watch window is
const-asserted **strictly shorter than `GHOST_GRACE_SECS`**, so the ghost
machinery can never engage during it. A suppression flag was rejected as the
weaker form — a flag can drift out of lockstep with the grace constant and the
failure would be silent, whereas an assert fails the build.

This is also why the window is short enough to be scientifically sound only
against a baseline: a liquid depth-200 contract delivers on the order of a
hundred thousand rows a minute, so tens of seconds of true silence is
overwhelming evidence, while the same window on a thin book is worth nothing.
That is finding 5, and it is why Part 4 makes the baseline admissibility a
condition rather than a nicety.

### 2026-09-12 — FOUR CADENCES, EVERY TRADED OPTION CONTRACT, RANKED ON VOLUME-PERCENTAGE CHANGE

**The verbatim operator demands (2026-09-12, typed directly in-session — preserve
EXACTLY, typos and expletives included):**

**Quote A (the cadences and the cut):**
> "Bro try to have 1s, 3s and even 5s also dude okay? Do you understand what I'm even asking dude. See if memory is not at all a big deal then have all these dude don't pick top 250 ick the entire options contracts dude okay?"

**Quote B (the fourth cadence, and the latency requirement):**
> "see whatever it is i need O(1) tracking latency espeicllay for this top volume rank dude thta too cosnider to take 1s,3s,5s and even inclduign one minute also dude okay? make eveythign as 100 percentage common runtime dynamic incremental scalable approach"

**Quote C (the ranking key — first statement):**
> "see clealry n toe have the lots and normal price prcenatage chnage but purely do thtis top volume rank purely absed on one an donly with this volume percnetage dude okay?"

**Quote D (the ranking key — sharpened minutes later):**
> "but i dont want this lots dude i just need this volume percentage chnage alone dude okay? but still let us keep this lots and normal price percnetage change dude okay?"

**Quote E (the authorization):**
> "go ahead with this ank on volume-percentage alone for all tehe ntire options contartcs enitlrey that too for every 1s,3s,5s and even 1m inclduign as well dude okay? taht too achieveign this O(1) dude okay?"

Quote E was given in DIRECT response to a published ledger that enumerated this
work, priced it, named the three defects that block it, and stated plainly that
per-sweep O(1) is impossible. That is the §28.2/§28.3 authorization shape this
repository already accepts. Recorded HERE before any code, per the
rule-file-first law.

#### What this authorizes

| Surface | Before | After |
|---|---|---|
| Snapshot cadences | 2 — `1s`, `5s` | **4 — `1s`, `3s`, `5s`, `1m`** |
| Persisted set per family per sweep | top 250 by rank | **every option contract that traded in the window** |
| Named ranking key | `window_lots_milli` (lots ×1000) | **volume-percentage change**, persisted as its own integer column |
| `window_lots_milli` | the key | **KEPT as a column** (Quote D) |
| `gain_pct` (the UNDERLYING's price move) | a column | **KEPT as a column** (Quote D) |
| Sweep iteration | every tracked contract | **only the contracts that traded in that window** |
| Everything else | — | UNCHANGED |

#### ⚠ The ordering does not change, and that is the point

The volume-percentage change and the lots figure are **monotone transforms of one
another** — `net_volume_chg_pct = window_lots_milli / 10 − 100`, so the two differ
by a fixed scale and offset and rank identically on every row. Ranking "purely on
volume percentage" therefore produces the **byte-identical order** that ships
today. What changes is that the figure is named, stored, and ranked-by in the
table rather than derived only in a view.

**The comparator MUST keep sorting the integer.** This file's own REJECT list bans
a float in a sort key — *"a non-finite comparator is non-transitive and corrupts a
sort wholesale"* — and `gain_pct` is an `f64` produced by dividing by a previous
close. Because the two forms are monotone transforms, sorting the integer and
naming the result by the percentage is not a compromise: it is the same answer,
reached safely. The persisted percentage column is likewise an INTEGER
(milli-percent), never a float.

#### ⚠ O(1): what is granted, and what is arithmetically impossible

Quote B asks for "O(1) tracking latency" and Quote E for "achieving this O(1)".
Three operations hide under that phrase and they do not have the same answer:

| Layer | Runs | Complexity | Verdict |
|---|---|---|---|
| **Track** — record one contract's volume | per tick | **O(1)**, one hash probe, zero allocation, map pre-sized so it cannot resize | **already met, and four cadences do not change it** |
| **Collect** — which contracts traded in this window | per sweep | O(tracked) today → **O(traded)** | **this is the work** |
| **Order** — assign a rank | per sweep | O(m log m) | inherent |

**Per-sweep O(1) is impossible and is NOT claimed anywhere.** The sweep's output
is one row per contract that traded; producing m rows costs at least m. Any design
that claims otherwise is silently reintroducing a top-k cut, which is exactly what
Quote A removes.

**What IS granted and delivered: O(1) in the size of the universe.** The sweep
stops scaling with how many contracts EXIST and scales only with how many TRADED.
Adding 20,000 quiet contracts costs nothing. The mechanism is a per-window dirty
set: the tick marks a contract when its volume actually advances (a bit test and a
bounded push on the accepted arm only, allocation-free because the buffers are
pre-sized and the bitmask caps them at one push per contract per window), and the
sweep drains only that buffer.

**The correctness invariant this rests on, stated because it was written down
nowhere:** a zero delta means the baseline already equals the volume, so the
sweep's baseline write for an untraded contract is a no-op. It holds because every
path that writes volume also settles the baseline. **It must be pinned by a test
in the same change**, or a later unrelated edit can break it and the dirty-set
sweep starts silently dropping contracts from the board.

#### ⚠ CORRECTION — the 900 µs figure this file and CLAUDE.md both cite is not a measurement

`rank_sweep_cost_at_the_authorized_ceiling` seeds its contracts through `observe`,
which sets every baseline equal to the contract's volume. Every subsequent `rank`
therefore computes a zero delta, which is dropped before the sort — **the sort ran
over an empty list on all fifty measured rounds.** The harness additionally passes
a constant stub where production performs an atomic load plus a hash probe per
contract per sweep to fetch the lot size.

So the 900 µs omits **both the sort and the per-contract probe** — the two costs a
wider window and an uncapped set increase. The true sweep cost is higher by an
unknown factor. Two further errors in CLAUDE.md's O(1) table, found alongside: it
states the measurement was taken at **25,000** contracts when the harness uses
**20,220**, and it states the database appends are "inside the measured 900 µs"
when the harness executes no append at all.

**The harness must be repaired in the same change** so it ranks contracts that
actually traded and uses a real lot-size lookup. No duty-cycle claim may cite the
old number.

#### ✅ REPAIRED AND MEASURED 2026-09-12 — and the true ceiling is 3.3× WORSE

`rank_sweep_cost_at_the_authorized_ceiling` now advances contracts before every
timed round, probes a real lot-size `HashMap` per contract (production resolves
through `global_contract_underlying_map().owner_of(..)` — a global load plus a
probe, which the constant `lot1` stub let the optimiser delete), times ONLY the
sweep, and **ASSERTS it filled the board** — so it can no longer report a number
from an empty sort. Release, x86 dev container, 20,220 tracked contracts:

| contracts that TRADED in the window | sweep | duty at 1 s |
|---|---:|---:|
| 20,220 — every one (the ceiling) | **2.95 ms** | 0.295% |
| 2,000 (Assumed realistic) | **123 µs** | 0.012% |
| 500 | 28.7 µs | 0.003% |
| 100 | 6.4 µs | 0.0006% |
| 20,220, full rank + distinct(5) | 2.50 ms | 0.050% at 5 s |

**The withdrawn figure was optimistic by 3.3×, not pessimistic.** Two separate
omissions compounded: the empty sort, and the folded-away lot probe — which
alone accounts for 1.78 ms → 2.95 ms, measured by running the repaired harness
once with the stub and once with the probe.

**What the dirty sets buy, on these numbers:** 2.95 ms → 123 µs at the assumed
realistic shape, a **24× reduction**, and 460× at 100 traded. The worst SECOND —
all four cadence arms landing together, both families, every contract trading —
is 8 sweeps ≈ **23.6 ms, a 2.4% duty cycle** on the drain task; at the realistic
shape it is ~1 ms, ~0.1%.

**Still Assumed:** the 2,000-traded row. Nobody has measured how many of ~20,000
strikes trade in one second; the measuring query is named in
`top_volume_rank_persistence`'s header. It is SWEPT here rather than asserted,
so a reader sees the shape instead of one number they would then quote.

#### ⚠ The defects that must be fixed BEFORE a cadence is added

1. **The compile error trains you to write the bug.** Adding a cadence correctly
   fails to compile at the cadence→slot mapping. The obvious fix — map the new one
   to slot 2 — compiles, while the per-contract baseline array is still sized from
   a SEPARATE hand-written `ALL` list nobody was forced to touch. The result is an
   out-of-range write on the frame drain; the release profile aborts rather than
   unwinds, so the process dies mid-session and takes tick capture with it.
2. **The guard for it cannot catch it.** `every_cadence_is_in_the_all_list`
   iterates `ALL` — the very list that would be missing the variant — and matches
   on the wire label rather than on the type. Its own comment claims it fails the
   exhaustive match. It does not.
3. **The views are on a separate wire.** `console_views::TopVolumeCadence` is a
   SECOND cadence enum with its own hand-written `ALL`, linked to
   `SnapshotCadence` by nothing. A new cadence would write rows whose view is
   never created, and the guarding test passes green because it checks its own
   list against itself.
4. **A missing timer arm is caught by nothing.** Forget a `select!` arm and that
   cadence simply never fires — no error, no counter, no log.
5. **The tracked/ranked gauges carry no cadence label**, so four cadences alias
   into one series, last writer wins.

**The sanctioned fix for 1–4 is ONE shape: a single declaration list from which
the enum, `ALL`, the labels, the interval seconds, the array index and the view
name are all generated**, so the compiler enforces agreement instead of three
hand-written lists and tests that check each against itself. Adding a cadence must
become a one-line edit.

#### What this does NOT authorize

- **Any change to depth steering.** `TOP_VOLUME_RANK_PER_FAMILY` is ALSO
  `DEPTH20_ENTRY_RANKS`. The persistence cut is removed by changing the
  PERSISTENCE bound; the constant itself stays pinned at 250 and depth-20 keeps
  entry 250 / exit 300. Depth-200 is untouched, as every prior section states.
- Any change to the socket or instrument budgets (250 + 5 remain).
- Any change to the depth apply cadence — Quote A of 2026-09-11 (THIRD) withdrew
  that ask and it stays withdrawn.
- Live order fire; `dry_run` stays true.
- Any edit to the §28 frozen indicator/strategy area.
- Any deletion of SEBI or audit rows.
- Any new CloudWatch metric name or alarm. This pipeline's nine counters reach no
  deployment surface today, and adding one costs ~$0.30/mo against a September
  forecast of $142.24 with an automatic `STOP_EC2_INSTANCES` line at $135.00.
  §2.3n of `dhan-rest-only-noise-lock-2026-07-14.md` requires a LEVER, not a cost
  note. The observability gap is RECORDED here and deliberately NOT closed.

#### Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: the
> per-tick path is O(1) and allocation-free, pinned by a build-failing DHAT gate;
> the cadence list, its labels, its intervals, its array index and its view names
> are generated from one declaration so they cannot drift; the zero-delta
> invariant the dirty set depends on is pinned by its own test; the persisted
> percentage column is an integer and the comparator stays integer-only.
> **NOT claimed:** per-sweep O(1) — impossible, and the honest floor is Θ(traded).
> **NOT claimed:** any duty-cycle figure derived from the old 900 µs harness —
> withdrawn, and replaced by the repaired harness's measured 2.95 ms ceiling /
> 123 µs realistic. **NOT claimed:** that the 2,000-traded figure those
> realistic numbers assume is measured — it is not, and it is swept rather than
> asserted for exactly that reason.
> **NOT claimed:** measured row counts for the 3s, 5s or 1m windows without the
> cut — those are modelled from one session's distinct-instrument count; only the
> 1-second case was ever read from the database. **NOT claimed:** that any of this
> has run at a market open with four cadences. **NOT claimed:** that the pipeline
> is observable outside the box — all nine of its counters reach zero deployment
> surfaces, and three of its failure modes are entirely silent."

#### ⚠ The 1-minute boundary, decided rather than discovered

The sweep timers start when the drain starts, not on a window boundary. At one
second the resulting quantisation error is at most one second. **At sixty seconds
the first in-window row can measure from up to 59 seconds before the open, and the
last 0–59 seconds before the capture cutoff land in NO one-minute row at all** —
the next sweep is out of window and rolls the baseline away. This is accepted as
the cost of a fixed-interval timer, it is recorded here so it is not later
reported as a defect, and a future change may align the 1-minute timer to the
wall-clock minute under its own note.

#### What a PR that violates this section looks like (REJECT)

- Sorts on a float, or persists the percentage as a float.
- Changes `TOP_VOLUME_RANK_PER_FAMILY`, or otherwise moves depth-20 entry/exit.
- Adds a cadence without the single-source declaration, or leaves either the
  second cadence enum or the self-referential guard in place.
- Ships a dirty-set sweep without a test pinning the zero-delta invariant.
- Cites the old 900 µs figure as a measurement, or ships the cadences without
  repairing the harness.
- Claims per-sweep O(1) anywhere, in code, comment, commit message or PR body.
- Removes the persistence bound entirely rather than raising it — the write path
  has no spill tier, and a batch too wide is DROPPED.
- Adds a CloudWatch metric name or alarm without a lever, per §2.3n.
