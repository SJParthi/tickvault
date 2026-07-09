# Implementation Plan: One live-edited boot bubble + change-only budget pings

**Status:** APPROVED
**Date:** 2026-07-09
**Approved by:** Parthiban (operator escalation 2026-07-09 — Telegram noise N1 boot spray / N2 hourly budget pings / N3 redundant per-component LOW confirmations)

## Plan Items

- [x] PR-1a — `EpisodeFamily::Boot` + `BootMilestone` + `BootChecklist` + boot renders in the pure episode core
  - Files: crates/core/src/notification/episode.rs
  - Tests: test_boot_checklist_fold_idempotent_and_order_free, test_render_boot_checklist_first_mid_final_under_ceiling, test_render_boot_checklist_lazy_mode_without_expectations, test_render_boot_checklist_honest_no_real_ticks_line, test_render_boot_checklist_keeps_tickvault_started_phrase, test_render_boot_checklist_deferred_off_hours_line, test_snapshot_excludes_boot_family, test_tick_never_closes_or_expires_boot_family, test_retire_boot_only_after_complete_plus_window, test_episode_config_for_boot_zero_throttle_ws_default, test_post_retire_boot_event_opens_fresh_mini_bubble, test_is_complete_only_after_complete_fold, test_set_boot_flavor_creates_and_updates_checklist, test_mark_boot_delivered_and_boot_redrive_candidate_dirty_flow
- [x] PR-1b — `episode_key()` boot arm (Groww-gated feed events), `boot_milestone()`, `OrderUpdateAuthenticated` Medium→Low
  - Files: crates/core/src/notification/events.rs
  - Tests: test_boot_milestone_mapping_all_variants, test_episode_key_boot_variants_map_to_boot_family, test_episode_key_non_groww_feed_is_none, test_boot_episode_key_only_success_milestones, test_order_update_authenticated_severity_is_low, test_episode_key_none_for_non_episode_variants (updated)
- [x] PR-1c — boot dispatcher (serialized fold→render→send/edit), ticker re-drive + retire, sha flavor, expectations, `boot_bubble` gate
  - Files: crates/core/src/notification/service.rs
  - Tests: test_dispatch_boot_serializes_concurrent_milestones_single_bubble, test_boot_first_milestone_sends_then_second_edits_same_id (asserts the completed render carries "tickvault started"), test_boot_bubble_mode_off_is_legacy_byte_identical, test_boot_redrive_ticker_retries_failed_first_page, test_is_new_code_deploy_fail_open_and_unknown_sha, test_init_boot_sha_flavor_never_panics_and_stamps_flavor
- [x] PR-1d — `[notification] boot_bubble = true` config key
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_notification_config_defaults (extended), test_notification_legacy_toml_parses (extended)
- [x] PR-1e — demote the instrument-load timing essay to a log; wire `set_boot_expectations` in both boot lanes; guard update
  - Files: crates/app/src/main.rs, crates/storage/tests/daily_universe_scope_guard.rs
  - Tests: daily_universe_scope_guard (updated pin — log-only demotion)
- [x] PR-1f — fold the deploy notice into the boot bubble (delete the workflow success publish)
  - Files: .github/workflows/deploy-aws.yml
  - Tests: n/a (workflow YAML; failure-path publishes retained)
- [x] PR-1g — episode wiring guard extended additively for the boot call sites + constants
  - Files: crates/core/tests/episode_edit_wiring_guard.rs
  - Tests: guard_c/guard_d updated counts + new boot pins
- [x] PR-2a — change-only budget running pings (state in SSM; breach stop never state-gated)
  - Files: deploy/aws/lambda/hard-stop-guard/handler.py, deploy/aws/lambda/hard-stop-guard/test_handler.py
  - Tests: test_same_bucket_silent, test_bucket_increase_pings, test_bucket_decrease_pings, test_bucket_decrease_one_step_silent_hysteresis, test_month_rollover_pings_once, test_cost_unknown_edge_latched, test_cost_recovered_pings, test_state_missing_pings_and_seeds, test_ssm_read_failure_fails_open_to_ping, test_ssm_write_failure_still_returns_ok, test_breach_stop_ignores_ping_state, test_approaching_wording_at_bucket_8_and_9
- [x] PR-2b — hard-stop-guard IAM + env for the ping-state SSM param
  - Files: deploy/aws/terraform/budget-guards.tf
  - Tests: n/a (terraform; least-privilege single-ARN statement)
