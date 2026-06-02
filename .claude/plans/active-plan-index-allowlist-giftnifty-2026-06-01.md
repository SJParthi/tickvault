# Implementation Plan: Fixed Index Allowlist (32) + GIFT Nifty market-hours exemption

**Status:** APPROVED
**Date:** 2026-06-01
**Approved by:** Parthiban (operator)

> **Guarantee matrices:** this plan inherits the 15-row + 7-row guarantee
> matrix from `.claude/rules/project/per-wave-guarantee-matrix.md` and the
> Z+ 7-layer checklist from `.claude/rules/project/z-plus-defense-doctrine.md`
> (cross-referenced per the per-item-guarantee-check contract). The compact
> Z+ checklist for this change is in the "Z+ checklist" section below.

## Verbatim operator quotes (do not paraphrase)

**Q1 (2026-06-01, index allowlist):**
> "see whatevr is shown here as indices i need only tehse indices alone only
> dude okay no need to ahve 100 plus idncises dude okay? alogn with this one
> more index which is BSE SENEX dude okay"

**Q2 (2026-06-01, GIFT Nifty market-hours exemption):**
> "see if im not worng only for giftnifty alone we cant have the 9.15 am
> between 3.30 pm tiemstamp restructio nrigth bro add this also into the queue
> and plan"

**Q3 (2026-06-01, approval):**
> "option A bro okay?" … "yes go ahead with the plan bro go ahead approved"

**Decision = Option A:** keep the ~218 F&O underlying stocks; trim the index
set from "all IDX_I NSE rows" to a FIXED allowlist of the 31 NSE indices
shown on Dhan's Markets → Index → NSE page + 1 BSE SENSEX = 32 indices.
GIFT Nifty (1 of the 32) is exempt from the 09:00/09:15–15:30 IST tick/candle
window because it trades ~21h/day on NSE-IX.

## The 32-index allowlist (NSE display set + BSE SENSEX)

NSE (31) — matched by normalized symbol name (case/space-insensitive):
NIFTY, BANKNIFTY, NIFTY NEXT 50, FINNIFTY, INDIA VIX, NIFTY IT, NIFTY AUTO,
NIFTY PHARMA, NIFTY FMCG, NIFTY METAL, NIFTY MEDIA, NIFTY REALTY, NIFTY ENERGY,
NIFTY INFRA, NIFTY MNC, NIFTY PSU BANK, NIFTY PVT BANK, NIFTY CONSUMPTION,
NIFTY SERV SECTOR, NIFTY 100, NIFTY 200, NIFTY 500, NIFTY MIDCAP SELECT,
NIFTY MIDCAP 50, NIFTY MIDCAP 100, NIFTY MIDCAP 150, NIFTY SMALLCAP 50,
NIFTY SMALLCAP 100, NIFTY SMALLCAP 250, NIFTY MICROCAP 250, GIFT NIFTY.

BSE (1): SENSEX (already handled by the existing BSE arm of `extract_indices`).

> **Honest data note:** 25 of the 32 names are verified against in-repo
> constants (`DISPLAY_INDEX_ENTRIES` + `LOCKED_UNIVERSE`). ~6 (NIFTY NEXT 50,
> FINNIFTY, NIFTY MIDCAP SELECT, NIFTY MIDCAP 100, NIFTY MICROCAP 250,
> GIFT NIFTY) are best-known Dhan symbol strings. The live `images.dhan.co`
> CSV is unreachable from the dev sandbox (egress 403), so the EXACT strings
> are confirmed against the live CSV on the AWS box at the next 08:30 boot via
> the boot-time "allowlist member not found in today's CSV" WARN (item 6).

## Plan Items

- [ ] 1. Rule-file edit FIRST (scope-lock §15): add a dated §30 to
  `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
  documenting the 32-index fixed allowlist (supersedes "all IDX_I NSE")
  + the GIFT Nifty always-on market-hours exemption + the boot-WARN mechanism.
  - Files: `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
  - Also: `.claude/rules/project/market-hours.md` (note the single 24h exemption)

