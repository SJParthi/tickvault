# Implementation Plan: Wave-1-3 Live Hotfixes (4 bugs caught at 12:49 IST 2026-04-28)

**Status:** VERIFIED
**Date:** 2026-04-28
**Approved by:** Parthiban (operator) — verbatim "fix everything"
**Branch:** `claude/verify-waves-status-HQP6A`
**Triggering incident:** Live observation at 12:49 IST 2026-04-28: boot_audit DDL failed, phase2_audit insert failed (nanos→micros), SLO-02 Telegram 400 Bad Request, Grafana false-healthy.

## Plan Items

- [x] **1. Add `ts` to DEDUP keys for boot_audit + selftest_audit** (fixes AUDIT-04 + missing selftest writes)
  - Files: crates/storage/src/boot_audit_persistence.rs
  - Files: crates/storage/src/selftest_audit_persistence.rs
  - Tests: test_dedup_key_includes_designated_timestamp

- [x] **2. Convert nanos to micros in all 6 audit INSERTs** (fixes AUDIT-01 across the family)
  - Files: crates/storage/src/boot_audit_persistence.rs
  - Files: crates/storage/src/selftest_audit_persistence.rs
  - Files: crates/storage/src/phase2_audit_persistence.rs
  - Files: crates/storage/src/ws_reconnect_audit_persistence.rs
  - Files: crates/storage/src/depth_rebalance_audit_persistence.rs
  - Files: crates/storage/src/order_audit_persistence.rs
  - Tests: test_insert_sql_uses_microseconds_not_nanoseconds

- [x] **3. Strip raw `<` from RealtimeGuaranteeCritical body** (fixes TELEGRAM-01 mid-batch failures)
  - Files: crates/core/src/notification/events.rs
  - Tests: test_realtime_guarantee_critical_body_has_no_unescaped_html_brackets
  - Tests: test_realtime_guarantee_degraded_body_has_no_unescaped_html_brackets
  - Tests: test_realtime_guarantee_healthy_body_has_no_unescaped_html_brackets

- [x] **4. Add HTTP health probe for Grafana** (fixes false-healthy at boot)
  - Files: crates/app/src/infra.rs
  - Tests: test_grafana_health_probe_polls_api_health_endpoint

## Verification

```bash
cargo check -p tickvault-storage -p tickvault-core -p tickvault-app
cargo test -p tickvault-storage --lib
cargo test -p tickvault-core --lib notification::events
cargo test -p tickvault-app --lib infra
```

All commands pass on commit. 38 audit-module + 3 SLO-body + 1 Grafana-probe tests green.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot at 12:49 IST | `boot_audit` DDL returns 200; `audit table ready` log emitted |
| 2 | Phase 2 dispatch at 12:50 | `phase2_audit` row inserted, no "timestamp beyond 9999-12-31" |
| 3 | SLO score crosses 0.80 | Telegram message delivered (no 400 Bad Request from `<` in body) |
| 4 | Cold boot, Grafana takes 8s to serve HTTP | App waits for HTTP-200 from `/api/health` before logging "service is healthy" |
