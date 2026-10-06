# FINAL SPEC wal-replay-log (ship=true)

## Summary
The defect is real, and wider than the task stated. All findings are against origin/main crates/app/src/main.rs.

Why the lines are lost (Verified): async_main installs its only tracing subscriber at about L1972-1980. Nothing installs a log backend or a default subscriber before that, so every log line before then reaches no sink: not stdout, app.log, errors.jsonl, or the CloudWatch filters.

The two reported sites (Verified):
- sandbox_window_boot (L740-749, OMS-GAP-06) is lost. Only std's bare 'Error: ...' reaches stderr, with no code.
- boot_replay_failed (L1288-1296, WS-SPILL-02) is lost, and boot carries on silently. It is worse than reported: ws_wal_replay_refused stays false, so the else arm at L3243-3256 runs confirm_replayed and archives earlier boots' staged segments unread. That arm's own comment admits it.

Other lines lost the same way (Verified):
- RESILIENCE-01 at L1127, then exit(1).
- The WsFrameSpill init failure at L1369, then exit(1). It also has no code.
- The telemetry spawn failure at L1066. It is mis-coded BOOT-02, which would raise a false 'BOOT IS BLOCKED' page once visible.
- Feed-overlay, clock and calendar warns.
- validate's LOUD-EXPIRY warn.
- lock_wal_dir's WS-SPILL-01 lines and replay_all_fenced's WS-SPILL-02 lines.
- Two errors swallowed with 'let _ = err'.
- All five WorkerGuards are Box::leak'ed, so the sinks are never flushed on exit.

Final design: fix the whole ordering instead of carrying one summary.
1. Install the metrics recorder first, right after config extract. Its Result is kept and logged once the subscriber exists.
2. Install the subscriber and the panic hook before validate and before every boot step that logs.
3. Flush the log sinks before every exit that happens after the install.
4. Code the newly visible lines: reuse WS-SPILL-01 for the WAL init failure; add HOT-PATH-04 for the telemetry spawn failure; add BOOT-04 for a validate or metrics-init failure.

Review points folded in (all Verified):
- (R1, R2) Fatal: ErrorCoalescer::new caches metrics::counter! at construction, so installing the subscriber before init_metrics would kill tv_log_lines_coalesced_total for the life of the process. init_metrics now runs first, and guard (c) is inverted.
- error_code_tag_guard asserts UNCODED_ERROR_BUDGET == 10 exactly. Coding L1369 needs the budget lowered to 9, plus doc edits.
- The design's whole-file guard (h) fails on the uncoded panic hook (L2391) and the API server error (L4841). Dropped: the exact tag-guard budget already pins the coding.
- error_code_alarm_coverage_guard rejects a new High code with no filter. HOT-PATH-04 and BOOT-04 are Medium.
- triage_rules_full_coverage_guard needs a rule for each new code.
- The '#[cfg(test)] mod tests' block (L5484-5956) contains a second set_hook, and PRODUCTION fns follow it, so guards must blank exactly that block rather than cut there.
- The simpler placement, between extract and validate, removes the design's config.rs change (expired_safety_gates) and the today_ist ordering bug. validate reads no logging or observability config (Verified).
- Flush guards in parallel under one deadline, so a slow stdout cannot starve errors.jsonl.
- The WS-SPILL-01 alarm text must say the boot source means the app is NOT running.
- Correct the tag-guard doc sentence that claims the boot replay line already pages; it never could.
- Say plainly that fn main's flush does not cover tasks that log after it runs.

Review points dropped:
- Widening the exit guard to crates/core ip_monitor.rs:410. It has no app call site (reviewer-verified), so it is recorded as latent.
- Detecting AddrInUse to label a metrics bind failure RESILIENCE-01. The exporter error is a String, so this would mean fragile string matching. BOOT-04 with a hint in the message is used instead.
- A per-guard leak-on-poison mutex is kept, but simplified.

