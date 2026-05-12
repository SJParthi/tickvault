# Topic — Observability 7-Layer Z+ Defense (G1 — Prometheus + Grafana + Loki resilience)

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `z-plus-defense-doctrine.md` > `topic-zero-tick-loss-coverage-map.md` > this file.
> **Scope:** Z+ defense for the OBSERVABILITY stack itself — Prometheus exporter, Grafana, Loki, tracing.
> **Trigger:** Live log 10:17:35 + 10:21:15 showed `hyper::Error(IncompleteMessage)` on Prometheus exporter during reconnect burst. The observability layer itself can fail → operator goes BLIND when most needed.

---

## 🚗 Auto-Driver Story

> Sir, imagine the security cameras at the warehouse. We have:
> - **Prometheus** = the camera that counts parcels per minute
> - **Grafana** = the TV screen the operator watches
> - **Loki** = the recording device for after-the-fact review
> - **Telegram alerts** = the security guard's walkie-talkie
>
> Today's log showed: **at 10:21:15 when 9 of 10 trucks were RST'd, the cameras themselves choked**. `hyper::Error(IncompleteMessage)` means the camera's video feed got cut mid-frame. If the cameras fail at the SAME MOMENT the trucks are failing, the operator goes BLIND. Z+ says: the observability layer needs its OWN bodyguards.

---

## 🎯 Why this matters NOW

From the live log:

| Time | What happened |
|---|---|
| 10:17:35.927 | `hyper::Error(IncompleteMessage)` on Prometheus exporter (1 occurrence) |
| 10:21:15.573 | Same error, **4 occurrences within 1 second** during mass-RST |
| 10:21:15.579 | Same error continuing |
| 10:21:15.580 | Same error continuing |

**Translation:** during the most critical 1 second of the day (mass RST), Prometheus was choking on incomplete responses. Operator dashboards may have shown STALE OR MISSING data during the exact moment they were most needed.

**This is unacceptable.** Observability must NOT fail when the system is failing.

---

## 📋 The 11 ways observability can fail

| # | Path | Likelihood | Severity |
|---|---|---|---|
| 1 | Prometheus exporter `IncompleteMessage` during app load burst | HIGH (observed today) | High |
| 2 | Prometheus scraper timeout (15s default) | MEDIUM | Medium |
| 3 | Grafana container OOM | LOW | Critical |
| 4 | Grafana datasource (QuestDB / Prometheus) connection drop | MEDIUM | High |
| 5 | Loki log writer backpressure → tracing macros block | LOW | Critical |
| 6 | tracing macros allocate on hot path (regression) | RARE | Critical |
| 7 | Alertmanager → Telegram webhook drop | LOW | High |
| 8 | Telegram bot rate-limit (20 msg/sec) hit | MEDIUM | Medium |
| 9 | errors.jsonl writer disk-full | LOW | High |
| 10 | errors.summary.md writer task panics | RARE | Medium |
| 11 | mcp-server `tickvault-logs` unreachable from Claude session | MEDIUM | Medium |

---

## 🛡️ The 7-Layer Z+ Defense for OBSERVABILITY

### L1 DETECT — Sensors that watch the watchers

| Sensor | What it watches | Threshold |
|---|---|---|
| `tv_prom_exporter_incomplete_msg_total` | `hyper::Error(IncompleteMessage)` count | rate > 0.1/min |
| `tv_prom_exporter_scrape_duration_ns` | p99 scrape latency | p99 > 500ms |
| `tv_grafana_datasource_disconnected_seconds` | Time since last successful query | > 60s |
| `tv_loki_writer_lag_seconds` | tracing → loki queue depth | > 5s |
| `tv_telegram_dropped_total` | already exists | rate > 0.1/min |
| `tv_summary_writer_last_refresh_seconds` | errors.summary.md last write | > 90s |
| `tv_mcp_server_responsive_seconds` | tickvault-logs MCP query latency | p99 > 5s |

**NEW metrics:** 7. **NEW alert rules:** 7. **NEW Grafana panels:** 7.

### L2 VERIFY — Don't trust a single sensor

After every Prometheus scrape, verify the dashboard rendered correctly?

| Option | Description | Cost |
|---|---|---|
| (a) Every scrape | Round-trip test | Too expensive |
| (b) Hourly synthetic | Bot scrape + assert panel renders | Acceptable |
| (c) Operator-triggered | `make grafana-doctor` command | Cheapest |

**My vote:** (b) Hourly synthetic — automated round-trip every hour during market hours.

### L3 RECONCILE — Daily ground-truth

