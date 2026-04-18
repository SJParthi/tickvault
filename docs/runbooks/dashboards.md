# Dashboards index

Five Grafana dashboards ship with tickvault, auto-provisioned from
`deploy/docker/grafana/dashboards/`. Each has a stable UID so bookmarks
and alert links don't break when dashboard JSON is edited.

Open Grafana at http://localhost:3000 (or whatever the operator's
deployment exposes) and jump to any dashboard via its UID URL:
`http://localhost:3000/d/<uid>`.

## Morning-ops routine

1. **Start with `tv-operator-health`** — the one-glance green/red
   board. If every panel is green, nothing needs attention.
2. **Any red panel?** Follow the panel's description to the drill-down
   dashboard below.

## The five dashboards

| UID | Title | Refresh | Use when |
|---|---|---|---|
| `tv-operator-health` | Operator Health | 30s | Morning glance — is the system alive? |
| `tv-trading-flow` | Trading Flow | 15s | Orders / P&L / risk / dry-run state |
| `tv-depth-flow` | Depth Flow | 30s | 20+200-level depth subscription health |
| `tv-auth-health` | Auth Health | 30s | Token / circuit / IP / 401s |
| `tv-data-lifecycle` | Data Lifecycle | 1m | Universe / persistence / candles / disk |

## Drill-down map

```
tv-operator-health (14 panels, one-glance)
├── App down? / QuestDB down? / WS pool degraded?
│     └─> investigate via docker-compose ps + docker logs
├── Ticks/min = 0?
│     └─> tv-depth-flow (depth feed sanity)
│     └─> tv-data-lifecycle (universe + persistence)
├── Circuit breaker OPEN?
│     └─> tv-trading-flow (P&L + rate limiter + OMS state)
├── Token remaining < 1h?
│     └─> tv-auth-health (renewal + TOTP + IP)
└── Ticks lost > 0 OR spilled > 0?
      └─> tv-data-lifecycle (ILP flush errors, disk, MV lag)
```

## Pinned invariants (mechanical guards)

| Dashboard | Guard test | What it pins |
|---|---|---|
| `tv-operator-health` | `operator_health_dashboard_guard` | UID stable, timezone IST, 14 panels present, zero-tick-loss chain covered |
| all 5 | `grafana_dashboard_snapshot_filter_guard` | No `count_distinct(security_id)` on cross-segment tables without segment qualifier |

The other 3 dashboards (`trading-flow`, `depth-flow`, `auth-health`,
`data-lifecycle`) don't have dedicated pin tests because they're
drill-downs — if a panel drifts, `tv-operator-health` still covers
the morning check. Add a pin guard if a specific panel becomes a
critical dependency.

## Adding a new dashboard

1. Write `deploy/docker/grafana/dashboards/<name>.json` following the
   same structure as `operator-health.json`.
2. Use a stable UID prefixed `tv-` (becomes the URL path).
3. `timezone: "Asia/Kolkata"` — everyone in ops reads IST.
4. Use the Prometheus `uid: prometheus` datasource.
5. Run `cargo test -p tickvault-storage --test grafana_dashboard_snapshot_filter_guard`
   — it will enforce the `security_id`+`segment` rule automatically.
6. Add a row to the table above.

## Deprecation rule

Grafana auto-provisions from the directory — deleting the JSON
removes the dashboard at next restart. Before deleting, confirm no
Telegram alerts, runbook links, or operator bookmarks reference its
UID.
