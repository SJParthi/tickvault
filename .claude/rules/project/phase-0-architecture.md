---
paths:
  - "crates/storage/src/*_audit_persistence.rs"
  - "crates/trading/src/oms/**"
  - "crates/trading/src/indicator/**"
  - "crates/app/src/main.rs"
  - "crates/storage/src/static_ip_audit_persistence.rs"
---

# Phase 0 Architecture (auto-loaded rule)

> **Authority:** CLAUDE.md > `.claude/rules/project/operator-charter-forever.md` > this file > defaults.
> **Scope:** Every PR touching Phase 0 work — audit tables, OMS, risk engine, indicator engine, defensive guards.
> **Trigger:** Always loaded for sessions editing `crates/storage/src/*_audit_persistence.rs`,
> `crates/trading/src/oms/*`, `crates/trading/src/indicator/*`, or `crates/app/src/main.rs` Step 6a-prime.
> **Companion docs:** `docs/phases/phase-0-readme.md` (PR-by-PR history, audit-table family table).

## The Lean Lock contract (one line)

**Phase 0 makes the system production-safe for live SEBI-regulated F&O
trading on a single Dhan account — every safety-critical lifecycle
event has a 5-year SEBI-retentioned QuestDB audit row, every
defensive guard has a static-label Prometheus counter, every boot
gate fails closed with operator-readable Telegram, and the strategy
layer is dry-run by default.**

## The 4 architectural pillars

```
┌──────────────────────────────────────────────────────────────┐
│ PILLAR 1 — BOOT GATES (refuse to start if unsafe)            │
│   • Static IP whitelist verified vs Dhan /v2/ip/getIP        │
│   • Valkey dual-instance lock (one tickvault per client-id)  │
│   • Clock-skew probe (BOOT-03 if >2s)                         │
│   • QuestDB readiness (BOOT-01/02 if unreachable)             │
└──────────────────────────────────────────────────────────────┘
                            ↓
┌──────────────────────────────────────────────────────────────┐
│ PILLAR 2 — DEFENSIVE GUARDS (clamp + count, don't crash)     │
│   • IndicatorSnapshot::sanitize_nan_inf() — clamp NaN/Inf    │
│   • check_tick_ohlc_integrity() — 3-bit violation mask       │
│   • is_valid_ltp() / is_valid_tick() — early filter           │
│   • compute_dh904_backoff() — exponential ladder helper      │
│   • SELF_TRADE_COOLDOWN_SECS = 60 — wash-trade prevention    │
└──────────────────────────────────────────────────────────────┘
                            ↓
┌──────────────────────────────────────────────────────────────┐
│ PILLAR 3 — AUDIT TABLES (forensic chain, SEBI 5y retention)  │
│   11 tables, all following the static_ip_audit template:     │
│   • static_ip_audit       (boot gate 18)                     │
│   • live_instance_lock    (dual-instance gate 19)            │
│   • auth_renewal_audit    (24h JWT 17)                       │
│   • open_price_audit      (09:15 candle source 16a)          │
│   • order_update_ws_audit (live WS events 24a)               │
│   • self_trade_audit      (60s cooldown 22f-a)               │
│   • sl_replacement_audit  (auto-replace 22b-a)               │
│   • pnl_audit             (per-instrument P&L 25-a)          │
│   • Plus: phase2_audit / boot_audit / selftest_audit         │
│     (pre-Phase-0 Wave 2 family)                              │
└──────────────────────────────────────────────────────────────┘
                            ↓
┌──────────────────────────────────────────────────────────────┐
│ PILLAR 4 — OPERATOR VISIBILITY (Telegram + Prometheus)       │
│   • Severity::Critical Telegram on every halt-class outcome  │
│   • Severity::Info daily pings: pre-open readiness 09:14,    │
│     post-open streaming 09:15:30, EOD digest 15:31:30        │
│   • Static-label counters per defensive-guard violation kind │
│   • Edge-triggered alerts (rising edge only — no spam)       │
└──────────────────────────────────────────────────────────────┘
```

## Boot sequence (Phase 0 specific — see main.rs for canonical)

The 4 Phase-0 boot gates run in this order:

```
Step 5.5 — Public IP vs SSM static IP (egress IP check)
Step 6   — Dhan authentication (JWT via TOTP)
Step 6a-prime — Item 19: Valkey dual-instance lock
   • Fetch Valkey password from SSM
   • try_acquire_instance_lock() — HALT if AlreadyHeld
   • Spawn 30s heartbeat task
Step 6a — Item 18: Dhan-side static IP boot gate
   • POST /v2/ip/getIP — HALT if ordersAllowed=false
   • Retry 30× with 60s backoff (30 min propagation window)
Step 6b — QuestDB DDL join (11+ audit tables + lifecycle tables)
Step 6c — Pre-market readiness check
... → Steps 7-N continue as normal
```

**Failure semantics:** Every Phase 0 gate is **fail-closed**. A failure
HALTs the boot, fires a Severity::Critical Telegram, and writes the
failure outcome to the relevant audit table. No silent skips.

## The audit-table template (canonical reference)

Every Phase 0 audit table follows the same 8-element template. The
canonical reference is `crates/storage/src/static_ip_audit_persistence.rs`.

