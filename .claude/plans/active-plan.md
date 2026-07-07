# Implementation Plan: Order-Update WS Outage Page + Honest Recovery (PR-1)

**Status:** VERIFIED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator directive 2026-07-06, this session: "Make it ready for merge monitor it until it gets merged... then go ahead with the plan")
**Branch:** `claude/sleepy-wright-lgj8am`
**Changed crates:** common (crates/common), core (crates/core), app (crates/app)

> Guarantee matrices: carried by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row, mandatory
> per `per-item-guarantee-check.sh`). Per-row this-PR specifics: code coverage —
> 12 unit tests + 4 new source-scan ratchets + 1 updated ratchet, coverage
> delta ≥ 0; audit coverage — `ws_event_audit` rows UNCHANGED (both Connected +
> Reconnected forensic rows preserved); testing coverage — unit (pure fns),
> edge/boundary (u32::MAX saturation, threshold boundaries), error-scenario
> (off-hours suppression, latch), source-scan integration ratchets; code checks —
> full pre-commit/pre-push battery, no `--no-verify`; performance — reconnect
> COLD path only, zero hot-path allocation (every new `format!` carries
> `// O(1) EXEMPT`); monitoring — 2 new static-label counters
> (`tv_order_update_outage_pages_total`, `tv_order_update_stable_reconnects_total`);
> logging — every new `error!` carries `code = ErrorCode::WsGap10OrderUpdateOutage`
> with reason labels; alerting — the [HIGH] Telegram page (bypasses the coalescer)
> + the existing `tv-prod-order-update-ws-inactive` CloudWatch alarm untouched;
> security — no secrets in any new log/message, `reason` html-escaped downstream;
> bugs fixing — closes RC1 (dead HIGH emit site) + RC2 (false-recovery storms)
> from the verified 2026-07-06 incident; scenarios — 2026-07-06 replay traced in
> Design; functionalities — no new pub fn (all new fns/consts private, each
> unit-tested + called); code review — evidence-driven design (E5/E6) + judge
> verdict; extreme check — 3 build-failing source-scan ratchets. Resilience rows:
> no new tick-drop path (order-update WAL append untouched); `SubscribeRxGuard` /
> pool watchdog untouched; no hot-path allocation; QuestDB self-heal untouched;
> O(1) per-decision (pure fns over integers/bools); composite-key/DEDUP untouched;
> real-time proof — counters + coded error! + Telegram + audit rows.
>
> Honest 100% envelope: "100% inside the tested envelope: the HIGH page is
> guaranteed for ANY outage reaching 3 consecutive in-market sub-stability
> session ends — transport-error closes AND clean closes both count (2026-07-06
> hostile-review fix; Dhan's documented auth-rejection delivery is a clean
> Close frame); upstream frame loss on a connected socket stays OUTSIDE this
> envelope (covered by the 14400s activity watchdog)."

## Plan Items

- [x] Item 1 — Archive the merged AUTH-P12 plan (Status: VERIFIED, branch merged
  to main) per plan-enforcement Phase 4; write this plan.
  - Files: .claude/plans/archive/2026-07-01-auth-p12-ip-monitor-wiring.md, .claude/plans/active-plan.md

- [x] Item 2 — Add `ErrorCode::WsGap10OrderUpdateOutage` ("WS-GAP-10",
  Severity::High, runbook `.claude/rules/project/wave-2-error-codes.md`) +
  the `## WS-GAP-10` runbook section (cross-ref test satisfied same commit).
  - Files: crates/common/src/error_code.rs, .claude/rules/project/wave-2-error-codes.md
  - Tests: test_all_variants_have_unique_code_str, test_code_str_roundtrip_via_from_str, every_error_code_variant_appears_in_a_rule_file

- [x] Item 3 — Reachable [HIGH] `OrderUpdateDisconnected` page from INSIDE the
  reconnect loop: edge-triggered once per outage episode at ≥3 consecutive
  in-market failures (Rules 3+4); latch re-arms only after a 60s-survived
  reconnect; the every-10th-failure `error!` gains `code = WS-GAP-10`
  (`reason = "threshold_streak"`).
  - Files: crates/core/src/websocket/order_update_connection.rs
  - Tests: test_should_page_outage_fires_at_threshold_in_market, test_should_page_outage_below_threshold_is_silent, test_should_page_outage_never_off_hours, test_should_page_outage_edge_triggered_once_per_episode, test_should_page_outage_fires_on_first_in_market_failure_after_off_hours_streak, test_should_page_outage_at_u32_max_saturation, test_outage_constants_pinned

