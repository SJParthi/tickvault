# Kill Switch Runbook — Emergency Stop

> **Authority:** SEBI compliance + operator-charter §H.
> **Scope:** Every operator must know this procedure before live capital is deployed.
> **When to use:** Runaway losses, system malfunction, regulator alert, suspected breach, end-of-shift handover.
> **Time-to-act target:** Activation under 30 seconds from decision to "trading stopped" confirmation.

## What the kill switch does

When activated, the kill switch:

  1. **Stops all new order placement** at the OMS layer — every new
     `place_order` call returns `OmsError::KillSwitchActive`
     immediately without touching Dhan's REST API.
  2. **Disables strategy signal evaluation** — the strategy FSM
     short-circuits at the entry point; no signal_audit /
     decision_audit rows produced.
  3. **PRESERVES existing positions** — does NOT auto-close open
     positions. Open positions remain until the operator manually
     closes them via Dhan web/app OR explicitly invokes
     `POST /v2/positions` (exit-all).
  4. **PRESERVES live data feed** — tick ingestion + audit persistence
     continue. The kill switch is a TRADING-only stop, not a
     SYSTEM stop.
  5. **Writes an `OperatorKillSwitchActivated` audit row** to
     `boot_audit` (Wave 2 table) with a typed `reason` field.

What the kill switch does NOT do:

  * Does NOT cancel pending orders. Use Dhan exit-all (`DELETE
    /v2/positions`) or kill switch via Dhan API for that.
  * Does NOT shut down the app. The tickvault process keeps running
    so log/audit retention continues.
  * Does NOT page anyone. The operator already knows — they pressed
    it. Severity::Info Telegram fires for record-keeping only.

## The 3 activation paths

### Path A — Dhan-side kill switch (preferred for live emergencies)

Per `.claude/rules/dhan/traders-control.md` rule 2:

```bash
# Activate
curl -X POST 'https://api.dhan.co/v2/killswitch?killSwitchStatus=ACTIVATE' \
  -H "access-token: <JWT>" \
  -H "dhanClientId: 1106656882"

# Check status
curl -X GET 'https://api.dhan.co/v2/killswitch' \
  -H "access-token: <JWT>" \
  -H "dhanClientId: 1106656882"

# Deactivate (manual recovery only)
curl -X POST 'https://api.dhan.co/v2/killswitch?killSwitchStatus=DEACTIVATE' \
  -H "access-token: <JWT>" \
  -H "dhanClientId: 1106656882"
```

**Dhan-side kill switch prerequisite:** all positions must be CLOSED
and no pending orders. If positions exist, close them FIRST via
exit-all OR individual cancels.

**Effect:** disables ALL trading for the day, exchange-side. No
new order can reach Dhan even if tickvault's local gate is
bypassed.

### Path B — tickvault local kill switch (preferred for system-level issues)

A future Item 27e-b commit will expose an HTTP endpoint and a
CLI flag. The wiring lives behind the existing OMS-GAP-01 state
machine — `engine.activate_kill_switch()` already exists in
`crates/trading/src/oms/engine.rs`.

```bash
# (planned — Item 27e-b)
curl -X POST 'http://localhost:3001/api/kill-switch/activate' \
  -H "Authorization: Bearer $TV_API_TOKEN"
```

**Effect:** tickvault refuses to place orders going forward;
existing positions untouched; Dhan side is NOT touched (so
operator can use Path A on top if needed).

### Path C — Process-level kill (last resort)

If the app is unresponsive:

```bash
# 1. Stop the systemd unit (clean shutdown — flushes audit
#    buffers, releases the instance-lock cleanly).
sudo systemctl stop tickvault

# 2. If systemd is unresponsive, SIGTERM the binary directly:
pkill -SIGTERM -f tickvault

# 3. ONLY if SIGTERM doesn't work in 30s, SIGKILL:
pkill -SIGKILL -f tickvault
```

SIGKILL skips graceful shutdown:
  * Last 90s of audit buffers may be lost (ring + spill catches
    most of it).
  * The Valkey instance-lock will expire after 90s TTL (next
    boot waits ~90s before acquiring).
  * No GracefulRelease audit row will land (Item 19f).

## Decision tree

