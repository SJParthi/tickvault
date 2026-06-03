# Implementation Plan: Observability stack → CloudWatch-only

**Status:** VERIFIED
**Date:** 2026-05-20
**Approved by:** Parthiban — verbatim: "except questdb app and cloud
watch we planned to remove everything."
**Completed:** 2026-06-03 — migration verified 100% done in code/files.
#O1–#O4 merged earlier (Grafana/Alertmanager/Prometheus containers + Valkey
removed; SSM dual-instance lock kept). #O5 code verified complete:
`PrometheusConfig` struct + `[prometheus]` `config/base.toml` section +
`alertmanager_url`/`TICKVAULT_ALERTMANAGER_URL` are all DELETED (only
"DELETED in #O5" provenance comments + an absence-asserting
`loki_alloy_profile_guard.rs` test remain). #O6 verified: the
`block-04-tradingview-terminal.md` phase doc is already gone, and a fresh
sweep found NO stale live-service doc — the residual hits are (a) the KEPT
`/metrics` exporter (port 9091, Prometheus-format text, CloudWatch-scraped)
in `CONSOLE.md`/`URLS.md`, (b) `mobile-dashboard-setup.md` describing
external **Grafana Cloud** reading CloudWatch + QuestDB (compatible with
CloudWatch-only — 0 RAM on the box, not the removed local container), and
(c) dated point-in-time audit docs that must be preserved unchanged. The
`.claude/rules/` Grafana/Prometheus/Alertmanager mentions are deliberate
historical/retirement audit trail. Checkboxes ticked to match merged reality;
archived per plan-enforcement.md.

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

- [x] **#O4 — Valkey removal — merged (PR #764, 2026-05-24)**
  - Valkey was LOAD-BEARING: (a) token cache, (b) dual-instance lock.
  - **As shipped (supersedes the 2026-05-20 "retire entirely" note):**
    Valkey itself is fully removed (container, `valkey_cache.rs`,
    `fetch_valkey_password`, `chaos_valkey_kill.rs`, the `[valkey]`
    config, the Valkey token-cache layer). The token cache is now a
    local FILE (`/tmp/tv-token-cache`, crash-recovery only); auth reads
    SSM directly. The dual-instance lock was **MIGRATED to SSM** (not
    retired) — `crates/core/src/instance_lock.rs` is now SSM-backed and
    still load-bearing in boot Step 6a-prime, gated on
    `trading_mode.is_live()` so it is inert under the `dry_run = true`
    default. Operator decision 2026-05-30: KEEP the SSM lock (real
    two-process safety guard, zero Valkey dependency, zero cost in
    dry-run). The `Resilience01DualInstanceDetected` ErrorCode + its
    RESILIENCE-01 runbook stay.
  - Carry into the `dry_run = false` go-live checklist: the SSM lock
    now provides the dual-instance guard; confirm it is exercised
    before flipping live.
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

