# Implementation Plan: Depth-200 SELF Token Authentication Mode

**Status:** APPROVED
**Date:** 2026-04-28
**Approved by:** Parthiban ("yes fix everythign dudde okay?")
**Branch:** `claude/debug-grafana-issues-LxiLH`

## Background

Dhan's `wss://full-depth-api.dhan.co/` rejects access tokens with
`tokenConsumerType: "APP"` (the only type our TOTP-driven `/app/generateAccessToken`
flow can mint), but accepts `tokenConsumerType: "SELF"` (manually generated from
web.dhan.co). Verified 2026-04-28 with the official Dhan Python SDK 2.2.0rc1 on
SecurityId 74147 — same script, same SID, only token source changed.

Email + GitHub link sent to apihelp@dhan.co requesting they confirm the policy
and document a programmatic SELF mint endpoint. While we wait for Dhan, we ship
a per-endpoint SELF-token authentication path **gated behind a config flag**
(default OFF) so the existing TOTP/APP flow is untouched until Dhan replies.

## Plan Items

- [x] **1. Add config flag for depth-200 auth mode**
  - Files: `crates/common/src/config.rs`, `config/base.toml`
  - New struct `Depth200AuthConfig { mode: String, cache_path: String, renewal_interval_secs: u64 }`
  - Mode values: `"totp_app"` (default) | `"manual_self_with_renewal"`
  - Tests: `test_depth_200_auth_config_default_is_totp_app`, `test_depth_200_auth_config_parses_manual_self_mode`

- [x] **2. Add cache file path constant + gitignore**
  - Files: `crates/common/src/constants.rs`, `.gitignore`
  - Constant: `DEPTH_200_SELF_TOKEN_CACHE_PATH = "data/cache/depth-200-self-token-cache"`
  - Tests: `test_depth_200_self_cache_path_is_separate_from_main_token_cache`

- [x] **3. New module `depth_200_self_token_manager.rs`**
  - Files: `crates/core/src/auth/depth_200_self_token_manager.rs`, `crates/core/src/auth/mod.rs`
  - `Depth200SelfTokenManager` owns its own `TokenHandle` (Arc<ArcSwap<Option<TokenState>>>)
  - `boot_from_cache()` — reads SELF cache file, validates `tokenConsumerType == "SELF"` claim, parses expiry
  - `spawn_renewal_task()` — 23h cadence, calls `GET /v2/RenewToken`, swaps the handle atomically
  - `handle()` returns `&TokenHandle` for the depth-200 connection to consume
  - Telegram CRITICAL on renewal failure with operator runbook reference
  - Tests: `test_boot_from_cache_rejects_app_token`, `test_boot_from_cache_accepts_self_token`,
    `test_renewal_preserves_self_consumer_type`, `test_missing_cache_file_returns_error`,
    `test_corrupt_cache_file_returns_error`, `test_handle_returns_arc_swap_token`

- [x] **4. New error code WAVE3-DEPTH200-01 + runbook entry**
  - Files: `crates/common/src/error_code.rs`, `.claude/rules/project/wave-3-error-codes.md`
  - New ErrorCode variant `Depth200SelfTokenRenewalFailed`
  - Runbook: paste fresh SELF token at `data/cache/depth-200-self-token-cache`, restart depth-200 task
  - Severity: Critical
  - Tests: cross-ref test passes, tag-guard test passes

- [x] **5. Wire main.rs to select TokenHandle based on config**
  - Files: `crates/app/src/main.rs`
  - At line ~3459 (depth-200 setup), branch on `config.depth_200_auth.mode`:
    - `"totp_app"` → use existing `token_handle.clone()` (no behavior change)
    - `"manual_self_with_renewal"` → instantiate `Depth200SelfTokenManager`, spawn its renewal task, pass `manager.handle().clone()` to `run_two_hundred_depth_connection`
  - Tests: `test_depth_200_default_mode_uses_shared_token_handle`,
    `test_depth_200_manual_mode_uses_separate_token_handle`

- [x] **6. Notification event for renewal lifecycle**
  - Files: `crates/core/src/notification/events.rs`
  - New variants: `Depth200SelfTokenLoaded` (Info), `Depth200SelfTokenRenewed` (Info), `Depth200SelfTokenRenewalFailed` (Critical)
  - Tests: `test_depth_200_self_token_events_have_correct_severities`

- [x] **7. Integration test (offline, no network)**
  - Files: `crates/core/tests/depth_200_self_token_integration.rs`
  - Spin up `Depth200SelfTokenManager` with a temp cache file containing a valid SELF JWT
  - Verify `handle()` returns the parsed token
  - Mock-renew (skip actual HTTP) and verify swap
  - Tests: `test_full_lifecycle_load_to_handle`, `test_renewal_swap_is_atomic`

## Mechanical Verifications

| Gate | Command | Expected |
|---|---|---|
| Config parses | `cargo test -p tickvault-common` | All tests pass |
| Auth module compiles | `cargo build -p tickvault-core` | Clean |
| New tests pass | `cargo test -p tickvault-core depth_200_self_token` | All pass |
| Tag-guard passes | `cargo test -p tickvault-common error_code_tag_guard` | Pass |
| Cross-ref passes | `cargo test -p tickvault-common error_code_rule_file_crossref` | Pass |
| Test count ratchet | Pre-push gate 4 | Count up |
| Banned patterns | Pre-push gate 2 | None |
| Default mode = OFF | New unit test | Confirmed |

## Out of Scope (Explicit)

- **Operator CLI to populate the cache.** For now, `echo "$SELF_JWT" > data/cache/depth-200-self-token-cache`. CLI can be added later.
- **AWS SSM integration for the SELF token.** When tickvault migrates to AWS, the existing `secret_manager.rs` pattern extends to a new SSM path. Not needed in dev.
- **Switching production to SELF mode by default.** Stays OFF. Operator flips manually after Dhan confirms the policy.
- **Refactoring existing `TokenManager`** to be parameterized — too risky. We build a smaller dedicated module.

## Verification

Run `bash .claude/hooks/plan-verify.sh` after all items complete.
