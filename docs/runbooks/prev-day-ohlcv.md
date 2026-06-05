# Runbook: Pre-market prev-day OHLCV fetch + coverage verification

> **What this answers:** "Where do I SEE the pre-market historical fetch, and
> did it actually get yesterday's data for everything?"
> **Authority:** `.claude/rules/project/live-feed-purity.md` rule 9 (the
> bounded prev-day fetch).
> **Code:** `crates/app/src/prev_day_ohlcv_boot.rs`, wired in
> `crates/app/src/main.rs`.
> **Sibling:** `docs/runbooks/cross-verify-1m.md` (the post-market twin).

## TL;DR

| Question | Answer |
|---|---|
| Pre-market historical fetch? | **Exists & runs at boot** — pulls yesterday's single daily candle (O/H/L/C/V + OI) per subscribed spot SID → `prev_day_ohlcv` table. |
| Is it verified? | **Yes (new)** — after the fetch, a coverage check confirms how many of the ~243 SIDs got a valid candle; below 90% → degraded, zero → empty (no false-OK). |
| Fastest way to see it? | `make prev-day-show` |

## What it does

At boot (after auth + instruments load), `run_prev_day_ohlcv_fetch` fetches
**yesterday's one daily candle** per subscribed spot SID via Dhan REST
`POST /v2/charts/historical` (daily, non-inclusive `toDate`) and writes it to
the `prev_day_ohlcv` table — **never** `ticks` (live-feed-purity stands). This
is the bounded fetch re-allowed 2026-06-01; it is **not** the deleted broad
intraday chain.

Then the new coverage verification (`evaluate_prev_day_coverage`, a pure
unit-tested function) compares `fetched` against the universe size:

| Outcome | Meaning | Signal |
|---|---|---|
| `ok` (≥ 90%) | Healthy — most/all symbols have yesterday's candle | `info!` "PROOF: prev-day OHLCV coverage OK" |
| `degraded` (< 90%, > 0) | Some symbols missing yesterday's candle | `warn!` (→ Telegram) |
| `empty` (0) | Nothing fetched — investigate | `error!` (→ Telegram) |

`ok` is **never** reported on an empty/thin set (audit-findings Rule 11).

## Where a human can see it

| Surface | How |
|---|---|
| 📄 **CSV** | `make prev-day-show` (or open `data/prev-day/prev-day-coverage-YYYY-MM-DD.csv`) — columns: `expected, fetched, skipped, failed, coverage_pct, outcome` |
| 📊 **QuestDB** | `make questdb` → `SELECT count(*) FROM prev_day_ohlcv` (the actual candles) |
| 📝 **Logs** | the `PROOF: prev-day OHLCV coverage OK` / `coverage DEGRADED` / `coverage EMPTY` line in `data/logs/errors.jsonl.*` |

## "I can't see anything" — why

- The fetch runs **once at boot** after successful auth. On a dev box with no
  live Dhan credentials it is skipped (logged), so there is no report — that is
  correct, and `make prev-day-show` says so.
- Coverage `empty` on a real boot means the REST fetch returned nothing for the
  whole universe — check Dhan Data-API health + the JWT.

## Honest scope

This is **cold-path operator tooling + a completeness check** — it does **not**
touch the hot path, so no O(1) / zero-tick-loss claims apply. It does **not**
re-add the deleted broad pre-market intraday chain (that would reverse the
2026-05-26 lock); it only verifies + surfaces the already-existing bounded
prev-day fetch.

## Cross-references

- `.claude/rules/project/live-feed-purity.md` rule 9 — the bounded prev-day fetch contract
- `docs/runbooks/cross-verify-1m.md` — the post-market 1-minute twin
