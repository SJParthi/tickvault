# Implementation Plan: Watermark-Aware Per-Minute Catch-Up Seal (BOUNDARY-01 first real emitter)

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — session directive 2026-07-03: "Design + implement a watermark-aware per-minute catch-up seal that NEVER mass-discards backlogged ticks"
**Branch:** `claude/laughing-faraday-xkkbty`
**Changed crates:** `trading` (crates/trading), `app` (crates/app)
**Evidence base:** `seal-lag-research-findings.md` (scratchpad, 2026-07-03) — its 16 design constraints are BINDING on this plan.

## Design

**Problem (Verified in the research findings).** A minute (and every other TF)
cell seals ONLY on (i) the SAME instrument's next tick crossing the bucket
boundary (`aggregator_cell.rs::consume_tick`), or (ii) the once-per-day
IST-midnight `force_seal_all` (`main.rs` Task 3 / `groww_bridge.rs`
`spawn_groww_ist_midnight_force_seal`). An instrument that stops ticking
mid-session leaves its candle rows absent until midnight; the final session
minute (15:29 M1) is structurally absent from 15:30 until 00:00 IST because
the out-of-session gate blocks ≥15:30 ticks. A NAIVE wall-clock force-seal is
forbidden: under `LatePolicy::Discard` (Groww) it mass-discards the entire
backlog (research Failure A), and even without interleaving it corrupts
candles by re-opening the sealed bucket as a fresh day-open bar (Failure B),
and it breaks Dhan's Option B 1-bucket-late amend (Failure C).

**The design (locked by the coordinator; implemented exactly):**

- **D1 — per-instance event-time watermark.** `MultiTfAggregator` gains
  `max_seen_exchange_ts_secs: Arc<AtomicU32>` (Arc because the struct is
  `#[derive(Clone)]` — a bare `AtomicU32` would give each clone, incl. the
  driver task's clone, its own divergent watermark; sharing through the Arc is
  the only correct semantics). `consume_tick` does ONE
  `fetch_max(tick.exchange_timestamp, Relaxed)` BEFORE the out-of-session
  early return (post-close ticks must advance it so the final session minute
  can seal). Duplicate-immune by construction: max never regresses on a
  frozen-snapshot re-dump (constraint 14). Getter `watermark_secs()`.
- **D2 — cell primitive `AggregatorCell::catch_up_seal(tf, cutoff_secs)`.**
  Cold path. Locks the slot (pinned order slot → last_sealed), `None` on an
  uninitialised slot, computes `bucket_end` via `TfIndex::bucket_end` (no
  duplicated math), refuses to seal when `bucket_end > cutoff_secs` (the
  no-mass-discard contract), otherwise drains the slot to empty AND COPIES
  the sealed state into `last_sealed[tf]` (constraint 5 — Option B survives).
  It does NOT touch `armed_for_day_open` (constraint 6) and does NOT clear
  `last_sealed` — explicitly contrasted with `force_seal` in the doc comment.
- **D3 — Failure-B guard in `consume_tick`'s uninitialised-slot arm.** After a
  catch-up seal empties a slot, a backlogged tick whose bucket is `<=`
  `last_sealed.bucket_start` must NOT `from_first_tick` (re-open) that bucket:
  it routes through the late-arm semantics instead (Refold: `fold_late_hlc` +
  `AmendedLate` iff same bucket, else `DiscardLate`; Discard: `DiscardLate`).
  Only `bucket_start > last_sealed.bucket_start` (or an uninitialised
  `last_sealed` — boot / post-force_seal) opens a fresh bucket, preserving the
  `bucket_start_cumulative` volume continuity exactly as before (constraint 7).
