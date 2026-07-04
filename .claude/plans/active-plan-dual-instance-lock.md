# Implementation Plan: Dual-Instance Lock Hardening (lock-before-mint, always-on)

**Status:** VERIFIED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator) — "go", 2026-07-04, this session (after the
local-vs-AWS coexistence audit proved a default local boot TOTP-mints a fresh
Dhan token from the same `/tickvault/prod` SSM credentials and invalidates the
AWS box's token; the RESILIENCE-01 lock was (a) gated `is_live()` while both
prod and local run `mode = "sandbox"` → never active, and (b) ordered AFTER the
Step 6 mint → the damage precedes the check).

> Guarantee matrices: see `per-wave-guarantee-matrix.md` — all 15 + 7 rows
> apply to every item in this plan (boot-path only; no hot-path change, no new
> tick-drop path, no DEDUP-key change; the new ErrorCode carries a rule-file
> runbook per the cross-ref ratchet).

## Design

Crates touched: `crates/app` (`crates/app/src/main.rs`), `crates/core`
(`crates/core/src/instance_lock.rs`, `crates/core/src/auth/token_manager.rs`,
`crates/core/src/auth/secret_manager.rs` test module), `crates/common`
(`crates/common/src/error_code.rs`).

1. **Un-gate + reorder the RESILIENCE-01 SSM dual-instance lock** inside
   `start_dhan_lane` (`crates/app/src/main.rs`): the Step 6a-prime block moves
   from AFTER the Step 6 Dhan token mint to BETWEEN Step 5.5 (IP verify) and
   Step 6 (auth), and the `if trading_mode.is_live()` gate is REMOVED — the
   lock is acquired on EVERY Dhan-enabled boot (the lane only runs when
   `feeds.dhan_enabled == true`; sandbox/dry-run included, because a sandbox
   boot still burns the shared one-active-token-at-a-time Dhan JWT). The lock
   acquisition needs ONLY an AWS SSM client (`create_ssm_client_public`) +
   `resolve_environment` — no Dhan token — so the reorder is safe.
2. **SSM-transport-error resilience:** the acquire attempt is wrapped in the
   SAME bounded 3-attempt / exponential (2s, 4s) retry pattern the Step 6 SSM
   credential fetch already uses, then HALTs fail-closed (RESILIENCE-01 —
   "cannot prove there's no peer"). Documented semantics: today's live-mode
   code HALTed on the FIRST SSM error; prod (sandbox) never ran it. After this
   change a sustained SSM outage halts boot at the lock — which is behaviour-
   equivalent for prod, because Step 6 immediately after would fail on the
   same SSM outage (its credential fetch has the same 3-attempt budget).
3. **Mint tripwire (defense in depth):** `TokenManager` gains an
   `instance_lock_held: Option<Arc<AtomicBool>>` field (passed into
   `initialize` / `initialize_deferred`; `None` = no lock expected, e.g.
   tests + the fast-boot arm). Immediately before `generateAccessToken` in
   `acquire_token` (the mint path ONLY — `try_renew_token` is untouched), a
   pure helper `mint_refused_by_instance_lock` refuses the mint fail-closed
   when the flag exists and reads `false` (lock lost to a peer). Refusal =
   typed `error!` with new `ErrorCode::Resilience03MintRefusedLockNotHeld`
   (`RESILIENCE-03`, Severity::Critical) + Telegram
   (`AuthenticationFailed`) + `ApplicationError::AuthenticationFailed`.
   `is_permanent_auth_error` recognises `RESILIENCE-03` so the boot retry
   loop fails fast instead of spinning.
4. **Heartbeat wires the flag:** `spawn_instance_lock_heartbeat` gains the
   `held_flag: Arc<AtomicBool>` parameter — sets it `false` when the lock is
   lost to a peer (`renew → Ok(false)`) and on shutdown release, so a
   mid-session renewal-fallback mint is refused after lock loss.
5. **Escape hatch:** CLI flag `--force-instance-takeover` (named const +
   pure matcher fn in main.rs). On `AlreadyHeld` with the flag present, the
   boot calls new `instance_lock::force_takeover_instance_lock` (SSM
   PutParameter overwrite=true) with a loud RESILIENCE-01-coded `error!`
   audit line + proceeds holding the lock. TTL expiry (90s) already
   auto-clears a crashed holder via `LockValue::is_stale` in
   `try_acquire_instance_lock` — pinned by new/existing tests.
