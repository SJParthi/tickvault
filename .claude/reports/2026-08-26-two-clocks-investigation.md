# The Two Clocks — why `ticks.ts` and `ticks.received_at` disagree

**Date:** 2026-08-26 · **Trigger:** operator report · **Type:** read-only investigation
**Published artifact:** https://claude.ai/code/artifact/5ca9ec5b-e8d0-4b80-a317-01e3cddee490
**Evidence basis:** the RUNNING production box (`i-0c3fe906dad5492fc`, r8g.xlarge,
ap-south-1b, build `ffe2162`), read over SSM → QuestDB REST between 09:40 and
09:47 IST **while the market was open**. Every figure below is a value read from
the live database or the live host. Nothing here is projected.

> **Why this file is Markdown and not the HTML it started as.** The first version
> of this report was committed as `.claude/reports/two-clocks.html` and
> `browser_surface_and_toolchain_guard::tracked_html_is_one_frontend_surface_plus_vendor_docs`
> correctly failed the build: tracked `.html` is allowed only at the operator
> console and under `docs/`, and a new one anywhere else is a new browser surface
> needing a dated operator note in `rust-only-forever-lock-2026-07-19.md`. No such
> note exists, so the file was removed rather than allowlisted — adding my own file
> to `HTML_ALLOWED_FRONTEND` to get my own PR green is exactly the guard-weakening
> those rules forbid. The rendered report lives as the published artifact above.

---

## 1. The question

The operator observed that for **Adani Enterprises** around 09:16 IST, `ticks.ts`
and `ticks.received_at` hold different times, and suspected a mapping or decode
bug. He separately noted that Dhan's own 1-minute chart close matches the price
of the row whose `received_at` closes that minute.

## 2. The answer — not a bug

`ts` and `received_at` are **two different clocks measuring two different events**,
and both are correct.

| Property | `ts` | `received_at` |
|---|---|---|
| Meaning | when NSE matched the trade (Last Traded Time) | when the packet reached our NIC |
| Set by | the exchange, relayed by Dhan | us, from the system clock |
| Resolution | whole seconds — always `.000000` | microseconds |
| Can be in the past? | **yes** — a stock that last traded an hour ago correctly carries that time | no |
| Buckets candles? | **yes** | no |

`row_timestamp_ist_nanos` (`crates/storage/src/tick_persistence.rs`) sets `ts` from
`exchange_timestamp` when it is inside the plausibility band, falling back to
receipt time only for a sentinel. Both values are IST-space, so their difference
needs no offset arithmetic.

### 2.1 Proof the stored candle is correct

The last tick whose LTT falls inside minute 09:16, for `security_id = 25`,
`segment = 'NSE_EQ'`:

| Column | Value |
|---|---|
| `ts` | `2026-08-26T09:16:59.000000` |
| `received_at` | `2026-08-26T09:17:00.176014` |
| `ltp` | **3112.1** |

And `candles_1m` for the 09:16 bucket:

| ts | open | high | low | **close** | volume | tick_count |
|---|---|---|---|---|---|---|
| 09:16:00 | 3107.8 | 3116.6 | 3106.3 | **3112.1** | 17,341 | 67 |

**Exact match, no rounding.**

### 2.2 The operator's second observation, explained

The trade closing minute 09:16 prints at `09:16:59` and lands on our machine at
`09:17:00.176`. So reading down the `received_at` column, minute 09:16's closing
price appears at the *top of minute 09:17*. Dhan's chart and our fold both file it
under 09:16 because **both bucket on trade time, not arrival time.** The operator
was watching the system agree with Dhan, not disagree with it.

## 3. Fast-consumer status — VERIFIED

**Do not read `received_at - ts` as our lag.** It is the sum of two things:
delivery lag **plus** time since that instrument last traded. Only continuously
trading instruments isolate the first.

Measured today, `ts >= 09:15`:

