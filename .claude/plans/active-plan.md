# Implementation Plan: Zero-Touch Error Observability & Auto-Triage

**Status:** APPROVED
**Date:** 2026-04-18
**Approved by:** Parthiban ("go ahead with all the approach ... 100 percent automation dude")

**Decisions confirmed:**
- A: all phases, phased rollout (one PR per phase, Phase 0 first)
- B: yes, trim QuestDB 4GB → 3.5GB to fit Loki + Alloy
- C: Claude ALWAYS pings Parthiban first for novel errors; auto-PR only when classifier confidence ≥ 0.95 AND runbook rule explicitly opts in
- D: AWS SNS vars deferred (no AWS instance bought yet); Phase 4 becomes mechanical-only, activated when instance provisioned

## Honesty clause on "100%"

The operator requested 100% guarantees on coverage, testing, bug-fixing, monitoring, logging, alerting, security, uniqueness, dedup, O(1), zero-tick-loss, zero-WS-disconnect, zero-QuestDB-fail.

**PROVABLE 100%s (each = a mechanical CI gate or compile-time constraint):**
- Line + branch coverage 100% via `cargo llvm-cov --fail-under 100`
- Mutation score 100% (zero survivors) via `cargo-mutants`
- Fuzz coverage — 24h clean run in nightly CI via `cargo-fuzz`
- Banned-pattern scan 100% via `.claude/hooks/banned-pattern-scanner.sh`
- Every pub fn has test via `pub-fn-test-guard`
- Every error path has `error!` via new meta-guard
- Every Result has `?` or explicit `match` via `#![deny(unused_must_use)]`
- Every shared collection keyed by `security_id` uses `(id, segment)` via hook category 5
- Every DEDUP key has segment via `dedup_segment_meta_guard`
- Every dashboard has status filter + segment via `grafana_dashboard_snapshot_filter_guard`
- O(1) hot path — zero alloc DHAT test + banned `.clone()/Vec::new()/format!/.collect()/dyn` hook
- Wire-to-done latency ≤ 10µs — Criterion benchmark budget gate
- Zero tick loss DURING normal operation — `tv_ticks_lost_total` assert-0 in market-hours integration test

**NON-PROVABLE (physics/external), replaced by MEASURABLE SLAs:**
- "WebSocket never disconnects" → "Reconnect ≤ 500ms, zero-loss via WAL replay, chaos test enforces"
- "QuestDB never fails" → "Backpressure → disk spill → auto-resume within 30s, chaos test enforces"
- "No future bug" → "Zero known bugs at HEAD + every pattern we've seen is a CI block"
- "Every scenario covered" → "Every enumerated scenario tested + fuzz finds unknowns + mutation verifies non-vacuity"

## Goal

Parthiban never monitors manually — Claude Code (or a cron-driven agent) owns the full error lifecycle: capture → classify → dedup → triage → fix-or-escalate. Works identically on Mac (dev) and AWS c7i.xlarge Mumbai (prod), within the ₹5,000/mo budget.

## Research Summary (from 4 parallel audits)

**Already wired (keep):**
- Prometheus + Alertmanager + 65 alert rules → Telegram webhook via Rust `NotificationService` with edge-triggered dedup (10s–1h).
- `NotificationService` carries structured events (`security_id`, `symbol`, `reason`, severity). Critical/High also go AWS SNS SMS.
- Grafana: 6 dashboards against Prometheus + QuestDB PG wire.
- AWS Terraform: 5 CloudWatch alarms, SNS topic, 14-day CloudWatch log group, EventBridge EC2 start/stop.
- `.claude/`: 33 hooks, 31 rules, `notification-log.sh` already audits every Telegram message.
- Systemd: `tickvault.service` with `WatchdogSec=60`, `Restart=always`.

**Gaps blocking full automation:**
- 39 flush/broadcast/persist failures logged at `warn!` — NEVER reach Telegram (Rule 5 violated).
- Loki + Alloy removed for memory budget → no LogQL alerting ("3 ERRORs in 5min" impossible today).
- App container logs not in CloudWatch (only systemd journalctl captured).
- No app-emitted CloudWatch custom metrics (Prometheus-only).
- SNS has no Terraform-managed subscription (manual `aws sns subscribe` post-deploy).
- Error codes (I-P*, OMS-*, WS-*, STORAGE-*, DH-9xx) are string literals in rustdoc — not a typed enum Claude can enumerate.
- No log-ingestion MCP, no dedup-by-signature, no auto-triage YAML rules, no "Claude watches logs" daemon.
- Traefik dashboard public + insecure on AWS EC2 IP.