- [x] PR-3 — 2026-07-09 hostile-review fixes: honest post-retire MINI "Feed update" bubble (neutral header, no "Boot finishing" promise, idle retirement — never a false "tickvault starting" from a mid-day feed/lane restart); name-visible tests for the 4 new registry pub fns (pub-fn ratchet back to baseline); run_boot_tick transient edit failures defer through the same 2-failure ladder as the milestone path (honest transient_deferred/transient_exhausted/not_found metric reasons); sha-store write failure clears the stale sha (never a repeated false "NEW CODE" claim); MarketOpenReadinessConfirmation added to the #1443 instant-lane pin; budget bucket-decrease hysteresis (1-bucket CE jitter silent, ≥2-bucket credit pings); comment truth-ups (boot_bubble requires episode_mode; deploy-provenance gate comment)
  - Files: crates/core/src/notification/episode.rs, crates/core/src/notification/service.rs, crates/core/src/notification/events.rs, config/base.toml, .github/workflows/deploy-aws.yml, deploy/aws/lambda/hard-stop-guard/handler.py, deploy/aws/lambda/hard-stop-guard/test_handler.py
  - Tests: test_post_retire_boot_event_opens_fresh_mini_bubble, test_is_complete_only_after_complete_fold, test_set_boot_flavor_creates_and_updates_checklist, test_mark_boot_delivered_and_boot_redrive_candidate_dirty_flow, test_bucket_decrease_one_step_silent_hysteresis

## Design

