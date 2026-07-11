# Implementation Plan: Dual-Feed Scoreboard PR-C — Groww lag ring + per-feed day lag distributions + dashboard

**Status:** VERIFIED
**Date:** 2026-07-11
**Approved by:** Parthiban (operator) — dual-feed scoreboard directive 2026-07-10 (verbatim: *"run these two websockets live for a month... all tracked, captured, visualized, logged, monitored, 100% automated"*), PR-3 slice of the synthesized scoreboard design (judge-approved 2026-07-10; PR-A #1473 + PR-B #1475 merged). Task delegated by the coordinator session 2026-07-11.

> **Guarantee matrices:** this plan carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (the canonical copy), instantiated below in Design/Test Plan/Observability.
> Honest 100% claim (charter §F wording): 100% inside the tested envelope,
> with ratcheted regression coverage — zero-alloc O(1) lag folds
> (DHAT-ratcheted `dhat_feed_lag_groww.rs` + Criterion budget
> `feed_lag_record_groww_tick` ≤100ns), replay/re-tail exclusion at record
> time (counted, never silent), ≥50-sample + in-session publish gates
> (never a fabricated 0), day histograms process-local + day-scoped with
> the partial/restart honesty stamps. Beyond the envelope: backfill days
> read −1 sentinels, never invented distributions.

## Design

Third slice (PR-3/PR-C) of the dual-feed scoreboard: make exchange→receipt
lag MEASURED for both feeds, per day, per the design contract §3.

1. **Groww lag ring** (`crates/core/src/pipeline/feed_lag_monitor.rs`): a
   second `FeedLagRing` OnceLock (`GROWW_LAG_RING`) + pure
   `classify_groww_sample(capture_ist, wake_receipt_ist, exchange_ts_ist)`
   → Admitted{recv_utc, lag_ns, clamped} / ExcludedNoCapture /
   ExcludedStaleCapture. `record_groww_tick` folds admitted samples into
   the ring + the Groww day histogram; exclusions increment
   `tv_groww_lag_samples_excluded_total{reason}`. Millisecond precision
   preserved end-to-end (IST-nanos math, never floored to seconds).
   Receipt clock = the sidecar's capture-at-receipt `capture_ns` (one hop
   downstream of the socket — semantics caveat on every surface).
2. **Producer hook** (`crates/app/src/groww_bridge.rs::drain_new_data`):
   one call at the validated drain site using operands the drain already
   computes (`capture_stamp_ist_nanos` output + `wake_receipt_ist_nanos` +
   `parsed.tick.ts_ist_nanos`) — ZERO new clock reads, zero allocation.
3. **Supervised Groww publisher** (`run_groww_lag_publisher` +
   `spawn_supervised_groww_lag_publisher` in main.rs, single
   process-global-prefix spawn): emits `tv_groww_exchange_lag_p99_seconds`
   (its OWN gauge name — an EMF `feed` label would fold the Dhan/Groww
   series), 10s cadence, in-session + ≥50-sample gates, own respawn
   counter `tv_groww_lag_publisher_respawn_total{reason}`.
4. **Per-feed DAY histograms** (`DailyLagHistogram`, 96 quarter-octave
   log2 buckets 1ms…1h, fixed atomic arrays, const-init statics): fed
   inside BOTH `record_*_tick` admitted arms; reset at IST midnight from
   BOTH existing force-seal tasks; drained (non-destructively) by the
   15:45 scoreboard into `feed_scoreboard_daily.lag_*` (same-day runs
   only — backfills keep −1). Telegram converter renders measured p50/p99
   with the Dhan ≥1s-floor + Groww ms/sidecar-capture footnote.
5. **CloudWatch:** `tv_groww_exchange_lag_p99_seconds` joins the EMF
   allowlist (27th name, both agent configs) + alarm
   `groww_exchange_lag_p99_high` (5s ×10min, window-gated — the 12th gated
   alarm) + dashboard slot 2 (`tv-<env>-scoreboard`, Dhan-vs-Groww lag
   row). Cost +~$0.40/mo, dated notes in silent-feed-alarms.tf +
   aws-budget.md.

## Plan Items

