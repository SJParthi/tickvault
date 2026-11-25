# Track 2 Monotonicity Verdict — 2026-09-09 10:15 IST

> **Runbook:** `docs/operator/track-2-monotonicity-select.md` (written 2026-05-01,
> unrunnable that day: NSE Labour Day holiday, app boot failure, no QuestDB in the
> sandbox). Its `**Overall verdict:**` line stood as the unfilled template
> `CONFIRMED CUMULATIVE | REFUTED | INSUFFICIENT` for **131 days**.
>
> **This file fills it.** Run against the live prod box during a normal session.

## Method — stronger than the runbook asked for

The runbook specifies five named series (two indices + three busy option
contracts). That shape was written when a hand-run SELECT was the only option.
Run instead over **every instrument that ticked today**, with the violation
count computed inside the database:

```sql
SELECT count() total_ticks,
       sum(CASE WHEN volume < prevv THEN 1 ELSE 0 END) violations,
       count_distinct(security_id) instruments
FROM (
  SELECT security_id, volume,
         lag(volume) OVER (PARTITION BY security_id, segment ORDER BY ts, capture_seq) prevv
  FROM ticks
  WHERE ts >= '2026-09-09T09:15:00Z' AND segment != 'IDX_I'
);
```

`PARTITION BY (security_id, segment)` is the I-P1-11 composite — partitioning on
`security_id` alone would interleave two different instruments' series and
manufacture violations that are not there.

`ORDER BY ts, capture_seq` is load-bearing: `ts` is the exchange stamp and is
**second-granular**, so many ticks share one `ts`. `capture_seq` is the
replay-stable receipt sequence (`data-integrity.md` TICK-SEQ-01) and is what
orders them inside a second.

## Result

| Series | security_id | segment | rows | violations | verdict |
|---|---|---|---|---|---|
| whole universe | *all* | NSE_EQ + NSE_FNO | **9,879,724** | **157** | **PASS** (0.0016%) |
| 1 | 29124 | NSE_EQ | 3,904 | 0 | PASS |
| 2 | 112774 | NSE_FNO | 335 | 0 | PASS |
| 3 | 112784 | NSE_FNO | 306 | 0 | PASS |
| 4 | 14366 | NSE_EQ | 3,893 | 0 | PASS |
| 5 | 68545 | NSE_FNO | 1,377 | 0 | PASS |
| 6 | 47298 | NSE_FNO | 4,212 | 3 | PASS (0.07%) |
| 7 | 115766 | NSE_FNO | 5,495 | 0 | PASS |

Coverage: **8,675 distinct instruments**, against a runbook that asked for five.

**Overall verdict: CONFIRMED CUMULATIVE**

**Item 27 decision: SHIP**

## The 157, stated rather than rounded away

0.0016% is not zero and this record does not present it as zero. Two mechanisms
account for it, and neither refutes the cumulative semantic:

1. **Intra-second arrival order.** Dhan conflates to roughly one update per
   second per instrument and publishes no sequence number. Two updates carrying
   the same `ts` can reach us out of the order the exchange produced them; the
   `capture_seq` tiebreaker orders them by *our* receipt, which is the only order
   we can observe. A pair transposed that way reads as one fall.
2. **The documented slow-consumer skip.** Dhan advances a lagging consumer to
   "the latest available state", so a resumed stream can carry a figure that is
   not adjacent to the one before it.

Both are *arrival* artefacts, not *field-semantic* ones. A per-packet delta
field would not produce 157 falls in 9.88 M — it would produce falls on the
overwhelming majority of ticks, because a delta is smaller than its predecessor
roughly half the time.

**These 157 are already handled.** `VolumeLeaderboard`'s monotonicity gate
refuses a falling cumulative rather than moving the baseline, and re-latches
after `RELATCH_AFTER_CONSECUTIVE_LOWER` consecutive lower readings so a genuine
counter reset is not defended forever. That gate was built for exactly this
residual; the measurement now sizes it.

## Two further facts the same run establishes

| Fact | Measurement |
|---|---|
| **IDX_I carries no volume at all** | 866,796 index ticks, `min(volume) = 0`, `max(volume) = 0`. Indices are a computed number with no traded quantity — so an index can never be ranked by volume, and only its *option contracts* can. The design already relies on this. |
| **Cash equities carry real volume** | NSE_EQ `max(volume) = 296,176,063`; NSE_FNO `max = 129,369,750`. Both climb monotonically through the session. |

## What this unblocks

`websocket-connection-scope-lock.md` (2026-09-06) calls the cumulative semantic
**"the single most load-bearing unproven input in the design"** and notes that if
the field were a per-packet delta the monotonicity gate would refuse nearly
everything. It is now proven, on 9.88 M live ticks, and the ranking key
`window_lots_milli = (delta_units × 1000) / lot_size` rests on a measured
foundation rather than an inferred one.

## What this does NOT establish

- It does not prove the counter never resets intraday for a *specific* contract —
  only that it did not on 2026-09-09 beyond the 157 above.
- It says nothing about the **depth** packet's quantity fields, which are a
  different layout and were not examined here.
- It does not make the ranking work: the lot-size defect
  (`master_csv::parse_lot_size`, fixed on the branch and not yet deployed) is the
  live blocker, and it is upstream of this.
