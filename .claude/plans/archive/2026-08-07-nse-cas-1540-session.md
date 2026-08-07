# Implementation Plan: NSE CAS + F&O 15:40 session close (effective 2026-08-03)

> **ARCHIVED 2026-08-07** — shipped in PR #1724 (squash `993a144c`, merged
> 06:50:12Z by `github-actions[bot]` after `All Green` succeeded on head
> `3ba410c7`). Archived per `plan-enforcement.md` rule 7 (mandatory once the
> PR merges; the stale-plan pile is what made the design-first wall vacuous
> in the 2026-07-10 incident).
>
> **ONE CARRY-FORWARD RESIDUAL — do not lose it.** Item 3 shipped **PARTIAL**
> and is ticked only because its ACTIVE half (the constants + the boundary
> predicate) landed. Still NOT built: the CAS row **tagging** and the
> `tv_cas_window_rows_total` counter promised in this plan's Design and
> Observability sections. `is_in_cas_window()` is therefore a **dormant
> pub fn** carrying a `WIRING-EXEMPT` that names the gap rather than an
> invented call site. The next session owning CAS work must either wire the
> tag + counter or delete the predicate — archiving this plan does **not**
> discharge that obligation.

**Status:** VERIFIED (merged)
**Date:** 2026-08-07
**Approved by:** Parthiban (operator) — "yes bro", in direct response to
"Want me to fix the 15:40 close?", this session. Operator originally spotted
the change himself ("startign august 3 timign got changed right", then
"im talkign newer cas bro" — CAS = Closing Auction Session).

## The fact (Verified, external sources)

Effective **Monday 2026-08-03**, NSE changed the close:

| Segment | Before | From 2026-08-03 |
|---|---|---|
| Equity derivatives (F&O) continuous | 15:30 | **15:40** (+ post-close 15:50–16:00) |
| F&O-constituent CASH stocks continuous | 15:30 | **15:15**, then CAS |
| Closing Auction Session (CAS) | did not exist | **15:15 → 15:35** (price discovery 15:30–15:35) |
| Non-F&O cash stocks | 15:30 | 15:30 (unchanged) |
| VWAP window | 15:00–15:30 | **15:10–15:40** |

tickvault hardcodes `MARKET_CLOSE_IST_NANOS = 55_800_000_000_000` (15:30:00)
and const-asserts two further constants equal to it. **Every trading day since
2026-08-03 we have silently stopped capturing 10 minutes before the F&O market
actually closes** — and those are the minutes containing the closing auction
outcome, which is the whole reason NSE extended derivatives trading.

Our captured universe is NIFTY / BANKNIFTY / SENSEX / INDIA VIX spot plus
their option chains and index futures. The chains and futures are equity
derivatives → they trade to **15:40**. The indices are computed from
constituents that are F&O stocks → they enter CAS at 15:15 and their official
close is discovered 15:30–15:35. So a single session-close move to **15:40**
covers the whole captured universe; no per-segment split is required *for what
we capture today*. (A per-segment model WOULD be required if non-F&O cash
equities were ever added — recorded in Edge Cases.)

## Design

Single source of truth stays `MARKET_CLOSE_IST_NANOS`. Move it 55_800 →
**56_400** seconds (15:40:00) and let the existing const-asserts propagate the
change to `SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST` and
`FOLD_SESSION_CLOSE_SECS_OF_DAY_IST`, which already assert equality — that
assertion chain is exactly the mechanism that makes this a safe one-value
change rather than a scattered hunt.

Add a NEW, separate constant for the CAS window
(`CAS_WINDOW_OPEN/CLOSE_SECS_OF_DAY_IST` = 15:15 / 15:35) used ONLY to tag
rows/telemetry, not to gate capture. Capture must NOT stop during CAS — the
auction is precisely what we want recorded.

Post-close session (15:50–16:00) is deliberately OUT OF SCOPE: it is a
separate NSE session with different semantics, we have never captured it, and
adding it is a scope expansion needing its own operator quote.