- [x] Groww lag classifier + ring + `record_groww_tick` + `record_dhan_tick` day-hist fold
  - Files: crates/core/src/pipeline/feed_lag_monitor.rs
  - Tests: test_classify_groww_sample_no_capture_is_excluded, test_classify_groww_sample_stale_capture_boundary_excludes_at_exactly_60s, test_classify_groww_sample_preserves_sub_second_millisecond_lag, test_classify_groww_sample_negative_lag_clamped, test_record_groww_tick_smoke_on_global_ring
- [x] DailyLagHistogram + day_lag_summary + reset_day_lag_histogram
  - Files: crates/core/src/pipeline/feed_lag_monitor.rs
  - Tests: test_lag_hist_bucket_index_and_bounds_roundtrip, test_day_histogram_percentiles_on_known_distribution, test_day_histogram_thin_day_returns_none_and_reset_clears, test_reset_day_lag_histogram_and_day_lag_summary_route_to_the_right_feed
- [x] Groww drain hook + IST-midnight resets (both feeds)
  - Files: crates/app/src/groww_bridge.rs, crates/app/src/main.rs
  - Tests: test_record_groww_tick_producer_site_wired_into_drain
- [x] Supervised Groww publisher + wiring ratchet
  - Files: crates/app/src/main.rs, crates/core/src/auth/secret_manager.rs, crates/core/src/pipeline/feed_lag_monitor.rs
  - Tests: test_groww_lag_publisher_supervisor_is_wired_into_main
- [x] Scoreboard lag columns flip from −1 to measured + converter + footnotes
  - Files: crates/app/src/feed_scoreboard_boot.rs, crates/app/src/main.rs, crates/core/src/notification/events.rs
  - Tests: test_dual_feed_scorecard_body_sentinels_and_footnotes, test_dual_feed_scorecard_body_verdict_ladder
- [x] DHAT + Criterion + budget for the Groww fold
  - Files: crates/core/tests/dhat_feed_lag_groww.rs, crates/core/benches/feed_lag_ring.rs, quality/benchmark-budgets.toml
  - Tests: dhat_record_groww_tick_hot_path_zero_allocation
- [x] CloudWatch: EMF allowlist ×2 + alarm + window-gate join + dashboard slot 2 + ratchets
  - Files: deploy/aws/terraform/user-data.sh.tftpl, deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/silent-feed-alarms.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, deploy/aws/terraform/dashboard.tf, crates/common/tests/cloudwatch_app_alarms_wiring.rs
  - Tests: test_emf_metric_selectors_name_count_is_twenty_seven, test_groww_exchange_lag_alarm_shape_is_pinned, test_silent_feed_alarms_are_window_gated, test_app_alarms_count_is_twenty_three
- [x] Docs: runbook + rule files + metrics catalog + aws-budget dated notes; archive the merged PR-B plan
  - Files: docs/runbooks/dual-feed-scoreboard.md, .claude/rules/project/dual-feed-scoreboard-error-codes.md, .claude/rules/project/daily-universe-scope-expansion-2026-05-27.md, .claude/rules/project/aws-budget.md, crates/app/src/metrics_catalog.rs, .claude/plans/archive/2026-07-10-scoreboard-stalls.md
  - Tests: (docs — covered by the ratchets above)

### Fix round 1 (2026-07-11 — hostile review, 5 confirmed + trivial LOWs)

- [x] HIGH+MEDIUM (same root): lag keep-better — a same-day post-close
  RunCatchUp rerun / past-day backfill with empty day histograms must fold
  the day's EXISTING measured lag columns forward instead of UPSERTing −1
  over them (step-5a read extended to the lag columns; step-6c fold;
  `stage="lag_regression"` on suppression); runbook + rule-file claims
  corrected.
  - Files: crates/app/src/feed_scoreboard_boot.rs, docs/runbooks/dual-feed-scoreboard.md, .claude/rules/project/dual-feed-scoreboard-error-codes.md
  - Tests: test_fold_existing_lag_keep_better_preserves_measured_on_unmeasured_rerun, test_parse_existing_daily_lag_reads_columns_2_through_5, test_build_existing_daily_outcome_sql_micros_window