Out of scope (Recommendation, needs its own PR): making replay Err keep replaying/ instead of archiving unread.

Hot path untouched. Ship.

Ordered edits. Line numbers are origin/main approximations; every site is also named by its symbol.

0. Plan file. .claude/plans/active-plan-boot-log-order.md, Status APPROVED. It names crates app and common, and has all six sections: Design, Edge Cases, Failure Modes, Test Plan, Rollback, Observability. Approval quote, operator 2026-10-06: "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?".

1. crates/common/src/error_code.rs: two new variants, wired through every match (code_str, severity, runbook_path, all()).
- HotPath04TelemetrySpawnFailed => "HOT-PATH-04", Severity::Medium, runbook docs/error-runbooks/hot-path-stall-error-codes.md. Doc: the hot-path telemetry publisher thread failed to spawn; the HOT-PATH-STALL-01 page is blind for this process.
- Boot04BootStepFailed => "BOOT-04", Severity::Medium, runbook docs/error-runbooks/wave-2-error-codes.md. Doc: config validation or the metrics exporter failed at boot; the process returns Err and systemd restarts it.
- Both are covered by the existing HOT-PATH- and BOOT- prefix arms in the triage-safe classifier (~L2218).
- Medium keeps both out of error_code_alarm_coverage_guard, which checks only High and Critical. CloudWatch matches the exact code (Verified, error-code-alarms.tf patterns), so BOOT-04 cannot match the BOOT-02 filter.

2. .claude/triage/error-rules.yaml: add two silence rules shaped like hot-path-03-reader-pin-not-applied-silence: hot-path-04-telemetry-spawn-failed-silence and boot-04-boot-step-failed-silence (confidence 1.0, cooldown_secs 86400, each with its runbook_path).

