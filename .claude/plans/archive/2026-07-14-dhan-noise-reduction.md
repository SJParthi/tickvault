# Implementation Plan: Dhan REST-Only Noise Reduction (2026-07-14 operator lock)

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — 2026-07-14 directive relayed verbatim via the coordinator session: "for Dhan except spot 1m and option chain nothing else should work… these fucking issues of mid profile and all other fucking issues of Dhan should be entirely removed… always make the telegram messages/notifications cleaner, always mention precisely which broker."

> **Guarantee matrices:** this plan carries the 15-row + 7-row guarantee
> matrices BY CROSS-REFERENCE to the canonical
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the house-approved
> brevity form). Per-item specifics: every deletion below is pinned by an
> updated/negative ratchet test named in its Tests: list; no hot-path code is
> touched (all changes are cold-path spawn/notification/terraform surface);
> no new tick-drop path is introduced; DEDUP keys and the WS resilience chain
> are untouched.

## Design

Changed crates: **core, app, common** (plus terraform under `deploy/aws/`).

After this PR (+ PR #1522, which deletes the Dhan live-WS lane and merges
independently — this branch is cut from current main), the ONLY Dhan Telegram
alerts that can fire are: (1) DHAN spot-1m pull failing/recovered (`Spot1m*`
family), (2) DHAN option-chain pull failing/recovered (`Chain*` family),
(3) DHAN token-unobtainable Critical (`AuthenticationFailed` /
`TokenRenewalFailed`, reworded to name the broker + the consequence), and
(4) the CloudWatch `tv-<env>-token-remaining-low` 4h early warning (kept;
its Lambda wording is another session's scope). Everything else Dhan is
deleted or silenced per `.claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md`
(the rule edits land in this PR BEFORE the code, house protocol):

- `crates/app/src/dhan_rest_stack.rs`: Phase 5a (order-update WS spawn +
  drain + auth listener + audit consumer) and Phase 5b (REST canary spawn)
  deleted; Phase 3 gains the GAP-02 stale-token sweep + the GAP-06 re-homed
  token-health gauge poller.
- `crates/core/src/auth/mid_session_watchdog.rs`: the two Telegram sends
  removed (probe + AUTH-GAP-05 re-mint stay, silent); the terminal re-mint
  failure emits the family-(3) `AuthenticationFailed` Critical (verified:
  `force_renewal` → `acquire_token` pages nothing on a non-RESILIENCE-03
  permanent failure); GAP-04 re-arms the retry-once latch every 2nd failing
  900s cycle.
- Deleted modules: `crates/core/src/pipeline/no_tick_watchdog.rs` (+ the
  `NoLiveTicksDuringMarketHours` variant), `crates/core/src/auth/fast_boot_validation.rs`,
  `crates/app/src/rest_canary_boot.rs`. Deleted variants:
  `MidSessionProfileInvalidated`, `TokenForcedRemintTriggered`,
  `NoLiveTicksDuringMarketHours` (ErrorCode variants all RETAINED until C4).
- `crates/core/src/auth/token_health_gauge.rs` RE-HOMED (GAP-06): module
  kept, lane/fast-arm spawn sites in main.rs deleted, spawned from
  `dhan_rest_stack` Phase 3 instead.
- Terraform: `order_update_ws_inactive` alarm +
  `order-update-reconnect-storm-alarm.tf` + the `rest-canary-01`
  error-code-alarms map entry deleted; the market-hours gate ALARM_NAMES
  list and count comments updated; `tv-<env>-token-remaining-low` untouched.
- Plan housekeeping: the two merged-PR plans (#1524 spot-1m diagnostics,
  #1504 questdb-partition-s3-archive) are archived in this PR per
  plan-enforcement rule 7 (coordinator-approved) so the plan-gate V7 cap
  holds at 5 active plans including this one.

## Edge Cases

- A terminally-dead token with the Telegram pages removed: the watchdog's
  terminal arm now emits `AuthenticationFailed` (Critical) directly —
  Rule 11, no silent terminal failure.
- A Dhan-side-KILLED but locally-fresh token AFTER market close: the profile
  probe is market-hours-gated and the 4h sweep re-mints only on <4h local
  headroom, so the 15:33:30 post-close spot sweep can still fail on such a
  token until the next boot re-mints — honest, documented limit in the lock
  file §4 (the in-session surface is covered).
- GAP-04 latch re-arm must NOT bypass the ~125s mint cooldown or the
  RESILIENCE-03 lock refusals (pure-fn tests pin retry on failing cycles
  2, 4, 6, …; clean cycle resets; cooldown/lock arms unchanged).
- The paging drift guard (`error_code_paging_filter_drift_guard.rs`) pins
  tf ↔ doc ↔ emit three ways: the `rest-canary-01` map entry, the doc list
  line, and the emit site are removed in lockstep (entry count 10 → 9 keeps
  the ≥9 self-check green; the doc removal note sits outside the parsed
  paragraph).
- Deleting the order-update spawn with the module retained: the negative
  ratchet in dhan_rest_stack asserts the production region contains NO
  `run_order_update_connection(` call; the module's own unit tests stay.
- `run_tick_processor`'s `tick_heartbeat` param stays (both main.rs call
  sites pass `None`) — removing the param would churn the Dhan pipeline
  #1522 deletes wholesale.

## Failure Modes

- Silent terminal token failure (the removed pages masking a dead token):
  prevented by the watchdog terminal-arm `AuthenticationFailed` emit + the
  kept `tv-<env>-token-remaining-low` alarm + the legs' own 3-minute
  escalation edges (`Spot1mFetchDegraded`/`ChainFetchDegraded`).
- A wedged/halted renewal loop (circuit-breaker return kills the in-loop 30s
  gauge writer): covered by GAP-02 (the 900s stack sweep re-mints on <4h
  headroom) and GAP-06 (the independent 15s poller keeps
  `tv_token_remaining_seconds`/`tv_token_valid` alive so alarm #4 stays
  sighted).
- A failed forced re-mint previously latched forever (one attempt per
  episode): GAP-04 retries every ~30 min while the episode persists.
- Terraform reference breakage: the gate Lambda's ALARM_NAMES join drops the
  deleted storm alarm's resource reference in the same commit (a dangling
  reference fails `terraform validate`).

