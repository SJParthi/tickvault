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

## 2026-04-24 Addendum — deep audit + 2 false-positive Telegram classes killed

Triggered by a live 15:47 IST fresh-clone incident where operator booted at
14:54 IST and got a green "CROSS-MATCH OK / All OHLCV values match bit-for-bit"
Telegram even though only 36 minutes of live data (out of 375 min session) had
been captured. Cascaded into a full deep-audit via the `general-purpose`
subagent (see "Rule 10" below for the audit verification discipline).

### Variant H pin (PR #340 merged)

2+ weeks of `Protocol(ResetWithoutClosingHandshake)` on `full-depth-api.dhan.co`
were caused by wire-level mismatch vs Dhan's Python SDK. Fix (merged):

- URL path: `/` (root) — locked in by 2026-04-23 PR
- User-Agent: `Python/3.12 websockets/16.0` (NEW — from Python SDK)
- TLS ALPN: NONE (NEW — new `build_websocket_tls_connector_no_alpn()`)
- 10 regression ratchets in `tls.rs` + `depth_connection.rs`

**Lesson (reinforces 2026-04-23):** When Dhan support, Dhan docs, and Dhan SDK
disagree, the SDK wins because it's what their own infra accepts. Variant
matrix tests (`depth_200_variants.rs` example) are the empirical verification.

### Cross-match coverage guard (PR #341 merged)

`determine_cross_match_passed` previously returned true when
`total_mismatches=0 && total_missing_live=0 && total_compared>0` even though
LEFT-JOIN-based counts didn't detect full-grid coverage gaps. Added a 6th
condition: `coverage_pct >= CROSS_MATCH_MIN_COVERAGE_PCT (= 90)`, computed as
`(total_live_candles * 100) / total_compared`. Routing: `coverage_pct < 90` →
`CandleCrossMatchSkipped` with explicit partial-coverage reason.

9 new regression tests. Mid-session boot with 10% coverage now correctly
routes to Skipped instead of a false OK.

### 8 audit findings (PR #342 through #349)

| # | Sev | Issue | Fix | PR |
|---|---|---|---|---|
| 1 | HIGH | `HistoricalFetchComplete` fired OK when `instruments_fetched=0 && total_candles=0` (Dhan outage, empty universe, mid-boot race) | `zero_fetched_degenerate` gate → `HistoricalFetchFailed` with synthesized reason `"zero_fetched_zero_candles"` | #342 |
| 2 | HIGH | `TickGapTracker::detect_stale_instruments()` defined but never called — per-instrument stalls invisible until 120s global watchdog | 30s periodic call in `run_slow_boot_observability` via new `STALE_LTP_SCAN_INTERVAL_SECS = 30` constant | #349 |
| 3 | HIGH (**deferred**) | Audit claimed TickGapTracker bypassed when QuestDB flaps — **actually a false alarm**: two trackers already exist (`run_tick_persistence_consumer` + `run_slow_boot_observability`), and `record_tick` fires BEFORE `append_tick` | No code change; documented here so future audits don't re-flag this | — |
| 4 | MED | `order_update_connection.rs:357` used `let _ = order_sender.send()` — silently drops order-fill updates if OMS subscriber crashes | Replaced with error-logged branch + `tv_order_update_broadcast_drops_total` counter; zero-alloc hot path (reads `err.0.order_no`, no clone) | #345 |
| 5 | MED | Grafana operator-health dashboard showed `tv_ticks_dropped_total` / `tv_ticks_spilled_total` as RAW counters — after restart reads 0, hiding ongoing drops | Wrapped both in `increase(...[5m])` | #346 |
| 6 | MED | `InstrumentBuildSuccess` event defined + tested but NEVER emitted — operator had no positive signal after daily 08:15 IST rebuild | Emitted from both boot paths with `source` tag `"fresh_csv_build"` or `"rkyv_cache"` | #343 |
| 7 | MED | `summary_writer.rs::atomic_write` did `f.sync_all().ok()` — silently discarded flush errors, hard-poweroff could promote unflushed file | Changed to `with_context(...)?` — matches adjacent error-propagation pattern | #344 |
| 8 | LOW | `MarketOpenStreamingConfirmation` (Severity::Info) fired even on `main_feed_active == 0` — catastrophic "no connections at market open" read as healthy heartbeat | New `MarketOpenStreamingFailed` variant at Severity::High, routed when `main_active == 0` exactly | #347 |

### Infrastructure shipped in parallel

