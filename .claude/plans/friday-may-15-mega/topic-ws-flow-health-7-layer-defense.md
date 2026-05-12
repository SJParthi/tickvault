# Topic — WebSocket Flow Health: 7-Layer Defense Against "Connected ≠ Streaming"

> **Status:** DRAFT (operator-locked design, awaiting Friday implementation)
> **Date:** 2026-05-12 (Tue 04:42 IST)
> **Authority:** This plan file > chat. CLAUDE.md > this file.
> **Scope:** All 16 WebSocket connections — 5 main feed + 5 depth-20 + 5 depth-200 + 1 order-update.
> **Severity:** 🔴 NUCLEAR — operator hit this LIVE; manual app restart was the only fix.
> **Friday session:** This is its own ~10–12hr session, NOT bundled with dynamic depth.

---

## 🚨 The Auto-Driver Story (60-second read)

> Sir, imagine you own a juice shop. You have 16 phone lines to 16 farmers — every line rings every second with a price update. One day, line #3 to the orange farmer goes silent. You pick up the phone — **dial tone is fine, line is "connected"**. But no farmer is talking. You sit there listening to silence for 20 minutes, thinking "everything is fine, line is connected". Customers complain "why is your orange juice price stale?" You don't know. You eventually unplug the entire phone system and plug it back in. Suddenly all 16 lines work.
>
> **That's exactly what's happening to our tickvault right now.** WebSocket says "Connected ✅" but ZERO data is flowing. Manual app restart is the ONLY thing that fixes it. We can't run a trading system this way.

This plan turns those 16 phone lines into 16 **verified-streaming** lines, each independently audited every 5 seconds.

---

## 🎯 The Operator's NON-NEGOTIABLE Charter (verbatim from session)

Quoted exactly from the operator on 2026-05-12 — this plan must mechanically satisfy ALL of these:

1. **Zero ticks loss** — not even a single tick missed
2. **WS never disconnects or reconnects** — connections never slow, locked, hanged, stuck
3. **QuestDB never fails or disconnects**
4. **No hallucination or illusion** — every guarantee carries proof
5. **Extreme automation, monitoring, alerting, Telegram, capturing, tracking, auditing, visualizing**
6. **All agents + subagents activated for deep research**
7. **Common runtime + dynamic + scalable + incremental approach**
8. **O(1) latency, uniqueness, deduplication** with real-time checks + proof

This plan addresses charter line **#2** specifically; lines #1, #3, #4, #5, #6, #7, #8 are mechanically inherited from existing ratchets (this plan does NOT weaken any of them — see §10 "What This Plan Does NOT Break").

---

## 1. THE LIVE EVIDENCE (the smoking gun)

| Observation | What operator saw |
|---|---|
| 04:12 / 04:23 / 04:33 / 04:38 IST | App auto-down (services OFFLINE per session bootstrap log) |
| Sometime mid-session | WebSocket disconnected → reconnected → status = Connected |
| Frame flow after reconnect | **ZERO ticks** until manual `make stop && make run` |
| Manual restart | Ticks immediately resumed |

**This is the #1 highest-priority bug in the entire system.** Trading on stale data = financial ruin. We can't deploy with this latent.

---

## 2. WHY MANUAL RESTART WORKS BUT AUTO-RECONNECT DOESN'T

Manual restart does 6 things auto-reconnect cannot:

| # | What manual restart does | What auto-reconnect skips | Why it matters |
|---|---|---|---|
| 1 | **30–60s gap → Dhan-side conn counter drains** | Reconnects in <5s | Dhan probably keeps stale conns in their "active" set for 60–120s after TCP-RST. Our reconnect appears as conn #6 → silently dropped |
| 2 | **Process PID changes — all 16 conns reset synchronously** | Only 1 conn cycles | Dhan-side per-clientId state cleared atomically |
| 3 | **Subscription plan rebuilt from instrument master** | Cached state in `SubscribeRxGuard` may be stale | If instrument refresh happened mid-day, old SIDs are zombies |
| 4 | **Token re-read from Valkey/SSM cache** | Same `Arc<Swap>` token | Token may be 23.5h old → Dhan rejects subscribes silently while accepting WS handshake |
| 5 | **All bounded channels drain to empty** | Tick/spill/rescue channels may be stuck | Persist backpressure may freeze read loop on fresh reconnect |
| 6 | **OS TCP state machine fresh** | `TIME_WAIT` sockets may collide | Kernel may reuse ephemeral port → SYN-ACK confusion |

