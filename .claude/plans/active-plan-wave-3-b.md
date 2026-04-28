# Implementation Plan: Wave-3-B — Telegram Bucket Coalescer (Item 11 narrow)

**Status:** APPROVED
**Date:** 2026-04-28
**Approved by:** Parthiban (operator) — "just fix everything you go ahead"
**Branch:** `claude/automation-charter-setup-fgrtf` (system-assigned; supersedes the prompt's `claude/wave-3-b-telegram-coalescer` reference per system instruction)
**Scope source:** Operator prompt SECTION 4 SCOPE (Wave-3-B Item 11 narrow). Excludes the master plan's GCRA rate-limit + bounded queue (those are deferred follow-ups).

## Three Principles

| # | Principle | Mechanism |
|---|-----------|-----------|
| 1 | O(1) hot path | Coalescer entry is `Mutex<HashMap>` lookup + bounded `ArrayVec` push; bypass path takes one comparison and returns. DHAT-pinned. |
| 2 | Uniqueness | Bucket key is `(event_topic: &'static str, severity: Severity)` — both Copy types, no allocation per insert |
| 3 | Dedup | The coalescer IS the deduplication primitive — burst of N identical-topic events → 1 summary message |

## Authoritative Facts (do NOT relitigate)

| Fact | Source |
|------|--------|
| Coalescer applies ONLY to Severity::Low and Severity::Info | Operator SCOPE 11.1 |
| Severity::Critical and Severity::High BYPASS coalescing | Operator SCOPE 11.1 |
| Severity::Medium passes through immediately (no coalescing) | Conservative default — Medium is operator-visible state change; matches the existing master plan's silence for Medium |
| Default window 60s, configurable | Operator SCOPE 11.1 |
| Counters: `tv_telegram_dispatched_total{severity, coalesced}`, `tv_telegram_dropped_total{reason}` | Operator SCOPE 11.2 |
| C9 flag `telegram_bucket_coalescer` already exists, defaults to `true` | `crates/common/src/config.rs:109,130` |
| Notification module is COLD path; tokio::spawn allowed | `crates/core/src/notification/service.rs:22` doc |
| ErrorCode prefix expansion needs `TELEGRAM-` allowlist | `crates/common/src/error_code.rs:823-825` |

## Plan Items (9-box per item)

- [ ] **11.1 / 11.2 — `TelegramCoalescer` module + counters**
  - Files: `crates/core/src/notification/coalescer.rs` (NEW), `crates/core/src/notification/mod.rs`
  - Tests: `test_low_severity_coalesces_within_window`, `test_info_severity_coalesces_within_window`, `test_critical_bypasses_coalescer`, `test_high_bypasses_coalescer`, `test_medium_bypasses_coalescer`, `test_summary_includes_count_first_last_samples`, `test_drain_clears_buckets`, `test_sample_cap_at_10`
  - 9-box: typed event = N/A (this IS the dispatcher); ErrorCode `TELEGRAM-01`/`TELEGRAM-02`; Prom counters defined; Grafana panel new; Alert new; Triage rule new; Call site = `service.rs::notify`; Ratchet test = `test_telegram_*` unit tests + DHAT

- [ ] **11.1 — `topic()` method on `NotificationEvent`**
  - Files: `crates/core/src/notification/events.rs`
  - Tests: `test_topic_returns_static_str_for_each_variant_kind`
  - Returns `&'static str` of variant name (no allocation)

- [ ] **11.1 — Wire coalescer into `NotificationService`**
  - Files: `crates/core/src/notification/service.rs`
  - Tests: `test_service_with_coalescer_disabled_passes_through`, `test_service_with_coalescer_enabled_coalesces_low_severity`
  - Field: `coalescer: Option<Arc<TelegramCoalescer>>` (None = legacy passthrough)
  - Background flush task spawned at `initialize()` with cancellation token

- [ ] **11.5 — ErrorCodes TELEGRAM-01 / TELEGRAM-02**
  - Files: `crates/common/src/error_code.rs`
  - Tests: existing exhaustive ratchets auto-cover; bump count 77 → 79; expand prefix allowlist
  - `Telegram01Dropped` → Severity::Medium (drop = visible quality degradation)
  - `Telegram02CoalescerStateInconsistency` → Severity::Low (informational, self-recovers on next drain)

- [ ] **11.5 — Runbook for TELEGRAM-01 / TELEGRAM-02**
  - Files: `.claude/rules/project/wave-3-error-codes.md` (NEW)
  - Tests: `every_runbook_path_exists_on_disk` (already in `error_code_rule_file_crossref.rs`)

- [ ] **11.5 — Triage rules**
  - Files: `.claude/triage/error-rules.yaml`
  - Tests: existing triage_rules_guard meta-test

- [ ] **11.6 — Grafana panel + dashboard guard**
  - Files: `deploy/docker/grafana/dashboards/operator-health.json`, `crates/storage/tests/operator_health_dashboard_guard.rs`
  - Tests: `operator_health_dashboard_has_every_required_panel`
  - Panel title: "Telegram dispatch rate (5m)"
  - Expr: `increase(tv_telegram_dispatched_total[5m])` (Rule 12 — wrap counters)

- [ ] **11.6 — Prometheus alert**
  - Files: `deploy/docker/grafana/provisioning/alerting/alerts.yml`
  - Expr: `rate(tv_telegram_dropped_total[1m]) > 0.1`, for 60s, severity HIGH
  - Title: "Telegram drops detected — operator alerts may be missed"

- [ ] **11.4 — DHAT zero-alloc bypass test**
  - Files: `crates/core/tests/dhat_telegram_dispatcher.rs` (NEW)
  - Tests: `bypass_path_zero_allocation` (Critical/High events do not allocate in the coalescer pre-spawn check)

- [ ] **11.4 — Service-level integration test**
  - Files: `crates/core/src/notification/service.rs` `#[cfg(test)] mod tests`
  - Tests: `test_dispatched_counter_labels_set_on_send`, `test_dropped_counter_increments_on_drop_reason`

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 100× `WebSocketDisconnectedOffHours` (Severity::Low) within 60s | 1 summary Telegram with count=100, samples ≤10, `tv_telegram_dispatched_total{severity="low",coalesced="true"}` += 1, `tv_telegram_dispatched_total{severity="low",coalesced="false"}` not incremented |
| 2 | 1× `RiskHalt` (Severity::Critical) | Sent immediately, `tv_telegram_dispatched_total{severity="critical",coalesced="false"}` += 1 |
| 3 | Coalescer disabled (`features.telegram_bucket_coalescer = false`) | All events pass through unchanged (legacy path) |
| 4 | Bucket pruned after drain | Next event in same bucket starts a fresh window |
| 5 | Sample cap | 100× events in window → samples Vec is capped at 10 (no unbounded growth) |
| 6 | Critical from a hot-path call site | DHAT confirms zero allocation in the coalescer pre-spawn check |

## Verification Gates

```bash
cargo check -p tickvault-core
cargo test -p tickvault-common --lib
cargo test -p tickvault-core --lib
cargo test -p tickvault-storage --test operator_health_dashboard_guard
cargo test -p tickvault-core --features dhat --test dhat_telegram_dispatcher
bash .claude/hooks/banned-pattern-scanner.sh
python3 -c "import yaml; yaml.safe_load(open('deploy/docker/grafana/provisioning/alerting/alerts.yml'))"
python3 -c "import json; json.load(open('deploy/docker/grafana/dashboards/operator-health.json'))"
```

## File Totals (estimate)

| Type | Lines |
|------|-------|
| Production code | ~450 |
| Tests | ~350 |
| YAML / JSON | ~80 |
| Markdown | ~120 |
| **Total** | **~1,000** |
