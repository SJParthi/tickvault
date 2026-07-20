# Troubleshooting — One Section Per ErrorCode

> **Authority:** SEBI compliance + operator-charter §H.
> **Scope:** Every Phase 0 ErrorCode that an operator can encounter at boot or during market hours.
> **How to use:** Search this file by ErrorCode (e.g. `BOOT-01`). Every entry has Symptom → Cause → Fix → Cross-reference.
> **Source of truth:** `crates/common/src/error_code.rs::ErrorCode` enum + per-code runbook in `.claude/rules/project/wave-*-error-codes.md`.

## Quick index

| Code | Subsystem | Severity | Auto-triage safe |
|---|---|---|---|
| `BOOT-01` | QuestDB readiness (slow boot) | High | No |
| `BOOT-02` | QuestDB readiness (deadline exceeded) | Critical | No |
| `BOOT-03` | Clock skew vs trusted source | Critical | No |
| `RESILIENCE-01` | Dual-instance lock collision | Critical | No |
| `WS-GAP-04` | WS post-close sleep entered | Low | Yes |
| `WS-GAP-05` | Pool supervisor respawned task | Medium | Yes |
| `WS-GAP-06` | Tick-gap detector summary | Medium | Manual |
| `AUTH-GAP-02` | Token expired 807 trigger | High | Yes (after force-renewal) |
| `AUTH-GAP-03` | Token force-renewed on wake | Low | Yes |
| `DH-901` | Dhan auth error | High | First retry only |
| `DH-904` | Rate limit | High | Backoff ladder |
| `PHASE2-01` | Phase 2 dispatch failed | Critical | No |
| `PHASE2-02` | Phase2EmitGuard dropped | Critical | No |
| `PHASE2-READY-01` | Phase 2 pre-flight failed | Critical | No |
| `PREVCLOSE-01` | prev_close ILP write/flush failed | Medium | Yes (retry) |
| `PREVCLOSE-04` | Boot loader degraded | Medium | Yes (next boot) |
| `PREVOI-01` | prev_oi cache empty at boot | Medium | Yes (PR #452) |
| `SELFTEST-02` | Market-open self-test failed | Critical | No |
| `SLO-02` | Composite SLO score degraded | High / Critical | Per-dimension |
| `TELEGRAM-01` | Telegram event dropped | Medium | Yes |
| `AUDIT-01..06` | Audit-table write failed | High | Yes (rescue ring) |
| `STORAGE-GAP-03` | Audit write failure (any) | High | Yes |
| `STORAGE-GAP-05` | Disk-full pre-flight failed | Critical | No |
| `CORE-PIN-01` | Core affinity failed at boot | High | No |
| `CORE-PIN-02` | Worker drifted off core | Medium | Yes (re-pin) |
| `VOLUME-MONO-01` | Volume monotonicity breach | High | Manual |
| `I-P1-11` | Cross-segment collision | Info (was Error) | N/A — stored OK |
| `DEPTH200-SMOKE-01` | depth-200 no frames at boot | Critical | No (irrelevant under Phase 0) |
| `HOT-PATH-01` | Sync FS I/O failed in async task | High | Yes |
| `HOT-PATH-02` | Prev-close writer queue full | Medium | Yes (next snapshot) |

## Boot-time codes

### BOOT-01 — QuestDB slow-boot (>30s)

**Symptom:** Telegram WARN "BOOT-01 — boot deadline approaching"; app still trying to boot.
**Cause:** QuestDB container cold-starting (typical 20-40s) OR Docker daemon paused.
**Fix:**
  1. `docker ps` — confirm `tv-questdb` is in state `Up`.
  2. `make doctor` section 4 — green = wait; red = check Docker daemon.
  3. Boot will auto-succeed within 60s if QuestDB recovers; otherwise BOOT-02 fires + app HALTs.
**Cross-reference:** `docs/error-runbooks/wave-2-error-codes.md::BOOT-01`.

### BOOT-02 — QuestDB boot deadline exceeded (HALTING)

**Symptom:** Telegram CRITICAL "BOOT-02 — HALTING"; app exited.
**Cause:** QuestDB unreachable for 60s+. Docker daemon down OR network firewall.
**Fix:**
  1. `nc -z 127.0.0.1 9009` — must succeed.
  2. `tail -50 data/logs/auto-up.*.log` — inspect docker compose attempt.
  3. After QuestDB recovers, restart the app — boot will succeed in ~10s.
**Cross-reference:** `docs/error-runbooks/wave-2-error-codes.md::BOOT-02`.

### BOOT-03 — Clock skew (HALTING)

**Symptom:** Telegram CRITICAL "BOOT-03 — clock skew exceeded"; app exited.
**Cause:** Host wall-clock drift ≥ 2s vs `chronyc tracking` source OR QuestDB `now()`.
**Fix:**
  1. `chronyc tracking` — inspect "Last offset".
  2. `chronyc -a 'burst 4/4' && chronyc -a makestep` — force step adjustment.
  3. Restart app — BOOT-03 should clear.
**Do NOT:** silently raise the 2.0s threshold. IST midnight + 09:00 open + 15:30 close gates depend on accurate wall-clock.
**Cross-reference:** `docs/error-runbooks/wave-2-c-error-codes.md::BOOT-03`.

### RESILIENCE-01 — Dual-instance lock collision

**Symptom:** Telegram CRITICAL "DualInstanceDetected"; app refuses to start.
**Cause:** Another tickvault process is running against the same Dhan client-id.
**Fix:**
  1. `docker ps -a` + `ps aux | grep tickvault` — find the duplicate.
  2. Stop it cleanly: `systemctl stop tickvault` OR `kill -SIGTERM <pid>`.
  3. Wait 90s for the Valkey instance-lock TTL to expire.
  4. Restart this process.
**Cross-reference:** `docs/error-runbooks/wave-4-error-codes.md::RESILIENCE-01`.

## WebSocket codes

### WS-GAP-04 — Post-close sleep entered

**Symptom:** Telegram Info "WebSocketSleepEntered" after 15:30 IST.
**Cause:** Normal post-close transition; connection dormant until next market open.
**Fix:** None — informational. Pool supervisor keeps slot alive.
**Cross-reference:** `docs/error-runbooks/wave-2-error-codes.md::WS-GAP-04`.

### WS-GAP-05 — Pool supervisor respawned task

**Symptom:** `tv_ws_pool_respawn_total{reason="unexpected_clean_exit"}` increments.
**Cause:** A main-feed connection session ended with a server Close frame / TCP stream-end without a shutdown request (W2#8, 2026-07-10).
**Fix:** The slot's supervised loop auto-respawns within ~5s (storm-bounded 5s→300s backoff). If the rate sustains, Dhan keeps closing the session server-side — cross-check the token + Dhan status in `data/logs/errors.jsonl.*` (the WS-GAP-05 lines carry `backoff_secs` + `consecutive_quick_exits`). Release-build panics abort the process (`panic = "abort"`); recovery there is restart + WAL replay, not respawn.
**Cross-reference:** `docs/error-runbooks/wave-2-error-codes.md::WS-GAP-05`.

### WS-GAP-06 — Tick-gap detector fired

**Symptom:** Telegram coalesced summary every 60s naming instruments with ≥30s silence.
**Cause:** Slow socket, Dhan-side ExchangeSegment outage, or a starved subset of instruments.
**Fix:**
  1. Inspect the `top_10_samples` field on the event.
  2. If symbols cluster on one segment → Dhan-side outage; wait or escalate to Dhan support.
  3. If scattered → check `tv_websocket_connections_active` (1 expected under Phase 0 lock).
**Cross-reference:** `docs/error-runbooks/wave-2-error-codes.md::WS-GAP-06`.

## Auth / Dhan API codes

### AUTH-GAP-02 — Token expired (807) triggered refresh

**Symptom:** Disconnect code 807 in Dhan packet → automatic token refresh.
**Cause:** JWT crossed 24h validity window mid-session.
**Fix:** Token manager auto-refreshes via Valkey cache → SSM → TOTP regen.
**Cross-reference:** `docs/error-runbooks/wave-2-error-codes.md::AUTH-GAP-02`.

### AUTH-GAP-03 — Force-renew on WS wake

**Symptom:** Telegram Info "Token force-renewed on WS wake".
**Cause:** Token had < 4h validity when WS woke from post-close sleep.
**Fix:** None — preventive auto-action.
**Cross-reference:** `docs/error-runbooks/wave-2-error-codes.md::AUTH-GAP-03`.

### DH-901 — Dhan auth error

**Symptom:** Telegram HIGH "DH-901 — invalid auth".
**Fix:** Rotate token (Valkey → SSM → TOTP) → retry ONCE → if still fails, HALT + escalate.
**Cross-reference:** `.claude/rules/dhan/annexure-enums.md` rule 11.

### DH-904 — Rate limit

**Symptom:** HTTP 429-class from Dhan REST.
**Fix:** Exponential backoff 10s → 20s → 40s → 80s, then fail + CRITICAL.
**Source:** `crates/trading/src/oms/dh904_backoff.rs::compute_dh904_backoff`.
**Cross-reference:** `.claude/rules/dhan/api-introduction.md` rule 8.

## Phase 2 codes

### PHASE2-01 — Phase 2 dispatch failed

**Symptom:** Telegram CRITICAL "Phase2Failed" at 09:13 IST.
**Cause:** Empty plan at trigger (no NSE_EQ prices in pre-open buffer) OR Dhan rejected subscribe OR retries exhausted.
**Fix:**
  1. Inspect Telegram payload for `buffer_entries`, `skipped_no_price`, `skipped_no_expiry`.
  2. If `buffer_entries == 0` → pre-open buffer empty; check IDX_I + NSE_EQ subscriptions for 09:00-09:12.
  3. Decide go/no-go by 09:15 IST.
**Cross-reference:** `docs/error-runbooks/wave-1-error-codes.md::PHASE2-01`.

### PHASE2-02 — Phase2EmitGuard dropped

**Symptom:** `tv_phase2_emit_guard_dropped_total` increments; alert fires for 24h.
**Cause:** Code regression — Phase 2 scheduler returned without calling `emit_complete`/`emit_failed`/`emit_skipped`.
**Fix:** Find the recent edit to `phase2_scheduler.rs` that added a return path; ensure it calls `guard.emit_*(...)` before returning.
**Cross-reference:** `docs/error-runbooks/wave-1-error-codes.md::PHASE2-02`.

### PHASE2-READY-01 — Pre-flight failed at 09:13:01 IST

**Symptom:** Telegram CRITICAL "Phase2ReadinessFailed" naming which of 11 checks failed.
**Fix:** Each failing check name has a specific runbook (token_expiry_headroom → AUTH-GAP-03; questdb_ilp → BOOT-01/02; etc.). Operator has ~120s before 09:15 open.
**Cross-reference:** `docs/error-runbooks/wave-5-error-codes.md::PHASE2-READY-01`.

## previous_close codes

### PREVCLOSE-01 — ILP write/flush failed

**Symptom:** `tv_prev_close_persist_errors_total` increments.
**Fix:** `make doctor` confirms QuestDB. If healthy, ILP TCP transient — auto-recovers on next snapshot.
**Cross-reference:** `docs/error-runbooks/wave-1-error-codes.md::PREVCLOSE-01`.

### PREVCLOSE-04 — Boot loader degraded

**Symptom:** Telegram WARN/ERROR "PREVCLOSE-04" at boot; OI Change / OI Change % columns show 0.
**Cause:** QuestDB unreachable OR `previous_close` table empty for the last 7 trading days.
**Fix:** Run `make refresh-instruments` to re-fetch bhavcopy; restart app. Cascade falls back to 0.0 for the 3 % fields until next boot succeeds.
**Cross-reference:** `docs/error-runbooks/wave-1-error-codes.md::PREVCLOSE-04`.

### PREVOI-01 — prev_oi cache empty at boot

**Symptom:** Single WARN at boot only.
**Cause:** Boot-time bhavcopy + Option Chain prev_oi loader not yet wired (deferred to PR #452).
**Fix:** None — operator MUST NOT trust OI Change columns on `/api/movers` until #452 ships.
**Cross-reference:** `docs/error-runbooks/wave-1-error-codes.md::PREVOI-01`.

## Self-test / SLO codes

### SELFTEST-02 — Market-open self-test failed

**Symptom:** Telegram CRITICAL at 09:16:30 IST naming `failed: [check1, check2, ...]`.
**Fix:** Each sub-check has a specific runbook (main_feed_active → disaster-recovery.md scenario 6; questdb_connected → BOOT-01/02; token_expiry_headroom → AUTH-GAP-03).
**Cross-reference:** `docs/error-runbooks/wave-3-c-error-codes.md::SELFTEST-02`.

### SLO-02 — Composite SLO score degraded

**Symptom:** Telegram HIGH/CRITICAL "RealtimeGuaranteeDegraded/Critical" with `weakest` dimension named.
**Fix:** Follow the runbook for the named weakest dimension (WS_health, QDB_health, Tick_freshness, Token_freshness, Spill_health, Phase2_health). Do NOT try to "fix the composite" — fix the underlying typed code.
**Cross-reference:** `docs/error-runbooks/wave-3-d-error-codes.md::SLO-02`.

## Storage / audit codes

### AUDIT-01..06 — Audit-table write failed

**Symptom:** `STORAGE-GAP-03` umbrella + specific AUDIT-NN.
**Fix:** Check QuestDB ILP TCP + disk-full state. Ring buffer absorbs transient failures.
**Cross-reference:** `docs/error-runbooks/wave-2-error-codes.md::STORAGE-GAP-03`.

### STORAGE-GAP-05 — Disk-full pre-flight failed (HALTING)

**Symptom:** Telegram CRITICAL; app exited or refuses spill writes.
**Fix:** `df -h /data` — free space. Move old spill files to S3 cold archive OR enlarge EBS.
**Cross-reference:** `docs/error-runbooks/wave-4-error-codes.md::STORAGE-GAP-05`.

## Performance / hot-path codes

### CORE-PIN-01 — Core affinity failed at boot

**Symptom:** Telegram HIGH at boot; `tv_core_pinning_workers_pinned_total < 4`.
**Cause:** Host has < 4 logical cores OR cgroup cpuset rejects pin OR not running on t4g.medium / equivalent.
**Fix:** `nproc` ≥ 4 required; container needs `--cpuset-cpus=0-3` equivalent.
**Cross-reference:** `docs/error-runbooks/wave-5-error-codes.md::CORE-PIN-01`.

### CORE-PIN-02 — Worker drifted off core

**Symptom:** `tv_core_pinning_drift_total{worker_kind}` increments.
**Fix:** Drift watchdog re-pins automatically. If drift recurs > 1/min for 5min, escalate to CORE-PIN-01 root cause.

### HOT-PATH-01 — Sync filesystem I/O failed in async task

**Symptom:** Telegram HIGH; prev_close cache writer task errored.
**Fix:** `df -h data/instrument-cache/`; check mount + permissions. Writer self-recovers on next cache update.
**Cross-reference:** `docs/error-runbooks/wave-1-error-codes.md::HOT-PATH-01`.

### HOT-PATH-02 — Prev-close writer queue full

**Symptom:** `tv_prev_close_writer_dropped_total{reason=...}`.
**Fix:** `full` = slow disk (transient); `closed` = writer task exited (look for HOT-PATH-01); `uninit` = boot wiring missing (file a bug).

## Data-integrity codes

### VOLUME-MONO-01 — Cumulative volume monotonicity breach

**Symptom:** `tv_volume_monotonicity_breaches_total` increments; Telegram HIGH lists violating `(security_id, segment, previous, current, delta)`.
**Fix:**
  1. Confirm parser byte offsets (Quote bytes 22-25, Full bytes 22-25) are correct.
  2. Run the monotonicity SELECT from `docs/operator/track-2-monotonicity-select.md`.
  3. If reproducible → escalate Dhan via the draft email from PR #414.
**Cross-reference:** `docs/error-runbooks/wave-5-error-codes.md::VOLUME-MONO-01`.

### I-P1-11 — Cross-segment SecurityId collision

**Symptom:** `tv_instrument_registry_cross_segment_collisions` > 0 at boot.
**Cause:** Dhan CSV reuses the same `security_id` across different `ExchangeSegment` values (known FINNIFTY = 27 case).
**Fix:** None — `InstrumentRegistry::by_composite` stores both safely; `get_with_segment(id, seg)` is the correct lookup. Log level downgraded to Info 2026-04-19.
**Cross-reference:** `.claude/rules/project/security-id-uniqueness.md`.

## Notification codes

### TELEGRAM-01 — Telegram event dropped

**Symptom:** `tv_telegram_dropped_total{reason}` rate.
**Fix:**
  * `send_failed` → check Telegram bot API health, SSM bot-token validity.
  * `coalesced_sample_capped` → a single topic firing > 10×/60s; investigate the noisy subsystem.
  * `noop_mode` → SSM unavailable at boot; `make doctor` is RED.
**Cross-reference:** `docs/error-runbooks/wave-3-error-codes.md::TELEGRAM-01`.

## When you don't know the ErrorCode

Use the MCP tools — they're auto-loaded in every session:

```
mcp__tickvault-logs__tail_errors                    # last 50 ERRORs
mcp__tickvault-logs__find_runbook_for_code <CODE>   # auto-resolves runbook path
mcp__tickvault-logs__summary_snapshot               # last-hour signatures
mcp__tickvault-logs__run_doctor                     # 7-section health snapshot + CloudWatch alarms
```

The MCP server enforces the operator-charter §A automation rule: every
session has automated access to logs/queries/dbs without manual setup.

## Cross-references

  * Error code definitions: `crates/common/src/error_code.rs`
  * Per-code runbooks: `.claude/rules/project/wave-*-error-codes.md`
  * Operator daily startup: `docs/runbooks/operator-daily-startup.md`
  * Daily discipline: `docs/runbooks/daily-operations.md`
  * Phase 0 architecture: `.claude/rules/project/phase-0-architecture.md`
