# FUTIDX-4 Index-Futures Subscription — Error Codes (FUTIDX-01 / FUTIDX-02)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `daily-universe-scope-expansion-2026-05-27.md` §36 (the 2026-07-08 operator grant) >
> `groww-second-feed-scope-2026-06-19.md` §36 > this file.
> **Operator directive (2026-07-08, verbatim):** *"for both dhan and groww we need to add
> futures and those also should be subscribed along with this, especially only for nifty
> banknifty and sensex nifty midcap."*
> **Companion code:** `crates/core/src/instrument/index_futures.rs`
> (`INDEX_FUTURES_UNDERLYINGS`, `select_index_future_expiry`,
> `select_index_future_contracts`, `record_index_future_selection`,
> `compare_index_future_selections`),
> `crates/common/src/error_code.rs::ErrorCode::{Futidx01SelectionDegraded,
> Futidx02CrossFeedExpiryMismatch}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to
> mention every `Futidx0*` variant verbatim — `Futidx01SelectionDegraded` and
> `Futidx02CrossFeedExpiryMismatch` appear below.

---

## §0. Why these codes exist

The §36 grant subscribes exactly FOUR nearest-expiry index-futures contracts (NIFTY,
BANKNIFTY, MIDCPNIFTY on NSE_FNO; SENSEX on BSE_FNO) on BOTH feeds. Selection is a pure
function of (instrument master, IST trading date), evaluated once per build, per feed. Every
selection failure is a per-underlying DEGRADE (the spot universe is never affected, boot never
HALTs), and every degrade or cross-feed divergence is LOUD — never silently absorbed.

## §1. FUTIDX-01 — index-future selection degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already happened — that feed simply
runs WITHOUT that future for the day; the operator inspects the master CSV at leisure).

**Trigger:** at the Dhan ~08:45 daily-universe build (`ErrorCode::Futidx01SelectionDegraded`,
emitted from `daily_universe_orchestrator.rs`) or the Groww watch-set build
(`groww_activation.rs` / `instruments.rs` extraction), ≥1 of the 4 underlyings resolved ZERO
valid nearest-expiry contracts. Reasons (the `reason` payload field):

| Reason | Meaning |
|---|---|
| `NoFutRows` | the master carried no FUTIDX/FUT rows for the underlying on its expected exchange/segment |
| `AllExpiriesPast` | every parsed expiry < today (data anomaly — stale master) |
| `AmbiguousDuplicateExpiry` | ≥2 rows share the chosen (underlying, expiry) — fail-closed, never guess a SID/token |
| `BadExpiryFormat` | every candidate row's expiry was unparsable (skipped + counted) |

**Consequence:** that feed runs WITHOUT that future for the day — degrade, never HALT; the
spot universe is unaffected. Payload: `feed`, `underlying`, `reason`, `candidates_seen`
(distinct `underlying_symbol` values seen among FUT rows, bounded — so an alias can be
extended with evidence, never guessed). Counter:
`tv_index_futures_selection_missing_total{feed, underlying}` (static labels, 2×4 bounded).
Gauge: `tv_index_futures_selected{feed}` (0-4).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `FUTIDX-01`; read `feed`, `underlying`, `reason`.
2. Inspect the day's master CSV for the underlying's FUT rows (Dhan Detailed CSV `FUTIDX`
   rows / Groww master `instrument_type=FUT, segment=FNO` rows).
3. `INDEX_SYMBOL_ALIASES` drift (esp. MIDCPNIFTY — "NIFTY MIDCAP SELECT" literal): the
   FUTIDX-01 payload lists the distinct `underlying_symbol` values seen so an alias can be
   extended with evidence in a dated commit, never guessed.
4. A one-off on one feed self-heals at the next day's build; sustained misses = vendor master
   regression — record a dated note.

## §2. FUTIDX-02 — cross-feed expiry mismatch

**Severity:** High. **Auto-triage safe:** No (data-comparability signal; operator must decide
which vendor master is stale).

**Trigger:** the boot-time comparator (`compare_index_future_selections`, fired from
`record_index_future_selection` once BOTH feeds have recorded — order-independent;
single-feed runs never fire it) found `expiry(dhan, U) != expiry(groww, U)` or one-sided
presence for an underlying U (`ErrorCode::Futidx02CrossFeedExpiryMismatch`).

**Consequence:** BOTH feeds STAY LIVE (visibility, never a halt); cross-feed rows for U are
not comparable that day. Counter: `tv_index_futures_parity_mismatch_total`.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — the payload carries `underlying`, `dhan_expiry`,
   `groww_expiry` (None = missing on that feed).
2. One vendor's master is stale/divergent — compare the two masters' FUT rows for U;
   record a dated note. Do NOT hand-patch either selection: both re-select from fresh
   masters at the next build.

## §3. Companion observability (documented here, no own ErrorCode)

- `tv_prev_day_futidx_skipped_total` — the prev-day OHLCV REST loop skips IndexFuture-role
  targets (Dhan-historical FUTIDX support + expiryCode convention UNVERIFIED-LIVE per
  annexure rule 8); `*_pct_from_prev_day` columns read 0, fail-soft PREVDAY-01 semantics.
- `tv_cross_verify_futidx_skipped_total` — the 15:31 IST 1m cross-verify stays spot-only by
  construction; futures are skipped + counted.
- Honest consequence: `ticks.oi = 0` for the 4 futures (Quote mode; the separate code-5 OI
  packet is counted-and-dropped by the tick processor today — a future dated-quote feature).
- Boot evidence lines: one structured `info!` per feed per boot —
  `index-futures selection feed=<f> underlying=<U> expiry=<date> native_id=<id> segment=<seg>`
  — plus the parity OK/mismatch verdict line.

## §4. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `Futidx0*` variant)
- `crates/core/src/instrument/index_futures.rs`
- Any file containing `FUTIDX-01`, `FUTIDX-02`, `Futidx01SelectionDegraded`,
  `Futidx02CrossFeedExpiryMismatch`, `INDEX_FUTURES_UNDERLYINGS`, or
  `select_index_future_expiry`