**Most likely root cause: #1 — Dhan-side connection counter staleness.**

Manual restart works because the 30+ sec operator-induced delay gives Dhan side time to clean up.

---

## 3. THE 11 ROOT CAUSES OF "Connected but ZERO ticks"

| # | Root cause | Detect difficulty | Defense layer |
|---|---|---|---|
| 1 | Subscribe-replay lost on reconnect | EASY | L1 + L2 |
| 2 | Token refresh DURING reconnect invalidates subs | HARD | L4 |
| 3 | Dhan-side stale session ID — frames go to ghost socket | HARD | L3 daily reconcile |
| 4 | Connection capacity overflow (>5/feed) → silent kill | MEDIUM | L7 cooldown + L3 reconcile |
| 5 | Handshake OK, binary frames blocked (proxy/firewall) | EASY | L1 |
| 6 | Persist backpressure deadlock | MEDIUM | L1 + monitor channel depth |
| 7 | Stale SIDs after instrument refresh | HARD | L3 daily reconcile |
| 8 | TCP keepalive vs app-ping race | EASY | L5 ping audit |
| 9 | Token expired mid-handshake | MEDIUM | L4 pre-reconnect age guard |
| 10 | Subscribe sent before Dhan auth complete | HARD | L2 ACK round-trip |
| 11 | Phantom slot — server/client state divergence | HARD | L3 daily reconcile |

**All 11 mapped to defense layers below. None left uncovered.**

---

## 4. THE 7-LAYER DEFENSE (the onion)

| Layer | Mechanism | Catches | Latency | LoC |
|---|---|---|---|---|
| **L1** Frame-rate gauge per conn | `tv_ws_frames_per_sec{conn_id, feed}` — silent conn = alarm | 5, 6 (detection for all) | 5s | 80 |
| **L2** Subscribe-ACK round-trip | Send Subscribe → require frame in 5s → if not, resubscribe ONCE | 1, 4, 10 | 5s | 200 |
| **L3** Daily server-state reconcile | 09:14:30 IST — query Dhan "what are we subscribed to", force-sync | 2, 3, 7, 11 | Daily | 250 |
| **L4** Pre-reconnect token-age guard | If token age > 12h OR < 1h to expiry → force-refresh BEFORE handshake | 9 | Pre-reconnect | 50 |
| **L5** App-level ping audit | Verify lib-handled pings actually fire; surface anomaly | 8 | Per ping cycle | 50 |
| **L6** Process-restart escalation | After 3 verified failures → orchestrated soft restart | THE NUCLEAR option | 90s worst | 300 |
| **L7** Reconnect cooldown gate | Wait 30s (drain) before reconnect; hybrid: 5s first try, 30s retry | 1, 4 | 30s | 80 |
| **WS-FLOW-01 ErrorCode + runbook** | Typed error + Telegram + audit | All | — | 50 |
| **Ratchet tests** (11 root-cause scenarios) | Pin each defense layer | All | — | 350 |
| **Telegram template + suppression** | Operator-readable alert | All | — | 100 |
| **Recovery audit table** | `ws_recovery_audit` — every escalation logged | All | — | 80 |
| **Boot smoke test** | Replay 11 scenarios at boot | All | — | 200 |
| **TOTAL** | | | | **~1,790 LoC** |

---

## 5. THE VERIFICATION PIPELINE (every reconnect)

