# Implementation Plan: Comprehensive Resilience — Zero Human Intervention

**Status:** VERIFIED
**Date:** 2026-03-22
**Approved by:** Parthiban ("yes go ahead dude")

## Vision

The system must survive ANY crash scenario — Docker daemon crash, QuestDB crash, WebSocket disconnect, app crash, network outage — and automatically recover without human intervention. Every missed tick must be detected and backfilled. Every failure must be alerted. Every recovery must be verified.

## Current State (What We Have)

- Docker `restart: unless-stopped` on all 8 services
- WebSocket exponential backoff reconnection (500ms -> 30s, 10 attempts)
- Order WS market-hours-aware reconnection
- QuestDB reconnection (3 retries, 7s total)
- Token cache survives process crashes (fast boot ~2ms)
- Instrument rkyv cache (~5ms load)
- Tick gap detection (30s warn, 120s error, Telegram alert)
- 20+ Prometheus alert rules, 30+ Telegram notification events
- Historical backfill post-market with 5 retry waves
- SPSC 65K tick buffer between WS and processor
- Graceful 2-phase shutdown (market close + full exit)

## Gaps Identified (What's Missing)

### CRITICAL

1. **WebSocket gives up after 10 attempts** — system goes permanently dark
2. **App runs on host with no process supervisor** — crash = manual restart needed
3. **No mid-session historical backfill after crash recovery** — gaps persist until market close

### HIGH

4. **Ticks dropped when QuestDB is down** — after 3 retries (~7s), data lost forever
5. **No container restart alerting** — a service crashes and restarts silently
6. **No automatic gap-fill after WebSocket reconnect** — known gap, no automated fix
7. **Grafana alert rules not routing to Telegram** — alerts visible only in Grafana UI

### MEDIUM

8. **Token cache on /tmp** — Linux tmpfs wipes on reboot, forces full SSM auth chain
9. **No data integrity verification after recovery** — silent holes in QuestDB

## Plan Items

### Phase A: Never Give Up (WebSocket + App Supervision)

- [x] A1: Remove WebSocket max reconnection limit — retry forever during market hours
  - Files: crates/core/src/websocket/connection.rs, crates/common/src/config.rs, config/base.toml
  - Change: During market hours (09:00-16:00 IST), never stop retrying. Cap at 60s backoff.
    After market close, respect max_attempts limit. Add Telegram CRITICAL alert after
    10 consecutive failures (but keep retrying). Same for Order Update WS.
  - Tests: test_reconnect_infinite_during_market_hours, test_reconnect_respects_limit_after_hours

- [ ] A2: Mid-session gap backfill after WebSocket reconnect
  - Files: crates/core/src/websocket/connection.rs, crates/core/src/historical/candle_fetcher.rs, crates/app/src/main.rs
  - Change: After successful reconnect during market hours, spawn background task to fetch
    1-minute candles from Dhan Historical API for the gap period (disconnect_time to now).
    Store as historical_candles. Dedup prevents duplicates if ticks also arrived.
    Throttle: max 1 gap-fill per 5 minutes to avoid API rate limits.
  - Tests: test_gap_backfill_triggered_on_reconnect, test_gap_backfill_throttled

- [x] A3: App process supervision via systemd (Linux) / launchd (macOS)
  - Files: deploy/systemd/dlt-app.service (new), scripts/install-service.sh (new)
  - Change: systemd unit file with Restart=always, WatchdogSec=30, auto-start on boot.
    App emits sd_notify WATCHDOG=1 every 15s (via sd-notify crate or raw socket).
    If app hangs (no watchdog ping for 30s), systemd kills and restarts it.
  - Tests: N/A (system-level config, verified via `systemctl status`)

### Phase B: Never Lose a Tick (Buffering + Recovery)

- [x] B1: In-memory tick ring buffer for QuestDB outages
  - Files: crates/storage/src/tick_persistence.rs
  - Change: When QuestDB write fails, buffer ticks in a bounded ring buffer
    (capacity: 300,000 ticks = ~5 minutes at 1000 ticks/sec = ~20MB).
    On QuestDB recovery, drain buffer first (oldest-first) before accepting new ticks.
    If buffer full, drop oldest ticks (log ERROR + Telegram alert with count).
    Metric: dlt_tick_buffer_size gauge, dlt_ticks_dropped_total counter.
  - Tests: test_tick_buffer_fills_on_questdb_down, test_tick_buffer_drains_on_recovery,
    test_tick_buffer_drops_oldest_when_full

- [x] B2: Automatic data integrity check after any recovery
  - Files: crates/storage/src/tick_persistence.rs, crates/app/src/main.rs
  - Change: After QuestDB reconnect, run quick gap-check query:
    `SELECT count() FROM ticks WHERE ts >= dateadd('m', -30, now()) SAMPLE BY 1m`
    If any 1-minute bucket has 0 ticks during market hours, trigger mid-session
    gap backfill (from A2) for affected securities.
  - Tests: test_gap_check_detects_missing_minutes, test_gap_check_skips_off_hours

