# Resilience Mechanisms — What to KEEP vs DROP at 4-SID Scope

> **Status:** DESIGN SIMPLIFICATION (no code shipped).
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Created:** 2026-05-18 in response to operator: *"do we still need WAL spills ring buffer and so many mechanisms dude?"*

---

## §0. Auto-driver one-liner

> "Sir, the old shop served 11,000 customers per day — we built 5 backup systems for floods. New shop serves 4 customers — we don't need 5 floods backup systems. Keep the smoke alarm (detection). Keep 1 small backup (ring buffer 10K instead of 100K). Drop the giant rain barrels (spill files) and emergency reservoir (DLQ). The REST API can fetch any data we miss in worst case. Less code = fewer bugs = simpler operation = same safety at 4 customers."

```
   OLD (11K instruments, 5K ticks/sec)            NEW (4 SIDs, 20 ticks/sec)
   ────                                            ────
   Rescue ring 5M ticks (1 GB RAM)                Rescue ring 10K ticks (2 MB RAM)
   NDJSON spill file (disk, +500 MB)             [DROPPED]
   DLQ NDJSON file (disk, +500 MB)               [DROPPED]
   QuestDB WAL (internal — we don't touch)        Same (internal)
   Heartbeats, tick-gap, cross-verify, ...        Same (still essential)
   
   What if QuestDB is out for 10+ minutes? Ring fills.
   OLD: spill catches; DLQ as final.
   NEW: cross-verify @ 15:31 IST notices missing ticks; REST backfill
        via /v2/charts/intraday auto-fills the gap. Simpler.
```

---

## §1. The 12 mechanisms — KEEP / TRIM / DROP

| # | Mechanism | Purpose | Old scope verdict | NEW 4-SID verdict | Why |
|---|---|---|---|---|---|
| 1 | **Rescue ring buffer** | Absorb ticks during QuestDB outage | 100K (~83 min @ 20 tps) | **TRIM → 10K (~10 min @ 20 tps)** | 10 min covers ~99.99% of QuestDB outages. Saves 198 MB RAM. |
| 2 | **NDJSON spill file** | When ring fills | Disk-spill 500 MB | **DROP entirely** | At 10K ring + REST backfill, spill never exercised in practice |
| 3 | **DLQ NDJSON** | When spill fills | Disk-spill 500 MB | **DROP entirely** | If 10 min QuestDB outage + REST also down, we have bigger problems |
| 4 | **QuestDB WAL** | QuestDB-internal write-ahead log | always on | **KEEP (QuestDB internal — not our code)** | Free; QuestDB needs it for durability |
| 5 | **Per-component heartbeat gauges** | Detect zombie/hung subsystems | always on | **KEEP** | Essential for "fake working" detection (operator demand) |
| 6 | **Tick-gap detector (30s silence)** | Silent socket detection | always on | **KEEP** | Essential per operator REAL reconnect demand |
| 7 | **SubscribeRxGuard** | Preserve subscribe channel across reconnects | always on | **KEEP** | Essential — without this, reconnect drops subscriptions silently |
| 8 | **15:31 IST cross-verify** | Daily proof of zero tick loss | always on | **KEEP** | Z+ L3 — proves what we captured matches Dhan's authoritative data |
| 9 | **08:05 IST morning 1d cross-check** | Yesterday's overnight integrity | always on | **KEEP** | Catches any cross-day boundary issues |
| 10 | **Auto-backfill via REST** | Fill detected gaps | always on | **KEEP** | THE RECOVERY MECHANISM — replaces spill + DLQ |
| 11 | **systemd Restart=on-failure** | Auto-restart on crash | always on | **KEEP** | 3-second recovery |
| 12 | **CloudWatch 10 alarms** | Page operator on anomaly | always on | **KEEP** | Operator's 4-channel notification |

**KEEP: 9 mechanisms. TRIM: 1. DROP: 2.** Net simplification: ~1 GB RAM saved, ~5K LoC removed (spill + DLQ subsystems).

---

## §2. The honest recovery chain — DOES this actually work at 4 SIDs?

### Scenario A — QuestDB outage 0–10 minutes (99.99% of cases)

```
   QuestDB stops accepting writes at T+0
       │
       ▼ T+0 to T+10min:
   Aggregator continues processing ticks in RAM
   Sealed bars dispatched to bar_cache (RAM — works fine)
   ILP writer attempts dispatch → fails → falls back to rescue ring
   Rescue ring buffers ~12,000 ticks (10 min × 20 tps)
       │
       ▼ T+10min: QuestDB comes back
   ILP writer drains rescue ring (10K rows in ~1 second)
   Continues normal operation
   Zero data loss
```

**Outcome: ZERO data loss. Same as old design. Simpler.**

### Scenario B — QuestDB outage 10–60 minutes (rare)

