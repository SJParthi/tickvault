# Capacity + Failure-Mode + Cost Audit — Movers 22-TF Design

**Branch:** main (post #408 merge)
**Date:** 2026-04-28
**Agent:** Explore (read-only)

## Executive Summary — DO NOT MERGE without addressing 6 gaps

The proposed design has **8 risks** identified, **6 unmitigated**, and **3 unverified claims**. Capacity math is sound but several throughput claims and failure-mode mitigations are aspirational, not yet implemented.

## Risk Matrix — 8 risks identified

| # | Failure mode | Severity | Current mitigation | Gap | Impact |
|---|---|---|---|---|---|
| 1 | QuestDB ILP TCP disconnect mid-snapshot | HIGH | Rescue ring (for ticks only) | No movers-specific rescue ring; design assumes but doesn't name it | Rows silently dropped if QuestDB reconnects late |
| 2 | mpsc(8192) saturation under 24K-row burst | HIGH | Drop-oldest fallback | No backpressure; no SLA on drop rate; no alert threshold | Snapshots missed silently; operator unaware until anomaly |
| 3 | Scheduler clock drift > 100ms | MEDIUM | tokio::interval Skip behavior | No drift detection or alerting; metric spikes but threshold undefined | One timeframe misses snapshot; lag metric shows it but no runbook |
| 4 | 22 tasks all panic | HIGH | Pool supervisor (referenced but not named) | No explicit supervisor; panic hook not wired in design | Silent task death; 1-5 timeframes stop; operator unaware |
| 5 | IDX_I prev_close missing for new index mid-day | MEDIUM | previous_close table (bootstrap only) | No fallback; new index post-boot = NaN prev_close = invalid change_pct | Invalid change_pct; query filtering required |
| 6 | F&O contract expires intraday | MEDIUM | No explicit lifecycle | Movers tables accumulate dead instruments; no status column | Dashboard shows dead contracts; manual filtering required |
| 7 | **Movers tables not added to partition manager** | **HIGH** | Partition manager exists for other tables | **Movers NOT in `HOUR_PARTITIONED_TABLES` list (`partition_manager.rs:30-42`)** | EBS fills to 100GB in ~90d; writes fail; emergency incident |
| 8 | SEBI retention classification for movers unclear | MEDIUM | S3 lifecycle defined for audit tables only | Movers NOT addressed; assumed exempt but not documented | If movers ARE 5y-retained, cost/lifecycle not budgeted |

## Unverified Claims — 3 require proof before merge

| Claim | Source | Status |
|---|---|---|
| "QuestDB ILP sustained throughput >1M rows/sec verified in chaos tests" | `active-plan-movers-22tf-redesign.md:222` | **UNSUBSTANTIATED** — no chaos test measures this |
| "Single writer for all 22 tables via MoversWriter22" | `active-plan:79` | DRAFT — not implemented; must verify single TCP handles 86K writes/sec burst |
| "Pool supervisor re-spawns dead tasks within 5s" | `active-plan:112` | NOT IMPLEMENTED — needs naming + wiring |

## Capacity Math — VERIFIED ✓

| Metric | Claim | Verified |
|---|---|---|
| Daily row volume | ~117M rows/day across 67 tables | ✓ Math sound |
| 90-day storage | ~17GB | ✓ Math sound |
| 100GB EBS hot tier | Per `aws-budget.md:14` | ✓ Fits IF partition manager runs |
| Network bandwidth | ~4.4 MB/sec sustained, 15.5 MB/sec peak | ✓ Trivial on loopback |
| Memory footprint | ~18.4 MB total | ✓ Within 1GB app budget |

## Peak Write Rate — DANGER ZONE

| Time | Tasks firing | Aligned writes |
|---|---|---|
| Every 1s | movers_1s | 24,324 |
| Every 5s | movers_5s adds | +4,865 |
| Every 60s aligned | All 22 tasks fire same wall-clock | **86,000+ writes/sec** |

QuestDB ILP `QDB_LINE_TCP_NET_ACTIVE_CONNECTION_LIMIT: 20` (`docker-compose.yml:82`). Single-writer design serializes all 22 timeframes through ONE TCP connection. mpsc(8192) saturates in <100ms.

## File:Line Citations

- `active-plan-movers-22tf-redesign.md:56-67, 79, 93, 110-113, 222, 226`
- `aws-budget.md:14, 36-46, 61-64`
- `crates/storage/src/partition_manager.rs:30-42`
- `deploy/aws/s3-lifecycle-audit-tables.json:1-71` (no movers entry)
- `docker-compose.yml:6, 82, 74, 85`
- `crates/storage/tests/chaos_questdb_*.rs` (18 files; none measures throughput)

## Recommendation

**DO NOT MERGE** until:
1. Chaos test proves ">1M rows/sec sustained" on c7i.xlarge
2. Rescue ring for movers explicitly designed + added
3. Partition manager updated to include all 22 movers_{T} tables
4. F&O expiry lifecycle (status column + UPDATE on expiry, or filtering doc)
5. SEBI retention classification documented (5y or 90d+Glacier)
6. Pool supervisor for 22 tasks implemented + chaos-tested
7. Drop rate SLA defined (e.g., max 0.1% missed snapshots) + metric + alert