| Element | Purpose | Ratchet test |
|---|---|---|
| `QUESTDB_TABLE_*` const | Wire-format table name | `test_table_name_constant` |
| `DEDUP_KEY_*` const | Composite UPSERT KEYS clause | `test_dedup_key_includes_designated_timestamp` (regression: 2026-04-28) |
| Typed `*Outcome` enum | SYMBOL column wire-format labels | `test_outcome_as_str_stable` |
| `ensure_*_table` async fn | Idempotent CREATE TABLE IF NOT EXISTS | TEST-EXEMPT (live QuestDB) |
| `append_*_audit_row` async fn | INSERT helper with nanos→micros | TEST-EXEMPT (live QuestDB) |
| Schema DDL string | Column list (8-11 columns typical) | `test_ddl_contains_expected_columns` |
| nanos→micros conversion | `ts_micros_ist = ts_nanos_ist / 1_000` | `test_insert_sql_uses_microseconds_not_nanoseconds` |
| Tokio error-path coverage | 2-3 `#[tokio::test]` covering each outcome | (per-test) |

**Composite DEDUP rules:**

  1. `trading_date_ist` MUST be in every DEDUP key (designated timestamp
     partition column — required by QuestDB, regression-class fix
     2026-04-28).
  2. For per-instrument audits, include `(security_id, exchange_segment)`
     per I-P1-11 composite identity rule.
  3. For lifecycle-chain audits (Detected → Replaced, etc.), include
     `outcome` so transition rows BOTH survive — without it the
     terminal row silently overwrites the trigger row.
  4. For rapid-succession events (partial fills), include `ts` explicitly
     so two same-second rows for the same key both survive.
  5. NEVER include `security_id` alone — always paired with
     `exchange_segment` per I-P1-11.

## The defensive-guard template (canonical reference)

Every Phase 0 defensive guard follows the same 4-element pattern:

| Element | Purpose |
|---|---|
| Pure-function helper | Returns a bitmask or `Option<Duration>` — caller decides action |
| Hot-path wiring (`#[inline]`) | Zero-alloc, O(1), called per-tick or per-fill |
| Static-label counter | `tv_<guard>_<kind>_total` — no allocation at emission |
| Source-scan meta-guard | Pinned in `secret_manager.rs::tests` — fails build if call site or counter is removed |

**Canonical examples:**

  * `IndicatorSnapshot::sanitize_nan_inf()` — clears 20 f64 fields,
    increments `tv_indicator_nan_guard_fired_total` per cleared field.
  * `check_tick_ohlc_integrity(ltp, high, low) -> u8` — 3-bit
    violation mask, 3 separate static-label counters per bit.
  * `compute_dh904_backoff(attempts) -> Option<Duration>` — caller
    decides between `sleep` and CRITICAL escalation.

## Cross-references (auto-loaded sibling rules)

  * `operator-charter-forever.md` — the 14-row VIP-protection demand matrix
  * `testing.md` — 22-test taxonomy each touched crate must satisfy
  * `hot-path.md` — zero-allocation enforcement
  * `live-feed-purity.md` — `ticks` table has ONE write path; no historical
    backfill EVER lands there
  * `security-id-uniqueness.md` — I-P1-11: `security_id` ALONE is not unique
  * `audit-findings-2026-04-17.md` — 11 rules that prevent regression of
    bug classes from the 2026-04-17 audit session
  * `data-integrity.md` — WebSocket vs Greeks timestamp rules (the
    CRITICAL +5:30 IST offset asymmetry)
  * `market-hours.md` — 09:00-15:30 IST trading window; background
    workers must be market-hours aware

## Checkpoints for every Phase 0 PR

Before merging any PR touching Phase 0 paths, verify:

  * [ ] Does it add a new audit table? → Follow the 8-element template
        + add `ensure_*_table` to main.rs boot-time `tokio::join!`.
  * [ ] Does it add a new defensive guard? → Follow the 4-element
        pattern + add a `secret_manager.rs::tests::test_*_is_wired`
        source-scan meta-guard.
  * [ ] Does it add a new ErrorCode? → Add a rule-file mention (the
        cross-ref test requires it) + a runbook path under
        `.claude/rules/`.
  * [ ] Does it add a new Telegram event variant? → Verify
        Severity::Critical for halt-class, Severity::Info for daily
        pings; route via `notification::events::NotificationEvent`.
  * [ ] Does it add a new Prometheus counter? → Static label only,
        no per-emission allocation; document in this file or
        `phase-0-readme.md`.
  * [ ] Does it touch the OMS hot path? → Run `cargo bench` against
        `quality/benchmark-budgets.toml` to confirm no >5% regression.
  * [ ] Is it a docs-only PR? → Cross-reference against existing rule
        files; no broken links.

## Phase 1 promotion criteria

Before `dry_run = false` flips:

  1. All Phase 0 plan items checked in
     `topic-PHASE-0-LEAN-LOCKED.md`.
  2. Boot gates 18 + 19 both pass on the AWS prod instance (not just
     dev Mac).
  3. All 11 audit tables exist in prod QuestDB (`make doctor` confirms).
  4. Items 22a-wire / 22b-b / 22f-b OMS engine integrations all
     source-scan-meta-guarded.
  5. Item 25-b/c P&L scaffolding promoted from dry-run to live
     accumulator + daily-loss-threshold Telegram variant + Prom alert.
  6. 27a-h documentation chain complete (this file is 27h).

## How to read this file

When opening a session that touches Phase 0 code, this file appears
in your auto-loaded context alongside the operator-charter. Use it as
the canonical mental model:

  * What pillar is the change touching? (Boot gate, defensive guard,
    audit table, operator visibility)
  * Does it follow the canonical template for that pillar?
  * Does the checkpoints list catch anything that needs an extra
    ratchet test / meta-guard?

If the answer to any of those is unclear, READ:

  * `docs/phases/phase-0-readme.md` for the PR-by-PR history
  * `.claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md`
    for the canonical item list with `[x]` / `[ ]` checkboxes
  * The relevant sibling rule file from the cross-reference list above
