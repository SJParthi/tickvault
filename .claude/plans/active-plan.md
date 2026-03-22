# Implementation Plan: IST Timezone Consistency + Logging Pipeline Fix

**Status:** VERIFIED
**Date:** 2026-03-22
**Approved by:** Parthiban ("Yes go ahead with the plan")

## Context

1. Market-data dashboard uses `"timezone": "utc"` while all 4 other dashboards use `"timezone": "Asia/Kolkata"`
2. QuestDB timestamps stored as IST-as-UTC (IST values labeled as UTC) — prevents consistent timezone display
3. Loki logging pipeline shows "No data" — Alloy -> Loki pipeline not delivering
4. Loki healthcheck only checks binary exists, not readiness
5. System-overview dashboard missing Alloy health panel
6. Loki Ingestion metric may be wrong for single-binary mode

## Plan Items

- [x] Item 1: Fix Grafana market-data dashboard timezone + SQL queries
  - Files: deploy/docker/grafana/dashboards/market-data.json
  - Changes: timezone utc->Asia/Kolkata, dateadd('s',19800,...) in WHERE clauses, to_str() for display timestamps
  - Tests: N/A (JSON config)

- [x] Item 2: Set Grafana server default timezone to IST
  - Files: deploy/docker/docker-compose.yml
  - Changes: Added GF_DATE_FORMATS_DEFAULT_TIMEZONE: "Asia/Kolkata"
  - Tests: N/A (Docker config)

- [x] Item 3: Fix Loki healthcheck to verify readiness
  - Files: deploy/docker/docker-compose.yml
  - Changes: loki --version -> wget /ready endpoint, Grafana depends_on service_healthy
  - Tests: N/A (Docker config)

- [x] Item 4: Add Alloy+Valkey health panels + fix Loki Ingestion metric
  - Files: deploy/docker/grafana/dashboards/system-overview.json
  - Changes: 8 health panels (was 6), Loki metric added loki_request_duration fallback
  - Tests: N/A (JSON config)

- [x] Item 5: Verify depth WebSocket parser byte mappings (audit only)
  - Files: crates/core/src/parser/deep_depth.rs, crates/common/src/constants.rs
  - Result: PERFECT MATCH — zero mismatches across all 24 checked values

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB console shows UTC timestamp | Correct — UTC is standard for DB |
| 2 | Grafana shows IST for all 5 dashboards | Correct — Asia/Kolkata converts UTC to IST |
| 3 | Exchange time 15:30 IST stored as UTC 10:00 | Grafana IST shows 15:30 |
| 4 | received_at from Utc::now() stored as UTC | Grafana IST shows correct IST |
| 5 | Loki receives container logs from Alloy | Log Volume panel shows data |
| 6 | Alloy status visible in system-overview | UP/DOWN stat panel present |
| 7 | Existing dev data in QuestDB off by +5:30 | Acceptable — dev data only |
