# Implementation Plan: CloudWatch gap closure — error!→page restoration + §19 readiness pager + reconnect-storm alarm

**Status:** VERIFIED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator directive 2026-07-06, this session: "ultracode... SCOPE (terraform + minimal code, NEW .tf files)..." — the zero-page incident day)
**Branch:** `claude/trusting-sagan-n2jefi`
**Changed crates:** `crates/app` (round-5 review fix: one boot-time metric
pre-registration in `crates/app/src/main.rs` immediately after the recorder
install, a defensive no-op registration + retargeted source-scan ratchet in
`crates/app/src/groww_sidecar_supervisor.rs` — cold-path, boot only,
zero hot-path code) + one TEST-file addition in `crates/common/tests/`

> Guarantee matrices: carried by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row, mandatory
> per `per-item-guarantee-check.sh`). Infra-only scope notes: no hot-path code is
> touched (hot-path / DHAT / bench rows are N/A — zero production Rust); audit
> coverage = CloudWatch alarm state history + the errors.jsonl forensic sink
> (log-derived alarms need no new QuestDB table); testing coverage = pytest on
> the pure Lambda core + the Rust source-scan ratchet + terraform fmt/validate.
> **Honest 100% envelope (operator-charter §F wording):** these alarms guarantee
> paging 100% inside the tested envelope ONLY — a coded `error!` that reaches
> the errors.jsonl sink AND is shipped by the CloudWatch agent AND matches one
> of the 8 filtered codes pages within ~1-5 min via filter → alarm → SNS →
> Telegram (FEED-STALL-01: the only ERROR-level emission is the sidecar's own
> >5-restarts-per-5-min STORM escalation — per-restart emissions are
> warn!-level and never reach the ERROR-only errors.jsonl sink, so the errcode
> alarm pages within ~5 min of the 6th rapid restart, i.e. flap cycles of
> ~60s or faster (span math, round-13: 6 restarts span 5 gaps ≤ the 300s
> window — the earlier "~50s" used 300/6 average-rate math; the Rust
> detector's window is ANCHORED-reset, not sliding, so a burst straddling
> the anchor can defer the escalation by up to ~one extra 300s window);
> the ≥3-restarts-per-15-min band, all cadences, is covered by the
> separate `tv-<env>-feed-stall-restarts` counter alarm — round-3 review fix;
> the round-2 "≥3 restarts per 15-min window / first page ≈ 15-17 min" claim
> was FALSE against the errors.jsonl route because 3-5 warn!-level restarts
> produce zero ERROR lines). They do NOT guarantee: paging for un-filtered
> codes (log-sink-only), paging through a total SNS/Telegram outage (outside
> the envelope, stated in the handler), catching an UNLOGGED DH-906 reject
> (term-filter tripwire only; Rust emit-site is a flagged follow-up), paging
> a Groww stall-flap SLOWER than ~1 restart per 7.5 min (span math,
> round-12: 3 restarts span 2 gaps, so only cycles > ~450s can NEVER fit 3
> inside one aligned 900s window — the restart pager's true never-page
> floor; cycles faster than ~5 min page promptly; the ~5-7.5 min band pages
> EVENTUALLY via phase drift of a sustained flap — the earlier "~5 min
> floor" used 900/3 average-rate math where the per-window bound needs span
> math; a burst that stops
> after straddling the aligned 900s boundary at 2+1 pages NEVER — up to 4
> restarts (2 per side) inside a SLIDING 15-min span can go unpaged; only a
> flap sustaining faster than the stated floor always pages — CloudWatch
> windows are aligned/tumbling, not sliding (round-10 correction of the
> earlier "pages one window later" example); the counter
> is pre-registered at 0 in main.rs immediately AFTER the metrics recorder
> installs at boot — round-5 fix; the round-4 supervisor-spawn registration
> was VOID (the supervisor is spawned pre-install, so its handle resolved to
> the no-op recorder) — so the delta pipeline's dropped first sample is the
> 0 baseline and the session's FIRST restart counts; without a post-install
> registration the first restart of every app session was uncounted and the
> effective first-episode threshold was 4; residual: a stall-restart inside
> the pre-install boot window would be uncounted — physically implausible), or
> paging an order-update flap SLOWER than ~1 cycle per 3 min (>5
> once-per-cycle increments per 900s window = the detection floor; the
> 3-min+ slow-flap band is a stated residual; a burst that stops after
> straddling the aligned 900s boundary at ≤5 per side (e.g. 4+4-then-stop
> over ~12 min) pages NEVER — up to 10 reconnects inside a SLIDING 15-min
> span can go unpaged; tumbling windows, not sliding — round-10 correction
> of the round-8 "pages one window later" example; only a flap sustaining
> faster than the ~180s floor always pages, so the 2026-07-06 all-session
> shape is unaffected; an app restart costs at most
> the agent's dropped first post-restart counter sample (~8 increments
> worst-case = one scrape-interval under the reconnect backoff ladder) —
> restarts are owned
> by the liveness alarms; and the leg-3 reconnect-storm alarm pages only if
> the market-hours gate Lambda's 09:20 IST open invocation ARMED it — a
> gate-Lambda error now pages via `tv-<env>-market-hours-gate-errors`
> (round-13); residual: a rule that never INVOKES the Lambda — scheduler
> drop / disabled rule — yields no Errors datapoint at all (notBreaching →
> silent); the explicit state="ENABLED" pins + the liveness alarms are the
> backstop). Claiming more than this envelope = REJECT in
> review.