- [x] MEDIUM: Groww stale_capture exclusion is now the TWO-condition
  discriminator (bridge episode-level `replay_window` flag AND ≥60s dwell)
  — a live backlog drain (ILP-backpressure pause / respawn-backoff wake,
  preserved offset) is ADMITTED to the day histogram
  (`AdmittedBacklog`, ring skipped, `tv_groww_lag_backlog_admitted_total`),
  mirroring the Dhan 2026-07-07 round-2 fix.
  - Files: crates/core/src/pipeline/feed_lag_monitor.rs, crates/app/src/groww_bridge.rs, crates/core/tests/dhat_feed_lag_groww.rs, crates/core/benches/feed_lag_ring.rs
  - Tests: test_classify_groww_sample_live_backlog_without_retail_is_admitted_day_only, test_replay_window_lifecycle_clears_on_catchup_and_rearms_on_shrink, test_classify_groww_sample_stale_capture_boundary_excludes_at_exactly_60s
- [x] MEDIUM: rung-2 verdict clock-floor guard — a lag winner is declared
  only when |dhan_p99 − groww_p99| > 1000 ms (the Dhan whole-second
  floor); wording "faster prices beyond the clock floor".
  - Files: crates/core/src/notification/events.rs
  - Tests: test_dual_feed_scorecard_body_verdict_ladder
- [x] MEDIUM (+2 dup LOWs): the "Delay could not be measured today"
  footnote keys on EITHER feed — a mixed-state day (one feed measured)
  never renders the unmeasured claim over a measured value.
  - Files: crates/core/src/notification/events.rs
  - Tests: test_dual_feed_scorecard_mixed_lag_state_footnote_keys_on_either_feed
- [x] LOW: `FeedLagRing::push_sample` head bump is a relaxed `fetch_add`
  RMW (multi-writer under the dormant §34 shard fleet degrades to sampling
  noise, never a broken single-writer invariant); ring docs updated.
  - Files: crates/core/src/pipeline/feed_lag_monitor.rs
  - Tests: test_ring_wraparound (existing — semantics preserved)
- [x] LOW ×2: comment-count lockstep — boot-heartbeat-alarm.tf gated-alarm
  rationale 11→12 (10 + 2 split); app-alarms.tf cost arithmetic 28→29
  series / $8.40→$8.70 / $7.20→$7.50.
  - Files: deploy/aws/terraform/boot-heartbeat-alarm.tf, deploy/aws/terraform/app-alarms.tf
  - Tests: (comment-only; aws_alarm_semantics_guard + cloudwatch_app_alarms_wiring stay green)

## Edge Cases

- **Line without `capture_ns`** (old-format / reconcile-sweep): no trusted
  receipt instant → EXCLUDED + counted (`reason="no_capture"`), never
  fabricated from the wake clock.
- **Byte-0 re-tail backlog** (file shrank / offset reset): capture ≥60s
  older than the wake → EXCLUDED + counted (`reason="stale_capture"`);
  strict `<` on the admit side (exactly 60.000000000s excluded — test-pinned).
  Honest residual: a re-tail of the last <60s re-records recent samples
  once — bounded duplicate in a p99 sample estimate.
- **Negative lag** (host-vs-Groww clock skew): clamped to 0 + counted,
  never a panic, never a negative sample.
- **Thin day** (<50 samples) / **Groww disabled**: histogram summary =
  None → −1 sentinels; publisher publishes NOTHING (empty ring) — never a
  false "perfect lag".
- **Past-day backfill**: histograms are process-local + day-scoped → lag
  columns stay −1 (the 6b gate is `is_same_day_run`).
- **Mid-day restart**: only the post-restart window exists; the existing
  restart-day partial floor stamps the row partial + the card carries the
  restart footnote — measured-but-partial, never fabricated.
- **Weekend midnight**: resets fire at EVERY IST midnight (before the
  trading-day/enabled gates), so Friday's distribution can never bleed
  into Monday.
- **Off-session samples**: both day histograms can hold a few off-session
  samples (§30 always-on SIDs on Dhan; off-session sidecar lines on
  Groww) — documented envelope, dominated by the ~375-min session volume.
- **Sub-second precision**: Groww lag math stays in IST nanos (ms-precision
  exchange stamps); bucket estimates are quarter-octave (±~9%), clamped to
  the true recorded max.

## Failure Modes

