# FINAL SPEC live-proof (ship=true)

## Summary
The defect is real in all three parts. I re-read origin/main at 60bdfd97a; none of the facts the design relied on at 1d31c37a1 have changed.

(1) No per-minute quantile reaches CloudWatch. Only the two one-minute peak maxima do: tv_dhan_ws_lag_max_ms and tv_dhan_feed_ring_dwell_max_ms. (Verified)

(2) The kernel_rx_queue_backlog WS-GAP-03 line pages nobody. None of the four WS-GAP-03 filters matches its source. (Verified)

(3) tv_doctor has DEFAULT_METRICS_URL = "http://localhost:9090/metrics". The exporter listens on 127.0.0.1:9091 (base.toml, ObservabilityConfig::default). (Verified)

Two of the task's premises are stale. (Verified)
- The EMF selector no longer has a byte budget. What binds now is the exact-count ratchet assert_eq!(names.len(), 113), plus cost and the producer and dashboard guards.
- An errors.jsonl metric filter cannot carry a host dimension. The page therefore has no dimension, the same as §2.8.

Review points folded in:
- WS lag p99 now reads the quarter-octave day histogram DHAN_DAY_LAG_HIST (96 buckets, about ±9%) instead of WsLagSlot's 14 coarse bounds.
  - record_ws_lag already feeds it the same samples, so this adds zero per-tick work. (Verified)
  - This fixes the "pinned at 2,500" problem. The 0–999 ms whole-second stamp bias remains, and the spec states it honestly.
- A -1 "not measured" sentinel replaces NaN. No host-only gauge sets NaN today, and the agent's behaviour with NaN is unproven. (Verified / Assumed risk)
- The alarm uses period 3600, matching all four WS-GAP-03 siblings and RISK-GAP-03, because the latch re-arms with no rate limit.
- The triage text is corrected. FrameSink::accept is synchronous and sheds on RingFull, so a kernel backlog means the reader or its runtime is slow, not the drain. (Verified, pool_supervisor.rs accept/try_reserve_detailed)
- PR-B does not cite a metric from PR-C.
- Overflow buckets publish a value distinct from the finite bounds.
- The window logic is tested on local instances, never the global.
- The hook is panic-free, enforced by clippy deny attributes, and runs after the stall check.
- tv_doctor's URL is built with SocketAddr.
- The quantile window is pinned to prometheus.yaml scrape_interval: 60s.
- The every_ws_gap_03 floor goes 4 to 5.
- dhat_hot_path_telemetry is a NEW test binary; that test file does not exist today.
- Plan R3-10 is amended, plus gap-enforcement.md, EMF-METRIC-SELECTOR-NOTES.md, the CLAUDE.md rows and the module table.
- main.rs uses the tickvault_app:: path.

Review points dropped:
- "Expose WsLagSlot windowing on a WsLagHandles method". Superseded: the quarter-octave source is a const static in core, and the window state is a local, testable struct.

Authorization gate for PR-B and PR-C:
- The user's relayed words "See do everything whatever is recommended dude okay?" are verbatim. "Go ahead with whatever you want dude" comes only from script text.
- Noise-lock §2.10 says a general go-ahead does not authorize a page; the §2.8 precedent accepts one only when it answers an enumerated, priced recommendation.
- So the §2.11 row must name the recommendation each quote answered. If that recommendation did not list this page and these two gauges with their price, post one decision card first. That would be a §3 REJECT otherwise.
- PR-A is ungated.

Cost (Assumed list prices): about $0.70–$1.00 per month in total; tv_doctor costs $0.

ORDER: three serial PRs, A then B then C. Each merges on All Green before the next opens.

No WebSocket behaviour changes: no subscribe, redial, 805 handling or ROTATION_HALTED is touched, so websocket-connection-scope-lock.md is not edited. (Verified)

There are 5 active plans, exactly at the V7 cap. (Verified, origin/main tree.) Create NO new plan file; amend .claude/plans/active-plan-audit-fixes-2026-09-26.md, which is Status APPROVED.