## Design

**The severed route + its restoration.** The canonical routing after this PR:
`error!` → errors.jsonl (`data/logs/machine/`) → CloudWatch Logs
`/tickvault/prod/app` (CW agent) → log metric filter → `tv_errcode_*` metric →
CloudWatch alarm (≤5 min) → SNS `tv-prod-alerts` → Telegram webhook Lambda.
An `error!` ALONE does not reach Telegram; only codes with a filter+alarm (or
paths that also call `NotificationService::notify`) page. The
Loki→Alertmanager→Telegram path was retired in the CloudWatch-only migration
(#O1/#O2/#O3) with no replacement — the 2026-07-06 zero-page incident (three
legs: 12:00 IST REST-CANARY-01 silent, 94-min-late launch silent, order-update
flapper invisible) is the proof.

- **P1 log-shipping fix:** the 2026-07-05 machine/ move (`observability.rs:44,51`;
  hourly app log too, `main.rs:1217`) killed BOTH `/tickvault/prod/app` streams —
  the CW-agent globs point at the old top level. Fix: 5-entry collect_list in
  BOTH `deploy/aws/cloudwatch-agent.json` and `user-data.sh.tftpl` — machine/
  paths with the dotted glob `machine/errors.jsonl.*` (excludes the bare
  `errors.jsonl` compat symlink, observability.rs:178) + legacy globs under
  distinct `-legacy` stream names (grace window; removed after one confirmed
  trading day). Delivery: deploy-aws.yml:631-633 copies + fetch-configs the repo
  agent JSON on every deploy; the E3 paths edit makes agent-config-only changes
  self-deploying AND makes THIS PR self-trigger a deploy on merge. The
  user-data.sh.tftpl mirror is provably inert for the running box
  (`user_data_replace_on_change = false` main.tf:356 + `user_data` in
  `lifecycle.ignore_changes` main.tf:383-392) — fresh-provision parity only.
- **P2 8-code filter+alarm map** (`error-code-alarms.tf`, for_each): REST-CANARY-01,
  DH-901, DH-906, AUTH-GAP-04, WS-GAP-07, FEED-STALL-01, WS-REINJECT-01, PROC-01.
  Coded JSON patterns `{ $.code = "X" && $.level = "ERROR" }` on the app log
  group; DH-906 is an honest plain TERM filter `"DH-906"` (zero coded emit sites
  exist — the literal arrives only inside Dhan response text; dormant under
  dry_run=true; a 3-line Rust emit site is a flagged follow-up, NOT this PR).
  Dimensionless metrics by design (errors.jsonl carries no host field; filters
  cannot emit constant dimensions); no default_value (sparse + notBreaching).
  eval 3 / dta 1 anti-flap (identical first-page latency; 1 page + 1 OK per
  repeating-error episode); FEED-STALL-01 is Sum ≥ 1 per 300s (round-3 review
  fix: the ONLY ERROR-level FEED-STALL-01 emission is the sidecar's own storm
  escalation — the 6th+ rapid restart within a 300s ANCHORED-reset window
  (round-13: the window start resets when it elapses, NOT sliding — a burst
  straddling the anchor can defer the escalation by up to ~one extra 300s
  window); per-restart
  emissions are warn!-level and invisible to the ERROR-only errors.jsonl route,
  so the round-2 "Sum ≥ 3 restarts per 900s" tuning could never see 3-5
  restarts/15 min. One storm line = page; a single self-heal restart still
  never pages because the Rust detector debounces at >5 restarts/5 min).