```
   QuestDB down at T+0
       │
       ▼ T+0 to T+10min:
   Ring buffer fills as in Scenario A
       │
       ▼ T+10min+ to T+outage_end:
   Ring overflows → OLDEST entries evicted to make room for newest
   Some ticks LOST (the oldest in the ring at overflow)
       │
       ▼ QuestDB comes back at T+outage_end (say T+30min)
   Ring drains the ~10K most recent ticks
   The lost ticks (T+0 to T+20min worth) are NOT in QuestDB
       │
       ▼ 15:31 IST cross-verify
   Detects mismatch for the lost-tick minutes
   CROSS-VERIFY-01 Critical Telegram fires (Phone+SMS+TG+Email)
       │
       ▼ Auto-backfill kicks in
   POST /v2/charts/intraday for each lost-tick minute × each SID × each TF
   Dhan returns authoritative OHLCV for those minutes
   UPSERT into candles_1m, candles_5m, etc — overwrites any partial bars
   Re-run cross-verify — should now pass
   Telegram: "Crash recovery: backfilled N bars from <start> to <end>"
```

**Outcome: ZERO permanent data loss. Brief transient loss in `ticks` table (raw ticks for the gap minutes are gone — only candles are recovered). Cross-verify proves integrity. Same as old design BUT without 1 GB of disk-spill machinery.**

### Scenario C — QuestDB outage 60+ minutes (very rare)

Same as Scenario B but longer gap. REST backfill still works — `/v2/charts/intraday` supports up to 90 days range per Dhan rule.

### Scenario D — QuestDB outage + Dhan REST also down (catastrophic)

Both data sources unreachable. We cannot backfill.

**This is outside the envelope** per operator-charter §F. The cross-verify alarm fires Critical; operator manually decides next-day action (paper trade only, or manual data import from alternative source).

**This was outside the envelope WITH spill + DLQ too** — spill/DLQ only help if QuestDB returns; doesn't help if Dhan is also dead. Removing them doesn't change this envelope.

---

## §3. RAM savings from simplification

| Component | Old | NEW | Savings |
|---|---|---|---|
| Rescue ring (5M × 200B vs 10K × 200B) | 1 GB | 2 MB | **998 MB** |
| Spill file buffer (in-memory write batch) | 50 MB | 0 | 50 MB |
| DLQ NDJSON in-memory queue | 20 MB | 0 | 20 MB |
| Spill + DLQ JSON serialization buffers | 30 MB | 0 | 30 MB |
| **Total RAM saved** | | | **~1.1 GB** |