- **D4 — `MultiTfAggregator::catch_up_seal_all(cutoff, on_seal) -> usize`.**
  Mirrors `force_seal_all` (papaya iterate × 21 TFs, cold path O(N×21));
  callback signature is `FnMut(u64, u8, TfIndex, LiveCandleState)` — the same
  as `force_seal_all`, because the app-side `route_seal` needs the instrument
  identity (the D4 sketch's `(TfIndex, &LiveCandleState)` cannot route).
  Returns the seal count for the coalesced BOUNDARY-01 log line.
- **D5 — per-feed driver tasks (cold path, self-gating).** Constants
  `CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN = 5`,
  `CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW = 60` (per-feed margins — F4
  hardening 2026-07-03: Groww's NDJSON path has measured per-subject
  delivery skew, so one full minute of allowed lateness converts ">5 s skew
  = counted discard" into "only >60 s skew discards" while bounding Groww
  seal lag to ~65 s), `CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS = 10` and
  `CATCHUP_SEAL_POLL_INTERVAL_SECS = 5` in
  `crates/trading/src/candles/multi_tf_aggregator.rs` (re-exported from
  `candles::mod`). Each wave gates through the shared PURE
  `compute_catchup_cutoff(watermark, last_scanned, now_ist, margin, guard)`
  (F2 hardening): `None` on no-tick-yet / self-gate / POISONED watermark
  (> now_ist + guard — a garbage future-dated tick advanced the
  never-regressing fetch_max; the driver emits ONE edge-latched
  `error!(code = "BOUNDARY-01", reason = "watermark_future_skew")` +
  `tv_boundary_catchup_skipped_total{feed, reason="future_skew"}` per wave
  and does NOT update last_scanned), else
  `Some(min(watermark − margin, now_ist))` — the wall-clock clamp is
  defense-in-depth so a bucket can never seal before the wall clock passes
  its end. Self-heal: `MultiTfAggregator::reset_watermark()` runs in BOTH
  IST-midnight force-seal tasks right after `force_seal_all`, so poisoning
  self-heals within one day. Dhan: Task 4
  in `spawn_engine_b_aggregator` next to the midnight Task 3, same bare
  `tokio::spawn` supervision level as both midnight tasks; every 5 s reads
  `watermark_secs()`, skips when it has not advanced since the last scan
  (self-gating — overnight/holiday = no-op), gates on
  `is_trading_day_today()` + `global_seal_sender()`, then
  `catch_up_seal_all(cutoff, …)` routing through the SAME Dhan
  `SealRouteParams` as the per-tick site (Feed::Dhan, drop_d1=true,
  prev_day_cache, heartbeat). Groww: `spawn_groww_catchup_seal` in
  `groww_bridge.rs`, spawned once by `spawn_supervised_groww_bridge` next to
  the midnight force-seal, additionally gated on
  `feed_runtime.is_enabled(Feed::Groww)`, routing with the Groww midnight
  params (Feed::Groww, drop_d1=false, no pct-stamp, no heartbeat). The two
  midnight force-seal tasks are UNTOUCHED (they keep the cross-day clear +
  re-arm duties; post-catch-up they are idempotent — force_seal on an empty
  slot returns None).
- **D6 — observability.** Counter `tv_boundary_catchup_total{feed}` per
  catch-up-sealed candle (static labels). ONE coalesced
  `warn!(code = ErrorCode::Boundary01CatchupSeal.code_str(), …)` per scan
  wave that sealed > 0 candles (never per-seal spam; BOUNDARY-01 is
  Severity::Medium so `warn!` is level-appropriate and the tag-guard `code =`
  contract is satisfied). Close the silent-Groww-discard gap: the Groww
  consume site now reads `stats.late_count` and increments
  `tv_aggregator_late_ticks_discarded_total{feed="groww"}` (the Dhan site
  stays label-less, following the established `tv_seal_mpsc_dropped_total`
  precedent in `seal_routing.rs` — Dhan unlabeled, Groww `feed=groww` — so
  the existing `tv-aggregator-late-tick-sustained` alert series is not
  renamed/broken). Rule file `wave-6-error-codes.md` BOUNDARY-01 +
  AGGREGATOR-LATE-01 sections updated with a dated 2026-07-03 block.

**Why the watermark is safe under both late policies:** file order preserves
capture order and `consume_tick` is single-writer per instrument, so every
tick belonging to a bucket whose end is ≤ (max-seen event time − the
per-feed margin: 5 s Dhan / 60 s Groww)
has provably already been consumed by the time the scan runs — sealing such a
bucket can never orphan a backlogged tick. A bucket the backlog is still
filling has `bucket_end > cutoff` and is left open. D1 (daily TF) never
catch-up seals intraday (`bucket_end` = next day's 09:15 > any same-day
watermark) — the midnight force-seal keeps owning D1, matching today.

## Edge Cases

1. **Backlogged feed (watermark minutes behind wall-clock)** — cutoff trails
   the backlog; zero seals fire ahead of it (test 8). This is the exact
   Failure-A scenario; no mass discard is possible by construction.
2. **Frozen-snapshot re-dump (same ts re-delivered)** — `fetch_max` never
   regresses/advances on duplicates (constraint 14; test 1).
3. **Post-close ticks (≥ 15:30)** — advance the watermark even though the
   cell fold is gated (test 2), so the 15:29 bucket seals once
   watermark ≥ 15:30:00 + margin (test 10).
4. **Zero post-close ticks after 15:29:59** — the watermark never crosses
   15:30 + margin; the final minute waits for the IST-midnight force-seal
   (backstop unchanged — honest envelope (b)).
5. **Dead feed / ILP-backpressure pause** — watermark stalls → the driver's
   advanced-since-last-scan gate skips every wave; NO seals fire.
   FEED-STALL-01 owns the dead-feed page (constraint 15); there is
   deliberately NO "assume dead then force-seal anyway" escape hatch.
6. **Dhan Option B late tick after a catch-up seal** — `last_sealed` was
   populated (not cleared), so the 1-bucket-late tick still amends in place
   (test 5).
7. **Groww late tick after a catch-up seal** — the D3 guard prevents the
   uninit slot from re-opening the sealed bucket; DiscardLate (test 6),
   which the new Groww late counter now makes visible.
8. **Newer tick after a catch-up seal** — opens the next bucket with
   `bucket_start_cumulative` = the per-instrument `last_cumulative` baseline,
   preserving incremental-volume continuity (test 7); `armed_for_day_open`
   was not re-armed so the bucket opens at LTP, not day_open (test 4).
9. **Midnight after a catch-up-drained day** — `force_seal` on emptied slots
   returns None; its clear-last_sealed + re-arm duties still run (test 9).
10. **Late tick that does not advance the watermark opening a fresh bucket
    after the last watermark advance of the day** — that bucket is not
    catch-up-sealed until the next watermark advance (or midnight). Bounded
    staleness, never data loss; documented, accepted.
11. **Non-trading-day / disabled-Groww midnight parity** — the drivers carry
    the same `is_trading_day_today()` / `is_enabled(Feed::Groww)` gates as
    the midnight tasks (constraint 11); the watermark self-gate makes the
    idle case a no-op anyway.
12. **`global_seal_sender() == None`** (seal-writer not yet installed) —
    the wave is skipped without consuming the watermark advance, so the next
    wave retries (no seal is drained-and-dropped).

## Failure Modes

| Failure | Behaviour | Signal |
|---|---|---|
| Seal mpsc full during a catch-up wave | `route_seal` returns `DroppedFull`; downstream ring→spill→DLQ absorbs; wave continues | `tv_seal_mpsc_dropped_total` (+`feed=groww` label for Groww) |
| Watermark stalls (dead feed) | No catch-up seals — fail-closed; buckets wait for ticks or midnight | FEED-STALL-01 (existing watchdog) |
| Driver task panics | Same exposure as the existing midnight tasks (bare `tokio::spawn`, mirrored supervision level); per-tick sealing and the midnight backstop are unaffected | AGGREGATOR-HB-01 heartbeat + 15:31 cross-verify still bound the loss window |
| Catch-up wave overlaps a live tick fold | Per-slot `parking_lot::Mutex` serializes; either the tick seals the bucket first (catch-up then sees an uninit/newer slot) or catch-up seals first (the tick routes through the D3 guard) — no double-seal, and a double EMIT for the same bucket UPSERTs in place (DEDUP `ts, security_id, segment, feed`) |
| Seal burst at the 1200-SID cap | ≤ ~25K seals per wave (1200 × 21) — inside the 200K `SEAL_BUFFER_CAPACITY` ring envelope drained 1024/100 ms (constraint 13) | ring/spill gauges |

## Test Plan

All block-scoped to the trading crate (`cargo test -p tickvault-trading`),
plus `cargo check -p tickvault-app` for the drivers. Named tests:

1. `test_watermark_advances_and_never_regresses` — fetch_max semantics incl.
   stale/duplicate ts (multi_tf_aggregator.rs).
2. `test_watermark_advances_on_out_of_session_tick` — post-15:30 tick
   advances the watermark though the fold is gated.
3. `test_catch_up_seal_respects_cutoff` — bucket_end > cutoff → None;
   ≤ cutoff → Some (aggregator_cell.rs).
4. `test_catch_up_seal_populates_last_sealed_and_keeps_day_open_unarmed` —
   contrast with force_seal.
5. `test_post_catchup_late_tick_dhan_amends_in_place` — Option B survives
   (AmendedLate on the catch-up-sealed minute).
6. `test_post_catchup_late_tick_groww_discards_not_reopens` — Failure-B
   guard: uninit slot + older-or-equal bucket → DiscardLate, never
   from_first_tick.
7. `test_post_catchup_newer_tick_opens_next_bucket_with_volume_continuity` —
   cumulative baseline preserved.
8. `test_catch_up_seal_all_never_seals_past_watermark` — the no-mass-discard
   ratchet: backlogged watermark → 0 seals.
9. `test_force_seal_after_catchup_is_idempotent` — force_seal on emptied
   slot → None; midnight duties (clear last_sealed, re-arm) still verified.
10. `test_boundary_catchup_final_session_minute_seals_when_watermark_passes_close` —
    the 15:29 bucket seals once watermark ≥ 15:30:00 + margin.
11. Existing ratchets stay green: DHAT
    `dhat_multi_tf_consume_tick` (fetch_max adds zero allocations),
    `aggregator_daily_universe_scale_guard` (1200-SID force_seal_all), the
    full trading-crate suite, and the app-crate check + its source-scan
    anchors (`test_run_groww_bridge_spawns_ist_midnight_force_seal` etc.).

## Rollback

Pure-additive change: revert the branch commits (`git revert`) to return to
next-tick + IST-midnight-only sealing. No schema change (catch-up seals write
the SAME `candles_<tf>` tables through the SAME writer with the SAME DEDUP
key), no config change, no new dependency. Rows already catch-up-sealed
remain valid candles; a post-revert re-seal of the same bucket UPSERTs in
place. The Groww late counter and the BOUNDARY-01 log are observability-only
and revert cleanly with the code.

## Observability

- `tv_boundary_catchup_total{feed="dhan"|"groww"}` — one increment per
  ROUTED catch-up-sealed candle (static labels only; Dhan excludes D1,
  which is dropped at the write boundary — F5 hardening 2026-07-03).
- `tv_boundary_catchup_skipped_total{feed, reason="future_skew"}` — one
  increment per wave skipped by the poisoned-watermark guard, plus ONE
  edge-latched coalesced `error!(code = "BOUNDARY-01",
  reason = "watermark_future_skew")` per poisoning episode (F2 hardening).
- ONE coalesced `warn!(code = "BOUNDARY-01" …, feed, seals, cutoff_secs,
  watermark_secs)` per scan wave that sealed > 0 (Severity::Medium per
  `ErrorCode::Boundary01CatchupSeal`; runbook =
  `.claude/rules/project/wave-6-error-codes.md`, updated in this PR with the
  real source pointers replacing the never-built `boundary_timer.rs`).
- `tv_aggregator_late_ticks_discarded_total{feed="groww"}` — Groww late
  discards were previously completely silent (`stats.late_count` unread);
  now counted, mirroring the Dhan site (which keeps its label-less series so
  the existing `tv-aggregator-late-tick-sustained` alert is unbroken).
- Existing signals unchanged: AGGREGATOR-HB-01 heartbeat (catch-up seals
  routed with the Dhan heartbeat params count in `seals_emitted`), seal-ring
  gauges, 15:31 IST 1m cross-verify.

## Plan Items

- [x] Item 1 — Plan file (this document, design-first wall gate)
  - Files: .claude/plans/active-plan-watermark-catchup-seal.md
  - Tests: (none — docs)
- [x] Item 2 — D1 watermark + D2 `catch_up_seal` + D3 Failure-B guard + D4
  `catch_up_seal_all` + constants + the 10 new tests
  - Files: crates/trading/src/candles/multi_tf_aggregator.rs, crates/trading/src/candles/aggregator_cell.rs, crates/trading/src/candles/mod.rs
  - Tests: test_watermark_advances_and_never_regresses, test_watermark_advances_on_out_of_session_tick, test_catch_up_seal_respects_cutoff, test_catch_up_seal_populates_last_sealed_and_keeps_day_open_unarmed, test_post_catchup_late_tick_dhan_amends_in_place, test_post_catchup_late_tick_groww_discards_not_reopens, test_post_catchup_newer_tick_opens_next_bucket_with_volume_continuity, test_catch_up_seal_all_never_seals_past_watermark, test_force_seal_after_catchup_is_idempotent, test_boundary_catchup_final_session_minute_seals_when_watermark_passes_close
- [x] Item 3 — D5 Dhan + Groww driver tasks, D6 observability (BOUNDARY-01
  emitter, Groww late counter), rule-file update
  - Files: crates/app/src/main.rs, crates/app/src/groww_bridge.rs, .claude/rules/project/wave-6-error-codes.md
  - Tests: (app compile + existing source-scan ratchets; the load-bearing
    seal-routing closure params are pinned by the existing `route_seal`
    unit tests; the trading-crate primitives above carry the logic tests)
- [x] Item 4 — Consolidated adversarial-review hardening (2026-07-03):
  F1 Groww validator fail-closed on implausible receipt clock
  (`GrowwTickReject::ImplausibleReceiptClock`); F2 poisoned-watermark
  defense — pure `compute_catchup_cutoff` gate shared by both drivers
  (self-gate + future-skew guard + wall-clock clamp), coalesced BOUNDARY-01
  `error!` with `reason = "watermark_future_skew"` +
  `tv_boundary_catchup_skipped_total{feed, reason="future_skew"}`, and
  `reset_watermark()` self-heal in BOTH IST-midnight force-seal tasks;
  F3 overflow-safe (saturating) bucket-end comparison in
  `AggregatorCell::catch_up_seal`; F4 per-feed lateness margins
  (`CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN = 5`,
  `CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW = 60`); F5 Dhan catch-up counter
  + coalesced `seals` count exclude D1 (dropped at the write boundary);
  F6 honest-envelope addenda in the rule file + this plan.
  - Files: crates/trading/src/candles/multi_tf_aggregator.rs, crates/trading/src/candles/aggregator_cell.rs, crates/trading/src/candles/mod.rs, crates/app/src/main.rs, crates/app/src/groww_bridge.rs, .claude/rules/project/wave-6-error-codes.md
  - Tests: test_compute_catchup_cutoff_gates, test_compute_catchup_cutoff_clamps_to_wall_clock, test_reset_watermark_zeroes_and_next_tick_rebuilds, test_catch_up_seal_bucket_end_saturates_at_u32_max, test_validate_groww_tick_fail_closed_on_implausible_receipt_clock

## Per-Item Guarantee Matrix (15-row, per `per-wave-guarantee-matrix.md`)

| Demand | This change's proof |
|---|---|
| 100% code coverage | 10 new unit tests over every new branch (cutoff refuse/allow, guard refold/discard/fresh-open, watermark advance/regress/out-of-session); coverage delta ≥ 0 |
| 100% audit coverage | N/A — no new typed event class; catch-up seals land in the SAME `candles_<tf>` tables with the SAME DEDUP UPSERT key `(ts, security_id, segment, feed)` (the audit surface) |
| 100% testing coverage | unit (10 new) + existing property/loom untouched + DHAT zero-alloc ratchet re-run + 1200-SID scale guard re-run |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + plan-gate + secret-scan all run pre-commit/push |
| 100% code performance | hot path gains exactly ONE relaxed atomic fetch_max; DHAT `dhat_multi_tf_consume_tick` re-run green (0 alloc delta); scan is cold-path O(N×21) at 5 s cadence |
| 100% monitoring | `tv_boundary_catchup_total{feed}` counter + coalesced BOUNDARY-01 log + existing heartbeat |
| 100% logging | `warn!` carries `code = ErrorCode::Boundary01CatchupSeal.code_str()` (tag-guard) |
| 100% alerting | BOUNDARY-01 severity Medium routes via the existing 5-sink error pipeline; no new Critical class introduced |
| 100% security | no new input surface, no secrets, no unsafe; security posture unchanged |
| 100% security hardening | N/A — no attack-surface delta (in-process cold-path task) |
| 100% bugs fixing | the three researched failure sequences (A mass-discard, B day-open corruption, C Option-B break) each have a dedicated regression test (8, 4+6, 5) |
| 100% scenarios covering | 12 edge cases enumerated above, each mapped to a test or an explicit honest-envelope line |
| 100% functionalities covering | every new pub fn (`watermark_secs`, `catch_up_seal`, `catch_up_seal_all`) has BOTH a test and a production call site |
| 100% code review | coordinator runs the adversarial 3-agent review on this branch before the PR opens (per the task contract) |
| 100% extreme check | test 8 is the build-failing ratchet for the no-seal-past-watermark contract |

## Resilience Demand Matrix (7-row)

| Demand | This change's proof |
|---|---|
| Zero ticks lost | NO new tick-drop path: the catch-up seal refuses to seal at/past the watermark cutoff, so no backlogged tick can be orphaned; the D3 guard converts the one new post-seal arrival case into the EXISTING late-arm semantics (amend or counted discard) |
| WS never disconnects | untouched — no WS code changed |
| Never slow/locked/hanged | one relaxed fetch_max on the hot path (DHAT-proven 0 alloc); scans are 5 s-cadence cold path taking the same per-slot Mutexes as `force_seal_all` |
| QuestDB never fails | untouched — same route_seal → ring→spill→DLQ absorption chain |
| O(1) latency | fetch_max is O(1); the scan is O(N×21) and honestly FLAGGED as such (cold path, same class as `force_seal_all`) — never claimed O(1) |
| Uniqueness + dedup | catch-up seals stamp bucket-start (never wall-clock) into `ts` and route through the same writer — DEDUP `(ts, security_id, segment, feed)` collapses any re-emit |
| Real-time proof | `tv_boundary_catchup_total{feed}` + the coalesced BOUNDARY-01 line + heartbeat `seals_emitted` observable within one 60 s heartbeat window |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: (a) the
catch-up seal fires ONLY for buckets whose end ≤ min(the feed's event-time
watermark − the PER-FEED margin, the IST wall clock) —
`CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN = 5` /
`CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW = 60` (Groww's NDJSON path has
measured per-subject delivery skew: 60 s converts ">5 s skew = counted
discard" into "only >60 s skew discards" while bounding Groww seal lag to
~65 s) — ratcheted by `test_catch_up_seal_all_never_seals_past_watermark` +
`test_compute_catchup_cutoff_gates` +
`test_compute_catchup_cutoff_clamps_to_wall_clock`; a feed whose watermark
stalls (dead feed, ILP-backpressure pause) receives NO catch-up seals —
FEED-STALL-01 owns the dead-feed page, and there is deliberately NO "assume
dead then force-seal anyway" escape hatch; (b) if zero post-close ticks
arrive after 15:29:59, the final session minute still waits for the
IST-midnight force-seal (backstop unchanged) — and a day whose LAST tick
lands inside [15:30:00, 15:30:00 + margin) likewise still waits for the
midnight force-seal (the watermark never clears that bucket's end + margin);
(c) the worst catch-up wave is ≤ ~25K seals at the 1200-SID cap (1200 × 21
TFs), inside the 200K `SEAL_BUFFER_CAPACITY` seal-ring envelope drained at
1024/100 ms — and the coalesced `warn!`'s `seals` count includes any
mpsc-DroppedFull seals (those rows reach the ring→spill→DLQ absorption chain
and are counted separately by `tv_seal_mpsc_dropped_total`); (d) a watermark
more than `CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS = 10` s ahead of the IST
wall clock is POISONED — catch-up sealing is disabled (ONE coalesced
BOUNDARY-01 `error!` with `reason = "watermark_future_skew"` +
`tv_boundary_catchup_skipped_total{feed, reason="future_skew"}`) until the
IST-midnight `reset_watermark()` self-heals it, and the Groww validator now
fails CLOSED on an implausible receipt clock so a broken host clock can no
longer poison the watermark in the first place
(`test_validate_groww_tick_fail_closed_on_implausible_receipt_clock`);
(e) the Dhan driver's counter + `seals` count exclude D1 (dropped at the
write boundary per `live-feed-purity.md` rule 10; Groww routes + counts D1).
Beyond the envelope, the seal-ring's spill → DLQ NDJSON catches every
payload as recoverable text.