- **P3b feed-stall restart pager** (`feed-stall-restart-alarm.tf`, round-3
  review fix): the ≥3-restarts-per-15-min claim is made TRUE by alarming on
  the counter `tv_feed_sidecar_stall_restart_total`
  (groww_sidecar_supervisor.rs:1118 — increments once per restart, warn! +
  error! alike). Same route as P3: NOT in the 21-name EMF allowlist, extracted
  via a log metric filter on `/tickvault/prod/metrics` ($.host dimension),
  plain `Sum ≥ 3 / 900s` (per-scrape-delta model + the same
  not-live-verified residual as P3). Round-5 fix (supersedes the VOID
  round-4 supervisor-spawn registration — that supervisor is spawned in
  main.rs BEFORE observability::init_metrics installs the recorder, so its
  counter! handle resolved to the no-op recorder and registered nothing;
  the order_update_connection.rs:86 task-start analogy did not transfer
  since that task starts post-auth, long after the install): the counter is
  PRE-REGISTERED at 0 in main.rs immediately AFTER the recorder install
  (boot Step 2, next to prewarm_dispatcher_counters; ratcheted by
  test_stall_restart_counter_is_preregistered_after_recorder_install — a
  source-order scan of main.rs pinning install < registration) so the
  agent's dropped-first-sample delta baseline is the harmless 0, not restart
  #1 — without it the lazily-born series lost the first restart of every app
  session (effective first-episode threshold 4, not 3, on a box that
  restarts daily). Honest residual: a stall-restart inside the pre-install
  boot window (supervisor spawn → Step 2) is uncounted — physically
  implausible. NO market-hours gate needed:
  `should_restart_on_stall` requires market_open, so the counter cannot
  increment off-hours. Honest floor (span math, round-12): cycles faster
  than ~5 min page promptly; the ~5-7.5 min band pages eventually via phase
  drift of a sustained flap; only cycles slower than ~7.5 min (>450s — 3
  restarts span 2 gaps that no longer fit one aligned 900s window) never
  page — stated residual; a
  burst that stops after straddling the boundary at 2+1 never pages — up to
  4 restarts in a sliding 15-min span can go unpaged (tumbling windows, not
  sliding; round-10 correction of the "one window later" example). Fast
  flaps (<=~60s cycle — span math, round-13: 6 restarts span 5 gaps ≤ the
  300s window; anchored-reset detector window, a straddling burst can defer
  the escalation by up to ~one extra 300s window) also trip the P2 storm
  tripwire.
- **P3 reconnect-storm** (`order-update-reconnect-storm-alarm.tf`): the counter
  `tv_order_update_reconnections_total` (order_update_connection.rs:86/:254) is
  NOT in the 21-name EMF allowlist (ratcheted pin preserved) — extracted via a
  log metric filter on `/tickvault/prod/metrics` (the tv_boot_completed
  fallback precedent, host dimension from `$.host`) + a plain
  `Sum > 5 / 900s` alarm (round-2 review fix: the CW agent's prometheus
  pipeline ships COUNTER samples as PER-SCRAPE DELTAS — the AWS-documented
  agent behavior and the model every Sum-based sibling counter alarm in
  app-alarms.tf assumes — so Sum over the window IS the reconnect count in
  the window; the earlier `DIFF(Maximum(cumulative))` math was dead on
  arrival under delta shipping. Window is 900s per the round-1 fix: the
  counter increments ONCE per flap cycle, so a 5-min window could never
  exceed 5 for any cycle ≥ 60s; over 900s the documented ~90s incident
  cadence yields ~10 ≫ 5 while the ≤3-attempt close churn stays ≤3. Honest
  detection floor: cycles slower than ~180s do not page — stated residual.
  Aligned tumbling windows, not sliding (round-10 correction, mirrors the
  feed-stall clause): a burst that stops after straddling the 900s boundary
  at ≤5 per side pages NEVER — up to 10 reconnects in a sliding 15-min span
  can go unpaged; only a flap sustaining faster than the ~180s floor always
  pages.
  Honest residual: the delta shape is not live-verified from the sandbox —
  the post-apply runbook checks one /metrics event; if it proved cumulative,
  Sum overcounts and pages too EAGERLY, never silently misses).
  Market-hours-gated via a one-line
  `ALARM_NAMES` append in market-hours-liveness-alarm.tf (the aggregator_no_seals
  precedent): 15:30-close churn (≤3 attempts before dormant sleep) stays silent,
  and the gate's open-time set_alarm_state(OK) immunizes against latched state.
  Counter reset on restart → absorbed by the agent's delta calculation (first
  post-restart sample dropped) → cannot false-page. Arming dependence
  (round-13): the gate Lambda's 09:20 IST open invocation is the ONLY arming
  path for this alarm — a gate failure silently re-opened the leg-3
  zero-page gap, so the gate Lambda is now watched by its own AWS/Lambda
  Errors alarm `tv-<env>-market-hours-gate-errors` (Sum ≥ 1/300s, the same
  watchman shape as readiness-lambda-errors); residual: a rule that never
  INVOKES the Lambda yields no Errors datapoint at all — the explicit
  state="ENABLED" pins + the liveness alarms are the backstop.
