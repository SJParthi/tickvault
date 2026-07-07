# Implementation Plan: Restore app->CloudWatch log shipping + never-again ratchets

**Status:** APPROVED
**Date:** 2026-07-07
**Approved by:** Parthiban (operator directive 2026-07-07, this session)
**Changed crates:** core (crates/core), app (crates/app), common (crates/common — test extension only)

## Design

**Root cause:** commit `c81a81a` (2026-07-05, "one human log file; robot files into machine/ subfolder")
moved every machine sink the app writes from `data/logs/` to `data/logs/machine/`
(`crates/app/src/observability.rs::MACHINE_LOGS_DIR`), but the CloudWatch agent's watched globs at
`deploy/aws/cloudwatch-agent.json:33-34` (and the divergent first-boot copy in
`deploy/aws/terraform/user-data.sh.tftpl`) still point at the top level
(`/opt/tickvault/data/logs/errors.jsonl*`, `/opt/tickvault/data/logs/app.*`). Since 2026-07-06 17:19 IST
zero app log events reach `/tickvault/prod/app` — app healthy, shipper dead-tailing nothing.

**The fix package (judge-synthesized final design — full text in the task scratchpad `final-design.md`):**

1. **Shipper glob fix (both configs, byte-identical `.2*` pins):** replace both collect_list entries in
   `deploy/aws/cloudwatch-agent.json:33-34` with
   `/opt/tickvault/data/logs/machine/errors.jsonl.2*` (stream `{instance_id}/errors-jsonl`) and
   `/opt/tickvault/data/logs/machine/app.2*` (stream `{instance_id}/app`); same two globs in
   `deploy/aws/terraform/user-data.sh.tftpl` (keep `/tickvault/$${ENVIRONMENT}/app` group, keep
   `/var/log/messages` entry, fix the stale comment, add `data/logs/machine` to the first-boot mkdir).
   The `.2*` pin matches the hourly `errors.jsonl.YYYY-MM-DD-HH` / `app.YYYY-MM-DD-HH` files while
   excluding the `errors.jsonl` compat symlink and the 0-byte `app.log` Alloy placeholder. Old
   top-level globs are DROPPED (launcher-owned top level; dead globs ARE the drift class). Group and
   stream names preserved — dashboards/queries/retention untouched. `machine/candles/`,
   `machine/live_ticks/`, `errors.log`, `errors.summary.md` deliberately NOT shipped
   (ingestion-runaway budget + duplicates).
2. **Silence alarm:** new `aws_cloudwatch_metric_alarm.app_log_ingestion_silent` in
   `deploy/aws/terraform/log-retention.tf` (below `logs_ingestion_runaway`): namespace `AWS/Logs`,
   metric `IncomingLogEvents`, `dimensions = { LogGroupName = aws_cloudwatch_log_group.tv_app.name }`,
   Sum / period 300 / evaluation_periods 3 / threshold 1 / `LessThanThreshold`,
   `treat_missing_data = "breaching"` (AWS/Logs publishes NO datapoint at zero events — silence IS
   missing), `actions_enabled = false`, alarm/ok actions `[aws_sns_topic.tv_alerts.arn]` (direct refs,
   sibling style). Gated by appending a 4th member
   `aws_cloudwatch_metric_alarm.app_log_ingestion_silent.alarm_name` to the existing market-hours gate
   Lambda's `ALARM_NAMES` join in `deploy/aws/terraform/market-hours-liveness-alarm.tf` — inherits the
   09:20 IST open + OK-reset / 15:35 IST close (no nightly-stop false pages, zero new infra).