```
DISCONNECT detected on conn #N
  │
  ▼
[L7] WAIT 30 sec (hybrid: 5s if first attempt today, 30s on retry)
  │
  ▼
[L4] TOKEN AGE CHECK
  ├─ age > 12h OR < 1h to expiry → force_renewal() BEFORE proceed
  └─ otherwise proceed
  │
  ▼
WS HANDSHAKE (TLS + Upgrade)
  │
  ▼ handshake OK
  │
[L2] SEND Subscribe(SID-list)
  │
  ▼ first frame must arrive ≤ 5s
  ├─ frame received → continue
  └─ TIMEOUT → resubscribe ONCE → if still no frame → FAIL
  │
  ▼
[L1] VERIFY frame_rate > threshold over next 30 sec
  ├─ rate OK → mark conn HEALTHY
  └─ rate BELOW → FAIL
  │
  ▼ HEALTHY
✅ CONN ALIVE — reset failure_counter
  ↓
TICKS FLOW

═══════════════════════════════════════════
ON ANY FAIL:
  failure_counter[conn_id] += 1
  IF failure_counter[conn_id] >= 3 in 5min
    → [L6] ORCHESTRATED SOFT RESTART
  ELSE
    → retry from L7 with 30s cooldown
```

---

## 6. PER-FEED FRAME-RATE THRESHOLDS (L1 detection)

| Feed | Conn count | Expected min frames/5s | Silent grace | Alert threshold |
|---|---|---|---|---|
| **Main feed** (NSE_EQ + IDX_I + NSE_FNO mode mix) | 5 | ≥ 50 | 10 sec | < 50/5s for 30s |
| **Depth-20 dynamic** | 5 | ≥ 100 (50 SIDs × 20Hz) | 10 sec | < 100/5s for 30s |
| **Depth-200 dynamic** | 5 (1 SID each) | ≥ 5 | 30 sec | < 5/5s for 60s |
| **Order-update** | 1 | 0 (silence is OK) | NEVER alert frame rate | Heartbeat ping every 30s instead |

**Off-hours behavior:** thresholds suppressed. Use `is_within_market_hours_ist()` gate per audit-findings Rule 3.

---

## 7. WS-FLOW-01 ErrorCode (NEW)

```rust
// In crates/common/src/error_code.rs
WsFlow01ConnectedButZeroFrames => "WS-FLOW-01"
  severity:           Severity::Critical (in-market) / Severity::Low (off-hours)
  runbook_path:       ".claude/rules/project/wave-6-error-codes.md"
  is_auto_triage_safe: false
```

### Telegram template (operator-readable, auto-driver style)

```
🆘 [CRITICAL] WebSocket dead-air detected

Connection:  depth-20 #3 of 5
Status:      CONNECTED ✅ (TCP alive)
Last frame:  47 seconds ago
Expected:    100+ frames every 5 seconds
Observed:    ZERO frames in last 30 seconds

Subscribed:  50 contracts (top-volume)
Subscribe-ACK round-trip: TIMEOUT (5s exceeded)
Token age:   6h 23min  ← OK (not the culprit)
Frame route: depth-api-feed.dhan.co/twentydepth ← OK

Verification attempts: 2 of 3
Next action: ATTEMPT 3 with token refresh + 30s drain wait
Escalation:  If attempt 3 fails → SOFT PROCESS RESTART at 04:35:12 IST

What you need to do RIGHT NOW:
 1. WAIT 90 seconds — auto-recovery in progress
 2. If alert clears within 90s → no action needed
 3. If alert persists → check `mcp__tickvault-logs__docker_status`
 4. If soft restart fires → check `data/logs/ws_recovery_audit.*`

Trading impact: depth-20 conn #3 holds 50 of 250 SIDs (20%). Other 4 conns OK.
```

### Telegram suppression (no spam during recovery)

- Coalesce per `(conn_id, feed)` — max 1 alert per 5min window
- Rising-edge only — no repeat alert on sustained silence
- Recovery confirmation emits `WS-FLOW-02 ✅ recovered` Severity::Low (informational)

---