=== PR-A: tv_doctor metrics port (one implementation file; PLAN-EXEMPT allowed) ===

A1. crates/app/src/bin/tv_doctor.rs
- Delete `const DEFAULT_METRICS_URL`.
- Add `fn default_metrics_url() -> String`:
  - `let c = tickvault_common::config::ObservabilityConfig::default();`
  - `format!("http://{}/metrics", std::net::SocketAddr::new(c.metrics_bind_addr, c.metrics_port))`
  - Result: http://127.0.0.1:9091/metrics.
  - SocketAddr's Display brackets IPv6 addresses.
  - The URL is derived because banned-pattern rejects both `"localhost` and `"127.0.0.1` literals.
- main: `parse_flag(&args, "--metrics-url").unwrap_or_else(default_metrics_url)`.

A2. Module doc in the same file:
- Example port 9090 becomes 9091.
- Replace "Called automatically by systemd / CloudWatch alarm handlers" with "Run by hand; no automatic caller today". (Verified: nothing in deploy/, scripts/, Makefile or .github references tv-doctor.)
- Remove tv-valkey, which is retired.

A3. Test `default_metrics_url_points_at_the_exporter_port`:
- Asserts the URL ends with ":9091/metrics".
- Asserts include_str!("../../../../config/base.toml") contains "metrics_port = 9091".
- Bite: restoring 9090 turns it red.
- Commit body: PLAN-EXEMPT: one-file default-port fix in a manual CLI.

=== PR-B: kernel receive-queue backlog page ===

B0. Rule files first, in the same PR.

docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md: new section "## §2.11 — 2026-10-06: a SLOW-CONSUMER page on the kernel receive-queue backlog (WS-GAP-03, source kernel_rx_queue_backlog)". It contains:
- The owner quotes verbatim: "See do everything whatever is recommended dude okay?" and "Go ahead with whatever you want dude". Record the date 2026-10-06 and WHICH enumerated recommendation each answered, in the §2.8 shape. If no enumerated, priced recommendation named this page, stop and post a decision card. Do not merge.
- Table row: `tv-<env>-errcode-ws-gap-03-kernel-rx-backlog` | `{ $.code = "WS-GAP-03" && $.level = "ERROR" && $.source = "kernel_rx_queue_backlog" }` | Sum >= 1 in one 3600 s period.
- Shape: treat_missing_data notBreaching; NO ok_actions; NO dimension (errors.jsonl has no host field and a filter cannot add a constant one; §2.8 precedent); the broker is named in the Telegram phrase; no EMF name.
- Edge: the BacklogLatch fires once per episode (5 consecutive 1 s samples above 4 MiB, re-arms below 2 MiB, session-gated). Period 3600 caps a reader oscillating around 4 MiB at about one page an hour, as RISK-GAP-03 does.
- Cost: $0.10 per month for the alarm, plus the filter metric's $0.30 per month prorated to the hours it has data.
- NOT claimed: that a page proves loss; any coverage outside 09:00–15:40; that 4 MiB × 5 s is measured-optimal; that port 443 is Dhan-only.
- REJECT rows: adding ok_actions or any dimension; a bare-code WS-GAP-03 filter (one without a source); removing the latch or making emission level-triggered; a period shorter than 3600 without a dated row; filtering on the SUM gauge; removing the session gate or the broker name.

.claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md (summary stub): add one "(§2.11, 2026-10-06 ...)" bullet to the allowed set and one §2.11 REJECT headline.

