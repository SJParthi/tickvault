# Zero-Touch Observability Stack — Complete Reference

> **CloudWatch-only migration (#O1–#O4, 2026-05-19/05-24):** the Grafana
> (#O1), Alertmanager (#O2), Prometheus (#O3) and Valkey (#O4) containers
> were removed; Loki/Alloy/Jaeger/Traefik were retired earlier. The live
> runtime is QuestDB + the tickvault app + AWS CloudWatch only. CloudWatch
> (metrics + logs + alarms + dashboards) is the entire prod observability
> layer; the QuestDB web console covers ad-hoc queries. The diagram and the
> phase-by-phase HISTORY table below still reference the retired
> Loki/Alloy/Grafana/Alertmanager hops — those are accurate as historical
> deliverables but are NOT the current live path (annotations inline).

> **Ground truth this file derives from (auto-loaded):**
> `.claude/rules/project/observability-architecture.md`
>
> **Scope:** The end-to-end chain that takes a `tracing::error!` macro
> call and turns it into either (a) silenced noise, (b) an auto-fix
> script invocation, or (c) a Telegram alert + draft GitHub issue —
> with zero human intervention per the operator's 100%-automation
> directive.
>
> **Audience:** Future Claude Code sessions, operators reading the
> repo for the first time, and any new engineer onboarding. Reading
> this ONE document means you don't need to rediscover 40+ commits
> worth of design decisions.

## The one-paragraph version

`error!(code = ErrorCode::X, ...)` fires inside the Rust app.
`tracing-subscriber` fans it to five sinks simultaneously: stdout,
`data/logs/app.log`, `data/logs/errors.log` (WARN+), a new
`data/logs/errors.jsonl.YYYY-MM-DD-HH` (ERROR-only JSONL, hourly
rotated, 48h retention swept by a tokio task), and the
`NotificationService` webhook which relays to Telegram + AWS SNS.
A background `summary_writer` task reads the JSONL every 60s and
writes `data/logs/errors.summary.md`. A shell hook
(`.claude/hooks/error-triage.sh`) OR Claude Code's `/loop`
runbook reads the summary, applies
`.claude/triage/error-rules.yaml`, and takes one of four actions:
silence, auto_restart, auto_fix, escalate. Optional Loki + Alloy
stack (opt-in via `--profile logs`) adds LogQL-based alerts on top
of the file layer. On AWS, CloudWatch alarms flow into an opt-in
Lambda that runs `tmux send-keys` to trigger a Claude Code session
on the EC2 instance. Every layer has a pinned Rust meta-guard
blocking regression. Validation: `make validate-automation` runs
25 checks in 30 seconds.

## Visual flow (ASCII)

```
┌────────────────────────────────────────────────────────────────────┐
│  Rust app — any tracing::error!(code = ErrorCode::X, ...) site     │
└──────────────────────────────┬─────────────────────────────────────┘
                               │
         ┌─────────────────────┼─────────────────────────────────┐
         │                     │                                 │
         ▼                     ▼                                 ▼
  stdout/journald   data/logs/app.YYYY-MM-DD.log      errors.log (WARN+)
                                                                  │
                    data/logs/errors.jsonl.YYYY-MM-DD-HH ◄────────┤
                         │      (Phase 2 JSONL, ERROR-only)
                         │      rotated hourly, 48h swept
                         ▼
          CloudWatch Logs agent    NotificationService
                    │                   │
                    ▼                   ▼
            AWS CloudWatch        Telegram + AWS SNS
            (logs + metrics       (edge-trigger dedup)
             + alarms)                  │
                    │                   │
                    └───────┬───────────┘
                            ▼
                  summary_writer (60s tick)
                   data/logs/errors.summary.md
                            │
         ┌──────────────────┼──────────────────┐
         │                                     │
         ▼                                     ▼
  .claude/hooks/          Claude Code /loop 5m .claude/triage/
  error-triage.sh         claude-loop-prompt.md
  (shell, dep-free)
         │                                     │
         │  reads .claude/triage/error-rules.yaml
         │                                     │
         ▼                                     ▼
  silence | auto_fix | auto_restart | escalate
         │
         ├── auto-fix: scripts/auto-fix-*.sh (dry-run then real)
         ├── silence: append to .claude/state/triage-seen.jsonl
         └── escalate: Telegram + draft GitHub issue (via github MCP)

AWS path (opt-in):
  CloudWatch alarm -> SNS topic tv_alerts -> claude-triage Lambda
  -> SSM RunCommand -> tmux send-keys on EC2
  -> claude CLI session reads the alert, runs the same triage flow.
```

## Phase-by-phase deliverables

| Phase | Shipped in commit | What it delivers |
|---|---|---|
| 0.1 | `b53bca6`, `0ca2cb6`, `f37484d` | 25 `warn!`→`error!` escalations across flush/persist/drain paths. Operator Telegram now fires on data-loss risk. |
| 0.2 | `eea39f7` | `#![cfg_attr(not(test), deny(unused_must_use))]` workspace-wide + `error_level_meta_guard` test (28 phrases ratcheted). |
| 1.1 | `e4fdf95` | `crates/common/src/error_code.rs` — 54-variant `ErrorCode` enum + 3 cross-ref tests. |
| 1.2 | `8d34421` | Sample production sites migrated to carry `code = ErrorCode::X.code_str()`; `error_code_tag_guard` meta-test. |
| 2.1 | `cce7188`, `d942695` | `tracing-appender` dep, `init_errors_jsonl_appender`, `sweep_errors_jsonl_retention`, wired into `main.rs` subscriber chain. |
| 2.2 | `a81206f` | `make tail-errors` + `make errors-summary` + `make triage-dry-run`/`triage-execute`. |
| 3.1, 3.2 | `26f99c5` | Loki + Alloy under `--profile logs`; 4 LogQL rules (`ErrorBurst`, `FlushErrorStorm`, `AuthFailureBurst`, `NovelErrorSignature`); 11-test `loki_alloy_profile_guard`. _(Loki/Alloy retired in the CloudWatch-only migration; logs now ship to CloudWatch Logs.)_ |
| 5.1, 5.2 | `7b87ab2` | `summary_writer` tokio task, FNV-1a signature grouping, 18 unit tests; `make status` integration. |
| 6.1, 6.2 | `a3145e9` | `error-rules.yaml` (7 seed rules), `error-triage.sh` shell hook, 3 auto-fix scripts, 7-test `triage_rules_guard`. |
| 7.1 | `a3145e9` | `claude-loop-prompt.md` runbook. |
| 7.2 | `8c53928` | `scripts/mcp-servers/tickvault-logs/server.py` — 5-tool MCP (`tail_errors`, `list_novel_signatures`, `summary_snapshot`, `triage_log_tail`, `signature_history`); 7-test `tickvault_logs_mcp_guard`. |
| 8.1 | `a3145e9`, `a81206f` | `scripts/auto-fix-{restart-depth,refresh-instruments,clear-spill}.sh`. |
| 8.2 | `c68346d` | `deploy/aws/lambda/claude-triage/` Lambda + `claude-triage-lambda.tf` (opt-in); 7-test `claude_triage_lambda_guard`. _(RETIRED 2026-07-18 — claude-triage lambda deleted in the Rust-only purge, never provisioned (0 of 13 AWS functions); the guard was deleted with it. See git history.)_ |
| 9.1 | `1cdd78a` | `operator-health.json` single-page Grafana dashboard (14 panels); 7-test `operator_health_dashboard_guard`. _(RETIRED in the CloudWatch-only migration #O1; the dashboard tree + its guard were deleted. CloudWatch Dashboards replace operator visualization in prod.)_ |
| 9.2 | `a81206f`, extended each phase | `scripts/validate-automation.sh` + `make validate-automation` — 25 end-to-end checks. |
| 10.1 | `275157a` | `zero_tick_loss_alert_guard` — pins tick-loss metric emissions + buffer capacity constant + doc coherence. _(Post #O3 the 4 Prometheus alert-rule assertions were removed when the Prometheus container retired; the early-warning signal moved to CloudWatch Alarms over the same metrics, which the guard still pins as EMITTED.)_ |
| 11 | `897f7b6` | `resilience_sla_alert_guard` — 6 tests pin WS/QuestDB/Valkey SLA alerts. |
| 12.1 | existing `quality/crate-coverage-thresholds.toml` | 100% line-coverage floor per crate, enforced by `scripts/coverage-gate.sh`. |
| 12.2 | existing `.github/workflows/mutation.yml:103-113` | Mutation zero-survivor gate. |
| 12.5 | existing `quality/benchmark-budgets.toml` | DHAT zero-alloc + Criterion latency budgets, 5% regression gate. |
| 12.6 | `9e807ca` | `observability_chain_e2e` integration test — in-process end-to-end proof; caught a real `flatten_event(true)` bug on first run. |

## Where every piece lives (canonical paths)

| Concept | Source of truth |
|---|---|
| Error taxonomy | `crates/common/src/error_code.rs` |
| 48h-swept JSONL stream | `crates/app/src/observability.rs::{init_errors_jsonl_appender, sweep_errors_jsonl_retention}` |
| Summary writer | `crates/core/src/notification/summary_writer.rs` |
| Triage rules | `.claude/triage/error-rules.yaml` |
| Claude loop prompt | `.claude/triage/claude-loop-prompt.md` |
| Shell triage hook | `.claude/hooks/error-triage.sh` |
| Auto-fix scripts | `scripts/auto-fix-*.sh` |
| MCP log server | `scripts/mcp-servers/tickvault-logs/server.py` |
| AWS triage Lambda | RETIRED 2026-07-18 — `deploy/aws/lambda/claude-triage/handler.py` deleted in the Rust-only purge, never provisioned (0 of 13 AWS functions); see git history |
| Lambda Terraform | RETIRED 2026-07-18 — `deploy/aws/terraform/claude-triage-lambda.tf` deleted with the lambda; see git history |
| Logs shipping (current) | CloudWatch Logs agent → CloudWatch Logs _(Loki/Alloy retired in the CloudWatch-only migration #O1–#O3)_ |
| Operator alarms (current) | AWS CloudWatch Alarms _(the Prometheus `tickvault-alerts.yml` rules + Alertmanager were retired #O2/#O3)_ |
| Operator dashboard (current) | CloudWatch Dashboards _(the local Grafana `operator-health.json` + its tree were retired #O1)_ |
| Validate automation | `scripts/validate-automation.sh` |

## All 25 validate-automation.sh checks

Run `make validate-automation` to exercise all of these in ~30 seconds.

**Compile gates (1):** workspace compiles.

**Observability guards (11):**
1. `error_code` unit tests (12 tests)
2. `error_code_rule_file_crossref` (3 tests)
3. `error_code_tag_guard` (2 tests)
4. `triage_rules_guard` (7 tests)
5. `error_level_meta_guard` (2 tests)
6. seal-ring ratchet suite (`candles::seal_ring` lib tests) _(2026-07-18: `zero_tick_loss_alert_guard` was DELETED with the tick rescue ring in the stage-4 dead-producer sweep)_
7. `summary_writer` unit tests (18 tests)
8. `observability` library tests (37 tests)
9. `observability_chain_e2e` (4 tests)
10. `operator_health_dashboard_guard` _(RETIRED with Grafana removal #O1)_
11. `resilience_sla_alert_guard` _(Prometheus-side assertions retired #O2/#O3; chaos tests remain)_
12. `tickvault_logs_mcp_guard` (7 tests)
13. `claude_triage_lambda_guard` _(RETIRED 2026-07-18 — deleted with the claude-triage lambda in the Rust-only purge; see git history)_
14. `loki_alloy_profile_guard` _(RETIRED with Loki/Alloy removal)_

**File-level invariants (10):**
- architecture doc / triage YAML / loop prompt / 4 auto-fix scripts / error-triage hook / MCP server / MCP self-test passes. _(The local operator-health dashboard invariant was retired with Grafana #O1; the dashboard now lives in CloudWatch.)_

**Source-code invariants (3):**
- `flatten_event(true)` in main.rs tracing layer (regression risk).
- `ADD COLUMN IF NOT EXISTS` in `instrument_persistence.rs` (schema self-heal).
- `deny(unused_must_use)` in every crate.

## Guarantees (mechanically enforced, not promised)

| Claim | Proof |
|---|---|
| Every `error!` reaches Telegram within ~15s | CloudWatch Alarms (over the metrics formerly scraped by Prometheus) + `error-level` tracing layer |
| No flush/persist/drain failure is silenced | `error_level_meta_guard` — 28 phrases pinned |
| Every ErrorCode has rule docs | `error_code_rule_file_crossref` forward+reverse |
| Seal-chain capacity + drop paging stay ratcheted | `seal_ring.rs` lib suite pins `SEAL_BUFFER_CAPACITY`; AGGREGATOR-DROP-01 pagers pinned by `seal_drop_paging_wiring_guard.rs` _(2026-07-18: `zero_tick_loss_alert_guard` + its tick metrics were DELETED with the tick rescue ring, stage-4 sweep)_ |
| WS/QuestDB SLA chaos coverage exists | `resilience_sla_alert_guard` Prometheus-side assertions retired #O2/#O3; chaos tests (`chaos_questdb_*`, `chaos_ws_*`, `chaos_valkey_kill`) remain |
| Operator dashboard has every required widget | CloudWatch Dashboards _(the Grafana `operator_health_dashboard_guard` was retired with Grafana #O1)_ |
| Loki/Alloy not in the default stack | RETIRED — Loki/Alloy removed entirely in the CloudWatch-only migration |
| Claude-triage Lambda never provisions until `enable_claude_triage_lambda = true` | RETIRED 2026-07-18 — the claude-triage lambda + `claude_triage_lambda_guard` were deleted in the Rust-only purge (never provisioned, 0 of 13 AWS functions); see git history |
| MCP log server exposes 5 tools, no pip deps | `tickvault_logs_mcp_guard` — imports allow-list enforced |
| Instrument lifecycle tables self-heal on boot | `ALTER TABLE ADD COLUMN IF NOT EXISTS` pattern — source-scan asserted |

## What operator runs each morning

```bash
# Check everything is green (30s)
make validate-automation

# Glance at the operator health dashboard
# (CloudWatch console → Dashboards → operator-health; the local Grafana
#  dashboard at :3000 was retired in the CloudWatch-only migration #O1)

# See recent errors (if any)
make errors-summary

# Live tail (optional)
make tail-errors

# Dry-run the triage hook (no auto-fix executed)
make triage-dry-run
```

If every widget is green on the CloudWatch operator-health dashboard and
`errors.summary.md` says "Zero ERROR-level events in the lookback
window", the system needs no attention. If anything is red, the
widget title + description names the runbook path to follow.

## What Claude Code runs on every triage cycle

See `.claude/triage/claude-loop-prompt.md` for the authoritative
runbook. The MCP server (`tickvault-logs`) gives Claude typed
access to the logs so it never has to parse free-text output.

## Pending integrations (not blockers for zero-touch)

- Phase 4 AWS CloudWatch parity: deferred until EC2 is provisioned.
  Terraform ready (`enable_claude_triage_lambda`-style gates).
- Phase 4.3 app metrics → CloudWatch: needs `aws-sdk-cloudwatch` dep.
  Not added yet — would emit the 10 business-critical metrics on a
  60s batch cadence.
- Phase 10.2 sequence-hole detector extension: being handled by a
  parallel Claude session.
- Phase 10.3 chaos test: needs a docker-kill test harness in CI
  (separate workstream).

## Change-control rules for this stack

New session adding/changing observability code:
1. Read `.claude/rules/project/observability-architecture.md` FIRST.
2. Run `make validate-automation` before any change — know the green
   baseline.
3. If adding a new error code: add an `ErrorCode` variant AND a rule-
   file mention in the SAME commit. Cross-ref test enforces both.
4. If adding a new Prometheus alert: add a pin to the relevant
   `*_alert_guard.rs` test OR create one.
5. If adding a new pub fn: either write a test OR add
   `// TEST-EXEMPT: <reason>` on the preceding line.
6. If adding a new dep: update `Cargo.toml` root with exact version
   (no `^`, `~`, `*`, `>=`) AND document the reason inline as a comment.
7. Run `make validate-automation` AGAIN after your change. Count
   can only stay the same or go up, never down.
