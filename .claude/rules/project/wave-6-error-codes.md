# Wave 6 Error Codes

> **Authority:** This file is the runbook target for the Wave 6 ErrorCode
> variants added across Sub-PRs #1–#4 of
> `.claude/plans/active-plan-aggregator-direct-flush-rehydrate.md`.
> **Cross-ref test** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires every variant in `crates/common/src/error_code.rs::ErrorCode` to
> be mentioned in at least one rule file under `.claude/rules/`. This file
> serves that contract for the codes Sub-PR #1 introduces. Sub-PR #2/#3/#4
> will append further sections as they land (REHYDRATE-*, BUFFER-GATE-*,
> POSTMARKET-*, VERIFY-*).

## AGGREGATOR-DROP-01 — sealed candle dropped after ring+spill+DLQ all failed

**Trigger:** the multi-TF aggregator sealed a candle, attempted to flush
it via the ring buffer (`SEAL_BUFFER_CAPACITY`), the disk spill
(`data/spill/seals-YYYYMMDD.bin`) and the NDJSON DLQ
(`data/dlq/seals-*.ndjson`); ALL three absorbing tiers refused the row.
This is the only code path that constitutes silent data loss for a
sealed candle. Severity::Critical.

**Why this is Critical and not Medium:** the mirror of `tick_persistence`
ring→spill→DLQ explicitly absorbs the IST-midnight burst (~99K seals
across 11K instruments × 9 TFs). If all three tiers fail, the host is
out of memory AND out of disk AND the `data/dlq/` directory is
unwritable — by definition catastrophic.

**Triage:**
1. `mcp__tickvault-logs__docker_status` — is the host OOM-killed?
2. `df -h /data` — is the volume full?
3. `ls -la data/spill/ data/dlq/` — are the directories writable?
4. If host is healthy and dirs are writable, restart the app — the
   bounded ring resets and resumes from healthy state.

**Source:** `crates/storage/src/shadow_persistence.rs::ShadowCandleWriter::handle_drop`.

### 2026-05-11 Update — 4-alert drop-class family is now live

Per Wave 6 Sub-PR #1 items 1.4j/l/n/o (merged #584/#587/#589/#590) the
operator has Telegram coverage for EVERY drop class shown on the 1.4k
dashboard panel:

| Prometheus alert uid | Counter | Threshold | for | PR |
|---|---|---|---|---|
| `tv-aggregator-no-seals-during-market` | `tv_aggregator_seals_emitted_total == 0` | seals == 0 / 5m | 5m | #584 |
| `tv-aggregator-mpsc-drop-storm` | `tv_seal_mpsc_dropped_total` | > 100 / 1m | 1m | #587 |
| `tv-aggregator-broadcast-lag-storm` | `tv_aggregator_tick_lag_total` | > 100 / 1m | 1m | #589 |
| `tv-aggregator-late-tick-sustained` | `tv_aggregator_late_ticks_discarded_total` | > 300 / 5m sustained | 5m | #590 |

All 4 alerts:
- Gated by `tv_market_hours_active == 1` (audit-findings Rule 3)
- Severity::High (pages Telegram)
- Wrapped in `increase()` per Rule 12

