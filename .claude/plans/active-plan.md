# Implementation Plan: Phase B — Monitoring & Dashboard Fixes

**Status:** IN_PROGRESS
**Date:** 2026-04-06
**Approved by:** Parthiban ("everything bro")

## Plan Items

- [x] Add missing Prometheus alert rules (token expiry, daily P&L, disk/memory, zero ticks 30s)
  - Files: deploy/docker/prometheus/rules/dlt-alerts.yml
  - Added: TokenExpiryWarning, TokenExpiryCritical, ZeroTicksBurst, HighMemoryUsage, ValkeyDown, AlloyDown

- [x] Add Valkey redis-exporter sidecar to Docker Compose and uncomment scrape job
  - Files: deploy/docker/docker-compose.yml, deploy/docker/prometheus/prometheus.yml
  - Added: dlt-valkey-exporter service (oliver006/redis_exporter:v1.67.0)

- [x] Fix System Overview dashboard Valkey panel to query correct exporter job
  - Files: deploy/docker/grafana/dashboards/system-overview.json
  - Result: Already queries up{job="dlt-valkey"} which matches new scrape job — no change needed

- [ ] Verify clippy/fmt/tests pass, commit, push
  - Files: all modified files
  - Tests: cargo clippy, cargo fmt

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Docker compose up | All services start including redis-exporter |
| 2 | Prometheus targets page | dlt-valkey shows UP |
| 3 | Grafana System Overview | Valkey shows UP instead of "No data" |
| 4 | Token approaching expiry | TokenExpiryWarning alert fires |