## Test Plan

Block-scoped per `testing-scope.md`: `cargo fmt --check`,
`cargo clippy -p tickvault-core -p tickvault-app -p tickvault-common -- -D warnings -W clippy::perf`,
`cargo test -p tickvault-core -p tickvault-app -p tickvault-common`
(common carries the terraform-pinning tests), plus
`bash .claude/hooks/banned-pattern-scanner.sh`,
`bash .claude/hooks/plan-verify.sh`,
`bash .claude/hooks/per-item-guarantee-check.sh`,
`bash .claude/hooks/plan-gate.sh "$PWD"`. Per-item ratchets are named in
each plan item's Tests: list below.

## Rollback

Every change is a plain git revert of this branch's commits (no schema
changes, no data migration, no config-default flips). The order-update
spawn, the canary, and the watchdog pages are restorable from git history;
re-introducing ANY of them requires a fresh dated operator quote per
`dhan-rest-only-noise-lock-2026-07-14.md` §3 (and the scope-lock §A.1 for
the order-update spawn). Terraform deletions revert the same way; the
alarms were dead/misleading on dhan-off boots (documented in the lock file),
so reverting is safe but re-pages the noise the operator banned.

## Observability

- KEPT: the 4-item Dhan alert set (lock file §2), every coded `error!` +
  counter in the watchdog (`tv_token_forced_remint_total{outcome}`), the
  `tv-<env>-token-remaining-low` alarm, `tv_token_valid` +
  `tv_token_remaining_seconds` (now published by the re-homed stack poller).
