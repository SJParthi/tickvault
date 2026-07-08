# Implementation Plan: Silent-Feed Alerting Hardening (2026-07-06 incident)

**Status:** VERIFIED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator directive 2026-07-06)
**Changed crates:** core (`crates/core`), app (`crates/app`), common (test ratchets in `crates/common`) — plus terraform (`deploy/aws/terraform/`) and the operator-portal Lambda (`deploy/aws/lambda/operator-control/handler.py`).

> **Incident context (2026-07-06):** the Dhan feed degraded ALL day and the
> operator received ZERO pages, despite FOUR independent signals firing
> internally: (1) exchange→receive lag p99 46s / max 199s all session;
> (2) 29–67 of 776 subscribed instruments silent EVERY minute — the
> tick-gap alarm threshold of 100 was never crossed; (3) 125 SLO score
> crossings in the 0.94–0.97 band — SLO-02 has been Telegram-suppressed
> (log-only) since 2026-05-11 and the <0.80 Critical tier is mathematically
> unreachable for feed degradation (tick_freshness alone bottoms at
> 1−67/776 ≈ 0.914); (4) BOUNDARY-01 catch-up sealing ran at 9k–11.5k
> seals/10min (coalesced warn!, no page). A fifth defect compounded the
> blindness: the operator-portal lag SQL used `now()` (UTC) against
> IST-shifted `ts`, producing a ~5h40m window that conflated WAL-replay
> rows with live rows, AND its exchange-time window structurally
> lag-censored the very lag it was meant to show (Rule-11 false-OK).

## Plan Items

