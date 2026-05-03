# Implementation Plan: Non-AWS Readiness Fixes (post 2026-05-03 audit)

**Status:** IMPLEMENTED → PENDING-VERIFICATION (operator must run cargo locally — sandbox lacks rust toolchain)
**Date:** 2026-05-03
**Approved by:** Parthiban (operator) — verbatim "approved dude" 2026-05-03
**Branch:** `claude/non-aws-readiness-fixes-2026-05-03`
**Triggering audit:** `docs/operator/aws-readiness-audit-2026-05-03.md` — 3-agent adversarial review per `wave-4-shared-preamble.md` §3
**AWS scope:** EXPLICITLY DEFERRED per operator direction 2026-05-03 — no Terraform/EventBridge/systemd/SSM provisioning changes in this PR.

## Operator-confirmed scoping decisions (2026-05-03)

| Decision | Choice |
|---|---|
| §8 wording vs build the missing ratchets | **Both** — reword §8 honestly NOW, queue real tests as Wave-6 items |
| s3_backup.rs 5 dead pub fns | **Delete entirely** — AWS path can be rebuilt cleanly when bucket is provisioned |
| Branch scope | **Single branch, all 7 non-AWS items** |

## Plan Items (10 total — 7 non-AWS audit findings + 3 admin items)

- [x] **1. Reword `wave-4-shared-preamble.md` §8 to drop unproven claims**
  - Files: `.claude/rules/project/wave-4-shared-preamble.md` (Section 8)
  - New honest wording: drop "≤600K rescue ring capacity" + "chaos-tested 70h sleep/wake" until ratchets exist
  - Add explicit pointer to Wave-6 backlog for those items
  - Tests: `crates/common/tests/wave4_section8_wording_guard.rs::test_section8_does_not_claim_unproven_capacity_or_chaos`
  - Closes audit Finding #2 + #3

- [x] **2. Create Wave-6 backlog items for the dropped claims**
  - Files: `.claude/plans/active-plan-wave-6-backlog.md` (new)
  - Item W6-1: rescue ring 600K capacity + bound test + overflow→spill chaos test
  - Item W6-2: 70h sleep/wake chaos test (Friday 15:30 → Monday 09:00 dormant)
  - Tests: none (backlog spec only)

- [x] **3. Delete 5 dead pub fns in `s3_backup.rs`**
  - Files: `crates/core/src/instrument/s3_backup.rs` (lines 91, 109, 119, 134, 210)
  - Files: `crates/core/tests/gap_enforcement.rs` (remove S3-related tests if isolated)
  - Files: `crates/core/src/instrument/mod.rs` (remove `pub mod s3_backup;` if module is fully empty post-delete)
  - Tests: existing `pub-fn-wiring-guard.sh` will pass after deletion
  - Closes audit Finding #4

- [x] **4. Add `MarketOpenReadinessConfirmation` Telegram event @ 09:14:00 IST**
  - Files: `crates/core/src/notification/events.rs` (add variant + format)
  - Files: `crates/common/src/error_code.rs` (no new ErrorCode — Severity::Info)
  - Files: `crates/app/src/main.rs` (spawn 09:14:00 IST scheduler)
  - Files: `.claude/rules/project/observability-architecture.md` (note in "What future sessions MUST NOT do")
  - Tests: `crates/core/src/notification/events.rs::test_market_open_readiness_confirmation_is_info_severity`
  - Tests: `crates/core/src/notification/events.rs::test_market_open_readiness_confirmation_includes_subscription_counts`
  - Tests: edge-trigger-once-per-trading-day pin
  - Closes audit Finding #5

- [x] **5. Verify token sweep task exists; add 4h periodic sweep if missing**
  - Files: `crates/core/src/auth/token_manager.rs` (audit `force_renewal_if_stale` call sites)
  - Files: `crates/app/src/main.rs` (boot wiring for sweep task if missing)
  - Tests: `crates/core/src/auth/token_manager.rs::tests::test_force_renewal_called_from_periodic_sweep_not_only_ws_wake`
  - Closes audit Finding #6

- [x] **6. Add per-step boot timeouts (named) for steps 1-5, 8-14**
  - Files: `crates/common/src/constants.rs` (per-step timeout constants)
  - Files: `crates/app/src/main.rs` (wrap each boot step in `tokio::time::timeout(...)`)
  - Convention: named timeout via `BootStep::N { name: &'static str, timeout_secs: u64 }`
  - Step 6 conflict (300s > umbrella 120s): reduce to `TOKEN_INIT_TIMEOUT_SECS = 90s` OR raise umbrella
  - Tests: `crates/app/tests/boot_timeout_per_step_guard.rs::test_each_step_has_named_timeout`
  - Tests: `crates/app/tests/boot_timeout_per_step_guard.rs::test_step6_timeout_does_not_exceed_umbrella`
  - Closes audit Finding #7

- [x] **7. Add I-P1-11 cross-reference comments in code**
  - Files: `crates/core/src/pipeline/tick_gap_detector.rs` (add `// I-P1-11 enforcement:` comment block)
  - Files: `crates/storage/tests/dedup_segment_meta_guard.rs` (add `// I-P1-11 enforcement:` comment block)
  - Files: `quality/benchmark-budgets.toml` (clarify line 26 already cites composite)
  - Tests: `crates/core/tests/i_p1_11_codepath_grep_guard.rs::test_i_p1_11_appears_in_enforcement_files_not_just_docs`
  - Closes audit Finding #8