- ADDED: the GAP-02 sweep logs debug/info per cycle (silent self-heal);
  terminal mint failures route to the family-(3) Critical.
- REMOVED (with rationale in the lock file): REST-CANARY-01 filter+alarm,
  `order_update_ws_inactive` (missing-data-blind on dhan-off boots),
  `order_update_reconnect_storm`, the `NoLiveTicksDuringMarketHours` page
  (Dhan-heartbeat-fed only), the two watchdog pages.

## Plan Items

- [x] Rule files first (A1–A4): scope-lock §A.1 + new noise-lock file +
      3 retirement banners + paging-list edit
  - Files: .claude/rules/project/websocket-connection-scope-lock.md,
    .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md,
    .claude/rules/project/dhan-rest-canary-error-codes.md,
    .claude/rules/project/wave-4-error-codes.md,
    .claude/rules/project/wave-2-error-codes.md,
    .claude/rules/project/observability-architecture.md
  - Tests: every_rule_file_code_has_an_enum_variant,
    every_error_code_variant_appears_in_a_rule_file,
    tf_map_and_doc_paging_list_agree_bidirectionally

- [x] Delete the no-tick watchdog + NoLiveTicksDuringMarketHours
  - Files: crates/core/src/pipeline/no_tick_watchdog.rs (deleted),
    crates/core/src/pipeline/mod.rs, crates/core/src/pipeline/tick_processor.rs,
    crates/app/src/main.rs, crates/app/src/observability.rs,
    crates/common/src/constants.rs, crates/core/src/notification/events.rs,
    crates/core/tests/ws_telegram_visibility_guard.rs,
    crates/core/tests/event_formatting_coverage.rs
  - Tests: grep-zero references workspace-wide; core/app crate suites green

- [x] Delete fast_boot_validation (AUTH-GAP-06 module; variant retained)
  - Files: crates/core/src/auth/fast_boot_validation.rs (deleted),
    crates/core/src/auth/mod.rs, crates/app/src/main.rs,
    crates/app/tests/fast_boot_token_validation_wiring_guard.rs (deleted)
  - Tests: core/app crate suites green; grep-zero call sites

- [x] Retire the REST canary (module + both spawn sites + CW filter entry)
  - Files: crates/app/src/rest_canary_boot.rs (deleted),
    crates/app/src/lib.rs, crates/app/src/main.rs,
    crates/app/src/dhan_rest_stack.rs (Phase 5b),
    deploy/aws/terraform/error-code-alarms.tf,
    crates/app/tests/dhan_live_off_phase_a_guard.rs
  - Tests: error_code_paging_filter_drift_guard (tf↔doc↔emit lockstep),
    test_rest_stack_module_is_not_a_stub (needle updated)