## Architecture (after plan)

```
Rust app  ──┬── metrics::*     → Prometheus :9091 ──┐
            ├── error!/warn!   → tracing subscriber ┤
            │                    ├→ JSONL stdout  ──┼→ docker awslogs → CloudWatch Logs
            │                    └→ errors.jsonl  ──┤       (AWS only)
            └── notify(event)  → Alertmanager WH ──┤
                                                    │
Prometheus ──(65 rules)── Alertmanager ────────────┤
CloudWatch ──(5 alarms) ── SNS topic ──────────────┤
                                                    │
                     ┌──────────────────────────────┘
                     ↓
              NotificationService (dedup by signature hash, edge-trigger)
                     │
       ┌─────────────┼──────────────┐
       ↓             ↓              ↓
   Telegram     SNS SMS         errors.summary.md
                              (auto-generated every 60s
                               — human AND Claude-readable)
                                     │
                                     ↓
                       .claude/hooks/error-triage.sh
                                     │
                         ┌───────────┴────────────┐
                         ↓                        ↓
                    Known signature          Novel signature
                    (YAML runbook)           (escalate, open
                    auto-execute or          GitHub Issue, ping
                    link runbook             Claude agent)
```

## Plan Items (10 phases, each independently approvable)

### Phase 0 — Stop the bleeding (BLOCKING for everything else)

- [x] 0.1 Upgrade 39 `warn!` → `error!` on flush/broadcast/persist failures per audit-findings Rule 5
  - Files: `crates/storage/src/tick_persistence.rs` (8 sites), `crates/storage/src/candle_persistence.rs` (9 sites), `crates/app/src/main.rs` (5 sites), `crates/core/src/pipeline/tick_processor.rs` (2 sites), `crates/app/src/greeks_pipeline.rs` (1 site), `crates/core/src/historical/candle_fetcher.rs` (1 site)
  - Tests: add `test_flush_failure_emits_error_level` source-scan guard in `crates/storage/tests/error_level_meta_guard.rs`

- [x] 0.2 Compile-enforce error capture
  - Files: every `crates/*/src/lib.rs`
  - Adds: `#![cfg_attr(not(test), deny(unused_must_use))]`, `#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]`, `#![cfg_attr(not(test), warn(clippy::unused_async))]`
  - Tests: existing clippy gate catches regressions

### Phase 1 — Structured error catalogue

- [x] 1.1 Central `ErrorCode` enum in `crates/common/src/error.rs`
  - Variants: every I-P*, OMS-GAP-*, WS-GAP-*, STORAGE-GAP-*, RISK-GAP-*, AUTH-GAP-*, DH-9xx, 8xx codes
  - Methods: `.code_str()`, `.severity()`, `.runbook_url()`, `.is_actionable()`, `.suggested_fix()`
  - Tests: `test_every_code_has_rule_doc`, `test_every_rule_doc_has_code`

- [~] 1.2 Every `error!` / `warn!` on known paths must carry `code = ErrorCode::X.code_str()` as a structured field
  - Files: `crates/*/src/**/*.rs` (mechanical migration, one ErrorCode per existing code)
  - Tests: `crates/common/tests/error_code_field_guard.rs` scans source + fails if an error site in a tagged module lacks `code =`

### Phase 2 — JSONL error stream + rotation

- [x] 2.1 Tracing subscriber emits a separate `data/logs/errors.jsonl` filtered to ERROR-level only, one JSON object per line, `{ts_ist, code, severity, signature_hash, module, message, fields}`
  - File: `crates/app/src/observability.rs`
  - Uses `tracing-appender` file rotation (hourly, 48h retention)
  - `signature_hash` = first 16 hex chars of sha256(code + module + truncated_message)

- [x] 2.2 Makefile target `make tail-errors` — `jq`-formatted live view
  - File: `Makefile`

### Phase 3 — Loki/Alloy re-add under memory budget

- [ ] 3.1 Re-add slim Loki + Alloy to `docker-compose.yml` with tight memory limits that fit the 8GB c7i.xlarge budget after QuestDB trim from 4GB to 3.5GB
  - Files: `deploy/docker/docker-compose.yml`, `deploy/docker/loki/loki-config.yml`, `deploy/docker/alloy/alloy-config.alloy`
  - Mem: Loki 384MB, Alloy 256MB (total new: 640MB; offset by QuestDB 512MB trim → net -128MB, still within budget)
  - Alloy scrapes: `data/logs/errors.jsonl` + `docker logs` for `tv-*` containers

