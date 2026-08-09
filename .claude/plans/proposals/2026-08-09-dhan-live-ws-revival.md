# Proposal: revive the Dhan live main-feed WebSocket (the only tick source this account can reach)

**Status:** DRAFT — implementation may NOT start until the operator flips this to APPROVED
**Date:** 2026-08-09
**Approved by:** pending
**Scope authority:** `.claude/rules/project/websocket-connection-scope-lock.md`
→ "2026-08-09 — DHAN LIVE MAIN-FEED WS REVIVAL AUTHORIZED" (operator quote recorded there
verbatim, reversing the 2026-07-13 retirement)
**Guarantee matrices:** the 15-row + 7-row matrices of
`.claude/rules/project/per-wave-guarantee-matrix.md` apply to every PR below
(cross-referenced, not re-pasted).

---

## Why this exists, in one paragraph

TrueData is explicitly excluded from this instance (operator, 2026-08-09). Groww live WS
is retired. GDF never started. So the Dhan live main-feed WS is the **ONLY tick source
this account can reach** — and without ticks there are zero 1s/5s/10s/15s/30s candles and
the r8g.xlarge 32 GiB holds 1.5 MB. Reviving it is what makes the Quote 13 sizing
coherent rather than 25 GB of idle RAM.

## ⚠️ The honest headline — this does NOT fix why it was retired

The operator selected this option from a table whose own text named the reason, so the
acceptance is informed. Recording it anyway, because a future reader will ask:

| Measured 2026-07-06 (776 SIDs, Quote mode, full session) | Value |
|---|---|
| Delivery lag, exchange→us | p50 1.38 s · p95 14.93 s · **p99 46.37 s · max 198.69 s** |
| **Groww, same host, same minutes** | **p99 562 ms — ~82× better** |
| Silent instruments per minute | **29–67**, gaps 300–978 s, 590 events |
| Live vs Dhan's OWN historical candles | operator: *"massive major mismatches"* |

**Every cause is Dhan-side.** The Groww comparison on the same host rules out our NIC,
network and pipeline. Reviving the lane changes none of it.

**Therefore this plan ships DETECTION, not a claim of repair.** The 15:31 cross-verify
must be live from day one — note it had been **BLIND SINCE BIRTH** until PR #1474
(2026-07-11) fixed a nanosecond-vs-microsecond literal that put its WHERE window in
~year 58502, matching zero rows on every run. Its divergence counts are the only honest
measure of whether this feed is usable, and they are the deliverable of PR-F.

---

## Design

Rebuild the Dhan tick lane around the **parser that survived**, then re-wire it into the
resilience chain that also survived. Nothing about the hot path is invented.

### What SURVIVES — verified by file scan, no rebuild needed

| Component | Path |
|---|---|
| **Binary parser — the O(1) core** | `crates/core/src/parser/{header,ticker,quote,full_packet,oi,previous_close,disconnect,dispatcher,market_status,read_helpers}.rs` |
| TLS (aws-lc-rs) | `crates/core/src/websocket/tls.rs` |
| WS types, activity watchdog, market-hours gate | `crates/core/src/websocket/{types,activity_watchdog,market_hours_gate}.rs` |
| Token/auth stack (TOTP → JWT → SSM publish) | `crates/core/src/auth/*` — running today for the REST legs |
| Seal ring / spill / DLQ / seal writer | `crates/storage/src/seal_*.rs` |
| Capture-at-receipt WAL | `crates/storage/src/ws_frame_spill.rs` |
| `TfIndex` + `LiveCandleState` | `crates/trading/src/candles/*` |

**The O(1) constraint is met by existing code.** The parser is fixed-offset
`from_le_bytes` with no loop and no allocation, already covered by DHAT zero-alloc tests
that gate every PR. This plan adds **no new hot-path parsing** — it re-attaches a
socket to a parser that is already proven.

### What must be REBUILT — all deleted 2026-07-13/17, verified absent

| Component | Deleted file |
|---|---|
| Main-feed connection + reconnect | `crates/core/src/websocket/connection.rs`, `connection_pool.rs` |
| Subscription builder | `crates/core/src/instrument/subscription_planner.rs` |
| Tick→timeframe aggregator | `crates/trading/src/candles/multi_tf_aggregator.rs` |
| Tick writer + dedup key | `crates/storage/src/tick_persistence.rs` |
| Tick-gap detector | (deleted with the lane) |

