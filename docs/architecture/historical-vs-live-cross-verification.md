# Historical vs Live Cross-Verification — Complete Reference

> **Scope:** Post-market validation that the live WebSocket-sourced candle
> data (materialized views `candles_1m`/`5m`/`15m`/`1h`/`1d`) matches the
> historical REST-API candle data (`historical_candles` table) for the same
> trading session.
>
> **Authority chain:** `CLAUDE.md` > this file > defaults.
> **Code:** `crates/core/src/historical/cross_verify.rs`
> **Rule:** `.claude/rules/project/historical-candles-cross-verify.md`
> **Dhan ref:** `docs/dhan-ref/05-historical-data.md`

---

## 1. What the cross-verification actually does

At the end of every trading day (post 15:30 IST), the app:

1. Fetches historical OHLCV candles from Dhan REST API for the full session.
2. Reads the corresponding candles from QuestDB materialized views (built
   from live WebSocket ticks).
3. Compares them cell-by-cell for every `(security_id, timeframe, ts)` tuple.
4. Emits a Telegram verdict: `CROSS-MATCH OK` / `CROSS-MATCH FAILED` /
   `CROSS-MATCH SKIPPED`.

Current implementation in `cross_match_historical_vs_live()`
(`cross_verify.rs:1347`).

### 1.1 Scope

| Instrument type | Cross-verified? | Reason |
|---|---|---|
| NSE indices (IDX_I, 28 instruments) | ✅ Yes | Dhan REST serves historical for indices |
| NSE stock equities (NSE_EQ, 213 instruments) | ✅ Yes | Dhan REST serves historical for stocks |
| F&O derivatives (NSE_FNO, ~25k contracts) | ❌ **NO** | Dhan REST `/charts/historical` does not support F&O |

**This means ~97% of our universe is NOT cross-verified today.** See §5.1.

### 1.2 Pass criteria (6 conditions, ALL required)

As of PR #341 (2026-04-24) the pass check is:

```
total_mismatches       == 0
total_missing_live     == 0
total_missing_historical == 0
total_compared         > 0
missing_views_count    == 0
coverage_pct           >= CROSS_MATCH_MIN_COVERAGE_PCT  (= 90)
```

If any condition fails → `passed = false` → routing to `Failed` or
`Skipped` based on specifics.

### 1.3 Telegram variants

| Event | When | Severity |
|---|---|---|
| `CandleCrossMatchPassed` | All 6 pass criteria met | Low (green) |
| `CandleCrossMatchFailed` | `mismatches > 0` or `missing_live > 0` | High (red) |
| `CandleCrossMatchSkipped` | `!live_candles_present`, `candles_compared == 0`, or `coverage_pct < 90` | Low (amber — NOT a pass, NOT a fail) |

---

## 2. Fixed bugs (shipped)

### 2.1 False-OK on mid-session boot (PR #341, 2026-04-24)

**Symptom:** Operator booted fresh clone at 14:54 IST. Cross-match ran at
15:47 IST. Live MVs captured only 36 min (out of 375 min session).
`total_mismatches = 0 && total_missing_live = 0 && total_compared > 0`
returned true. Telegram fired **"✅ CROSS-MATCH OK / All OHLCV values
match bit-for-bit"** despite ~10% actual coverage.

**Root cause:** The LEFT JOIN detail query used `AND (m.open IS NULL OR
...)` to catch NULL-live rows, but in practice didn't flag every missing
row because of how the join materialized. `total_missing_live` stayed 0.

**Fix:** Defense-in-depth `coverage_pct` computed from
`(total_live_candles * 100) / total_compared` where `total_live_candles`
comes from a direct count query (not the LEFT JOIN). Pass requires
`coverage_pct >= 90`.

### 2.2 Zero-fetched false OK (PR #342, 2026-04-24)

**Symptom:** `HistoricalFetchComplete` fired "OK / Fetched: 0 / Candles: 0"
whenever `instruments_failed == 0`, even when nothing actually fetched.

**Fix:** New `zero_fetched_degenerate = (instruments_fetched == 0 &&
total_candles == 0)` routes to `HistoricalFetchFailed` with synthesized
reason `"zero_fetched_zero_candles"`.

---

## 3. Fixed bugs — display-layer (pending)

### 3.1 Telegram table column labels are misleading

`events.rs:731` renders a per-TF table with columns labeled **"Cells | Gaps
| Diffs"**:

```
TF   │  Cells   │ Gaps │ Diffs
 1m  │    0     │  0   │   0
 5m  │    0     │  0   │   0
```

Problems:
1. The "Cells" column actually shows `per_timeframe_mismatches` (mismatch
   count per TF), NOT candles-compared cells.
2. "Gaps" and "Diffs" columns are hardcoded to `0` at `events.rs:735`.

**This is documentation-misleading but logic-correct** because the table
is only rendered on the Passed path where all three are provably zero.
Still should fix for consistency.

---

## 4. Current implementation details

### 4.1 Query structure