- **SessionStart auto-health hook** (PR #348): new `session-auto-health.sh`
  runs `doctor` + `validate-automation` + `mcp-doctor` in a detached
  background subshell on every session start. Writes verdict to
  `data/logs/session-auto-health.latest.txt` so the NEXT session sees the
  previous cycle's outcome immediately.

### New Rule 11 — False-OK classification is a CLASS bug, not an incident

Every "X OK" Telegram that is computed from a truthy-but-meaningless flag is
a bug of the same class as the 2026-04-24 15:47 IST CROSS-MATCH OK incident.
Before emitting ANY success-class notification, check:

1. **Was the success signal computed from a non-zero baseline?** (e.g., for
   cross-match: `coverage_pct >= 90 AND total_compared > 0`; for
   historical-fetch: `instruments_fetched > 0 OR total_candles > 0`).
2. **Could the denominator or divisor be zero?** Integer division by zero
   returns something "safe"-looking but semantically meaningless.
3. **Is the success condition identical to "nothing happened yet"?**
   Zero mismatches on an empty set passes a naive check.

If any answer is "maybe," route to a Skipped variant with a typed reason,
NOT to the success variant. The Skipped variant is free — it exists already
for the first-run / fresh-DB case — just reuse it with a reason string.

The PR #341 + #342 + #347 pattern is the reference: gate on a positive
progress signal (`coverage_pct`, `instruments_fetched > 0`,
`main_feed_active > 0`), and route the degenerate case to a Failed/Skipped
variant with an explicit diagnostic reason in Telegram.

### New Rule 12 — Dashboard "lying" check

Any Prometheus counter shown as a raw value (not `rate()` / `increase()`)
will lie to the operator after the process restarts, because the counter
resets to 0 and the panel reads "0" while drops are actively happening.
Every counter panel in `deploy/docker/grafana/dashboards/*.json` must use
`increase()` or `rate()`.

Enforcement: follow-up to the existing
`grafana_dashboard_snapshot_filter_guard.rs` — add a rule that scans for
bare counter expressions and requires them to be wrapped.

### New Rule 13 — If a method exists + is tested but is never called, it IS a bug

`TickGapTracker::detect_stale_instruments()` (Finding #2) had 11 unit test
call sites and zero production call sites for months. The audit caught it,
but only by spawning a subagent specifically to find "method defined but
never called in production" patterns.

Mechanical guard proposal (future PR): add a test that walks `pub fn` in
`crates/trading/src/` and `crates/core/src/` and asserts each has at least
one call site outside its own module (excluding `#[cfg(test)]`). `cargo
mutants` partially covers this, but a direct source-scan is cheaper.

---

## 2026-04-23 Addendum — 200-depth disconnect root cause FIXED

For 2+ weeks our Rust client had been TCP-reset by Dhan on every
200-depth subscribe (`Protocol(ResetWithoutClosingHandshake)`).
On 2026-04-23 Parthiban ran Dhan's official Python SDK
`dhanhq==2.2.0rc1` on the same account + same token + same
SecurityId 72271 — the SDK streamed 200-depth successfully for
30+ minutes using URL `wss://full-depth-api.dhan.co/?token=...`
(ROOT path `/`, NOT `/twohundreddepth` as ticket #5519522 had
told us).

Fix (this branch `claude/hardcode-dhan-api-7rgQC`):
- `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL` → `wss://full-depth-api.dhan.co`
- URL builder in `depth_connection.rs` now emits `/?token=...`
- Tests in `depth_connection.rs`, `dhan_api_coverage.rs`,
  `dhan_locked_facts.rs` all updated to assert the root path.
- Banned-pattern hook category 4 inverted: now rejects
  `wss://full-depth-api.dhan.co/twohundreddepth` literal.
- Rule 2 of `full-market-depth.md` rewritten with the 2026-04-23
  Python SDK verification as the new source of truth.
- `websocket-enforcement.md` rule 14 marks the Rust-side bug FIXED.

**Lesson:** a Dhan support ticket response is NOT infallible ground
truth. When the Python SDK (also from Dhan) contradicts the ticket's
advice, the SDK wins — it's what Dhan's own engineers ship and use.
Live-test both whenever a ticket prescribes a non-obvious config.

Diagnostic tooling kept in the branch for future regressions:
- `crates/core/examples/depth_200_variants.rs` — 8-variant matrix
- `scripts/dhan-200-depth-repro/` — Python SDK baseline + analyze script
- `docs/dhan-support/2026-04-24-depth-200-variant-test-results.md`
  — template for the next escalation email if this regresses.

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