## Edge Cases

- **CAS window prints are not continuous-trading prints.** Between 15:15 and
  15:35 an index value reflects auction transition, not continuous trading.
  Rows in that window are TAGGED (`in_cas_window`) so downstream analysis can
  separate them; they are never dropped and never silently blended.
- **15:30 is no longer "the last minute".** Code that treated 15:30 as the
  session's final cycle (cadence `decision.rs`, `runner.rs`) must now treat
  15:40 as final; the 15:31 post-session sweep must move after 15:40.
- **The 16:30 IST box stop is unaffected** — still ~50 min after the new close.
- **Backfill/sweep semantics unchanged** — repaired rows remain
  record-only per §38.8 decision-freshness; extending the window does not make
  a late row a decision input.
- **A non-F&O cash instrument added later would close at 15:30, not 15:40** —
  a single global constant would then over-run its session. Flagged, not
  solved here; today's universe has no such instrument.
- **BSE/SENSEX**: SENSEX derivatives are BSE_FNO. Sources cover NSE explicitly;
  BSE alignment is ASSUMED, not verified. Over-running by 10 min on a BSE
  contract costs at worst empty fetches (counted, not fatal) — the fail-safe
  direction.
- Holiday list is 2026-only with no staleness guard (pre-existing, separate).

## Failure Modes

| Mode | Detection | Result |
|---|---|---|
| Vendor serves no data 15:30–15:40 | existing per-minute `empty` counters + `rest_fetch_audit` outcome | loud, counted; never a silent gap |
| A const-assert I missed fires | compile error | build fails — cannot ship half-applied |
| A ratchet pins 55_800 literally | test failure | caught pre-merge |
| Over-run on a segment that really closed at 15:30 | empty-fetch counters rise for those 10 min | visible; fail-safe |
| Deployed mid-session | — | NOT DONE: merge only; deploy post-close |

## Test Plan

- `cargo test -p tickvault-common` — constants + gate tests (`g1_exchange_gate`,
  `g2_wall_clock_gate` boundary asserts at the new close)
- `cargo test -p tickvault-app` — `rest_candle_fold` session window,
  `day_ohlc_orchestrator` session-accept boundaries
- `cargo test -p tickvault-core` — cadence `decision`/`runner` last-cycle
- `cargo test -p tickvault-trading` — `tf_index`, `spot_bar_store`
- New boundary tests: 15:29:59 accepted, 15:30:00 accepted (was rejected),
  15:39:59 accepted, 15:40:00 rejected.
- New: CAS-window tagging unit test (15:14:59 out, 15:15:00 in, 15:34:59 in,
  15:35:00 out).

## Rollback

Single-value revert: `MARKET_CLOSE_IST_NANOS` back to 55_800_000_000_000; the
assert chain drags the dependents back with it. No schema change, no data
migration, no config flag — a revert commit fully restores prior behaviour.
Rows already captured in the extra 10 minutes remain valid and DEDUP-keyed.

## Observability

- The extra 10 minutes appear in the existing per-minute `rest_fetch_audit`
  rows and `close_to_data_ms` histograms — no new metric needed to SEE them.
- New counter `tv_cas_window_rows_total` so the auction window's volume is
  visible rather than inferred.
- The 15:45 daily scorecard line gains the new close time so a future reader
  can tell which session model produced a given day's data.

## Plan Items

- [x] Item 1 — move `MARKET_CLOSE_IST_NANOS` 55_800 → 56_400 and fix the
      const-assert chain
      - Files: `crates/common/src/constants.rs`
      - Tests: `test_market_close_is_1540`, existing g1/g2 gate boundary tests
- [x] Item 2 — update dependents that hardcode 15:30 semantics
      - Files: `crates/app/src/rest_candle_fold.rs`,
        `crates/app/src/day_ohlc_orchestrator.rs`,
        `crates/core/src/cadence/{decision,runner}.rs`
      - Tests: session-window boundary tests per crate