Each is recoverable from git history pre-`2026-07-17`, which makes this a **restore +
re-harden**, not a green-field build. The plan treats recovered code as a starting draft
that must re-pass today's guards, never as trusted.

### Deliberately NOT rebuilt

- **The instrument CSV download/parse chain.** Q3 of the retirement stands: hardcoded
  SIDs only. The subscription set is `SPOT_1M_REST_INDICES` (NIFTY 13, BANKNIFTY 25,
  SENSEX 51, INDIA VIX 21) — 4 SIDs, Quote mode, one connection.
- **Depth-20 / depth-200 / any second Dhan endpoint** — still REJECT.
- **`SubscriptionScope` as an enum.** 4 hardcoded SIDs need no scope machinery; adding
  the enum back re-creates the exact expansion surface the lock closed.
- **Live order fire.** `dry_run` stays true; the §28 indicator/strategy boundary stays frozen.

### Scale reality (so the sizing claim stays honest)

4 SIDs × 13 timeframes × 128 B = **6.7 KB** of live candle state. Ticks at ~2–4/sec/SID
≈ 360 K ticks/day ≈ **32 MB/day**. This is **nothing** against 32 GiB.

**Stated plainly: 4 SIDs does NOT justify r8g.xlarge either.** The instance is right-sized
for ~25,000 instruments, and Dhan-with-hardcoded-SIDs delivers 4 spots plus the ~920 REST
option-chain contracts. Reviving the WS makes the *timeframes* achievable; it does **not**
make 32 GiB necessary. Widening the subscription beyond 4 SIDs needs its own dated quote
(the CSV chain is banned, so any widening is a hardcoded-list edit + a scope note).

## Plan items

- [ ] **PR-A** — This plan (DRAFT) + the scope-lock revival section (**DONE**, committed
      with this plan). Docs only.
  - Files: `websocket-connection-scope-lock.md`, this plan
  - Tests: docs-only (design-first wall PASS on non-impl)

- [ ] **PR-B** — Re-enable the feed flag path: `dhan_enabled` config plumbing revived as a
      real gate (currently `false` + a 409 refusal on runtime enable). Keep the serde
      default **OFF**; flip `base.toml` only. Revive the runtime-enable ON-half that the
      2026-07-13 Phase A revoked.
  - Files: `crates/common/src/config.rs`, `config/base.toml`, `crates/api/src/*` (the 409)
  - Tests: serde-default-off; the 409 is gone; workspace escalation (common change)

- [ ] **PR-C** — Main-feed connection: `wss://api-feed.dhan.co?version=2&token=…&clientId=…&authType=2`,
      bounded expo reconnect, `SubscribeRxGuard` (subscription state survives reconnect),
      activity watchdog re-attached, `ws_event_audit` rows on every lifecycle edge.
      Subscription = the 4 hardcoded SIDs in **Quote mode (code 17)**, one JSON batch.
      Read task does **nothing but drain → WAL → ring** (the receive-buffer rule).
  - Files: `crates/core/src/websocket/{connection,subscribe}.rs`, `crates/app/src/dhan_lane.rs`
  - Tests: reconnect ladder incl. instant-first-attempt; SubscribeRxGuard survives a drop;
    disconnect-code classification (12 codes, 807→token refresh); no-panic on unknown

- [ ] **PR-D** — Wire into WAL→ring→spill→DLQ (per-feed path `data/spill/dhan/`) and
      **REBUILD `MultiTfAggregator`** (`FeedStrategy::Dhan`, late-tick policy Refold)
      deriving all 13 timeframes from ticks via the surviving `TfIndex`. Re-add the
      tick-gap detector (per-SID silence → WS-GAP-06) — the 2026-07-06 evidence says
      29–67 SIDs/minute went silent, so this detector is **load-bearing, not optional**.
  - Files: `crates/trading/src/candles/multi_tf_aggregator.rs` (restore + re-harden),
    `crates/storage/src/ws_frame_spill.rs`, `crates/app/src/dhan_lane.rs`
  - Tests: WAL replay DEDUP-idempotent; aggregator derives every TfIndex frame from a
    synthetic tick stream; DHAT zero-alloc on the fold; tick-gap fires + recovers

