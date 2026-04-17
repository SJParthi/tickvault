# Implementation Plan: Instrument Subsystem — 100% Comprehensive Hardening

**Status:** DRAFT — awaiting Parthiban approval
**Date:** 2026-04-17
**Approved by:** pending

## Goal (Parthiban's exact words)

> "Instruments are the core heartbeat. If it breaks in live trading = instant
> panic. We cannot tolerate any issue in the future. Need 100% coverage:
> no missing functionality, scenarios, edges, auditing, precautions, testing,
> performance, monitoring. Zero tick loss, zero WebSocket disconnect, zero
> QuestDB failure. O(1) latency always. Real proof, no hallucination."

## Scope

Every worst case, edge case, corruption path, and operational failure mode in
the instrument subsystem — detected, alerted, tested, and automatically
handled with zero human intervention.

---

## Comprehensive Worst-Case Catalog (enumerated)

### Tier 1 — Dhan CSV data integrity

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 1.1 | Empty CSV (0 rows) | ✓ I-P0-02 | Keep + test `test_empty_csv_hard_fails` |
| 1.2 | Truncated CSV (<100 derivatives) | ✓ I-P0-02 | Keep + test existing |
| 1.3 | Malformed CSV rows | Partial | Add fail-fast if >5% rows malformed |
| 1.4 | Duplicate SecurityId in CSV | ✓ I-P0-01 | Keep + test existing |
| 1.5 | ISIN collision (2 contracts, same ISIN) | MISSING | Add I-P1-09: detect + alert |
| 1.6 | UTF-8 corruption / BOM | ✓ BOM strip | Add lossy UTF-8 fallback |
| 1.7 | CSV endpoint 403/404/500 | ✓ 3 retries | Keep + test backoff |
| 1.8 | MITM / CSV tampered | MISSING | Add SHA256 hash pinning (new gap I-P1-12) |
| 1.9 | Mismatched row count (header says N, body has M) | MISSING | Add count validation |
| 1.10 | Column reorder (new version of CSV) | ✓ header-indexed | Keep, add test |

### Tier 2 — SecurityId edge cases

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 2.1 | ID reused across underlyings | ✓ I-P1-03 | Keep + test existing |
| 2.2 | Same contract gets new ID | ✓ I-P1-04 | Keep + test existing |
| 2.3 | **Two-way ID swap (A:1 B:2 ↔ A:2 B:1)** | MISSING | Add I-P1-10: detect swap |
| 2.4 | SecurityId = 0 | MISSING | Reject at parse |
| 2.5 | SecurityId < 0 (unsigned underflow) | Partial | Explicit negative check |
| 2.6 | SecurityId > i32::MAX | MISSING | Reject at parse |
| 2.7 | Missing SecurityId for new contract | MISSING | Skip + CRITICAL alert |
| 2.8 | SecurityId type change (int → string) | MISSING | Schema drift test |

### Tier 3 — Contract lifecycle edges

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 3.1 | Contract expires at build time (expiry < today) | ✓ Pass 5 filter | Keep + test existing |
| 3.2 | **Contract expires mid-session (weekly Thursday 3:30 PM)** | MISSING | Runtime expiry check, reject order |
| 3.3 | Underlying renamed (WIPRO → WIPRO-BE) | MISSING | Add I-P1-11: `SymbolRenamed` event |
| 3.4 | Lot size changes (corporate action) | ✓ I-P1-02 | Keep + test existing |
| 3.5 | Tick size changes | ✓ I-P1-02 | Keep + test existing |
| 3.6 | Strike/option_type changes | ✓ I-P1-02 | Keep + test existing |
| 3.7 | Series change (EQ → BE) | MISSING | New event `SeriesChanged` |
| 3.8 | Delisting | ✓ ContractExpired | Keep + test |
| 3.9 | ASM/GSM flag flipped (surveillance) | MISSING | New event `SurveillanceFlagChanged` |
| 3.10 | F&O ban period | MISSING | New event, disable orders for symbol |
| 3.11 | Merger/demerger (complete identity change) | MISSING | Manual mapping + Telegram alert |
| 3.12 | Upper/lower circuit | Partial (risk) | Verify risk engine rejects |