Family completeness is meta-ratcheted by 3 tests in
`crates/storage/tests/wave_6_aggregator_alert_guard.rs` (item 1.4p,
merged #591). Future deletion of any single alert OR severity
downgrade OR market-hours-gate removal fails the build.

## AGGREGATOR-LATE-01 — tick arrived after its bucket sealed (discarded)

**REVISED 2026-06-05 (operator lock — Option B "late tick re-folds its own
minute"):** a late tick whose `exchange_timestamp` floors to the
MOST-RECENTLY sealed bucket is NO LONGER discarded — it re-folds its OWN
minute's high/low/close (`ConsumeOutcome::AmendedLate`) and the candle is
re-emitted so the writer UPSERTs it in place (DEDUP `(ts, security_id,
segment)`). The tick's timestamp decides its minute, so this is NOT a
cross-bucket merge — it corrects the candle the tick always belonged to.
Observability: `tv_aggregator_amended_ticks_total` + the heartbeat `amended`
field. `AGGREGATOR-LATE-01` (`ErrorCode::AggregatorLate01...`) now fires ONLY
for a tick that is **≥ 2 buckets late** OR has **no amendable sealed bucket**
(e.g. just after the IST-midnight / 15:30 `force_seal` cleared it — the
cross-day-amend guard §4b) — only THEN is it `error!` + counter + discard.

**Original trigger (pre-2026-06-05, retained for history):** at a minute
boundary the aggregator sealed a 1m bucket; a tick whose `exchange_timestamp`
falls inside that bucket arrives ≥1 ms after the seal completes. The
aggregator MUST NOT silently merge the late tick into the next bucket (would
shift data across timestamps); MUST NOT silently discard it; the only correct
action is `error!` log + counter increment + discard. Severity::High.

**Triage:**
1. Counter `tv_aggregator_late_tick_total{action="discard"}` rate — a
   sustained > 1/sec rate indicates clock drift between Dhan and our
   host (BOOT-03 territory) OR a slow consumer keeping ticks in the
   pipeline channel longer than the boundary period.
2. Cross-check `tv_websocket_connections_active` and
   `tv_aggregator_seal_in_progress_duration_ns` — if seal duration
   spikes near the boundary, the lock-free seal is contending; raise
   the per-cell shard count or shorten the seal critical section.

**Source:** `crates/trading/src/aggregator/multi_tf.rs::AggregatorEngine::on_tick`
+ the `seal_in_progress` epoch fence per cell.

**2026-07-03 Update — Groww late discards are now COUNTED:** the Groww bridge
consume site previously captured `ConsumeStats` but never read `late_count`
(Groww `LatePolicy::Discard` drops were completely silent — no counter, no
log). It now increments `tv_aggregator_late_ticks_discarded_total{feed="groww"}`
(`crates/app/src/groww_bridge.rs`, the consume-stats arm); the Dhan site keeps
its label-less series (`crates/app/src/main.rs`) so the existing
`tv-aggregator-late-tick-sustained` alert series is unchanged — the per-feed
labelling mirrors the `tv_seal_mpsc_dropped_total` precedent in
`seal_routing.rs`. Same date: the watermark catch-up seal's Failure-B guard in
`AggregatorCell::consume_tick` routes a post-catch-up-seal backlog tick through
these SAME late-arm semantics (amend for Dhan / counted discard for Groww)
instead of re-opening the sealed bucket.

## AGGREGATOR-SEAL-01 — seal-time ILP write to a shadow table failed (ring caught it)

**Trigger:** at seal time the aggregator attempted to write the sealed
candle to one of the 9 `candles_*_shadow` tables via QuestDB ILP; the
ILP buffer/flush returned an error; the row was caught by the ring
buffer (`SEAL_BUFFER_CAPACITY`). Severity::Medium — the data is NOT
lost, just buffered; Telegram alert ensures the operator knows.

**Note (PR1 H11 fix):** the legacy `LiveCandleWriter` at
`crates/storage/src/candle_persistence.rs:636` previously logged this
condition at `warn!`. The PR1 precursor commit upgrades that site to
`error!(code = ErrorCode::AggregatorSeal01IlpFailed.code_str(), ...)`
so it routes through Telegram per `error_level_meta_guard.rs` Rule 5.

**Triage:**
1. Counter `tv_shadow_writer_buffered_total{table}` rate — if it
   sustains, QuestDB ILP is degraded; check `BOOT-01`/`BOOT-02`.
2. Inspect `data/spill/seals-*.bin` size — growing means the ring is
   filling and disk-spill is engaging.

**Source:** `crates/storage/src/shadow_persistence.rs::ShadowCandleWriter`
+ existing fix at `candle_persistence.rs::flush_buffer` (legacy path).

## AGGREGATOR-HB-01 — per-minute aggregator seal-burst heartbeat (positive signal)

**Trigger:** every minute boundary, after the seal burst completes, the
aggregator emits a coalesced 60s heartbeat carrying
`(seals_emitted, seals_dropped, late_ticks_discarded)`. Severity::Info.

**Why it exists:** per `audit-findings-2026-04-17.md` Rule 11 the system
MUST have a positive false-OK avoidance signal. Without this heartbeat,
the operator only learns aggregator is dead when Sub-PR #3's post-market
cross-verify fails — a long latency for a hot-path subsystem.

**Triage:** none. Absence of this code for > 90 s during market hours
should be detected by SLO-02 (the composite score weakest dimension
becomes `aggregator_health`).

**Source:** `crates/trading/src/aggregator/heartbeat.rs::emit_seal_burst_heartbeat`.

## BOUNDARY-01 — missed-boundary catch-up seal fired

**Trigger:** the boundary timer detected `last_seen_minute < expected_minute - 1`
(one or more minute boundaries skipped, typically due to OS scheduler
preemption, clock slew, or a slow consumer). The catch-up seal walks
forward from the last-seen minute to the current expected minute,
sealing each missed bucket with the in-cell state. Severity::Medium.

**Why this is not Critical:** the in-memory cell still holds the
correct OHLCV state for the missed bucket; the seal is correct, just
late. The Critical condition would be `last_seen_minute < expected_minute - K`
for some K (current value: 5 minutes); above K the run was preempted
for so long that the boundary timer can no longer trust its own
state — that case escalates to `AGGREGATOR-DROP-01`.

**Triage:**
1. Counter `tv_boundary_catchup_total` — repeated firing within a
   single minute window indicates wall-clock instability (re-check
   BOOT-03).
2. Inspect the `data/logs/errors.jsonl.*` event payload's
   `missed_minutes` field; if > 1 minute, escalate.

**Source:** `crates/trading/src/aggregator/boundary_timer.rs::tick_boundary_loop`.

### 2026-07-03 Update — IMPLEMENTED (watermark-aware catch-up seal; first real emitter)

The pre-2026-07-03 text above described a `boundary_timer.rs::tick_boundary_loop`
design that was NEVER BUILT (`ErrorCode::Boundary01CatchupSeal` had zero emit
sites; the cited source file does not exist). BOUNDARY-01 is now LIVE with
different — safer — semantics:

**Trigger (actual):** each per-feed catch-up driver task polls every
`CATCHUP_SEAL_POLL_INTERVAL_SECS` (5 s) and gates each wave through the
shared pure `compute_catchup_cutoff` (2026-07-03 hardening): scan ONLY when
that feed's **event-time watermark** advanced since the last scan AND is not
poisoned (see the future-skew guard below), sealing every bucket whose end ≤
`min(watermark − margin, now_ist)` where `margin` is **per-feed** —
`CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN` (5 s) /
`CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW` (60 s). The
watermark is the max `tick.exchange_timestamp` ever consumed by that
aggregator INSTANCE (`MultiTfAggregator::watermark_secs`, one relaxed
`fetch_max` per tick advanced BEFORE the out-of-session gate so post-close
ticks count) — Dhan and Groww run separate instances and their watermarks
never cross-apply. One coalesced `warn!(code = "BOUNDARY-01", feed, seals,
cutoff_secs, watermark_secs)` fires per scan wave that sealed > 0 candles
(never per-seal spam). Severity::Medium — late but correct.

**The no-seal-past-watermark contract (the safety core):** a bucket ending
past the cutoff is still potentially being filled by a backlogged tick
stream (ILP-backpressure pause, post-restart re-tail, broadcast lag), so it
is NEVER sealed — sealing ahead of the watermark under Groww's
`LatePolicy::Discard` would silently drop the entire backlog, and under
Dhan's Refold it would corrupt candles on re-open. NO wall-clock enters the
seal decision. A catch-up seal populates the cell's amendable `last_sealed`
(Dhan Option B survives) and does NOT re-arm day-open / clear `last_sealed`
(unlike `force_seal` — the IST-midnight tasks keep those cross-day duties;
post-catch-up they are idempotent). Ratchet:
`test_catch_up_seal_all_never_seals_past_watermark`.

**Per-feed margins (F4 hostile finding, 2026-07-03):** Dhan's WS broadcast
delivers in near-capture order — 5 s absorbs its boundary jitter. Groww's
NDJSON path has MEASURED per-subject delivery skew (sidecar snapshot
freezes, byte-0 re-tail replays, bounded 4 MiB chunk drains) AND runs
`LatePolicy::Discard`: with a 5 s margin any >5 s cross-subject skew became
a counted discard. The 60 s Groww margin means only >60 s skew discards,
while still bounding the Groww seal lag to ~65 s (margin + poll cadence)
instead of the pre-BOUNDARY-01 unbounded next-tick/midnight wait.

**Poisoned-watermark defense (`reason = "watermark_future_skew"`, F2
security finding 2026-07-03):** a legit watermark can lead the IST wall
clock only by host skew (≤ 2 s per BOOT-03). If the watermark sits more than
`CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS` (10 s) AHEAD of the wall clock, a
garbage future-dated tick advanced the never-regressing `fetch_max` — the
gate returns `None`; the driver emits ONE coalesced `error!(code =
"BOUNDARY-01", reason = "watermark_future_skew", feed, watermark_secs,
now_ist_secs)` per poisoning episode (edge-latched), increments
`tv_boundary_catchup_skipped_total{feed, reason="future_skew"}` per skipped
wave, and does NOT update its last-scanned watermark. Catch-up sealing stays
disabled until the watermark self-heals at the IST-midnight
`MultiTfAggregator::reset_watermark()` (called by BOTH midnight force-seal
tasks right after `force_seal_all`). Companion fail-closed guard: the Groww
validator now REJECTS every tick when its receipt-clock read is implausible
(`GrowwTickReject::ImplausibleReceiptClock` — a broken host clock can no
longer skip the ±60 s future-skew clamp and poison the watermark; BOOT-03
class). The cutoff's wall-clock clamp (`min(…, now_ist)`) is
defense-in-depth: a bucket can never seal before the wall clock passes its
end. The cell-side bucket-end comparison uses a saturating add, so a
bucket_start near `u32::MAX` (only reachable via poisoning) never
overflow-panics — it simply never seals.

**Triage (poisoned watermark):**
1. `mcp__tickvault-logs__tail_errors` — the `watermark_future_skew` payload
   carries `feed`, `watermark_secs`, `now_ist_secs`; the delta names how far
   future the poison sits.
2. Cross-check the feed's validator rejects (Groww:
   `ImplausibleReceiptClock` / `FutureTimestamp` reject logs) and BOOT-03
   (host clock skew) — either garbage upstream timestamps or a broken host
   clock.
3. No manual action is needed for the seal path: the IST-midnight watermark
   reset self-heals it; per-tick sealing and the midnight force-seal are
   unaffected in the meantime.

**Honest envelope:** (a) a feed whose watermark STALLS (dead feed,
ILP-backpressure pause) gets NO catch-up seals — FEED-STALL-01 owns the
dead-feed page; there is deliberately NO "assume dead then force-seal
anyway" escape hatch. (b) If zero post-close ticks arrive after 15:29:59,
the final session minute still waits for the IST-midnight force-seal
(backstop unchanged) — and a day whose LAST tick lands inside
[15:30:00, 15:30:00 + margin) also still waits for the midnight force-seal
(the watermark never clears that bucket's end + margin). (c) Worst catch-up
wave ≤ ~25K seals at the 1200-SID cap (1200 × 21 TFs), inside the 200K
`SEAL_BUFFER_CAPACITY` ring envelope. (d) D1 never catch-up seals intraday
(its bucket ends next-day 09:15, past any same-day watermark) — the
midnight force-seal keeps owning D1; the Dhan driver additionally EXCLUDES
D1 from its counter and coalesced `seals` count (Dhan drops D1 at the write
boundary per `live-feed-purity.md` rule 10; Groww routes + counts D1).
(e) The coalesced `warn!`'s `seals` count includes any mpsc-DroppedFull
seals — those rows reach the ring→spill→DLQ absorption chain and are
counted separately by `tv_seal_mpsc_dropped_total`. (f) A watermark >10 s
ahead of the wall clock disables catch-up (coalesced BOUNDARY-01 error,
reason=watermark_future_skew) until it self-heals at the IST-midnight
watermark reset.

**Counters:** `tv_boundary_catchup_total{feed="dhan"|"groww"}` — one
increment per ROUTED catch-up-sealed candle (Dhan excludes D1);
`tv_boundary_catchup_skipped_total{feed, reason="future_skew"}` — one
increment per wave skipped by the poisoned-watermark guard.

**Source (actual):**
`crates/trading/src/candles/multi_tf_aggregator.rs::{watermark_secs, reset_watermark, catch_up_seal_all, compute_catchup_cutoff, CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN, CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW, CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS, CATCHUP_SEAL_POLL_INTERVAL_SECS}`,
`crates/trading/src/candles/aggregator_cell.rs::AggregatorCell::catch_up_seal`
(+ the uninitialised-slot Failure-B guard in `consume_tick`). Drivers:
`crates/app/src/main.rs` (`spawn_engine_b_aggregator` Task 4, Dhan) and
`crates/app/src/groww_bridge.rs::spawn_groww_catchup_seal` (Groww, gated on
`feed_runtime.is_enabled(Feed::Groww)` + `is_trading_day_today`).

## AGGREGATOR-LAG-01 — candle aggregator tick-broadcast lagged (zero-tick-loss PR-8b, H2-lite)

**Trigger:** the candle aggregator's `tokio::broadcast` receiver
(`spawn_seal_writer_loop` in `crates/app/src/main.rs`) returned
`RecvError::Lagged(n)` — the aggregator fell so far behind that the
broadcast dropped `n` ticks from ITS view. `TICK_BROADCAST_CAPACITY`
is 262,144 (~52 seconds of buffer at ~5K ticks/sec across the 243-SID
universe), so a `Lagged` means the aggregator task stalled for tens of
seconds — a serious incident (OOM-pressure, CPU starvation, a blocked
seal-writer). Severity::High.

**CRITICAL ASSURANCE — ticks are NOT lost and NOT reordered.** The
dropped ticks are dropped only from the *aggregator's* broadcast view.
The lossless + ORDERED durable record is the **WAL frame spill**
(`crates/storage/src/ws_frame_spill.rs`): raw frames are captured by the
WS read loop *before* any broadcast fan-out, into single-producer FIFO
segments (ring → disk spill → DLQ), replayed in exact append order on
boot. Because this broadcast `Lagged` is strictly *downstream* of that
WAL, it can affect ONLY the derived candles (`candles_*_shadow`) for the
lagged window — never the durable tick record, never tick order. (The
`ticks` table's own persistence consumer is also a broadcast subscriber
that can lag — but it is backfilled from the WAL on recovery and alarmed
via `tv_ticks_permanently_lost`, so the WAL, not the ticks-table
consumer, is the lossless guarantor.) The 15:31 IST post-market 1m
cross-verify pinpoints the affected minutes for rebuild from the
WAL-backed, ts-ordered `ticks` table. Tick routing + ordering on the
live WS read loop are untouched by this code path.

**Why it was upgraded from a silent counter (audit Rule 5):** before
PR-8b the `Lagged` arm only did `counter!("tv_aggregator_tick_lag_total")`
— a candle-data-loss-class event with zero operator signal. It now emits
`error!(code = AGGREGATOR-LAG-01)` so it routes to Telegram + the
`errors.jsonl` forensic sink.

**Triage:**
1. The `error!` payload carries `skipped` (tick count). Inspect
   `data/logs/errors.jsonl.*` for the timestamp → that is the lagged
   window.
2. Root-cause the stall: `mcp__tickvault-logs__run_doctor` + check host
   memory/CPU (a >52s stall implies OOM pressure or a blocked
   seal-writer — cross-check `AGGREGATOR-SEAL-01` / `AGGREGATOR-DROP-01`
   and `tv_seal_mpsc_dropped_total`).
3. **Rebuild the affected candles (no tick is lost):** the 15:31 IST
   post-market 1-minute cross-verify (`CROSS-VERIFY-1M-01`) compares
   `candles_1m` vs Dhan's authoritative 1m candles, exact-match, and
   names every mismatched minute. Those minutes are rebuildable from the
   lossless, ordered `ticks` table.

**Auto-triage safe:** NO (Severity::High; a >52s aggregator stall needs
operator root-cause — the candle under-count is recoverable but the
underlying stall is not self-healing).

**Source:** `crates/app/src/main.rs` (the aggregator subscriber
`RecvError::Lagged` arm in `spawn_seal_writer_loop`),
`crates/common/src/error_code.rs::AggregatorLag01TickLagDropped`.
