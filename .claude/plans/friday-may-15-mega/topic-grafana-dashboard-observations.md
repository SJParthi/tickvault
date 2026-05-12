# Topic — Grafana Market Data Explorer Observations + Gaps

> **Status:** DRAFT (reference + analysis, discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** Reference file documenting observed Grafana state. Master coverage map references this.
> **Trigger:** Operator screenshot 2026-05-12 10:57 IST of Grafana → Dashboards → tickvault → Market Data Explorer.

---

## 🚗 Auto-Driver Story

> Sir, you showed me the Market Data Explorer dashboard. 4 things stood out:
> 1. ✅ **Instrument Build History** is captured — 240,129 CSV rows → 146,707 parsed → 192 indices, 2,468 equities, 218 underlyings, 98,293 derivatives, 687 option chains, 2.36s build time.
> 2. ✅ **NSE Trading Calendar 2026** has all 28 entries including 3 Mock Trading Sessions (Saturdays) — operator-awareness FYI.
> 3. ❌ **Index Summary — Stocks Per Index** = empty.
> 4. ❌ **NSE Index Constituents — All Stocks Tagged To Their Indices** = empty.
> 5. ⚠️ **build_time = 1970-01-01 05:30:00** = EPOCH BUG (epoch 0 + IST offset).
>
> Today we map these to Friday gaps.

---

## 📊 Panel-by-panel diagnosis

### Panel 1 — Instrument Build History ✅

| Field | Value | Interpretation |
|---|---|---|
| csv_source | primary | Dhan detailed CSV (matches `dhan-ref/09-instrument-master.md`) |
| csv_row_count | 240,129 | Total rows in Dhan CSV |
| parsed_row_count | 146,707 | After filter (rejected ~93K filter mismatches) |
| index_count | 192 | All indices visible |
| equity_count | 2,468 | All NSE equities |
| underlying_count | 218 | F&O underlyings (= stocks with futures + indices with futures) |
| derivative_count | 98,293 | All F&O contracts (every strike, every expiry) |
| option_chain_count | 687 | All option chains (underlying × expiry pairs) |
| build_duration_ms | 2.36 s | Universe build time (matches log: `build_ms: 2361`) |
| build_time | **1970-01-01 05:30:00** | ⚠️ **EPOCH BUG** — should be 2026-05-12 10:14:30 |

**Gap 1:** `build_time` column shows epoch 0 + IST offset. Bug in the persist path — either:
- `instrument_build_metadata` table column is `TIMESTAMP` but app writes `i64=0`
- OR app writes correct value but Grafana panel format is wrong

**Action for Friday:** Trace the writer in `crates/storage/src/instrument_persistence.rs::ensure_instrument_tables` and verify the `build_time` column is populated with `now() AT TIME ZONE 'Asia/Kolkata'` not `EPOCH 0`.

### Panel 2 — NSE Trading Calendar (2026) ✅

28 rows visible, including:
- 7 Mock Trading Sessions (Saturdays) — 2026-01-03, 02-07, 03-07 (and more on next pages)
- Holidays: Republic Day, Holi, Shri Ram Navami, Shri Mahavir Jayanti, ...

**Trading calendar is healthy.** `nse_holidays` table populated correctly.

**Observation (Operator-facing):** Mock Trading Sessions are first-class citizens in the calendar. App correctly treats them as TRADING DAYS (not holidays). This matches `trading_calendar.rs` behavior:
```
"is_trading_day": true,
"is_mock_trading_session": false   (today is regular trading)
```

### Panel 3 — Index Summary — Stocks Per Index ❌

**Status:** No data.

**Hypothesis:** The `index_constituents` table is empty (or has data but the query joins wrong).

**Check from QuestDB screenshot:** `index_constituents` table EXISTS but presumably is empty (the boot log says "no constituency cache available — continuing without constituency data").

**Root cause:** App couldn't read `data/instrument-cache/constituency-map.json` at boot. Log:
```
WARN no constituency cache available — continuing without constituency data
  error: failed to read constituency cache at data/instrument-cache/constituency-map.json
```

**Action for Friday:** Verify the constituency download path (`niftyindices.com` source). Empty constituency = empty Index Summary + empty Index Constituents panels.

### Panel 4 — NSE Index Constituents — All Stocks Tagged To Their Indices ❌

Same as Panel 3. Same root cause.

---

## 🚨 The 3 gaps surfaced by this dashboard

| # | Gap | Severity | Friday action |
|---|---|---|---|
| 1 | `build_time = 1970-01-01 05:30:00` (epoch bug) | LOW (cosmetic, audit-relevant) | Fix writer to use `now()` IST timestamp |
| 2 | `index_constituents` table empty | MEDIUM (dashboard shows blank) | Verify NSE indices download + cache path |
| 3 | Index Summary panel empty (depends on #2) | MEDIUM | Auto-fixes when #2 resolves |

---

## 🛡️ Z+ implications for these gaps

### Gap 1 — Epoch bug

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_instrument_build_metadata_invalid_timestamps_total` (NEW) |
| L2 VERIFY | Boot-time: query `instrument_build_metadata`; assert all `build_time != EPOCH(0)` |
| L3 RECONCILE | Daily: check `MIN(build_time) > '2020-01-01'` |
| L4 PREVENT | Fix the writer at source |
| L5 AUDIT | Already captured in this table |
| L6 RECOVER | UPDATE statement to backfill correct timestamps (one-time SQL) |
| L7 COOLDOWN | N/A |

### Gap 2 — Index constituents empty

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_index_constituents_count` gauge, alarm if `== 0` |
| L2 VERIFY | Daily download from niftyindices.com, fall back to cache |
| L3 RECONCILE | Daily 23:00 IST: row count check |
| L4 PREVENT | Pre-market check: if constituents empty AND it's a trading day → CRITICAL |
| L5 AUDIT | `constituency_download_audit` table (NEW) |
| L6 RECOVER | Operator manual: place a fresh JSON in `data/instrument-cache/constituency-map.json` |
| L7 COOLDOWN | 5 min between download retries |

---

## 📋 What the dashboard CONFIRMS about our plan

### ✅ Confirmed working

1. **Instrument master** — 98,293 derivatives parsed correctly
2. **Trading calendar** — 28 entries including Mock Trading Sessions
3. **Universe build** — 2.36s build time (fast)
4. **DEDUP keys** — `instrument_build_metadata` has rows (no dupes visible)

### ⚠️ Confirmed degraded

1. **Index constituents** — boot WARNed about missing cache; dashboard confirms it never loaded
2. **`build_time` field** — cosmetic bug, but audit-relevant (SEBI may want this column)

### ❓ Not yet visible

1. **`index_prev_close.json`** file exists per `data/` screenshot but its panel isn't shown here
2. **NSE_FNO derivatives breakdown** by underlying — should be a panel (operator request later?)
3. **Active subscription panel** — shows how many of 98,293 derivatives are currently subscribed (we sub 10,439 index + 0 stock = 10,439)

---

## 🎯 Discussion items

### D1 — Where is the index constituents source-of-truth?

3 candidate sources:
- (a) `niftyindices.com` HTML page (current implementation per `index_constituency/mod.rs`)
- (b) `nseindia.com` JSON API
- (c) Dhan REST API

**Question:** Which is most reliable? Today neither is loaded.

**Hypothesis:** `niftyindices.com` may have changed their HTML structure → parser broken.

**Action for Friday:** Investigate. Add fallback to (b) or (c).

### D2 — Should the dashboard FAIL LOUDLY if constituents are empty?

Today the panel shows "No data" silently. Operator may not notice for weeks.

**Proposal:** Add a Stat panel "Constituency Status" with red color if empty, green if loaded.

### D3 — How to migrate the build_time epoch bug?

If we fix the writer, future builds work. But existing rows in `instrument_build_metadata` are wrong.

**Option (a):** One-time SQL `UPDATE instrument_build_metadata SET build_time = ts WHERE build_time = 0;` (use `ts` column as a proxy).
**Option (b):** Accept historical rows are wrong; only new rows are correct.

**My vote:** (a) — clean fix.

### D4 — Mock Trading Sessions handling

Operator's calendar has Mock Trading Sessions on Saturdays. Per `trading_calendar.rs`, today's boot log shows:
```
"is_trading_day": true,
"is_mock_trading_session": false
```

**Verify:** On Mock Trading Saturday (e.g., 2026-01-03), does the app correctly treat it as a trading session — start WS, fetch Greeks, etc.?

**Open question:** Are Dhan APIs available on Mock Trading days? Or do they treat it as closed?

**Action for Friday:** Email Dhan support to confirm Mock Trading day API behavior.

---

## 🎤 Operator's question implicit answer

**Q: "What did the Grafana dashboards tell you about gaps?"**

**A:** 3 gaps mapped to Friday actions:
1. Cosmetic: build_time epoch bug — 1-line fix
2. Functional: index_constituents empty — need source-of-truth verification
3. Cosmetic: Index Summary panel empty — auto-fixes with #2

**All 3 are NON-critical for trading** (no order placement impact, no tick loss). They're operator-visibility issues. Important but lower priority than the 7-Layer Z+ work.

3 days no code. Floor's yours dude.