- [x] Item 4 — Honest recovery: `OrderUpdateReconnected` fires ONLY after the
  new socket survives a 60s stability window (time-survival, NEVER
  first-frame-gated — the order stream is legitimately silent for hours);
  connect-edge Telegram emission deleted (both `ws_event_audit` rows kept);
  sub-stability clean closes count toward the streak (see Item 7).
  - Files: crates/core/src/websocket/order_update_connection.rs
  - Tests: test_streak_after_clean_close_resets_after_stable_session, test_should_emit_stable_recovery_requires_prior_failures

- [x] Item 5 — Dead site at the main.rs order-update spawn: the post-await
  `OrderUpdateDisconnected` notify (unreachable since the WS-GAP-04
  never-give-up rewrite) replaced with a defensive coded `error!`
  (`reason = "task_exited_unreachable"`); unused `ou_notifier` binding removed;
  events.rs doc comments updated (doc-only — templates/severities unchanged).
  - Files: crates/app/src/main.rs, crates/core/src/notification/events.rs

- [x] Item 6 — Source-scan ratchets in the existing guard file: HIGH page
  emitted inside the reconnect loop; Reconnected emission is stability-gated
  (single full-token occurrence AFTER the stability pin); main.rs has zero
  post-await OrderUpdateDisconnected emissions; the existing reconnect-notify
  ratchet message updated.
  - Files: crates/core/tests/ws_telegram_visibility_guard.rs
  - Tests: order_update_high_page_is_emitted_inside_reconnect_loop, order_update_reconnected_is_stability_gated_not_connect_edge, main_rs_does_not_emit_order_update_disconnected_after_task_await, order_update_connection_fires_reconnect_notify

- [x] Item 7 — Hostile-review fix (2026-07-06, same day): clean-close outages
  must page. Any session ending BEFORE the 60s stability window increments the
  streak regardless of delivery mode (`streak_after_clean_close(prev,
  stability_reached)` → `prev.saturating_add(1)` when unstable, 0 when
  stable); the page decision (`should_page_outage`) is evaluated in BOTH
  reconnect-loop arms via the shared `emit_in_market_outage_page` helper; the
  off-hours clean-close flap enters the same WS-GAP-04 sleep after the attempt
  budget (`decide_reconnect_action` parity). Closes the mixed regime (≤2
  errors then one clean close, repeating) that perpetually defeated the
  threshold, and the pure clean-close dead-token regime (the file's own
  documented auth-rejection delivery mode).
  - Files: crates/core/src/websocket/order_update_connection.rs, crates/core/tests/ws_telegram_visibility_guard.rs, .claude/rules/project/wave-2-error-codes.md
  - Tests: test_streak_after_clean_close_counts_as_failure_when_unstable, test_streak_after_clean_close_mixed_regime_reaches_page_threshold, test_streak_after_clean_close_resets_after_stable_session, test_streak_after_clean_close_saturates_at_u32_max, order_update_clean_close_counts_as_failure_and_can_page