- **P4 readiness pager** (`market-open-readiness-lambda.tf` + handler): a
  STATE-check Lambda at 08:45 IST (cron(15 3 ? * MON-FRI *)) evaluating raw
  reality (EC2 state + `tv_boot_completed` presence in the last 10 min) and
  publishing DIRECTLY to SNS — immune to weekend-latched ALARM states (the
  2026-07-06 10:04 late-launch leg: no OK→ALARM edge → zero pages). Implements
  daily-universe §19 (specified 08:40, never implemented) at 08:45. Holiday
  heuristic: launch today ≥ 08:25 IST followed by stop = the holiday gate's
  deliberate self-stop → silent. Fail-toward-paging: an unverifiable morning
  pages a distinct warning; SNS publish failure RAISES so the Lambda-Errors
  watchman alarm pages. NO ec2:StartInstances (the start-watchdog heals; this
  Lambda pages — separation of duties). Deliberate cron collision with
  start_watchdog_check (both 08:45): on box-down BOTH page with different
  actionable statements; on the box-up-app-hung shape only readiness fires.
- **P5 runbook truth-sync:** 8 files (dhan-rest-canary, observability-architecture,
  wave-4, ws-reinject, dual-instance-lock, feed-stall-watchdog, wave-2,
  CLAUDE.md) — surgical claim rewrites + dated notes; operator verbatim quotes
  NEVER edited. Budget files NOT edited (aws-budget.md doubly SUPERSEDED;
  daily-universe §7 operator-locked, dated-quote-before-edit) — flagged in the
  PR body with the cost table + a proposed §7 addendum for operator blessing.

## Edge Cases

- Counter reset on app restart → absorbed by the CW agent's per-scrape delta
  calculation (the first post-restart sample is dropped) — no negative or
  latched artifact; already-shipped in-window deltas still count, and a
  mid-window restart loses up to one scrape-interval of increments — every
  attempt between the task restart and the first post-restart 60s scrape,
  ~8 worst-case under the 0ms/0.5s/1s/2s/4s/8s/16s/32s backoff ladder
  (round-10 de-stale of the round-2-era "at most ONE increment" claim;
  restarts themselves are owned by the liveness alarms; the earlier
  "negative DIFF datapoint" wording described the retired cumulative model).
- First evening after merge: the CW agent's new machine/ collect entries have
  no saved state and default to `from_beginning=true`, so the newest matching
  files backfill FROM BYTE 0 stamped with INGESTION time (no
  timestamp_format) — up to a few one-time errcode ALARM pages can replay
  today's already-known errors (e.g. the 12:00 REST-CANARY-01 lines) as if
  current. Bounded: one newest file per stream, one time per path move.
  Treat pages arriving within ~10 min of the agent-config deploy as backfill
  (round-2 review fix; a timestamp_format for the errors-jsonl entry is a
  flagged follow-up, not this PR).
- Apply evening, one-time green-page burst (round-8, accepted + pre-briefed
  in the PR body): every NEW alarm is created in INSUFFICIENT_DATA and — with
  `treat_missing_data = notBreaching` on sparse/absent metrics — transitions
  INSUFFICIENT_DATA→OK on its first evaluation; CloudWatch invokes ok_actions
  on ANY transition into OK, and the telegram-webhook Lambda formats every OK
  as a green ✅ message (it reads only NewStateValue — no OldStateValue
  filter). Expect up to ~7 one-time "recovered" messages the apply evening
  for conditions that were never in alarm: the 4 `ok_recovery = true` errcode
  alarms (dh-901, auth-gap-04, ws-gap-07, feed-stall-01) + feed-stall-restarts
  + readiness-lambda-errors + market-hours-gate-errors (round-13; the
  reconnect-storm alarm is exempt —
  `actions_enabled = false` until the market-hours gate arms it). Creation
  settling, not recoveries; house precedent (every prior alarm PR did the
  same). An OldStateValue == INSUFFICIENT_DATA suppression branch in the
  telegram-webhook Lambda is a flagged follow-up (benefits all future alarm
  PRs), NOT this PR.
- Order-update flap SLOWER than ~1 cycle per 3 min (≤5 once-per-cycle
  increments per 900s window) never breaches `>5` → stated residual; the
  durable-down alarm owns full outages, nothing owns the 3-min+ slow-flap
  band today (round-1 review honesty). A burst that stops after straddling
  the aligned 900s tumbling boundary at ≤5 per side (e.g. 4+4-then-stop)
  never pages — up to 10 reconnects in a sliding 15-min span can go
  unpaged; only a flap sustaining faster than the ~180s floor always pages
  (round-10 correction of the round-8 "pages one window later" example;
  same corrected clause the feed-stall pager carries).
- Budget stop colliding with the holiday heuristic: the hourly
  hard-stop-guard's in-window breach_stop (its cron ticks at exactly 08:30
  IST) or the SNS budget-killswitch or a manual portal stop between
  08:25-08:45 IST also matches the launch-today-≥-08:25-then-stopped shape →
  HOLIDAY_SILENT. Not blind: every one of those stop paths publishes its OWN
  page before/while stopping, and the boot-heartbeat gate window (08:50-09:10)
  is the backstop — documented in the handler comment (round-1 review fix; the
  earlier "only the holiday gate stops at that hour" premise was false).
