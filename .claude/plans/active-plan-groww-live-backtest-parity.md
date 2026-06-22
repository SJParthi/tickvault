# Implementation Plan: Groww live→backtest 1-minute parity check

**Status:** APPROVED
**Date:** 2026-06-22
**Approved by:** Parthiban — standing authorization in `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §0 Quote 1 ("generate our one minute especially to check whether atleast groww live feed data and groww backtesting is same or not dude") + 2026-06-22 "continue with the plan dude okay?"
**Authority for the feature:** the Groww-second-feed lock §0 + §5 (honest envelope) — this IS the locked deliverable of the Groww sequence, not new scope.

## Design

Goal: every trading day after close, prove whether **Groww's live 1-minute candles == Groww's own backtest (historical) 1-minute candles**, exact-match, minute-by-minute, per subscribed Groww spot SID — and count the mismatches. This is the whole reason Groww was added (an independent second source to measure Dhan's live-feed drift).

The research map (parallel agent, 2026-06-22) confirms the LIVE side + comparator + audit sink already exist:
- LIVE candles: `candles_1m WHERE feed='groww'` (shared table, DEDUP `(ts, security_id, segment, feed)`).
- Pure exact-match comparator: `crates/core/src/feed/groww/parity_1m.rs::compare_groww_1m` (zero-tolerance, false-OK guarded).
- Audit sink (built, unwired): `groww_cross_verify_1m_audit` table + `GrowwCrossVerify1mAuditWriter` (DEDUP `(trading_date_ist, security_id, segment, minute_ts_ist, field)`).
- Mirror template: `crates/app/src/cross_verify_1m_boot.rs` (the Dhan 15:31 IST cross-verify).

Two things are MISSING and this plan adds them:
1. **Backtest fetcher** — pull Groww historical 1m candles. The growwapi SDK exposes `get_historical_candles(exchange, segment, groww_symbol, start_time, end_time, candle_interval=CANDLE_INTERVAL_MIN_1)` (verified `docs/groww-ref/08-master-groww-nse-bse-context.md:83-95`). No code calls it yet. Reuse the EXISTING sidecar venv + auth: a one-shot Python fetch (`scripts/groww-sidecar/groww_backtest_fetch.py`) that, given today's Groww watch list, fetches each SID's 1m candles for the session and dumps `data/groww/backtest-1m-<date>.ndjson` (one `{security_id,segment,minute_ts_ist_nanos,open,high,low,close,volume}` per line). Mirrors the live-capture NDJSON contract so the Rust loader is symmetric.
2. **Parity orchestrator** — `crates/app/src/groww_cross_verify_1m_boot.rs`, mirroring `cross_verify_1m_boot.rs` scoped to `feed='groww'`: schedule 15:31 IST (+ force-now flag), (a) read live `candles_1m WHERE feed='groww'`, (b) load the backtest NDJSON, (c) `compare_groww_1m`, (d) flush mismatches to `groww_cross_verify_1m_audit` + a CSV `data/cross-verify/groww-parity-1m-<date>.csv`, (e) Telegram per-day summary (compared / mismatches / missing). Boot-wire `ensure_groww_cross_verify_1m_audit_table` + spawn the scheduler, gated by `feeds.groww_enabled`.

**Volume handling (honest envelope, MANDATORY):** Groww LIVE feed carries no traded volume — the sidecar stamps `volume=0` (Option A, `groww_sidecar.py:144`). Groww BACKTEST candles DO carry volume. So a full OHLCV exact-compare would mismatch volume on EVERY minute — a false alarm. Therefore the Groww parity comparator compares **OHLC exact-match only**; volume is reported separately as informational (live=0 by design), never counted as a mismatch. This is documented in the new rule file + the Telegram wording ("OHLC exact; volume not compared — Groww live carries no volume").

**Sub-PR split (per stream-resilience B per-PR-set protocol, ≤3):**
- **Sub-PR 1 (this plan, first):** backtest fetcher (Python one-shot + Rust NDJSON loader `groww_backtest_loader.rs` pure-parse + the OHLC-only comparator option in `parity_1m.rs`) + tests. No boot wiring yet (loader is pure, unit-tested).
- **Sub-PR 2:** the parity orchestrator + boot wiring + error codes (`GROWW-PARITY-01` mismatch, `GROWW-PARITY-02` backtest-fetch-degraded) + rule file `groww-parity-1m-error-codes.md` + CSV + Telegram. Mirrors `cross_verify_1m_boot.rs`.

## Edge Cases

- Groww disabled (`feeds.groww_enabled=false`): orchestrator never spawns; no fetch, no compare. No-op.
- Empty live candles (Groww ran but no ticks today): comparator's `compared_minutes>0` false-OK guard reports "blind / nothing to compare", NOT "all match".
- Backtest fetch partial/failed for some SIDs: degrade honestly (`GROWW-PARITY-02`), compare what we have, report coverage — never a clean "all match" on a partial set (audit Rule 11).
- Volume: excluded from mismatch count by design (see above); informational only.
- Backtest candle minute alignment: Groww historical tuple `[ts,o,h,l,c,v,oi]` UTC epoch → IST-minute nanos, same conversion the live aggregator uses, so keys line up exactly.
- Far-OTM / illiquid SID with no backtest minute: counted as `missing_in_backtest`, not a mismatch.
- 30-day per-request cap (1m): we only ever fetch ONE day (today), well inside the cap.

## Failure Modes

- Python fetch process crash / SDK auth failure: the orchestrator times out the fetch, emits `GROWW-PARITY-02` (Severity::High, degrade — never blocks), records partial coverage. Next day re-runs.
- QuestDB unreachable for the live-candle SELECT: degrade, emit, no panic (mirror `cross_verify_1m_boot` Blind/Degraded classification).
- Audit ILP write failure: best-effort forensic write; the CSV + Telegram still fire (mirror AUDIT-WS-01 honesty).
- NDJSON parse error on a backtest line: skip the line + count it, never panic (the loader is a pure fallible parser returning `Result`).

## Test Plan

Sub-PR 1:
- `groww_backtest_loader.rs`: pure-parse unit tests — valid line → row; malformed line → typed Err; UTC→IST-minute conversion; empty file → empty vec.
- `parity_1m.rs`: OHLC-only compare option — volume difference does NOT count as a mismatch; OHLC difference DOES; `compared_minutes==0` → not-exact (false-OK guard) retained.
- `scripts/groww-sidecar/groww_backtest_fetch.py`: a `--self-test` / dry mode that validates the watch-file parse + output schema without hitting the network (mirrors the sidecar's existing self-test pattern).
Sub-PR 2:
- orchestrator pure helpers (schedule decision, run-status classification, CSV row formatting) unit-tested; the I/O loop TEST-EXEMPT mirroring `cross_verify_1m_boot.rs`.
- error-code cross-ref + tag-guard satisfied by the new rule file.
- `cargo test -p tickvault-core -p tickvault-app -p tickvault-storage` green.

## Rollback

Both sub-PRs are additive + gated by `feeds.groww_enabled` (default OFF). Reverting either restores prior behaviour with zero data-model change (the `groww_cross_verify_1m_audit` table is `CREATE IF NOT EXISTS`; an unused table is harmless). The live feed, shared tables, and Dhan path are untouched.

## Observability

- `GROWW-PARITY-01` (mismatch found, Severity::High) + `GROWW-PARITY-02` (backtest fetch degraded, Severity::High) error codes → Telegram via the 5-sink pipeline + the `groww_cross_verify_1m_audit` forensic table + the CSV.
- `tv_groww_parity_mismatches_total` counter, `tv_groww_parity_compared_minutes` gauge.
- Per-day Telegram summary (compared / mismatches / missing / "OHLC exact; volume not compared").
- The mismatch count trend over days IS the quality signal (track trend, not absolute — sampling noise on H/L is expected, sustained O/C drift is the real problem).

## Plan Items

- [ ] Sub-PR 1 — Groww backtest fetcher (Python one-shot `groww_backtest_fetch.py` + Rust `groww_backtest_loader.rs` pure NDJSON loader) + OHLC-only compare option in `parity_1m.rs` + tests.
  - Files: `scripts/groww-sidecar/groww_backtest_fetch.py`, `crates/core/src/feed/groww/groww_backtest_loader.rs`, `crates/core/src/feed/groww/parity_1m.rs`, `crates/core/src/feed/groww/mod.rs`
  - Tests: `test_parse_backtest_line_valid`, `test_parse_backtest_line_malformed_errs`, `test_backtest_utc_to_ist_minute`, `test_ohlc_only_compare_ignores_volume`, `test_ohlc_compare_flags_close_drift`
- [ ] Sub-PR 2 — Groww parity orchestrator (`groww_cross_verify_1m_boot.rs`) + boot wiring + error codes + rule file + CSV + Telegram.
  - Files: `crates/app/src/groww_cross_verify_1m_boot.rs`, `crates/app/src/main.rs`, `crates/common/src/error_code.rs`, `.claude/rules/project/groww-parity-1m-error-codes.md`, `crates/storage/src/groww_cross_verify_audit_persistence.rs` (wire `ensure_*`)
  - Tests: orchestrator pure helpers + error-code cross-ref + tag-guard.

## Guarantee matrix

Carries the 15-row + 7-row matrix by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`. Item proof: 100% testing via the pure-parse + compare unit tests; 100% audit via `groww_cross_verify_1m_audit` DEDUP-keyed forensic table; 100% logging/alerting via `GROWW-PARITY-01/02` + Telegram + CSV; 100% scenarios via the empty/partial/volume/degrade edge cases; 100% performance via O(1) per-minute compare (HashMap keyed `(security_id, segment, minute)`); honest envelope: OHLC-only exact-match (volume excluded — Groww live carries no volume by design), partial-fetch degrades honestly, never a false "all match".

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Groww OFF | Orchestrator never runs; no-op |
| 2 | Live==Backtest OHLC | "All OHLC match", mismatch count 0 |
| 3 | Close drift on N minutes | N mismatches in audit + CSV + Telegram count |
| 4 | Volume differs (live=0, backtest>0) | NOT counted (informational only) |
| 5 | Backtest fetch fails for some SIDs | Degrade (GROWW-PARITY-02), partial coverage reported, never false "all match" |
| 6 | No live ticks today | "Blind — nothing to compare", not "all match" |
