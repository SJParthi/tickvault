# Implementation Plan: Observability stack → CloudWatch-only

**Status:** APPROVED
**Date:** 2026-05-20
**Approved by:** Parthiban — verbatim: "except questdb app and cloud
watch we planned to remove everything."

## Goal

Narrow the runtime to **three components**: QuestDB + the tickvault app
+ AWS CloudWatch. Remove the Grafana, Prometheus, Alertmanager and
Valkey containers and every code path, guard test, rule file and doc
that depends on them. CloudWatch becomes the entire observability layer
(metrics, logs, alarms).

Authority: `.claude/rules/project/aws-budget.md` "OPERATOR DECISION
2026-05-20" section supersedes the stale Wave 7-A kept-service list.

## Per-Item Guarantee Matrix

Every item below is bound by the canonical 15-row "100% everything"
matrix + 7-row resilience matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. Each item lands
only inside the tested envelope: compile-verified, scoped tests green,
ratchets updated, one PR at a time per `pr-completion-protocol.md` §H.

## Hard rules

- **§H** — one PR open at a time; merge to `main` before the next.
- **Rule 14** — no skeleton / half-finished PRs. Each item ships
  complete or not at all. A half-removed Valkey = broken boot.
- The candle / tick / order pipeline is LIVE — after every item,
  `cargo build --workspace` + scoped tests must be green.
- Develop on `claude/<descriptive>` branches. Never push to `main`.

## Plan Items (serial PRs)

- [ ] **#O1 — Grafana removal**
  - Drop the `grafana` service from `deploy/docker/docker-compose.yml`;
    delete `deploy/docker/grafana/` (dashboards + provisioning).
  - Remove the Grafana credential fetch (`fetch_grafana_credentials`)
    from `secret_manager.rs` + boot.
  - Remove the Grafana health check + "opened dashboard in browser"
    calls from `crates/app/src/infra.rs`.
  - Delete guard tests `grafana_dashboard_snapshot_filter_guard.rs`,
    `operator_health_dashboard_guard.rs`.
  - Files: `deploy/docker/docker-compose.yml`, `deploy/docker/grafana/**`,
    `crates/core/src/auth/secret_manager.rs`, `crates/app/src/infra.rs`,
    `crates/storage/tests/grafana_dashboard_snapshot_filter_guard.rs`,
    `crates/storage/tests/operator_health_dashboard_guard.rs`
  - Tests: `cargo build --workspace` green; `cargo test -p tickvault-app -p tickvault-storage`

- [ ] **#O2 — Alertmanager removal**
  - Drop the `alertmanager` service + its config from
    `deploy/docker/docker-compose.yml` / `deploy/docker/`.
  - Files: `deploy/docker/docker-compose.yml`, `deploy/docker/alertmanager/**`
  - Tests: compose still `docker compose config`-valid.

- [ ] **#O3 — Prometheus removal**
  - Drop the `prometheus` container from compose; delete `alerts.yml`
    + the Prometheus scrape config.
  - The app's `/metrics` endpoint (port 9091) STAYS — CloudWatch agent
    scrapes it (or the app pushes). Only the container goes.
  - Delete `resilience_sla_alert_guard.rs` + any alert-rule guards.
  - Files: `deploy/docker/docker-compose.yml`,
    `deploy/docker/prometheus/**`,
    `crates/storage/tests/resilience_sla_alert_guard.rs`
  - Tests: `cargo test -p tickvault-storage` green.

- [ ] **#O4 — Valkey removal — HIGHEST RISK, design-first**
  - Valkey is LOAD-BEARING: (a) token cache, (b) dual-instance lock.
  - Token cache: safe to drop — the auth chain is cache → SSM → TOTP;
    SSM is the source of truth, so cache removal is graceful degradation.
  - **Dual-instance lock: needs a replacement design BEFORE coding.**
    The QuestDB `live_instance_lock` table was already dropped (#T4),
    so the lock must move to a file lock, a QuestDB row, or be retired
    with operator sign-off. DO NOT start #O4 until this is decided.
  - Files: `crates/storage/src/valkey_cache.rs`,
    `crates/core/src/instance_lock.rs`,
    `crates/core/src/auth/token_cache.rs` (+ callers),
    `crates/app/src/main.rs` (boot wiring), `deploy/docker/docker-compose.yml`
  - Tests: `cargo build --workspace`; boot must still succeed.

- [ ] **#O5 — guard / rule cleanup**
  - Sweep `.claude/rules/` for stale Grafana / Prometheus / Valkey /
    Alertmanager references; update or retire each rule file.
  - Recompute the `aws-budget.md` memory-budget tables for the
    3-component runtime.
  - Files: `.claude/rules/project/aws-budget.md`,
    `.claude/rules/project/observability-architecture.md`, others as found.

- [ ] **#O6 — docs sweep**
  - Update every doc that describes Grafana / Prometheus / Valkey /
    the frontend as live (`CLAUDE.md`, `docs/PROJECT-SCOPE.md`,
    `docs/phases/*`, `docs/flow-*`, runbooks, `docs/architecture/*`).
  - Retire `docs/phases/block-04-tradingview-terminal.md`.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App boots after #O1–#O3 | QuestDB + app only; `/metrics` still served |
| 2 | App boots after #O4 (Valkey gone) | token loads via SSM; dual-instance lock uses the #O4 replacement |
| 3 | Operator wants a dashboard | CloudWatch console — no local Grafana |
| 4 | An `error!` fires | routed to CloudWatch Logs (Telegram path re-pointed off Alertmanager) |

## Open question for the operator (blocks #O4)

The dual-instance lock currently lives in Valkey. The QuestDB
`live_instance_lock` table was dropped in #T4. Before #O4 starts, the
operator must choose the replacement: (a) OS file lock, (b) a QuestDB
row, or (c) retire the dual-instance guard entirely. #O1–#O3 are
unblocked and can proceed regardless.
