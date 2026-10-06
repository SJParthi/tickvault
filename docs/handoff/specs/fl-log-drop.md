# FINAL SPEC log-drop (ship=true)

## Summary
The defect is real, but the original ask blames the wrong sink. Verified: stdout is not registered in production. config/base.toml:475 sets log_to_stdout = false, and nothing in config/ or deploy/ overrides it. The false pages come from three other sinks:
- errors_log, which is always registered (main.rs ~1855, init_lossy_non_blocking).
- candles and live_ticks, registered at main.rs ~1945 with register_log_drop_counter(cat.prefix()). Both are TRACE-level sinks.

Verified: none of those three files is shipped. collect_list in deploy/aws/cloudwatch-agent.json holds only errors.jsonl.2* and app.2*, and the category files live in subdirectories that the app.2* glob does not reach. Yet the alarm tv-<env>-log-lines-dropped (data-at-risk-alarms.tf:201) reads tv_log_lines_dropped_total as a Sum on the host dimension only. So a drop in a file nobody reads in CloudWatch pages anyway. That also buries the one case the page exists for: a drop on app_log or errors_jsonl, which can hide a coded ERROR from its filter alarm.

Fix: use the house metrics-log filter with a label slice (the seal-drop / §2.10 route). One filter over /tickvault/<env>/metrics matches $.sink = app_log or errors_jsonl and writes a derived metric, tv_log_lines_dropped_shipped_total. The existing alarm switches to that metric. Its name, threshold, period, notBreaching, empty ok_actions, host-only dimension and Telegram line stay the same. No Rust production change is needed, because the boot seed already covers exactly the two sliced labels.

Review points accepted:
- Ship as three serial PRs so the page is never blind:
  - PR-1 adds the filter only.
  - The operator then checks that the filter matches.
  - PR-2 switches the alarm.
  - PR-3 removes the raw name from the EMF selector, only after PR-2 is applied.
- The premise that the metrics log carries the label as a top-level sink field is NOT live-verified for any house slice. If it is wrong, the filter never matches and the page stays green forever. That failure is silent, not loud as the design said, which is why PR-2 waits on a measured match.
- Add a lockstep test tying the label each writer registers under to the sliced value. Otherwise renaming a writer's label would kill the page silently.
- The boot-seed label check needs its own parser for the `for sink in [..]` loop form, because zero_registered_names() reads names only.
- Rewrite the file header too, not just section 4.
- If PR-3 is skipped, dashboard_live_lane_visibility_guard needs a DELIBERATELY_NOT_CHARTED row.
- Do NOT seed the derived name in Rust, unlike seal-drop at main.rs:1497.
- PR-3 touches crates/common/tests, so it escalates to a workspace test run. The design's 'scoped runner decides' was wrong.
- Visibility loss from PR-3 is smaller than the design stated: the raw per-sink series stays in the metrics log group and remains queryable with Logs Insights (Assumed).
- The approval row must state plainly that this narrows an existing page (fewer pages, same alarm) and that it rests on the owner's general go-ahead answering this recommendation.

Review points changed:
- Reviewer 1 proposed switching the alarm in PR-1 while keeping the raw selector. Rejected, because if the label premise fails the page goes blind; it is replaced by the filter-only PR-1 plus a verification step.
- The design's fallback, filtering the warn line, stays rejected: it is self-referential, since the warn goes through the full sink.

Rejected outright: none. Every review point was either accepted or refined as above.