- [ ] 2. Index allowlist constant + normalizer.
  - Files: `crates/common/src/constants.rs`
  - Add `NSE_INDEX_ALLOWLIST: &[&str]` (31 normalized names), `GIFT_NIFTY_SYMBOL`,
    and `normalize_index_symbol(&str) -> ` (uppercase + collapse internal spaces).
  - Tests: `test_nse_index_allowlist_has_31`, `test_normalize_index_symbol_*`

- [ ] 3. Extractor filters NSE indices to the allowlist + reports misses.
  - Files: `crates/core/src/instrument/index_extractor.rs`
  - `extract_indices` keeps an NSE row only if `normalize_index_symbol(symbol)`
    ∈ allowlist; returns `allowlist_misses: Vec<&'static str>` (allowlisted
    names not seen in the CSV) so the orchestrator can WARN.
  - Update module docstring (no longer "all NSE indices").
  - Tests: `keeps_only_allowlisted_nse_indices`, `drops_non_allowlisted_nse_index`,
    `reports_allowlist_misses`, `gift_nifty_kept_when_present`, plus fix the
    existing `realistic_universe_count_matches_30` / `extracts_all_nse_idx_i_indices`
    fixtures to use real allowlisted names.

- [ ] 4. Build the always-on (no-market-hours) exemption set at boot.
  - Files: `crates/core/src/instrument/daily_universe.rs`, `crates/app/src/main.rs`
  - From the extracted indices, resolve the row whose normalized symbol ==
    `GIFT NIFTY` → its `(security_id, exchange_segment_code)` → into an
    `Arc<HashSet<(u32, u8)>>` always-on set. Empty (with WARN) if not found.
  - Tests: `gift_nifty_security_id_resolved_into_exemption_set`,
    `exemption_set_empty_when_gift_absent`

- [ ] 5. Hot-path gates honor the always-on set (GIFT exempt from window drops).
  - Files: `crates/core/src/pipeline/tick_processor.rs`
    (`is_within_persist_window` + `is_wall_clock_within_persist_window` call
    sites at ~904/926 — skip both when `(sid,seg)` ∈ always-on set; KEEP the
    `is_today_ist` stale-day check),
    `crates/trading/src/candles/multi_tf_aggregator.rs`
    (`consume_tick` window gate at ~241 — skip when `(sid,seg)` ∈ always-on set).
  - Both accept the `Arc<HashSet<(u32,u8)>>` (default empty → today's behavior
    unchanged for all non-GIFT instruments).
  - Tests: `gift_sid_persists_outside_window`, `non_gift_sid_dropped_outside_window`,
    `gift_sid_aggregates_outside_window`, `non_gift_sid_skipped_outside_window`

- [ ] 6. Boot-time loud WARN for any allowlist miss (no silent drops).
  - Files: `crates/app/src/main.rs` (daily-universe orchestrator wiring)
  - `error!`/`warn!` with the list of allowlisted names absent from today's CSV
    so a wrong/renamed symbol surfaces. (Uses existing INSTR-FETCH path codes.)
  - Tests: covered by item 3's `reports_allowlist_misses` + a wiring source-scan.

- [ ] 7. Live-CSV confirmation (NEXT 08:30 BOOT — not code):
  read the live Dhan CSV on the box (reachable from AWS, blocked here),
  confirm the ~6 best-known symbol strings + GIFT Nifty's segment/SID, push a
  one-line correction if the boot WARN flagged any. (Tracked, not a code item.)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | CSV has all 31 NSE allowlisted + SENSEX | 32 indices, 0 misses, 0 WARN |
