# Runbook — Alertmanager → Telegram Alerting

> **Purpose:** end-to-end reference for the automated alert pipeline. If an
> alert is not reaching Telegram during a real incident, this runbook is the
> triage tree.
> **Audience:** solo operator (Parthiban). Read top to bottom the first time
> you set up the stack, then use the "Symptom → fix" table for incidents.
> **Authority:** CLAUDE.md > `.claude/rules/project/aws-budget.md` > this file.

## 1. The alert pipeline (the whole chain)

```
┌──────────────┐   ┌───────────────┐   ┌───────────────┐   ┌──────────┐
│ tickvault    │→→→│ Prometheus    │→→→│ Alertmanager  │→→→│ Telegram │
│  (app)       │   │  (scrape)     │   │  (route)      │   │  Bot API │
└──────────────┘   └───────────────┘   └───────────────┘   └──────────┘
       │                   │                  │                  │
       │ metrics           │ alert rules      │ routing +        │ chat
       │ on :3001          │ + grafana rules  │ telegram webhook │ delivery
       │                   │                  │                  │
       └── also: ───────────────────────────────────────────────┘
           direct NotificationService.notify() calls for
           critical in-app events (bypasses the whole chain,
           used for SSM init failure, order errors, etc.)
```

**Two independent paths** deliver Telegram alerts. The app must NOT rely on
either one alone:

1. **In-app direct path** — `NotificationService::notify()` sends Telegram
   HTTP POST directly to the Bot API. Used for: OMS errors, circuit breaker
   trips, order rejections, sandbox guard hits. Does not need Prometheus,
   Alertmanager, or Grafana to be up. Session-8 C1 makes the app refuse to
   boot if this path is broken (`initialize_strict` panics in no-op mode).

2. **Metrics-based path** — Rust app emits Prometheus metrics on `:3001/metrics`,
   Prometheus scrapes every 15s, Grafana alert rules evaluate every 1m,
   matches go through Grafana's built-in alertmanager bridge (or the
   standalone Alertmanager container) and land in Telegram. Used for:
   DLQ writes, ring-buffer spillover, pool degradation, disk low, tick
   gap detection, QuestDB disconnected.

## 2. Configuration files and secrets

| File | What it does |
|---|---|
| `deploy/docker/alertmanager/alertmanager.yml` | Routing rules: severity=critical → telegram-critical receiver (10s wait, 1h repeat), severity=warning → telegram-warning (1m wait, 4h repeat). |
| `deploy/docker/grafana/provisioning/alerting/alerts.yml` | All Grafana alert rules: `tv-dlq-non-zero`, `tv-tick-spillover-active` (new in session 8), `tv-spill-disk-low-warn`, `tv-spill-disk-low-critical`, `tv-pool-degraded`, `tv-questdb-disconnected-30s`, etc. |
| AWS SSM `/tickvault/<env>/telegram/bot-token` | SecretString, fetched at boot by `NotificationService::initialize_strict`. |
| AWS SSM `/tickvault/<env>/telegram/chat-id` | SecretString, fetched at boot. |
| `config/base.toml` → `[notification]` | Non-secret notifier config (timeout, API base URL, SNS flag). |

**Never commit the bot token or chat ID. Never log them.** The
`banned-pattern-scanner.sh` and `secret-scanner.sh` block commits that
accidentally include them.

## 3. End-to-end smoke test (run before every market open)

After `make docker-up` and before market open:

```bash
# 1. Metrics endpoint alive.
curl -s http://127.0.0.1:3001/metrics | grep -c tv_ticks_total

# 2. Prometheus scraping the app.
curl -s 'http://127.0.0.1:9090/api/v1/targets' | grep -c '"health":"up"'

# 3. Grafana up and provisioned.
curl -s http://127.0.0.1:3000/api/health | grep '"database":"ok"'

# 4. Alertmanager (if used standalone, otherwise Grafana handles it).
curl -s http://127.0.0.1:9093/-/healthy

# 5. Direct app path — force a test notification.
#    The app exposes a debug /admin/test-alert endpoint ONLY in dev/sandbox.
#    Hit it to verify Telegram delivery end-to-end.
curl -X POST http://127.0.0.1:3001/admin/test-alert
# Expected: Telegram message "tickvault smoke test OK" within 2 seconds.
```