- [x] Item 3 — CAS window constants + `is_in_cas_window()` predicate
      - Files: `crates/common/src/constants.rs`
      - Tests: `test_cas_window_boundaries` (all four edges)
      - **PARTIAL, stated honestly:** the row TAGGING and the
        `tv_cas_window_rows_total` counter promised in Design/Observability are
        NOT built. The pub-fn wiring guard correctly flagged the predicate as
        dormant; it carries a `WIRING-EXEMPT` naming that gap rather than an
        invented call site. Scope call: this PR fixes the ACTIVE daily data
        loss (the 15:30 close); tagging is additive and can land with its
        consumer. Follow-up: wire the tag + counter, or delete the predicate.
- [x] Item 4 — update ratchets/docs that pin the old close
      - Files: affected `crates/*/tests/*.rs`, rule/docs references
      - Tests: full per-crate suites green

## Per-Item Guarantee Matrix

Per `.claude/rules/project/per-wave-guarantee-matrix.md`. Rows honestly marked
N/A where they do not apply — this is a session-boundary constant change, not
a new subsystem.

| Demand | Proof for THIS item |
|---|---|
| 100% code coverage | boundary tests added both sides of the new close; per-crate floors unchanged |
| 100% audit coverage | existing `rest_fetch_audit` covers the new minutes — no new event type |
| 100% testing coverage | unit + boundary + const-assert (compile-time) |
| 100% code checks | fmt + clippy + banned-pattern + pre-push gates |
| 100% code performance | N/A — compile-time constant, zero runtime cost |
| 100% monitoring | new `tv_cas_window_rows_total` counter |
| 100% logging | existing coded per-minute logs cover the window |
| 100% alerting | N/A — no new failure mode; empty-fetch counters already alert |
| 100% security | N/A — no input, no secret, no surface change |
| 100% security hardening | N/A |
| 100% bugs fixing | fixes a live silent data loss running since 2026-08-03 |
| 100% scenarios covering | CAS window, boundary minutes, holiday, BSE assumption — all in Edge Cases |
| 100% functionalities covering | no new pub fn without test |
| 100% code review | adversarial review before merge |
| 100% extreme check | const-assert chain fails the BUILD on any inconsistent value |

| Resilience demand | Envelope |
|---|---|
| Zero ticks lost | strictly INCREASES capture (10 min/day recovered) |
| WS never disconnects | N/A — no live WS in the runtime |
| Never slow/locked | N/A — compile-time constant |
| QuestDB never fails | unchanged; ~10 extra rows/day/instrument |
| O(1) latency | unchanged — no hot-path code touched |
| Uniqueness + dedup | unchanged DEDUP keys; extra minutes are new distinct `ts` |
| Real-time proof | new minutes visible in existing per-minute audit rows |

## Honest 100% claim

100% inside the tested envelope: the session close moves to the NSE-published
15:40 for the captured F&O universe, with compile-time assertion that every
dependent constant moves in lockstep and boundary tests on both sides of the
new close. NOT claimed: that BSE_FNO (SENSEX) adopted the identical 15:40
close — sources cover NSE explicitly and BSE is ASSUMED aligned; the fail-safe
direction is empty fetches, which are counted and visible. NOT claimed: the
post-close 15:50–16:00 session (deliberately out of scope). NOT claimed: that
CAS-window index prints are equivalent to continuous-trading prints — they are
tagged precisely because they are not.

## Auto-driver explanation

> Sir, the market office changed its closing bell on 3 August. Options and
> futures now trade ten minutes longer, till 3:40, because the shops now hold
> a final auction between 3:15 and 3:35 to decide the true closing price. Our
> boy was still packing up at 3:30 — so every single day since, we have missed
> the last ten minutes, which are the most important ten minutes, when the real
> closing price is decided. This change keeps him at the counter till 3:40, and
> he now writes a small mark on any price card that came from the auction
> period, so we never confuse an auction price with a normal one.