- [x] Item 1 — Retune `tick_gap_instruments_silent` IN PLACE (same resource address, no state churn): threshold 100 → 40 PROVISIONAL (fires ≥41; round-3 correction 2026-07-08, review finding 4 — the first cut shipped 25, BELOW the documented ~33 always-silent healthy floor (main.rs D2 note 2026-07-03; the gauge is set from the same scan with no always-silent exclusion), which would have breached every healthy in-session minute and paged daily; 40 clears the floor with margin and aligns with the SLO-degraded alarm's ≥39-silent freshness breach point; the 29–40 marginal band overlaps the healthy floor and is owned by the SLO-degraded + lag-p99 alarms; one-trading-week soak of the in-session gauge distribution mandated), period=60, statistic=Maximum, M-of-N 10-of-12 (`evaluation_periods=12`, `datapoints_to_alarm=10`), `treat_missing_data=notBreaching`, ABSOLUTE threshold (no %-of-universe denominator exists in CW; universe envelope-locked [100,1200] so 40 = 5.2% today / 3.3% at cap; dated revisit comment). Market-hours gate: `actions_enabled=false` + append alarm name to the window-gate Lambda `ALARM_NAMES` join (gauge is set only in-session, so the last in-session value keeps being scraped 15:30→16:30). Update stale alarm_description ("> 100" → "> 40 PROVISIONAL sustained ≥10 of 12 min in-market; WS-GAP-06 runbook") + the hardcoded alarm-count/cost text in the output block. Round-3 companion fix (review finding 6): the gauge producer's zero-skip previously sat BEFORE the gauge write, so a fully-recovered silence spike froze the gauge at its last breaching value (the exporter re-serves the last set value) and would have latched 10-of-12 ~10 min AFTER complete recovery — main.rs now writes the gauge UNCONDITIONALLY every in-session scan (including 0); the zero-skip guards only the WS-GAP-06 error emission. Round-4 companion fix (final-review findings 1/2/4, 2026-07-08): the gauge producer PINS the written value to 0.0 during the NSE pre-open/auction window [09:00, 09:15) IST (`TICK_GAP_GAUGE_SESSION_OPEN_SECS_OF_DAY_IST` in main.rs, the exact mirror of the round-3 SLO tick_freshness pre-open pin) — the session gate admits [09:00, 15:30), so the 09:08–09:15 matching/buffer freeze wrote ~6–7 guaranteed breaching datapoints (boot-seeded ~775 SIDs silent, ≫ 40) into the retuned alarm's 10-of-12 / 12-min lookback; the gate Lambda's 09:20 forced-OK does NOT purge datapoints, so ~3 open-ramp minutes > 40 would have false-paged at ~09:21 on ordinary days. Genuine counts start at the 09:15:00 continuous-session open; the WS-GAP-06 error emission is unchanged. The mandated one-trading-week soak additionally checks the 09:20–09:25 transition band, not just the steady-state distribution.
  - Files: deploy/aws/terraform/app-alarms.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, crates/app/src/main.rs
  - Tests: test_tick_gap_silent_alarm_threshold_is_forty, test_tick_gap_silent_alarm_is_window_gated, test_tick_gap_silent_gauge_producer_pins_pre_open_to_zero
  - Impl: deploy/aws/terraform/app-alarms.tf `resource "aws_cloudwatch_metric_alarm" "tick_gap_instruments_silent"` (same resource address, threshold 40 PROVISIONAL, 10-of-12, actions_enabled=false) + market-hours-liveness-alarm.tf ALARM_NAMES join + output-block cost text + main.rs unconditional in-session gauge write with the [09:00, 09:15) IST pre-open pin to 0.0

## Scenarios

- [x] Item 3 — BOUNDARY-01 catch-up storm alarm, two parts. EXPORT: add a SECOND byte-identical `emf_processor` metric_declaration entry to BOTH `deploy/aws/cloudwatch-agent.json` and `deploy/aws/terraform/user-data.sh.tftpl`: `{"source_labels":["host"],"label_matcher":"^tickvault-prod$","dimensions":[["host","feed"]],"metric_selectors":["^tv_boundary_catchup_total$"]}` — per-feed dimension is Rule-11-MANDATORY (Groww's 60s catch-up margin makes catch-up sealing its ROUTINE steady-state for quiet SIDs; host-only folding would mask a Dhan storm under the Groww baseline or page on healthy Groww). Zero Rust — both emit sites exist (main.rs dhan driver, groww_bridge.rs groww driver). Cost 2 series ~$0.60/mo, dated comment + aws-budget.md note. ALARM in `silent-feed-alarms.tf`: `boundary_catchup_storm_dhan`, metric `tv_boundary_catchup_total`, dimensions = explicit map `{ host = "tickvault-prod", feed = "dhan" }` (NOT `local.app_dimensions`), statistic=Sum, period=300 (CW agent ships counters as per-scrape DELTAS ⇒ Sum/300s IS increase(5m), Rule 12 — house precedent `tv_disk_watcher_respawn_total`), `GreaterThanThreshold` 2000 (incident 9k–11.5k/10min ≈ 4.5–5.75k/5min = 2.25–2.9× threshold → pages at minute 10; PROVISIONAL in alarm_description — healthy Dhan floor UNMEASURED, mandate 1 trading-week soak of the exported per-feed Sum(5m) distribution + dated ratchet note), 2-of-2 × 5min (a bounded one-shot restart catch-up wave ≤~25K drains inside one 5-min period → absorbed; the 08:30 boot wave sits outside the 09:20–15:35 gate), `notBreaching`, `actions_enabled=false` + gate append. Ratchets: EMF name pin bump + teach the two-file byte-equality + alarm-metric-superset tests the second-declaration SHAPE (extend, never weaken).
  - Files: deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/user-data.sh.tftpl, deploy/aws/terraform/silent-feed-alarms.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, crates/common/tests/cloudwatch_app_alarms_wiring.rs, .claude/rules/project/aws-budget.md
  - Tests: test_emf_metric_selectors_name_count_is_twenty_six, test_reference_cloudwatch_agent_json_matches_deployed_emf_declaration, test_boundary_catchup_alarm_uses_per_feed_dimensions
  - Impl: second [["host","feed"]] emf metric_declaration in cloudwatch-agent.json + user-data.sh.tftpl (byte-identical); silent-feed-alarms.tf `boundary_catchup_storm_dhan` (explicit {host,feed=dhan} dims, Sum/300s, PROVISIONAL 2000, 2-of-2); ratchets: EMF pin 21->26 (rebased over #1437's 21->24), alarm-count 17->22 (scan extended to silent-feed-alarms.tf; rebased over #1437's +2), test_second_emf_declaration_publishes_boundary_catchup_per_feed (round-2 hardened per review finding 2: new `emf_declaration_objects` brace-parser binds selector↔dimensions WITHIN one declaration object + exactly-one-catchup-declaration check — the previous three independent whole-file substrings were bypassable by a multi-declaration reshuffle, verified by mutation); aws-budget.md dated cost note

- [x] Item 4 — THE ONLY RUST ITEM: new gauge `tv_dhan_exchange_lag_p99_seconds` (unlabeled, dhan-only NAME — sidesteps the host-only EMF dimension label-folding trap; a future Groww gauge gets its own name). HOT PATH (tick_processor.rs dhan persist site, `received_at_nanos` + `capture_seq` in scope — verify at impl time): `lag_ns = received_at_nanos.saturating_sub(exchange_ts × 1e9)`, clamp negatives to 0 (count clamps), write into a preallocated single-writer 32,768-slot ring with relaxed atomic head (~65s headroom at 500 ticks/s) — O(1), zero-alloc, DHAT + Criterion ratcheted + benchmark-budgets entry. REPLAY EXCLUSION — TWO-condition discriminator (round-2 fix 2026-07-07, review finding 3 — the dwell ALONE misclassified live pipeline-delayed ticks as replay: received_at is stamped at DEQUEUE, so receipt−capture measures in-channel dwell for LIVE frames too; the frame channel holds ~4 min at ~500 ticks/s and an ILP stall >60s would have excluded EVERY live tick, starved the window below MIN_LAG_SAMPLES, and frozen the gauge at the last healthy value — a stale-healthy false-OK during the exact lag class the alarm exists for): exclude ONLY when `received_at_nanos − capture_seq ≥ REPLAY_EXCLUDE_DWELL_NANOS = 60_000_000_000` (named constant) AND `capture_seq < live_boundary` (stamped once at ring init; a boot-time WAL-replayed row was captured by a PREVIOUS process so it always predates the boundary — the tick processor spawns before the reinject await, and live captures happen after the WS pool spawns, i.e. after the boundary). capture_seq is stamped once at the original WS-read instant and PRESERVED through WAL re-injection while received_at is RE-stamped at dequeue, so a replayed row shows receipt−capture = downtime (≥minutes, pre-boundary, excluded EXACTLY) while genuinely-lagged live rows (the incident's real 46s/199s AND >60s stall-delayed live rows) are KEPT — no Rule-11 censoring of the measured signal. Honest residuals: a RUNTIME D2b lane cold-start reinject (same-process captures) is admitted → bounded transient contamination (drain + 60s window, cannot latch 10-of-10); boundary clock-read failure (0) fails OPEN (nothing excluded). Every exclusion increments `tv_dhan_lag_samples_excluded_total` (CloudWatch-exported — in the 26-name EMF allowlist (rebased over #1437), ~$0.30/mo; visible, never silent. Round-1 correction: earlier drafts said "/metrics-only" — the shipped allowlists + cost notes + EMF ratchet export it, and the docs now match the deploy). PUBLISHER: supervised 10s cold task (WS-GAP-05/SLO-03 respawn pattern) in NEW module `crates/core/src/pipeline/feed_lag_monitor.rs`, spawned ONCE per process from BOTH boot arms (round-1 fix, findings 1/6/9: the FAST crash-recovery arm returns via `run_shutdown_fast` and never reaches `start_dhan_lane` — lane-only wiring left the gauge silently dark after any mid-market crash restart; both sites share the once-per-process guard, pinned by the 2-call-site ratchet): snapshot trailing-60s window into preallocated scratch, p99 via `select_nth_unstable` — honestly labeled **O(N-window)**, N≤32,768, off the tick thread (NEVER claimed O(1)). Publish ONLY when (a) inside a trading-session window — the regular [09:00,15:30) IST persist window OR the Muhurat [18:00,19:30) window when the boot flag is set (round-1 fix, finding 5: mirrors the persist sites' `is_within_persist_window(_, muhurat_active)`; Muhurat coverage is gauge-visibility only — the CW window-gate Lambda still enables actions 09:20–15:35 only; §30 window-exempt always-on SIDs can put off-session samples in the RING, the gate only controls PUBLISHING) — (Rule 3, prevents the stale-gauge-after-close artifact) AND (b) window holds ≥ `MIN_LAG_SAMPLES = 50` (empty/thin window publishes NOTHING — 0 = "perfect lag" is a Rule-11 false-OK; feed-dead is owned by the silent-instruments + WS alarms via notBreaching). QUANTIZATION HONESTY (module doc + alarm_description + metrics catalog + portal caveat): Dhan LTT is u32 whole IST SECONDS → ≥1s floor; healthy p99 reads ~1–2s and can never read 0; sub-second wire lag UNMEASURABLE for feed=dhan; alarm threshold sits 10× above the floor. EXPORT: add the gauge to both allowlist regexes (+$0.30/mo, part of the 21→26 pin bump (rebased over #1437's 21→24)). ALARM (`silent-feed-alarms.tf`): `dhan_exchange_lag_p99_high` — `GreaterThanThreshold` 10 (seconds), period=60, statistic=Maximum, strict 10-of-10 (safe HERE unlike items 1/2: the metric is itself a trailing-60s p99 recomputed every 10s, so a one-burst transient decays out within ~60s and cannot hold 10 consecutive breaching minutes; incident all-day p99 46s pages at minute 10), `notBreaching`, `actions_enabled=false` + gate.
  - Files: crates/core/src/pipeline/feed_lag_monitor.rs, crates/core/src/pipeline/tick_processor.rs, crates/core/src/pipeline/mod.rs, crates/app/src/main.rs, deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/user-data.sh.tftpl, deploy/aws/terraform/silent-feed-alarms.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, quality/benchmark-budgets.toml, crates/core/tests/dhat_feed_lag_ring.rs
  - Tests: test_replay_dwell_boundary_excludes_at_exactly_60s, test_live_stall_dwell_over_60s_post_boundary_capture_is_kept, test_zero_boundary_fails_open_never_excludes, test_thin_window_publishes_nothing, test_compute_window_p99_ns_on_known_distribution, test_ring_wraparound, test_negative_lag_clamped, test_out_of_session_publishes_nothing, test_record_dhan_tick_producer_sites_wired_into_tick_processor
  - Round-3 ratchet (2026-07-08, review finding 2): `test_record_dhan_tick_producer_sites_wired_into_tick_processor` pins EXACTLY 2 non-comment producer call sites in tick_processor.rs — the publisher half was already pinned by the secret_manager.rs 2-call-site ratchet, but the producer half had no pin, so a refactor dropping either call would silently starve the ring below MIN_LAG_SAMPLES → publisher publishes nothing → alarm notBreaching forever, the exact 2026-07-06 dark-gauge class.
  - Impl: crates/core/src/pipeline/feed_lag_monitor.rs (32,768-slot single-writer ring, strict-< 60s replay dwell, IST clock alignment, p99 via select_nth_unstable labeled O(N-window), Muhurat-aware session gate); tick_processor.rs record_dhan_tick at BOTH Dhan persist sites; main.rs spawn_supervised_feed_lag_publisher at BOTH boot arms (fast crash-recovery + start_dhan_lane, once-per-process guard); silent-feed-alarms.tf `dhan_exchange_lag_p99_high` (>10s, 10-of-10); dhat_feed_lag_ring.rs + benches/feed_lag_ring.rs + benchmark-budgets.toml entries; secret_manager.rs wiring ratchet (2-call-site pin); metrics_catalog.rs doc registration

- [x] Item 5 — Rewrite `_pctl_feed_sql` + the feed-discovery query in the operator-portal Lambda with the two-layer predicate + IST-now correction. VERIFIED LATENT BUG folded in: `ts` and `received_at` are stored IST-SHIFTED while QuestDB `now()` is UTC → the current `ts > dateadd('m',-10,now())` is really a ~5h40m window (root cause of the all-day replay conflation), AND an exchange-time window LAG-CENSORS (any row whose lag exceeds the window has ts outside it — the panel structurally cannot show lag above its own window, Rule-11 false-OK). All predicates use `IST_now := dateadd('m', 330, now())`, population defined by RECEIVE time. Exact new inner WHERE (per feed $f): `feed = '$f' AND received_at != null AND ts > dateadd('h', -6, IST_now) AND received_at >= dateadd('s', -120, IST_now) AND received_at < dateadd('s', 60, cast(capture_seq / 1000 + 19800000000 as timestamp))`. Roles: (1) K=120s receive-recency — honest population "rows received in the last 2 minutes" (6K–60K rows at incident rates → stable p99); (2) EXACT replay excluder (capture_seq nanos → micros → +19800000000µs IST alignment): frame-WAL re-injection re-stamps received_at but preserves capture_seq, so replayed rows show receipt−capture = downtime and are excluded EVEN INSIDE the drain window; genuinely-lagged live rows (46s/199s) pass; ~~groww never excluded by construction (sidecar capture_ns predates bridge capture_seq → receipt−capture ≤ 0)~~ ← STRUCK (round-1 finding 10, round-4 doc sync 2026-07-08: that rationale was FALSE — groww capture_seq is EXCHANGE-ts-seeded, so the dwell would censor genuine groww lag; the shipped excluder is FEED-AWARE, applied to `feed='dhan'` ONLY, and groww renders "—" per Edge Case 5); TVW2 spill-drain rows preserve original received_at (correctly kept); (3) 6h ts bound = partition-pruning ONLY (`ticks` is PARTITION BY HOUR on ts; received_at is not the designated timestamp — without it, full-table scan); the implied >6h-lag ceiling is documented. Rule-11 additions: (a) companion cheap count of the excluded complement per feed, rendered in the panel ("N replay-restamped rows excluded"); (b) rows==0 renders "no rows received in last 120s" — never zero-valued percentiles; (c) extend the existing whole-second caveat block with the 6h ceiling + replay-exclusion + ≥1s Dhan LTT floor. Keep `received_at != null` (Groww NULL receipt legitimate); the LIMIT-bounded sample is `ORDER BY received_at DESC LIMIT 50000` (round-1 finding 2, round-4 doc sync 2026-07-08: the original "Keep ORDER BY ts DESC" was superseded — exchange-ts-ordered truncation evicts the MOST-LAGGED rows first when the cap binds; the population's own key is receive time, and test_handler.py asserts `order by ts desc` is absent from the percentile SQL). Feed-discovery query gets the same IST fix with the 6h window. Live verification best-effort, non-blocking: `select now()` should confirm UTC; if the box returns IST, drop the `dateadd('m',330)` terms — nothing else changes; default: ship WITH the +330m shift.
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: N/A — Lambda Python; verified by live portal render + the QuestDB `select now()` timezone probe (documented in Test Plan)
  - Impl: deploy/aws/lambda/operator-control/handler.py `_pctl_feed_sql` + feed-discovery query (IST_now = dateadd('m',330,now()), K=120s receive-recency, exact capture_seq replay excluder, 6h ts partition-prune, replay-excluded count rendered, zero-row honesty line, extended caveat block); round-2 honesty (finding 3): SQL has no process-boundary column, so a live row delayed ≥60s in-pipeline is indistinguishable from a replay-restamped row and lands in the same excluded count — the column/notes/caveat now say "replay/stall rows excluded" (the in-app gauge KEEPS such rows via its live-boundary discriminator, so the CW alarm still sees stall lag); test_handler.py pins updated (169 tests green)

## Design

Five defenses, one per silent signal, all gated to the trading session and
edge-triggered natively by CloudWatch (Rule 4):

1. **Silent-instruments retune (Item 1)** — the existing
   `tick_gap_instruments_silent` alarm keeps its resource address (in-place
   update in `app-alarms.tf`, zero state churn) and drops its threshold from
   100 to 40 PROVISIONAL (round-3 correction 2026-07-08: the first cut of 25
   sat BELOW the documented ~33 always-silent healthy floor — see Item 1)
   with an M-of-N 10-of-12 latch (a value flapping 39/41/39
   neither pages nor lets one clean scrape erase 9 minutes of evidence; a
   1–3 min reconnect blip cannot reach 10 breaching minutes). Absolute
   threshold, not %-of-universe: no universe-size denominator exists in
   CloudWatch (`tv_universe_size` is kind-labeled and folds under host-only
   dims) and the universe is envelope-locked [100,1200].
2. **SLO dead-band closure (Item 2)** — a NEW Medium-tier CloudWatch alarm on
   the ALREADY-exported `tv_realtime_guarantee_score` at <0.95, 9-of-15
   (round-2 corrected latch — see Item 2).
   In-app SLO-02 stays log-only (2026-05-11 suppression untouched); the
   <0.80 Critical sibling alarm stays UNTOUCHED. This closes the exact
   0.80–0.95 dead band with zero Rust and zero allowlist change.
3. **Catch-up storm detector (Item 3)** — per-feed EMF export of
   `tv_boundary_catchup_total` + a dhan-dimensioned Sum/5m alarm at a
   PROVISIONAL 2000 threshold (2-of-2). Per-feed is mandatory: Groww's 60s
   margin makes catch-up its healthy steady state; folding feeds together is
   either a mask or a false page.
4. **Exchange-lag gauge + alarm (Item 4, the only Rust)** — no
   exchange→received metric exists today (`tv_wire_to_done_duration_ns` is
   receive→done only). A zero-alloc O(1) ring write on the dhan persist path
   feeds a supervised 10s publisher computing trailing-60s p99 (honestly
   O(N-window), never O(1)) into `tv_dhan_exchange_lag_p99_seconds`, with an
   exact WAL-replay exclusion via the received_at−capture_seq dwell
   discriminator and Rule-3/Rule-11 publish gating.
5. **Portal lag SQL truth (Item 5)** — IST-now correction + receive-time
   population + exact capture_seq replay excluder + partition-pruning ts
   bound, with Rule-11 companion rendering (excluded-count, no zero-valued
   percentiles on empty sets).

Cross-cutting: all NEW alarms live in NEW `silent-feed-alarms.tf` (PR
conflict isolation; Item 1 stays in `app-alarms.tf` to avoid a state-address
change); namespace/dimensions/SNS come from the module-global locals in
`app-alarms.tf`; every alarm is `actions_enabled=false` + appended to the
window-gate Lambda `ALARM_NAMES` (market-hours-liveness-alarm.tf, 09:20–15:35
IST Mon–Fri); `treat_missing_data=notBreaching` everywhere (nightly box
stop). Ratchet bumps ship in the same PR: EMF name pin 21→26 (rebased over #1437's 21→24), two-file
byte-equality + superset tests taught the second metric_declaration shape,
alarm-count pin 17→22 (rebased over #1437's +2) — the current `test_app_alarms_count_is_thirteen`
scanner (`alarm_metric_names()`) reads ONLY `app-alarms.tf` (verified
file-scoped), so it MUST be extended to also scan `silent-feed-alarms.tf`.
Total cost ~$1.20/mo — dated header comment + aws-budget.md note.

## Edge Cases

1. **Market-close boundary (15:30→16:30 scrape tail):** the silent-instruments
   gauge and the lag gauge stop being SET after close but the agent keeps
   scraping the LAST value until box stop — handled by (a) the window-gate
   Lambda disabling actions outside 09:20–15:35 IST and (b) Item 4's Rule-3
   in-session publish gate (gauge simply stops updating; notBreaching absorbs
   the missing-data tail after box stop).
2. **IST midnight / date rollover:** capture_seq is UTC-wall-nanos-monotonic
   and received_at is IST-shifted storage; the Item 5 predicates compare
   like-with-like (both IST-aligned after the +19800000000µs alignment) so
   midnight does not flip sign; the Item 4 dwell discriminator is a pure
   duration difference, date-free.
3. **WAL replay at EXACTLY the 60s dwell boundary:** admission uses strict
   `< REPLAY_EXCLUDE_DWELL_NANOS`; a PRE-live-boundary row dwelling exactly
   60.000000000s is EXCLUDED (pinned by
   `test_replay_dwell_boundary_excludes_at_exactly_60s`); a POST-boundary
   (live) row is KEPT at ANY dwell — a >60s consumer stall must never
   self-censor the lag signal (round-2 fix, finding 3 — pinned by
   `test_live_stall_dwell_over_60s_post_boundary_capture_is_kept`).
4. **Negative lag / clock skew:** exchange_ts ahead of received_at (Dhan
   second-granularity rounding, ≤2s host skew per BOOT-03) →
   `saturating_sub` clamps to 0 and the clamp is COUNTED — never a panic,
   never a negative sample.
5. **Groww rows at the dhan site:** the ring write is wired at the DHAN
   persist site only; the dhan-only metric NAME prevents label folding even
   if routing ever changed. Item 5 SQL excluder for groww (round-1
   correction, finding 10 — the earlier "no-op by construction,
   receipt−capture ≤ 0" rationale was FALSE): groww `capture_seq` is
   seeded from the tick's EXCHANGE timestamp in IST nanos
   (`groww_bridge.rs` `next_capture_seq(ts_ist_nanos)` — replay-stable,
   NOT a receipt stamp), so receipt−capture equals the positive lag. The
   dhan discriminator would censor genuine groww lag >60s and would NOT
   exclude replay-restamped groww rows — so the excluder is FEED-AWARE
   (applied to `feed='dhan'` only); groww's "replay rows excluded" renders
   "—" (n/a, documented limitation — replay-restamped groww rows are not
   distinguishable via capture_seq), never a fake verified-0.
6. **Thin window (pre-open trickle, holiday, feed warming up):** <50 samples
   in the trailing 60s → publish NOTHING (never 0 = "perfect lag");
   notBreaching keeps the alarm quiet; feed-dead ownership stays with the
   silent-instruments + WS alarms.
7. **QuestDB now() is actually IST (fallback):** best-effort `select now()`
   probe; if the box returns IST, drop the `dateadd('m',330)` terms — the
   two-layer predicate structure is unchanged. Default ships WITH +330m.
8. **CW agent restart mid-day:** counter DELTA semantics reset cleanly (first
   post-restart scrape publishes the delta since agent start); Sum/5m dips
   for ≤1 period; M-of-N latches tolerate the gap; missing scrapes are
   notBreaching.
9. **Gate-window edges (09:20 / 15:35 IST):** the 08:30 boot catch-up wave
   sits entirely outside the gate so it can never page. Round-3 correction
   (2026-07-08, review finding 5): the earlier wording ("an alarm latched
   into ALARM before 09:20 pages only once actions are enabled") described
   the wrong mechanism — the gate Lambda's `set_alarm_state(OK)` at 09:20
   does NOT purge datapoints, so the new degraded alarm's 9-of-15 lookback
   at ~09:21 reached back into [~09:05, 09:21) where the SLO publisher
   computed GENUINE tick_freshness during the NSE pre-open/auction window
   (most of the ~775 boot-seeded SIDs legitimately silent, esp. the
   09:08–09:15 matching freeze) → ≥9 pre-open breaching datapoints →
   near-daily false page at open (every previously-gated alarm has a
   lookback ≤5 min, which is why the 2-of-2 critical sibling never paged at
   open). FIX: the publisher pins tick_freshness to 1.0 before the 09:15:00
   continuous-session open (`SLO_TICK_FRESHNESS_SESSION_OPEN_SECS_OF_DAY_IST`
   in main.rs, mirroring the off-hours pin) — pre-open silence is not
   degradation; genuine freshness math starts at 09:15. Round-4 correction
   (2026-07-08, final-review findings 1/2/4): the SAME pre-open-latch class
   also applied to the retuned tick-gap alarm — its input gauge was written
   with GENUINE freeze counts from 09:00 and its 10-of-12 / 12-min lookback
   (the longest of any gated alarm) reached back to ~09:08 at the first
   gated evaluations. FIX: the gauge producer pins the written value to 0.0
   before 09:15:00 IST (`TICK_GAP_GAUGE_SESSION_OPEN_SECS_OF_DAY_IST`,
   ratcheted by test_tick_gap_silent_gauge_producer_pins_pre_open_to_zero).
10. **Universe re-scope toward the 1200 cap:** absolute threshold 40 becomes
    3.3% of universe — dated comment in app-alarms.tf mandates a revisit if
    the subscription set re-scopes; the [100,1200] envelope lock bounds the
    drift.
11. **Ring wraparound under burst (>32,768 ticks between snapshots):** oldest
    samples overwritten — the trailing-60s window is best-effort above ~546
    ticks/s sustained; 32,768 slots = ~65s headroom at the measured ~500/s;
    documented in the module doc, and the p99 remains a valid sample-based
    estimate of the window (pinned by `test_ring_wraparound`).
12. **A 1–3 min reconnect blip:** cannot reach 10-of-12 breaching minutes
    (Item 1), cannot reach 9-of-15 (Item 2), cannot hold 10/10 on a
    self-decaying trailing p99 (Item 4) — by construction none of the new
    alarms page on a healthy bounded reconnect.

## Failure Modes

1. **Window-gate Lambda dead → all gated alarms stay actions-disabled
   (dark):** accepted single point of coupling — it already gates the
   existing liveness alarms and its own health is covered by the
   market-hours-liveness machinery. Documented in Open Questions.
2. **Lag publisher task dies:** supervised respawn (WS-GAP-05/SLO-03
   pattern) with a respawn counter; a flapping publisher is visible, and a
   dead-forever publisher leaves the gauge UNSET → missing data →
   notBreaching (silent-instruments + WS alarms still own feed-dead).
   **Never-spawned case (round-1 fix, findings 1/6/9):** the supervisor is
   spawned from BOTH boot arms (fast crash-recovery + start_dhan_lane)
   behind a once-per-process guard, and the secret_manager.rs ratchet pins
   exactly 2 guarded call sites — removing either boot-path spawn fails
   the build (a lane-only spawn left the gauge dark for the whole session
   after a mid-market crash restart, silently, because the lag alarm is
   notBreaching on missing data).
3. **Ring writer starved / tick thread stalls:** no new samples → thin
   window → publisher publishes nothing (never stale-republishes) — the
   stall itself is owned by the existing tick-gap/WS alarms.
4. **CW export lag / EMF drop:** missing datapoints are notBreaching on
   every new alarm; M-of-N latches tolerate isolated gaps without either
   paging falsely or erasing accumulated evidence.
5. **Threshold-2000 healthy floor unknown:** labeled PROVISIONAL in the
   alarm_description; one trading-week observation of the per-feed Sum(5m)
   distribution is mandated; ratchet with a dated note if the healthy floor
   approaches 2000.
6. **Correlated 4-page fatigue:** one real feed-degradation incident will
   page up to 4 alarms (silent-instruments, SLO-degraded, catchup-storm,
   lag-p99) — ACCEPTED BY DESIGN: after a zero-page all-day incident,
   redundant paging is the chosen failure direction.
7. **Terraform apply partially fails (new file applies, gate append
   doesn't):** alarms are created with `actions_enabled=false` → fail-dark,
   not fail-noisy; the gate append is in the same PR/apply and the
   liveness-alarm ratchets pin the ALARM_NAMES join.

## Test Plan

- **Rust unit (crates/core, `feed_lag_monitor` + tick_processor wiring):**
  `test_replay_dwell_boundary_excludes_at_exactly_60s`,
  `test_thin_window_publishes_nothing`, `test_compute_window_p99_ns_on_known_distribution`,
  `test_ring_wraparound`, `test_negative_lag_clamped`,
  `test_out_of_session_publishes_nothing`.
- **DHAT zero-alloc:** `crates/core/tests/dhat_feed_lag_ring.rs` — ring write
  path allocates nothing across 10K calls.
- **Criterion + budget:** ring-write bench with a
  `quality/benchmark-budgets.toml` entry; the publisher's p99 computation is
  COLD path (10s cadence) — measured but budgeted as cold, labeled
  O(N-window) in comments and PR body (NEVER O(1)).
- **Ratchets (crates/common/tests/cloudwatch_app_alarms_wiring.rs):** EMF
  name-count pin 21→26 (rebased over #1437's 21→24) (`test_emf_metric_selectors_name_count_is_twenty_six`
  replaces `test_emf_metric_selectors_name_count_is_twenty_one`), two-file
  byte-equality (`test_reference_cloudwatch_agent_json_matches_deployed_emf_declaration`)
  + superset (`test_deployed_emf_declaration_is_superset_of_every_alarm_metric`)
  tests extended to parse the SECOND metric_declaration shape; alarm-count
  pin extended to scan `silent-feed-alarms.tf` (currently file-scoped to
  app-alarms.tf — verified); new pins:
  `test_tick_gap_silent_alarm_threshold_is_forty` (round-3 re-pin 2026-07-08),
  `test_tick_gap_silent_alarm_is_window_gated`,
  `test_realtime_guarantee_degraded_alarm_threshold_matches_slo_warn`,
  `test_silent_feed_alarms_are_window_gated`,
  `test_boundary_catchup_alarm_uses_per_feed_dimensions`.
- **Terraform:** `terraform validate` + `terraform plan` — Item 1 must show
  an IN-PLACE update (`~`) on the existing alarm resource, never
  destroy/create.
- **Lambda (Item 5):** local `python -m py_compile`; live best-effort
  `select now()` probe against QuestDB (UTC assumption); portal render check
  post-deploy: excluded-count line present, zero-row case renders "no rows
  received in last 120s", never zero-valued percentiles.
- **Scoped per testing-scope.md:** `cargo test -p tickvault-core`; the
  `crates/common` ratchet-file edit escalates to `cargo test --workspace`.

## Rollback

- **Terraform:** revert the PR and `terraform apply` — new alarms in
  `silent-feed-alarms.tf` are deleted wholesale (new file, no shared state);
  Item 1 reverts in place to threshold 100 (same resource address, no state
  surgery); the gate-Lambda ALARM_NAMES join reverts with the same apply.
- **Rust:** reverting the commit removes the ring write (both Dhan persist
  sites in `run_tick_processor` — which runs on BOTH boot arms, fast and
  slow) and the publisher spawns (both boot arms, once-per-process guard);
  the metric simply stops being published (missing data = notBreaching, no
  false page during rollback).
- **Portal SQL:** single-function revert of `_pctl_feed_sql` (+ the
  feed-discovery query) in handler.py restores the previous behavior
  verbatim.
- No schema changes, no data migration, no DEDUP-key changes anywhere — all
  rollbacks are pure code/config reverts.

## Observability

- **New metrics:** `tv_dhan_exchange_lag_p99_seconds` (gauge, in-session
  only — regular + Muhurat windows — ≥50-sample gate),
  `tv_dhan_lag_samples_excluded_total` (exclusion visibility — never
  silent censoring; CloudWatch-exported, part of the 26-name EMF (rebased over #1437)
  allowlist), `tv_dhan_lag_negative_clamped_total` (/metrics-only),
  publisher respawn counter (WS-GAP-05-pattern `{reason}` labels),
  per-feed `tv_boundary_catchup_total` CW series (host,feed dims).
- **New/retuned alarms (4):** `tick_gap_instruments_silent` (retuned 40
  PROVISIONAL, 10-of-12), `realtime_guarantee_degraded` (<0.95, 9-of-15),
  `boundary_catchup_storm_dhan` (Sum/5m >2000 PROVISIONAL, 2-of-2),
  `dhan_exchange_lag_p99_high` (>10s, 10/10) — all window-gated
  09:20–15:35 IST, all notBreaching, all edge-triggered natively (Rule 4).
- **Portal:** honest lag panel — receive-time population, replay-excluded
  count rendered, empty-population honesty line, extended caveat block (6h
  ceiling, replay exclusion, ≥1s Dhan LTT floor).
- **Docs:** metrics catalog entries, aws-budget.md cost note (~$1.20/mo
  total), dated PROVISIONAL-threshold soak note for the catch-up storm
  alarm.

## Per-Item Guarantee Matrix (per `.claude/rules/project/per-wave-guarantee-matrix.md`)

### 15-row 100% Guarantee Matrix

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | unit tests on every `feed_lag_monitor` pure fn; coverage delta ≥ 0 vs `quality/crate-coverage-thresholds.toml` floors | post-merge llvm-cov | Item 4 PR includes coverage delta |
| 100% audit coverage | N/A — no new SEBI-relevant typed event; alarms/metrics are observability-plane (CloudWatch alarm history is the record) | CW alarm history | noted per item |
| 100% testing coverage | unit + DHAT + Criterion + ratchet source-scan, scoped to crates/core + crates/common per testing.md | `cargo test -p tickvault-core` + workspace on common | Test Plan section |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + pre-commit gates | pre-push mandatory | all gates green before PR |
| 100% code performance | DHAT zero-alloc on the ring write + Criterion + benchmark-budgets entry; publisher honestly O(N-window) COLD | `scripts/bench-gate.sh` | Item 4 is the only hot-path item |
| 100% monitoring | the deliverable IS monitoring: 1 new gauge + 1 exclusion counter + 1 respawn counter + per-feed CW series + 4 alarms | CW console + `/metrics` | Observability section |
| 100% logging | publisher respawn logs `error!` + respawn counter (WS-GAP-05 supervisor pattern); deliberately NO new ErrorCode / `code =` field (least-new-surface decision documented at the supervisor doc comment — the tag-guard requires `code =` only when the message mentions a tracked code, which this one does not); no new silent path | errors.jsonl | Item 4 |
| 100% alerting | 4 window-gated CW alarms → SNS → Telegram; notBreaching + M-of-N semantics documented per alarm | gate-Lambda ALARM_NAMES pin | Items 1–4 |
| 100% security | no secrets, no new inputs, no unsafe; Lambda SQL interpolates only a fixed internal feed-name set (no user input) | secret-scan + cargo audit | all items |
| 100% security hardening | no new attack surface (no new endpoints, no new deps) | post-deploy IP verify unchanged | N/A — observability-only change |
| 100% bugs fixing | adversarial 3-agent review BEFORE + AFTER impl | per-PR | required |
| 100% scenarios covering | 12 edge cases enumerated above, each with an owner (test, gate, or notBreaching) | Edge Cases section | per item |
| 100% functionalities covering | every new pub fn in feed_lag_monitor has a call site + matching #[test] | pre-push gates 6+11 | Item 4 |
| 100% code review | 3-agent adversarial on the diff before AND after implementation | per-PR | required |
| 100% extreme check | ratchet tests fail the build on regression (thresholds, gate-wiring, EMF pins, alarm counts) | every commit | Items 1–4 ratchets |

### 7-row Resilience Demand Matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | NO new tick-drop path — the ring write is a side-observation; ring→spill→DLQ untouched | Item 4 touches only the persist-site observation point |
| WS never disconnects | `SubscribeRxGuard` + pool watchdog untouched; new alarms only OBSERVE degradation | Items 1–4 are read-side |
| Never slow/locked/hanged | ring write O(1) zero-alloc (DHAT + Criterion); p99 compute is honestly O(N-window), N≤32,768, on a 10s COLD task off the tick thread | Item 4 DHAT + bench |
| QuestDB never fails | no schema change, no new write path; Item 5 is read-only SQL with a partition-pruning bound (no full-table scan) | Item 5 |
| O(1) latency | hot-path insert O(1); the p99 computation is O(N) and is LABELED O(N) in code comments + PR body — never claimed O(1) | Item 4 |
| Uniqueness + dedup | N/A — no new table, no DEDUP-key change; the per-feed CW dimension prevents cross-feed series folding (the CW analogue of I-P1-11) | Item 3 |
| Real-time proof | 4 alarms + gauge + excluded-samples counter + respawn counter; incident replay: each of the 4 silent signals now pages within 10–15 min | Observability section |

## Honest 100% Claim (operator-charter §F wording)

> 100% inside the tested envelope, with ratcheted regression coverage:
> the four 2026-07-06 silent signals each gain a window-gated, notBreaching,
> M-of-N-latched CloudWatch page that fires within 10–15 minutes on an
> incident replay (thresholds pinned by build-failing ratchets in
> `crates/common/tests/cloudwatch_app_alarms_wiring.rs`); the lag ring write
> is DHAT-zero-alloc + Criterion-budgeted O(1) on the hot path while the
> p99 computation is honestly O(N-window), N≤32,768, on a cold 10s task;
> the ≤100,000-tick rescue ring / spill / DLQ chain is untouched (no new
> tick-drop path). Beyond the envelope: the boundary-catchup threshold 2000
> is PROVISIONAL until a one-trading-week soak measures the healthy Dhan
> floor; sub-second Dhan wire lag is UNMEASURABLE (u32 whole-second LTT
> floor); the alarms depend on the window-gate Lambda being alive (its
> failure leaves them dark — accepted and documented); and lag above 6h is
> invisible to the portal panel by the documented partition-pruning ceiling.

## Open Questions (with shipped defaults)

1. **QuestDB `now()` timezone** — Assumed UTC (matches storage code:
   received_at is written `Utc::now()+IST_UTC_OFFSET_NANOS`, i.e. the DB
   clock is UTC and rows are IST-shifted). Ship WITH
   `dateadd('m',330,now())`; best-effort live `select now()` probe; if the
   box returns IST, drop the +330m terms — nothing else changes.
2. **capture_seq wall-clock assumption** — Assumed: capture_seq ≈ UTC wall
   nanos at the original WS-read instant (`max(prev+1, wall_nanos)` per
   data-integrity.md) and is PRESERVED through WAL re-injection. Spot-check
   on live data pre-merge (compare capture_seq vs received_at on fresh
   rows).
3. **Boundary-catchup healthy Dhan floor** — Unknown. Ship PROVISIONAL 2000
   (2.25–2.9× below incident rate) + mandatory 1-trading-week soak of the
   exported per-feed Sum(5m) distribution; ratchet with a dated note if the
   healthy floor approaches 2000.
4. **M-of-N deviation from literal "sustained N×M" wording** — Deliberate:
   strict-consecutive evaluation on the oscillating incident data (125
   crossings, 24/26/24 flapping) would never latch and reproduces the miss
   (Rule-11 false-OK). 10-of-12 / 9-of-15 are the honest latches (9-of-15
   is the round-2 correction: freshness-only breach needs ≥39 of 776
   silent, so incident minutes at 29–38 silent sample Healthy and a
   12-of-15 latch could miss the marginal band); Item 4
   keeps strict 10/10 because its metric is self-smoothing.
5. **Medium-tier Telegram rendering** — Limitation: telegram-webhook
   handler.py stamps the same emoji on any ALARM; a Medium-vs-Critical
   visual split is a FLAGGED FOLLOW-UP, not in this PR.
6. **Alarm-count ratchet scan scope** — Verified file-scoped to
   `app-alarms.tf` (`alarm_metric_names()` reads only that file); MUST be
   extended in this PR to also scan `silent-feed-alarms.tf`, else the new
   alarms escape the three-way drift guard.
7. **Gate-Lambda single point of coupling** — Accepted risk: all four
   alarms are dark if the window-gate Lambda dies; it already gates the
   existing liveness alarms and is itself covered by the
   market-hours-liveness machinery.
8. **Correlated-page fatigue** — Accepted by design: one real feed
   degradation may fire up to 4 pages; after a zero-page all-day incident,
   redundancy is the chosen failure direction.
9. **Seconds-vs-milliseconds units** — Seconds chosen for the lag gauge:
   Dhan LTT has a ≥1s quantization floor, so millisecond units would imply
   false precision; the name carries `_seconds` and the module doc states
   the floor.
