# tickvault Runbooks — Error Code → Action

**When a Telegram alert fires:** look up the code in the table below.
Each row points at either an auto-fix script (runs in dry-run first)
or the authoritative `.claude/rules/*.md` rule file that defines
the remediation steps.

**Runbook discipline:**
- Always dry-run before executing an auto-fix (`scripts/foo.sh --dry-run`)
- Always check `data/logs/errors.summary.md` before escalating
- Never bypass the circuit breaker manually

## Table — all 54 ErrorCode variants mapped to actions

| Code | Severity | Primary runbook | Auto-fix available | Claude-auto-triage-safe |
|---|---|---|---|---|
| **I-P0-01** DuplicateSecurityId | Medium | `.claude/rules/project/security-id-uniqueness.md` | — | yes |
| **I-P0-02** CountConsistency | Medium | `.claude/rules/project/gap-enforcement.md` | `scripts/auto-fix-refresh-instruments.sh` | yes |
| **I-P0-03** ExpiryAtGate4 | High | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **I-P0-04** CachePersistence | Medium | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **I-P0-05** S3Backup | Medium | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **I-P0-06** EmergencyDownload | Critical | `.claude/rules/project/gap-enforcement.md` | — | **no** |
| **I-P1-01** DailyScheduler | Low | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **I-P1-02** DeltaFieldCoverage | Low | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **I-P1-03** SecurityIdReuse | Medium | `.claude/rules/project/security-id-uniqueness.md` | — | yes |
| **I-P1-05** CompoundDedupKey | Medium | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **I-P1-06** SegmentInTickDedup | Medium | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **I-P1-08** SingleRowLifecycle | Medium | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **I-P1-11** CrossSegmentCollision | Medium | `.claude/rules/project/security-id-uniqueness.md` | silence (expected) | yes |
| **I-P2-02** TradingDayGuard | Low | `.claude/rules/project/market-hours.md` | — | yes |
| **GAP-NET-01** IpMonitor | Medium | `.claude/rules/dhan/authentication.md` | — | yes |
| **GAP-SEC-01** ApiAuth | Medium | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **OMS-GAP-01** StateMachine | High | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **OMS-GAP-02** Reconciliation | High | `.claude/rules/dhan/orders.md` | — | yes |
| **OMS-GAP-03** CircuitBreaker | Critical | `.claude/rules/project/gap-enforcement.md` | — | **no** |
| **OMS-GAP-04** RateLimit | High | `.claude/rules/dhan/api-introduction.md` | backoff (auto) | yes |
| **OMS-GAP-05** Idempotency | High | `.claude/rules/dhan/orders.md` | — | yes |
| **OMS-GAP-06** DryRunSafety | Critical | `.claude/rules/project/gap-enforcement.md` | — | **no** |
| **WS-GAP-01** DisconnectClassification | Medium | `.claude/rules/dhan/live-market-feed.md` | — | yes |
| **WS-GAP-02** SubscriptionBatching | Medium | `.claude/rules/dhan/live-market-feed.md` | — | yes |
| **WS-GAP-03** ConnectionState | Medium | `.claude/rules/dhan/live-market-feed.md` | `scripts/auto-fix-restart-depth.sh` (depth only) | yes |
| **RISK-GAP-01** PreTrade | High | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **RISK-GAP-02** PositionPnl | High | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **RISK-GAP-03** TickGap | Medium | `.claude/rules/project/gap-enforcement.md` | — | yes |
| **AUTH-GAP-01** TokenExpiry | Critical | `.claude/rules/dhan/authentication.md` | auto-refresh (23h) | **no** |
| **AUTH-GAP-02** DisconnectTokenMap | Critical | `.claude/rules/dhan/authentication.md` | auto-refresh on 807 | **no** |
| **STORAGE-GAP-01** TickDedupSegment | Medium | `.claude/rules/project/gap-enforcement.md` | `scripts/auto-fix-clear-spill.sh` | yes |
| **STORAGE-GAP-02** F32F64Precision | Medium | `.claude/rules/project/data-integrity.md` | — | yes |
| **DH-901** InvalidAuth | Critical | `.claude/rules/dhan/authentication.md` | rotate-once (auto) | **no** |
| **DH-902** NoApiAccess | Critical | `.claude/rules/dhan/authentication.md` | — | **no** |
| **DH-903** AccountIssue | Critical | `.claude/rules/dhan/authentication.md` | — | **no** |
| **DH-904** RateLimit | High | `.claude/rules/dhan/api-introduction.md` | exponential backoff (auto) | yes |
| **DH-905** InputException | High | `.claude/rules/dhan/annexure-enums.md` | never retry (fix request) | yes |
| **DH-906** OrderError | High | `.claude/rules/dhan/orders.md` | never retry (fix order) | yes |
| **DH-907** DataError | Medium | `.claude/rules/dhan/annexure-enums.md` | check params | yes |
| **DH-908** InternalServerError | Medium | `.claude/rules/dhan/annexure-enums.md` | retry-with-backoff | yes |
| **DH-909** NetworkError | Medium | `.claude/rules/dhan/annexure-enums.md` | retry-with-backoff | yes |
| **DH-910** Other | Low | `.claude/rules/dhan/annexure-enums.md` | — | yes |
| **DATA-800** InternalServerError | Medium | `.claude/rules/dhan/annexure-enums.md` | retry-with-backoff | yes |
| **DATA-804** InstrumentsExceedLimit | Medium | `.claude/rules/dhan/live-market-feed.md` | — | yes |
| **DATA-805** TooManyConnections | Critical | `.claude/rules/dhan/live-market-feed.md` | STOP ALL 60s (auto) | **no** |
| **DATA-806** NotSubscribed | Medium | `.claude/rules/dhan/api-introduction.md` | — | yes |
| **DATA-807** TokenExpired | High | `.claude/rules/dhan/authentication.md` | trigger refresh (auto) | yes |
| **DATA-808** AuthFailed | Critical | `.claude/rules/dhan/authentication.md` | — | **no** |
| **DATA-809** TokenInvalid | Critical | `.claude/rules/dhan/authentication.md` | — | **no** |
| **DATA-810** ClientIdInvalid | Critical | `.claude/rules/dhan/authentication.md` | — | **no** |
| **DATA-811** InvalidExpiry | Medium | `.claude/rules/dhan/option-chain.md` | check params | yes |
| **DATA-812** InvalidDateFormat | Medium | `.claude/rules/dhan/historical-data.md` | check params | yes |
| **DATA-813** InvalidSecurityId | Medium | `.claude/rules/dhan/instrument-master.md` | `scripts/auto-fix-refresh-instruments.sh` | yes |
| **DATA-814** InvalidRequest | Medium | `.claude/rules/dhan/api-introduction.md` | — | yes |

## How Claude's auto-triage uses this table

1. `.claude/triage/error-rules.yaml` defines the 7 seed rules.
2. For any code not covered by a seed rule, default action = `escalate`.
3. `is_auto_triage_safe()` (from `ErrorCode` enum) returns false for
   every Critical-severity code above — these ALWAYS escalate,
   never auto-action. That's the safety invariant the
   `test_critical_codes_never_auto_triage` unit test enforces.

## Morning ops cheat sheet

| Telegram emoji | Action |
|---|---|
| ✅ `[INFO]` | No action |
| 🟡 `[LOW]` or `[MEDIUM]` | Read summary at `make errors-summary`; no urgency |
| 🟠 `[HIGH]` | Check dashboard; if auto-fix attempted, verify outcome in `data/logs/auto-fix.log` |
| 🔴 `[CRITICAL]` | Halt trading if app hasn't already; read runbook; reply in session if needed |

## When to add a new runbook

If a new ErrorCode variant is added:
1. The `error_code_rule_file_crossref` test enforces a `.claude/rules/*.md` mention.
2. Update this table in the SAME commit that adds the variant.
3. If auto-triage should apply, add a rule to `.claude/triage/error-rules.yaml`.
