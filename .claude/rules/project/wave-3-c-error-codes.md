# Wave 3-C Error Codes

> **Authority:** This file is the runbook target for the new ErrorCode
> variants added in the Wave 3-C hardening implementation (Item 12 —
> market-open self-test). Cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `crates/common/src/error_code.rs::ErrorCode` is
> mentioned in at least one rule file under `.claude/rules/`.

## SELFTEST-01 — market-open self-test passed

**Trigger:** the once-per-trading-day self-test scheduler in
`crates/core/src/instrument/market_open_self_test.rs::evaluate_self_test`
returned `MarketOpenSelfTestOutcome::Passed { checks_passed: 7 }`.
Severity::Info.

**Why it exists:** the operator's "is anything broken right now?" is
otherwise answered only by the absence of `[CRITICAL]`/`[HIGH]` pages,
which is a negative signal. This code provides a positive once-a-day
ping that the seven sub-checks (main feed, depth-20, depth-200,
order-update WS, pipeline, recent tick, QuestDB, token headroom) are
all green.

**Triage:**
1. None — this is a positive confirmation.
2. If the operator never sees this code after 09:16:30 IST, check the
   scheduler boot wiring and the `c9_flags.market_open_self_test`
   config toggle.

**Auto-triage safe:** YES (Info severity is always safe).

**Source:** `crates/core/src/instrument/market_open_self_test.rs`

## SELFTEST-02 — market-open self-test detected a failure

**Trigger:** the same scheduler returned `Degraded` or `Critical`.
Severity::Critical so the operator pages immediately even on the
`Degraded` path — false-OK Rule 11 says we surface ANY failure, not
just the truly fatal ones.

**Sub-check semantics:**

| Sub-check name | Critical? | Meaning of failure |
|---|---|---|
| `main_feed_active` | YES | zero connections in `Connected` — no live ticks |
| `questdb_connected` | YES | persist will fail; rescue ring filling |
| `token_expiry_headroom` | YES | < 4h until JWT expiry — will die mid-session |
| `depth_20_active` | no | depth fan-out broken; main feed still OK |
| `depth_200_active` | no | ATM CE/PE depth missing; main feed still OK |
| `order_update_active` | no | order WS down; can place orders blind |
| `pipeline_active` | no | tick processor task dead |
| `recent_tick` | no | last tick > 60s old (silent socket likely) |

**Triage:**
1. Read the `failed: Vec<&'static str>` payload on the Telegram event
   to see which sub-checks tripped.
2. For each Critical sub-check, follow the named runbook:
   * `main_feed_active = 0` → `.claude/rules/project/disaster-recovery.md`
     scenario 6 (all 5 WS connections dropped).
   * `questdb_connected = false` → `BOOT-01` / `BOOT-02` runbook
     (`wave-2-error-codes.md`).
   * `token_expiry_headroom < 4h` → `AUTH-GAP-03` runbook
     (`wave-2-error-codes.md`).
3. For Degraded-only failures, check the relevant feed-watchdog
   metrics and re-run `make doctor`.
4. Persist the outcome in `selftest_audit` (Wave-2-D Item 9 table) for
   the day's audit trail; the row is keyed by
   `(trading_date_ist, check_name)` so re-runs deduplicate.

**Auto-triage safe:** NO (Critical severity).

**Source:** `crates/core/src/instrument/market_open_self_test.rs`,
notification variants `SelfTestDegraded` / `SelfTestCritical` in
`crates/core/src/notification/events.rs`.

**Ratchet tests:** see `market_open_self_test.rs::tests` —
`test_self_test_passes_when_all_seven_checks_green`,
`test_self_test_critical_when_main_feed_zero`,
`test_self_test_critical_when_questdb_disconnected`,
`test_self_test_critical_when_token_expired`,
`test_self_test_degraded_when_pipeline_inactive`,
`test_self_test_degraded_when_no_recent_tick`,
`test_self_test_critical_wins_over_degraded`,
`test_self_test_constants_pinned`.
