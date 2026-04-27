# `tv_realtime_guarantee_score` SLO + `make 100pct-audit` + Operator Dashboard

> Item 13 in detail. The single number, the single command, the single page.

## The single number — `tv_realtime_guarantee_score` (gauge, 0..=100)

```
tv_realtime_guarantee_score = 100 *
  bool(tv_websocket_connections_active{kind="main"} == 5) *
  bool(tv_websocket_connections_active{kind="depth_20"} == 4) *
  bool(tv_websocket_connections_active{kind="depth_200"} == 2) *
  bool(tv_websocket_connections_active{kind="order_update"} == 1) *
  bool(tv_ticks_dropped_total == 0) *
  bool(tv_prev_close_persisted_total{source="CODE6"} >= 3) *
  bool(tv_prev_close_persisted_total{source="QUOTE_CLOSE"} >= 1) *
  bool(tv_prev_close_persisted_total{source="FULL_CLOSE"} >= 1) *
  bool(tv_phase2_outcome_total{result="complete"} >= 1) *
  bool(tv_market_open_depth_anchor_total >= 3) *
  bool(tv_questdb_write_errors_total == 0) *
  bool(tv_telegram_dropped_total == 0)
```

Multiplication semantics — ANY sub-gauge fails (returns 0) → entire score = 0. The dashboard shows WHICH sub-gauge dragged the score down.

## The 12 sub-gauges

| # | Sub-gauge | Wired by item |
|---|-----------|---------------|
| 1 | `main_feed_5_of_5` | Item 5 + existing `health_counter_fix7_guard.rs` |
| 2 | `depth_20_4_of_4` | Item 6 |
| 3 | `depth_200_2_of_2` | Item 6 |
| 4 | `order_update_1_of_1` | Item 6 |
| 5 | `zero_ticks_dropped` | Item 0 (chaos-pinned by `chaos_zero_tick_loss.rs`) |
| 6 | `prev_close_idx_i_present` | Item 4 |
| 7 | `prev_close_nse_eq_present` | Item 4 |
| 8 | `prev_close_nse_fno_present` | Item 4 |
| 9 | `phase2_complete_today` | Item 1 |
| 10 | `depth_anchor_3_of_3` | existing `MarketOpenDepthAnchor` |
| 11 | `questdb_zero_write_errors` | Item 0 + Item 9 |
| 12 | `telegram_zero_drops` | Item 11 |

## The single command — `make 100pct-audit`

| Mode | When | Command | Source | Exit code |
|---|---|---|---|---|
| Runtime | Operator daily, post-market | `make 100pct-audit` | Live Prometheus query | 0 if score == 100 with all sub-gauges green; non-zero with table report otherwise |
| CI | Pre-merge on every PR | `make 100pct-audit-ci` | `data/snapshots/last-green.json` (updated only on `main` post-merge by green build) | 0 if last-green snapshot score == 100; fail otherwise |

### `scripts/100pct-audit.sh` skeleton

```bash
#!/usr/bin/env bash
set -euo pipefail
MODE="${1:-runtime}"  # runtime | ci

if [ "$MODE" = "runtime" ]; then
    PROM_URL="${TICKVAULT_PROMETHEUS_URL:-http://localhost:9090}"
    SCORE=$(curl -s "$PROM_URL/api/v1/query?query=tv_realtime_guarantee_score" \
            | jq -r '.data.result[0].value[1] // "0"')
    SUB_QUERIES=(
        'tv_websocket_connections_active{kind="main"}'
        'tv_websocket_connections_active{kind="depth_20"}'
        # ... 10 more
    )
    # build table report
    echo "Score: $SCORE / 100"
    # sub-table per sub-gauge with PASS/FAIL
    [ "$SCORE" = "100" ] && exit 0 || exit 1
elif [ "$MODE" = "ci" ]; then
    SNAPSHOT="data/snapshots/last-green.json"
    [ -f "$SNAPSHOT" ] || { echo "no green snapshot yet"; exit 0; }
    SCORE=$(jq -r '.score' "$SNAPSHOT")
    [ "$SCORE" = "100" ] && exit 0 || exit 1
fi
```