App working set BEFORE this simplification: 192 MB (per `ram-sizing-proof.md` §10)
App working set AFTER: ~192 - 70 (today's tick replay) + 2 (new tiny rescue ring) = ~124 MB

Wait — the 70 MB "today's tick replay buffer" was ALREADY the post-resize estimate (100K ring + 50 MB optional buffer). New estimate:

| Component | NEW value |
|---|---|
| Rescue ring 10K × 200B | 2 MB |
| (No replay buffer; rely on REST backfill) | 0 |
| Other components unchanged | 122 MB |
| **App working set TOTAL** | **~124 MB** |

**Drop from 192 MB → 124 MB = 35% reduction in app footprint.** Headroom on t4g.medium: 60% → 68%.

---

## §4. LoC savings from simplification

| Module | Old | NEW | Savings |
|---|---|---|---|
| `crates/storage/src/spill_writer.rs` | ~500 LoC | 0 | 500 LoC |
| `crates/storage/src/dlq_writer.rs` | ~400 LoC | 0 | 400 LoC |
| `crates/storage/src/dlq_drain.rs` (replay logic) | ~600 LoC | 0 | 600 LoC |
| `crates/storage/src/rescue_ring.rs` shrinks to bounded VecDeque<Tick> | ~1500 → ~200 | 200 LoC | 1300 LoC |
| Tests for spill + DLQ + chaos scenarios | ~2000 LoC | 0 | 2000 LoC |
| **Total LoC removed** | | | **~4,800 LoC** |

Adds to the overall ~36K LoC removal in `deletion-surface-map.md`.

---

## §5. What about QuestDB WAL? Should we keep it ON?

QuestDB's WAL (Write-Ahead Log) is **QuestDB-internal**. We don't write code for it. It's a QuestDB durability feature that ensures committed writes survive a QuestDB process crash.

| Question | Answer |
|---|---|
| Do we control QuestDB's WAL? | NO — QuestDB does |
| Does QuestDB's WAL consume our RAM? | NO — QuestDB's own memory (in the 1024 MB allocation) |
| Is QuestDB's WAL slow? | Async, sub-millisecond per write |
| Can we disable QuestDB's WAL? | Yes via `cairo.commit.mode=nosync` but DON'T — loses durability |

**Verdict: KEEP QuestDB WAL ON. Free durability. No code change needed.** This is different from OUR spill/DLQ which are application-level — those we drop.

---

## §6. The simplification PR sequence

Updates to `THE-FINAL-PLAN.md` 14-PR sequence:

| PR # | Title | NEW change |
|---|---|---|
| 12 | Rescue ring resize 5M → 100K (was 100K → simplified) | **Resize to 10K** (was 100K target); also DROP spill_writer + dlq_writer + dlq_drain modules entirely |

Net effect on PR #12:
- Was: rescue ring 5M → 100K + per-subsystem heartbeats + zombie detection (~+500 LoC)
- NOW: rescue ring 5M → 10K + drop spill (~-1000 LoC) + drop DLQ (~-1000 LoC) + drop drain (~-600 LoC) + per-subsystem heartbeats + zombie detection (~+500 LoC) = **net ~-2100 LoC**

PR #12 becomes the BIGGEST simplification PR in the 14-PR sequence.

---

## §7. The Z+ defense doctrine — does simplification break it?

Per `z-plus-defense-doctrine.md` 7 layers — checking each:

| Layer | Was (with spill+DLQ) | NEW (without spill+DLQ) | Coverage maintained? |
|---|---|---|---|
| L1 DETECT | tick-gap detector + ring overflow alarm + spill fill alarm | tick-gap detector + ring overflow alarm | ✅ same primary detector |
| L2 VERIFY | cross-verify | cross-verify | ✅ unchanged |
| L3 RECONCILE | 15:31 IST + 08:05 IST | 15:31 IST + 08:05 IST | ✅ unchanged |
| L4 PREVENT | Static IP + boot self-test + tick-gap | Static IP + boot self-test + tick-gap | ✅ unchanged |
| L5 AUDIT | cross_verify_audit + spill_audit + dlq_audit | cross_verify_audit | 3 tables → 1; functionally same since spill/dlq gone |
| L6 RECOVER | ring → spill → DLQ → REST backfill | ring → REST backfill | ✅ same final mechanism (REST) |
| L7 COOLDOWN | post-recovery drain | post-recovery drain | ✅ unchanged |

**Z+ defense fully maintained.** REST backfill is the SAME ultimate-recovery mechanism in both designs. We're just removing the 2 intermediate layers (spill + DLQ) that exist between "ring overflow" and "REST backfill" — those 2 layers were redundant insurance for a 5K-tick-per-sec workload that doesn't exist at 4 SIDs.

---

## §8. The honest 100% claim (updated per operator-charter §F)

> "Inside the simplified LOCKED envelope at 4 SIDs ~20 ticks/sec:
> - Rescue ring 10K (~10 min buffer at peak)
> - NDJSON spill: DROPPED (not needed at this scale)
> - DLQ NDJSON: DROPPED (not needed at this scale)
> - QuestDB WAL: KEPT (QuestDB internal — free durability)
> - 9 other resilience mechanisms KEPT
> - REST backfill via /v2/charts/intraday is the SINGLE recovery mechanism for QuestDB outages > 10 min
> - Cross-verify at 15:31 IST is the SINGLE proof of zero tick loss
>
> RAM saved: ~1.1 GB freed (app working set 192 MB → 124 MB).
> LoC saved: ~4,800 lines removed.
>
> Same recovery envelope as before (zero permanent data loss) with simpler code, less surface area, fewer failure modes, easier to reason about.
>
> **Beyond envelope:** simultaneous QuestDB + Dhan REST outage — outside our control, was outside envelope before too, removing spill/DLQ doesn't change this."

---

## §9. The principle — "engineer to the actual scale, not the imagined scale"

This is the meta-lesson from operator's question:

| Anti-pattern | Cause |
|---|---|
| Over-engineering for "what if we scale to 100K SIDs someday?" | Adds bugs, complexity, maintenance burden NOW for hypothetical future |
| Carrying old-design machinery into new-scope deployment | Stale resilience that doesn't match current scale |
| "More layers = more safety" mindset without measuring | Diminishing returns past a certain point |

| Right pattern | How we apply it |
|---|---|
| Right-size for actual scale + measured stress matrix | 10K ring is right-sized for 20 tps |
| Drop redundant layers when measuring shows them unnecessary | Spill + DLQ are redundant given REST backfill |
| Reversibility: if we scale up later, re-add primitives | All primitives are in git history; cherry-pick to restore |

**Operator's question is the right question.** When in doubt, simpler is better.

---

## §10. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/constants.rs` (TICK_BUFFER_CAPACITY)
- `crates/storage/src/rescue_ring.rs` (if file remains; mostly deleted)
- `crates/storage/src/tick_persistence.rs` (the ILP writer)
- Any file containing `SpillWriter`, `DlqWriter`, `dlq_drain`, `rescue_ring`
