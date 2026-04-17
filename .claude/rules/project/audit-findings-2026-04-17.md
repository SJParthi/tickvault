---
paths:
  - "crates/core/src/pipeline/tick_processor.rs"
  - "crates/core/src/websocket/**/*.rs"
  - "crates/app/src/main.rs"
  - "crates/app/src/trading_pipeline.rs"
  - "crates/core/src/instrument/depth_rebalancer.rs"
  - "crates/common/src/instrument_registry.rs"
  - "crates/storage/tests/*.rs"
  - "deploy/docker/grafana/dashboards/*.json"
---

# Audit Findings 2026-04-17 — Session Memory

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** Every pattern that caused a real bug on 2026-04-17 is
> documented here so future Claude sessions avoid repeating the
> same "missing scenarios" diagnosis conversation.
>
> **Parthiban's directive:** "always keep updating the doc dude okay
> so that in the future again and again i cant come up with same
> issues scenarios bugs".

## Timeline of 2026-04-17 session

1. **Morning** — QuestDB showed NIFTY IDX_I ticks never arrived.
   Root cause: Dhan reuses `security_id` across segments and our
   `InstrumentRegistry` dropped one on HashMap insert. (I-P1-11)
2. **13:00** — FINNIFTY depth-20 missing from Grafana dashboard.
   Same root cause — Option C v1 dropped mandatory underlyings.
   (Option C v2)
3. **15:45** — Depth stale-spot alert STORM on Telegram.
   Root cause: rebalancer fires post-market when main feed stops
   streaming; no market-hours gate, no edge-trigger suppression.
4. **16:53** — Post-market redeploy hit BOOT DEADLINE MISSED
   (342s > 120s). Option C v2's 5-minute hard cap waits for
   LTPs that cannot arrive outside market hours. (Option C v3)