- The originally-specced "evening stopped-box invoke drill" can NEVER produce
  the expected page: on a normal trading evening LaunchTime = today's 08:30
  auto-start (LaunchTime updates on every start), so a stopped box classifies
  HOLIDAY_SILENT. The SNS end-to-end drill is now the explicit
  `{"mode": "drill"}` handler branch, which force-publishes a labelled test
  page through the real publish path (round-1 review fix).
- The WARN-level `code=DH-901` renewToken-fallback line (token_manager.rs:898)
  is excluded three ways: errors.jsonl layer is ERROR-only, the app stream nests
  fields under `$.fields`, and the `&& $.level = "ERROR"` pattern guard.
- NSE holiday self-stop at 08:3x → launch-time heuristic (today ≥ 08:25 IST +
  stopped/stopping) → HOLIDAY_SILENT; manual holiday run → the poolless-idle
  boot metric emits → READY_SILENT.
- `pending` state at 08:45 (start-watchdog self-heal race) → NOT_RUNNING_PAGE,
  whose wording explicitly says "watch for the watchdog's started message".
- 15:30-close order-update reconnect churn (≤3 attempts before WS-GAP-04
  dormant sleep) is ≤3 per 15-min window, below threshold 5, AND outside the
  09:20-15:35 gate window.
- Repeating DH-901 every 15 min (the 2026-07-06 shape): eval 3 / dta 1 holds
  ALARM across ≤15-min gaps → 1 page + 1 OK per episode instead of ~32.
- Rollback of the machine/ move itself → the `-legacy` globs still ship the
  old paths (no blind window in either direction during the grace period).
- The bare `errors.jsonl` compat symlink → excluded by the dotted glob so the
  agent's newest-file tailing cannot flap between symlink and hourly file.
- `machine/app.*` also matches the 0-byte `app.log` Alloy placeholder —
  harmless (agent tails newest mtime).
- Launch-time boundary: exactly 08:25 IST → holiday-eligible; 08:24 → not
  (pinned by a unit test).

## Failure Modes

- Filters DOA until the agent config ships → bounded to the same evening
  (deploy self-triggers on merge; deploy-aws-after-close.yml is the backstop);
  silent-not-wrong in the window (notBreaching); verification step confirms
  stream freshness post-deploy.
- Readiness Lambda AWS-API error (EC2/CW probe) → VERIFY_FAILED_PAGE
  ("cannot verify ≠ OK" — fail-toward-paging).
- SNS publish failure in the readiness Lambda → logger.exception + RAISE →
  AWS/Lambda Errors → the readiness-errors watchman alarm → tv_alerts.
  Honest residual: a TOTAL SNS outage silences both legs — outside the
  envelope, stated in the handler docstring.