- [ ] **PR-E** — **REBUILD `tick_persistence.rs`** with `DEDUP_KEY_TICKS` as a **const**
      (never an inline write-site literal — an inline key evades the feed-in-key allowlist
      guard). Async cold-path QuestDB writer, `feed='dhan'`, 5-key dedup
      `(ts, security_id, segment, capture_seq, feed)` — **already the live schema**, verified
      by `questdb_init_script_guard`, so **no DDL migration**. RAM-first: no DB read in any
      decision path.
  - Files: `crates/storage/src/tick_persistence.rs`, banned-pattern guard extension
  - Tests: `DEDUP_KEY_TICKS` includes `feed` (meta-guard); N ticks in ONE second all
    survive; DB-down does not block the lane

- [ ] **PR-F** — **The mismatch detector, and the reason this plan is honest.** Re-arm the
      15:31 IST cross-verify (live WS candles vs Dhan historical) with its post-#1474
      microsecond literals, plus the lag histogram (`exchange LTT → receive`) and the
      silent-SID gauge. Daily Telegram line reporting divergence count + lag p99.
  - Files: `crates/app/src/cross_verify_1m_boot.rs`, `crates/app/src/observability.rs`,
    `docs/error-runbooks/dhan-live-ws-error-codes.md`
  - Tests: cross-verify window uses MICROSECOND literals (digit-magnitude ratchet — the
    #1474 regression pin); lag histogram registration; alarm wiring

- [ ] **PR-G** — Adversarial 3-agent review (hot-path, security, hostile) on the full diff.

## Edge Cases

| # | Case | Handling |
|---|---|---|
| E1 | Dhan lag spikes to 199 s again | Lag histogram + p99 alarm make it VISIBLE. Not prevented — cannot be, it is upstream. |
| E2 | 29–67 SIDs silent per minute | Tick-gap detector per SID (PR-D). With only 4 SIDs, any silence is immediately obvious. |
| E3 | Live candles disagree with Dhan historical | PR-F cross-verify counts it daily. **This is the deliverable, not a failure.** |
| E4 | Token invalidated mid-session (the 2026-07-06 DH-906 class) | Disconnect code 807 → token refresh; the token minter Lambda re-mints daily off-box |
| E5 | Bare TCP RST storms (2026-07-02 / 07-08 class) | Bounded expo reconnect + `ws_event_audit` per cycle; a storm is counted, never out-reconnected |
| E6 | Recovered git code fails today's guards | Treated as a draft, must re-pass banned-pattern + DHAT + pub-fn guards. Not trusted because it once shipped. |
| E7 | An inline dedup-key literal sneaks in | `dedup_segment_meta_guard` + the feed-in-key allowlist fail the build |
| E8 | Someone widens beyond the 4 hardcoded SIDs | Needs its own dated quote; the CSV chain stays banned, so widening is a visible list edit |
| E9 | Reconnect loses the subscription | `SubscribeRxGuard` re-subscribes; PR-C test pins it |
| E10 | Ticks arrive before the market-hours gate opens | Existing pre-open buffer behaviour; capture-at-receipt WALs regardless |

## Failure Modes

| # | Failure | Detection | Response |
|---|---|---|---|
| F1 | Feed delivers nothing | Tick-gap detector + the market-hours liveness alarm | Page; REST legs unaffected (independent) |
| F2 | Aggregator drops seals | `tv-<env>-seal-writer-dropped` + AGGREGATOR-DROP-01 | Existing pager |
| F3 | QuestDB down | ring → spill → DLQ absorbs; DB write is async cold path | No tick loss inside the envelope |
| F4 | Cross-verify blind again | PR-F's digit-magnitude ratchet (the #1474 pin) | Fails the build |
| F5 | Hot-path allocation introduced | DHAT tests gate every PR | Fails the build |
| F6 | Lane death | Supervised spawn (house respawn pattern) + `ws_event_audit` | Auto-respawn + counter |

## Test Plan

| Layer | What |
|---|---|
| Unit | Parser (already green), reconnect ladder, disconnect codes, subscription batching |
| Property | proptest over tick streams into the aggregator |
| Zero-alloc | DHAT on parse + fold — **the O(1) proof** |
| Integration | WAL replay idempotence; DB-down absorption; SubscribeRxGuard across a drop |
| Ratchet | `dedup_segment_meta_guard`, `questdb_init_script_guard`, cross-verify digit-magnitude, feed-in-key |
| Bench | Criterion on `dispatch_frame` + `pipeline` against the existing 10 ns / 100 ns budgets |
| Bite-proof | Every new guard broken deliberately, observed FAILING, restored |
| Adversarial | 3-agent review before and after |

## Rollback

Per-PR revert. The feed is **flag-gated** (`dhan_enabled`, serde default OFF), so the
fastest rollback is a config flip + restart — no code revert needed, exactly how the
2026-07-13 Phase A retirement worked. Data already written stays (`feed='dhan'` rows are
SEBI-retained, never deleted). No DDL migration exists to undo: the `ticks` 5-key dedup
schema is already live.

## Observability

| Signal | Where |
|---|---|
| **Delivery lag p99** (the 46 s class) | new histogram + CloudWatch alarm |
| **Silent SIDs** | tick-gap detector gauge, per SID |
| **Live-vs-historical divergence** | 15:31 cross-verify audit table + daily Telegram line |
| WS lifecycle | `ws_event_audit` rows on every connect/disconnect/subscribe |
| Seal drops | existing seal-writer pager |
| Tick throughput | `tv_tick_*` counters |
| RAM | `tv_process_rss_bytes` — expect a few MB at 4 SIDs |

## Cost

**₹0 incremental.** The instance, disk and ceiling are already approved under Quote 13 and
unchanged by this plan. A handful of new alarms/metrics ≈ **+₹40–70/month**.

**Honest note on the sizing argument:** this plan makes the 13 timeframes *achievable*,
which was the stated justification for r8g.xlarge. It does **not** make 32 GiB
*necessary* — 4 SIDs need ~7 KB of candle state. If the subscription stays at 4 SIDs,
Option A (t4g.large, ~₹1,900/mo) remains the right-sized choice and this revival works
identically on it. **The instance decision and this revival are independent.**

## Blocking note — the active-plan cap

`plan-gate.sh` V7 blocks every implementation push above 5 `active-plan*.md` files and
there are exactly **5** today, so this sits in `plans/proposals/` and cannot be
implemented until promoted. Promotion requires archiving one — the honest candidate is
`active-plan-dhan-token-minter-lambda.md` (work complete, shipped in PR #1730). **Not**
`active-plan-dhan-order-surface.md`: its 3 unchecked items are real pending work.

## Honest envelope

> 100% inside the tested envelope, with ratcheted regression coverage: the O(1) hot path
> needs no rebuild — the fixed-offset parser survived intact with its DHAT zero-alloc
> gates; every recovered module must re-pass today's guards; the `ticks` 5-key dedup
> schema is already live so no migration exists; the feed is flag-gated so rollback is a
> config flip.
> **NOT claimed:** (a) that Dhan's data quality is fixed — **it is not, and cannot be from
> our side**; the 46 s p99, the 29–67 silent SIDs and the live-vs-historical mismatch are
> Dhan-side, and this plan ships DETECTION of them, never repair; (b) that 4 SIDs justify
> r8g.xlarge — they do not, and the plan says so; (c) that the recovered aggregator and
> tick writer will restore cleanly — they are 3-week-old deletions and must be re-hardened,
> not trusted; (d) any live behaviour — the first enabled session is the probe, and the
> cross-verify divergence count on day one is the real verdict on whether this feed is
> usable at all.

## Auto-driver explanation

> Sir, you asked to bring back supplier Dhan's live price board — the one we took down in
> July because its shouted prices didn't match its own printed record book, and because
> sometimes a price took 46 seconds to arrive when the other supplier managed half a
> second on the same wall.
>
> Good news: the **best part was never thrown away.** The machine that reads Dhan's
> shorthand instantly — the fast one — is still bolted to the bench, tested and sealed. We
> only need to re-run the phone line to it, and rebuild three things we scrapped: the
> price-folding machine, the filing clerk, and the silence-detector.
>
> Two honest words. One: putting the board back **does not make the prices better** — the
> lateness and the mismatch are on Dhan's side of the wall. So we are also installing the
> INSPECTOR: every evening he compares Dhan's shouted prices against Dhan's own record
> book and writes the disagreement count on the board. That number tells you whether this
> supplier is worth keeping. Two: four fruits do not need the giant table — the big table
> was for twenty-five thousand. The live board and the table size are separate decisions.
