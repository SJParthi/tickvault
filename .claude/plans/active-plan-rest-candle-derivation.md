# Implementation Plan: REST-Era Multi-TF Candle Derivation (bar-fold writer)

**Status:** VERIFIED
**Date:** 2026-07-16
**Approved by:** Parthiban (operator directive 2026-07-16, verbatim quotes below)

> **Operator authority (2026-07-16, verbatim):**
> 1. *"why the fuck remaining candles 1m till 1day is not yet generated and populated — resolve these"*
> 2. *"for only spots we will have minimum one month data because anyhow based on underlying spots alone only trading decision will be entered or exited — but option only for the current day"*
> 3. *"everything should be always available in our own questdb right — our entire one month should be stored and fetched from questdb even before premarket"*

Crates touched: **crates/app** (new `crates/app/src/rest_candle_fold.rs`, edits to
`crates/app/src/spot_1m_rest_boot.rs`, `crates/app/src/groww_spot_1m_boot.rs`,
`crates/app/src/main.rs`, new test `crates/app/tests/rest_candle_fold_wiring_guard.rs`),
**crates/common** (`crates/common/src/config.rs` — new `RestCandleFoldConfig`;
`crates/common/src/error_code.rs` — new `RestCandleFold01Degraded` / `FOLD-01`),
**crates/storage** (`crates/storage/src/partition_archive.rs` — retention-class edit),
plus `config/base.toml`, rule files, and `.claude/triage/error-rules.yaml`.

## Design