ORDERED EDITS. There are three serial PRs, each merged and verified before the next opens. No PR changes crates/*/src, so plan-gate.sh is not engaged.

=== PR-1: noise-lock row, the filter, the guards. The alarm is not touched yet ===

1. docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md: add a new subsection directly after §2.9 (heading at ~line 4440; table row at line 4471).
   - Heading: "§2.9-i — 2026-10-06: the log-lines-dropped page reads only the two log files shipped to CloudWatch".
   - Quote the owner verbatim, as two separate quotes: "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?".
   - State that these quotes are general. They are accepted here because this change is a NARROWING: one existing alarm keeps its name, threshold, period, notBreaching, empty ok_actions, host-only dimension and Telegram line, and only its input shrinks to the shipped sinks. It never adds a page. Reverting is one field (metric_name).
   - Annotate the §2.9 table row (house convention: annotate, never silently rewrite). The cell `tv_log_lines_dropped_total` becomes `tv_log_lines_dropped_shipped_total`: a metrics-log filter over `tv_log_lines_dropped_total` with sink = app_log or errors_jsonl.
   - WHY: errors.log, candles/, live_ticks/ and stdout are not in cloudwatch-agent.json collect_list, so their drops paged falsely. errors.log is lossy by design anyway (it is reset to 0 at its size cap, main.rs ~2046). Every box-local drop still logs a warn naming the sink into the shipped app log, and its per-sink series stays in /tickvault/<env>/metrics for Logs Insights.
   - COST: net $0 after PR-3 (-1 EMF name, +1 filter-derived metric; filters are free). +$0.30/month during the PR-1 to PR-3 interval.
   - NOT claimed: (a) that the agent writes the sink label as a top-level field. PR-2 is gated on a measured match. (b) that counters arrive as per-scrape deltas (the §2.10 residual).
   - Residual, not a regression: drops before the agent's first scrape at boot are absorbed into the dropped baseline sample.
   - REJECT rows:
     - widening the slice to a sink whose file is not shipped, without a dated row;
     - pointing the alarm back at the raw all-sink sum;
     - adding a sink dimension or splitting into per-sink alarms;
     - adding ok_actions;
     - dropping the boot-zero seed for either sliced label;
     - filtering the warn line instead of the scrape;
     - seeding the derived name in Rust;
     - removing the raw name from the selector before the alarm reads the derived metric and a match is verified.

2. .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md (summary stub), §2.9 bullet: append "(§2.9-i, 2026-10-06: log-lines-dropped reads only the shipped sinks app_log + errors_jsonl, through a metrics-log filter into tv_log_lines_dropped_shipped_total)". Add one REJECT headline: "(§2.9-i) widens the log-drop slice to an unshipped sink or points the alarm back at the raw sum".

3. deploy/aws/terraform/data-at-risk-alarms.tf: add this new resource directly above the log_lines_dropped alarm, with a comment citing §2.9-i and the seal-drop naming rule (a distinct derived name, so a later EMF selection of the raw name can never be counted twice):
   resource "aws_cloudwatch_log_metric_filter" "log_lines_dropped_shipped" {
     name           = "tv-${var.environment}-log-lines-dropped-shipped"
     log_group_name = "/tickvault/${var.environment}/metrics"
     pattern        = "{ $.tv_log_lines_dropped_total = * && ($.sink = \"app_log\" || $.sink = \"errors_jsonl\") }"
     metric_transformation {
       name       = "tv_log_lines_dropped_shipped_total"
       namespace  = local.app_namespace
       value      = "$.tv_log_lines_dropped_total"
       dimensions = { host = "$.host" }
     }
   }
   - Do NOT set default_value: AWS rejects it together with dimensions.
   - The `&& ( || )` JSON form already runs in the rebalance-swaps-refused slice in loss-group-alarms.tf (Verified).
   - Filter budget on the metrics log group: about 55 (loss-group for_each) plus about 6, plus 1, against the limit of 100. Count the exact figure in the PR body (Assumed ~62).

4. NEW crates/app/tests/log_drop_alarm_shipped_sinks_guard.rs. It reads origin files through repo_root() and uses tickvault_app::observability::{APP_LOG_PREFIX, ERRORS_JSONL_PREFIX, CATEGORY_CANDLES_PREFIX, CATEGORY_LIVE_TICKS_PREFIX}. It has a const SHIP_MAP: &[(&str /*file prefix*/, &str /*sink label*/)] = &[(APP_LOG_PREFIX, "app_log"), (ERRORS_JSONL_PREFIX, "errors_jsonl")] and a const SLICED: [&str; 2] = ["app_log", "errors_jsonl"]. The tests are listed in `tests`. PR-1 carries all of them except the_log_drop_alarm_reads_the_shipped_slice, which lands in PR-2.

5. crates/app/tests/alarmed_counters_are_seeded_guard.rs:
   - Add const DERIVED_SLICE_FILTER_METRICS: &[(&str, &str, &str, &[&str])] = &[("tv_log_lines_dropped_shipped_total", "tv_log_lines_dropped_total", "sink", &["app_log", "errors_jsonl"])], with a doc comment.
   - Why not KNOWN_NO_PRODUCER: that list means 'an alarm that can never fire', which is a finding. This metric is a live derived slice whose SOURCE must be seeded per sliced label, and the const carries the data for that check.
   - Why not DERIVED_LOSS_GROUP_METRICS: the_loss_group_alarms_read_the_metrics_the_filters_write asserts set equality with loss-group-alarms.tf.
   - Add a `|| DERIVED_SLICE_FILTER_METRICS.iter().any(|(d, ..)| d == name)` arm to the skip list in every_alarmed_counter_is_registered_at_boot (~line 502).
   - Add the test every_derived_slice_filter_source_is_registered_for_each_sliced_label (see tests).
   - Landing it in PR-1 is harmless, because no alarm reads the name yet.