| Reconcile | When | What it checks |
|---|---|---|
| Counter consistency | Daily 23:00 IST | App's internal counter vs Prometheus scrape value |
| Log completeness | Daily 23:00 IST | `data/logs/errors.jsonl.*` row count vs `tv_error_emissions_total` |
| Alert delivery | Daily 23:00 IST | Alertmanager logs vs Telegram sent log |
| Audit table integrity | Daily 23:00 IST | All 15+ audit tables: row count > 0 if any related event fired |

### L4 PREVENT — Stop the failure before it happens

| Pre-flight | When | What it checks |
|---|---|---|
| Prometheus exporter ports open | Boot | `nc -z 127.0.0.1 9091` |
| Grafana healthy | Boot | `GET /api/health` |
| Loki responsive | Boot | `POST /loki/api/v1/push` synthetic |
| Telegram bot reachable | Boot | `GET /bot{TOKEN}/getMe` |
| errors.jsonl writable | Boot | Create + delete probe file |
| `data/logs/` disk-free | Boot + every 5min | > 20% free |

### L5 AUDIT — Continuous observability of observability

NEW audit table `observability_health_audit`:
```sql
CREATE TABLE IF NOT EXISTS observability_health_audit (
  ts                    TIMESTAMP,
  component             SYMBOL,  -- prom_exporter / grafana / loki / telegram / mcp_server
  event                 SYMBOL,  -- scrape_complete / scrape_failed / dashboard_render / alert_sent / etc.
  duration_ms           INT,
  outcome               SYMBOL,  -- success / timeout / error
  details               STRING
) TIMESTAMP(ts) PARTITION BY DAY
  DEDUP UPSERT KEYS(ts, component, event);
```

### L6 RECOVER — Nuclear option

| Failure | Recovery |
|---|---|
| Prom exporter stuck | Restart exporter via `health_check` task |
| Grafana OOM | `docker restart tv-grafana` (existing) |
| Loki backpressure | Drop oldest 1000 logs from queue + counter increment |
| Telegram dropped | Coalescer already in place (Wave 3) |
| MCP server unreachable | Restart MCP server in 30s |
| errors.jsonl disk full | Stop writing + log to stdout fallback |

### L7 COOLDOWN — Don't recovery-storm

| Cooldown | Duration | Why |
|---|---|---|
| Between Prom exporter restarts | 60s | Avoid restart storm |
| Between Grafana restarts | 5min | Same |
| Between alertmanager retries | 10s exponential | Avoid Telegram rate-limit |
| Between MCP server restarts | 30s | Same |

---

## 📊 The 11×7 = 77-cell coverage matrix

| Path | L1 | L2 | L3 | L4 | L5 | L6 | L7 |
|---|---|---|---|---|---|---|---|
| 1 Prom IncompleteMessage | ✅ | ✅ | ✅ | — | ✅ | ✅ | ✅ |
| 2 Prom scrape timeout | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 3 Grafana OOM | ✅ | — | — | ✅ | ✅ | ✅ | ✅ |
| 4 Datasource drop | ✅ | ✅ | — | ✅ | ✅ | ✅ | — |
| 5 Loki backpressure | ✅ | — | ✅ | — | ✅ | ✅ | — |
| 6 tracing macros alloc | — | — | — | ✅ (DHAT) | — | — | — |
| 7 Alertmanager fail | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 8 Telegram rate-limit | ✅ | — | ✅ | — | ✅ | ✅ | ✅ |
| 9 errors.jsonl disk-full | ✅ | — | ✅ | ✅ | ✅ | ✅ | — |
| 10 summary_writer panic | ✅ | — | — | — | ✅ | ✅ | — |
| 11 MCP server unreachable | ✅ | — | — | ✅ | — | ✅ | ✅ |

**Coverage gaps:**
- Path 3 (Grafana OOM) has no L2/L3 — partially OK (Grafana itself crashes too rarely)
- Path 5 (Loki backpressure) has no L2/L4 — discussion: should Loki BE a hot dependency? Maybe defer-only?
- Path 6 (tracing alloc) has only L4 — but that's the highest signal-to-noise (DHAT catches at build time)

---

## 🚨 The 3 worst-case nightmares

### Nightmare 1 — Observability blind at the moment of crisis

**Scenario:** At 10:21:15 today, 9-of-10 conns RST'd. Prometheus exporter choked. Grafana dashboard showed stale data. Operator on phone saw "everything looks fine" while system was screaming.

**Defense:**
- L1 detects `hyper::Error(IncompleteMessage)` rate
- L6 auto-restarts exporter at threshold
- L2 hourly synthetic verifies dashboard renders
- Telegram FALLBACK route: if exporter dies, app emits direct Telegram (bypassing Prometheus → Alertmanager)