### Tier 4 — Universe size anomalies (DoS / API bug detection)

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 4.1 | >50% contracts disappear overnight | MISSING | New alert `MassiveContraction` → HALT |
| 4.2 | >50% contracts appear overnight | MISSING | New alert `MassiveExpansion` → manual review |
| 4.3 | Single underlying disappears (all NIFTY gone) | MISSING | New alert `CoreUnderlyingMissing` → HALT |
| 4.4 | All contracts same expiry | MISSING | New alert `NoFutureExpiries` → HALT |
| 4.5 | Stock count outside [150, 300] | ✓ I-P0-02 | Keep |
| 4.6 | Sudden zero active contracts | MISSING | New alert `UniverseEmpty` → HALT |

### Tier 5 — Timing / clock issues

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 5.1 | System clock drift | MISSING | NTP check at boot, alert if skew > 60s |
| 5.2 | Running in wrong timezone | ✓ IST enforced | Keep |
| 5.3 | Download on weekly expiry day 3:29 PM | ✓ freshness marker | Keep |
| 5.4 | Run across IST midnight boundary | Partial | Add midnight refresh trigger |
| 5.5 | Holiday weekend (Fri PM → Tue) | ✓ holiday calendar | Keep |
| 5.6 | DST transition (host only) | ✓ IST fixed | Keep |
| 5.7 | Clock jumps backward (NTP sync) | MISSING | Saturating math on all date comparisons |

### Tier 6 — Persistence integrity

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 6.1 | QuestDB crash mid-write | ✓ WAL + retries | Keep |
| 6.2 | DEDUP key lost (ALTER TABLE mistake) | MISSING | Boot-time assertion |
| 6.3 | Disk full during write | ✓ DLQ | Keep |
| 6.4 | **Multiple app instances running (race)** | MISSING | PID file lock + port conflict detection |
| 6.5 | ILP buffer overflow | ✓ batched flush | Keep |
| 6.6 | **Write fails mid-batch (20K of 100K)** | MISSING | Staging table + atomic swap |
| 6.7 | Connection timeout | ✓ backoff | Keep |
| 6.8 | QuestDB schema drift (column renamed by DBA) | MISSING | Schema version check |

### Tier 7 — Audit trail

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 7.1 | Reconstruct universe at past date | MISSING (after I-P1-08 change) | Add `first_seen_date`, `expired_date` cols |
| 7.2 | Missed Telegram alert | MISSING | Write CRITICAL alerts to `critical_alerts` QuestDB table |
| 7.3 | Silent data loss | ✓ DLQ metric | Keep + test `dlq_ticks_total == 0` |
| 7.4 | Change attribution (who/what triggered update) | MISSING | Add `change_source` column |
| 7.5 | SEBI compliance query "what traded on March 1?" | MISSING | Join orders + lifecycle cols |

### Tier 8 — Security

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 8.1 | CSV endpoint MITM | MISSING | HTTPS hash pinning |
| 8.2 | Malicious instruments injected | ✓ bounds validation | Keep + test extreme values |
| 8.3 | Rate limit hit on /v2/instrument/ | ✓ backoff | Keep |
| 8.4 | IP block | ✓ GAP-NET-01 | Keep |
| 8.5 | Secret leakage in logs | ✓ Secret<T> | Keep |
| 8.6 | Dependency supply chain attack | Weekly audit | Add `cargo-audit` to pre-push |

### Tier 9 — Monitoring / alerting gaps

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 9.1 | Build duration spike (normally 2s, now 30s) | MISSING | Alert on >10x median |
| 9.2 | CSV download size drops 50% (API bug) | MISSING | Alert on size anomaly |
| 9.3 | Delta event count anomaly (500 daily → 50000) | MISSING | Alert on anomalous event rate |
| 9.4 | Tick gap per instrument > threshold | ✓ RISK-GAP-03 | Keep |
| 9.5 | Zero ticks for >1 min during market hours | ✓ watchdog | Keep |
| 9.6 | Metrics endpoint down | MISSING | Prometheus scrape health check |

### Tier 10 — Performance / O(1) guarantees

