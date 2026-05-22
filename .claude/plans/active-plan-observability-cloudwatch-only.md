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

- [x] **#O2 — Alertmanager removal** — merged
  - Dropped the `tv-alertmanager` service from
    `deploy/docker/docker-compose.yml`; deleted `deploy/docker/alertmanager/`.
  - Removed the `alerting:` block from `prometheus.yml` and the
    `alertmanager_url` from the opt-in `logs`-profile `loki-config.yml`
    so neither config references the deleted service.
  - Updated `loki_alloy_profile_guard.rs` (also fixed the
    `compose_preserves_*` test left stale by #O1's Grafana removal).
  - Files: `deploy/docker/docker-compose.yml`,
    `deploy/docker/alertmanager/**` (deleted),
    `deploy/docker/prometheus/prometheus.yml`,
    `deploy/docker/loki/loki-config.yml`,
    `crates/common/tests/loki_alloy_profile_guard.rs`
  - Tests: `docker compose config` valid;
    `cargo test -p tickvault-common --test loki_alloy_profile_guard` green.

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

- [ ] **#O4 — Valkey removal — UNBLOCKED (operator decision 2026-05-20)**
  - Valkey is LOAD-BEARING: (a) token cache, (b) dual-instance lock.
  - **Operator decision 2026-05-20 — "Just remove valkey also":** the
    dual-instance lock is **RETIRED**, not replaced. No file lock, no
    QuestDB row. Acceptable because sandbox mode already skips the lock
    and `dry_run = true` is the default — the lock only mattered in
    live multi-host trading. Re-evaluate before `dry_run = false` if
    multi-host deployment is ever on the table (note it in the live-go
    checklist).
  - Token cache: dropped — the auth chain is cache → SSM → TOTP; SSM
    is the source of truth, so removal is graceful degradation.
  - Work: delete `valkey_cache.rs`; delete `instance_lock.rs` + its
    boot wiring + `Resilience01DualInstanceDetected` emit sites;
    rip the Valkey token-cache layer out of `token_cache.rs` so the
    token manager goes straight to SSM; drop the `valkey` service +
    the Valkey credential fetch (`fetch_valkey_password`).
  - Files: `crates/storage/src/valkey_cache.rs` (delete),
    `crates/core/src/instance_lock.rs` (delete),
    `crates/core/src/auth/token_cache.rs` + `token_manager.rs` callers,
    `crates/core/src/auth/secret_manager.rs` (`fetch_valkey_password`),
    `crates/app/src/main.rs` (boot wiring — Step 6a-prime),
    `deploy/docker/docker-compose.yml`,
    instance-lock guard tests under `crates/*/tests/`.
  - Tests: `cargo build --workspace`; boot must still succeed with the
    token manager reading SSM directly.

- [ ] **#O5 — guard / rule / script cleanup**
  - Sweep `.claude/rules/` for stale Grafana / Prometheus / Valkey /
    Alertmanager references; update or retire each rule file.
  - Sweep the operational shell scripts in one consistent pass —
    `scripts/doctor.sh`, `scripts/verify-stack.sh`,
    `scripts/ensure-ready.sh`, `scripts/setup-observability.sh`,
    `scripts/docker-restart.sh` — removing all four removed-service
    references together (#O1–#O4 left these deliberately, since a
    per-PR partial edit while sibling refs linger is itself
    half-finished). Also sweep the `tickvault-logs` MCP server +
    `claude_mcp_endpoints_config_guard.rs` / `claude_session_bootstrap_guard.rs`
    `alertmanager_url` / `TICKVAULT_ALERTMANAGER_URL` references.
  - Recompute the `aws-budget.md` memory-budget tables for the
    3-component runtime.
  - Files: `.claude/rules/project/aws-budget.md`,
    `.claude/rules/project/observability-architecture.md`,
    `scripts/**`, `scripts/mcp-servers/tickvault-logs/server.py`,
    `crates/common/tests/claude_mcp_endpoints_config_guard.rs`,
    `crates/common/tests/claude_session_bootstrap_guard.rs`,
    others as found.

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

## #O4 lock question — RESOLVED 2026-05-20

The dual-instance lock lived in Valkey. Operator decision 2026-05-20
("Just remove valkey also"): **retire the dual-instance guard
entirely** — option (c). No replacement infra. All 6 items #O1–#O6 are
now unblocked; execute serially per §H. Carry one note into the
`dry_run = false` go-live checklist: re-evaluate dual-instance
protection if multi-host deployment is ever considered.