- [ ] 3.2 Loki-ruler log-pattern alerts
  - File: `deploy/docker/loki/rules.yml`
  - Rules: `ErrorBurst` (>5 errors in 5min with same signature_hash), `NovelErrorSignature` (signature_hash never seen before in rolling 24h), `FlushErrorStorm`, `AuthFailureBurst`
  - Route: Alertmanager → existing `NotificationService` webhook

- [ ] 3.3 Re-add the "Errors" dashboard to Grafana
  - File: `deploy/docker/grafana/dashboards/errors.json`
  - Panels: live error tail (LogQL), top-10 signatures 1h, error rate time series, novel signatures callout, code → severity heatmap
  - Single-page dashboard operator can glance at

### Phase 4 — AWS observability parity

- [ ] 4.1 Terraform: SNS subscriptions (email + Telegram HTTPS webhook) auto-created from `var.operator_email` and `var.telegram_webhook_url`
  - File: `deploy/aws/terraform/sns.tf`
  - Tests: `terraform plan` + `terraform validate` in CI

- [ ] 4.2 Docker containers use `awslogs` driver in the AWS-specific compose override
  - Files: `deploy/docker/docker-compose.aws.override.yml`, user-data.sh.tftpl
  - Log group: `/tickvault/prod/containers/<service>`; retention 14d

- [ ] 4.3 App emits CloudWatch custom metrics via a shim when `AWS_REGION` env set
  - File: `crates/app/src/observability.rs`
  - Uses `aws-sdk-cloudwatch` crate (already pinned — confirm with `dependency-checker`); 60s batch PutMetricData; only the 10 business-critical metrics to stay under free tier

- [ ] 4.4 Secure Traefik dashboard
  - File: `deploy/docker/traefik/traefik.yml` (remove `insecure=true`); `dynamic/services.yml` (add HTTP BasicAuth middleware, credentials from SSM)

### Phase 5 — errors.summary.md (Claude-readable single-file view)

- [x] 5.1 Background task inside the Rust app emits `data/logs/errors.summary.md` every 60s — markdown tables:
  - "Active errors (last 15min)" — signature | count | severity | code | runbook | first_seen_ist
  - "Novel today" — signatures first seen in last 24h
  - "Infrastructure up/down" — each alert from Prometheus + CloudWatch
  - "System KPIs" — tick rate, WS connections, memory, disk
  - File: `crates/core/src/notification/summary_writer.rs` (new)
  - Tests: `test_summary_file_regenerates`, `test_summary_edge_triggered_novel`

- [x] 5.2 `make status` reads this file + prints dashboard
  - File: `Makefile`, `scripts/status-dashboard.sh`

### Phase 6 — Auto-triage rules engine