### Snapshot updater (post-merge CI)

```bash
# .github/workflows/snapshot-update.yml
on:
  push:
    branches: [main]
jobs:
  snapshot:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          # boot tickvault, run `make 100pct-audit`, capture score
          # if 100, write data/snapshots/last-green.json with sha + timestamp + score
          # commit + push to main
```

## The single page — `100pct.json` Grafana dashboard

| Panel | Type | Query | Visual |
|---|---|---|---|
| 1 (top, large) | Stat | `tv_realtime_guarantee_score` | Big number; green at 100, red at < 100 |
| 2..13 (12 sub-stats) | Stat | each sub-gauge expression | Small; green/red icon per sub |
| 14 (bottom, full-width) | Time series | `tv_realtime_guarantee_score` over 24h | Line graph showing score over time |

Dashboard pinned by `crates/storage/tests/operator_100pct_dashboard_guard.rs` snapshot test (similar to existing `operator_health_dashboard_guard.rs`).

## Tests

| Test | File | Asserts |
|---|---|---|
| `test_score_is_100_when_all_subgauges_pass` | `crates/app/tests/realtime_score.rs` | All 12 sub-gauges green → score = 100 |
| `test_score_drops_to_0_when_any_subgauge_fails` | same | Any sub-gauge red → score = 0 (multiplication) |
| `test_100pct_audit_exits_0_on_runtime_pass` | `crates/app/tests/audit_script.rs` | Shell script exits 0 |
| `test_100pct_audit_exits_nonzero_with_failing_subgauge` | same | Exits non-zero with which-sub-failed report |
| `test_100pct_audit_ci_reads_snapshot` | same | CI mode reads from `data/snapshots/last-green.json` |
| `test_100pct_audit_ci_passes_when_no_snapshot` | same | First-ever run with no snapshot → exit 0 (don't block initial PRs) |
| `test_grafana_100pct_dashboard_pinned` | `crates/storage/tests/operator_100pct_dashboard_guard.rs` | JSON snapshot match |
| `test_score_alert_fires_below_100_for_60s` | `crates/storage/tests/realtime_score_alert_guard.rs` | Prometheus alert rule pinned |

## ErrorCode + Telegram + audit

| Field | Value |
|---|---|
| ErrorCode | `SLO-01` (score dropped below 100), `SLO-02` (snapshot stale > 24h on main) |
| Typed event | `RealtimeGuaranteeScoreDropped{score, failing_dimensions: Vec<String>}` (Severity::High) |
| Triage YAML | rules for `SLO-01/02` |
| Audit | every score change in `selftest_audit` table (Item 9) — composite key `(trading_date_ist, "score_change")` |

## Operator routine after Wave 3 ships

| When | Command | Expected |
|---|---|---|
| Post-market 15:35 IST daily | `make 100pct-audit` | exit 0, score = 100 |
| Pre-market 08:45 IST daily | `make doctor` | all 7 sections PASS |
| On any Telegram CRITICAL | follow runbook from `mcp__tickvault-logs__find_runbook_for_code` | resolved in ≤ 30 min |
| Weekly | review `audit-trails.json` for unexpected patterns | manual |
| Monthly | review S3 lifecycle progression in AWS console | costs ≤ ₹333/mo for cold storage |

## What this gives the operator (one paragraph)

After Wave 3 merges, the operator answers "is everything OK?" by glancing at one Grafana panel showing one number. If it's 100, every guarantee in the matrix is currently true. If it's not, the dashboard shows which sub-gauge failed; the SLO-01 Telegram alert points at the runbook; the auto-triage either fixes it or escalates with full context. No grep, no `docker logs`, no manual SQL — the whole observability stack converges to a single `bool`.