- **Publisher task dies** → supervisor respawns (5s backoff), `error!` +
  `tv_groww_lag_publisher_respawn_total{reason}` — the gauge stream can
  never vanish silently (SLO-03 class closed for the second feed).
- **Producer site dropped by refactor** → source-scan ratchet
  `test_record_groww_tick_producer_site_wired_into_drain` fails the build
  (the 2026-07-06 dark-gauge class, producer half).
- **Publisher spawn dropped** → `test_groww_lag_publisher_supervisor_is_wired_into_main`
  fails the build.
- **EMF allowlist / alarm / gate drift** → the 27-name count pin, the
  reference-vs-deployed drift guard, the alarm-shape pin, the
  window-gate membership pin, and the alarm-metric superset guard each
  fail the build.
- **Midnight reset dropped** → the reset-site scan in the producer ratchet
  fails the build.
- **Histogram race during drain**: relaxed snapshot may shift a percentile
  by ~1 sample out of ≥50 — documented, statistically negligible.

## Test Plan

- Unit: classifier (4 tests incl. boundary + clamp), bucket math
  roundtrip + monotonicity, percentile estimates on known distributions,
  thin-day gate, reset, feed routing (all in feed_lag_monitor.rs).
- Source-scan ratchets: producer site + midnight reset (groww_bridge),
  publisher wiring (secret_manager), EMF/alarm/gate/dashboard shape pins
  (cloudwatch_app_alarms_wiring).
- DHAT: `dhat_feed_lag_groww.rs` (≤1KiB/≤8 blocks across 10K admitted
  folds — zero-alloc hot path).
- Criterion: `feed_lag/record_groww_tick` with budget
  `feed_lag_record_groww_tick = 100` ns.
- Scorecard body tests: measured lag rendering (ms + s precision),
  resolution-asymmetry footnote, retired PR-1 footnote absence.
- Block-scoped suites for touched crates (common/core/app; storage
  untouched by code — trading untouched).

## Rollback

- `git revert` of the single PR-C commit restores −1 sentinels + the
  PR-1 footnote wording; no schema change was made (the lag columns
  already existed with sentinels since PR-A), so no data migration in
  either direction.
- The Groww gauge/alarm/dashboard are additive terraform — a revert
  removes them cleanly (`terraform apply` deletes the alarm + dashboard +
  allowlist entry; no other alarm references them).
- Runtime kill without revert: disabling the Groww feed empties the ring
  (publisher goes silent by the ≥50-sample gate); the scoreboard keeps
  running with measured-Dhan/-1-Groww columns.

## Observability

- Gauge `tv_groww_exchange_lag_p99_seconds` (EMF-exported, alarmed
  `tv-<env>-groww-exchange-lag-p99-high`, window-gated 09:20–15:35 IST —
  the 12th gated alarm; dashboard slot 2 row 1 next to the Dhan gauge).
- Counters (all /metrics-only, ₹0): `tv_groww_lag_samples_excluded_total{reason}`,
  `tv_groww_lag_negative_clamped_total`,
  `tv_groww_lag_publisher_respawn_total{reason}`.
- Daily durable record: `feed_scoreboard_daily.lag_p50_ms/lag_p99_ms/
  lag_max_ms/lag_samples` (+ `lag_floor_ms` honesty column) + the 3:45 PM
  Telegram delay lines with the resolution-asymmetry footnote.
- No new ErrorCode (no new `error!` emit site — the supervisor `error!`
  follows the documented Dhan-supervisor least-new-surface decision).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy Groww session | gauge ~0.1–0.5s in-session; scorecard p50/p99 in ms |
| 2 | Groww disabled all day | gauge silent; lag columns −1; card "not measured yet" |
| 3 | Re-tail replay wave | ≥60s-old lines excluded+counted; gauge/histogram unpoisoned |
| 4 | Reconcile-sweep lines | no_capture excluded+counted |
| 5 | Mid-day restart | post-restart lag measured; row partial + restart footnote |
| 6 | Past-day backfill | lag −1; blame/coverage unaffected |
| 7 | Publisher panic | respawn ≤5s + counter + error! |
| 8 | 2026-07-06-class lag storm on Groww | alarm pages at minute 10 in-window |