```sql
-- Per-timeframe count query (line ~1478)
SELECT count() FROM historical_candles h
LEFT JOIN candles_1m m
  ON h.security_id = m.security_id AND h.ts = m.ts AND h.segment = m.segment
WHERE h.timeframe = '1m' AND <ts_window> AND <scope_idx_or_nse_eq>

-- Per-TF live count (line ~1465)
SELECT count() FROM candles_1m
WHERE <ts_window> AND <scope_idx_or_nse_eq>

-- Detail query — returns mismatches + missing-live rows (line ~1501)
SELECT h.security_id, h.segment, h.ts,
       h.open, h.high, h.low, h.close, h.volume,
       m.open, m.high, m.low, m.close, m.volume,
       h.open_interest, m.open_interest
FROM historical_candles h
LEFT JOIN candles_1m m ON ...
WHERE h.timeframe = '1m' AND <ts_window> AND <scope>
  AND (m.open IS NULL
       OR abs(h.open - m.open) > 0
       OR ... )

-- Missing-historical query (line ~1611)
SELECT m.security_id, ... FROM candles_1m m
LEFT JOIN historical_candles h ON ...
WHERE <ts_window> AND <scope> AND h.security_id IS NULL
```

### 4.2 Timeframes compared

```rust
const CROSS_MATCH_TIMEFRAMES: &[(&str, &str)] = &[
    ("1m",  "candles_1m"),
    ("5m",  "candles_5m"),
    ("15m", "candles_15m"),
    ("60m", "candles_1h"),
    ("1d",  "candles_1d"),
];
```

### 4.3 Tolerance

**Zero tolerance for every field.** Any `abs(h.x - m.x) > 0` flags as
mismatch. Comment in rule file: *"NO tolerance. NO epsilon. NO ±10%.
Every value must match exactly."*

### 4.4 Success idempotency

A one-shot daily success marker prevents re-running cross-verify multiple
times per day (`cross_verify_success_marker_path()` in `cross_verify.rs`).
Same day reboot → skip.

---

## 5. Open design questions (decisions needed)

### 5.1 F&O cross-verification (HIGHEST ROI gap)

**Problem:** `NSE_FNO` derivatives are ~97% of our universe and are NOT
cross-verified at all. Dhan REST `/charts/historical` returns errors for
F&O contracts.

**Options:**

| Option | Approach | Pros | Cons |
|---|---|---|---|
| **A. Accept the gap** | Document it; F&O is only checked via live tick-gap detection | No new code | 97% of universe has zero OHLCV verification |
| **B. NSE bhavcopy daily CSV** | Download free public bhavcopy from NSE at 18:00 IST, cross-reference live candles vs bhavcopy EOD OHLCV | Authoritative source, free, one file per day | EOD only — can't catch intra-day candle bugs |
| **C. Option chain snapshots** | Fetch Dhan option chain hourly, compare ATM strikes' LTP progression vs live | Intra-day coverage | Fragile; options chain is not a canonical OHLCV source |
| **D. Trade Book REST reconciliation** | Post-market, pull Dhan Trade Book (our filled orders) and reconcile vs live ticks at fill timestamps | Zero infra; validates the exact prices our orders saw | Only covers contracts we actually traded |

**Recommendation: (B)** — NSE bhavcopy. One fetcher, one diff routine,
free public data.

### 5.2 Historical data itself corrupted

**Problem:** Today we compare live vs historical. If historical has bad
data from Dhan, live matches bad-to-bad and passes.

**Options:**

| Option | Approach |
|---|---|
| **A. NSE bhavcopy cross-reference** (same as 5.1 B) | bhavcopy serves as ground-truth third source |
| **B. Multi-day drift detection** | Does today's 09:15 candle match yesterday's 09:15 shape? Outlier = suspicious |
| **C. Standard-deviation outlier flagging** | Flag any OHLC that's >3σ from its neighbors |

**Recommendation:** **(A)** — addresses 5.1 + 5.2 with one mechanism.

### 5.3 Intra-day verification

**Problem:** Cross-match runs once post-market. If live breaks at 10:00,
we find out at 15:47.

**Options:**