| # | Scenario | Current Coverage | Required Action |
|---|----------|------------------|-----------------|
| 10.1 | Hot-path instrument lookup | ✓ papaya O(1) | Keep + benchmark |
| 10.2 | Zero-alloc parsing | ✓ DHAT test | Keep |
| 10.3 | Build takes >10s | MISSING | Alert + profile |
| 10.4 | Memory growth during build | ✓ DHAT | Keep |
| 10.5 | rkyv deserialization latency | ✓ benchmark | Keep + budget enforcement |

---

## Gap Enforcement Additions (13 new gaps)

- **I-P1-09**: ISIN collision detection
- **I-P1-10**: Two-way SecurityId swap detection
- **I-P1-11**: Symbol rename detection
- **I-P1-12**: CSV SHA256 hash pinning
- **I-P1-13**: Massive universe contraction/expansion alerts
- **I-P1-14**: Core underlying missing alert
- **I-P1-15**: Runtime expiry check on every order
- **I-P1-16**: Multiple app instance detection (PID lock)
- **I-P1-17**: DEDUP key boot-time assertion
- **I-P1-18**: Schema drift detection
- **I-P1-19**: NTP clock skew check
- **I-P1-20**: CSV size anomaly alert
- **I-P1-21**: Build duration anomaly alert

Each has: rule text + mechanical test + Telegram alert wiring.

## Plan Items (in priority order)

- [ ] **Priority 1 — Data integrity locks:**
  - Add runtime expiry check (I-P1-15) — prevents expired contract orders
  - Add DEDUP key boot assertion (I-P1-17) — ensures dedup never silently off
  - Add universe size anomaly alerts (I-P1-13, I-P1-14)
  - Tests: property test `prop_same_day_reload_idempotent_1000_runs`,
    property test `prop_cross_day_no_duplication_30_days`

- [ ] **Priority 2 — SecurityId safety:**
  - Add two-way ID swap detection (I-P1-10)
  - Add symbol rename detection (I-P1-11)
  - Add ISIN collision detection (I-P1-09)
  - Tests: `test_id_swap_detected`, `test_symbol_rename_detected`, `test_isin_collision_detected`

- [ ] **Priority 3 — Audit trail (lifecycle columns):**
  - Add `status`, `first_seen_date`, `last_seen_date`, `expired_date` columns
  - Add `mark_missing_as_expired` function
  - Update Grafana queries to filter `WHERE status = 'active'`
  - Tests: `test_expired_contract_marked`, `test_reappeared_contract_reactivated`,
    `test_historical_query_reconstruct_universe`

- [ ] **Priority 4 — Operational hardening:**
  - NTP clock skew check (I-P1-19)
  - Multiple instance detection via PID lock (I-P1-16)
  - CSV SHA256 hash pinning (I-P1-12)
  - Schema drift detection (I-P1-18)

- [ ] **Priority 5 — Monitoring:**
  - CSV size anomaly alert (I-P1-20)
  - Build duration anomaly alert (I-P1-21)
  - CRITICAL alerts written to `critical_alerts` QuestDB table
  - Alert delivery SLO: 99.9% within 60s

- [ ] **Priority 6 — Property tests for 100% coverage:**
  - `prop_upsert_idempotency_100_runs_same_day`
  - `prop_cross_day_universe_size_stable`
  - `prop_all_expired_contracts_have_expired_date`
  - `prop_no_ghost_rows_in_active_set`
  - `prop_security_id_changes_always_detected`

## Mechanical Enforcement Summary

Every item in this plan becomes:
1. A rule in `.claude/rules/project/gap-enforcement.md`
2. A test (unit + integration + property) that blocks commit/push if broken
3. A Prometheus metric
4. A Telegram alert rule
5. A runbook entry in `docs/runbooks/instruments.md`

## Rollout

Phases (each = separate commit + PR):
1. **Hardening** (Priority 1): Zero-duplicate guarantee + universe size alerts
2. **Detection** (Priority 2): SecurityId + symbol + ISIN detection
3. **Audit** (Priority 3): Lifecycle columns + historical queries
4. **Operational** (Priority 4): NTP + PID lock + hash pinning + schema drift
5. **Monitoring** (Priority 5): Anomaly alerts + critical_alerts table
6. **Property tests** (Priority 6): 100% invariant coverage

## Estimated Effort

~6 focused sessions (~15 hours total). Each phase is independently testable
and mergeable.

## Approval Required

**Confirm:** "approved — proceed with Priority 1 first" or specify priority order.
