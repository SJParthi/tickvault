---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/app/src/prev_day_ohlcv_boot.rs"
---

# Boot-Time Previous-Day OHLCV Fetch — Error Codes (PREVDAY-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C > this file.
> **Operator directive (2026-06-01):** *"once authentication is successful and
> instruments get loaded successfully then instantly we planned to pull
> historical one day previous day data of all these subscribed symbols"* — the
> bounded one-day, one-candle prev-day fetch (`live-feed-purity.md` rule 9).
> **Companion code:** `crates/app/src/prev_day_ohlcv_boot.rs`,
> `crates/common/src/error_code.rs::ErrorCode::PrevDay01CoverageEmpty`,
> EMPTY-coverage emit site in `crates/app/src/main.rs`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `PrevDay01*` variant.

---

> **⚠ RETIREMENT AUTHORIZED 2026-07-13 — deletion LANDED in PR-C3 (2026-07-14);
> the `PrevDay01CoverageEmpty` variant DELETED in the C4 sweep (2026-07-15):
> `prev_day_ohlcv_boot.rs` is DELETED; the `prev_day_ohlcv` table is retained
> (forensic). Original authorization text follows:**
> `PREVDAY-01` (`PrevDay01CoverageEmpty`) and the boot-time prev-day OHLCV fetch retire
> with the Dhan live WS lane (Q1, 2026-07-13 — the fetch's universe input and its
> `*_pct_from_prev_day` consumers are lane-only; `websocket-connection-scope-lock.md`
> "2026-07-13 Amendment"). The retained Dhan REST surface is the §8 per-minute
> `spot_1m_rest` / `option_chain_1m` pulls on hardcoded SIDs — no prev-day sweep. The
> `prev_day_ohlcv` table is retained (forensic). Content below retained for historical
> audit.

## §0. Why this exists (the 2026-06-26 silent-empties signature)

After auth + instruments load, the boot-time prev-day fetch
(`run_prev_day_ohlcv_fetch`) requests yesterday's SINGLE daily candle
(O/H/L/C/V + OI) for every subscribed SID via Dhan REST
`POST /v2/charts/historical` (daily), and persists to the separate
`prev_day_ohlcv` table (NEVER `ticks`). It is fail-soft: a symbol Dhan can't
return is skipped; boot never blocks.

On 2026-06-26 the live boot logged 774 symbols counted as `skipped` SILENTLY —
the `Ok(None)` arm (Dhan returned 200 with an empty/malformed body) had NO log
and NO metric, so the empties were invisible, and the EMPTY-coverage `error!`
was untyped (no `code =` field). PREVDAY-01 closes both gaps: the EMPTY-coverage
ERROR is now typed, and each `Ok(None)` increments
`tv_prev_day_ohlcv_empty_total` with a single coalesced summary `debug!`.

---

## §1. PREVDAY-01 — previous-day OHLCV coverage EMPTY at boot

**Severity:** High. **Auto-triage safe:** Yes (visibility-only; boot is
fail-soft and never blocks — the operator informs upstream, no automated fix).

**Trigger:** `evaluate_prev_day_coverage(expected, fetched)` returned
`PrevDayCoverage::Empty` — i.e. ZERO yesterday candles were persisted for the
subscribed universe. Two sub-causes:
1. **Dhan 200-with-empty-body for every symbol** — the dominant case (the 774
   `Ok(None)` arms of 2026-06-26). Each is counted by
   `tv_prev_day_ohlcv_empty_total`.
2. **Empty universe** — `expected == 0` (e.g. Indices4Only scope, or the
   universe build produced no subscription targets).

**Consequence:** the prev-day reference cache has no rows until the next
successful boot, so the `*_pct_from_prev_day` columns (close/oi/volume) read 0.
No tick is lost; the live feed is untouched (separate path, separate table).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `PREVDAY-01`; note `expected` +
   `failed` on the payload.
2. `tv_prev_day_ohlcv_empty_total` rate — a high empty count with low `failed`
   means Dhan is returning 200-with-empty-body (data-plan / market-closed /
   far-OTM), NOT transport errors. Cross-check the REST canary (REST-CANARY-01)
   and `/v2/profile` data-plan status.
3. `mcp__tickvault-logs__questdb_sql "select count(*) from prev_day_ohlcv where ts > dateadd('d', -2, now())"`
   — confirm the table is genuinely empty for the window.
4. If `expected == 0`: the universe build is the root cause — check the
   daily-universe orchestrator (INSTR-FETCH-* / NTM-CONSTITUENCY-01) and the
   subscription scope.
5. Re-run / next boot re-attempts the fetch; there is no in-process retry of the
   whole pass (fail-soft per symbol with a bounded 429 backoff inside one pass).

**Honest envelope:** PREVDAY-01 is a boot-data-gap signal, not data loss — the
live `ticks` / `candles_*` path is unaffected and the next successful boot
re-populates `prev_day_ohlcv`.

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::PrevDay01CoverageEmpty`
- `crates/app/src/main.rs` (the `PrevDayCoverage::Empty` `error!` emit site)
- `crates/app/src/prev_day_ohlcv_boot.rs::run_prev_day_ohlcv_fetch`
  (`Ok(None)` arm → `tv_prev_day_ohlcv_empty_total` + coalesced `debug!`)

---

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `PrevDay01*` variant)
- `crates/app/src/prev_day_ohlcv_boot.rs`
- Any file containing `PREVDAY-01`, `PrevDay01`, `prev_day_ohlcv`, or
  `tv_prev_day_ohlcv_empty_total`