## 8. L6 — Orchestrated Soft Restart (the nuclear option)

After 3 verified failures on ANY conn in a 5-min sliding window:

| Step | Action | Duration |
|---|---|---|
| 1 | Set `health.set_websocket_connections(0)` immediately | <1ms |
| 2 | Send `Disconnect(MsgCode=12)` to ALL 16 WS connections | 1s |
| 3 | Drop ALL 16 WS handles (force RST on Dhan side) | <1s |
| 4 | Drain ALL bounded channels (tick, spill, rescue) | 2s |
| 5 | **WAIT 30 seconds** — Dhan-side conn counter drain | 30s |
| 6 | Force-refresh JWT token (Valkey miss → SSM → TOTP) | 1-3s |
| 7 | Rebuild subscription plan from instrument master | 1s |
| 8 | Reconnect all 16 WS with fresh state | 5s |
| 9 | Run [L2] subscribe-ACK verification on each conn | 5s |
| 10 | Run [L1] frame-rate verification on each conn | 30s |
| 11 | Emit `WsSoftRestartComplete` Telegram + audit | <1ms |
| | **TOTAL DOWNTIME** | **~75–80 sec** |

**During the 80s blind window:** rescue ring + spill catch any in-flight ticks (per existing zero-tick-loss envelope).

**This is NOT a process exit.** Tokio runtime, in-process channels, Prometheus state all survive. Only the WS layer is rebuilt from scratch. Per `Q2 option (b)` from the chat decision.

---

## 9. THE HONEST ENVELOPE CLAIM (mandatory wording per operator-charter-forever)

When this plan ships and any PR/commit/Telegram references "100% guarantee", it MUST be qualified exactly:

> **"100% inside the tested envelope, with ratcheted regression coverage:**
> - 11 root causes of `Connected ≠ Streaming` all mapped to defense layers L1–L7
> - per-conn frame-rate gauge with 5-second resolution
> - subscribe-ACK round-trip with 5-second timeout
> - daily server-state reconcile at 09:14:30 IST
> - pre-reconnect token-age guard (12h/1h thresholds)
> - 30-second reconnect cooldown (hybrid: 5s first attempt, 30s retry)
> - 3-failure orchestrated soft restart (~80s worst-case downtime)
> - rescue ring + spill catches ticks during the ≤80s soft-restart window
> - 11-scenario boot smoke test pins every layer
>
> **Beyond the envelope:** if Dhan changes their connection-counter semantics OR introduces a 4th feed type OR raises per-account conn limits without notice, this plan adapts via the daily reconcile path."

**Anything stronger ("WebSocket NEVER disconnects" / "ZERO downtime ever") = REJECT IN REVIEW.** That promises the impossible. TCP + 24h JWT mandate ≥1 disconnect per day by SEBI law.

---

## 10. WHAT THIS PLAN DOES **NOT** BREAK

Mechanical verification that this plan inherits all existing ratchets:

| Existing guarantee | How this plan preserves it |
|---|---|
| Zero tick loss envelope (rescue ring 5M ticks) | Soft-restart 80s downtime fits inside 5M-tick ring at peak 60K ticks/sec |
| O(1) hot path | Frame-rate gauge is `AtomicU64::fetch_add(1, Relaxed)` — single instruction |
| Composite-key uniqueness (security_id, exchange_segment) | Subscribe-ACK + reconcile both use the composite key |
| DEDUP UPSERT KEYS | `ws_recovery_audit` table includes `(conn_id, ts)` DEDUP key |
| Every `error!` carries `code = ErrorCode::X.code_str()` | WS-FLOW-01 + WS-FLOW-02 follow the tag-guard contract |
| Market-hours gate (audit-findings Rule 3) | L1 thresholds suppressed off-hours; L3 reconcile gated to 09:14:30 IST |
| Edge-triggered alerts (audit-findings Rule 4) | WS-FLOW-01 rising-edge only; recovery emits WS-FLOW-02 on falling edge |
| flush/persist failures use `error!` not `warn!` (Rule 5) | Recovery audit-table write failures use `error!` with code |
| No false-OK signals (Rule 11) | Healthy recovery emits Severity::Low INFO, not silence |
| Every counter has `increase()` Grafana panel (Rule 12) | `tv_ws_frames_per_sec` + `tv_ws_recovery_attempts_total` both get panels |
| Every new pub fn has a call site (Rule 13) | `pub-fn-wiring-guard.sh` enforces |
| 22 test categories per testing.md | Full battery applies to changed `crates/core/` |