- [ ] Item 4 — THE ONLY RUST ITEM: new gauge `tv_dhan_exchange_lag_p99_seconds` (unlabeled, dhan-only NAME — sidesteps the host-only EMF dimension label-folding trap; a future Groww gauge gets its own name). HOT PATH (tick_processor.rs dhan persist site, `received_at_nanos` + `capture_seq` in scope — verify at impl time): `lag_ns = received_at_nanos.saturating_sub(exchange_ts × 1e9)`, clamp negatives to 0 (count clamps), write into a preallocated single-writer 32,768-slot ring with relaxed atomic head (~65s headroom at 500 ticks/s) — O(1), zero-alloc, DHAT + Criterion ratcheted + benchmark-budgets entry. REPLAY EXCLUSION discriminator: admit only if `received_at_nanos − capture_seq < REPLAY_EXCLUDE_DWELL_NANOS = 60_000_000_000` (named constant) — capture_seq is stamped once at the original WS-read instant and PRESERVED through WAL re-injection while received_at is RE-stamped at dequeue, so a replayed row shows receipt−capture = downtime (≥minutes, excluded EXACTLY) while genuinely-lagged live rows (the incident's real 46s/199s) have FRESH capture instants and are KEPT — no Rule-11 censoring of the measured signal. Every exclusion increments `tv_dhan_lag_samples_excluded_total` (/metrics-only; visible, never silent). PUBLISHER: supervised 10s cold task (WS-GAP-05/SLO-03 respawn pattern) in NEW module `crates/core/src/pipeline/feed_lag_monitor.rs`, spawned in `start_dhan_lane` next to the SLO publisher: snapshot trailing-60s window into preallocated scratch, p99 via `select_nth_unstable` — honestly labeled **O(N-window)**, N≤32,768, off the tick thread (NEVER claimed O(1)). Publish ONLY when (a) inside the trading session (Rule 3 — prevents the stale-gauge-after-close artifact) AND (b) window holds ≥ `MIN_LAG_SAMPLES = 50` (empty/thin window publishes NOTHING — 0 = "perfect lag" is a Rule-11 false-OK; feed-dead is owned by the silent-instruments + WS alarms via notBreaching). QUANTIZATION HONESTY (module doc + alarm_description + metrics catalog + portal caveat): Dhan LTT is u32 whole IST SECONDS → ≥1s floor; healthy p99 reads ~1–2s and can never read 0; sub-second wire lag UNMEASURABLE for feed=dhan; alarm threshold sits 10× above the floor. EXPORT: add the gauge to both allowlist regexes (+$0.30/mo, part of the 21→23 pin bump). ALARM (`silent-feed-alarms.tf`): `dhan_exchange_lag_p99_high` — `GreaterThanThreshold` 10 (seconds), period=60, statistic=Maximum, strict 10-of-10 (safe HERE unlike items 1/2: the metric is itself a trailing-60s p99 recomputed every 10s, so a one-burst transient decays out within ~60s and cannot hold 10 consecutive breaching minutes; incident all-day p99 46s pages at minute 10), `notBreaching`, `actions_enabled=false` + gate.
  - Files: crates/core/src/pipeline/feed_lag_monitor.rs, crates/core/src/pipeline/tick_processor.rs, crates/core/src/pipeline/mod.rs, crates/app/src/main.rs, deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/user-data.sh.tftpl, deploy/aws/terraform/silent-feed-alarms.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, quality/benchmark-budgets.toml, crates/core/tests/dhat_feed_lag_ring.rs
  - Tests: test_replay_dwell_boundary_excludes_at_exactly_60s, test_thin_window_publishes_nothing, test_p99_on_known_distribution, test_ring_wraparound, test_negative_lag_clamped, test_out_of_session_publishes_nothing

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 2026-07-06 replay: dead token, RST ~10ms after login, 39+ in-market failures | ONE [HIGH] page ≈1s in (failure 3); zero false Low "reconnected"; coded error! at 10/20/30 |
| 2 | Operator fixes token; connect survives 60s | ONE [LOW] reconnected; latch re-armed; streak 0 |
| 3 | Off-hours failure streak of any length | No HIGH page (existing Low/audit semantics stand) |
| 4 | Streak accumulated off-hours crosses 09:00 IST | HIGH page on FIRST in-market failure at/above threshold (`>=` latch) |
| 5 | Clean close mid-paged-episode | Streak keeps counting; latch prevents duplicate page; exponential backoff, no silent 0ms flap loop |
| 6 | Pure clean-close dead-token flap (in-market) | Streak 1→2→3 → ONE [HIGH] page (hostile-review fix — was silent) |
| 6b | Mixed regime: 2 errors + 1 clean close, repeating | Clean close is failure #3 → ONE [HIGH] page (was perpetually defeated) |
| 6c | Off-hours clean-close flap | Counts + capped backoff, then WS-GAP-04 sleep-until-open after the attempt budget; no HIGH page (market-hours gate) |
| 6d | Healthy 4h session ends with a clean close | Stability was reached → streak resets to 0; nothing counts, nothing pages |
| 7 | Idle day: zero frames for hours on a healthy socket | Stability window still completes at 60s (time-survival, not frame-gated) |
| 8 | Socket dies at 59.9s | No recovery Telegram; latch stays armed (correct) |

## Design

RC1 (dead HIGH emit) + RC2 (false-recovery storms), both verified 2026-07-06
(evidence: E5/E6). The fix:

1. **WS-GAP-10 error code** (`WsGap10OrderUpdateOutage`, Severity::High,
   wave-2 runbook) tagging three events by `reason` label: `in_market_outage`
   (the once-per-episode HIGH page), `threshold_streak` (the every-10th-failure
   persist error), `task_exited_unreachable` (the defensive main.rs log).
2. **In-loop edge-triggered page**: private pure fn
   `should_page_outage(within_market_hours, consecutive_failures, already_paged)`
   — fires when in-market AND not already paged this episode AND
   streak ≥ `ORDER_UPDATE_OUTAGE_PAGE_FAILURE_THRESHOLD` (3). Uses `>=` (not
   `==`) so an off-hours-accumulated streak pages on the first in-market
   failure. Wired into the `IncrementAndRetry` arm of
   `run_order_update_connection`; market-hours input is the EXISTING
   `is_within_market_hours(&calendar)` (trading-day + [09:00,16:00) IST).
   [HIGH] severity bypasses the coalescer → instant Telegram. Telegram body
   passes the 10 commandments (plain English, specific numbers, action verbs).
3. **60s time-survival recovery gate**: `connect_and_listen` gains a
   `stability_reached: &mut bool` param + a `tokio::select!` arm on a pinned
   60s sleep (`ORDER_UPDATE_RECONNECT_STABILITY_SECS`). The arm sets the flag
   unconditionally (so the episode re-arm always works) and emits
   `OrderUpdateReconnected` only when `failures_before_attempt > 0`
   (`should_emit_stable_recovery`). TIME-survival, NOT first-frame gating: the
   order stream is legitimately silent for hours (watchdog threshold history
   660s→1800s→14400s), so a frame gate would never fire. 60s is far beyond the
   observed die-after-login window (~10ms) and far below the 14400s watchdog.
   The connect-edge Telegram emission is DELETED; both audit rows stay.
4. **Episode lifecycle**: `outage_paged` latch in the outer loop. Re-armed
   (with streak reset) when a connect attempt reaches the stability window;
   also re-armed by the WS-GAP-04 post-close sleep and the PR-E Dhan
   re-enable (fresh episodes). Hostile-review fix (2026-07-06, same day): a
   session ending BEFORE the stability window is a FAILURE regardless of
   delivery mode — `streak_after_clean_close(prev, stability_reached)`
   increments on a sub-60s clean close (Dhan's documented auth-rejection
   delivery is a clean Close frame) and resets only on stability survival.
   The `Ok(())` arm evaluates the SAME `should_page_outage` decision and
   routes through the shared `emit_in_market_outage_page` helper, plus the
   same off-hours WS-GAP-04 exhaustion path (`decide_reconnect_action`
   parity). Normal idle-day clean closes (sessions living minutes/hours)
   reached stability and never count.
5. **Dead site**: main.rs post-await notify → defensive coded `error!`
   (`task_exited_unreachable`); the fast-boot spawn site needs no change (no
   post-await code; the in-loop page lives in the shared core fn — boot
   symmetry by construction).

2026-07-06 event-flow replay: 14:05:49 watchdog kills 4h-silent socket
(session had passed 60s → episode fresh) → RST failures 1 (0ms), 2 (0ms),
3 (500ms) → ONE [HIGH] Telegram ≈1s in + `error!(code=WS-GAP-10)` → failures
4..39: warn! each, coded error! at 10/20/30, ZERO further Telegram (no connect
survives 10ms → stability never fires → no false Low) → operator fixes token →
connect survives 60s → ONE [LOW] reconnected → latch re-armed, streak 0.

## Edge Cases

- Off-hours suppression: `should_page_outage` requires `within_market_hours`;
  existing off-hours Low/audit semantics stand untouched.
- Off-hours streak crossing 09:00 IST: pages on first in-market failure
  (`>=` with latch).
- Sub-stability clean close: counts as a failure (increments streak) and can
  page — in-market pure clean-close and mixed regimes both reach the 3-failure
  threshold; the paged latch still prevents duplicate pages.
- Off-hours clean-close flap: counts + capped exponential backoff, then the
  WS-GAP-04 sleep-until-open after the attempt budget (parity with the error
  arm); never a HIGH page off-hours.
- Healthy long-lived session ending in a clean close: `stability_reached`
  is true → streak resets to 0; nothing counts.
- PR-E Dhan runtime toggle + WS-GAP-04 post-close sleep both re-arm the
  episode (fresh streak + latch).
- Boot-time in-market failure streak pages at 3 like any other episode.
- u32 saturation: `saturating_add` streak; `should_page_outage(true, u32::MAX,
  false)` fires (tested).
- `notifier = None` (tests / minimal wiring): coded `error!` + counter still
  fire; only the Telegram leg is skipped.
- Idle-day zero-frame sockets legitimately count stable at 60s (time-survival
  by design).
- Completed tokio Sleep is never re-polled: the select arm carries an
  `if !*stability_reached` guard (load-bearing — re-polling a completed Sleep
  panics).

## Failure Modes

- Telegram send failure → TELEGRAM-01 path; the coded `error!` still reaches
  errors.jsonl/CloudWatch independently (5-sink chain).
- Socket dying at 59.9s emits nothing and the latch stays armed (correct —
  not a recovery).
- Pathological connect→survive-60s→die metronome re-pages at most ~1 HIGH per
  ~61s worst case (stability window + 3-failure ladder) — bounded, and each
  cycle is a genuine outage+recovery (runbook documents it).
- `/health` order-update connected-flag never flipping false during an in-loop
  outage is a PRE-EXISTING gap — flagged follow-up, NOT fixed here (scope
  lock).
- A future refactor that makes `run_order_update_connection` return is caught
  loudly by the defensive `task_exited_unreachable` error! at the spawn site.

## Test Plan

- Unit tests (crates/core, `order_update_connection.rs::tests`): 12 tests
  over the 3 private pure fns + pinned constants (see Items 3+4+7 test lists).
- Source-scan ratchets (crates/core/tests/ws_telegram_visibility_guard.rs):
  4 new + 1 updated (see Items 6+7).
- Existing tests kept byte-identical-green:
  test_first_reconnect_attempt_is_zero_ms_instant, test_backoff_calculation,
  all test_compute_reconnect_backoff_*, all test_reconnect_action_*,
  order_update_watchdog_threshold_is_at_least_1800_secs,
  test_order_update_reconnected_severity_is_low,
  test_order_update_disconnected_escapes_external_reason.
- Command battery: `cargo fmt --check`; `cargo clippy -p tickvault-common -p
  tickvault-core -p tickvault-app --no-deps -- -D warnings`; `cargo test -p
  tickvault-common --lib --tests` (common touched → error-code catalogue +
  cross-ref + tag-guard); `cargo test -p tickvault-core --lib --tests`;
  `cargo test -p tickvault-app --lib`; plan-gate + plan-verify hooks.
- Negative ratchet self-check: temporarily re-add a connect-edge Reconnected
  emit → ratchet must FAIL; temporarily restore the main.rs notify → ratchet
  must FAIL; revert both (noted in PR body).

## Rollback

`git revert` of the code commits restores prior behavior exactly; no
schema/config/data migration; the ErrorCode variant is additive and inert
after emit-site revert. No config toggle by design: operator-paging
correctness must not be switch-off-able (audit Rule 11 no-false-OK); B12
feature-flag rollback judged N/A with this rationale.

## Observability

- `error!` sites carry `code = WS-GAP-10` with reason labels
  `in_market_outage` / `threshold_streak` / `task_exited_unreachable` →
  the 5-sink chain (stdout / app.log / errors.log / errors.jsonl /
  CloudWatch).
- New static-label counters: `tv_order_update_outage_pages_total` (one per
  HIGH page) + `tv_order_update_stable_reconnects_total` (one per survived
  60s recovery). Panel/alert guard files
  (`operator_health_dashboard_guard.rs` / `resilience_sla_alert_guard.rs`)
  verified ABSENT — retired with the CloudWatch-only migration.
- `ws_event_audit` rows unchanged (Connected / Reconnected / Disconnected /
  Sleep rows all preserved).
- The `tv-prod-order-update-ws-inactive` CloudWatch alarm untouched.
- Runbook: `## WS-GAP-10` section in
  `.claude/rules/project/wave-2-error-codes.md` (trigger, incident evidence,
  triage, honest envelope).