5. **16:57** — Dhan email: 200-level depth works on their end
   (ticket #5543510). Retest next market open.
6. **17:00** — Full deep-audit: 20 findings, 9 real, 11 false
   alarms. Fixed the reals, documented the dismissals here.

## Rule 1 — `security_id` alone is NOT unique. See `security-id-uniqueness.md`.

## Rule 2 — Live market feed must contain live data only. See `live-feed-purity.md`.

## Rule 3 — All background workers must be market-hours aware

Every tokio task that polls or schedules work based on live data MUST
check market hours before doing work + alerting. Otherwise it either
wastes resources post-market or fires false alerts.

**Canonical helper:**
```rust
use tickvault_common::constants::{
    IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};
let now_utc = chrono::Utc::now().timestamp();
let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
let sec_of_day = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
let is_market_hours = (TICK_PERSIST_START_SECS_OF_DAY_IST
    ..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day);
```

**Live sites already using this:**
- `crates/core/src/instrument/depth_rebalancer.rs::is_within_market_hours_ist`
- `crates/app/src/main.rs` (Option C v3 depth boot wait)

**When writing a new background task:**
1. Decide if it should run post-market (dashboards / persistence = yes;
   alerts / scheduled subscribes / depth rebalance = no).
2. If it should NOT run post-market, add the gate.
3. Downgrade off-hours alerts to INFO/WARN, not ERROR (ERROR → Telegram).

## Rule 4 — Edge-triggered alerts, not level-triggered

Any alerting loop that polls a condition must track `currently_alerted`
and fire Telegram ONLY on the rising edge. Polling every N seconds and
firing every time the condition holds = Telegram spam.

**Reference implementation:**
`depth_rebalancer.rs` — `currently_stale: HashSet<String>`, rising-edge
fires ERROR + Telegram, falling-edge fires INFO only (operator already
knew from the rising-edge alert).

## Rule 5 — All flush/broadcast errors are ERROR, not WARN

Telegram alerts fire only on `error!` macro (via Loki ERROR-level
routing). If a flush or broadcast failure is logged at WARN level,
the operator never sees it on Telegram.

**Fixed sites (audit finding #2, #4, #1, #19):**
- `tick_processor.rs::periodic tick flush failed` — ERROR
- `tick_processor.rs::periodic depth flush failed` — ERROR
- `tick_processor.rs::periodic live candle flush failed` — ERROR
- `tick_processor.rs::tick broadcast send failed` — ERROR (from silent)
- `trading_pipeline.rs::trading pipeline lagged` — ERROR if > 1000

**When writing NEW flush/broadcast handlers:**
- Never use `let _ = sender.send(tick)` — always match on `Err` and
  log ERROR + increment a `*_errors_total` counter.
- Always log ERROR (not WARN) on flush/persist failures.
- Structured context in the log: `error!(?err, field = value, ...)`.

## Rule 6 — Wall-clock + exchange-timestamp guards on EVERY persist site

Ticks + depth + candles MUST check BOTH:
1. `is_within_persist_window(exchange_timestamp_secs)` — in-session data
2. `is_wall_clock_within_persist_window(received_at_nanos)` — received
   during session, not a post-market replay/buffer

**Fixed site (audit finding #6):** depth persist in `tick_processor.rs`
had only the exchange-timestamp check. A Full packet (code 8) could
carry an exchange_timestamp from inside market hours but actually
arrive post-close, polluting `market_depth` table. Now both guards.

## Rule 7 — Composite DEDUP keys, always

Every QuestDB table that holds `security_id` MUST include segment
(or equivalent segment column) in its `DEDUP UPSERT KEY`. Enforced
mechanically by `crates/storage/tests/dedup_segment_meta_guard.rs`.

## Rule 8 — Banned-pattern hook is the first line of defence

`.claude/hooks/banned-pattern-scanner.sh` — 6 categories now live:

| # | Category | Scope |
|---|---|---|
| 1 | Universal bans (unwrap/expect/println/DashMap/bincode) | All prod |
| 2 | Hot-path bans (.clone, Vec::new, format!, .collect, dyn) | core/trading/ws/oms |
| 3 | Hardcoded values (Duration::from_secs(N), ports, URLs) | All prod |
| 4 | Dhan locked facts (#5519522 200-depth path) | All prod |
| 5 | I-P1-11 single-segment collections | instrument/pipeline/ws/greeks paths |
| 6 | LIVE-FEED-PURITY (backfill/synth) | historical/ + synth paths |

**When adding a NEW banned pattern:**
1. Add the scan to `banned-pattern-scanner.sh` under the right category.
2. Run a negative self-test to confirm it blocks.
3. Document the rule in `.claude/rules/project/`.
4. Add a regression test somewhere that fails if the pattern returns.

## Rule 9 — Dashboard panel ratchet

`crates/storage/tests/grafana_dashboard_snapshot_filter_guard.rs`
enforces:
1. Every `count_distinct(security_id)` must also include segment.
2. Every required metric prefix must have at least one panel.
3. `KNOWN_DASHBOARD_GAPS` is a shrinking-only list.

**When adding a NEW critical metric:** add the prefix to
`REQUIRED_DASHBOARD_METRIC_PREFIXES`. If the panel isn't built yet,
also add to `KNOWN_DASHBOARD_GAPS` so the test is green; remove
when the panel ships.

## Rule 10 — Subagent audits need verification

The 2026-04-17 deep-audit spawned by the Explore subagent produced
20 findings. Verification showed:

| Finding | Verdict |
|---|---|
| #1 broadcast send errors | REAL, fixed (commit 9a13f4c) |
| #2 flush errors WARN | REAL, fixed (commit 9a13f4c) |
| #3 shutdown not flushing | FALSE ALARM — `flush_on_shutdown()` exists |
| #4 candle flush WARN | REAL, fixed (commit 9a13f4c) |
| #5 dashboard depth panels | REAL, ratchet added (commit 5c7e8b9) |
| #6 depth wall-clock guard | REAL, fixed (commit 9a13f4c) |
| #7 order update heartbeat | FALSE ALARM — ActivityWatchdog exists |
| #8 trading pipeline lag | REAL, fixed (commit ea97de9) |
| #9 dispatch_subscribe silent | REAL, fixed (commit 12827cb) |
| #10 Instant vs SystemTime | FALSE ALARM — Instant correct per process |
| #11 clock skew on wall-clock | DEFERRED — theoretical, low probability |
| #12 shutdown notifier coverage | DEFERRED — needs observation |
| #13 market hours constants | FALSE ALARM — compile assert + tests exist |
| #14 token backoff cap | FALSE ALARM — attempt capped at 3 |
| #15 rebalancer tests | FALSE ALARM — 4 tests exist |
| #16 install_subscribe twice | FALSE ALARM — only called once at boot |
| #17 rescue_in_flight full ring | FALSE ALARM — cascades to spill/DLQ |
| #18 movers snapshot refresh | BY DESIGN — registry immutable |
| #19 broadcast send errors | DUPLICATE of #1 |
| #20 chaos test matrix | DEFERRED — nice-to-have |

**When running a new audit:** always verify each finding against
the real code before acting. The subagent was roughly 45% real / 55%
false alarm on this pass.

## Live bugs caught by Parthiban (not by audit) this session

- **09:xx** — QuestDB query showed NIFTY IDX_I ticks = 0. Root: I-P1-11.
- **15:45** — Telegram storm of stale-spot alerts. Root: no market-hours
  gate + no edge trigger.
- **16:53** — Post-market redeploy hit BOOT DEADLINE MISSED. Root:
  Option C v2 hard-cap waits for LTPs that can't arrive off-hours.

**Pattern:** every one of these was "background worker doing work
post-market or without market-hours awareness". Rule 3 above exists
specifically because this kept recurring.

## Checkpoints for every new feature / edit

Before merging any edit touching the paths in this file's frontmatter:

- [ ] Does it touch data keyed by `security_id`? → Rule 1
- [ ] Does it write to the `ticks` table? → Rule 2
- [ ] Does it run as a tokio task? → Rule 3 (market-hours) + Rule 4 (edge trigger)
- [ ] Does it log a failure? → Rule 5 (ERROR not WARN)
- [ ] Does it persist a tick/depth/candle? → Rule 6 (both guards)
- [ ] Does it declare a QuestDB DEDUP key? → Rule 7
- [ ] Does it declare a new collection type? → Rule 8 (banned patterns)
- [ ] Does it emit a new metric? → Rule 9 (dashboard ratchet)
- [ ] Is it based on an audit finding? → Rule 10 (verify first)

If any answer is yes and the rule's mechanical guard is missing,
STOP and add the guard before merging.
