# Tick Spill Replay — Error Codes (TICK-SPILL-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §F >
> `zero-loss-guarantee-charter.md` §1 > this file.
> **Companion code:** `crates/storage/src/tick_spill_replay.rs`.
> **Related:** `HOT-PATH-02` (the rescue that created the file),
> `WS-GAP-03` (the flush failure that triggered the rescue).

---

## §0. What the spill tier is for, and the half nobody had exercised

When an ILP flush to QuestDB fails, the writer rescues the buffered rows to a
spill file rather than dropping them. That is the guarantee: **a failed flush
costs disk, not data.**

It has a second half. The file has to be *replayable*. A malformed file is
refused with an HTTP 4xx, and no amount of retrying changes a malformed byte —
so the drain, which stopped the whole round on any failure, retried it forever
and never reached anything behind it.

## §1. TICK-SPILL-01 — a spill file was permanently refused and quarantined

`ErrorCode::TickSpill01FileQuarantined` (`code_str() == "TICK-SPILL-01"`).
**Severity:** High. **Auto-triage safe:** YES — the file is already set aside
and the queue is already moving; the operator decides what to salvage.
**Delivery:** log-sink-only.

**Trigger.** QuestDB answered a replay chunk with a permanent refusal — any
4xx except 408 (timeout) and 429 (rate limit), both of which mean *try again*
and take the retry path. The file is MOVED to `<spill_dir>/quarantine/` and
the drain continues to the next file.

**The incident that created this code (2026-08-25).** After a disk-full halt,
two tick spill files sat on disk:

| File | Size | Contents |
|---|---:|---|
| `ticks-dhan-496566.ilp` | 401 KB | 1,293 lines, **one** of them torn |
| `ticks-dhan-496567.ilp` | 512 MB | **1,662,318 lines, every one intact** |

`tv_tick_spill_replayed_bytes_total` read **0**. `tv_tick_spill_replay_failed_total`
climbed by one every five minutes. The 512 MB file was **never once attempted**,
because the 401 KB file ahead of it was refused every round and the round
stopped there.

Manual recovery filtered the small file to well-formed lines (1,292 of 1,293
survived) and posted both: **all 1,662,318 ticks went in, 9 of 9 chunks
accepted.** Nothing in the big file had ever been wrong.

**Where the tear came from — and it is the uncomfortable part.** The spill
file is appended to. During the disk-full window one append was PARTIAL: a
line was cut mid-field and the next buffer's first line ran into it, producing
the token `lasticks` (a truncated `last_trade_qty` fused to the next
`ticks,segment=`). So the rescue path corrupted its own output under exactly
the condition it exists to survive. **That writer-side defect is NOT fixed by
this code** — this code stops one torn file from taking the backlog with it.
Repairing the append itself is separate and still open.

## §2. Triage

1. `mcp__tickvault-logs__tail_errors` — find `TICK-SPILL-01`. It names the
   quarantined path, the HTTP status and the byte count.
2. Find the actual bad line. Post a slice and read QuestDB's own error, which
   names the line number and the column:
   ```
   curl --data-binary @<quarantined-file> http://127.0.0.1:9000/write
   ```
   A genuine cast error (`FLOAT to column type: LONG`) usually means a torn
   line, not a wrong writer — check whether the line is truncated before
   concluding the schema is wrong.
3. Salvage the rest. Keep only well-formed lines, then re-post. Safe to
   repeat: the `ticks` dedup key carries `capture_seq`, so duplicates collapse.
   ```
   awk '{ n=gsub(/ticks,segment=/,"&");
          if (n==1 && index($0,"ticks,segment=")==1 && $0 ~ / [0-9]{15,}$/) print }' \
     <quarantined-file> > clean.ilp
   split -l 200000 -d clean.ilp part-
   for p in part-*; do curl --data-binary @"$p" http://127.0.0.1:9000/write; done
   ```
   The `n==1` test matters: a fused line starts correctly and ends correctly
   and is still malformed in the middle, so a naive prefix/suffix filter
   passes it through.
4. Confirm: `select count() from ticks` should rise by the kept-line count.

## §3. What this does NOT do

- It does not repair the file. Quarantine MOVES, never deletes — the surviving
  lines are the point, and a rescue tier that destroys what it cannot parse is
  worse than the loss it exists to prevent.
- It does not fix the partial-append tear (§1).
- It does not page. Log-sink-only, and the exemption in
  `error_code_alarm_coverage_guard.rs` records that as a choice with its
  reason, not an oversight.
- A 5xx or a transport error still stops the round, unchanged. That rule is
  correct for a struggling server — pushing a backlog at one is how a bounded
  tick loss becomes an unbounded outage. The fix is only that a *payload*
  problem no longer borrows the *server* problem's behaviour.

## §4. Counters

| Counter | Meaning |
|---|---|
| `tv_tick_spill_replay_quarantined_total` | files permanently refused and set aside |
| `tv_tick_spill_replay_failed_total` | transient failures; the round stopped, file retried |
| `tv_tick_spill_replayed_bytes_total` | bytes QuestDB accepted |

A steady `failed` with a flat `replayed_bytes` was the live signature of the
blocked queue. After this change that shape means a genuinely unhealthy
database; a poison file shows up as `quarantined` instead.