| Option | Approach |
|---|---|
| **A. Every-hour partial cross-match** | Windowed cross-match at 10:30, 11:30, … 15:30 |
| **B. Real-time tick-rate anomaly detection** | Already partly implemented via `TickGapTracker` + stall poller (Finding #2) |
| **C. Leave as-is** | Acceptable for SEBI audit; operators have other signals |

**Recommendation: (C)** — existing mechanisms cover this. Option (A) is
load for marginal value. Option (B) is already being strengthened.

### 5.4 Candle boundary semantics

**Problem:** Dhan's 09:15 1m candle: is it `[09:15:00, 09:15:59]`
stamped-as-09:15, or `[09:14:00, 09:14:59]` stamped-as-09:15? Our live
aggregation assumes the former. If Dhan's historical uses the latter,
**every live candle is off-by-one bucket vs historical**, producing
false mismatches.

**Options:**

| Option | Approach |
|---|---|
| **A. Dhan support email** | Ask definitively and pin the answer forever |
| **B. Empirical correlation** | Compare known-stable day (e.g. a non-news index candle) across both sources |
| **C. Trust current assumption** | Current cross-match passes on good days → assumption is right |

**Recommendation: (A)** — one email, 15 min to write. Permanent answer.

### 5.5 Amendments / late revisions

**Problem:** Dhan historical API sometimes revises past candles (volume
corrections hours-to-days later). We fetch once post-market; if historical
changes afterward, our cross-match state is stale.

**Options:**

| Option | Approach |
|---|---|
| **A. Re-fetch + re-compare last 7 days** | Nightly |
| **B. One-shot only** | Current behaviour |
| **C. Trigger re-fetch only when `missing_live > 0`** | Narrow |

**Recommendation: (A)** — 7 × 241 × 4 TFs = 6,748 rows, not expensive.

### 5.6 Clock skew between our wall-clock and Dhan exchange-timestamp

**Problem:** If our host clock drifts, `received_at - exchange_timestamp`
grows, and cross-match rows don't align → false `missing_live`.

**Options:**

| Option | Approach |
|---|---|
| **A. NTP monitoring metric** | `tv_clock_skew_seconds` gauge |
| **B. Distribution check** | Every N ticks, record percentile of `received_at - exchange_timestamp` |
| **C. Accept** | — |

**Recommendation: (B)** — cheap, surfaces NTP issues early.

### 5.7 Tolerance policy

**Current:** Zero tolerance on every field (prices, volume, OI).

**Question:** Should volume get a small tolerance? Exchange sometimes
rounds volume differently between live-tick aggregation and EOD batch.

**Recommendation: KEEP zero tolerance.** Any divergence IS a bug worth
surfacing. A small tolerance masks real issues.

---

## 6. Priority ranking

| # | Item | Severity | Effort | Blocks prod? |
|---|---|---|---|---|
| 1 | **NSE bhavcopy cross-ref** (§5.1 + §5.2) | HIGH — 97% of universe unverified | ~4 hrs | No, but major audit gap |
| 2 | **Candle boundary Dhan ticket** (§5.4) | HIGH if wrong | 15 min | No, but potential silent bug |
| 3 | **Display fix** (§3.1) | LOW | 30 min | No |
| 4 | **Multi-day re-fetch** (§5.5) | MEDIUM | 2 hrs | No |
| 5 | **Clock skew gauge** (§5.6) | LOW | 1 hr | No |

---

## 7. What existed before today's work

Before 2026-04-24:
- Pass criteria did NOT include `coverage_pct` check → false OK on
  mid-session boot.
- `HistoricalFetchComplete` fired OK with `Fetched: 0 / Candles: 0` on
  Dhan outages.
- Telegram table labels were (and still are) display-misleading.
- F&O had zero cross-verification and still does.

After 2026-04-24 (PR #341 + #342 merged):
- Mid-session-boot false OK is killed (tests + coverage guard).
- Zero-fetched false OK is killed (tests + explicit gate).
- F&O gap is documented but not yet addressed.

---

## 8. Files involved

| File | Role |
|---|---|
| `crates/core/src/historical/cross_verify.rs` | Main logic — `cross_match_historical_vs_live()`, `determine_cross_match_passed()`, `compute_coverage_pct()` |
| `crates/core/src/historical/candle_fetcher.rs` | Dhan REST historical fetcher |
| `crates/core/src/notification/events.rs` | Telegram variants `CandleCrossMatch{Passed,Failed,Skipped}` |
| `crates/app/src/main.rs` (~line 5049) | Cross-match caller + routing |
| `crates/storage/src/candle_persistence.rs` | Historical + MV write paths |
| `.claude/rules/project/historical-candles-cross-verify.md` | Mechanical rules |
| `.claude/rules/project/audit-findings-2026-04-17.md` | Rule 11 (false-OK class bug) |

---

## 9. Regression tests currently locking this in

All in `crates/core/src/historical/cross_verify.rs::tests`:

| Test | Asserts |
|---|---|
| `test_determine_cross_match_passed_false_when_coverage_below_90` | 10% + 89% coverage both fail |
| `test_determine_cross_match_passed_true_at_exact_threshold` | 90% passes |
| `test_determine_cross_match_passed_true_at_full_coverage` | 100% passes |
| `test_compute_coverage_pct_zero_compared_returns_100` | No historical grid → 100 |
| `test_compute_coverage_pct_full_coverage` | 1000/1000 = 100 |
| `test_compute_coverage_pct_partial_mid_session_boot` | 8676/90375 = 9 (the exact 2026-04-24 scenario) |
| `test_compute_coverage_pct_above_100_is_clamped` | Can't exceed 100 |
| `test_compute_coverage_pct_exact_90_percent` | 90/100 = 90 |
| `test_cross_match_min_coverage_pct_constant_is_90` | Constant stays at 90 |

Plus `crates/app/tests/mid_session_boot_guard.rs::test_historical_fetch_guards_zero_fetched_zero_candles` for PR #342's gate.