B1. deploy/aws/terraform/error-code-alarms.tf: a new `local.error_code_alerts` entry after "ws-gap-03-xverify-diverged":
```
"ws-gap-03-kernel-rx-backlog" = {
  pattern     = "{ $.code = \"WS-GAP-03\" && $.level = \"ERROR\" && $.source = \"kernel_rx_queue_backlog\" }"
  period      = 3600
  threshold   = 1
  eval        = 1
  dta         = 1
  ok_recovery = false # once per episode; an auto-OK means the datapoint aged out, not that the reader caught up
  desc        = "WS-GAP-03 SLOW CONSUMER: in session, a DHAN feed socket's kernel receive queue stayed above 4 MiB for 5 consecutive 1-second samples. Bytes Dhan already sent are waiting unread on our side, and Dhan skips a slow consumer forward with no sequence number, so ticks may be lost at Dhan's side with no counter of ours moving. The socket reader never waits on the ring (a full ring sheds and is counted on tv_dhan_ws_ring_full_total), so this means the READER or its runtime is slow: check tv_hot_path_stage_max_ns for reader_runtime_lag and socket_to_wal, a HOT-PATH-STALL-01 page, and host CPU. Once per episode; fields max_bytes, sum_bytes, sockets. No recovered page. Runbook: .claude/rules/project/gap-enforcement.md"
}
```
- The existing for_each generates:
  - the filter: metric tv_errcode_ws_gap_03_kernel_rx_backlog, value 1, no default_value;
  - the alarm: Sum >= 1, notBreaching, no dimensions, ok_actions = [].
- desc plus the ~162-character suffix must stay under 1024 characters.
- Update the map-count comment (23 → 24) with "+1 on 2026-10-06, ws-gap-03-kernel-rx-backlog, noise-lock §2.11".

B2. crates/aws-lambdas/src/telegram_webhook.rs: `ALARM_PHRASES` grows from [(&str,&str);126] to 127, adding
`("errcode-ws-gap-03-kernel-rx-backlog", "🔷 DHAN: our server is reading the live feed too slowly — Dhan may skip some prices; check the server")`.
It contains no banned tokens.

B3. crates/app/src/kernel_rx_queue_sampler.rs:
- Module doc: replace "No CloudWatch selector, alarm, terraform or Telegram notification is attached" with "the WS-GAP-03 line pages via errcode-ws-gap-03-kernel-rx-backlog (noise-lock §2.11); the two gauges stay Prometheus-only".
- error! message: replace "Check the frame drain (ring dwell, fold stalls, disk)" with "Check the socket reader: reader runtime lag, the socket-to-WAL step, host CPU. A full ring sheds and is counted separately."
- Keep the code field. Leave the latch and the threshold logic unchanged.

B4. .claude/rules/project/gap-enforcement.md, which is the WsGapConnectionState runbook_path: add a triage section for source=kernel_rx_queue_backlog that matches B1.

B5. .claude/rules/project/observability-architecture.md, "Filtered+alarmed codes": append "WS-GAP-03/kernel-rx-backlog (added 2026-10-06, noise-lock §2.11)".

B6. active-plan-audit-fixes-2026-09-26.md:
- Amend R3-10's "no alarm (needs a dated quote)" to "alarm added by R3-10b, noise-lock §2.11".
- Append item R3-10b with its files and tests, and a cross-reference to the 15-row + 7-row matrix so per-item-guarantee-check.sh passes.
- Tick it inside the PR diff.

B7. CLAUDE.md, the `crates/app/src/kernel_rx_queue_sampler.rs` row: append "pages via errcode-ws-gap-03-kernel-rx-backlog (2026-10-06, §2.11)".

B8. Guards:
- cloudwatch_app_alarms_wiring.rs::every_ws_gap_03_filter_carries_a_source_discriminator: raise the floor `patterns.len() >= 4` to 5, with a dated comment.
- live_lane_paging_lockstep_guard.rs: add the new test the_kernel_rx_backlog_page_exists_without_a_false_recovery. It asserts the block has the three-clause pattern, period = 3600 and ok_recovery = false, and that NOISE_LOCK contains "§2.11" and "kernel_rx_queue_backlog".
- alarm_phrase_coverage_guard.rs::guard_self_test_parser_bites: raise the key floor from 21 to 24.

=== PR-C: windowed per-minute p99 gauges (two unalarmed EMF names) ===