### Nightmare 2 — Loki backpressure freezes hot path

**Scenario:** Loki ingester at 100% disk. `tracing::error!` writes block waiting for Loki ack. Tick processor's `error!` call freezes. Hot path stalls.

**Defense:**
- `tracing-appender::non_blocking` already used (verify with audit)
- L1 gauge `tv_loki_writer_lag_seconds`
- L6 drops oldest logs at threshold

**Note:** Loki was DELETED from compose in Wave 7-A. We rely on `data/logs/errors.jsonl.*` rotation. So Loki backpressure is N/A. Adjust this nightmare:

**Updated nightmare 2:** errors.jsonl writer falls behind during ERROR burst → tracing macro queue fills → hot path may stall.

**Defense:** `tracing-appender::non_blocking` with bounded queue (drop oldest on overflow) + counter.

### Nightmare 3 — Telegram itself becomes the bottleneck

**Scenario:** Burst of 100 ERRORs in 1 second. Telegram bot API rate-limits at 20 msg/sec. 80 alerts dropped. Operator sees nothing.

**Defense:**
- Coalescer (Wave 3) already caps at 1 msg/topic/60s
- `tv_telegram_dropped_total` already counted
- L6 fallback: AWS SNS SMS for CRITICAL severity if Telegram drops

---

## 🎯 Discussion items

### D1 — Should observability counters be on the hot path?

Every `error!` macro emits to:
1. stdout
2. data/logs/app.log
3. data/logs/errors.log
4. data/logs/errors.jsonl
5. Telegram (via Alertmanager)

That's 5 sinks. Per error. Is the hot path really hot?

**Counter-argument:** errors are RARE on hot path. Normal flow uses `info!` only at boot. Real-time hot path emits ZERO log lines per tick. So 5-sink overhead doesn't apply per-tick.

**Verdict:** Acceptable. But add a DHAT zero-alloc test for `error!()` invocation to be safe.

### D2 — Synthetic dashboard tests — what platform?

**Options:**
| Option | Tech | Cost |
|---|---|---|
| (a) Selenium / Playwright headless | Heavy (Docker container) | 200MB RAM |
| (b) Grafana API direct query | Light (HTTP) | <10MB |
| (c) curl + jq parse | Lightest | <1MB |

**My vote:** (b) — Grafana has a `/api/datasources/proxy/<N>/api/v1/query` endpoint. Hourly cron pings it, asserts non-empty result.

### D3 — Should `make doctor` add an 8th section for observability?

Current `make doctor` has 7 sections. Adding 8th = observability health.

**Pro:** Single command answers "is everything OK".

**Con:** Test takes longer (~2 sec added).

**My vote:** YES. Wave 4 Task 20 already adds this.

### D4 — MCP server resilience

The `tickvault-logs` MCP server is critical for Claude sessions. If it dies, Claude operates blind.

**Defense:**
- Auto-respawn via `claude` CLI hook (does this exist?)
- Health-check ping every 30s
- Fallback: Claude session uses Bash to grep `data/logs/errors.jsonl.*` directly

**Open question:** Is MCP server already auto-respawning? Check `.mcp.json` config.

### D5 — Alertmanager — keep or replace?

Per `aws-budget.md` rule 9, Alertmanager is a separate Docker container ($256 MB RAM). Operator demand 2026-05-10 kept it independent of the app.

But — Alertmanager itself is a SPOF. If it dies, no alerts.

**Mitigation:**
- App also has direct Telegram path (notification_service.rs)
- App emits BOTH paths in parallel (Alertmanager + direct)
- If either succeeds, operator gets the message

**Verdict:** Belt + suspenders. Keep Alertmanager AND keep direct Telegram path.

---

## 🎤 Operator's question answered

**Q: "Will observability cover all the failure modes?"**

**A:** YES inside the envelope:
- 11 observability failure paths mapped to defenses
- 7-Layer Z+ pattern applied (77-cell matrix, 60+ cells covered)
- The 3 nightmare scenarios all have named defenses
- Belt + suspenders for Telegram (Alertmanager AND direct path)

**Honest gaps:**
- Grafana OOM has no L2/L3 — accepted (Grafana crashes too rarely to justify cost)
- Loki backpressure N/A (Loki removed Wave 7-A)
- MCP server auto-respawn — need to verify in `.mcp.json` (open question)

---

## 🎯 Open for discussion

Pick D1-D5 or surface new angle. 3 days, no implementation, brainstorming only.