| Segment | Ticks | Mean gap | Max | What the number is |
|---|---|---|---|---|
| **IDX_I** | 452,969 | **506 ms** | 2.97 s | indices recompute every second ⇒ staleness ≈ 0 ⇒ **this is our true delivery lag** |
| NSE_EQ | 1,066,313 | 4.52 s | 387 s | liquid names ~1 s; illiquid ones drag the mean |
| NSE_FNO | 4,199,383 | 63.3 s | 1,839 s | **NOT lag** — a far-OTM strike genuinely last traded 30 min ago |
| ADANIENT (sid 25) | 2,043 | 1.33 s | 6.6 s | min observed 56 ms |

**506 ms on indices — we are a fast consumer.**

### 3.1 The honest limit, which is Dhan's and not ours

Adani's cumulative volume jumped **6,147 shares inside one second** (09:15:42),
delivered as a **single** update carrying one price. Dhan's published architecture
states that slow consumers "catch up with the latest available state", and their
India feed carries **no sequence number and no snapshot-on-subscribe**. It is a
conflated ~1-per-second snapshot. Individual prints inside a second are merged
before they ever reach us and are invisible to every counter we own, in every
configuration.

> **"Not one tick missed" can only honestly mean "not one RECEIVED packet lost".**

Corroborating: for Adani 09:15–09:40 there were 1,660 rows against 979 distinct
LTT values — ~41% of rows are Dhan re-sending an unchanged state.

## 4. Defects found — NOT fixed

None of these is what the operator was looking at. All need his dated ruling before
any code moves (design-first wall + plan-enforcement).

| # | Sev | Finding |
|---|---|---|
| 1 | **CRITICAL** | The **depth ILP flush still runs synchronously on the frame drain** (`flush_depth`, inside `block_in_place`), now with a retry — ceiling ~10 s. The *tick* flush was offloaded in `ffe2162` for exactly this reason; depth was left behind, and a second blocking call survives even in the offloaded path via inline-depth flush. **Found independently by two of the seven audits.** At the open burst the 65,536-frame ring fills in ~5.2 s. This is the one mechanism that can genuinely make us the slow consumer Dhan skips forward. |
| 2 | **CRITICAL** | **We cannot prove we are fast.** The frame's ring dwell (`queued_nanos`) is computed ~5,000×/sec and used only to correct a timestamp, never recorded. `RingByteBudget::resident()` exists with zero production callers. `tv_dhan_ws_lag_ms` exists but is not EMF-selected, so it never leaves the box — it is loopback-only. No CloudWatch signal distinguishes "Dhan was slow" from "we were slow". |
| 3 | **HIGH** | **987,658 rows written today (14.7% of 6,734,195) carry a `ts` from a previous day** — NSE_FNO back to `2026-07-23`, NSE_EQ back to `2026-08-25T15:28`. Correct per row (the instrument has not traded today), but they land as out-of-order writes into **already-closed partitions**, which is the O3 merge pressure behind the 2026-08-25 `CairoException: [28] No space left` that suspended 15 tables. Also: `WHERE ts >= today` silently misses one row in seven. |
| 4 | **HIGH** | A socket that keeps ponging but stops delivering **never trips the 27 s idle watchdog**. Only the silence scan catches it — 60–90 s to log, alarm period **3,600 s**. Separately the entire reconnect family (`tv_dhan_ws_reconnect_total`, `_dial_failed_total`, `_subscribe_failed_total`) is **dashboard-only and cannot page**. |

### 4.1 Secondary findings

- `ConsumeStats.late_count` is written and has **zero production readers** — no
  counter, no EMF, no alarm. Candle ticks discarded as late are invisible.
- The watermark plausibility band spans **2020→2050**. One packet with a future
  LTT force-seals every open bucket across all instruments. `reset_watermark()` —
  the documented self-heal — has **zero production callers**. (Checked live: no
  future-dated rows exist today, `max(ts) = 09:47:16`.)
- `/api/quote/{security_id}` queries without `segment` or `feed`. `security_id 25`
  is **BANKNIFTY in IDX_I and Adani Enterprises in NSE_EQ** — the endpoint returns
  whichever ticked last.
