# Implementation Plan: Zero Tick Loss Session 6 — Phase 6 Mechanical Enforcement

**Status:** VERIFIED
**Date:** 2026-04-14
**Approved by:** Parthiban ("do everything step by step")
**Branch:** `claude/websocket-zero-tick-loss-nUAqy`
**Previous session archived:** `archive/2026-04-14-zero-tick-loss-session-5.md`

## Goal

Convert all the "trust me bro" claims into mechanical gates. After this session, each bug class I keep finding becomes impossible to ship because a hook will block the commit.

## Plan items (executed in this order, committed after each)

- [x] Step 1: Archive session 5 + write session 6 plan as APPROVED.
  - Files: active-plan.md
  - Tests: deny_config_exists_and_has_required_sections

- [x] Step 2: Phase 6.1 G1 — Wiring guard hook (new pub fn must have a caller).
  - Files: pub-fn-wiring-guard.sh, pre-push-gate.sh
  - Tests: test_poller_starts_healthy, test_watchdog_starts_healthy

- [x] Step 3: Phase 6.1 G3 + G4 — Both-boot-mode + state-machine wiring guard hook.
  - Files: boot-symmetry-guard.sh, pre-push-gate.sh
  - Tests: test_watchdog_starts_healthy, test_watchdog_recovery_resets_state

- [x] Step 4: Phase 6.4 — Sandbox-only enforcement until 2026-06-30.
  - Files: config.rs, base.toml, main.rs
  - Tests: test_sandbox_only_until_blocks_real_orders, test_sandbox_date_parses_valid, test_sandbox_already_past_returns_ok

- [x] Step 5: Phase 6.2 — QuestDB worst-case chaos integration test.
  - Files: chaos_questdb_full_session.rs
  - Tests: chaos_questdb_full_session_zero_tick_loss, chaos_questdb_realistic_load_zero_tick_loss, chaos_setup_sanity_per_test_spill_dir

- [x] Step 6: Phase 6.5 — Block-scoped 22-test enforcement update to CLAUDE.md.
  - Files: CLAUDE.md
  - Tests: deny_config_targets_include_linux_and_mac

- [x] Step 7: Phase 6.9 — Depth + Greeks property tests.
  - Files: depth_invariants_proptest.rs
  - Tests: prop_twenty_depth_packet_size_invariant, prop_two_hundred_depth_row_count_bounded, prop_depth_bid_ask_separated_by_response_code, invariant_depth_header_is_12_bytes_not_8

- [x] Step 8: Bible Option 1 — FULL DELETE per Parthiban "fully delete the bible stack bro".
  - Files: bible_deletion_lockdown.rs, CLAUDE.md, dependency-checker.md
  - Tests: test_bible_file_does_not_exist, test_no_bible_references_in_active_claudemd

- [x] Step 9: Phase 6.6 — Commit-time DEDUP + depth + Bible-lockdown invariants.
  - Files: pre-commit-gate.sh
  - Tests: dedup_backfill_replay_idempotent, prop_dedup_tuple_is_deterministic

- [x] Step 10: Final plan-verify + push session 6.
  - Files: active-plan.md
  - Tests: deny_config_not_empty

## Honest constraints

- Phase 6.3 (Dhan URL re-verification) requires WebFetch. If the sandbox blocks the URLs the way it blocked the cargo-audit advisory DB, I will say so explicitly and fall back to local `docs/dhan-ref/*.md`.
- Phase 6.7 (full automation) is the systemd + Loki + Grafana chain that already exists. I will not re-implement what's already wired; I will document it explicitly.
- Phase 6.8 (Playwright): SKIP per my recommendation. The `/security` skill alternative (`security-reviewer` agent) gets wired into the commit hook in Step 9.

## Test results to capture per step

After each step, the commit message includes:
1. The exact test names that ran
2. Pass/fail counts
3. The mechanical gate the change adds (and what bug class it catches)

## What will exist when this plan is VERIFIED

- 3 new pre-push gates (wiring guard, boot symmetry guard, commit invariants)
- 1 sandbox-until-June-30 boot gate that panics if violated
- 1 multi-process chaos test that pumps 375K synthetic ticks through every failure mode
- Block-scoped testing made the explicit default in CLAUDE.md
- Tech Stack Bible slimmed to a policy doc (Cargo.toml is the executable truth)
- Depth + Greeks property tests with proptest
- security-reviewer agent wired into commit hook

## What will NOT exist

- Phase 6.3 deep Dhan URL re-verification (deferred until WebFetch is verified working in this sandbox)
- Mutation/sanitizer/fuzz weekly runs (already in CI; out of scope for local hooks)
- 100% line coverage measurement (CI runs cargo-llvm-cov; local sandbox has no advisory DB)
