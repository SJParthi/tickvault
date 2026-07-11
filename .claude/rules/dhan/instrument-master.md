# Dhan Instrument Master Enforcement

> **Ground truth:** `docs/dhan-ref/09-instrument-master.md`
> **Scope:** Any file touching the instrument-master CSV download, parsing, the
> daily universe build, the F&O underlying / contract extraction, or the
> `instrument_lifecycle` master table.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment,
> InstrumentType, ExpiryCode), `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
> (the operator-locked daily-universe + applicable-F&O-master contract — AUTHORITATIVE).

> **Note (2026-05-29):** this enforcement rule was created to fill the slot
> CLAUDE.md's rule index already referenced. The download/retry/hardening
> contract lives in the daily-universe lock (§3/§4/§9/§18/§26); THIS file is
> the always-on mechanical-rule digest for instrument-master code.

## Mechanical Rules

1. **Read the ground truth first.** Before adding/modifying any CSV downloader,
   parser, universe builder, extractor, or lifecycle persistence:
   `Read docs/dhan-ref/09-instrument-master.md`.

2. **Two CSV URLs — exact.** Compact `https://images.dhan.co/api-data/api-scrip-master.csv`;
   **Detailed** `https://images.dhan.co/api-data/api-scrip-master-detailed.csv`.
   The daily universe uses the **Detailed** CSV (it carries
   `UNDERLYING_SECURITY_ID`, `STRIKE_PRICE`, `OPTION_TYPE`, `SM_EXPIRY_DATE`
   needed for F&O). Per-segment REST `/v2/instrument/{exchangeSegment}` is a
   FALLBACK that is BANNED in the daily-universe path (lock §3 "no fallback").

3. **SecurityId is the sole primary key — NEVER hardcode it.** SecurityIds change
   when instruments are relisted / new series issued. Download fresh daily.
   Derivative SecurityIds are especially unstable (new contracts list, old
   expire). Composite key `(security_id, exchange_segment)` per I-P1-11 —
   `security_id` alone is NOT unique.

4. **SEGMENT codes (single char in CSV):** `C`=Currency, `D`=Derivatives,
   `E`=Equity, `I`=Index, `M`=Commodity. The numeric ExchangeSegment enum
   (`IDX_I=0`, `NSE_EQ=1`, `NSE_FNO=2`, …) is in `08-annexure-enums.md` rule 2.

5. **InstrumentType — derivative prefixes carry expiry; INDEX/EQUITY do NOT.**
   The 8 derivative types `FUTIDX`/`OPTIDX`/`FUTSTK`/`OPTSTK`/`FUTCUR`/`OPTCUR`/
   `FUTCOM`/`OPTFUT` have a real `SM_EXPIRY_DATE`. `INDEX`/`EQUITY` rows ship the
   placeholder `0001-01-01` (NOT empty) — never flag them malformed-expiry
   (regression 2026-05-29, PR #868).

6. **Daily refresh, idempotent UPSERT, NEVER DELETE.** The `instrument_lifecycle`
   master is UPSERTed daily keyed on `(security_id, exchange_segment)`; rows that
   disappear flip to an `expired_*` `lifecycle_state` — they are NEVER deleted
   (SEBI 5-year point-in-time, lock §25). No `DELETE FROM instrument_lifecycle`.

7. **Applicable-F&O master vs subscription set (lock §5, operator Quote 5
   2026-05-29).** `instrument_lifecycle` stores the indices + F&O underlying
   spots + **applicable F&O contracts** (FUTSTK/OPTSTK for resolved underlyings +
   FUTIDX/OPTIDX for tracked indices; currency/commodity EXCLUDED). The
   **WebSocket subscription is the 331-SID indices+spots subset plus the §36.7
   all-monthly-expiry FUTIDX contracts of the 4 underlyings (2026-07-10, promoted into
   `subscription_targets` at build time)** — the
   subscription dispatcher reads `DailyUniverse::subscription_targets`, NEVER the
   contract rows. The `[100,400]` `MAX_DAILY_UNIVERSE_SIZE` envelope bounds the
   SUBSCRIPTION set, not the lifecycle master (~219K applicable-F&O rows is fine).

8. **Dangling-reference reject.** Every `UNDERLYING_SECURITY_ID` cited by a
   FUTSTK/OPTSTK row MUST resolve to an NSE_EQ row in the SAME CSV; > 0.5%
   dangling → reject the CSV (lock §3 / INSTR-FETCH-03). A contract whose
   underlying does not resolve is NOT "applicable" and is excluded from the master.

9. **CSV parser robustness (lock §26).** Strip BOM, normalize CRLF/LF/CR, handle
   quoted commas, reject non-UTF-8, strip BiDi overrides in `symbol_name`.

10. **No `f64::from(f32)` in storage; round prices to 2dp at write (PR #864).**
    `instrument_lifecycle.tick_size`/`strike_price` are DOUBLE from the CSV
    (already f64 text) — parse directly; do not widen an f32.

## What This Prevents

- Hardcoded SecurityIds → silent wrong-instrument on relist
- Compact CSV used for F&O → missing underlying/strike/option columns
- Per-segment REST fallback sneaking into the daily-universe path → lock §3 violation
- INDEX/EQUITY rows flagged malformed-expiry → ~120 bogus Telegram errors/boot (PR #868)
- `security_id`-only collections → cross-segment collision (I-P1-11)
- DELETE on the lifecycle master → SEBI retention breach
- Confusing the master (full applicable-F&O) with the subscription set (331) → either a 2-WS-lock violation OR a too-small master

## Trigger

This rule activates when editing files matching:
- `crates/core/src/instrument/csv_downloader.rs`
- `crates/core/src/instrument/csv_parser.rs`
- `crates/core/src/instrument/daily_universe.rs`
- `crates/core/src/instrument/fno_underlying_extractor.rs`
- `crates/core/src/instrument/index_extractor.rs`
- `crates/app/src/today_instrument.rs`
- `crates/app/src/lifecycle_reconcile_orchestrator.rs`
- `crates/app/src/daily_universe_boot.rs`
- `crates/storage/src/instrument_lifecycle_persistence.rs`
- Any file containing `api-scrip-master`, `UNDERLYING_SECURITY_ID`, `DailyUniverse`,
  `instrument_lifecycle`, `FnoUnderlyingExtraction`, `build_daily_universe`,
  `InstrumentRole`, `extract_fno_underlyings`