The 21 `candles_*` tables are PRODUCER-DEAD on main (Verified by the 2026-07-16 audit,
`scratchpad/candle-truth.md`): the 21-TF aggregator + seal writer spawn but the tick
broadcast is publisher-less (Dhan input severed by #1522, Groww by #1581). The operator
demands candles_1m..candles_1d generated + a month of spot depth resident in QuestDB
before pre-market.

**The fix is a cold-path BAR-FOLD writer** (new module `crates/app/src/rest_candle_fold.rs`),
NOT tick synthesis (live-feed-purity forbids synthesized ticks; a bar fold is EXACT for
O/H/L/C/ΣV given 1m bars):

1. **Input** = confirmed `spot_1m_rest` 1-minute bars, per feed (`dhan` + `groww`), per
   SID (4 IDX_I spot indices each). Two legs:
   - **Live handoff:** both spot REST legs (`spot_1m_rest_boot.rs` Dhan fire + sweep;
     `groww_spot_1m_boot.rs` fire + sweep) collect a `ConfirmedBar` alongside each
     `staged.push(...)` and — ONLY after the ILP flush ACK (the same `flush_result.is_ok()`
     gate that advances the persisted watermark) — hand them to the fold task via
     `rest_candle_fold::send_confirmed_bars(...)`. The sender is a process-global
     bounded mpsc behind a `OnceLock` (`set_global_fold_bar_sender` — the
     `global_seal_sender` house precedent), so NO param-struct cascade through
     `DhanRestStackParams` / `Spot1mRestTaskParams` / `GrowwSpot1mTaskParams` is needed;
     a disabled fold (no sender installed) makes the handoff a zero-cost no-op.
   - **Boot catch-up:** at boot (after a quiet QuestDB readiness probe), for each feed,
     discover the `(security_id, exchange_segment)` identities present in `spot_1m_rest`
     over the last `catchup_days` (default 35), then per SID read the bars (bounded
     `/exec` SELECTs — micros WHERE window, nanos-projected ts, explicit LIMIT with loud
     truncation, 8 MiB response cap, segment allowlist re-check — the hardened
     `tf_consistency_boot.rs` read shapes) and fold them through the SAME engine,
     emitting seals. DEDUP-idempotent (`ts, security_id, segment, feed`), so re-runs
     UPSERT in place. This is what backfills candles_* from whatever spot_1m_rest
     history exists — the month-deep-in-QuestDB requirement.
2. **The fold core is pure** (`SidFoldState::fold_bar`): per (feed, sid, segment), 21
   per-TF open buckets keyed by `TfIndex::bucket_start` (the 09:15-anchored grid); fold
   semantics = o=first, h=max, l=min, c=last, vol=Σ (i64 checked/saturating, negatives
   clamped at intake — matches the Groww i64 volume discipline). A TF bucket SEALS when
   (a) a folded bar's minute-end reaches the bucket's session-truncated effective end
   (`min(start + tf_secs, 15:30)` — covers final partials AND the 1d bucket, which
   seals at session close), or (b) a bar belonging to a LATER bucket arrives (covers
   gaps + day boundaries). Golden-tested to agree EXACTLY with
   `tf_consistency_boot::recompute_window` over the same synthetic day.
3. **Emission** = `BufferedSeal::new(sid, segment_code, tf, state, feed)` into the
   EXISTING `global_seal_sender`, so the proven ring→spill→DLQ→ILP chain
   (`ShadowCandleWriter`, DEDUP `ts, security_id, segment, feed`) is reused unchanged.
   `tick_count` written 0 honestly (unknowable from REST bars); `oi` 0; pct columns
   0.0 fail-soft (the PREVDAY-01 precedent). D1 is EMITTED (no drop_d1) per the
   operator's "1m till 1day" demand + the dated live-feed-purity rule-10 edit riding
   this PR — the fold does NOT route through `route_seal` (whose Dhan policy drops D1
   and inflates live-aggregator counters); it try_sends directly with its own
   `tv_rest_candle_fold_*` counters.
4. **Out-of-order bars (backfill/sweep repairs — REDESIGNED 2026-07-16 round-2 HIGH):**
   every bar received for the CURRENT trading day ALSO lands in a per-(feed, sid,
   segment) in-RAM **day-map** (minute → last-received bar, last-write-wins; ≤375
   entries × 8 keys — trivial memory). A current-day repair — an out-of-order minute OR
   a value-UPDATE of an already-folded minute — updates the map and synchronously
   REFOLDS the whole day from the map through a fresh engine (`refold_from_day_map`,
   ≤375 bars × 21 TFs — microseconds, cold path), swaps the live engine, and re-emits
   every closed bucket (DEDUP UPSERT heals in place). Lossless by construction — NO
   QuestDB read, NO ILP-ACK → `/exec` WAL-apply race on the today path (the round-1
   `/exec` today-refold, whose presence-only gate missed value-updates and whose read
   could miss bars folded between mark and drain, is RETIRED). Day identity is the
   ENGINE's current day, never the wall clock — a post-midnight repair for the still-open
   session day refolds from the retained map (kills the round-2 midnight-clobber
   MEDIUM). Only a repair for a STRICTLY-OLDER day (day-map already rolled) takes the
   debounced PAST-day `/exec` refold: corrected CLOSED-day seals only, NEVER a live
   engine touch, WAL-apply visibility GATED (M2 — the mark carries the triggering
   minute; a stale read re-queues bounded 5 attempts, then degrades loudly,
   `refold_stale_read`); the next boot's catch-up re-derives. Day roll: a new trading
   day's first bar transition-seals the old day's residue via the engine (as before)
   and clears the map. Boot catch-up SEEDS the day-map for today from the catch-up's
   today rows (`SidDayFold::from_catchup`), so a mid-session restart refolds
   losslessly.
5. **Config:** `[rest_candle_fold]` → `RestCandleFoldConfig { enabled (serde default
   FALSE — fail-safe), catchup_days (default 35, validated 1..=370 — headroom so a future retention widening never needs a validation-bound edit) }`; `config/base.toml`
   opts in with `enabled = true`, `catchup_days = 35`.
6. **Retention (the month-hot ruling + the 30 GB disk constraint,
   `scratchpad/retention-truth.md`):** `market_data_hot_days` default + base.toml 14 → 35
   (candles_* month-hot; `ticks` shares the class but is frozen post-2026-07-15, so no
   growth impact), AND `option_chain_1m` + `option_contract_1m_rest` MOVE from the 90d
   Standard class into the 35d MarketData class (smallest clean change: two table names
   added to `retention_class`) per the operator's "option only for the current day" +
   month-for-verification ruling — month-hot chain ≈ 3.0–3.2 GB both feeds vs ~8.6 GB
   at 90d.
7. **Observability:** ErrorCode `FOLD-01` (`RestCandleFold01Degraded`, High,
   auto-triage-safe, log-sink-only — no `error_code_alerts` entry) with stage taxonomy
   `catchup_query` (incl. HTTP-client build failure + the round-2 LOW-4
   `reason="no_http_client"` past-day-marks drop) / `catchup_parse` /
   `discovery_truncated` / `refold_stale_read` (counter per failed ATTEMPT, `error!` on
   exhaustion — round-2 LOW-7 wording; past-day path only) / `seal_send` /
   `future_dated` (round-3 — the BOUNDARY-01 future-skew clamp; one coalesced coded
   error per drained batch) / `volume_saturated` / `bad_identity` (round-2 LOW-6 — the
   handoff-site coalesced warn) / `receiver_lost` / `sender_install` / `task_respawn`;
   counters
   `tv_rest_candle_fold_seals_total{feed}`,
   `tv_rest_candle_fold_catchup_rows_total{feed}`, `tv_rest_candle_fold_errors_total{stage}`,
   `tv_rest_candle_fold_dropped_total{reason}` (round-2 adds `no_http_client`; LOW-1: a
   CLOSED seal channel counts every remaining seal of the slice; round-3 adds
   `seal_channel_closed` — closed is its own reason on both live + paced paths — and
   `future_dated`),
   `tv_rest_candle_fold_day_refolds_total{feed}` (round-2 — current-day day-map
   refolds; round-3: one per DIRTY SLOT per drained repair batch, never per repair
   bar), `tv_rest_candle_fold_duplicate_bars_total` (value-identical redeliveries
   (PartialEq; NaN-unequal falls through to a harmless idempotent refold) — no-op),
   `tv_rest_candle_fold_paced_waits_total` (budget is per (feed, SID, day) —
   round-2 LOW-7), `tv_rest_candle_fold_volume_saturated_total`,
   `tv_rest_candle_fold_refold_queued_total` (PAST-day marks only since round-2),
   `tv_rest_candle_fold_heartbeat_total` (the dense per-interval liveness signal — the
   HIGH-3 positive progress series), `tv_rest_candle_fold_task_respawn_total{reason}`
   (static bounded labels only — never per-TF); supervised task (house respawn pattern)
   with an RAII receiver re-park guard (HIGH-2 — a respawn RESUMES bar consumption; a
   lost receiver is a LOUD `receiver_lost` error, never a silent clean_exit);
   new rule file `.claude/rules/project/rest-candle-fold-error-codes.md` + triage rule.

## Edge Cases

- **Session window:** only minutes in `[09:15:00, 15:30:00)` IST fold; out-of-session
  bars are counted + skipped (never a bucket). Session constants const-asserted against
  the canonical common-crate nanos gates (the tf_consistency discipline).
- **09:15 open / 15:30 close boundaries:** first bucket of every TF starts exactly
  09:15; the 15:29 bar completes the M1 bucket AND every final partial (2m..4h, 1d) via
  the effective-end rule — no timer needed.
- **Final partial buckets:** H4 `[13:15, 17:15)` truncates to 15:30; sealed on the
  15:29 bar (or on the next day's first bar if 15:29 is absent — honest late seal).
- **Missing session tail (no 15:29 bar):** final buckets stay open until the next
  bar transitions them (next trading day's 09:15 bar seals yesterday's partials); a
  later sweep repair of the tail triggers the dirty-day refold which re-emits corrected
  buckets.
- **Day boundary / D1:** `bucket_start(D1)` = the day's 09:15 (day-anchored), so a new
  day's bar transitions + seals the previous D1; catch-up force-seals open buckets of
  PAST days at the end of the pass.
- **Duplicate delivery of the same minute:** a value-identical redelivery (PartialEq;
  a NaN field compares unequal and falls through to a harmless idempotent refold) is a
  counted no-op (`duplicate_bars_total`); a SAME-minute value-UPDATE refolds the day
  from the day-map (idempotent DEDUP re-emit; round-3: once per dirty slot per drained
  batch) — never a double-count in RAM (round-2).
- **Negative / overflowing volume:** clamped to 0 at intake (counted); Σ SATURATES at
  `i64::MAX` in place (M3 — the fold stays atomic, never a torn bucket; counted +
  one coalesced warn per (feed, sid, day); index spot volume is legitimately 0 anyway).
- **Groww i64 ids:** `spot_1m_rest.security_id` is i64; negative values cannot map to
  `BufferedSeal.security_id: u64` → skipped + counted (defensive; today's ids are the
  4 IDX_I SIDs / stable Groww index ids, all positive).
- **Segment strings from discovery:** re-validated against the exact
  `segment_str_to_code` allowlist before any follow-up query interpolation (the
  tf_consistency L8 second-order-injection defense).
- **catchup_days = 0 / >370:** rejected at boot by `validate()` (1..=370 — headroom
  so a retention widening never needs a validation-bound edit).
- **Fold disabled:** no sender installed; both legs' handoff is a no-op; behaviour
  byte-identical to today (candles stay producer-dead — the rollback state).

## Failure Modes

- **QuestDB unreachable at boot catch-up:** quiet bounded readiness probe fails →
  FOLD-01 `stage="catchup_query"` per feed, catch-up skipped, LIVE folding continues
  (bars arrive over the mpsc); the next boot repairs via DEDUP-idempotent catch-up.
- **HTTP client build fails:** FOLD-01 `stage="client_build"` (HTTP-CLIENT-01 class);
  catch-up + refold legs degrade, live folding continues.
- **/exec query fails / malformed body / oversize (streamed cap):** that (feed, sid)
  is skipped with FOLD-01 `stage="catchup_query"`/`"catchup_parse"` — never a partial
  silent fold. The 8 MiB response cap is enforced chunk-by-chunk during the streamed
  read (M1 — a chunked-transfer body without Content-Length can never buffer
  unbounded).
- **Per-SID day query hits its LIMIT:** FOLD-01 `stage="catchup_parse"` — the WHOLE
  (feed, sid, day) fold is SKIPPED loudly (a truncated day is never partially folded
  — the tf_consistency tripwire discipline).
- **Discovery hits its LIMIT:** FOLD-01 `stage="discovery_truncated"` — the whole
  (feed, day) pass is skipped loudly (M4 — a partial instrument set is never trusted).
- **Seal channel full (seal writer behind):** the BOOT catch-up path PACES — newest
  days first, sleep-and-retry the SAME seal (100ms steps) up to a bounded per-day
  budget (60s) before counting a drop (`tv_rest_candle_fold_paced_waits_total`;
  HIGH-1). A drop past the budget is counted
  (`tv_rest_candle_fold_dropped_total{reason="seal_channel_full"}`) + logged honestly
  as underived until a later refold or the NEXT boot's catch-up (never a same-session
  "re-derives them" claim). The LIVE path stays non-blocking try_send.
- **Fold-bar mpsc full (fold task wedged):** legs drop the handoff with
  `tv_rest_candle_fold_dropped_total{reason="channel_full"}` (`channel_closed` when
  the task is gone) — the spot legs are NEVER blocked (their persist path is
  untouched); the boot catch-up repairs.
- **Current-day repair (round-2 HIGH; round-3 burst coalescing):** refolds from the
  in-RAM day-map — no `/exec` read, no WAL race on the repair path; the consumer loop
  drains each burst as ONE batch and refolds every dirty slot ONCE per batch (a
  mid-day-outage sweep of N repairs costs one refold per slot, never N×~450 seals);
  the swapped engine equals a fresh in-order fold over the updated bar set
  (unit-pinned against the recompute-golden-tested fold). Lossless claim scoped to
  bars received in-process this incarnation.
- **Crash-restart WAL residual on the TODAY seed (round-3 doc-honesty):** the boot
  catch-up's TODAY seed IS an `/exec` read — a minute persisted seconds before a
  crash-restart can be WAL-invisible to that seed and stays out of the derived candles
  until the NEXT boot's catch-up; the 15:40 IST tf-verify (Blind/mismatch) is the
  pager for that window.
- **Future-dated bar (round-3 — the BOUNDARY-01 future-skew class):** a bar whose IST
  date is after the wall-clock IST today NEVER rolls the live day forward — dropped +
  counted (`dropped{reason="future_dated"}`) + one coalesced coded error per drained
  batch. With no current day yet (failed/empty catch-up), only a TODAY-dated first bar
  is adopted; a past-dated first bar routes to the past-day `/exec` queue (prevents a
  1-bar refold force-sealing + DEDUP-clobbering a previously-correct closed day).
- **Seal channel CLOSED (shutdown/teardown):** labeled distinctly
  (`dropped{reason="seal_channel_closed"}`) on BOTH the live and paced paths — never
  conflated with backpressure (round-3).
- **PAST-day refold reads a stale /exec view (WAL apply lag — the only residual
  WAL-lag surface):** the refold verifies the triggering repair minute is present
  BEFORE emitting (M2); a stale read re-queues the dirty mark (bounded 5 attempts,
  counter per failed attempt) and exhaustion degrades loudly
  (`stage="refold_stale_read"`); the next boot's catch-up re-derives. Past-day refolds
  emit closed-day seals only and never touch a live engine.
- **No HTTP client at drain time (round-2 LOW-4):** queued past-day marks are dropped
  LOUDLY (`dropped{reason="no_http_client"}` + coded error) — the client is built once
  per incarnation and can never appear later; the next boot's catch-up re-derives.
- **Fold task dies (unwind builds):** supervised respawn (5s backoff) + FOLD-01
  `stage="task_respawn"` + counter; release panics abort the process (panic="abort" —
  the TICK-FLUSH-01 honesty note); recovery = restart + boot catch-up.
- **Retention first-sweep burst (round-2 MEDIUM, dated honest note 2026-07-16):** the
  chain 90→35d class move makes up to ~110 partitions (~4 GB) drop-eligible in the
  FIRST post-merge sweep. No code change: `max_partitions_per_run` (200) caps the
  worklist; partitions process strictly serially (one temp-disk gzip at a time —
  minutes-class wall clock, off-hours); the leg is fail-closed (no verified S3 copy ⇒
  no drop — a degraded S3 leg means "nothing freed yet", never loss). Twin notes:
  `partition_archive.rs::CHAIN_MARKET_DATA_TABLES` + the fold rule file §1.
- **Retention edit risk:** raising market_data_hot_days 14→35 only SLOWS drops
  (fail-safe direction); moving the chain tables to 35d TIGHTENS their window — the
  archive→verify→drop leg is fail-closed (no verified S3 copy ⇒ no drop), and the
  MIN_HOT_DAYS=2 floor is untouched.

## Test Plan

- **Golden fold-vs-recompute test:** synthetic trading day of 1m bars → fold engine
  seals must agree EXACTLY (o/h/l/c/vol) with `tf_consistency_boot::recompute_window`
  over `bucket_grid` windows for every TF 1m..4h + the 1d session fold
  (`test_golden_fold_agrees_with_tf_consistency_recompute`).
- **Bucket boundary tests:** 09:15 open anchoring, 15:30 close truncation, final
  partials (M30/H1/H4), D1 seals at close, M1 seals every bar, gap (missing minute)
  transition-seal, day-boundary transition (`test_fold_*` family).
- **Order tolerance:** out-of-order bar → `FoldOutcome::OutOfOrder` (never folded
  directly); at the `SidDayFold` layer (round-2): current-day repair → day-map refold
  (`test_apply_live_bar_value_update_repair_refolds_to_recompute`,
  `test_apply_live_bar_late_missing_minute_refolds_losslessly`), value-identical
  duplicate → no-op (`test_apply_live_bar_duplicate_bar_is_noop`), day roll clears the
  map + seals residue (`test_apply_live_bar_day_roll_clears_day_map_and_seals_residue`),
  past-day bar routes to the /exec queue without touching live state
  (`test_apply_live_bar_past_day_routes_to_exec_queue_not_live_state`), mid-session
  restart seeding (`test_apply_live_bar_after_from_catchup_seeding_refolds_losslessly`,
  `test_refold_from_day_map_matches_direct_fold`); out-of-session bar → OutOfSession.
- **Round-3 hardening:** repair-burst coalescing — 40 out-of-order repairs in one
  drained batch = exactly ONE refold's worth of re-emitted seals per slot, value-exact
  vs a fresh fold (`test_apply_bar_batch_repair_burst_coalesces_into_one_refold_per_slot`,
  `test_apply_live_bar_deferred_and_refold_current_day`); future-date clamp — never
  rolls the day, real bars keep working (`test_apply_live_bar_future_dated_never_rolls_day`);
  None-current-day adoption arms — today adopts, past routes to the /exec queue,
  future drops (`test_first_bar_none_day_adopts_today_only`); burst drain primitive
  (`test_receiver_guard_try_recv_drains_burst`).
- **Volume semantics:** i64 Σ, negative clamp, u64 conversion clamp
  (`test_fold_volume_i64_clamp_semantics`).
- **Idempotency:** folding the same input twice through fresh engines yields identical
  seal sets (`test_fold_refold_same_input_identical_seals`).
- **Config:** serde default OFF; empty-TOML deserialize disabled; base.toml sets
  enabled=true + catchup_days=35; validate rejects 0 / 371
  (`test_rest_candle_fold_config_*`).
- **Catch-up SQL/parse:** query shapes (micros window, LIMIT, feed/segment scoping),
  dataset parse skip-malformed + truncation flag.
- **Retention:** `retention_class` maps option_chain_1m + option_contract_1m_rest +
  every candle table + ticks to MarketData; default 35 pinned; base.toml pinned.
- **Wiring ratchet:** `crates/app/tests/rest_candle_fold_wiring_guard.rs` pins the
  main.rs shared-infra spawn (config-gated, REST-only-boot reachable), the
  `set_global_fold_bar_sender` install, BOTH spot legs' `send_confirmed_bars(` handoff
  sites (flush-gated), and the boot catch-up call inside the fold module's production
  region.
- **Error-code chain:** crossref both directions green (`FOLD-` added to
  TRACKED_PREFIXES), tag guard, triage full-coverage guard (new escalate rule),
  paging drift guard untouched (log-sink-only — no tf entry).
- **Gates:** cargo fmt, clippy -D warnings -W clippy::perf, `cargo test --workspace`
  (crates/common touched → escalation per testing-scope.md), banned-pattern scanner,
  pub-fn guards, plan-verify.

## Rollback

- **Config rollback (no code change):** `[rest_candle_fold] enabled = false` (or delete
  the section — serde default OFF) ⇒ no sender installed, no catch-up, both legs'
  handoff no-ops; behaviour byte-identical to pre-PR (candles_* stay unwritten).
- **Retention rollback:** set `market_data_hot_days = 14` in base.toml to restore the
  old window (config-only); the chain-table class move reverts with a 2-line
  `retention_class` edit. Nothing already dropped is lost — every drop was preceded by
  a verified S3 copy (`questdb-partitions/<table>/<partition>.csv.gz`).
- **Full revert:** the PR is additive (new module + hook sites + config); `git revert`
  of the squash commit restores main exactly; folded candle rows remain in QuestDB as
  inert data (DEDUP-keyed, feed-tagged) and age out under the retention window.

## Observability

- **Counters:** `tv_rest_candle_fold_seals_total{feed}` (one per emitted BufferedSeal),
  `tv_rest_candle_fold_catchup_rows_total{feed}` (bars folded by the boot catch-up),
  `tv_rest_candle_fold_errors_total{stage}` (catchup_query / catchup_parse /
  discovery_truncated / refold_stale_read / seal_send / volume_saturated /
  receiver_lost / sender_install / task_respawn), `tv_rest_candle_fold_dropped_total
  {reason}` (channel_full / channel_closed / seal_channel_full / no_seal_sender /
  bad_identity / out_of_session), `tv_rest_candle_fold_paced_waits_total`,
  `tv_rest_candle_fold_volume_saturated_total`, `tv_rest_candle_fold_refold_queued_total`,
  `tv_rest_candle_fold_heartbeat_total` (dense per-interval liveness),
  `tv_rest_candle_fold_task_respawn_total{reason}` — all static bounded labels.
- **Coded logs:** every degrade is `error!(code = ErrorCode::RestCandleFold01Degraded
  .code_str(), stage = ...)` (tag-guard compliant); one coalesced boot-catch-up summary
  `info!` per feed (bars folded, seals emitted, sids); one `info!` per dirty-day refold.
- **Delivery boundary (honest):** FOLD-01 is log-sink-only — NO `error_code_alerts`
  terraform entry, NO observability-architecture paging-list entry (the paging drift
  guard sees no drift). The operator-facing verification signal is the EXISTING 15:40
  IST tf-consistency verifier (which recomputes candles_* from candles_1m and pages
  TF-VERIFY-01 on divergence) + the `candles_*` row counts via
  `mcp__tickvault-logs__questdb_sql`. A CloudWatch filter is a flagged follow-up.
- **Runbook:** `.claude/rules/project/rest-candle-fold-error-codes.md` (new, rides this
  PR) + a dated rule-10 edit in `live-feed-purity.md` + triage rule in
  `.claude/triage/error-rules.yaml`.

## Plan Items

- [x] Item 0 — This plan file (Status APPROVED, operator quotes recorded)
  - Files: .claude/plans/active-plan-rest-candle-derivation.md
  - Tests: (docs only)
- [x] Item 1 — Fold module: pure core + global sender + catch-up + refold + supervised task
  - Files: crates/app/src/rest_candle_fold.rs, crates/app/src/lib.rs
  - Tests: test_golden_fold_agrees_with_tf_consistency_recompute, test_single_bar_folds_into_all_21_tfs_and_m1_seals, test_final_session_minute_seals_everything_including_d1, test_out_of_order_bar_is_refused_not_folded, test_out_of_session_bar_is_skipped, test_refold_same_input_produces_identical_seals, test_volume_saturates_never_tears_the_fold, test_spot_bars_sql_shape, test_parse_spot_bars_and_truncation_tripwire, test_parse_spot_bars_float_volume_fallback (truth-synced 2026-07-16 fix round — the pre-impl placeholder names are retired)
- [x] Item 2 — Config: RestCandleFoldConfig (serde default OFF) + validate + base.toml opt-in
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_rest_candle_fold_config_default_off, test_rest_candle_fold_config_validate_bounds
- [x] Item 3 — Spot-leg handoffs (flush-ACK-gated) in both feeds' fire + sweep paths
  - Files: crates/app/src/spot_1m_rest_boot.rs, crates/app/src/groww_spot_1m_boot.rs
  - Tests: rest_candle_fold_wiring_guard (handoff pins)
- [x] Item 4 — main.rs shared-infra spawn (config-gated, REST-only-boot reachable)
  - Files: crates/app/src/main.rs
  - Tests: rest_candle_fold_wiring_guard (spawn pin)
- [x] Item 5 — ErrorCode FOLD-01 + rule file + triage rule + crossref prefix
  - Files: crates/common/src/error_code.rs, .claude/rules/project/rest-candle-fold-error-codes.md, .claude/triage/error-rules.yaml, crates/common/tests/error_code_rule_file_crossref.rs
  - Tests: error_code_rule_file_crossref, triage_rules_full_coverage_guard, test_code_str_follows_expected_prefix_pattern
- [x] Item 6 — Retention: market_data_hot_days 14→35 + chain tables into the 35d class + dated notes
  - Files: crates/storage/src/partition_archive.rs, crates/common/src/config.rs, config/base.toml
  - Tests: test_retention_class_chain_tables_are_market_data, config default/parse pins
- [x] Item 7 — live-feed-purity rule 10 dated edit (REST-era bar-fold 1d permission)
  - Files: .claude/rules/project/live-feed-purity.md
  - Tests: (docs; scanner-clean)
- [x] Item 8 — Wiring ratchet test
  - Files: crates/app/tests/rest_candle_fold_wiring_guard.rs
  - Tests: the guard itself (4+ pins)
- [x] Item 9 — Hostile-review round-1 fixes (HIGH-1 paced newest-first catch-up,
  HIGH-2 RAII receiver re-park guard, HIGH-3 dead-fold Blind classification +
  heartbeat, M1 streamed body cap, M2 stale-read refold gate, M3 saturating volume,
  M4 discovery truncation tripwire, M5 call-site guard anchor, M7 real golden
  cross-implementation test)
  - Files: crates/app/src/rest_candle_fold.rs, crates/app/src/tf_consistency_boot.rs, crates/app/src/main.rs, crates/app/tests/rest_candle_fold_wiring_guard.rs, .claude/rules/project/rest-candle-fold-error-codes.md, .claude/rules/project/tf-consistency-error-codes.md
  - Tests: test_catchup_pace_action_budget_boundaries, test_catchup_day_offsets_newest_first, test_receiver_guard_take_reparks_on_panic_and_respawn_resumes, test_empty_candles_with_spot_rows_is_blind_vs_nodata, test_select_spot_1m_count_sql_and_parse_count_dataset, test_accumulate_capped_streamed_body_cap, test_should_requeue_refold_bounds, test_rows_contain_minute_stale_read_gate, test_volume_saturates_never_tears_the_fold, test_parse_spot_discovery_segment_allowlist_and_truncation, test_golden_fold_agrees_with_tf_consistency_recompute

- [x] Item 10 — Hostile-review round-2 fixes (HIGH: lossless current-day day-map
  refold replaces the /exec today-refold — value-updates + race-window bars can no
  longer be lost to WAL-apply lag; MEDIUM: past/today decided by the ENGINE's day, so
  a post-midnight repair for the open session day never clobbers via stale past-day
  seals; MEDIUM: retention first-sweep burst dated note (docs-only — bounded by
  max_partitions_per_run + serial archive-verify-before-drop, fail-closed);
  LOW-1 Closed-arm full-slice drop count; LOW-2 module-doc spot_1m_rest retention
  class correction; LOW-3 catchup_day_offsets exactly-catchup_days semantics;
  LOW-4 loud no-client past-day-mark drop; LOW-5 float-volume parse fallback;
  LOW-6 Groww handoff bad-identity counter + coalesced warn; LOW-7 rule-file
  wording alignment (per-attempt counter, per-(feed,SID,day) pace budget))
  - Files: crates/app/src/rest_candle_fold.rs, crates/app/src/groww_spot_1m_boot.rs,
    crates/common/src/config.rs, crates/storage/src/partition_archive.rs,
    .claude/rules/project/rest-candle-fold-error-codes.md,
    .claude/plans/active-plan-rest-candle-derivation.md
  - Tests: test_apply_live_bar_value_update_repair_refolds_to_recompute,
    test_apply_live_bar_late_missing_minute_refolds_losslessly,
    test_apply_live_bar_duplicate_bar_is_noop,
    test_apply_live_bar_day_roll_clears_day_map_and_seals_residue,
    test_apply_live_bar_past_day_routes_to_exec_queue_not_live_state,
    test_apply_live_bar_after_from_catchup_seeding_refolds_losslessly,
    test_refold_from_day_map_matches_direct_fold,
    test_note_unfoldable_identity_counts_and_never_panics,
    test_parse_spot_bars_float_volume_fallback,
    test_catchup_day_offsets_newest_first

- [x] Item 11 — Post-verify fix round (2026-07-16): the two Item-2 config pin
  tests land for real (they were promised names, now real code); same-batch
  day-roll displaced-repair routing — a batch carrying [old-day repair, new-day
  first bar] parks the just-deferred repair at the roll and routes its OLD day
  into the past-day /exec dirty queue instead of silently dropping it with the
  cleared map; heartbeat catch-up-window liveness note in the rule file's
  honest envelope (the heartbeat is created AFTER boot catch-up — catch-up
  silence is not death)
  - Files: crates/common/src/config.rs, crates/app/src/rest_candle_fold.rs,
    .claude/rules/project/rest-candle-fold-error-codes.md,
    .claude/plans/active-plan-rest-candle-derivation.md
  - Tests: test_rest_candle_fold_config_default_off,
    test_rest_candle_fold_config_validate_bounds,
    test_same_batch_day_roll_take_displaced_repair_routes_to_past_day_queue

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot at 08:30 with 20 days of spot_1m_rest history | catch-up folds both feeds' bars, candles_1m..1d populate for every past day, DEDUP-idempotent |
| 2 | Live minute confirms at 09:16:01 | M1 seal emitted within the fire; higher TFs seal at their bucket ends |
| 3 | 15:30:00 fire (15:29 bar) | every open bucket incl. final partials + 1d seals |
| 4 | Sweep repairs 10:20 at 15:33 | current-day day-map refold (RAM, synchronous) re-emits the day's closed buckets; live engine swapped with the healed state — no /exec, no WAL race (round-2) |
| 5 | QuestDB down at boot | catch-up degrades loudly (FOLD-01), live folding continues |
| 6 | Fold disabled | byte-identical to today; zero sends, zero spawns |
| 7 | Re-run catch-up twice | identical rows (DEDUP UPSERT), no duplicates |
| 8 | Chain partitions age past 35d | archived→verified→dropped (was 90d); disk fits the 30 GB constraint |

## Per-Item Guarantee Matrix (15-row + 7-row, per `.claude/rules/project/per-wave-guarantee-matrix.md`)

### 15-row 100% Guarantee Matrix

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | fold core + config + retention fully unit-tested; `quality/crate-coverage-thresholds.toml` ratcheted floors | post-merge llvm-cov | tests listed per item above |
| 100% audit coverage | output lands in the DEDUP-keyed `candles_*` tables (ts, security_id, segment, feed); every degrade coded FOLD-01 in errors.jsonl | `mcp__tickvault-logs__questdb_sql` | seal chain reused unchanged |
| 100% testing coverage | 22-category standard scoped to app/common/storage/trading; golden + boundary + property-style idempotency tests | `cargo test --workspace` green | Test Plan section |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + pre-commit gates | pre-push mandatory | all gates run before PR |
| 100% code performance | cold path (≤8 bars/min live; boot catch-up bounded SELECTs) — no hot-path involvement; zero per-tick cost | n/a (not hot path) | N/A — cold/per-minute path (no DHAT/Criterion per spec) |
| 100% monitoring | tv_rest_candle_fold_* counters + coded logs + the existing seal-chain counters + tf-consistency verifier downstream | `mcp__tickvault-logs__run_doctor` | Observability section |
| 100% logging | every error path `error!` with `code = FOLD-01` (tag-guard); coalesced info summaries | errors.jsonl hourly | tag-guard green |
| 100% alerting | log-sink-only by design (honest delivery boundary — no false claim of paging); downstream TF-VERIFY-01 pages on fold divergence | paging drift guard green | documented in rule file §delivery |
| 100% security | no secrets touched (QuestDB /exec local reads only); segment allowlist re-check blocks second-order injection; no URL/token logging | `cargo audit` | catch-up SQL scoping tests |
| 100% security hardening | bounded LIMIT + 8 MiB response cap + timeout + redirect-none client | post-deploy | constants pinned in module |
| 100% bugs fixing | adversarial review by the parent session (hostile review before PR opens per task contract) | pre-PR | parent-session review |
| 100% scenarios covering | Scenarios table (8) + Edge Cases section each mapped to a test or documented residual | scoped tests | Test Plan |
| 100% functionalities covering | every new pub fn has a call site + test (pub-fn guards) | pre-push gates 6+11 | guards green |
| 100% code review | parent session runs the hostile review pass on the diff before opening the draft PR | per-PR | task contract |
| 100% extreme check | rest_candle_fold_wiring_guard + retention-class tests + config-default pins fail the build on regression | every commit | Item 8 |

### 7-row Resilience Demand Matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | NO tick path touched — bars fold only AFTER the spot legs' flush ACK; a dropped fold bar is repaired by boot catch-up (DEDUP-idempotent), never data loss | handoff sits strictly post-flush (wiring guard) |
| WS never disconnects | no WebSocket involvement (REST-only runtime); no reconnect machinery touched | n/a |
| Never slow/locked/hanged | cold path: ≤8 bars/min live, bounded boot SELECTs with LIMIT + caps + timeouts; legs use try_send (never block) | constants + wiring guard |
| QuestDB never fails | ABSORB: seals ride the existing ring→spill→DLQ chain; catch-up degrades loudly and re-runs next boot | Failure Modes section |
| O(1) latency | O(21) per folded bar (fixed TF array), O(1) engine lookup per (feed,sid,segment); the boot catch-up is honestly O(days×minutes) — flagged O(N), cold, bounded by catchup_days | fold core structure |
| Uniqueness + dedup | composite (security_id, exchange_segment) + feed in the candles DEDUP key — reused unchanged; refold/catch-up re-emits UPSERT in place | DEDUP key tests already pin it |
| Real-time proof | seals visible in candles_* within one seal-writer drain cycle (~100ms); counters per seal; the 15:40 tf-consistency verifier is the daily exact-match proof | Observability section |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the fold is EXACT
for O/H/L/C/ΣV given confirmed 1m REST bars (golden-tested against the tripwire-tested
`recompute_window`); persistence rides the existing ring→spill→DLQ seal chain with
feed-in-key DEDUP idempotency; the boot catch-up re-derives all 21 TFs from whatever
`spot_1m_rest` history exists (bounded by `catchup_days`). NOT claimed: intra-minute
fidelity (state granularity is honestly 1 minute — `tick_count` written 0, never faked);
minutes the vendors never served (absent bars stay absent — the SPOT1M named-gap
machinery owns that visibility); pct-from-prev-day columns (written 0.0 fail-soft).