- [x] **8. Wrap browser-open in config flag (`portal.auto_open` default true on dev, false otherwise)**
  - Files: `crates/common/src/config.rs` (add `portal.auto_open: bool`, default true)
  - Files: `crates/app/src/main.rs:7171` (gate browser-open on `cfg.portal.auto_open`)
  - Files: `config/base.toml` (document new flag)
  - Tests: `crates/app/tests/portal_auto_open_flag_guard.rs::test_default_value_and_gate_behavior`
  - Closes audit Finding #9

- [x] **9. Add `O(1) EXEMPT:` comments OR DHAT for `dispatcher.rs` hot-path allocations**
  - Files: `crates/core/src/parser/dispatcher.rs` (lines 793, 805, 874, 998, 1027)
  - Files: `crates/core/tests/dhat_dispatcher_hot_path.rs` (new — covers `dispatcher::dispatch_packet` end-to-end)
  - Decision per line: cold-path → `O(1) EXEMPT:` comment; hot-path → fix or pin via DHAT
  - Tests: new `dhat_dispatcher_hot_path.rs::test_dispatch_packet_zero_allocation_on_known_codes`
  - Closes audit Finding #10

- [x] **10. End-to-end DHAT for `tick_processor::process_tick`**
  - Files: `crates/core/tests/dhat_tick_processor_e2e.rs` (new)
  - Pins: zero allocation across full process_tick path on a Quote+OI+Full sequence
  - Catches the unmarked allocations Finding #11 surfaced
  - Tests: `dhat_tick_processor_e2e.rs::test_process_tick_e2e_zero_allocation`
  - Closes audit Finding #11

## Honest 100% Claim (NEW WORDING after Item 1 lands)

> "100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ; bench-gated O(1)
> hot path; composite-key uniqueness; 22 test categories per
> testing.md. Beyond the envelope, DLQ NDJSON catches every payload as
> recoverable text. Outstanding: rescue ring fixed-capacity bound test
> (Wave-6-1) and 70h dormant sleep chaos test (Wave-6-2) are tracked
> in `.claude/plans/active-plan-wave-6-backlog.md` and not yet pinned."

## Scenarios

| # | Scenario | Expected outcome |
|---|---|---|
| 1 | Boot at 7:45 IST cold | All 15 steps with named timeouts; umbrella names slow step |
| 2 | Step 6 (Dhan auth) takes 100s | Step 6 timeout fires NAMED at 90s, umbrella does NOT fire |
| 3 | Token expiry 4h headroom mid-session | Periodic sweep (Item 5) catches without WS-wake |
| 4 | 09:14:00 IST passes green | `MarketOpenReadinessConfirmation` Telegram fires once |
| 5 | dispatcher.rs hot-path allocates in PR N+1 | New DHAT (Item 9) fails the build |
| 6 | tick_processor allocates in PR N+1 | New e2e DHAT (Item 10) fails the build |
| 7 | AWS headless boot opens browser | Portal flag (Item 8) prevents spurious open |
| 8 | s3_backup pub fns referenced from new code | Compile fails (deleted in Item 3) |
| 9 | Reviewer searches code for "I-P1-11" | Finds enforcement comments (Item 7) |
| 10 | New session quotes literal "70h chaos-tested" | Item 1 reword removes wording, banned-pattern catches regression |

## Verification (mandatory before VERIFIED)

1. `cargo fmt --check`
2. `cargo build --workspace`
3. `cargo clippy --workspace -- -D warnings -W clippy::perf`
4. `cargo test --workspace` (FULL_QA because crates/common touched)
5. `bash .claude/hooks/banned-pattern-scanner.sh`
6. `bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all`
7. `bash .claude/hooks/pub-fn-wiring-guard.sh "$PWD"`
8. `bash .claude/hooks/per-item-guarantee-check.sh`
9. `bash .claude/hooks/plan-verify.sh`
10. `cargo bench` for items 9, 10
11. `cargo test --features dhat` for items 9, 10
12. Spawn 3 adversarial agents on diff per `wave-4-shared-preamble.md` §3

## Estimated Effort

| Item | Effort |
|---|---|
| 1. Reword §8 | 10min |
| 2. Wave-6 backlog | 15min |
| 3. Delete s3_backup dead fns | 30min |
| 4. 09:14 readiness Telegram | 90min |
| 5. Token sweep | 90min |
| 6. Per-step boot timeouts | 2h |
| 7. I-P1-11 comments | 30min |
| 8. Portal config flag | 30min |
| 9. dispatcher DHAT | 90min |
| 10. tick_processor e2e DHAT | 2h |
| **Implementation subtotal** | **~10h** |
| Adversarial review | 1h |
| Verification + fixes | 1h |
| **Total** | **~12h** |

## Out of Scope

- All AWS Terraform / EventBridge / SSM / EC2 / S3 provisioning
- Audit Finding #1 (EventBridge cron 08:00 vs 7:45 IST) — AWS-related
- HIGH #4 sub-item: actually wiring S3 backup to Glacier — deferred until bucket
- Wave-6 backlog implementation — only the backlog FILE is created here