C0. Rule and cost notes first:
- noise-lock §2.11 addendum: two UNALARMED host-only gauges, same quotes, same gating condition.
- aws-budget.md "COST NOTE 2026-10-06": +2 EMF names, about $0.60 per month upper bound. The budget position is not read live. The §2.3n lever rule is not met, so this is the owner choosing spend at the stated price.
- deploy/aws/EMF-METRIC-SELECTOR-NOTES.md:
  - correct the stale LOCKSTEP/user-data byte-budget text; the selector has lived only in cloudwatch-agent.json since 2026-08-25;
  - record why two unalarmed names pass the inclusion rule: they are SATURATION signals (feed delay, drain wait) charted as live proof.

C1. crates/storage/src/hot_path_telemetry.rs (pure core and ring-dwell gauge). Add:
- `pub const QUANTILE_WINDOW: Duration = Duration::from_secs(60);` with an // APPROVED: comment. It equals prometheus.yaml scrape_interval 60s.
- `pub const QUANTILE_MIN_SAMPLES: u64 = 100;`
- `pub const P99_PER_MILLE: u64 = 990;`
- `pub const QUANTILE_NOT_MEASURED: f64 = -1.0;`
- `pub const RING_DWELL_P99_MS_GAUGE: &str = "tv_hot_path_ring_dwell_p99_ms";`

`#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)] pub fn windowed_quantile_bucket(prev: &mut [u64], cur: &[u64], per_mille: u64, min_samples: u64) -> WindowQuantile`, where `pub enum WindowQuantile { Bucket(usize), Thin, Reset }`:
- If any cur < prev (a reset), copy cur into prev and return Reset.
- n = Σ (cur − prev), computed with zip, saturating_sub and saturating_add.
- Copy cur into prev. The window is CONSUMED even when it is thin, and is never merged into the next one; the doc says so.
- If n < min_samples, return Thin.
- rank = n.saturating_mul(per_mille).saturating_add(999) / 1000, i.e. ceil(0.99n).
- Walk the deltas cumulatively and return Bucket(first index where cum >= rank).
- Never indexes and never panics. It is O(len), zero allocation.

`fn ring_dwell_bucket_ms(k) -> f64`:
- finite k: bucket_upper_bound_nanos(k) / 1e6;
- +Inf (k = 27): 2^37 ns / 1e6 = 137,438.953472 ms, documented as "beyond 68.7 s". It is distinct from bucket 26's 68,719.476736 ms.

TelemetryHandles gains `ring_dwell_p99: Gauge`, resolved once in resolve().

`static QUANTILE_HOOK: OnceLock<fn()>` and `pub fn register_quantile_hook(f: fn()) -> bool`.

spawn_publisher loop:
- A stack-held `QuantileWindow { prev_ring: [u64; STAGE_BUCKETS], primed: bool, next_due: Instant }`.
- AFTER `alarm.observe` / `report_stall` (so the stall check is never delayed), when now >= next_due:
  - snapshot stage_snapshot(Stage::RingDwell).buckets;
  - first call: prime and set QUANTILE_NOT_MEASURED;
  - later calls: Bucket(k) sets ring_dwell_bucket_ms(k); Thin or Reset sets -1;
  - then `if let Some(h) = QUANTILE_HOOK.get() { h() }`;
  - next_due += QUANTILE_WINDOW.
- Update the module complexity table (publish_once row): "+ once per 60 s: 28 loads + O(28) walk".

C2. crates/core/src/pipeline/feed_lag_monitor.rs (accessor only; the record path is untouched):
- `pub const DAY_LAG_BUCKETS: usize = LAG_HIST_BUCKETS;`
- `pub fn day_lag_bucket_counts(feed: Feed) -> [u64; DAY_LAG_BUCKETS]`: relaxed loads, O(96), cold.
- `pub fn day_lag_bucket_upper_ms(idx: usize) -> f64`: lag_hist_bucket_lower_quarter_ms(idx + 1) as f64 / 4.0, with idx bounded by the bucket count.
- Fix the stale "ZERO producers" doc: live since 2026-09-05 via record_ws_lag.