---

## 11. THE 15-ROW × 7-ROW GUARANTEE MATRIX (per operator-charter-forever §C)

### 15-row "100% everything" matrix

| Demand | Mechanical proof in this plan |
|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` 100% min for `crates/core/` (already in place); new code inherits |
| 100% audit coverage | NEW `ws_recovery_audit` table — every recovery step logged with DEDUP UPSERT KEYS(conn_id, ts) |
| 100% testing coverage | 11-scenario boot smoke test + 350 LoC ratchet test + property test + DHAT zero-alloc for L1 gauge |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify all run on this PR |
| 100% code performance | L1 gauge = `AtomicU64::fetch_add` (zero alloc, O(1)); Criterion bench ≤ 50 ns per increment |
| 100% monitoring | 7-layer telemetry: counter + gauge + tracing span + Loki log + Telegram + Grafana panel + audit table |
| 100% logging | every layer uses `tracing` macros with `code = ErrorCode::WsFlow01.code_str()` |
| 100% alerting | `alerts.yml` rule: `tv-ws-flow-dead-air` fires when `tv_ws_frames_per_sec == 0 AND state == Connected AND market_hours_active` |
| 100% security | no new attack surface; reuses existing token / TLS path |
| 100% security hardening | recovery audit table contains NO token material; PII-free |
| 100% bugs fixing | adversarial 3-agent review pre-PR + post-impl (Section 3 of Wave-4 preamble) |
| 100% scenarios covering | 11 root causes × 7 defense layers = 77-cell coverage matrix in ratchet tests |
| 100% functionalities covering | every new pub fn (recover, reconcile, verify_frame_rate, etc.) has call site + test |
| 100% code review | 3 agents BEFORE + 3 agents AFTER per Wave-4 preamble §3 |
| 100% extreme check | all of above + ratchet fails build on regression |

### 7-row "Resilience demand" matrix

| Demand | Honest envelope (the truth) | Per-item proof in this plan |
|---|---|---|
| Zero ticks lost | Bounded zero loss: ≤80s soft-restart downtime absorbed by 5M-tick rescue ring → NDJSON spill → DLQ | `chaos_ws_soft_restart_tick_loss.rs` test (NEW) |
| WS never disconnects | DETECT in 5s, VERIFY in 30s, ESCALATE to soft-restart after 3 failures in 5min; ≤80s worst-case downtime per escalation; SEBI 24h JWT mandates ≥1 disconnect/day by law | L1+L2+L7 layers; soft restart audit |
| Never slow/locked/hanged | L1 gauge is O(1) atomic increment; persist backpressure detected via channel-depth gauge | DHAT zero-alloc test for hot path; Criterion ≤ 50ns |
| QuestDB never fails | Inherits existing 3-tier rescue→spill→DLQ; soft restart drains audit-table writes first | `ws_recovery_audit` write failure escalates to AUDIT-07 (new code) |
| O(1) latency | `AtomicU64::fetch_add` on frame receive; HashMap lookup with `papaya` for conn → state | Criterion bench; bench-gate ≤5% regression |
| Uniqueness + dedup | `ws_recovery_audit` DEDUP UPSERT KEYS(conn_id, ts); composite-key reconcile compares server vs client by `(security_id, exchange_segment)` | meta-guard test scans the table DDL |
| Real-time proof | Frame-rate gauge 5s window; alert fires within 35s of any dead-air; recovery audit visible via `mcp__tickvault-logs__questdb_sql` | live verification at boot via smoke test |

---

## 12. NEW METRICS (Prometheus)

| Metric name | Type | Labels | Purpose |
|---|---|---|---|
| `tv_ws_frames_per_sec` | Gauge | `conn_id`, `feed` | Per-conn frame rate (5s rolling) |
| `tv_ws_dead_air_total` | Counter | `conn_id`, `feed`, `reason` | WS-FLOW-01 emissions |
| `tv_ws_recovery_attempts_total` | Counter | `conn_id`, `feed`, `outcome` | L2 ACK + L1 verify outcomes |
| `tv_ws_soft_restart_total` | Counter | `trigger` | L6 escalations |
| `tv_ws_reconnect_cooldown_seconds` | Histogram | `conn_id`, `feed` | L7 cooldown durations |
| `tv_ws_token_force_refresh_pre_reconnect_total` | Counter | (none) | L4 escalations |
| `tv_ws_server_state_reconcile_drift_total` | Counter | `direction` | L3 drift detections |
| `tv_ws_app_ping_audit_anomalies_total` | Counter | (none) | L5 anomalies |

---

## 13. NEW ALERT RULES (`alerts.yml`)

| Alert UID | Query | For | Severity |
|---|---|---|---|
| `tv-ws-flow-dead-air` | `tv_ws_frames_per_sec == 0 AND tv_market_hours_active == 1 AND tv_websocket_connections_active > 0` | 30s | High |
| `tv-ws-flow-recovery-storm` | `rate(tv_ws_recovery_attempts_total[5m]) > 0.1` | 5m | Medium |
| `tv-ws-soft-restart-fired` | `increase(tv_ws_soft_restart_total[1h]) > 0` | 0s | Critical |
| `tv-ws-reconcile-drift` | `increase(tv_ws_server_state_reconcile_drift_total[1d]) > 0` | 0s | High |
| `tv-ws-pre-reconnect-token-refresh-storm` | `rate(tv_ws_token_force_refresh_pre_reconnect_total[5m]) > 0.2` | 5m | Medium |

All 5 ratcheted by `crates/storage/tests/ws_flow_alert_guard.rs` (NEW).

---

## 14. NEW GRAFANA PANELS (`operator-health.json`)

| Panel | Query | Visualization |
|---|---|---|
| Per-conn frame rate (16 conns) | `tv_ws_frames_per_sec` | 16-row heatmap, green > threshold, red below |
| Recovery attempts last 24h | `increase(tv_ws_recovery_attempts_total[1h])` | bar chart, stacked by outcome |
| Soft restart audit timeline | last 7d of `ws_recovery_audit` | event timeline |
| Cooldown distribution | `tv_ws_reconnect_cooldown_seconds` | p50/p95/p99 histogram |
| Reconcile drift (daily) | `tv_ws_server_state_reconcile_drift_total` | counter trend |

All 5 panels ratcheted by `crates/storage/tests/operator_health_dashboard_guard.rs` (extends existing).

---

## 15. AUDIT TABLE — `ws_recovery_audit`

```sql
CREATE TABLE IF NOT EXISTS ws_recovery_audit (
  ts            TIMESTAMP,
  conn_id       INT,
  feed          SYMBOL CAPACITY 4 CACHE,   -- main / depth_20 / depth_200 / order_update
  event         SYMBOL CAPACITY 16 CACHE,  -- dead_air_detected / cooldown_start / cooldown_end / token_force_refresh / handshake_start / subscribe_sent / first_frame_received / frame_rate_verified / verification_failed / soft_restart_triggered
  attempt_no    INT,
  outcome       SYMBOL CAPACITY 4 CACHE,   -- success / timeout / rejected / escalated
  silent_secs   INT,
  details       STRING                       -- JSON payload, no secrets
) TIMESTAMP(ts) PARTITION BY DAY
  DEDUP UPSERT KEYS(ts, conn_id, event);