### Phase C: Never Miss an Alert (Container + Grafana -> Telegram)

- [x] C1: Container restart alerting via Prometheus
  - Files: deploy/docker/prometheus/rules/dlt-alerts.yml
  - Change: Add alert rule:
    `ContainerRestarted: increase(container_start_time_seconds[5m]) > 0`
    Requires cAdvisor or Docker metrics exporter. Alternative: use Alloy to detect
    container restart events and push to Loki, then Grafana alert rule on log pattern.
    Simpler: use Prometheus alert on `changes(up{job=~"dlt-.*"}[5m]) > 1` — if a target
    flaps UP/DOWN within 5 minutes, it restarted.
  - Tests: N/A (Prometheus config)

- [x] C2: Grafana alerting -> Telegram contact point (already implemented in setup-observability.sh)
  - Files: deploy/docker/grafana/provisioning/alerting/alerts.yml
  - Change: Add Telegram contact point to Grafana alerting provisioning.
    Configure notification policy to route all alerts to Telegram.
    This enables Grafana-managed alerts (from dashboard panels) to fire Telegram messages
    even when the Rust app is not running (infrastructure-level alerts).
  - Tests: N/A (YAML config, verified via Grafana UI)

- [x] C3: Docker service restart count in system-overview dashboard
  - Files: deploy/docker/grafana/dashboards/system-overview.json
  - Change: Add panel showing `changes(up[1h])` per service — visualizes how many times
    each service restarted in the last hour. Red if > 0. Also add uptime panel.
  - Tests: N/A (JSON config)

### Phase D: Hardened Recovery (Token + Config)

- [x] D1: Move token cache from /tmp to persistent data directory
  - Files: crates/core/src/auth/token_cache.rs, crates/common/src/config.rs
  - Change: Token cache path from `/tmp/dlt-token-cache` to `{data_dir}/cache/dlt-token-cache`
    where data_dir is configurable (default: `./data`). Survives reboot on real filesystem.
    Validate cache dir is NOT on tmpfs (use existing I-P0-04 validate_cache_persistence).
  - Tests: test_token_cache_path_configurable, test_token_cache_not_on_tmpfs

## Priority Order

1. **A1** (infinite reconnect) — prevents going dark permanently
2. **B1** (tick buffer) — prevents data loss during QuestDB outage
3. **A2** (mid-session backfill) — fills gaps automatically
4. **C2** (Grafana -> Telegram) — ensures alerts reach you even if app is down
5. **A3** (systemd supervisor) — auto-restarts app on crash
6. **C1** (container restart alerts) — visibility into silent restarts
7. **B2** (integrity check) — catches silent gaps
8. **C3** (restart dashboard) — visual monitoring
9. **D1** (token cache) — faster cold starts

## Crash Scenarios After Implementation

| Scenario | Detection | Recovery | Data Impact |
|----------|-----------|----------|-------------|
| Docker daemon crash | Prometheus TargetDown (1min) + Telegram | Docker auto-restarts all services, app systemd restarts, infinite WS reconnect | Ticks buffered in memory up to 5min, gap-fill for longer outages |
| QuestDB crash | Storage error metric + Telegram | QuestDB auto-restart (Docker), tick buffer holds data, drain on recovery | Zero tick loss for outages < 5 min |
| WebSocket disconnect | WS reconnect metric + Telegram | Infinite reconnect during market hours, gap-fill after reconnect | Gap filled from Historical API within minutes |
| App crash | systemd watchdog (30s) + Telegram | systemd auto-restart, fast boot (2ms token + 5ms cache), WS reconnect | Gap-fill covers crash period |
| Network outage | WS timeout (40s) + Telegram | WS reconnect when network returns, gap-fill | Gap filled automatically |
| Token expires mid-session | Disconnect code 807 + Telegram | Auto-renew (arc-swap), WS reconnects with new token | Sub-second, zero impact |
| Grafana crash | Docker restart + Telegram (via Prometheus) | Auto-restart, provisioned dashboards auto-load | No data impact (display only) |
| Full machine reboot | systemd auto-start + Telegram | All services start in order, fast boot if token cached | Gap-fill covers reboot period |

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | WS disconnects at attempt 11 during market hours | Keeps retrying (no limit during market hours) |
| 2 | QuestDB down for 3 minutes | Ring buffer holds ~180K ticks, drains on recovery |
| 3 | QuestDB down for 10 minutes | Buffer full at 5min, drops oldest 5min, keeps newest |
| 4 | WS reconnects after 2-minute outage | Gap-fill fetches 1min candles for the 2-minute period |
| 5 | App crashes during market hours | systemd restarts in <5s, fast boot, WS reconnects, gap-fill |
| 6 | Grafana alert fires (WebSocket down) | Telegram message sent even though Rust app is down |
| 7 | Container restarts silently | Prometheus alert fires, visible in dashboard |