- EventBridge scheduler drop → the rule carries explicit `state = "ENABLED"`
  (the Jul-4 pause/#1404 incident class); the boot-heartbeat gate window
  (08:50-09:10) is the backstop pager.
- An UNLOGGED DH-906 reject is invisible → flagged Rust emit-site follow-up;
  dormant while dry_run=true (no live orders).
- Market-hours gate Lambda fails at the 09:20 IST open (the ONLY arming path
  for the leg-3 reconnect-storm alarm + the other 3 gated alarms) →
  `tv-<env>-market-hours-gate-errors` pages (AWS/Lambda Errors, Sum ≥ 1/300s
  — the same watchman shape as readiness-lambda-errors; round-13). Before
  this alarm a gate failure silently re-opened the 2026-07-06 zero-page gap.
  Residual: a rule that never INVOKES the Lambda (scheduler drop / disabled
  rule) produces no Errors datapoint — the explicit state="ENABLED" pins +
  the liveness alarms are the backstop.
- A future sink-path move re-blinding the filters → the
  `test_cw_agent_collects_machine_log_paths` ratchet fails the build for
  BOTH vectors (round-2 review fix): it derives the expected globs from the
  Rust sink constants (`observability.rs::ERRORS_JSONL_DIR` /
  `MACHINE_LOGS_DIR`) and asserts both agent configs carry them — a
  code-side sink move (the exact 2026-07-05 vector) now fails the build
  until the agent configs move in lockstep, not just a config-side glob
  regression.

## Test Plan

- pytest: `deploy/aws/lambda/market-open-readiness/test_handler.py` — the 11
  spec cases (ready-silent, not-booted, not-running stale launch, holiday
  stopped/stopping, pre-cutoff launch, pending, probe-error, subject limits,
  08:25 boundary, SNS-raise propagation).
- `cargo test -p tickvault-common --test cloudwatch_app_alarms_wiring` —
  existing pins untouched + the new machine-path ratchet.
- `terraform fmt -check -recursive` + `terraform init -backend=false` +
  `terraform validate` locally; terraform-apply.yml's plan job re-runs both on
  the PR (path-triggered by the new .tf files). Plan output must show
  create-only resources + ZERO changes to `aws_instance.tv_app` (user_data
  inert per main.tf:356/383-392).
- `bash .claude/hooks/banned-pattern-scanner.sh` + `bash .claude/hooks/plan-verify.sh`.
- Post-apply (PR-body runbook): `describe-log-streams` freshness check on
  `/tickvault/prod/app` (expect up to a few one-time backfill errcode pages
  within ~10 min of the agent-config deploy — from_beginning replay of
  today's already-logged errors, see Edge Cases; ALSO expect up to ~7
  one-time green ✅ "OK" messages the apply evening — new-alarm
  INSUFFICIENT_DATA→OK creation settling, not recoveries, see Edge Cases;
  wait for those to settle
  BEFORE the fire drill), `set-alarm-state` fire drill on
  tv-prod-errcode-rest-canary-01, the SNS end-to-end drill
  `aws lambda invoke --function-name tv-prod-market-open-readiness
  --cli-binary-format raw-in-base64-out --payload '{"mode":"drill"}'
  /dev/stdout` → expect the [DRILL] test page on Telegram (round-1 review
  fix: the previously-specced evening stopped-box invoke classifies
  HOLIDAY_SILENT and can never prove the SNS leg; round-2:
  `--cli-binary-format raw-in-base64-out` is required on AWS CLI v2 or the
  JSON payload errors as invalid base64), and the counter-shape check for
  the storm alarm: filter one `/tickvault/prod/metrics` event for
  `tv_order_update_reconnections_total` (and, round-3, for
  `tv_feed_sidecar_stall_restart_total` — the feed-stall restart pager rides
  the same route) across two scrapes — the value must
  behave as a per-scrape DELTA (0/small per scrape), NOT a growing
  cumulative; if cumulative, rework the storm alarm to `DIFF(Maximum)`
  (round-2 review fix — the Sum model is AWS-documented + sibling-alarm
  consistent but not live-verified from the sandbox).

## Rollback

- All terraform resources are ADDITIVE → `terraform destroy -target` or a
  revert-PR removes them cleanly; no instance-touching resource exists in the
  plan (user_data is ignored by lifecycle; the instance never replans).
- Agent config: revert `cloudwatch-agent.json` → the next deploy's copy +
  fetch-config restores the old collect_list (the legacy entries mean no data
  loss in either direction during the window).
- Lambda + rule + alarms: single-file resources, revert = clean destroy.
- Runbook edits: `git revert` (docs only).

## Observability

- 12 new alarms (8 errcode + 1 reconnect-storm + 1 feed-stall-restarts +
  1 readiness-lambda-errors + 1 market-hours-gate-errors, round-13) →
  tv_alerts → Telegram; ok_actions symmetric for repeat-emitters (the OK is
  the "recovered" message the operator wanted on 2026-07-06) — EXCEPT the
  one-shot/discrete emitters (`ok_recovery = false`, round-1 fix widened in
  round-4): rest-canary-01 (3 probes/day — OK means "no new probe ran yet"),
  ws-reinject-01 (fires once per boot; the staged-WAL condition persists
  until the NEXT boot), proc-01 (a discrete OOM kill — the pressure behind
  it is not fixed by the episode aging out) and dh-906 (a discrete per-order
  reject). For these the auto-OK ~15 min after the datapoint ages out would
  be a Rule-11 false "recovered" page (the webhook Lambda forwards OK states
  as a green message), so their OK pages are suppressed. auth-gap-04 keeps
  ok_actions ON with a stated residual: it repeat-emits per failing boot
  cycle under systemd Restart=always, so OK ≈ stopped firing — unless
  StartLimitBurst (8/600s) halted the unit, in which case the OK is
  aged-out; the alarm desc carries this caveat.
- New metrics: 8 sparse `tv_errcode_*` (billed only in hours a code fires),
  2 host-dimensioned /metrics-derived counters
  (`tv_order_update_reconnections_total`,
  `tv_feed_sidecar_stall_restart_total` — both DENSE during app uptime: the
  reconnect counter registers at order-update task start (post-install) and
  the stall counter in main.rs right after the recorder installs (round-5
  fix), so each ships a 0-delta event per 60s scrape; the 0s sum
  harmlessly).
- The readiness Lambda logs its verdict on every run (silent-healthy mornings
  are visible in its log group); its AWS/Lambda Errors alarm watches the
  watchman.
- Ratchet: `test_cw_agent_collects_machine_log_paths` derives the shipping
  globs from the Rust sink constants (observability.rs) and asserts both
  agent configs carry them — the next sink move (code-side OR config-side)
  fails the build instead of silently re-blinding every filter (round-2
  review fix: literal-only pinning missed the code-side vector).
- Alarm-count honesty: 33 → 45 (verified real count — 33 alarm resources
  across 11 .tf files pre-PR; 45 alarms from 38 resource blocks across 15
  .tf files post-PR, the errcode block being a for_each over 8 codes; the
  market-hours-gate-errors watchman joined round-13; the rule-file "10 free
  tier" claims were already stale pre-PR; overage above the 10 free-tier
  alarms moves $2.30 → $3.50/mo). Realistic
  cost delta (round-13 recompute; worst-case corrected round-8): +12 alarms
  × $0.10 = $1.20, + 2 DENSE
  /metrics-derived custom metrics × $0.30 × 270/730 uptime-hrs ≈ $0.22, +
  sparse `tv_errcode_*` $0.05-0.40 ⇒ ≈ **+$1.5-1.8/mo (~₹130-160 incl 18%
  GST; worst realistic ≈ +$2.3/mo, ~₹230 incl 18% GST — the sum of the
  worst components: $1.20 alarms + $0.22 dense + $0.89 sparse worst-case,
  a code firing every running hour = 8 × $0.30 × 270/730; the round-5
  "≈ +$2.0" label sat below its own component arithmetic)** — the earlier
  "+$1.2-1.5 (~₹105-130
  incl GST)" band understated both ends and its ₹ figure omitted the GST it
  claimed. Still comfortably inside the $35 pre-GST budget-alarm ceiling.