6. aws-budget.md: add "COST NOTE 2026-10-06 (log-drop slice)". +1 derived custom metric (+$0.30/month). After PR-3, -1 EMF name (-$0.30), so net $0. If PR-3 is skipped, the +$0.30 stays.

GATE between PR-1 and PR-2. The operator or CI runs this with AWS credentials. Not done in-sandbox; Unknown until run.
   (a) Logs Insights on /tickvault/prod/metrics: `filter ispresent(tv_log_lines_dropped_total) | stats count(*) by sink`. Expected: rows for app_log and errors_jsonl, plus errors_log, candles and live_ticks.
   (b) aws cloudwatch get-metric-statistics, namespace = local.app_namespace, metric tv_log_lines_dropped_shipped_total, dimension host, statistic SampleCount, over a window while the box runs. It must be > 0. Healthy datapoints are 0-valued about every 60 s, so a nonzero SampleCount proves the slice matches.
   If (a) shows no sink field, STOP. Fall back to the Rust second-counter route (design alternative b), which needs a plan and a src change. Do not proceed to PR-2.

=== PR-2: switch the alarm (terraform plus one test). Apply outside 09:00-15:40 IST ===

7. data-at-risk-alarms.tf, aws_cloudwatch_metric_alarm.log_lines_dropped: change metric_name to "tv_log_lines_dropped_shipped_total". Change nothing else.
   - Add one sentence to alarm_description (stays under the 900-character warn band; the current text is about 470): "Counts only the two log files shipped to CloudWatch (app log, errors.jsonl); drops on box-local files (errors.log, candles, live_ticks, stdout) are logged as a warn in the app log and do not page."
   - Rewrite section 4's comment (~lines 193-200). Name the shipped sinks, the filter, §2.9-i, and why box-local sinks do not page.
   - Header (lines 8-32): add a dated note that tv_log_lines_dropped_total's page reads the sink slice through the metrics-log filter, not the EMF host-sum. The 'EMF folds every label' line no longer describes this counter's page.

8. Add the_log_drop_alarm_reads_the_shipped_slice_not_the_raw_counter to the new guard file.

9. Leave crates/aws-lambdas/src/telegram_webhook.rs (~line 336) unchanged: the alarm name and the plain-language line are still true.

POST-APPLY CHECK: describe-alarms shows MetricName = tv_log_lines_dropped_shipped_total, and the alarm is in OK or INSUFFICIENT_DATA reading notBreaching.

=== PR-3: remove the raw name from the EMF selector. Optional but recommended ===

10. deploy/aws/cloudwatch-agent.json: delete `|tv_log_lines_dropped_total` from the metric_selectors alternation (113 to 112 names). The selector exists only here (guard user_data_carries_no_second_copy_of_the_selector), so the user-data byte budget does not bind.

11. crates/common/tests/cloudwatch_app_alarms_wiring.rs::test_emf_metric_selectors_name_count_is_pinned: change 113 to 112, with a dated 2026-10-06 comment and a message line naming §2.9-i.

12. deploy/aws/EMF-METRIC-SELECTOR-NOTES.md: add a dated 2026-10-06 note.
   - tv_log_lines_dropped_total leaves the selector.
   - The page reads the derived sink slice from /tickvault/<env>/metrics.
   - The raw per-sink series stays in that log group (queryable with Logs Insights), and stays LOGGED through the warn! in publish_log_drop_counters.

13. data-at-risk-alarms.tf header: the 'They now ship (cloudwatch-agent.json)' sentence gets an annotation that this counter now ships through the metrics log, not EMF.

14. If PR-3 is skipped: add a row for tv_log_lines_dropped_total to DELIBERATELY_NOT_CHARTED in crates/common/tests/dashboard_live_lane_visibility_guard.rs in PR-2 instead. Reason: 'all-sink chart; the page reads the shipped slice'. Otherwise every_published_metric_is_visible_somewhere goes red once the alarm moves (Verified by reviewer: the name is on no dashboard).

RUST PRODUCTION: none.
- publish_log_drop_counters (observability.rs:504) is unchanged: O(sinks <= 8) every 10 s on its own task, with a warn! about 5 lines below the counter! (keeps the raw name LOGGED for loss_counter_visibility_guard, within LOG_LOOKAHEAD_LINES = 12).
- The main.rs:1545 seed loop over ["app_log", "errors_jsonl"] already satisfies the §2.9 boot-zero REJECT row for both sliced labels.

