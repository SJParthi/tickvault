# Follow-up Plan: Post-market 1-minute cross-verification (NEXT PR)

**Status:** APPROVED — operator locked **CSV output + exact-match + mismatch count** (2026-06-02: *"csv and exact match cross verification is needed"*). Implementing on branch `claude/cross-verify-1m`.
**Date:** 2026-06-02
**Approved by:** Parthiban — feature + CSV + exact-match all confirmed 2026-06-02.

**Operator quote (2026-06-02):** *"put back the historical cross verification at precise 3.31 pm onwards … cross verification between dhan historical intraday of entire one minute candle OHLCV for every timestamp … among our candles_1m … highlighted or stored or csv or excel file or easily accessible and trackable to find how many times there is a mismatch … only for one min alone … for all those subscribed spot instruments."*

## Scope (locked by operator)
- **When:** 15:31 IST onwards (post-close), once per trading day, cold path.
- **What:** for every subscribed **spot** instrument (the 243 daily-universe SIDs — indices + F&O underlyings), fetch Dhan **intraday 1-minute** candles (REST `/v2/charts/intraday`, interval `"1"`) and compare OHLCV **timestamp-by-timestamp** against our `candles_1m`.
- **Only 1m.** No other timeframe.
- **Output:** mismatches written to an easily-accessible, trackable artefact — **CSV/Excel file** + a QuestDB audit table (`cross_verify_1m_audit`) + a per-day **mismatch count** + Telegram summary.
- Pre-market finalized close → 09:15 open is ALREADY done (separate, untouched).

## Plan items (next PR)
- [ ] `crates/core/src/historical/intraday_1m_fetcher.rs` — bounded REST `/v2/charts/intraday` (interval "1", today only), rate-limited 5/sec, fail-soft per symbol. (NOT the deleted 90-day chain — needs `live-feed-purity.md` carve-out extension, written FIRST.)
- [ ] `crates/core/src/historical/cross_verify_1m.rs` — pure timestamp-keyed OHLCV diff (O(1) HashMap join on `(security_id, segment, minute_ts)`); ZERO tolerance (exact match) per `historical-candles-cross-verify.md`.
- [ ] `crates/storage/src/cross_verify_1m_audit_persistence.rs` — QuestDB table, DEDUP `(trading_date_ist, security_id, segment, minute_ts, field)`; + CSV writer to `data/cross-verify/cross-verify-1m-YYYY-MM-DD.csv`.
- [ ] `crates/app/src/main.rs` — 15:31 IST scheduler spawn (market-hours/edge-trigger gated).
- [ ] ErrorCode `CROSS-VERIFY-1M-01` (mismatch found) + `-02` (REST fetch degraded) + rule file + runbook + cross-ref test.
- [ ] NotificationEvent `CrossVerify1mComplete { compared, mismatches, missing }` (Severity gated on mismatch count); honest false-OK gate (audit Rule 11 — never report "OK" on an empty compare set).
- [ ] Ratchets: pure-diff unit/property tests, CSV round-trip, DEDUP meta-guard, source-scan boot-wiring guard, the 15-row + 7-row guarantee matrix.

## Pros / Cons / What it adds / What's still missing (operator-requested analysis)

| Dimension | Detail |
|---|---|
| ✅ **Pro — correctness proof** | Daily, automated, exact-match evidence that our live 1m candles match NSE's authoritative tape (the very gap seen on the NIFTY chart). Trackable mismatch count over time = a quality SLI. |
| ✅ **Pro — auditable artefact** | CSV/Excel + QuestDB table + Telegram. Open the CSV in Excel, sort by mismatch, see exactly which SID/minute/field drifted. SEBI-friendly trail. |
| ✅ **Pro — O(1) compare** | HashMap keyed `(sid, segment, minute_ts)` → O(1) per-minute lookup; O(N) total over the day (~375 min × 243 SIDs). Cold path, off the live hot path — zero trading-latency impact. |
| ✅ **Pro — bounded & fail-soft** | Today-only, 1m-only, spot-only; a symbol Dhan can't return is skipped + logged; never blocks. |
| ⚠️ **Con — expected non-zero mismatches** | Our live feed is **sampled** (2–4 ticks/sec) vs Dhan's full tape, so H/L especially WILL differ on some minutes. Zero-tolerance flags these. **Mitigation:** track the trend — a stable baseline = sampling noise; a sudden spike = real problem. (Optional tolerance band later, operator's call.) |
| ⚠️ **Con — Dhan Data-API quota** | 243 intraday calls/day at 5/sec ≈ 50s of calls. Well inside the 100k/day Data-API budget. |
| ⚠️ **Con — re-adds a deleted surface** | `cross_verify.rs` was deleted 2026-05-26. This is a NARROWER replacement (1m-only, spot-only, today-only). Needs `live-feed-purity.md` carve-out extended so the banned-pattern scanner doesn't reject it. |
| ❌ **Missing (deferred)** | Excel `.xlsx` (vs `.csv`) needs a crate dep (`rust_xlsxwriter`) → Parthiban version-pin approval. Start with CSV (Excel opens it natively); add `.xlsx` only if formatting/colours wanted. |
| ❌ **Missing (deferred)** | Per-field tolerance bands (e.g., ignore ±0.05 on H/L) — start exact, tune later. |
| ❌ **Missing (deferred)** | A dashboard/web view of the mismatch trend — CSV + Telegram first; visualization is a follow-up. |

## Open question for operator (before implementation)
1. **CSV now, Excel later?** (CSV opens in Excel natively; `.xlsx` needs a new pinned dep → your approval.)
2. **Exact-match or tolerance band?** (Recommend exact first + track the count; the count itself tells you if it's noise or a real break.)
3. **All 243 spot SIDs, or indices-only to start?** (Operator said "all subscribed spot instruments" → all 243. RESOLVED.)