C3. crates/app/src/dhan_feed_stack.rs. Production code, outside #[cfg(test)]:
- `pub const WS_LAG_P99_MS_GAUGE: &str = "tv_dhan_ws_lag_p99_ms";`
- `pub struct WsLagP99Window { prev: [u64; DAY_LAG_BUCKETS], primed: bool }`:
  - `const fn new()`;
  - `#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)] fn step(&mut self, cur: &[u64; DAY_LAG_BUCKETS]) -> f64`. First call primes and returns -1. Later calls map Bucket(k) to day_lag_bucket_upper_ms(k), and Thin or Reset to -1. A Reset is the IST-midnight reset, which happens outside the session.
- `static WS_LAG_P99_WINDOW: std::sync::Mutex<WsLagP99Window> = Mutex::new(WsLagP99Window::new());` Locked only by tv-telemetry, once a minute; `if let Ok(mut w) = lock()`.
- `pub fn publish_ws_lag_p99_window()`: reads day_lag_bucket_counts(Feed::Dhan), steps the window, then `metrics::gauge!(WS_LAG_P99_MS_GAUGE).set(v)`. Static name, no labels, cold.
- It does NOT call ws_lag_handles().
- record_ws_lag and WsLagSlot are NOT touched.

C4. crates/app/src/main.rs, immediately BEFORE the spawn_publisher call (~line 1065):
`if !tickvault_storage::hot_path_telemetry::register_quantile_hook(tickvault_app::dhan_feed_stack::publish_ws_lag_p99_window) { warn!("quantile hook already registered") }`.
Registering first means the first window is not skipped. If the spawn fails, the hook simply never runs.

C5. deploy/aws/cloudwatch-agent.json: append `|tv_dhan_ws_lag_p99_ms|tv_hot_path_ring_dwell_p99_ms` inside the anchored alternation. The host dimension only; both gauges are unlabelled.

C6. crates/common/tests/cloudwatch_app_alarms_wiring.rs: assert_eq 113 → 115, with a dated comment and the message string updated.

C7. deploy/aws/terraform/dashboard.tf, in the "How far behind the drain is" widget:
- `[local.dash_namespace,"tv_hot_path_ring_dwell_p99_ms",{label="ring wait p99, all frames main+depth (bucket upper bound, <2x; -1 = too few samples)",stat="Maximum"}]`
- `[local.dash_namespace,"tv_dhan_ws_lag_p99_ms",{label="feed delay p99 (quarter-octave upper bound; includes up to 1 s exchange-stamp rounding; -1 = too few samples)",stat="Maximum"}]`
- No alarm on either.

C8. CLAUDE.md, hot_path_telemetry.rs row: append "once per 60 s on tv-telemetry: 28 + 96 relaxed loads, two O(N) walks, two gauge sets, one uncontended mutex; zero allocation".

C9. Plan: append LP-1 (files and tests, matrix cross-reference) to active-plan-audit-fixes-2026-09-26.md; tick it in the PR diff.

HONEST READING of the gauges (goes in the docs):
- Each value is the UPPER BOUND of the bucket holding the ceil(0.99n)-th sample. It never understates.
- Ring dwell overstates by less than 2×.
- WS lag overstates by up to about 25% per quarter-octave step, and every sample carries a 0–999 ms positive bias from Dhan's whole-second LTT (ws_lag_whole_ms = receipt_ms − floor(LTT)×1000). So a healthy feed reads about transit-p99 + up to 1 s, and the gauge detects meaningful shifts above about 1.3 s, not sub-second transit.
- Repeat quotes are excluded, the same as the existing histograms.
- The p99 is lane-wide; the per-minute MAX gauges cover a single slow socket.
- The agent scrapes at an unaligned 60 s phase, so a window may be read twice or skipped.