**N1 (boot spray) — one consolidated boot bubble, edited in place.** The #1439 episode
live-edit machinery gains a third family, `EpisodeFamily::Boot`, with a single fixed key
(`BOOT_EPISODE_KEY`, conn 0). Boot-milestone events (`AuthenticationSuccess`,
`InstrumentBuildSuccess`, `WebSocketPoolOnline`, `WebSocketPoolDeferredOffHours`,
`OrderUpdateConnected`, `OrderUpdateAuthenticated`, `BootHealthCheck`, `StartupComplete`,
plus the Groww-feed trio `FeedAuthOk`/`FeedInstrumentsLoaded`/`FeedConnectedAwaitingTicks`
gated on `feed == "Groww"`) map to that key via `episode_key()` (still a zero-alloc match)
and carry a typed `BootMilestone` payload via a new `boot_milestone()` accessor. The
registry holds ONE `BootChecklist` (per-line Option slots, idempotent + commutative
`fold`) behind its own mutex; `apply_boot_milestone` folds the milestone then delegates
to the EXISTING pure FSM (`next_episode_action`, role Open, severity Low) purely for the
message-id / fallback mechanics. The transport half of `dispatch_episode_event` is reused
verbatim (id-capture send, fnv1a hash-skip, not-found fallback fresh send, transient
ladder, TELEGRAM-01/03 loudness); the ONLY boot-specific render is
`render_boot_checklist` (fixed template order, IST 12-hour, ≤ `BOOT_BUBBLE_MAX_CHARS`).
Two correctness grafts over a naive FSM-ride: (1) a `boot_dispatch_lock`
(`tokio::sync::Mutex<()>`) serializes the whole fold→render→send/edit body so two boot
milestones racing the in-flight first send can never open duplicate bubbles; (2) a
`dirty` flag on the checklist plus a boot arm in `run_episode_tick` re-drives a
throttled/deferred/failed final edit within ≤10s (there is no "next milestone" after
`StartupComplete`). Boot is EXCLUDED from the registry tick's green-promote and
stale-expire scans and from the snapshot codec (a new process must never edit the
previous process's bubble); a completed checklist retires silently
`BOOT_BUBBLE_RETIRE_SECS` (900s) after `Complete` — later mapped events (mid-day lane
cold start / feed re-enable) open a fresh mini-checklist bubble. `episode_config_for`
gives Boot a 0s edit throttle (bounded by ≤ ~12 milestones/process + hash-skip); WS
families keep the pinned defaults. Deploy detection: `data/notify/last-boot-sha` vs
`build_info::BUILD_GIT_SHA` (both must be valid 7–40 lowercase hex AND differ to claim
"NEW CODE"; `unknown`/fs errors ⇒ plain header, fail-open). The deploy workflow's
"✅ tickvault deployed" SNS publish is deleted in the same PR (the bubble header replaces
it — atomic signal handoff). N3: the instrument-load timing essay (`Custom` at
main.rs) demotes to a structured `info!`; the counts live on the bubble's Instruments
line. `OrderUpdateAuthenticated` Medium→Low so folding it can never flip the green
checklist amber; the bubble prefix is FIXED at `telegram_message_prefix(Severity::Low)`.
Kill switch: `[notification] boot_bubble` (default true) — `false` ⇒ boot events fall
through to their UNCHANGED `DispatchPolicy::Immediate` legacy lanes (today's spray,
byte-identical), independent of `episode_mode` (#1439 WS episodes untouched by the
rollback). The FAST-boot "crash recovery: ticks flowing" `Custom` stays standalone
(mid-market signal, not in the operator's N1 list).

**N2 (hourly budget ping) — change-only.** Source found: the hard-stop-guard Lambda's
in-window arm publishes an unconditional hourly "[BUDGET] box still running" SNS ping.
It becomes change-only via one SSM String param
(`/tickvault/prod/budget-guard/ping-state`, JSON `{"month","bucket","outcome"}`):
pure `spend_bucket` (10% steps of the stop-budget) + `classify_ping_decision` (reasons:
`ping_state_missing` fail-open · `ping_new_month` · `ping_cost_unknown_edge`
edge-latched · `ping_cost_recovered` falling edge · `ping_bucket_changed` either
direction · `silent`). `classify_in_window_action` is evaluated FIRST and `breach_stop`
returns via `_execute_breach_stop` BEFORE any state read — the stop is never state-gated;
the out-of-window force-stop and the existing 17:30 IST daily digest (the EOD summary)
are untouched. Subject flavor by bucket (≥9 → "approaching stop-budget — 90% used" 🔴,
=8 → 80% ⚠, else "spend crossed N0%"). IAM: one added least-privilege statement
(`ssm:GetParameter`+`PutParameter` on the single ping-state ARN) + `PING_STATE_PARAM`
env; no seed resource (`PutParameter(Overwrite=True)` creates it; the one-time
state-missing ping is the designed first run).

## Edge Cases

- Out-of-order / concurrent boot milestones: `boot_dispatch_lock` serializes; `fold` is
  idempotent + commutative, so duplicates and any arrival order converge to one render.
- A pool-online (or any mapped event) arriving AFTER `StartupComplete`: folds into the
  completed bubble inside the 900s retire window; after retire, a fresh mini bubble opens
  (correct for a mid-day D2b lane cold start / feed re-enable — one small bubble, never
  an edit of the morning's message).
- Non-Groww `Feed*` events (future feed #3): `episode_key()` returns `None` → legacy
  immediate lane, unchanged.
- Unknown / missing build sha, unreadable/unwritable `data/notify/`: plain header, never
  a false "NEW CODE" claim, never boot-blocking (fail-open `new_code=false` + debug log).
- NoOp notifier mode: boot dispatcher returns before any state/transport work.
- Off-hours boot: `WebSocketPoolDeferredOffHours` renders the 🌙 waiting-for-market-open
  line instead of leaving a ⏳ forever.
- Expectations unset (event arrives before `set_boot_expectations`): lazy render — only
  lines with data + "⏳ Boot finishing…" (no phantom pending lines).
- Budget: month rollover pings once with its own reason; a bucket DECREASE (mid-month
  credit/refund) pings; a manual weekend start hits the untouched out-of-window arm;
  corrupt/typed-wrong state JSON classifies as `ping_state_missing` (fail-open ping).

## Failure Modes

- First boot send fails (Telegram down): TELEGRAM-01 error! + counter (unchanged
  loudness); checklist stays dirty; the ticker re-drive (≤10s) or the next milestone
  retries via the FSM's `SendNewFallback` with the FULL checklist — never a drop.
- Edit → 400 not-found (bubble deleted / >48h): existing fallback rung sends a fresh
  full-checklist bubble + replaces the id (`tv_telegram_edit_fallback_total{not_found}`).
- Edit → transient exhausted: sub-threshold stays counted + TELEGRAM-03 error!-loud and
  the dirty re-drive retries ≤10s; at threshold the fallback fresh send fires
  (duplicate-over-drop).
- Hung boot (milestone never arrives): the bubble honestly sits at ⏳ forever in-process
  (never retired while incomplete); loudness comes from the UNTOUCHED
  AuthenticationFailed / InstrumentBuildFailed / WebSocketPoolPartialAfterDeadline /
  BootDeadlineMissed CRITICAL/HIGH pages.
- `data/notify/` unwritable: sha flavor degrades to plain header; bubble delivery
  unaffected.
- Budget SSM read failure: fail-open → ping stands (one ping beats a silent guard).
- Budget SSM write failure: logged, handler still returns ok; worst case one duplicate
  ping per hour until the write lands (duplicate-over-silent).
- Breach during a ping-state outage: `breach_stop` is evaluated before any state read —
  the stop can never be blocked by ping-state problems (ratcheted by
  test_breach_stop_ignores_ping_state).

## Test Plan

The per-item test lists above, plus the verbatim gate commands:

```
cargo fmt --check
cargo clippy -p tickvault-core --no-deps -- -D warnings -W clippy::perf
cargo clippy -p tickvault-app  --no-deps -- -D warnings -W clippy::perf
cargo test -p tickvault-core --lib notification
cargo test -p tickvault-app --lib
cargo test -p tickvault-core --test episode_edit_wiring_guard
cargo test -p tickvault-storage --test daily_universe_scope_guard
python3 -m pytest deploy/aws/lambda/hard-stop-guard/test_handler.py
```

No-regress pins kept green: #1439 WS episode UX (constants + FSM + transport untouched
for WS families), #1443 instant lane (MarketOpenReadinessConfirmation /
MarketOpenStreamingConfirmation / SelfTestPassed keep `DispatchPolicy::Immediate` and
map to NO episode), HIGH/CRITICAL instant paging + SNS-SMS leg, ws_event_audit emit
choke points (guard_g), the 10 Telegram commandments (render tests scan banned strings).

## Rollback

- `[notification] boot_bubble = false` ⇒ boot events fall through to their unchanged
  `DispatchPolicy::Immediate` legacy dispatch — byte-identical to today's spray; #1439
  WS episodes unaffected.
- `[notification] episode_mode = false` ⇒ full legacy dispatch including WS events
  (pre-#1439 behavior), as before.
- Budget: `git revert` the handler.py commit — the SSM state param becomes inert (no
  reader); the old hourly ping returns.
- Deploy notice: restore the deleted workflow step from git history.

## Observability

- ZERO new metrics families and ZERO new ErrorCodes: the boot bubble reuses
  `tv_telegram_episode_events_total{action=open|edit|edit_throttled}`,
  `tv_telegram_edit_fallback_total{reason}` and TELEGRAM-01 / TELEGRAM-03 with their
  existing documented reasons (no `error_code_rule_file_crossref` churn).
- Budget Lambda return dict gains `ping_reason` (CloudWatch logs show every hourly
  decision); expected Telegram volume drops from ~8 identical pings/day to ~1-2
  change pings/week + the untouched 17:30 IST daily digest.
- Boot bubble timeline is itself the observability: one message that ends as the
  full boot checklist with IST timestamps.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Normal evening redeploy ×2 | 2 boot bubbles (one per boot), each editing itself to the ✅ checklist; no deploy notice; no per-component LOW bubbles |
| 2 | Boot with Telegram flapping | dirty re-drive ≤10s; duplicate-over-drop; TELEGRAM-01/03 loud |
| 3 | Mid-day Groww re-enable 2h after boot | fresh mini-checklist bubble (post-retire) |
| 4 | Budget: spend 31%→33% across 3 hours | silence (same bucket) |
| 5 | Budget: spend crosses 40% | one "[BUDGET] spend crossed 40%" ping |
| 6 | Budget: CE down 3 hours then recovers | one cost-unknown ping + one recovered ping |
| 7 | Budget breach with SSM down | box STOPPED + start rule disabled (state never consulted) |

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` — the 15-row
"100% everything" matrix and the 7-row resilience matrix apply to every item above;
this plan's specific proofs: ratcheted wiring-guard extensions (PR-1g), pure-function
unit tests on every new fold/classify/render primitive, zero new hot-path code (all
cold-path notification/deploy/Lambda surfaces), no new tick-drop path, no DEDUP or
storage-schema change, composite-key rules untouched. Honest 100% claim: "100% inside
the tested envelope, with ratcheted regression coverage: mutex-serialized
fold→render→edit, ticker re-drive ≤10s for any deferred edit, duplicate-over-drop on
every transport failure with TELEGRAM-01/03 loudness unchanged. NOT claimed:
cross-restart bubble continuity (a crash mid-boot leaves the old bubble honestly frozen
at ⏳), deploy detection when the binary embeds an `unknown` build sha, or fewer
`ShutdownInitiated` one-liners (the ⏹-fold is a flagged follow-up)."