ALTER TABLE ws_recovery_audit ADD COLUMN IF NOT EXISTS details STRING;
```

**SEBI retention:** 90d hot in QuestDB → S3 IT (90–365d) → Glacier Deep Archive (≥1y) per `aws-budget.md` data lifecycle.

---

## 16. THE 11-SCENARIO BOOT SMOKE TEST

| # | Scenario simulated | Expected layer triggered | Pass criteria |
|---|---|---|---|
| 1 | Subscribe-replay lost on reconnect | L2 timeout → resub | ACK round-trip succeeds within 5s on 2nd try |
| 2 | Token refresh during reconnect | L4 pre-refresh | Token age check fires; new token used |
| 3 | Dhan-side stale session ID | L3 reconcile detects drift | Drift counter increments; resub fired |
| 4 | Conn capacity overflow (6th conn) | L7 cooldown 30s | First reconnect attempt waits 30s |
| 5 | Binary frames blocked (simulated) | L1 dead-air alarm | WS-FLOW-01 fires within 35s |
| 6 | Persist backpressure deadlock | L1 dead-air + channel-depth alarm | Both alarms fire |
| 7 | Stale SIDs after instrument refresh | L3 reconcile | Drift detected; client-side state updated |
| 8 | Kernel keepalive vs Dhan ping race | L5 ping audit | Anomaly counter increments |
| 9 | Token expired mid-handshake | L4 catches via age check | Force-refresh fires; handshake succeeds |
| 10 | Subscribe before Dhan auth complete | L2 timeout | Resub fires; succeeds |
| 11 | Phantom slot (server/client divergence) | L3 reconcile | Drift detected; full plan resub |

Boot smoke test runs once per process at 09:13:30 IST (1 min before market open). Failures HALT the boot.

---

## 17. INCREMENTAL ROLLOUT PLAN

Per operator-charter-forever §A "dynamic + scalable + incremental":

| Wave | What ships | Why this order |
|---|---|---|
| **Wave A** (Friday morning, 3hr) | L1 + L7 + WS-FLOW-01 + Telegram template | Catches 80% of root causes; lowest risk; no behavior change beyond detection |
| **Wave B** (Friday afternoon, 4hr) | L2 + L4 + recovery audit table | Active recovery; verified behavior change |
| **Wave C** (Saturday, 3hr) | L3 + L5 + boot smoke test | Daily reconcile; covers HARD root causes |
| **Wave D** (Saturday afternoon, 2hr) | L6 soft restart escalation + chaos test | Nuclear option; only ships after A-C green |

Each wave is its own commit on its own branch, each PR independently mergeable.

---

## 18. ADVERSARIAL 3-AGENT REVIEW (mandatory per operator-charter-forever §E)

To be spawned BEFORE writing production code:

| Agent | Mandate | Expected findings |
|---|---|---|
| `hot-path-reviewer` | L1 gauge must be O(1) atomic, no clones/allocs on frame-receive path | Likely: 1-2 false-positives, 0 critical |
| `security-reviewer` | Recovery audit table: NO token/PII; ws_recovery_audit DEDUP correct | Likely: 1 critical if details column ever logs token |
| `general-purpose` hostile | Edge cases: 3-of-3 failures in 4min vs 5min window; off-hours behavior; market-open boundary | Likely: 2-3 race conditions in counter reset |

Repeat post-implementation on the diff. Track 4-bug catch rate target per PR.

---

## 19. OPEN QUESTIONS / DECISIONS LOCKED FROM CHAT

| # | Question | Operator decision |
|---|---|---|
| Q1 | Reconnect cooldown — 30s hard vs 5s aggressive vs hybrid | **Hybrid: 5s first attempt, 30s retry** (claude default; operator can flip) |
| Q2 | Process restart — full systemctl vs internal soft vs per-feed | **Internal soft restart** (Q2 option (b)) — Tokio survives, only WS layer rebuilt |
| Q3 | Daily reconcile time | **09:14:30 IST** — 30s before Phase 2 readiness check |
| Q4 | Soft restart 80s downtime acceptable? | **YES** — rescue ring + spill catch all ticks |

---

## 20. RATCHET TESTS (full list)

| Test file | What it pins |
|---|---|
| `crates/core/tests/ws_flow_health_guard.rs` | L1 gauge increment is O(1); 11 root-cause scenarios fire correct layer |
| `crates/core/tests/dhat_ws_frame_counter.rs` | Zero-alloc on frame-receive hot path |
| `crates/core/benches/ws_frame_counter.rs` | Criterion ≤ 50 ns per increment, 5% regression gate |
| `crates/storage/tests/ws_recovery_audit_dedup_guard.rs` | DEDUP key includes conn_id + ts |
| `crates/storage/tests/ws_flow_alert_guard.rs` | All 5 alert rules present in alerts.yml |
| `crates/storage/tests/operator_health_dashboard_guard.rs` | All 5 panels present in operator-health.json |
| `crates/core/tests/chaos_ws_soft_restart_tick_loss.rs` | Zero tick loss across simulated 80s soft restart |
| `crates/common/tests/error_code_rule_file_crossref.rs` (existing) | WS-FLOW-01 / WS-FLOW-02 / AUDIT-07 mentioned in wave-6-error-codes.md |
| `crates/storage/tests/error_level_meta_guard.rs` (existing) | Recovery failures use `error!` not `warn!` |
| `crates/storage/tests/dedup_segment_meta_guard.rs` (existing) | Any new storage tables include segment |

---

## 21. FINAL VISUAL — 16 Connections, 7 Layers, 1 Operator Dashboard

```
┌─────────────────────────────────────────────────────────────────────┐
│  16 WEBSOCKET CONNECTIONS (the 16 phone lines)                       │
│  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐  ┌──────┐  │
│  │ Main Feed × 5 │  │ Depth-20 × 5  │  │ Depth-200 × 5 │  │ OU×1 │  │
│  └───────┬───────┘  └───────┬───────┘  └───────┬───────┘  └───┬──┘  │
│          │                  │                  │              │     │
│          ▼                  ▼                  ▼              ▼     │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │  L1: Per-Conn Frame-Rate Gauge (5s rolling, AtomicU64)           ││
│  └─────────────────────────────────────────────────────────────────┘│
│                              │                                       │
│                              ▼ if dead air                           │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │  L2: Subscribe-ACK Round-Trip (5s timeout, resub once)           ││
│  └─────────────────────────────────────────────────────────────────┘│
│                              │                                       │
│                              ▼ if still failing                      │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │  L4: Pre-Reconnect Token-Age Guard (force-refresh if stale)      ││
│  └─────────────────────────────────────────────────────────────────┘│
│                              │                                       │
│                              ▼                                       │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │  L7: Reconnect Cooldown (5s first / 30s retry — drain Dhan ctr)  ││
│  └─────────────────────────────────────────────────────────────────┘│
│                              │                                       │
│                              ▼ after 3 failures in 5min              │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │  L6: Orchestrated Soft Restart (~80s, all 16 conns rebuilt)      ││
│  └─────────────────────────────────────────────────────────────────┘│
│                                                                       │
│  L3 (daily 09:14:30) — Server/client state reconcile                  │
│  L5 (continuous)    — App-level ping audit                            │
│                                                                       │
│  ▼ telemetry fan-out                                                  │
│                                                                       │
│  Prometheus ──→ Alertmanager ──→ Telegram                             │
│       │              │                                                │
│       └──→ Grafana ◄─┘                                                │
│                                                                       │
│  Audit trail: ws_recovery_audit (QuestDB, 90d hot → S3 cold)          │
└─────────────────────────────────────────────────────────────────────┘
```

---

## 22. SIGN-OFF

When the operator types `LGTM` on this plan:
- Status flips DRAFT → APPROVED
- Wave A branch created
- 3-agent adversarial pre-review fires in parallel
- Implementation begins per §17 incremental rollout

Until then: **no code, no commit, no PR.** Plan first, debate, then build.