## Per-item guarantee matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row +
7-row) — this plan inherits it per the mandatory cross-reference clause.
Infra-only scope: no hot-path code touched (DHAT/bench rows N/A — zero
production Rust); zero-tick-loss chain untouched (no new tick-drop path; the
WAL/ring/spill/DLQ code is not modified); uniqueness/dedup N/A (no QuestDB
table); the honest-100% envelope wording is in the header block above.

## Plan Items

- [x] P1 log-shipping fix — Files: deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/user-data.sh.tftpl, .github/workflows/deploy-aws.yml, crates/common/tests/cloudwatch_app_alarms_wiring.rs — Tests: test_cw_agent_collects_machine_log_paths
- [x] P2 error-code filters+alarms — Files: deploy/aws/terraform/error-code-alarms.tf — Tests: terraform validate + post-apply fire drill
- [x] P3 reconnect-storm — Files: deploy/aws/terraform/order-update-reconnect-storm-alarm.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf — Tests: terraform validate
- [x] P3b feed-stall restart pager (round-3 review fix) — Files: deploy/aws/terraform/feed-stall-restart-alarm.tf, deploy/aws/terraform/error-code-alarms.tf (feed-stall-01 entry retuned to the storm-escalation tripwire) — Tests: terraform validate
- [x] P4 readiness pager — Files: deploy/aws/terraform/market-open-readiness-lambda.tf, deploy/aws/lambda/market-open-readiness/handler.py, deploy/aws/lambda/market-open-readiness/test_handler.py — Tests: test_running_and_booted_is_ready_silent, test_running_without_boot_metric_pages_not_booted, test_stopped_with_stale_launch_pages_not_running, test_stopped_after_holiday_gate_self_stop_is_silent, test_stopping_after_holiday_gate_self_stop_is_silent, test_stopped_with_early_launch_pages_not_running, test_pending_pages_not_running, test_probe_error_pages_verify_failed, test_subjects_are_ascii_and_within_sns_limit, test_holiday_cutoff_boundary_0825_ist, test_sns_publish_failure_propagates, test_handler_drill_mode_force_publishes_test_page, test_drill_verdict_never_returned_by_classifier
- [x] P3c stall-restart counter pre-registration (round-4 review fix; registration point corrected by P3d) — Files: crates/app/src/groww_sidecar_supervisor.rs, deploy/aws/terraform/feed-stall-restart-alarm.tf, .claude/rules/project/feed-stall-watchdog-error-codes.md — Tests: superseded by test_stall_restart_counter_is_preregistered_after_recorder_install (P3d)
- [x] P3d stall-restart counter registration moved post-recorder-install (round-5 review fix — the round-4 supervisor-spawn registration raced the recorder install and resolved to a no-op) — Files: crates/app/src/main.rs (registration after observability::init_metrics), crates/app/src/groww_sidecar_supervisor.rs (defensive-only comment + retargeted ratchet), deploy/aws/terraform/feed-stall-restart-alarm.tf, .claude/rules/project/feed-stall-watchdog-error-codes.md — Tests: test_stall_restart_counter_is_preregistered_after_recorder_install
- [x] P2b one-shot-code OK-page suppression (round-4 review fix) — Files: deploy/aws/terraform/error-code-alarms.tf (ws-reinject-01 + proc-01 + dh-906 ok_recovery=false; auth-gap-04 cadence caveat documented) — Tests: terraform validate
- [x] P5 runbook truth-sync — Files: .claude/rules/project/dhan-rest-canary-error-codes.md, .claude/rules/project/observability-architecture.md, .claude/rules/project/wave-4-error-codes.md, .claude/rules/project/ws-reinject-error-codes.md, .claude/rules/project/dual-instance-lock-2026-07-04.md, .claude/rules/project/feed-stall-watchdog-error-codes.md, .claude/rules/project/wave-2-error-codes.md, CLAUDE.md — Tests: n/a (docs)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 12:00 IST REST canary probe fails (2026-07-06 leg 1) | errcode filter → alarm ALARM within ~5 min → SNS → Telegram 🆘; NO OK page for this alarm (the auto-OK ~15 min later means the episode aged out, NOT recovery — next probe / DH-901 alarm is the recovery signal) |
| 2 | Box launches 94 min late / app hung at 08:45 (leg 2) | readiness Lambda pages NOT_RUNNING / NOT_BOOTED regardless of latched alarm state |
| 3 | Order-update WS flaps every ~90s in market hours (leg 3) | Sum>5/15min storm alarm pages (~10 once-per-cycle delta increments per 900s window — the agent ships per-scrape counter deltas; gated 09:20-15:35 IST) |
| 4 | Healthy trading morning | readiness Lambda runs silent; verdict logged; zero pages |
| 5 | NSE holiday (gate self-stops box at ~08:32) | HOLIDAY_SILENT — no page |
| 6 | AWS API lookup fails at 08:45 | VERIFY_FAILED_PAGE ⚠️ (fail-toward-paging) |
| 7 | DH-901 repeats every 15 min | 1 🆘 page + 1 🟢 OK per episode (eval 3/dta 1) |
| 8 | Future session moves the log sinks again (code-side const move OR config-side glob edit) | test_cw_agent_collects_machine_log_paths fails the build — it derives the expected globs from the observability.rs sink constants and checks both agent configs (round-2 cross-coupling) |
| 9 | Operator invokes the readiness Lambda with `{"mode":"drill"}` (any time, any EC2 state) | 🧪 test page through the real SNS→Telegram path — the end-to-end proof for leg 2 |
| 10 | Budget breach_stop / killswitch / portal stop lands 08:25-08:45 IST | readiness classifies HOLIDAY_SILENT, but the stopping path itself already paged (breach_stop/killswitch publish before stopping); boot-heartbeat gate window is the backstop |
| 11 | Order-update WS flaps slower than ~1 cycle / 3 min | NOT paged — stated residual (detection floor >5 per 15 min); durable-down alarm owns full outages |
| 12 | Groww sidecar stall-flaps at a ~90s-300s cycle (warn!-level restarts, no storm escalation) | `tv-prod-feed-stall-restarts` counter alarm pages (Sum ≥ 3 restarts per 15-min window) — round-3 fix; the errcode alarm alone could NEVER see this band (zero ERROR lines) |
| 13 | Groww sidecar stall-flaps at a <=~60s cycle (>5 restarts / 5 min — span math, round-13: 6 restarts span 5 gaps ≤ 300s) | BOTH page: the sidecar's storm-escalation ERROR line trips errcode-feed-stall-01 within ~5 min (anchored-reset detector window — a straddling burst can defer the escalation by up to ~one extra 300s window) AND the counter alarm trips within the 15-min window |
| 14 | Single Groww stall-restart that recovers | NO page on either route (self-heal by design; 1 < threshold 3, warn!-level line matches no filter) |
| 15 | Groww stall-flap slower than ~1 restart / 7.5 min | NEVER paged — stated residual (span-derived never-page floor, round-12: 3 restarts span 2 gaps > 900s at cycles > ~450s; the ~5-7.5 min band pages eventually via phase drift of a sustained flap; tumbling not sliding) |
| 16 | First stall episode of the day (3 restarts, fresh app session) | `tv-prod-feed-stall-restarts` pages — the post-recorder-install counter registration in main.rs makes the agent's dropped first sample the 0 baseline, so all 3 restarts count (round-5; the round-4 supervisor-spawn registration raced the install and was a no-op — previously Sum=2 → silent) |
| 17 | WS-REINJECT-01 / PROC-01 / DH-906 fires once, episode ages out ~15 min later | NO green "recovered" OK page (ok_recovery=false, round-4) — the condition persists (staged WAL / memory pressure / rejected order); only genuine repeat-emitters send OK-on-silence |
| 18 | Market-hours gate Lambda errors at the 09:20 IST open (the ONLY arming path for the leg-3 reconnect-storm pager + the other 3 gated alarms) | `tv-prod-market-hours-gate-errors` pages (AWS/Lambda Errors, Sum ≥ 1/300s — round-13); before it, a gate failure silently left all 4 gated alarms disarmed for the session; residual: a rule that never INVOKES the Lambda produces no Errors datapoint — state="ENABLED" pins + the liveness alarms are the backstop |