6. **Documented residual gap (honest envelope):** the FAST boot arm (crash
   recovery, cached token, market hours) does not mint at boot and today
   acquires no lock in any mode; it passes `None` for the tripwire flag and
   keeps today's behaviour byte-identical. Closing that arm (lock re-acquire
   after a mid-market crash restart) needs an operator decision on mid-market
   halt semantics — flagged as a follow-up, NOT silently changed here.

## Edge Cases

- **Sandbox + local Mac boot (the incident class):** slow boot with
  `dhan_enabled=true` now blocks at the lock BEFORE any mint when the AWS box
  holds `/tickvault/prod/instance-lock` — the AWS token is never invalidated.
- **AWS Monday 08:30 IST cold boot:** no peer → atomic claim succeeds on the
  first PutParameter (one SSM round-trip, <1s added before auth); heartbeat
  unchanged (30s / 90s TTL). Boot timeline otherwise identical.
- **Crashed holder:** stale heartbeat (>90s) → automatic takeover (existing
  `is_stale` path, unchanged); a same-host systemd restart after a crash
  waits at most ~90s only if the previous process could not release.
- **Wedged/stale lock that never expires (corrupt JSON):** operator boots with
  `--force-instance-takeover` → loud audited overwrite.
- **SSM blip at boot:** 3 attempts with backoff, then HALT (fail-closed,
  same class as the Step 6 credential-fetch failure that would follow).
- **Lock lost mid-session:** heartbeat flips the flag false + exits (existing
  RESILIENCE-01 error); the next renewal-fallback mint is REFUSED with
  RESILIENCE-03 instead of silently killing the peer's token. `RenewToken`
  (non-mint) is deliberately NOT gated — renewing our own still-active token
  harms no peer.
- **Tests / fast boot:** `instance_lock_held = None` → tripwire inert,
  behaviour unchanged.
- **Two simultaneous boots racing:** SSM `PutParameter(overwrite=false)` is
  the atomic claim — exactly one wins; the loser reads the holder and halts.
  The stale-takeover path (Get→Put overwrite=true) is NOT atomic: two racers
  both seeing a stale lock could both overwrite; the 30s heartbeat's
  ownership check (`renew → Ok(false)`) detects the loser within one beat and
  halts its heartbeat + flips its mint flag false. Honest envelope: a ≤30s
  dual-hold window exists in the stale-takeover race, unchanged from today's
  design and documented in the rule file.

## Failure Modes

- **AlreadyHeld (fresh peer):** HALT with RESILIENCE-01 `error!` + Telegram
  `DualInstanceDetected` (unchanged semantics, now BEFORE the mint).