## Tests
["PR-A tv_doctor.rs #[cfg(test)] default_metrics_url_points_at_the_exporter_port — ends with ':9091/metrics' and include_str!(\"../../../../config/base.toml\") contains 'metrics_port = 9091'; bite: 9090 restored -> red","PR-B crates/common/tests/live_lane_paging_lockstep_guard.rs::the_kernel_rx_backlog_page_exists_without_a_false_recovery (new): three-clause pattern, period 3600, ok_recovery=false, NOISE_LOCK contains '§2.11' and 'kernel_rx_queue_backlog'","PR-B crates/app/src/kernel_rx_queue_sampler.rs tests::the_alarm_filter_matches_this_modules_source_value — include_str!(error-code-alarms.tf) contains the escaped `$.source = \\\"kernel_rx_queue_backlog\\\"` built from KERNEL_RX_BACKLOG_SOURCE; bite: renaming const or filter -> red","PR-B crates/common/tests/cloudwatch_app_alarms_wiring.rs::every_ws_gap_03_filter_carries_a_source_discriminator with floor raised 4 -> 5","PR-B crates/aws-lambdas/tests/alarm_phrase_coverage_guard.rs (floor 21 -> 24) + telegram_webhook tests::test_alarm_phrases_pass_telegram_commandments stay green with entry 127","PR-B run unchanged and green: error_code_paging_filter_drift_guard, crates/storage/tests/alarm_description_length_guard.rs, error_code_alarm_coverage_guard, breaching_alarms_are_gated_guard, alarm_metric_has_a_route_guard","PR-C storage hot_path_telemetry unit tests: windowed_quantile_bucket_thin_below_min_samples; windowed_quantile_bucket_rank_is_ceil_99 (n=100: 99 in b0 + 1 in b5 -> Bucket(0); 98+2 -> Bucket(5)); windowed_quantile_bucket_uses_only_the_window_delta (large prev, small window); windowed_quantile_bucket_consumes_a_thin_window (next window excludes it); windowed_quantile_bucket_reset_when_cur_below_prev; windowed_quantile_bucket_overflow_returns_last_index","PR-C storage proptest proptest_windowed_p99_matches_a_sorted_reference — random ns samples, bucket_index each, sorted reference element at ceil(0.99n)-1 maps to the same bucket as windowed_quantile_bucket","PR-C storage ring_dwell_bucket_ms_is_the_upper_bound_and_overflow_is_distinct — k=10 -> 1.048576, k=26 -> 68719.476736, k=27 -> 137438.953472 (all distinct)","PR-C storage quantile_window_equals_the_prometheus_scrape_interval — include_str deploy/aws/prometheus.yaml contains 'scrape_interval: 60s' and QUANTILE_WINDOW == 60 s","PR-C NEW crates/storage/tests/dhat_hot_path_telemetry.rs (own dhat global allocator): after warm-up, 1,000 windowed_quantile_bucket calls on stack arrays allocate 0 blocks","PR-C core feed_lag_monitor tests: day_lag_bucket_counts_reflects_record_day_lag_ms; day_lag_bucket_upper_ms_matches_bucket_index_inverse (for v in samples, v <= upper(index(v)))","PR-C app dhan_feed_stack tests on a LOCAL WsLagP99Window::new() (never the static): first_step_primes_and_returns_minus_one; thin_window_returns_minus_one; p99_is_quarter_octave_upper_bound (1000 samples at 1200 ms + 5 at 4000 ms -> upper bound of index(1200)); midnight_reset_returns_minus_one_then_recovers","PR-C crates/app/tests/dhat_ws_lag.rs and dhat_ws_lag_recorder.rs stay green unchanged (record path untouched)","PR-C crates/common/tests/cloudwatch_app_alarms_wiring.rs::test_emf_metric_selectors_name_count_is_pinned at 115; emf_selector_producer_guard and dashboard_live_lane_visibility_guard green; emf_loss_series_seeding_sweep and loss_counter_visibility_guard green unchanged","CI clippy -D warnings enforces the #[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)] on windowed_quantile_bucket and WsLagP99Window::step (panic=abort safety on tv-telemetry)","Post-deploy evidence (not CI): get-metric-statistics for tv_dhan_ws_lag_p99_ms / tv_hot_path_ring_dwell_p99_ms (host=tickvault-prod) shows positive values in session and -1 outside; no other selected metric goes missing the first session (proves the sentinel path); describe-alarms shows tv-prod-errcode-ws-gap-03-kernel-rx-backlog OK with empty OKActions"]