- [x] **#O5 — guard / rule / script cleanup** — DONE (verified 2026-06-03: PrometheusConfig + [prometheus] + alertmanager_url all deleted in code) (Valkey slice DONE; PrometheusConfig+AM config slice DONE #891; query-tooling+script sweep DONE 2026-05-30; aws-budget memory recompute + triage/verify.sh matched-pair PENDING)
  - **DONE (symmetric query-tooling removal, PR 2026-05-30):** operator-decided
    symmetric full removal — deleted the dead `prometheus_query` (Prometheus
    :9090, removed #O3) + `list_active_alerts` (Alertmanager :9093, removed #O2)
    MCP tools + both `ToolSpec`s from `server.py`; dropped `prometheus_url` from
    all 3 endpoint profiles + the `Profile` struct + `claude_mcp_endpoints_config_guard.rs`;
    dropped `TICKVAULT_PROMETHEUS_URL` from `claude_session_bootstrap_guard.rs`;
    removed the dead Prometheus session-start pull from `session-sanity.sh` + its
    lockstep guard test; removed the 2 dead tools from
    `tickvault_logs_mcp_guard.rs::REQUIRED_TOOL_NAMES`; swept the
    Grafana/Prometheus/Alertmanager container probes from `doctor.sh`,
    `verify-stack.sh`, `mcp-doctor.sh`, `claude-session-bootstrap.sh`,
    `tv-tunnel/doctor.sh` (incl. `--emit-config`); name-swapped every
    `prometheus_query`/`list_active_alerts` ref across the auto-loaded `.claude/rules/`
    charter + 8 rule files + 2 slash commands + CLAUDE.md + 8 docs to
    CloudWatch / `run_doctor` / `questdb_sql`. Kept the `/metrics` exporter
    (port 9091, CloudWatch-scraped) untouched.
  - **DONE (#O5 final closeout, PR 2026-05-30):** (a) `aws-budget.md` memory
    recompute — VERIFIED already 3-component (rule 6: QuestDB + app + OS only;
    Valkey/Prometheus/Grafana/Alertmanager listed as REMOVED); its t4g.medium
    scope is correct for that SUPERSEDED historical doc (authoritative m8g.large
    table lives in daily-universe §7 Mechanical Rule 2) — no edit needed. (b)
    `scripts/triage/verify.sh` retargeted off the retired Prometheus PromQL path
    to an interim `/metrics`-scrape evaluator (`TICKVAULT_METRICS_URL`, default
    `127.0.0.1:9091/metrics`); supports `sum(METRIC)[== N]` / `METRIC[== N]` via
    awk (sum across label series, float-safe compare); unsupported PromQL → exit 2.
    `autonomous_ops_m3_m4_guard.rs` updated in lockstep (usage signature +
    `TICKVAULT_METRICS_URL`). Full CloudWatch GetMetricData version deferred to
    the AWS phase (needs provisioned CloudWatch). **The CloudWatch-only migration
    (#O1–#O6) is now COMPLETE — no dead Prometheus/Alertmanager/Grafana/Valkey
    reference remains in any active code, config, script, or hook path.**
  - **DONE (PR after #889):** removed all residual *Valkey* refs —
    `Makefile` status target, `.github/workflows/chaos-nightly.yml`
    `TV_VALKEY_URL`, `scripts/{verify-stack,smoke-test,ensure-ready,setup-observability}.sh`
    Valkey TCP-waits + `:6379` port hints, `deny.toml` dead `redis`
    skip, `crates/app/src/infra.rs` test port array, and the
    `auto-fix-rotate-token-rollback.sh` header comment. Reworded the
    kept SSM-lock doc-comments (`error_code.rs`, `events.rs`) from
    Valkey→SSM.
  - **PENDING (next PR):** the now-inert `PrometheusConfig` struct +
    `[prometheus]` `config/base.toml` section (+ `config_round_trip.rs`);
    `alertmanager_url` / `TICKVAULT_ALERTMANAGER_URL` in the
    `tickvault-logs` MCP server + `claude_mcp_endpoints_config_guard.rs`
    / `claude_session_bootstrap_guard.rs`; recompute `aws-budget.md`
    memory tables for the 3-component runtime; sweep `.claude/rules/`
    for stale Grafana/Prometheus/Alertmanager references.
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

- [x] **#O6 — docs sweep** — DONE (verified 2026-06-03: block-04 phase doc already gone; no stale live-service doc — see Completed note at top) (Valkey slice DONE 2026-05-30; Grafana/Prometheus slice pending)
  - **DONE (PR after #889):** corrected every stale "live Valkey"
    description across `CLAUDE.md`, `README.md`, `docs/PROJECT-SCOPE.md`,
    `docs/flow-{technical,diagrams}.md`, `docs/phases/{phase-1-live-trading,phase-0-readme}.md`,
    `docs/standards/{data-integrity,quality-gates,failure-scenarios}.md`,
    `docs/runbooks/{backtest-runner,macbook-dies-mid-session}.md`,
    `docs/operator/aws-readiness-audit-2026-05-03.md`,
    `crates/app/src/bin/tv_doctor.rs`, `http/health-checks.http`.
    Accurate-history lines annotated `(removed #O4 2026-05-24)`.
  - **PENDING (next PR):** sweep the same docs for Grafana / Prometheus /
    the frontend described as live; retire
    `docs/phases/block-04-tradingview-terminal.md`.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App boots after #O1–#O3 | QuestDB + app only; `/metrics` still served |
| 2 | App boots after #O4 (Valkey gone) | token loads via SSM; dual-instance lock uses the #O4 replacement |
| 3 | Operator wants a dashboard | CloudWatch console — no local Grafana |
| 4 | An `error!` fires | routed to CloudWatch Logs (Telegram path re-pointed off Alertmanager) |

## #O4 lock question — RESOLVED 2026-05-30 (supersedes the 2026-05-20 note)

The dual-instance lock originally lived in Valkey. The 2026-05-20 note
proposed retiring it entirely with no replacement. What actually shipped
in PR #764 was different and better: Valkey is gone, but the lock was
**migrated to SSM** (`crates/core/src/instance_lock.rs`) rather than
retired. Operator decision 2026-05-30: **KEEP the SSM lock** — it is a
genuine two-process safety guard (prevents two instances racing the 24h
JWT into DH-901), carries no Valkey dependency, and is inert under the
`dry_run = true` default (gated on `trading_mode.is_live()`). The
`Resilience01DualInstanceDetected` ErrorCode + RESILIENCE-01 runbook
stay. #O4 is DONE; remaining serial items per §H are #O5 then #O6.
