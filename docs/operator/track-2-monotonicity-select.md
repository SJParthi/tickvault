# Track 2 — Volume Cumulative-Semantic Monotonicity Verification (Mon May 4)

> **Authority:** `.claude/plans/active-plan-wave-5-indices-only.md` Items 26 + 29.
> **Created:** 2026-05-01.
> **Audience:** Parthiban (operator). Run at Mon May 4 09:45 IST.
> **Outcome gates:** Wave 5 Item 27 (movers materialised-view DDL) ship/halt
> decision; promotion of the volume guarantee from INCONCLUSIVE to CONFIRMED.

## Why this exists

Cowork's 4-track verification (2026-05-01) found Tracks 1, 3, 4 strongly
suggesting Dhan's `volume` field is cumulative-day, but Track 2 — the only
mathematical proof — was unrunnable in the sandbox because:

1. NSE Labour Day holiday → no live ticks.
2. App boot failed (depth-200 SELF token workaround was in place; retired 2026-05-02 per Dhan Ticket #5610706).
3. QuestDB not bound in the Cowork sandbox.

The Mon May 4 market open is the next opportunity. This runbook gives the
exact SQL + verdict logic so the operator can execute it in <5 minutes.

## Pre-conditions (verify before running the SELECT)

| Check | Expected | How |
|---|---|---|
| App alive at 09:15:30 IST | `MarketOpenStreamingConfirmation` Telegram fired | check Telegram |
| Tick rate non-trivial by 09:45 IST | ≥ 50 K ticks captured | `select count(*) from ticks where ts > '2026-05-04T09:15:00.000000Z';` |

If any pre-condition fails: HALT, fix the underlying issue, retry next
trading day.

## The 5-series SELECT (run at 09:45 IST)

### Series 1 — NIFTY index value

```sql
SELECT 'series_1_nifty' AS series, ts, volume
FROM ticks
WHERE security_id = 13 AND segment = 'IDX_I'
  AND ts > '2026-05-04T09:15:00.000000Z'::timestamp
  AND ts < '2026-05-04T09:45:00.000000Z'::timestamp
ORDER BY ts ASC LIMIT 500;
```

### Series 2 — BANKNIFTY index value

```sql
SELECT 'series_2_banknifty' AS series, ts, volume
FROM ticks
WHERE security_id = 25 AND segment = 'IDX_I'
  AND ts > '2026-05-04T09:15:00.000000Z'::timestamp
  AND ts < '2026-05-04T09:45:00.000000Z'::timestamp
ORDER BY ts ASC LIMIT 500;
```

### Series 3-5 — top-3 most-active option contracts

First identify the candidates:

```sql
SELECT security_id, segment, count(*) AS tick_count
FROM ticks
WHERE ts > '2026-05-04T09:15:00.000000Z'::timestamp
  AND segment IN ('NSE_FNO', 'BSE_FNO')
GROUP BY security_id, segment
ORDER BY tick_count DESC
LIMIT 3;
```

Then for each `(security_id, segment)` pair returned, run:

```sql
SELECT 'series_<N>_<sid>' AS series, ts, volume
FROM ticks
WHERE security_id = <SID>
  AND segment = '<SEG>'
  AND ts > '2026-05-04T09:15:00.000000Z'::timestamp
  AND ts < '2026-05-04T09:45:00.000000Z'::timestamp
ORDER BY ts ASC LIMIT 500;
```

## Verdict logic per series

For each series, a row violates monotonicity if:

```
volume[i] < volume[i-1]   for any i > 0
```

QuestDB shorthand using `lag()`:

```sql
WITH s AS (
  SELECT ts, volume,
         lag(volume) OVER (ORDER BY ts) AS prev_volume
  FROM ticks
  WHERE security_id = <SID> AND segment = '<SEG>'
    AND ts > '2026-05-04T09:15:00.000000Z'::timestamp
    AND ts < '2026-05-04T09:45:00.000000Z'::timestamp
)
SELECT count(*) AS violations
FROM s WHERE prev_volume IS NOT NULL AND volume < prev_volume;
```

`violations = 0` ⇒ **series passes monotonicity** (consistent with cumulative).
`violations > 0` ⇒ **series REFUTED**.

## Decision matrix

| Result | Action | Item 27 |
|---|---|---|
| 5 / 5 series pass | Promote volume guarantee from INCONCLUSIVE → **CONFIRMED CUMULATIVE**; tick Item 26 L1 done. | **SHIP** as designed |
| 1+ series with violations | Capture the violating rows verbatim, attach to Item 26 L3 Dhan support email. | **HALT** until Dhan reply |
| Insufficient ticks (any series < 50 rows) | Insufficient data ≠ refutation. Re-run after another 30 min, or pick a more active contract. | **WAIT** for re-run |

## Recording the outcome

After running, write the result to a fresh file under
`.claude/plans/research/track-2-result-2026-05-04.md`:

```markdown
# Track 2 Monotonicity Verdict — 2026-05-04 09:45 IST

| Series | security_id | segment | rows | violations | verdict |
|---|---|---|---|---|---|
| 1 | 13 | IDX_I | <N> | <V> | PASS / REFUTED / INSUFFICIENT |
| 2 | 25 | IDX_I | ... | ... | ... |
| 3 | <SID> | <SEG> | ... | ... | ... |
| 4 | ... | ... | ... | ... | ... |
| 5 | ... | ... | ... | ... | ... |

**Overall verdict:** CONFIRMED CUMULATIVE | REFUTED | INSUFFICIENT

**Item 27 decision:** SHIP | HALT | WAIT
```

Commit + push. The next Claude session that picks up Item 27 reads this
file first.

## If REFUTED — Item 26 L3 Dhan support email

Use `docs/dhan-support/2026-05-01-volume-semantic-clarification.md` as
the template (already drafted PR #414, awaiting operator Gmail send).
Append the violating rows table from this run. Include:

- Verbatim QuestDB rows (`ts`, `volume`, `prev_volume`).
- Wire-binary parser citation (`crates/core/src/parser/quote.rs:40` —
  bytes 22-25 u32 LE).
- Question: "Is the `volume` field at bytes 22-25 of the Quote packet
  a cumulative-day total, or per-tick incremental? Our 30-minute
  window observed N rows where `volume[i] < volume[i-1]`. Could
  Dhan confirm semantic + sample raw bytes?"

Send via Gmail with GitHub link to the support file.

## What's covered by ratchets even without Track 2

- Item 28 (candles cascade volume bug fix) — already shipped via PR #415.
  Endogenous bug; fix valid regardless of Dhan-side semantic.
- Item 13 ratchets (PREVCLOSE-03) pin the segment-aware feed-mode matrix.
- Item 26 L2 NSE bhavcopy cross-check — daily semantic + accuracy check
  (still useful with INCONCLUSIVE Track 2 verdict).

So Track 2 strictly gates Item 27 (mat view DDL) only. Everything else
in Wave 5 ships independently.

## Cross-references

- `.claude/plans/active-plan-wave-5-indices-only.md` Items 26 + 27 + 28 + 29
- `docs/dhan-support/2026-05-01-volume-semantic-clarification.md` (L3 ticket draft)
- `crates/core/src/parser/quote.rs:40` (parser citation)
- `crates/common/src/tick_types.rs:31` (volume field semantic comment)