| 2 | CSV has 200 NSE IDX_I index rows | only the 31 allowlisted kept; rest dropped |
| 3 | CSV missing FINNIFTY (renamed) | 31 indices, WARN names "FINNIFTY" |
| 4 | GIFT Nifty tick at 20:00 IST | persisted + aggregated (exempt) |
| 5 | NIFTY tick at 20:00 IST | dropped (outside window) — unchanged |
| 6 | GIFT Nifty absent from CSV | exemption set empty + WARN; no crash |
| 7 | GIFT Nifty tick from previous day | still dropped by `is_today_ist` |

## Z+ checklist (compact)

- L1 DETECT: boot WARN on allowlist miss; counter on dropped vs persisted ticks.
- L2 VERIFY: extractor unit tests + tick-gate unit tests (pure functions).
- L3 RECONCILE: next-boot live-CSV confirmation (item 7).
- L4 PREVENT: `[100,400]` universe-size envelope unchanged; allowlist bounds indices ≤32.
- L5 AUDIT: `instrument_lifecycle` rows unchanged (still UPSERT all); index trim
  is subscription-side only — master/audit retention untouched (SEBI safe).
- L6 RECOVER: GIFT-absent / miss → empty exemption set + WARN, never a halt for
  the exemption path; allowlist miss does NOT block boot (indices still subscribe).
- L7 COOLDOWN: n/a (no retry path added).
- Honest 100%: "100% inside the tested envelope — allowlist + exemption are
  pure-function unit-tested; exact CSV symbol strings + GIFT segment confirmed
  against the live box CSV at next boot via the loud miss-WARN."

## VERIFIED 2026-06-01 from the operator's uploaded master CSV (api-scrip-master-detailed.csv, 220,287 rows)

**F&O single-stock count is 211 (CORRECTED 2026-06-01 vs NSE fo_mktlots.csv).**
Dhan's master has 229 distinct NSE FUTSTK/OPTSTK `UNDERLYING_SYMBOL`, but 18 of
those are dummy `011NSETEST…181NSETEST` test scrips. NSE's official
`fo_mktlots.csv` lists exactly **211 stock F&O underlyings** + 5 index F&O
(NIFTY/BANKNIFTY/FINNIFTY/MIDCPNIFTY/NIFTYNXT50); the 211 real symbols match
Dhan **exactly** (intersection 211, zero missing; diff = the 18 NSETEST only).
tickvault ALREADY excludes them (`CSV_TEST_SYMBOL_MARKER="TEST"` →
`is_test_instrument` `.contains("TEST")`), so the extractor yields **211**, not
229. Real total universe = **32 indices + 211 stock spots = 243** (inside the
[100,400] envelope). Pinned by `excludes_real_nsetest_scrips_011_through_181`.

**GIFT Nifty confirmed in IDX_I:** `EXCH=NSE SEG=I INSTR=INDEX sid=5024
SYM='GIFTNIFTY' DISP='Gift Nifty'` → the index allowlist + always-on exemption
both work by matching `GIFTNIFTY`; exemption resolves sid 5024 dynamically.

**NSE INDEX rows in the master = 119** (the "100+" to trim). The 31 picks map to
exact `SYMBOL_NAME` / sid (VERIFIED — no guessing, no boot-WARN misses expected):

| SYMBOL_NAME | sid | SYMBOL_NAME | sid |
|---|---|---|---|
| NIFTY | 13 | NIFTY CONSUMPTION | 40 |
| NIFTY NEXT 50 | 38 | NIFTY SERV SECTOR | 46 |
| BANKNIFTY | 25 | MIDCPNIFTY | 442 |
| FINNIFTY | 27 | NIFTYMCAP50 | 20 |
| INDIA VIX | 21 | NIFTY MID100 FREE | 37 |
| NIFTYIT | 29 | NIFTY MIDCAP 150 | 1 |
| NIFTY AUTO | 14 | NIFTY SMALLCAP 50 | 22 |
| NIFTY PHARMA | 32 | NIFTY SMALLCAP 100 | 5 |
| NIFTY FMCG | 28 | NIFTY SMALLCAP 250 | 3 |
| NIFTY METAL | 31 | NIFTY MICROCAP250 | 444 |
| NIFTY MEDIA | 30 | NIFTY PVT BANK | 15 |
| NIFTY 100 | 17 | NIFTY PSU BANK | 33 |
| NIFTY 200 | 18 | NIFTY REALTY | 34 |
| NIFTY 500 | 19 | NIFTY ENERGY | 42 |
| GIFTNIFTY | 5024 | NIFTYINFRA | 43 |
| NIFTY MNC | 44 | (+ BSE SENSEX via BSE arm) | |