## Guards to update
["crates/common/tests/cloudwatch_app_alarms_wiring.rs::test_emf_metric_selectors_name_count_is_pinned: 113 to 115, with a dated comment and the message string updated (PR-C)","crates/common/tests/cloudwatch_app_alarms_wiring.rs::every_ws_gap_03_filter_carries_a_source_discriminator: floor patterns.len() >= 4 raised to 5, with a dated comment (PR-B)","crates/common/tests/live_lane_paging_lockstep_guard.rs: ADD the_kernel_rx_backlog_page_exists_without_a_false_recovery (PR-B)","crates/aws-lambdas/src/telegram_webhook.rs ALARM_PHRASES: length 126 to 127, plus the new entry (PR-B)","crates/aws-lambdas/tests/alarm_phrase_coverage_guard.rs::guard_self_test_parser_bites: key floor 21 to 24 (PR-B)","crates/common/tests/error_code_paging_filter_drift_guard.rs and crates/storage/tests/alarm_description_length_guard.rs: no code change; desc must stay under 1024 characters including the suffix","crates/common/tests/error_code_alarm_coverage_guard.rs, breaching_alarms_are_gated_guard.rs, alarm_metric_has_a_route_guard.rs: no change; run them","crates/storage/tests/emf_selector_producer_guard.rs: no change; satisfied by the production const literals in hot_path_telemetry.rs and dhan_feed_stack.rs, which must sit outside #[cfg(test)]","crates/common/tests/dashboard_live_lane_visibility_guard.rs: no change; satisfied by the two dashboard.tf entries using local.dash_namespace","crates/storage/tests/emf_loss_series_seeding_sweep.rs and crates/common/tests/loss_counter_visibility_guard.rs: no change (the names contain no loss word or suffix)","NEW crates/storage/tests/dhat_hot_path_telemetry.rs (this file does not exist on origin/main; it needs its own dhat allocator)",".claude/hooks/per-item-guarantee-check.sh: plan items R3-10b and LP-1 must carry or cross-reference the 15-row + 7-row matrix"]

## Residual risks
["Authorization (gating): noise-lock §2.10 says a general go-ahead does not authorize a page. The quote 'See do everything whatever is recommended dude okay?' authorizes PR-B and PR-C only if the recommendation it answered named this page and these gauges with their price. If it did not, post a decision card first. 'Go ahead with whatever you want dude' appears only in script text and is unverified.","WS lag p99 includes Dhan's whole-second LTT rounding (up to +999 ms on every sample). It detects shifts above about 1.3 s, not sub-second transit. The dashboard label must say so or the number reads as alarming.","The values are bucket upper bounds: ring dwell overstates by less than 2×, WS lag by about 25% per step. Do not read either as exact.","The -1 sentinel shows on the dashboard outside the session and in thin minutes. Any future alarm on these gauges must exclude it. A CloudWatch Maximum over 5 minutes in session is dominated by real values (Assumed).","Scrape phase is not aligned to the window, so a window can be read twice or skipped. The figure is not guaranteed for every minute.","The lane-wide p99 can hide one slow socket that carries less than 1% of samples. The per-minute MAX gauges remain the per-socket worst-case signal.","Port-443 matching includes AWS SDK and SNS sockets. A sustained large download on 443 inside the session could page. Raw-frame upload runs outside 09:00–15:40 unless disk pressure forces it (Assumed from the 45e-1 row).","Period 3600 means a second genuine episode within an hour of the first does not re-page (the alarm is still in ALARM, or OK with a quiet transition). This is accepted for flap control, as for RISK-GAP-03.","Cost uses Assumed ap-south-1 list prices: $0.30 per custom metric-month prorated hourly, $0.10 per standard alarm-month. The -1 sentinel publishes in every box-hour, so budget the full $0.60 for the two names, about $0.70–$1.00 per month in total. The October ceiling is $150; the current budget position was not read live.","The hook runs app code on the tv-telemetry thread with panic=abort. Panic-freedom rests on the clippy deny attributes and saturating arithmetic; the mutex is uncontended, and lock failure is handled with if-let."]