3. crates/app/src/observability.rs (lib):
- pub struct SinkInitFailure { pub sink: &'static str, pub error: String }.
- pub fn report_sink_init_failures(f: &[SinkInitFailure]): one warn! per entry, 'continuing without the <sink> log sink', with fields sink and error.
- static LOG_SINK_GUARDS: std::sync::Mutex<Vec<WorkerGuard>>.
- pub fn retain_log_guard(g: WorkerGuard): push; on a poisoned lock, std::mem::forget(g). A guard is never dropped early.
- pub const LOG_SINK_FLUSH_BUDGET: Duration = Duration::from_millis(1_000).
- pub enum LogFlushOutcome { Flushed, TimedOut, NothingToFlush }.
- fn flush_guards(slot: &Mutex<Vec<WorkerGuard>>, budget: Duration) -> LogFlushOutcome:
  - Take the Vec; if it is empty, return NothingToFlush.
  - For each guard: put it in an Arc<Mutex<Option<WorkerGuard>>>, spawn a std thread named tv-log-flush that takes it, drops it, and sends on an mpsc channel.
  - If a spawn fails, take the guard back from the caller's Arc clone and std::mem::forget it, so a drop never runs on the caller thread.
  - Wait with recv_timeout against one shared deadline (Instant::now() + budget). All received: Flushed. Otherwise: TimedOut.
  - The guards drop in parallel, so a slow stdout cannot starve errors.jsonl.
- pub fn flush_log_sinks(budget: Duration) -> LogFlushOutcome wraps the static.
- pub fn exit_after_log_flush(code: i32) -> ! does flush_log_sinks(LOG_SINK_FLUSH_BUDGET), then std::process::exit(code).

4. crates/app/src/main.rs: new free fn install_tracing_subscriber(config: &ApplicationConfig) -> anyhow::Result<(Option<opentelemetry_sdk::trace::SdkTracerProvider>, Vec<SinkInitFailure>)>.
- DEFINE IT TEXTUALLY AFTER async_main and before the '#[cfg(test)] mod tests' block, so offset guards see the call site first.
- Body: the existing block from 'let log_filter = config.logging.level.as_str()' (~L1750) through '.init();' (~L1980), moved verbatim. It depends only on config, which was Verified by an identifier diff against L651-1746 bindings.
- Changes inside the body:
  - The L1913 tracing::warn becomes push(SinkInitFailure{sink:"errors_jsonl", ...}).
  - The file_log_layer 'let _ = err' pushes "app_log".
  - create_error_log_writer() returning None pushes "errors_log".
  - The category Err pushes cat.prefix().
  - Any init_tracing failure currently written to stderr also pushes "otel" (Assumed shape; keep the stderr line).
  - Each of the five 'Box::leak(Box::new(guard))' becomes tickvault_app::observability::retain_log_guard(guard).
  - No log macro may appear in the fn.

5. main.rs: new free fn install_panic_hook(), also defined after async_main and before the test module.
- Body: the block at ~L2352-2397 ('let default_panic_hook = std::panic::take_hook(); std::panic::set_hook(...)'), moved verbatim. It captures only statics and constants (Verified).
- It must stay the FIRST 'std::panic::set_hook(' in the file. The second one is inside mod tests at L5814, and shutdown_budget_fits_systemd_guard uses find().

6. async_main new order, immediately after the '.extract().context(...)?' that ends at ~L725:
- (a) let metrics_init = observability::init_metrics(&config.observability);
  - if metrics_init.is_ok() { tickvault_core::parser::prewarm_dispatcher_counters(); }
  - MOVE the whole comment block above the old L1053 call ('MOVED HERE 2026-08-26 ...', which contains 'no-op') with it.
  - Add a dated 2026-10-06 paragraph: 'init_metrics now runs before the subscriber install because log_coalescer::ErrorCoalescer::new caches a metrics::Counter at construction; a pre-recorder handle is a permanent no-op'.
- (b) let (otel_provider, sink_failures) = install_tracing_subscriber(&config)?;
- (c) tickvault_app::observability::report_sink_init_failures(&sink_failures);
- (d) install_panic_hook();
- (e) if let Err(err) = metrics_init:
  - error!(code = ErrorCode::Boot04BootStepFailed.code_str(), source = "metrics_exporter_init", error = %format!("{err:#}"), "BOOT: the Prometheus metrics exporter failed to start, most often because another tickvault process holds the metrics port; refusing to boot");
  - return Err(err.context("failed to initialize Prometheus metrics")).
- (f) if let Err(err) = config.validate():
  - error!(code = ErrorCode::Boot04BootStepFailed.code_str(), source = "config_validate", error = %format!("{err:#}"), "BOOT: configuration validation failed; refusing to boot");
  - return Err(err.context("configuration validation failed")).
  - validate's LOUD-EXPIRY warn now reaches the sinks unchanged. No config.rs edit.
- Delete the old init_metrics and prewarm site (~L1053-1060) and the old logging block (~L1750-1980). otel_provider keeps flowing to run_process_runloop unchanged.
- hot_path_telemetry::spawn_publisher stays where it is, after both installs.
- Everything still before the install: the host-tuning CLI (L593), trading-day gate (L116), holiday gate and ensure-questdb (L699, L706), the rustls install, config extract, init_metrics and prewarm. None of these use tracing macros (Verified for main.rs; holiday_gate, ensure_questdb and host_tuning write to stderr).

7. main.rs, sites that become visible:
- Telemetry spawn failure (~L1066): code becomes ErrorCode::HotPath04TelemetrySpawnFailed.code_str(); add source = "telemetry_spawn". The text is unchanged.
- WsFrameSpill init failure (~L1369):
  - add code = ErrorCode::WsSpill01WriterRespawn.code_str() and source = "boot_writer_init_failed";
  - replace std::process::exit(1) (~L1383) with tickvault_app::observability::exit_after_log_flush(1);
  - edit the comment: the ERROR now pages through the existing ws-spill-01 filter.
- Dual-instance (~L1135): exit_after_log_flush(1).
- Second Ctrl+C task (~L5166): tokio::task::block_in_place(|| tickvault_app::observability::exit_after_log_flush(1)). It blocks one worker for at most 1 s at forced exit, which is acceptable.
- boot_replay_failed (~L1288):
  - keep code WS-SPILL-02 and its source;
  - add field staged_segments_archived_unread = true;
  - message: 'STAGE-C: WAL replay failed — continuing boot with fresh WAL; segments earlier boots staged will be archived UNREAD (raw bytes kept under archive/, not refolded, frame count lost)'. Do not call the archive harmless.
- sandbox_window_boot: unchanged. Its Err now runs after the install and is flushed in fn main.

8. fn main:
- replace 'runtime.block_on(async_main())' with: 'let result = runtime.block_on(async_main()); let _ = tickvault_app::observability::flush_log_sinks(tickvault_app::observability::LOG_SINK_FLUSH_BUDGET); result'.
- Comment it: tasks still running after block_on returns can log after the flush, and those lines can still be lost (no worse than today).

9. crates/common/tests/error_code_tag_guard.rs:
- UNCODED_ERROR_BUDGET 10 -> 9.
- Edit the doc list: the WAL init failure is CODED on 2026-10-06 as WS-SPILL-01 source=boot_writer_init_failed. This reverses the earlier note that it needed its own code; reason: the existing alarm's triage (disk full or permissions) is the right one, and the site exists to page. The remaining nine are unchanged.
- Add a dated correction: the sentence saying the boot WAL-replay WS-SPILL-02 line 'now reaches the existing frame-drop alarm' was false until 2026-10-06, because it ran before the subscriber existed.

10. crates/app/tests/counter_preregistration_after_recorder_guard.rs: add "ErrorCoalescer::new(" to the needle list in the_sibling_post_install_registrations_stay_post_install.

11. deploy/aws/terraform/error-code-alarms.tf, desc text only:
- ws-spill-01, append: 'source=boot_writer_init_failed means the WAL could not initialise at boot: the app is NOT running and systemd is restart-looping it.'
- ws-spill-02, append: 'Boot sources: boot_replay_failed (staged segments archived unread), deferred or unrestored staged segments, mid_segment_resync (a crash boot can page this when bytes are skipped).'
- No new alarm, filter, metric, dimension or ok_actions.

12. Rule and runbook files:
- docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md: new §2.11 dated 2026-10-06, quoting the operator verbatim: "Go ahead with whatever you want dude" and "See do everything whatever is recommended dude okay?".
  - Content: boot lines that ran before the log sink existed now reach errors.jsonl. No new alarm, filter, metric, dimension or ok_actions.
  - ws-spill-02 may now fire on the boot replay pass (boot_replay_failed, deferred or unrestored segments, mid_segment_resync).
  - ws-spill-01 may now fire on lock_wal_dir's degraded claim and on boot_writer_init_failed.
  - The telemetry spawn line moves from BOOT-02 to log-sink-only HOT-PATH-04. The validate and metrics-init failures are log-sink-only BOOT-04.
  - OMS-GAP-06 sandbox_window_boot (its filter requires source=runtime_respawn) and RESILIENCE-01 reach the logs and page nothing.
  - REJECT: a log macro before the subscriber install; a post-install exit without the log flush; the telemetry spawn line coded BOOT-02 again; ok_actions or a dimension added to ws-spill-01 or ws-spill-02 under this row; building the subscriber before init_metrics.
- .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md stub: one §2.11 bullet and one REJECT headline.
- docs/error-runbooks/ws-frame-spill-error-codes.md: a 'Boot sources' section with a triage step for each source in step 11, and the mid_segment_resync crash-boot note.
- docs/error-runbooks/hot-path-stall-error-codes.md: a HOT-PATH-04 section (stall page blind for this process; check host thread and memory limits; log-sink only).
- docs/error-runbooks/wave-2-error-codes.md: a BOOT-04 section (sources config_validate and metrics_exporter_init; check for a second process on the metrics port; fix the config; the process is restart-looping).

13. Out of scope (separate PR, Recommendation): treat a replay Err like the refusal arm, keeping replaying/ for the next boot, with a WS-REINJECT-01-class growth cap. Required by the 2026-09-29 zero-loss lock.

## Tests
["NEW crates/app/tests/tracing_subscriber_install_order_guard.rs. Source scan of main.rs: comments blanked with byte offsets kept (copy code_only from counter_preregistration_after_recorder_guard.rs). Then the span from the '#[cfg(test)]' line before 'mod tests {' through the first column-0 '}' after it is blanked. Production fns follow that block, so the guard blanks it and does not cut at it.","(a) install_tracing_subscriber_is_called_exactly_once: exactly one code call 'install_tracing_subscriber(&config)', and it lies inside async_main.","(b) no_log_macro_runs_before_the_subscriber_is_installed: between 'fn main() -> Result<()>' and the install call there is no 'error!(', 'warn!(', 'info!(', 'debug!(', 'trace!(' or 'event!('. The tracing:: prefixed forms are caught by substring.","(c) metrics_recorder_precedes_subscriber_and_subscriber_precedes_logging_boot_steps: init_metrics( comes before install_tracing_subscriber(&config). The install comes before each of: config.validate(, check_sandbox_window(, check_clock_drift(, run_index_constituency_ts_pin_migration_at_boot(, spawn_calendar_staleness_watchdog(, hot_path_telemetry::spawn_publisher(, lock_wal_dir(, replay_all_fenced(, WsFrameSpill::new_with_guard(, check_and_report_host_limits(. Position is the only check that covers logging done inside callees.","(d) error_coalescer_is_built_only_inside_the_install_fn: 'ErrorCoalescer::new(' occurs only inside fn install_tracing_subscriber. That fn has no log macro, exactly one '.init()', and main.rs code has zero 'Box::leak('.","(e) every_exit_after_install_flushes_the_log_sinks: after the install call, main.rs production code has zero 'std::process::exit('. fn main calls 'flush_log_sinks(' after 'block_on(async_main())'.","(f) panic_hook_is_installed_right_after_the_subscriber: the install_panic_hook() call comes after the install and before config.validate(. The first 'std::panic::set_hook(' in the comment-stripped file is inside fn install_panic_hook.","(g) log_flush_budget_fits_the_teardown_margin: LOG_SINK_FLUSH_BUDGET is at most 1 s.","Guard self-tests: an error! before the install is caught; a mention inside a comment is ignored; a bare process::exit after the install is caught; a set_hook inside a mod tests fixture is ignored while a production fn after that fixture is still scanned.","observability.rs unit test flush_guards_writes_a_queued_line_before_returning: tempfile through init_lossy_non_blocking; guard pushed into a LOCAL Mutex<Vec<WorkerGuard>>; one error! under tracing::subscriber::with_default; flush_guards(1 s) returns Flushed and the file holds the line.","observability.rs unit test flush_guards_second_call_is_nothing_to_flush.","observability.rs unit test flush_guards_honours_its_budget_when_one_sink_is_wedged: two sinks, one whose write() sleeps 5 s. flush_guards(200 ms) returns TimedOut in under 1 s of wall time, and the healthy sink's line is on disk (parallel drop).","observability.rs unit test retain_log_guard_then_flush_log_sinks_drains_the_static (run serially, since it touches the global).","observability.rs unit test report_sink_init_failures_emits_one_warn_per_sink: a capturing MakeWriter shows each sink name exactly once at WARN.","crates/common error_code unit tests: HOT-PATH-04 and BOOT-04 are in all() with the right code_str, Severity::Medium and runbook_path. Existing table tests cover the auto-triage prefix arms.","Existing guards that must stay green: error_code_tag_guard (budget now 9, exact); error_code_rule_file_crossref (needs HOT-PATH-04 and BOOT-04 named in their runbooks); triage_rules_full_coverage_guard (two new rules); error_code_alarm_coverage_guard (Medium, so out of its scope); metrics_recorder_install_order_guard (init_metrics still before the WAL spill and replay; the 'no-op' note travels within 2,500 bytes; the call stays single); counter_preregistration_after_recorder_guard (with the new ErrorCoalescer::new needle); shutdown_budget_fits_systemd_guard (panic-hook order test); wal_confirm_ordering_guard; live_lane_paging_lockstep_guard (after the §2.11 edit); error_code_paging_filter_drift_guard. crates/common changes, so run cargo test --workspace."]

## Guards to update
["crates/app/tests/tracing_subscriber_install_order_guard.rs (NEW)","crates/common/tests/error_code_tag_guard.rs (UNCODED_ERROR_BUDGET 10 -> 9; doc reversal and dated correction)","crates/app/tests/counter_preregistration_after_recorder_guard.rs (add ErrorCoalescer::new( needle)","crates/common/src/error_code.rs tests (HOT-PATH-04, BOOT-04)",".claude/triage/error-rules.yaml (two silence rules, for triage_rules_full_coverage_guard)","crates/app/tests/shutdown_budget_fits_systemd_guard.rs (doc row only: the log flush of at most 1 s runs inside the 20 s teardown margin)","deploy/aws/terraform/error-code-alarms.tf (desc text for ws-spill-01 and ws-spill-02 only)"]

## Residual risks
["Behaviour change: the existing ws-spill-02 and ws-spill-01 alarms can now page on boot events: replay failure, deferred or unrestored staged segments, mid_segment_resync, a degraded WAL-dir claim, and WAL init failure. These are real data-at-risk events. A crash boot that skips bytes mid-segment will page (Assumed possible after power loss). Recorded in §2.11 and the runbook.","Assumed: the production data volume supports flock, so lock_wal_dir's degraded-claim WS-SPILL-01 lines do not page on every boot. If it lacks flock, that page is correct and new.","Metrics now go live earlier: init_metrics moves from about L1053 to about L726. Metrics emitted by the early boot steps (clock-drift check, index_constituency migration task, calendar staleness watchdog) now reach the recorder where before they hit the no-op recorder. Assumed benign; check that no alarm keys on one of them before merge.","Limitation: these lines still run before the install and stay lost: the info lines inside init_metrics ('exporter started') and init_tracing, and the appender init functions. Their failure cases are carried as Result or SinkInitFailure and logged after the install.","Limitation: a config extract failure still reaches only stderr and the journal, because no logging config exists yet at that point.","Limitation: fn main's flush does not cover tasks that log after it runs (the multi-thread runtime keeps polling until it drops). This is no worse than today. A wedged sink loses its queued tail and returns TimedOut within 1 s.","Validate now runs after init_metrics, so a bad config binds the metrics port briefly before the process exits. init_tracing runs on an unvalidated otlp endpoint; tracing_enabled is false in base.toml (reviewer-verified).","RESILIENCE-01 stays effectively unreachable while init_metrics binds the port before lock_wal_dir. A second instance now fails as a logged BOOT-04 metrics_exporter_init line rather than RESILIENCE-01. Reordering the WAL claim ahead of the exporter bind is a possible follow-up.","Not fixed here (zero-loss follow-up): a replay Err still archives earlier boots' staged segments unread. This PR makes the event loud but does not change the decision.","Latent: crates/core/src/network/ip_monitor.rs:410 has an unflushed process::exit(1) with no app call site today. Recorded, not guarded.","Process: a multi-file change across app and common needs the APPROVED plan file before plan-gate allows the push. The PR changes a shrink-only budget (an allowed down-move) and adds two ErrorCodes with their runbooks and triage rules."]

