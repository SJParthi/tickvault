# Runbook: Post-market 1-minute cross-verification

> **What this answers:** "Where, as a human, do I SEE the historical
> cross-verification?" — and "is it even running?"
> **Authority:** `.claude/rules/project/live-feed-purity.md` rule 11,
> `.claude/rules/project/cross-verify-1m-error-codes.md`.
> **Code:** `crates/app/src/cross_verify_1m_boot.rs`,
> `crates/storage/src/feed_parity_1m_audit_persistence.rs`, wired in
> `crates/app/src/main.rs`.

## TL;DR

| Question | Answer |
|---|---|
| Pre-market historical fetch? | **Removed on purpose** (operator directive 2026-05-26, spot-only). It does not run. |
| Post-market cross-verification? | **Runs at 15:31 IST every trading day** — 1-minute OHLCV, exact match, per subscribed spot SID. |
| Fastest way to see it? | `make cross-verify-show` |
| See it without waiting for 15:31? | `make cross-verify-now` (needs a booted app: live auth + QuestDB) |

## What it does

At **15:31:00 IST** each trading day, for every subscribed **spot** instrument
(the ~243 daily-universe SIDs — indices + F&O underlyings) it fetches Dhan's
authoritative intraday **1-minute** candles (`POST /v2/charts/intraday`,
interval `"1"`) and compares OHLCV **timestamp-by-timestamp, EXACT match**
against our live `candles_1m`. The per-day **mismatch count** is the quality
signal — a stable low number is sampling noise (our feed is sampled; Dhan's
candles are built from their full tape), a sudden spike or sustained
open/close drift is a real problem.

It is **read-only** on `candles_1m`; it writes only to its own audit
table + CSV. It never writes to `ticks` (live-feed-purity stands).

## The 4 places a human can see it

| Surface | How | Note |
|---|---|---|
| 📄 **CSV** | `make cross-verify-show` (or open `data/cross-verify/cross-verify-1m-YYYY-MM-DD.csv`) | Easiest. One row per mismatching `(SID, minute, field)`. |
| 📊 **QuestDB** | `make questdb` → `localhost:9000` → `SELECT * FROM cross_verify_1m_audit WHERE trading_date_ist = today()` | Full, queryable, trendable across days. |
| 📱 **Telegram** | per-day summary at ~15:31 IST: *compared / mismatches / missing* | Pushed to you. |
| 📝 **Logs** | `CROSS-VERIFY-1M-01` (mismatch) / `-02` (fetch degraded) in `data/logs/errors.jsonl.*`, plus a `"PROOF: cross_verify_1m fired"` info line | Forensic. |

## "I can't see anything" — why, and what to do

1. **It only fires at 15:31 IST on a trading day, with the app running.**
   If the app wasn't up through a trading-day close, there is no report — that
   is correct, not a bug. `make cross-verify-show` says so plainly.
2. **Dev box without live Dhan + QuestDB candles** produces nothing real — and
   the tooling will not fabricate a sample. `make cross-verify-now` will boot
   and stop at auth if there are no live credentials; that is expected.
3. **Booted late (after 15:31)** → the scheduled run skips ("mid-evening
   boot"). Use `make cross-verify-now` to force it for today's data.

## On-demand run (prove it works)

`make cross-verify-now` sets `TICKVAULT_CROSS_VERIFY_NOW=1`, which makes the
boot-time task run the verification **immediately** instead of sleeping to
15:31. It overrides the trading-day + schedule gates (so you can run it any
time) but **not** correctness: on a quiet day with no `candles_1m`, it
produces an empty/degraded report — never invented data.

The scheduling decision is a pure, unit-tested function
(`decide_cross_verify_start` in `cross_verify_1m_boot.rs`); the env var only
flips its `force_now` input.

## Cross-references

- `.claude/rules/project/cross-verify-1m-error-codes.md` — CROSS-VERIFY-1M-01/02 triage
- `.claude/rules/project/live-feed-purity.md` rule 11 — the narrowed re-allow
- `docs/runbooks/phase-1-monitoring-rubric.md` — cross-verify match-rate target