**IMPLEMENTATION RISK to verify:** the master `SEGMENT` column is single-char
(`I`=index), but `extract_indices` matches `segment == "IDX_I"`. Confirm the
csv_parser normalizes `(EXCH_ID, SEGMENT='I') → "IDX_I"` BEFORE the extractor —
if it stores raw `"I"`, the allowlist (and the pre-existing extractor) match 0
rows. Check `crates/core/src/instrument/csv_parser.rs` during impl. (Match the
allowlist by the parser's post-normalization `symbol_name`, exact strings above.)

## NEW ITEM 8 (operator 2026-06-01) — per-candle "% change vs 09:15 open" column

**Operator quote:**
> "always try to have one more extra column in every candles which is one extra
> column it shoudl always have open 9.15 am percnetage change alone … you have
> this pre market finalised close price as 9.15 am open market price right"

**Confirmed understanding:** the 09:15:00 session OPEN price (= the price the
pre-open auction 09:00–09:08 finalizes) is the day's open. Every sealed candle
(all 21 TFs) gets ONE extra column `open_pct = (close − day_open_0915) /
day_open_0915 × 100` (2-dp, f64). Sibling of the existing seal-time
prev-day pct-stamping (`close_pct_from_prev_day`). div-by-zero → 0.0.

**ANCHOR = Option 2 (operator 2026-06-01):** the official 09:15 open = the
exchange `day_open` field carried in every Quote packet (bytes 34-37 = NSE
pre-open auction result). No deleted module needed — captured live on
`LiveCandleState.session_open` (last-non-zero-wins, mirrors `prev_day_close`).
RAM: `LiveCandleState` budget bumped 96→112 B (+84 KB total, negligible).
STATUS: DONE — PR3 (trading candles 1257 green, storage 492 green, app builds).

- [x] 8a. Add `open_pct` (DOUBLE) to every `candles_*_shadow` table DDL +
  `ALTER ADD COLUMN IF NOT EXISTS` self-heal. Files:
  `crates/storage/src/shadow_persistence.rs` (+ `candle_persistence.rs` if it
  writes live candles). DEDUP key UNCHANGED `(ts, security_id, segment)`.
- [ ] 8b. Capture per-instrument day-open @ 09:15 in the aggregator (first 1m
  bucket open of the IST day, or the tick `day_open` field) + stamp `open_pct`
  on every `LiveCandleState` at seal. Files:
  `crates/trading/src/candles/multi_tf_aggregator.rs` (+ the seal/writer slice).
  GIFT Nifty: its "09:15 open" = its session open per its own trading day.
- [ ] 8c. Tests: `open_pct_zero_when_close_equals_open`,
  `open_pct_positive_when_above_open`, `open_pct_div_by_zero_is_zero`,
  DDL-has-open_pct, ALTER-self-heal present.

**PR sequencing (serial per operator rule):** PR1 = Items 1–7 (allowlist + GIFT
exemption). Drive to merge. PR2 = Item 8 (candle open_pct column). Drive to merge.

## Out of scope / unchanged

- 2-WebSocket lock (1 main + 1 order) — UNCHANGED.
- ~218 F&O underlyings — UNCHANGED (Option A).
- `instrument_lifecycle` master (still stores all applicable F&O + indices) — UNCHANGED.
- Quote subscription mode for all — UNCHANGED.