- **SSM transport error after 3 attempts:** HALT fail-closed with
  RESILIENCE-01 (same arm as today's live-mode code).
- **Mint attempted without lock (flag false):** RESILIENCE-03 `error!` +
  Telegram + typed Err; classified permanent → no infinite retry loop.
- **Heartbeat loses lock:** existing RESILIENCE-01 error + task exit, plus
  the flag flip so future mints refuse.
- **Force takeover misuse:** the overwrite is logged at `error!` (Telegram
  pages) with the prior holder identity — never silent.

## Test Plan

- `crates/core/src/auth/token_manager.rs` unit tests:
  `test_mint_refused_when_lock_flag_false`,
  `test_mint_allowed_when_lock_flag_true_or_absent`,
  `test_acquire_token_refuses_before_any_network_when_lock_lost` (tokio),
  `test_resilience03_is_permanent_auth_error`.
- `crates/core/src/instance_lock.rs` unit tests:
  `test_force_takeover_instance_lock_smoke`,
  `test_crashed_holder_stale_after_ttl_is_takeover_eligible`,
  `test_spawn_instance_lock_heartbeat_smoke` (updated signature).
- `crates/core/src/auth/secret_manager.rs` ratchets (source-scan of main.rs):
  `test_instance_lock_boot_gate_and_heartbeat_are_wired_together` (existing,
  still green), NEW `test_instance_lock_acquired_before_token_mint` (source
  order: `try_acquire_instance_lock` before `TokenManager::initialize(` in
  `start_dhan_lane`) and NEW `test_instance_lock_not_gated_on_live_mode`
  (pins the always-on marker + absence of the `is_live()` gate on the lock).
- `crates/app/src/main.rs` unit tests: `test_force_takeover_flag_matcher`.
- Commands: `cargo test -p tickvault-common -p tickvault-core -p
  tickvault-app`, `cargo fmt --check`, `cargo clippy --workspace --no-deps`.

## Rollback

Single-commit revert (`git revert` of the squash-merge) restores the prior
boot order + `is_live()` gate; the new ErrorCode variant, rule file, and CLI
flag are additive and revert cleanly with it. No schema change, no config
key change, no data migration — rollback risk is nil beyond losing the
protection itself.

## Observability

- Existing: RESILIENCE-01 `error!` + `DualInstanceDetected` Telegram on
  AlreadyHeld / SSM failure (unchanged, now pre-mint), heartbeat logs.
- New: `RESILIENCE-03` typed `error!` (Telegram via the 5-sink chain) +
  `AuthenticationFailed` notification on a refused mint; force-takeover
  audit `error!` line carrying the displaced holder id; boot log line
  stating the lock is acquired pre-auth for every Dhan-enabled mode.
- Runbook: `.claude/rules/project/dual-instance-lock-2026-07-04.md`
  (cross-ref test target for the new variant).

## Plan Items

- [x] Item 1 — Reorder + un-gate the lock in `start_dhan_lane`; bounded SSM
  retry; force-takeover arm; held-flag creation
  - Files: crates/app/src/main.rs
  - Tests: test_force_takeover_flag_matcher, test_instance_lock_acquired_before_token_mint, test_instance_lock_not_gated_on_live_mode
- [x] Item 2 — Mint tripwire in TokenManager (flag param, pure helper,
  permanent-error classification)
  - Files: crates/core/src/auth/token_manager.rs
  - Tests: test_mint_refused_when_lock_flag_false, test_mint_allowed_when_lock_flag_true_or_absent, test_acquire_token_refuses_before_any_network_when_lock_lost, test_resilience03_is_permanent_auth_error
- [x] Item 3 — Heartbeat held-flag wiring + `force_takeover_instance_lock`
  - Files: crates/core/src/instance_lock.rs
  - Tests: test_force_takeover_instance_lock_smoke, test_crashed_holder_stale_after_ttl_is_takeover_eligible
- [x] Item 4 — `ErrorCode::Resilience03MintRefusedLockNotHeld` (RESILIENCE-03)
  - Files: crates/common/src/error_code.rs
  - Tests: test_all_variants_have_unique_code_str (existing ratchet covers the new variant)
- [x] Item 5 — Rule file (operator quote, contract, REJECT list, runbook)
  - Files: .claude/rules/project/dual-instance-lock-2026-07-04.md
  - Tests: every_error_code_variant_appears_in_a_rule_file, every_runbook_path_exists_on_disk
- [x] Item 6 — Ratchet updates in secret_manager.rs test module
  - Files: crates/core/src/auth/secret_manager.rs
  - Tests: test_instance_lock_acquired_before_token_mint, test_instance_lock_not_gated_on_live_mode

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Local Mac default boot while AWS in-session | Local halts at lock (RESILIENCE-01) BEFORE minting; AWS token untouched |
| 2 | AWS 08:30 IST cold boot, no peer | Lock acquired pre-auth; boot proceeds identically |
| 3 | SSM blip at boot | 3 retries (2s/4s) then fail-closed HALT |
| 4 | Peer crashed >90s ago | Stale takeover, boot proceeds (existing path) |
| 5 | Wedged corrupt lock | `--force-instance-takeover` overwrites with audited error! line |
| 6 | Lock lost mid-session, renewal falls back to mint | Mint REFUSED with RESILIENCE-03, operator paged |
| 7 | Fast-boot crash recovery (cached token) | Behaviour unchanged (documented residual gap; flag None) |
