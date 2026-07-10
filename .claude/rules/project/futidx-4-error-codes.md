# FUTIDX-4 Index-Futures Subscription — Error Codes (FUTIDX-01 / FUTIDX-02)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `daily-universe-scope-expansion-2026-05-27.md` §36 (the 2026-07-08 operator grant) >
> `groww-second-feed-scope-2026-06-19.md` §36 > this file.
> **Operator directive (2026-07-08, verbatim):** *"for both dhan and groww we need to add
> futures and those also should be subscribed along with this, especially only for nifty
> banknifty and sensex nifty midcap."*
> **Companion code:** `crates/core/src/instrument/index_futures.rs`
> (`INDEX_FUTURES_UNDERLYINGS`, `select_index_future_expiries`,
> `select_index_future_expiry`, `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING`,
> `select_index_future_contracts`, `record_index_future_selection`,
> `compare_index_future_selections`),
> `crates/common/src/error_code.rs::ErrorCode::{Futidx01SelectionDegraded,
> Futidx02CrossFeedExpiryMismatch}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to
> mention every `Futidx0*` variant verbatim — `Futidx01SelectionDegraded` and
> `Futidx02CrossFeedExpiryMismatch` appear below.

---

## §0. Why these codes exist

The §36 grant subscribes ALL available monthly-expiry index-futures contracts (`>= today`,
envelope ≤6 serials per underlying) of exactly FOUR underlyings (NIFTY, BANKNIFTY,
MIDCPNIFTY on NSE_FNO; SENSEX on BSE_FNO) on BOTH feeds (§36.7, 2026-07-10; the nearest
expiry is the first of each set). Selection is a pure
function of (instrument master, IST trading date), evaluated once per build, per feed. Every
selection failure is a DEGRADE — whole-underlying for zero-candidate reasons, or
per-(underlying, expiry) for month-scoped reasons (2026-07-10; the spot universe is never
affected, boot never HALTs) — and every degrade or cross-feed divergence is LOUD, never
silently absorbed.

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
| `AmbiguousDuplicateExpiry` | ≥2 TRULY-DISTINCT ids share the chosen (underlying, expiry) — fail-closed, never guess a SID/token; per-(underlying, expiry) — drops ONLY that month (2026-07-10), other months of the underlying still subscribe. Hostile-review round 2 (2026-07-08): vendor-glitch EXACT-duplicate lines (same `SECURITY_ID` / same `exchange_token`) collapse first-row-wins (index_extractor precedent) BEFORE this count — a duplicated line no longer drops the mandated future |
| `BadExpiryFormat` | every candidate row's expiry was unparsable (skipped + counted) |
| `BadNativeToken` | (Groww only, 2026-07-08 hostile-review round 1) every candidate row's `exchange_token` was non-numeric with otherwise-valid expiries — the id-space triage arm, distinct from `BadExpiryFormat` |
| `SameExpiryCandidateFlood` | (2026-07-08 hostile-review round 3, both feeds) more than `FUTIDX_SAME_EXPIRY_CANDIDATE_CAP` (16) rows matched the chosen (underlying, expiry) — corrupt/flooded vendor master; fail-closed degrade, INSTR-FETCH-04 envelope discipline; per-(underlying, expiry) — drops ONLY that month (2026-07-10). The exact-duplicate dedup below the cap is HashSet-based O(n) (round 3 — was an O(n²) scan on unbounded untrusted rows) |
| `MonthlySerialFlood` | (§36.7, 2026-07-10) more than `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING` (6) DISTINCT future expiries for one underlying — corrupt/flooded master; the WHOLE underlying degrades fail-closed (never truncated-and-trusted) |

**Consequence:** a whole-underlying reason drops the underlying for the day; a per-month
reason drops ONLY the named month (2026-07-10) — the payload's `expiry` field (`"ALL"` =
whole underlying) says which. Degrade, never HALT; the
spot universe is unaffected. Payload: `feed`, `underlying`, `reason`, `expiry`,
`candidates_seen`
(distinct `underlying_symbol` values seen among FUT rows, bounded — so an alias can be
extended with evidence, never guessed). Counter unchanged:
`tv_index_futures_selection_missing_total{feed, underlying}` (static labels, 2×4 bounded —
a month-miss increments the same per-underlying series; the month lives in the `error!`
payload only).
Gauge: `tv_index_futures_selected{feed}` (selected CONTRACT count, 0..~12, envelope ≤24 —
semantics changed 2026-07-10 from underlying-count 0-4; the gauge is /metrics-local, not
shipped to CloudWatch, so no alarm continuity impact). Hostile-review round 2 (2026-07-08): the
gauge is POST-truth on BOTH feeds — `feed="dhan"` is set by the PLAN builder from the
post-plan `IndexDerivative` count (a planner-stage drop — unparsable SID / unknown segment /
composite-key dedup — fires a FUTIDX-01-coded `error!`, mirroring the Groww post-cap fix),
and `feed="groww"` is set from the POST-cap live set. FUTIDX-01 additional causes:
`tv_index_futures_dedup_dropped_total{feed="groww"}` — an intra-futures duplicate
`exchange_token` collapsed by the watch-set dedup (vendor id-space corruption) is its own
loud cause and is NEVER misattributed to a cap override (`expected` for the cap check is
the distinct-key count).

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

**Severity:** High. **Auto-triage safe:** **No** — hostile-review round 3 (2026-07-08)
reconciled the enum to the design contract (THIS section + the R3-4 record in
`.claude/plans/active-plan-futidx-4.md` — the in-repo authority; round 4 (2026-07-08)
replaced a dangling scratchpad "FINAL.md D0.5/T3.4" citation that never landed in the
tree): `is_auto_triage_safe()`
now carries a severity-independent override arm returning `false` for FUTIDX-02, so ANY
consumer of the documented API gets the correct answer (previously the blanket `!Critical`
derivation reported `true` and the drift was papered over here in prose). A
data-comparability signal is never auto-actioned — the operator decides which vendor
master is stale. Pinned by `test_futidx_codes_contract`.

**Trigger:** the boot-time comparator (`compare_index_future_selections`, fired from
`record_index_future_selection` once BOTH feeds have recorded — order-independent;
single-feed runs never fire it) compared
the sorted expiry SET per underlying U across feeds (§36.7, 2026-07-10; same-trading-date
gate unchanged). A divergence in any COMPARABLE month — the nearest month differs, a
one-sided month at or below the other feed's max expiry (a HOLE), or an underlying present
on only one feed — fires FUTIDX-02 naming the month(s) (`dhan_only` / `groww_only` date
lists in the payload; `ErrorCode::Futidx02CrossFeedExpiryMismatch`). A pure FAR-SUFFIX
depth difference (every one-sided month lies strictly beyond the shorter feed's max —
vendors publish far serials at different times) is NOT a page: one coalesced `info!` +
`tv_index_futures_parity_depth_mismatch_total{underlying}`. Policy: degrade-to-info for
suffix depth, page for everything else — vendor far-serial publication lag is a
legitimate, self-healing state, while any comparable-month divergence keeps the original
2026-07-08 detection guarantee.

**Same-trading-date gate (2026-07-08 hostile-review round 1):** each recorded selection
carries its IST trading date; the comparator fires ONLY when both feeds recorded for the
SAME date. A cross-date pair (e.g. a Groww re-activation the day AFTER a Dhan boot, across
an expiry boundary) is REFUSED — one coalesced `info!` line, the stale (older-dated)
feed's entry is evicted so a fresh same-date record re-pairs — never a false FUTIDX-02.
**Canonical provenance:** both feeds record the EXACT-match canonical from their selector
(`canonicalize_index_symbol` equality); substring re-derivation from symbol strings is
FORBIDDEN (pre-fix `"BANKNIFTY…".contains("NIFTY")` mislabeled BANKNIFTY/MIDCPNIFTY as
NIFTY → 2 false FUTIDX-02 pages every dual-feed boot).
**Round-4 hardening (2026-07-08):** (a) the Groww side records its parity entry (and its
FUTIDX-01 miss emissions) ONLY AFTER `assemble_watch_set` succeeds — mirror of the Dhan
Step-3d post-build ordering — so a day where the watch build persistently fails
(NtmDanglingExceeded / UniverseSizeOutOfBounds) never re-fires the FUTIDX-02 comparator
per ≤300s retry attempt (edge discipline, audit Rule 4) and never logs "parity OK" for a
feed that subscribed nothing (audit Rule 11). (b) The §29 warm-snapshot loader's
IndexFuture arm fail-closes on a NON-PARSEABLE expiry or NON-CANONICAL underlying (not
merely None/empty — the same two gates `dhan_selections_from_universe` applies), dedups
exact-duplicate future entries first-entry-wins, and fail-closes on two DISTINCT SIDs for
one underlying — a corrupt snapshot can no longer produce a subscribed-but-parity-
invisible future (false one-sided FUTIDX-02) or a spurious planner-drop FUTIDX-01.
(c) Plan-snapshot writes stamp the boot outcome's `build_trading_date_ist` (the date the
successful build's FUTIDX selection actually used, re-derived per §4 retry attempt) —
never the frozen boot-entry date, so a midnight-crossing cold build cannot write a
D-labeled snapshot carrying the D+1 front month.

**Consequence:** BOTH feeds STAY LIVE (visibility, never a halt); cross-feed rows for U are
not comparable that day. Counter: `tv_index_futures_parity_mismatch_total`.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — the payload carries `underlying`, `dhan_only`,
   `groww_only` (sorted month lists present on only that feed; §36.7 set semantics).
2. One vendor's master is stale/divergent — compare the two masters' FUT rows for U;
   record a dated note. Do NOT hand-patch either selection: both re-select from fresh
   masters at the next build.

## §3. Companion observability (documented here, no own ErrorCode)

- `tv_prev_day_futidx_skipped_total` — the prev-day OHLCV REST loop skips IndexFuture-role
  targets (Dhan-historical FUTIDX support + expiryCode convention UNVERIFIED-LIVE per
  annexure rule 8); `*_pct_from_prev_day` columns read 0, fail-soft PREVDAY-01 semantics.
  Counts ~12/day under §36.7 (was 4).
- `tv_cross_verify_futidx_skipped_total` — the 15:31 IST 1m cross-verify stays spot-only by
  construction; futures are skipped + counted (~12/day under §36.7).
- `tv_index_futures_parity_depth_mismatch_total{underlying}` — (§36.7, 2026-07-10) one
  increment per far-suffix depth note (log-sink-only companion, no alarm — same class as
  `tv_index_futures_cap_dropped_total`; static label, 4 canonicals).
- Alarm-gate note (§36.7, 2026-07-10): NON-NEAREST-month future SIDs are excluded from the
  SLO tick-freshness silent count and the `tv_tick_gap_instruments_silent` gauge
  (INDIA-VIX precedent — far months are legitimately sparse; threshold 40 and the 0.95 SLO
  boundary are unchanged). They remain SEEDED in the gap detector, so WS-GAP-06 still logs
  a never-ticking month per-SID — the live-delivery probe for months 2..N.
- `tv_index_futures_cap_dropped_total{feed="groww"}` — (2026-07-08 hostile-review round 1)
  the §36 futures are CAP-PRIORITY in the Groww live-subscribe set (prepended before the
  prefix truncate), and `tv_index_futures_selected{feed="groww"}` is set from the POST-cap
  live set (honest — never reports the full count while 0 survived). If an operator cap
  override ever drops a mandated future, this counter + a FUTIDX-01-coded `error!` fire —
  never a silent drop. The prev-day coverage denominator also EXCLUDES the always-skipped futures
  (a perfect day reads 100% again).
- Honest consequence: `ticks.oi = 0` for ALL §36 futures (~12; Quote mode; the separate
  code-5 OI packet is counted-and-dropped by the tick processor today — a future
  dated-quote feature).
- Warm-boot parity (hostile-review round 2, 2026-07-08): the §29 warm-snapshot boot path
  emits the SAME boot-evidence lines + Dhan parity entry via
  `record_dhan_selection_from_universe` (shared with orchestrator Step 3d) — previously
  deferred to the background reconcile and silently absent if it failed, so FUTIDX-02
  never ran on a warm-boot day.
- Midnight-spanning boot (round 2, F5): the §4 retry loop re-derives the IST trading date
  PER BUILD ATTEMPT (`run_daily_universe_fetch_runner` takes a date closure) — a boot
  crossing IST midnight (esp. the T-0→T+1 expiry crossing) selects with the NEW date,
  never the frozen boot-entry date. The `feed='groww'` FUTIDX lifecycle rows also now
  carry the canonical `underlying_symbol` (threaded via `WatchEntry.underlying_symbol`;
  `underlying_security_id` stays 0 — no numeric underlying id exists in the Groww space).
- Boot evidence lines: one structured `info!` per feed per boot —
  `index-futures selection feed=<f> underlying=<U> expiry=<date> native_id=<id> segment=<seg>`
  — plus the parity OK/mismatch verdict line.

## §4. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `Futidx0*` variant)
- `crates/core/src/instrument/index_futures.rs`
- Any file containing `FUTIDX-01`, `FUTIDX-02`, `Futidx01SelectionDegraded`,
  `Futidx02CrossFeedExpiryMismatch`, `INDEX_FUTURES_UNDERLYINGS`,
  `select_index_future_expiry`, `select_index_future_expiries`,
  `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING`, `MonthlySerialFlood`, or
  `tv_index_futures_parity_depth_mismatch_total`