NOT TOUCHED: websocket-connection-scope-lock.md (no WebSocket behaviour), every other alarm, ErrorCode, the hot path.

## Tests
["NEW log_drop_alarm_shipped_sinks_guard::the_log_drop_filter_slices_exactly_the_sinks_the_agent_ships (PR-1). Parse every file_path under /opt/tickvault/data/logs/machine/ in deploy/aws/cloudwatch-agent.json. Strip the directory and the trailing .2* glob to get a prefix, and map it through SHIP_MAP. Fail on any shipped file with no mapping, so shipping a new log file forces the slice to widen. Parse every `$.sink = \\\"..\\\"` value from the pattern of the log_lines_dropped_shipped filter and assert the two sets are equal. Bite-test: adding `|| $.sink = \\\"stdout\\\"` to the pattern must turn it red.","NEW ...::unshipped_sinks_never_feed_the_page (PR-1). Assert that \"stdout\", \"errors_log\", CATEGORY_CANDLES_PREFIX and CATEGORY_LIVE_TICKS_PREFIX are absent from the slice. Also assert that no collect_list file_path contains CATEGORY_CANDLES_DIR or CATEGORY_LIVE_TICKS_DIR.","NEW ...::each_sliced_label_is_the_label_its_shipped_writer_registers_under (PR-1). In crates/app/src/main.rs, with comments stripped: the first register_log_drop_counter( call within 12 lines after `init_app_log_appender(` must have the literal \"app_log\", and the first one after `init_errors_jsonl_appender(` must have \"errors_jsonl\". Each sliced label must appear exactly once as a register_log_drop_counter or init_lossy_non_blocking literal across main.rs. Bite-test: renaming the app.log register label to \"app\" must turn it red. This closes the silent-dead-page case that both reviewers raised.","NEW ...::the_log_drop_filter_shape_is_pinned (PR-1). Inside the aws_cloudwatch_log_metric_filter.log_lines_dropped_shipped block: log_group_name is /tickvault/${var.environment}/metrics, metric_transformation name is tv_log_lines_dropped_shipped_total (never tv_log_lines_dropped_total), value is $.tv_log_lines_dropped_total, dimensions are host only, and there is no default_value.","NEW ...::the_slice_labels_are_seeded_at_boot (PR-1). Use a dedicated parser in main.rs that finds a `for sink in [` array whose loop body (within 3 lines) contains counter!(\"tv_log_lines_dropped_total\", \"sink\" => sink).increment(0), and also accepts the literal `\\\"sink\\\" => \\\"x\\\"` form. Every SLICED value must be seeded. It must fail with a non-vacuity message if the parser finds zero seeds.","NEW ...::the_log_drop_alarm_reads_the_shipped_slice_not_the_raw_counter (PR-2). Inside the aws_cloudwatch_metric_alarm.log_lines_dropped block: metric_name equals the filter's metric_transformation name and equals tv_log_lines_dropped_shipped_total. Also pin the §2.9 shape: threshold 1, evaluation_periods 1, datapoints_to_alarm 1, period 300, statistic Sum, dimensions = local.app_dimensions, treat_missing_data notBreaching, ok_actions = [], alarm_actions = local.app_alarm_actions. Bite-test: reverting metric_name must turn it red.","NEW alarmed_counters_are_seeded_guard::every_derived_slice_filter_source_is_registered_for_each_sliced_label (PR-1). For each DERIVED_SLICE_FILTER_METRICS entry: the source is in zero_registered_names(), and main.rs seeds the source with label => each value. Reuse the loop-form parser, or a shared helper module in crates/app/tests/common if one exists. Also assert that the derived name appears as a metric_transformation name in some .tf file, so a stale entry fails.","UPDATED alarmed_counters_are_seeded_guard::every_alarmed_counter_is_registered_at_boot (PR-1/2): the skip list gains the DERIVED_SLICE_FILTER_METRICS arm.","UPDATED cloudwatch_app_alarms_wiring::test_emf_metric_selectors_name_count_is_pinned 113 -> 112 (PR-3 only).","RUN, with no edits expected (Verified by reading, results Unknown until run): cargo test -p tickvault-common --test alarm_metric_has_a_route_guard --test loss_counter_visibility_guard --test cloudwatch_app_alarms_wiring --test dashboard_live_lane_visibility_guard; cargo test -p tickvault-storage --test emf_selector_producer_guard --test cw_agent_selector_lockstep_guard --test emf_loss_series_seeding_sweep --test loss_writer_metrics_are_shipped_guard; cargo test -p tickvault-app --test alarmed_counters_are_seeded_guard --test log_drop_alarm_shipped_sinks_guard; cargo test -p tickvault-aws-lambdas telegram_webhook; cargo test -p tickvault-app observability::tests::log_sink_buffer_bound_is_pinned.","PR-3 edits crates/common/tests, so per testing-scope.md run cargo test --workspace. PR-1 and PR-2 run the scoped app and common test targets above.","terraform fmt -check and terraform validate on deploy/aws/terraform in PR-1 and PR-2 (pattern quoting).","Bite-tests for each new guard: show the red run in the PR body, then revert."]

