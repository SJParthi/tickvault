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

- [x] **#O1 — Grafana removal** — merged
  - The heavy parts (the `tv-grafana` compose service, the
    `deploy/docker/grafana/` provisioning tree, `fetch_grafana_credentials`,
    the two `grafana_dashboard_snapshot_filter_guard.rs` /
    `operator_health_dashboard_guard.rs` guard tests, and the
    `DASHBOARD_SERVICES` Grafana entry) were already deleted by
    earlier AWS-lifecycle PRs.
  - This PR cleaned the residue: `scripts/grafana-watch.sh` deleted;
    Makefile `grafana` / `grafana-reload` / `grafana-watch` targets dropped;
    `tv-grafana` references purged from `doctor.sh`, `verify-stack.sh`,
    `docker-restart.sh`, `validate-automation.sh`, `bootstrap.sh`,
    `provision-infra-secrets.sh`, `smoke-test.sh`, `setup-secrets.sh`,
    `100pct-audit.sh`, `tv-tunnel/doctor.sh`,
    `scripts/claude-session-bootstrap.sh`.
  - The MCP server `tool_grafana_query` + `ToolSpec` registration were
    dropped from `scripts/mcp-servers/tickvault-logs/server.py`; the
    `grafana_url` profile key was dropped from
    `config/claude-mcp-endpoints.toml` (all 3 profiles) and from
    `crates/common/tests/claude_mcp_endpoints_config_guard.rs`
    (struct field, REQUIRED_URL_KEYS, source-scan assertions).
  - The `TICKVAULT_GRAF_STATUS` / `TICKVAULT_GRAFANA_URL` env-var
    exports were dropped from the session-bootstrap hook; reachable
    count now `/4` (was `/5`).

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

- [x] **#O3 — Prometheus removal** — merged
  - Dropped the `tv-prometheus` service + `tv-prometheus-data` volume
    from compose; deleted `deploy/docker/prometheus/` (prometheus.yml +
    rules/tickvault-alerts.yml).
  - The app's `/metrics` endpoint (port 9091) STAYS — the CloudWatch
    agent scrapes it. Only the container is gone.
  - `infra.rs`: removed Prometheus from `DASHBOARD_SERVICES`, dropped
    `PROMETHEUS_PORT` + the `wait_for_service_healthy("Prometheus", …)`
    boot wait; updated `test_dashboard_services_count` → 2.
  - Deleted alert-rule guards `resilience_sla_alert_guard.rs` and
    `recording_rules_guard.rs`; trimmed `zero_tick_loss_alert_guard.rs`
    to its still-valid source-scan tests (metric emissions still
    matter — CloudWatch scrapes them); removed the
    `prometheus_alerts_include_depth_sequence_rules` test.
  - Updated `loki_alloy_profile_guard.rs::compose_preserves_default_services`
    → 2 services (questdb, valkey).
  - Deferred to #O5: the unused `PrometheusConfig` struct + `[prometheus]`
    `config/base.toml` section (inert config, no boot impact).
  - Files: `deploy/docker/docker-compose.yml`,
    `deploy/docker/prometheus/**` (deleted), `crates/app/src/infra.rs`,
    `crates/storage/tests/resilience_sla_alert_guard.rs` (deleted),
    `crates/storage/tests/zero_tick_loss_alert_guard.rs`,
    `crates/common/tests/recording_rules_guard.rs` (deleted),
    `crates/common/tests/claude_session_bootstrap_guard.rs`,
    `crates/common/tests/loki_alloy_profile_guard.rs`
  - Tests: `docker compose config` valid; `cargo test -p tickvault-app
    -p tickvault-storage -p tickvault-common` (scoped) green.

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
  - Remove the now-inert `PrometheusConfig` struct + the `[prometheus]`
    `config/base.toml` section (left in place by #O3 — it no longer
    connects to anything; `config_round_trip.rs` updates with it).
    Keep `ObservabilityConfig` — the `/metrics` exporter on port 9091
    stays for the CloudWatch agent.
  - Recompute the `aws-budget.md` memory-budget tables for the
    3-component runtime.
  - Files: `.claude/rules/project/aws-budget.md`,
    `.claude/rules/project/observability-architecture.md`,
    `scripts/**`, `scripts/mcp-servers/tickvault-logs/server.py`,
    `crates/common/src/config.rs`, `config/base.toml`,
    `crates/common/tests/config_round_trip.rs`,
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