3. **Deploy smoke check (non-fatal):** new step "Verify log ingestion (LOG-INGESTION-SMOKE,
   non-fatal)" in `.github/workflows/deploy-aws.yml` after "Verify app readiness", before "Record
   deployed binary git SHA to SSM". Self-contained `START_MS=$(date +%s%3N)` captured in-step
   (post-readiness — proves the NEW agent config ships), 12 polls × 25s of
   `aws logs filter-log-events --log-group-name /tickvault/prod/app --start-time "$START_MS" --limit 1`.
   Failure = `::warning::` + plain-English SNS page to `tv-prod-alerts`, then `exit 0` — NEVER fatal
   (2026-06-09 observability-never-blocks-the-trading-deploy doctrine). IAM: one statement in
   `deploy/aws/terraform/oidc.tf` github_deploy policy —
   `logs:FilterLogEvents` on `${aws_cloudwatch_log_group.tv_app.arn}:*`. Also extend the on-box
   `=== APP LOG DIR ===` echo with `ls -la /opt/tickvault/data/logs/machine/`.
4. **Token countdown (`crates/core/src/auth/token_manager.rs`):** the flat ~23h
   `tv_token_remaining_seconds` step becomes a live 30s countdown. New
   `const TOKEN_GAUGE_INTERVAL_SECS: u64 = 30;` extract private
   `fn emit_token_remaining_gauge(&self)` (arc-swap load, `time_until_refresh(0)`, skip-on-None —
   never set 0 pre-auth); re-point both existing emit sites at it; replace the single renewal_loop
   sleep with a sliced loop via pure `fn next_gauge_slice(remaining) -> Duration` (min(remaining, 30s)),
   emitting per slice. NO new task / pub fn / main.rs wiring — gauge lifecycle stays coupled to the
   lane-owned renewal task (dhan-lane C4). Metric name unchanged; the existing
   `tv-<env>-token-remaining-low` alarm semantics are preserved.
5. **Never-again ratchets:** NEW `crates/app/tests/cw_agent_log_glob_guard.rs` importing
   `tickvault_app::observability` path constants — asserts both shipper configs' globs derive from the
   Rust constants, no collect_list glob points at the top-level logs dir, stream names are stable, and
   the deploy workflow carries the LOG-INGESTION-SMOKE step + oidc.tf grant. Extend
   `crates/common/tests/cloudwatch_app_alarms_wiring.rs` to pin the new alarm resource, its
   `treat_missing_data = "breaching"`, and its membership in the gate Lambda's ALARM_NAMES join.

## Edge Cases

- **Compat symlink + 0-byte placeholder:** `machine/errors.jsonl` (symlink) and `machine/app.log`
  (0-byte Alloy placeholder) sit next to the hourly files; the `.2*` pins structurally exclude both, so
  the agent can never latch onto the wrong "newest" file. Pin safe until year 3000.
- **Hourly rotation:** the CW agent tails the newest-mtime match per glob — the hour-boundary hop to
  `app.YYYY-MM-DD-HH+1` is the same mechanism the old glob relied on; a hop delay is far inside the
  15-min alarm window.
- **First boot:** agent starts before the app creates `machine/` — pre-creating the dir in the
  user-data mkdir removes the agent scan-error window.
- **Nightly 16:30 IST stop / weekends:** box off ⇒ metric missing ⇒ alarm state ALARM, but
  `actions_enabled=false` outside the 09:20–15:35 gate window and the gate's open-time OK reset
  mean it can never page out of window.
- **Weekday NSE holidays (round-1 review fix — the original premise here was FALSE):** the box does
  NOT run on weekday NSE holidays — `deploy/aws/holiday-gate.sh` self-stops the instance at boot
  (~08:32 IST) on a definitive holiday verdict, while the gate Lambda's open cron is a holiday-blind
  plain MON-FRI schedule. A blind 09:20 enable + OK reset would therefore drive both
  breaching-on-missing gated alarms (`market_hours_liveness_missing` ~09:25,
  `app_log_ingestion_silent` ~09:35) OK→ALARM against an intentionally-stopped box every weekday
  holiday. Fix: the gate Lambda's `open` mode now verifies the tv-app instance is up
  (`ec2:DescribeInstances`, `EC2_INSTANCE_ID` env — the cycle-free start-watchdog pattern) before
  enabling; not-up ⇒ actions stay disabled, no OK reset. FAIL-OPEN on any EC2 API error so a real
  trading day never loses the liveness page. Ratchet: `test_gate_lambda_open_is_holiday_safe`.
- **Mid-day deploy:** fetch-config agent restart (~1-2 min) + app restart/WAL replay are inside the
  15-min window.