```
                    ┌──────────────────┐
                    │  Trading issue?  │
                    └─────────┬────────┘
                              │
              ┌───────────────┴───────────────┐
              │                               │
        OPEN POSITIONS                 NO OPEN POSITIONS
              │                               │
              ↓                               ↓
      Path A NOT available           Path A available
      (Dhan rejects if               (preferred — exchange
       positions exist)               enforces immediately)
              │                               │
              ↓                               ↓
      Path B (local OMS)             Path A + Path B
      then close positions            (defense in depth)
      via Dhan web/app
              │
              ↓
      Reassess: Path A
```

## Post-activation checklist

After activating kill switch via ANY path, the operator MUST:

  1. **Confirm activation in Telegram.** The
     `OperatorKillSwitchActivated` event fires within 5 seconds.
     If you don't see it within 30 seconds, escalate.
  2. **Query the audit table** to confirm persistence:
     ```sql
     SELECT * FROM boot_audit
     WHERE outcome = 'kill_switch_activated'
       AND trading_date_ist = today()
     ORDER BY ts DESC LIMIT 5;
     ```
  3. **Inspect open positions** via Dhan web/app OR
     `GET /v2/positions`. Decide individually whether to close.
  4. **Inspect pending orders** via `GET /v2/orders`. Cancel any
     that should not execute.
  5. **Log the activation reason** in the operator journal —
     timestamp, who activated, why, what state was at the time.
  6. **Page the operator-of-record** if you are not the
     operator-of-record. Phase 0 is solo-operator; this means
     "page Parthiban via WhatsApp."

## Recovery procedure

Kill switch deactivation is INTENTIONALLY manual — there is no
auto-recover. After investigating the trigger:

  1. **Document the root cause** in the operator journal.
  2. **Fix the underlying issue** (or confirm it can't recur).
  3. **Manual deactivate:**
     * Path A: `POST /v2/killswitch?killSwitchStatus=DEACTIVATE`
     * Path B: (Item 27e-b endpoint) — restart pending
  4. **Restart strategies cautiously** — start with dry-run mode
     enabled (`config.strategy.dry_run = true`) and a single
     low-conviction strategy. Verify normal signal_audit /
     decision_audit flow before unfreezing the full strategy set.

## SEBI compliance notes

  * Kill switch activation is a regulator-relevant event. The
    `boot_audit` row + the Telegram message together form the
    record SEBI auditors will check.
  * Per `.claude/rules/project/market-hours.md`: never submit
    orders at or after 15:30 IST. If you find yourself reaching
    for kill switch at 15:29:55 IST, you're too late — the
    exchange is about to close anyway.
  * Per `audit-findings-2026-04-17.md` Rule 5: kill switch
    activation logs at ERROR level so Loki routes to Telegram.

## Operator drill — practise this

Once per quarter:

  1. After-hours (post-15:30 IST, when no positions exist),
     activate Path A.
  2. Verify Telegram fires within 5 seconds.
  3. Verify `boot_audit` row lands.
  4. Verify `GET /v2/killswitch` returns `ACTIVATE`.
  5. Deactivate Path A.
  6. Verify `GET /v2/killswitch` returns `DEACTIVATE`.

If any step fails the drill, the kill switch chain is broken.
Treat it as a SEV-1 — the system is not safe to trade until
fixed.

## Cross-references

  * Dhan kill switch API: `.claude/rules/dhan/traders-control.md`
  * OMS state machine: `crates/trading/src/oms/state_machine.rs`
  * Wave 2 audit infrastructure: `crates/storage/src/boot_audit_persistence.rs`
  * Operator daily startup: `docs/runbooks/daily-operations.md`
  * Phase 0 architecture: `.claude/rules/project/phase-0-architecture.md`
  * Operator charter §H: `.claude/rules/project/operator-charter-forever.md`

## What this runbook does NOT cover

  * **Position close-out procedure** — see Dhan web/app interface
    or `dhan-ref/12-portfolio-positions.md` for the exit-all API.
  * **P&L exit auto-trigger** — see `.claude/rules/dhan/traders-control.md`
    rules 7-12 for the configurable per-day profit/loss thresholds.
    Different mechanism from kill switch.
  * **System restart procedure** — see
    `docs/runbooks/daily-operations.md`.