If step 5 fails, the in-app Telegram path is broken. Run:

```bash
# Verify SSM is reachable and secrets exist.
aws ssm get-parameter --name /tickvault/dev/telegram/bot-token --with-decryption --region ap-south-1 >/dev/null && echo OK
aws ssm get-parameter --name /tickvault/dev/telegram/chat-id --with-decryption --region ap-south-1 >/dev/null && echo OK
```

## 4. Symptom → fix triage

| Symptom | Most likely cause | Fix |
|---|---|---|
| No Telegram messages at all for >1h during a known incident | SSM fetch failed at boot; notifier in no-op mode | Session-8 C1 makes this impossible — the app would have refused to boot. If you see it, check systemd logs: `journalctl -u tickvault -n 200 --no-pager`. Look for "REFUSING BOOT — systemd will restart". |
| Grafana alerts fire but Telegram silent | Alertmanager down OR Grafana → Alertmanager bridge broken | `docker ps | grep alertmanager`, restart if dead. Check `http://127.0.0.1:9093/#/alerts`. |
| Telegram messages delayed >5m | `repeat_interval` is 1h for critical / 4h for warning, `group_interval` is 5m — an alert fires once, then gets silenced. Normal behaviour. | No fix needed. If you need immediate re-fire, reduce `repeat_interval` in `alertmanager.yml` and `docker compose restart tv-alertmanager`. |
| Duplicate Telegram messages | Two instances of the app are running, both sending | `ps aux | grep tickvault`. Kill the extra. Verify only one systemd unit is enabled: `systemctl list-units --state=running | grep tickvault`. |
| Telegram message missing fields (e.g., no security_id) | Event type was added without the field in `NotificationEvent::to_message()` | Edit `crates/core/src/notification/events.rs`, add the field, recompile. |
| Bot-token rotation | Bot was compromised or 24h rotation due | Put new token in SSM, restart app. App re-fetches on boot. Do NOT try to hot-swap — it's not supported. |
| SNS SMS not firing on Critical | SNS disabled in config OR phone number missing in SSM | `grep sns_enabled config/base.toml`. If `true`, check `aws ssm get-parameter --name /tickvault/dev/sns/phone-number`. SMS falls back silently if either fails. Telegram still fires. |
| Grafana rule exists but never fires | Metric name mismatch between rule and exporter | `curl -s http://127.0.0.1:3001/metrics | grep <metric_name>`. If empty, the Rust code isn't emitting the metric. Grep `crates/` for `metrics::counter!` / `metrics::gauge!` call sites. |

## 5. Self-test alert rule (optional, recommended)

A self-test rule that fires every `make alert-smoke-test` is useful for
verifying the whole chain without a real incident. It is NOT currently
wired; adding it is a one-line addition to `alerts.yml`:

```yaml
- uid: tv-self-test
  title: "SELF-TEST: alertmanager → telegram path is alive"
  condition: C
  data:
    - refId: A
      relativeTimeRange: { from: 60, to: 0 }
      datasourceUid: prometheus
      model:
        expr: "vector(1)"   # always fires when enabled
        intervalMs: 15000
  noDataState: OK
  execErrState: Alerting
  for: 0m
  annotations:
    summary: "SELF-TEST: alertmanager path is alive"
```

Toggle it on for 60 seconds via the Grafana UI, verify the Telegram
message arrives, then toggle it off. Do NOT leave it on permanently —
it will trigger every evaluation cycle.

## 6. References

- In-app path: `crates/core/src/notification/service.rs`
- Events: `crates/core/src/notification/events.rs`
- Strict init (session 8 C1): `NotificationService::initialize_strict`
- Prometheus config: `deploy/docker/prometheus/prometheus.yml`
- Grafana alert rules: `deploy/docker/grafana/provisioning/alerting/alerts.yml`
- Alertmanager routing: `deploy/docker/alertmanager/alertmanager.yml`
- AWS SSM secrets: see `docs/standards/secret-rotation.md`

## Update log

- **2026-04-14** — initial runbook, session 8. Added in response to
  Parthiban's "automated entirely without human intervention" request.
  Documents both the in-app direct path and the metrics-based path, the
  smoke test procedure, and the symptom → fix triage tree.