- **Pre-auth boot window:** gauge skip-on-None preserved — never emits 0 (would false-fire
  token-remaining-low, threshold 14400, stat Minimum).
- **Short final sleep slice:** `next_gauge_slice` passes remainders < 30s through unchanged, so the
  renewal instant never shifts (pause/advance test asserts no early renewal).
- **Deploy outside market hours with broken shipper:** smoke-step SNS page is the immediate signal;
  the alarm backstops at the next window open (~09:35 IST earliest).

## Failure Modes

- **Shipper still dead after fix (agent down, bad config merge):** smoke check pages at deploy time;
  `app_log_ingestion_silent` pages within ~15 min of the next market-hours window. Neither ever fails
  the deploy — trading unaffected by design.
- **Gate Lambda EventBridge state drift (#1404 class):** alarm actions stay disabled silently — same
  blast radius as the 3 existing gated alarms; the deploy smoke check is the independent second
  detector (residual risk accepted, documented).
- **Operator manual holiday run (`ALLOW_HOLIDAY_RUN` marker):** if the box was stopped at 09:20 the
  gated alarms stay disabled for that day even after a later manual start — accepted (manual runs
  are off-schedule by definition; the deploy LOG-INGESTION-SMOKE step still covers deploys).
- **Boot-heartbeat gate shares the holiday false-page class (pre-existing, DOCUMENTED residual):**
  `boot-heartbeat-alarm.tf` has its own separate holiday-blind gate Lambda (08:50–09:10 IST window,
  `tv_boot_completed` breaching-on-missing) — on a weekday NSE holiday it would false-page ~08:55.
  Out of this PR's scope (separate Lambda, not touched by this diff); follow-up: apply the same
  instance-up check there.
- **Mid-window terraform apply:** ALARM_NAMES env change redeploys the gate Lambda; the new alarm's
  actions stay disabled until the next 09:20 open (bounded one-session lag; smoke check covers it).
- **AWS/Logs vended-metric publish lag (1-5 min observed):** absorbed by period 300 × 3; do not
  tighten evaluation_periods without observing live lag.
- **FilterLogEvents eventual consistency / throttling:** 300s poll budget covers it; a false ⚠️ is
  non-fatal and self-corrects on operator check.
- **Renewal retry storm:** gauge freezes between attempts (same as today, honest envelope); a frozen
  countdown crossing 14400 still pages via the existing token alarm.
- **Partial terraform apply (alarm created, join not updated):** alarm exists but never pages — the
  `cloudwatch_app_alarms_wiring.rs` gate-membership rot guard pins the join in-repo; plan output
  review required for all three resources.

## Test Plan

Scoped per `testing-scope.md`: `cargo test -p tickvault-core` (token_manager),
`cargo test -p tickvault-app --test cw_agent_log_glob_guard`, `cargo test -p tickvault-common --test
cloudwatch_app_alarms_wiring`. Tests:

- `test_next_gauge_slice_caps_at_30s_and_passes_short_remainders` — pure fn (23h→30s, 7s→7s, 0→0).
- `test_renewal_loop_uses_sliced_sleep_gauge_countdown` — source-scan: renewal_loop body contains
  `next_gauge_slice` + `emit_token_remaining_gauge`; `TOKEN_GAUGE_INTERVAL_SECS == 30`.
- Pause/advance (`tokio::time::pause`) test asserting the loop survives multiple slices with NO early
  renewal.
- `test_reference_agent_config_globs_match_observability_constants` — serde_json parse of
  cloudwatch-agent.json; file_path set == globs derived from `MACHINE_LOGS_DIR` constants.
- `test_userdata_inline_config_globs_match_reference` — line-scan of user-data.sh.tftpl (allowlist
  `/var/log/messages`); set equality with the reference JSON.
- `test_no_collect_list_glob_points_at_top_level_logs_dir` — every `/opt/tickvault/data/logs/...`
  file_path in BOTH configs contains `/machine/` (the exact incident regression).
- `test_stream_names_are_stable` — pins `{instance_id}/errors-jsonl` + `{instance_id}/app`.
- `test_deploy_workflow_carries_log_ingestion_smoke` — pins "LOG-INGESTION-SMOKE",
  "filter-log-events" in deploy-aws.yml and "logs:FilterLogEvents" in oidc.tf.
- `cloudwatch_app_alarms_wiring.rs` extension — pins `app_log_ingestion_silent`,
  `IncomingLogEvents`, `treat_missing_data  = "breaching"` in log-retention.tf, and
  `app_log_ingestion_silent.alarm_name` inside the ALARM_NAMES join.

Live verification evidence (post-merge, per zero-loss charter §4): two-file config diff; on-box
`ls -la /opt/tickvault/data/logs/machine/`; first post-deploy `LOG-INGESTION-SMOKE OK` line verbatim;
`aws logs describe-log-streams --order-by LastEventTime` showing fresh lastEventTimestamp on both
streams; `aws cloudwatch describe-alarms` for the new alarm.

## Rollback

Every change is independently revertible with no data-loss surface (logs land locally regardless):
- Globs: `git revert` the two config files; next deploy re-copies + fetch-config's the old config.
- Alarm + gate join: `terraform apply` after revert destroys the alarm and restores the 3-member join;
  no state migration.
- Smoke step + IAM grant: revert deploy-aws.yml step + oidc.tf statement; the step is additive and
  non-fatal so an interim broken state cannot fail deploys.
- Token countdown: revert restores the single flat sleep; both original emit sites remain semantically
  identical (`time_until_refresh(0)`), so the alarm never notices.
- Ratchet tests: deleting the reverted assertions in the same revert commit keeps CI green.

## Observability

- New alarm `tv-<env>-app-log-ingestion-silent` (AWS/Logs `IncomingLogEvents`, vended = $0; +1 alarm
  ≈ $0.10/mo) — pages Telegram via `tv_alerts` SNS inside market hours; OK action self-clears.
- Deploy-time signal: LOG-INGESTION-SMOKE step output + ⚠️ SNS page on failure (plain-English body per
  the 10 Telegram commandments); on-box deploy log now carries `ls -la .../machine/` evidence.
- `tv_token_remaining_seconds` becomes a live ≤30s-stale countdown (agent scrapes 60s; alarm period
  300 stat Minimum sees ≥10 fresh samples) — the `tv-<env>-token-remaining-low` alarm finally
  evaluates a decaying value instead of a ~23h flat step.
- No new ErrorCode / rule file needed: no Rust `error!` emit site is added (cross-ref tests govern
  ErrorCode variants only). No hot-path change — the gauge loop is cold path (one arc-swap load +
  gauge set per 30s); no DHAT/Criterion required.

## Plan Items

- [x] Item 1 — Shipper glob fix in BOTH configs (`.2*` machine/ pins, drop top-level globs, mkdir + comment fix)
  - Files: deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/user-data.sh.tftpl
  - Tests: test_reference_agent_config_globs_match_observability_constants, test_userdata_inline_config_globs_match_reference, test_stream_names_are_stable

- [x] Item 2 — `app_log_ingestion_silent` alarm + gate Lambda ALARM_NAMES 4th member
  - Files: deploy/aws/terraform/log-retention.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf
  - Tests: test_app_log_ingestion_silent_alarm_pinned_in_terraform, test_alarm_is_gated_by_market_hours_lambda
  - Deviation note (2026-07-07, per task directive): the alarm pins live in the NEW
    `crates/app/tests/cloudwatch_agent_glob_guard.rs` instead of extending
    `crates/common/tests/cloudwatch_app_alarms_wiring.rs` — one file owns the whole ratchet suite.

- [x] Item 3 — Deploy LOG-INGESTION-SMOKE step (non-fatal, warn + SNS) + oidc `logs:FilterLogEvents` grant + on-box machine/ dir listing
  - Files: .github/workflows/deploy-aws.yml, deploy/aws/terraform/oidc.tf
  - Tests: test_deploy_workflow_carries_log_ingestion_smoke

- [x] Item 4 — Token countdown: dedicated 30s gauge emitter fused with renewal_loop via tokio::select!
  - Files: crates/core/src/auth/token_manager.rs
  - Tests: test_emit_token_remaining_gauge_computes_remaining_seconds, test_emit_token_remaining_gauge_skips_when_no_token, test_emit_token_remaining_gauge_saturates_to_zero_for_expired_token, test_token_gauge_loop_ticks_forever_and_is_cancel_safe, test_token_gauge_countdown_is_spawned_with_renewal_loop
  - Deviation note (2026-07-07, per task directive — overrides final-design.md §4 sliced-sleep):
    dedicated periodic emitter (`token_gauge_loop`, 30s interval) spawned at the renewal_loop
    spawn site; renewal_loop timing + its two existing emit sites are byte-untouched. Fused into
    the SAME JoinHandle via `tokio::select!` so lane teardown / renewal abort kills the countdown
    too (closes the judge's C4 fire-and-forget leak objection).

- [x] Item 5 — Glob-drift ratchet suite (NEW test file, constants-derived assertions)
  - Files: crates/app/tests/cloudwatch_agent_glob_guard.rs
  - Tests: test_no_collect_list_glob_points_at_top_level_logs_dir, test_stream_names_are_stable
  - Deviation note (2026-07-07): file named `cloudwatch_agent_glob_guard.rs` (the design's
    `cw_agent_log_glob_guard.rs` never existed on disk); all 7 ratchet tests live here, including
    the Item 2 alarm pins — no `cloudwatch_app_alarms_wiring.rs` edit.

- [ ] Item 6 — Evidence + docs: PR body with live verification artefacts; runbook/rule notes for the new alarm
  - Files: .claude/plans/active-plan.md (checkbox ticks), PR body (config diff, on-box ls, smoke OK line, describe-log-streams)
  - Tests: bash .claude/hooks/plan-verify.sh (Status → VERIFIED before push)

## Per-Item Guarantee Matrix

Carried by CROSS-REFERENCE to `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row,
mandatory) — permitted by that rule ("or cross-reference it"). Rows that genuinely apply here:

| Row | How this plan satisfies it |
|---|---|
| 100% monitoring | New AWS/Logs silence alarm + smoke-step page + live token countdown gauge (7-layer subset appropriate to an infra fix) |
| 100% alerting | Alarm gated via the market-hours Lambda (edge behavior via gate open OK-reset); rot-guard pins the alarm AND its gate membership so it can never silently fall out of the join |
| 100% testing coverage | Unit + source-scan + pause/advance time-control + config-parse ratchets, scoped per testing-scope.md (core + app + common test lanes, all feeding All Green) |
| No new hot-path allocation | Token gauge is cold path (1 arc-swap load / 30s); no per-tick code touched — O(1) hot path untouched |
| 100% extreme check | Every fix is paired with a build-failing ratchet (glob drift, top-level regression, workflow-step deletion, gate-join rot, sliced-sleep refactor) |

N/A rows (one-line reasons): audit coverage — no SEBI-relevant typed event added, no audit table;
code performance (DHAT/Criterion) — no hot-path change; security hardening delta — one read-only,
group-scoped IAM grant is the entire attack-surface change (least privilege, reviewed);
scenarios/chaos — failure modes are AWS-side config drift, covered by ratchets + alarm, not by
in-process chaos tests; bug-fixing 3-agent review — runs per operator-charter §E on the
implementation diff (this is the plan artifact).

**Honest 100% claim:** 100% inside the tested envelope, with ratcheted regression coverage: the two
shipper configs' globs are build-fail-pinned to the Rust path constants; the silence alarm pages
within ~15 min of a dead shipper inside the 09:20–15:35 IST gated window; the deploy smoke check
proves fresh ingestion within 300s or pages, never failing a verified-healthy trading deploy; the
token gauge decays with ≤30s staleness while a token is loaded. NOT claimed: paging outside the gate
window (smoke page + next-window alarm are the bounded backstop), immunity to gate-Lambda EventBridge
state drift (#1404 class — the smoke check is the independent detector), or gauge freshness during
renewal retry storms (frozen between attempts, same as today).