- [x] 6.1 YAML rules file `.claude/triage/error-rules.yaml`
  - Schema: `{ code, signature_pattern, action: "auto_fix" | "auto_restart" | "escalate" | "silence", runbook_url, confidence_threshold, cooldown_secs }`
  - Seed rules for known recurring errors (I-P1-11 collision → silence; CandleVerificationFailed → escalate with jq'd context; Depth spot stale → auto-restart rebalancer once; Token expired → auto-refresh already handled → silence)

- [x] 6.2 Triage hook `.claude/hooks/error-triage.sh`
  - Trigger: cron (every 60s via a userland launchd/systemd-user timer) OR `Stop` / `SessionStart` hooks so it fires during Claude sessions
  - Reads `errors.summary.md` + `error-rules.yaml` → applies rules → posts actions to Telegram and/or invokes `scripts/auto-fix-<action>.sh`
  - Edge-triggered via `.claude/state/triage-seen.jsonl` (signature_hash + last_action_ts)
  - Rate limit: max 10 triages/hour/signature

- [~] 6.3 Novel-signature escalation
  - When an error has `signature_hash` not in `triage-seen.jsonl`, `error-triage.sh` opens a draft GitHub Issue via existing `mcp__github__issue_write` (from the user's session) — issue body includes: code, severity, runbook, first 50 matching log lines, context links (Grafana, CloudWatch)

### Phase 7 — Claude-watches-logs daemon

- [x] 7.1 `/loop` skill runbook
  - File: `.claude/triage/claude-loop-prompt.md`
  - Prompt: "Read data/logs/errors.summary.md. For each row flagged 'ACTION_REQUIRED': investigate root cause, propose fix if tractable, open draft PR; otherwise ping operator with context."
  - Invoked via: `/loop 5m .claude/triage/claude-loop-prompt.md`

- [x] 7.2 MCP server for log access
  - File: `scripts/mcp-servers/tickvault-logs/server.py` (new)
  - Exposes: `tail_errors(signature, limit)`, `query_loki(logql)`, `query_cloudwatch(expression)`, `list_novel_signatures(since)`
  - Registered in `.mcp.json`
  - Permission-gated via `.claude/settings.json` allow list

### Phase 8 — Self-healing triggers

- [x] 8.1 Common auto-fix scripts
  - `scripts/auto-fix-restart-depth.sh` — calls existing depth rebalancer reset endpoint
  - `scripts/auto-fix-clear-spill.sh` — drains disk spill buffer after QuestDB recovery
  - `scripts/auto-fix-refresh-instruments.sh` — triggers `POST /api/instruments/rebuild`
  - Each script: idempotent, logs to `data/logs/auto-fix.log`, includes dry-run flag

- [ ] 8.2 CloudWatch webhook → Claude Code session (AWS only)
  - Lambda function: receives SNS → invokes `tmux send-keys 'claude --resume <session_id> "triage $ALARM"'` on the EC2 via SSM RunCommand
  - File: `deploy/aws/terraform/claude-triage-lambda.tf` + `deploy/aws/lambda/claude-triage.py`

### Phase 10 — Zero-tick-loss hardening (explicit SLA)

- [~] 10.1 Three-tier buffer proof
  - `crates/core/src/pipeline/tick_processor.rs` — SPSC 65K ring → disk spill (rkyv) → WAL replay on reconnect
  - Metric: `tv_ticks_lost_total` — must remain 0 during market hours; increments only on explicit drop decision with root cause label
  - Test: `crates/storage/tests/zero_tick_loss_invariant.rs` — integration test kills + resumes QuestDB, drops + resumes WS, asserts final stored count = emitted count

- [ ] 10.2 Sequence-hole detector
  - For each (security_id, segment), maintain last-exchange-timestamp; gap > 2s during market hours fires `TickGapDetected` with exact missed window
  - Already partially in `crates/trading/src/risk/tick_gap.rs` — extend + wire test

- [ ] 10.3 End-to-end "no lost tick" chaos test (nightly CI only)
  - `crates/storage/tests/chaos_zero_loss.rs`
  - Simulates: WS random disconnect (1% per minute), QuestDB SIGSTOP for 30s, disk near-full, Valkey down
  - Asserts: emitted = persisted after cool-down; never panic

### Phase 11 — WebSocket + QuestDB resilience SLAs (chaos)

- [x] 11.1-guard WS liveness SLA
  - Reconnect latency histogram `tv_ws_reconnect_duration_ms` — p99 < 500ms
  - Prometheus alert: `WsReconnectSlowP99` fires at > 1s for 2m
  - Chaos test: kill WS every 5 min × 10 iterations, assert p99 SLA

- [x] 11.2-guard QuestDB backpressure SLA
  - `tv_questdb_backpressure_duration_ms`; app stays up, spills to disk
  - Alert: `QuestDbBackpressureCritical` > 10s
  - Chaos test: SIGSTOP QuestDB for 30s, unSTOP, assert: 0 panics + all buffered data flushed within 30s + `tv_ticks_lost_total` still 0

- [x] 11.3-guard Valkey cache fallback
  - On Valkey down, non-critical features degrade gracefully; trading continues; alert fires
  - Chaos test: `docker kill tv-valkey`, assert app stays healthy, auto-reconnect on restart

### Phase 12 — Coverage / Mutation / Fuzz ratchet (100% or fail)

- [x] 12.1 Line + branch coverage 100%
  - CI gate: `cargo llvm-cov --workspace --fail-under-lines 100 --fail-under-regions 100`
  - File: `quality/crate-coverage-thresholds.toml` — set all crates to 100
  - Any exemption requires a `// COVERAGE-EXEMPT: <reason>` comment + operator sign-off

- [x] 12.2 Mutation score 100% (zero survivors)
  - CI gate: `cargo mutants --workspace --error-exit-code 1 -- --release`
  - Schedule: nightly, PR blocks if new survivor introduced
  - File: `.github/workflows/mutation.yml` (extend)

- [~] 12.3 Fuzz 24h clean
  - CI gate: nightly runs `cargo fuzz run tick_parser` + `cargo fuzz run config_parser` for 8h each
  - Zero crash + zero hang = pass
  - File: `.github/workflows/fuzz.yml` (extend duration)

- [ ] 12.4 `#![deny(warnings)]` workspace-wide (prod builds)
  - All `crates/*/src/lib.rs` get `#![cfg_attr(not(test), deny(warnings))]`
  - Any dep warning blocks the build

- [x] 12.5 O(1) hot-path ratchet
  - DHAT test budget: zero heap alloc per tick in `crates/core/tests/dhat_tick_processor.rs`
  - Benchmark budget: `tick_pipeline_routing < 100ns`, `papaya_lookup < 50ns`, `full_tick_processing < 10µs` in `quality/benchmark-budgets.toml`
  - CI gate: 5% regression on any budget = block

- [x] 12.6 Real-time self-check on every boot
  - `crates/app/src/main.rs` runs a 30-second smoke test at boot during market hours: synthesizes N synthetic ticks through the non-prod test pipeline, asserts they arrive in QuestDB, asserts O(1) latency samples within budget
  - On failure → HALT + CRITICAL Telegram

### Phase 9 — Dashboards + validation

- [x] 9.1 Single-page "Operator Health" Grafana dashboard
  - File: `deploy/docker/grafana/dashboards/operator-health.json`
  - One panel for every "Is it working?" question: trading live? WS UP? depth streaming? errors rising? P&L within limits? auto-triage running? CloudWatch alarms green?

- [x] 9.2 End-to-end validation script
  - File: `scripts/validate-automation.sh`
  - Simulates each known error code, asserts Telegram alert fires within SLA, asserts auto-fix runs (dry-run), asserts summary file regenerates

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Flush failure during live feed | `error!` → Telegram alert in ≤15s, counter increments, auto-clear on next successful flush |
| 2 | Known recurring I-P1-11 collision | Logged, counted in Prometheus gauge, **no Telegram** (silenced by triage rule) |
| 3 | Novel error signature never seen before | Telegram alert + GitHub draft Issue + `errors.summary.md` flags "novel" |
| 4 | Depth spot stale post-market | Silenced (market-hours gate — Rule 3) |
| 5 | CloudWatch alarm EC2 high CPU | SNS → Telegram (if subscribed) + email + optional Lambda → Claude triage |
| 6 | Dhan token 24h expiry | Auto-refresh happens → success INFO log → no escalation |
| 7 | Loki down | Prometheus `TargetDown` fires → operator alerted; app keeps running (errors still go Telegram via direct path) |
| 8 | Operator wakes up at 08:00 IST | Opens Telegram, sees zero-or-few alerts; opens Grafana "Operator Health" — one page, all panels green or red with links to summary |

## Tradeoffs & decisions to confirm with Parthiban

1. **Loki re-add:** costs ~640MB RAM, saves 4GB QuestDB pressure via 512MB trim. Alternative: skip Loki entirely, push errors via direct Alertmanager webhook. **Default: re-add** (LogQL is worth it).
2. **CloudWatch custom metrics:** 10 metrics × 60s batching = ~432k PutMetricData/month → still free tier. Alternative: Prometheus-only and rely on EC2 staying up. **Default: emit the 10** (belt-and-suspenders).
3. **Claude-watches-logs via `/loop` vs cron:** `/loop` only runs when Claude Code is open; cron runs 24/7 but can't do the "novel error → open PR" flow. **Default: both** — cron posts summary + Telegram; `/loop` handles triage when Claude session is active.
4. **GitHub Issue auto-open for novel errors:** risks noise if classifier is wrong. **Default: ON with 1-issue-per-signature-per-24h rate limit.**
5. **Scope:** all 10 phases in one shot vs phased rollout over several PRs. **Default: phased — one PR per phase, starting with Phase 0 (critical bug fixes) immediately.**

## Estimated effort

- Phase 0: ~45min (mechanical edit of 39 sites + guard test)
- Phase 1: ~3h (enum + migration across ~150 error sites)
- Phase 2: ~1h (subscriber config + rotation)
- Phase 3: ~2h (Docker wiring + Loki rules + dashboard)
- Phase 4: ~3h (Terraform + awslogs + CloudWatch shim)
- Phase 5: ~2h (summary writer + status cmd)
- Phase 6: ~3h (YAML schema + triage hook + tests)
- Phase 7: ~2h (MCP server + loop prompt)
- Phase 8: ~3h (auto-fix scripts + Lambda)
- Phase 9: ~2h (dashboard + e2e validation)

**Total: ~22h of focused work across ~10 PRs.** Phase 0 is ready to execute in the same session as approval.

## Open questions

- (A) Approve the whole plan, phase-by-phase, or only Phase 0 now?
- (B) Budget for Loki re-add (Q: trim QuestDB from 4GB → 3.5GB acceptable?)
- (C) Should Claude auto-open PRs for novel errors, or always escalate to Parthiban first?
- (D) `operator_email` and `telegram_webhook_url` for Terraform SNS subscriptions — provide now or leave as pending vars?