- `UnresolvedReason::SymbolMismatch` is declared and **never constructed** — the
  documented ISIN symbol cross-check does not exist.
- All four derived candle columns are dead: `close_pct_from_prev_day = 0.0` in
  **113,077 of 113,077** rows today.
- A duplicated `exchange_timestamp` band check in `on_tick` is unreachable dead
  code (2026-08-26 merge artifact).

## 5. Why nothing raised this for three days

- The **live-vs-REST cross-verification** — the feed's only ground truth — labelled
  every cash equity as `"INDEX"` when requesting official candles until
  **2026-08-25**. Roughly **86% of its fetches returned nothing** (every one of the
  ~750 stocks, Adani included) while it could still report clean on the few real
  indices. **For the exact days the operator was watching, it was structurally
  incapable of checking Adani.**
- The delivery-lag histogram is bound to `127.0.0.1` and never leaves the box.
- `tv_dhan_feed_ingest_refused_total` — the only signal that ticks are being thrown
  away — is EMF-selected, charted, and **read by no alarm**.
- `/api/debug/cross-verify/latest` returns **404 permanently**; it reads a file
  whose producer was deleted in July.

## 6. Verified healthy

Stated specifically so the good news is as checkable as the bad.

| Check | Result |
|---|---|
| 09:16 candle close vs the tape | **exact match** (3112.1) |
| Sockets connected | **16/16** (15 feed + 1 order-update) |
| WAL-suspended QuestDB tables | **0** |
| Disk | **8% used**, 278 GB free of 300 |
| App CPU throttling | **`nr_throttled = 0`** |
| Future-dated rows | **none** (`max(ts) = 09:47:16`) |
| Legacy `TVW1` WAL segments (would silently upsert ticks away on replay) | **none — 18/18 are `TVW2`** |
| `capture_seq` collision | **impossible on every live path** — proven across same-frame, same-millisecond, two-connection, replay, restart, and index-overflow axes |
| BANKNIFTY / Adani `id=25` collision in the fold | **impossible** — key is `(Feed, security_id, segment_code)`, pinned by a test using this exact pair |
| Instrument join | **ISIN-primary**; symbol-only matching banned and absent |
| Token | valid, 73,060 s remaining |

## 7. Recommended order of work

Nothing applied. Ranked by what each buys.

| # | Change | Why here | Size |
|---|---|---|---|
| 1 | Move the depth flush off the frame drain | The only change that alters whether we lose ticks. Copies a pattern already proven on the tick path. | small |
| 2 | Publish `queued_nanos` as a histogram and `resident()` as a gauge | Both already computed in production. Together they answer "them or us" permanently. | small |
| 3 | EMF-select `tv_dhan_ws_lag_ms` (16 fixed connection series) | Without it the vendor half stays unmeasured. Cardinality already authorized. | small |
| 4 | **Operator ruling:** what should a pre-open stale-trade row be stamped? | Keep the true old trade time and accept the merge cost, or stamp receipt time and lose that truth. A data-meaning decision, not an executor's. | ruling |
| 5 | Alarm the reconnect family; tighten the deaf-socket window | Turns an hour of silence into minutes. | small |
| 6 | Surface `late_count` / `amended_count` | Wiring an existing computed number to an existing counter. | small |

## 8. Method

Seven parallel audits: timestamp lineage, watermark/late-tick sealing, consumer
backpressure, socket resilience, instrument identity, workspace complexity and
Rust-only compliance, and observability coverage. Findings were then checked
against live production data — **two audit findings that the live data disproved
were withdrawn rather than reported**, and one agent's central hypothesis was
refuted by its own investigation and reported as refuted.

## 9. Not claimed

- Whether the cross-verification has ever recorded a non-zero `compared` is
  **Unknown from this repository** — that evidence exists only on the box.
- Ticks Dhan merges away before transmission are invisible to every counter here,
  in every configuration. The daily comparison against Dhan's own official record
  is the only ground truth for that class.
- The lag figures are one session on one day. They are a measurement, not a
  guarantee of future sessions.