- [x] Cut the order-update spawn (Phase 5a) + its two alarms; module dormant
  - Files: crates/app/src/dhan_rest_stack.rs,
    crates/app/tests/ws_event_audit_boot_guard.rs,
    crates/app/tests/dhan_live_off_phase_a_guard.rs,
    deploy/aws/terraform/app-alarms.tf (order_update_ws_inactive block),
    deploy/aws/terraform/order-update-reconnect-storm-alarm.tf (deleted),
    deploy/aws/terraform/market-hours-liveness-alarm.tf (gate list 12 → 11)
  - Tests: NEGATIVE ratchet replacing
    test_rest_stack_spawns_order_update_ws_functional_dormant (production
    region must NOT contain run_order_update_connection( nor the canary
    spawn needle); order_update_connection.rs unit tests untouched

- [x] Silence the mid-session watchdog pages; delete the two variants; page
      family-(3) on terminal re-mint failure
  - Files: crates/core/src/auth/mid_session_watchdog.rs,
    crates/core/src/notification/events.rs,
    crates/core/tests/mid_session_profile_watchdog_guard.rs,
    crates/core/tests/event_formatting_coverage.rs
  - Tests: watchdog unit suite green; terminal-arm AuthenticationFailed emit
    pinned; deleted-variant tests removed

- [x] GAP-06: re-home the token-health gauge poller into dhan_rest_stack
      Phase 3 (module kept; lane/fast-arm spawns + handle plumbing deleted)
  - Files: crates/app/src/main.rs (spawn sites + handle plumbing),
    crates/app/src/dhan_rest_stack.rs,
    crates/core/src/auth/secret_manager.rs (wiring meta-guard rewrite)
  - Tests: stack production-region ratchet asserts the poller spawn;
    api token_headroom_wired_guard stays green

- [x] GAP-02: REST-stack stale-token sweep (900s force_renewal_if_stale(4h))
  - Files: crates/app/src/dhan_rest_stack.rs (the sweep loop),
    crates/common/src/constants.rs (DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS)
  - Tests: production-region ratchet asserts the sweep spawn; pure cadence
    constant unit-pinned

- [x] GAP-04: re-arm the AUTH-GAP-05 re-mint latch every 2nd failing cycle
  - Files: crates/core/src/auth/mid_session_watchdog.rs
    (should_rearm_remint_latch + apply_cycle_outcome wiring)
  - Tests: should_rearm_remint_latch_truth_table,
    gap04_persisting_episode_remints_every_second_failing_cycle,
    gap04_rearm_never_fires_on_non_real_failures,
    gap04_rearm_cadence_constant_is_sane

- [x] Reword AuthenticationFailed + TokenRenewalFailed bodies (broker +
      consequence, plain English)
  - Files: crates/core/src/notification/events.rs
  - Tests: existing severity/escape tests green; body tests updated

- [x] Plan housekeeping: archive merged-PR plans #1524 + #1504
  - Files: .claude/plans/archive/2026-07-14-spot-1m-diagnostics.md,
    .claude/plans/archive/2026-07-13-questdb-partition-s3-archive.md
  - Tests: bash .claude/hooks/plan-gate.sh (V7 cap holds at 5)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan token dies mid-session | Legs page High within ~3-4 min; watchdog re-mints silently (retry ~every 30 min via GAP-04); ONE family-(3) Critical only if terminally unobtainable |
| 2 | Renewal loop circuit-breaker halts | TokenRenewalFailed Critical fires (kept); GAP-02 sweep keeps re-minting on <4h headroom; GAP-06 poller keeps the 4h alarm sighted |
| 3 | Dhan REST surface down (2026-06-10 class) | Spot1m/Chain High pages within minutes (canary deleted — strictly slower coverage removed) |
| 4 | Manual order placed in the Dhan app | No order-update socket exists — nothing fires (deliberate; dry_run=true, events were discarded anyway) |
| 5 | Dual tickvault instance | RESILIENCE-01/03 coded errors + DualInstanceDetected page — UNCHANGED |

## Fix Round (2026-07-14 — 4 hostile reviews, no CRITICAL)

- [x] H1a+H1b: once-per-episode family-(3) page (terminal_paged latch) +
      REMINT_MAX_ATTEMPTS_PER_EPISODE=3 cap on the GAP-04 re-arm (cap page
      names the N re-logins + blocked spot-1m/chain pulls — the
      dead-dataPlan class)
  - Files: crates/core/src/auth/mid_session_watchdog.rs
  - Tests: h1_cap_stops_rearm_after_max_attempts_per_episode,
    h1_terminal_page_latch_fires_once_per_episode_and_resets_on_ok,
    h1_cap_page_fires_once_when_profile_stays_invalid_after_cap,
    h1_transient_then_recover_pages_nothing
- [x] H2: silence ratchet de-vacuoused — call-form needle
      `.notify(NotificationEvent::AuthenticationFailed` pinned at count 2
      in the production region; module doc reworded so it can't satisfy it
  - Files: crates/core/tests/mid_session_profile_watchdog_guard.rs,
    crates/core/src/auth/mid_session_watchdog.rs
  - Tests: mid_session_watchdog_is_silent_except_terminal_auth_failure
- [x] H3: shared ~125s mint-cooldown gate inside
      TokenManager::renew_with_fallback (typed `mint-cooldown` refusal;
      initialize boot loop ungated; watchdog treats the skip as
      non-terminal — closes AG5-R2-1)
  - Files: crates/core/src/auth/token_manager.rs,
    crates/core/src/auth/mid_session_watchdog.rs
  - Tests: h3_mint_cooldown_allows_truth_table,
    h3_renew_with_fallback_refuses_mint_inside_cooldown,
    h3_renew_with_fallback_allows_mint_after_cooldown,
    h3_initialize_mint_path_is_ungated,
    h3_mint_cooldown_refusal_is_prefix_anchored
- [x] M1: RestSurfaceDegraded rising edge wording class-accurate (no
      re-login attempted); M2: terminal Telegram body redacted+truncated
      via capture_rest_error_body
  - Files: crates/core/src/auth/mid_session_watchdog.rs
  - Tests: existing classifier tests green; H2 ratchet pins the emits
- [x] M3: boot-time zero-pager-legs warn in dhan_rest_stack
  - Files: crates/app/src/dhan_rest_stack.rs
  - Tests: test_rest_stack_zero_pager_legs_truth_table_and_warn_wired
- [x] M4: ghost metric tv_order_update_ws_active removed (EMF allowlists,
      dashboard.tf panel, metrics catalog entry; 27-name pin -> 26)
  - Files: deploy/aws/cloudwatch-agent.json,
    deploy/aws/terraform/user-data.sh.tftpl,
    deploy/aws/terraform/dashboard.tf,
    crates/common/tests/metrics_catalog.rs,
    crates/common/tests/cloudwatch_app_alarms_wiring.rs
  - Tests: test_emf_metric_selectors_name_count_is_twenty_six
- [x] M5: scope-lock §D stale REJECT row annotated in place (dated
      2026-07-14; module deletion stays REJECT)
  - Files: .claude/rules/project/websocket-connection-scope-lock.md
  - Tests: docs-only
- [x] M6 (ACCEPTED residual): pre-#1522 dhan-ON lane boot leaves
      tv_token_valid unpublished — documented in the lock file + honest
      envelope; prod is dhan-off and #1522 merges first
  - Files: .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md
  - Tests: docs-only
- [x] L: lock/runbook wording truth fixes (once-per-episode + cap; sweep
      warn no longer claims the halted renewal loop retries; RESILIENCE-03
      comment aligned with acquire_token's in-flight page; RefuseLockLost
      ~30-min re-log cadence noted); triage YAML rest-canary entries
      annotated retired; GAP-02 sweep + GAP-06 gauge poller SUPERVISED
      (house respawn pattern, unwind-build self-heal only)
  - Files: .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md,
    .claude/rules/project/wave-4-error-codes.md,
    .claude/triage/error-rules.yaml, crates/app/src/dhan_rest_stack.rs,
    crates/core/src/auth/mid_session_watchdog.rs
  - Tests: test_supervised_stack_task_respawns_on_clean_exit,
    test_sweep_and_gauge_poller_are_supervised

### Fix-round honest envelope additions
- M6: `tv_token_valid` / live `tv_token_remaining_seconds` are NOT
  published on a hypothetical `dhan_enabled=true` LANE boot before #1522
  merges (lane poller spawn sites deleted; the stack does not run on that
  boot shape). ACCEPTED: prod is dhan-off (config + Phase-A 409) and
  #1522 merges first; this PR rebases after it.
- Supervision honesty: the sweep/poller respawn arms self-heal in unwind
  builds only — release builds run panic=abort, so a production panic
  aborts the process (restart = recovery), the TICK-FLUSH-01 precedent.