## Guards to update
["crates/app/tests/alarmed_counters_are_seeded_guard.rs: add a DERIVED_SLICE_FILTER_METRICS const and an arm in every_alarmed_counter_is_registered_at_boot's skip list, plus the new test every_derived_slice_filter_source_is_registered_for_each_sliced_label. Do not use KNOWN_NO_PRODUCER: it means an alarm that can never fire. Do not use DERIVED_LOSS_GROUP_METRICS: it is pinned equal to the loss-group alarms.","NEW crates/app/tests/log_drop_alarm_shipped_sinks_guard.rs: six tests (filter slice equals the shipped files, unshipped sinks excluded, writer register labels equal the sliced labels, filter shape, boot seed per label, alarm reads the derived metric with the §2.9 shape).","crates/common/tests/cloudwatch_app_alarms_wiring.rs::test_emf_metric_selectors_name_count_is_pinned: change 113 to 112 (PR-3 only).","crates/common/tests/dashboard_live_lane_visibility_guard.rs: no change if PR-3 ships. If PR-3 is skipped, add a DELIBERATELY_NOT_CHARTED row for tv_log_lines_dropped_total in PR-2.","Run with no edit expected: crates/common/tests/alarm_metric_has_a_route_guard.rs (filter metric_transformation names count as routes), crates/common/tests/loss_counter_visibility_guard.rs (raw name stays LOGGED through the warn!), crates/storage/tests/{emf_selector_producer_guard, cw_agent_selector_lockstep_guard, emf_loss_series_seeding_sweep, loss_writer_metrics_are_shipped_guard}.rs, crates/aws-lambdas/src/telegram_webhook.rs tests, crates/app/src/observability.rs::tests::log_sink_buffer_bound_is_pinned."]

## Residual risks
["Assumed and NOT live-verified for any house slice (seal-drop $.kind, loss-group $.reason/$.stage): that the CloudWatch agent writes the Prometheus label as a top-level `sink` field in /tickvault/<env>/metrics. If it does not, the filter never matches and a notBreaching alarm stays green forever, which is silent. Mitigated by the PR-1 filter-only stage and the Logs Insights plus SampleCount gate before PR-2. If the gate fails, fall back to a Rust-side second counter, which needs a plan and a src edit.","Assumed (the §2.10 residual): counters reach the metrics log as per-scrape deltas. If they were cumulative, the page would repeat every period after the first real drop until restart. That is loud, not silent.","Pre-existing and not a regression: publish_log_drop_counters can call .absolute(N) about 10 s after boot, before the agent's first scrape at 60 s. Drops in that first interval, the most log-heavy moment, become the dropped baseline sample and never page.","PR-3 removes the CloudWatch metric chart of drops on box-local files. They stay queryable per sink in /tickvault/<env>/metrics with Logs Insights (Assumed), and each produces a warn naming the sink in the shipped app log unless app_log itself is the sink dropping.","Approval: the 2026-10-06 owner quotes are general go-aheads. §2.10 records that a general go-ahead did not name that change and a card was posted. The §2.9-i row accepts them only because this NARROWS one existing page and adds none. A reviewer may still ask for a card. Reverting is one field.","Deploy windows. PR-2 moves the alarm to a metric with a short history: the first period may read notBreaching, so a real app_log or errors_jsonl drop during the apply could be missed once. Apply outside 09:00-15:40 IST. PR-3 only after PR-2's apply is confirmed, so the agent never stops publishing a metric the alarm still reads. deploy-aws.yml and terraform-apply.yml are not ordered with respect to each other, which is why the stages are split.","If log_to_stdout is ever turned on, nothing changes, because stdout is not shipped. If another file starts shipping (for example errors.log), the lockstep guard goes red until the slice and a dated row are added.","Filter count on the metrics log group is about 62 of the 100 limit (Assumed). Count it exactly in the PR-1 body. A future guard on that limit is a separate follow-up."]

