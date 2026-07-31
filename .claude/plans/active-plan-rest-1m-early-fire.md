# Implementation Plan: REST 1m early-fire + self-tuning start offset

**Status:** APPROVED
**Date:** 2026-07-31
**Approved by:** Parthiban (operator) — "yes go ahead and ensure to use only RUST O(1) entirely everywhere"

## Operator directive (verbatim, typos preserved)

> "See even beofre 800 ms or just below one second i need the enitre spot and
> otpion chain data dude okay? so just go ahead ad check after 5ms itself right
> ebcuase anyhwo dhan and groww are fuckign fast to generte the mintue level
> candle within microseocnd or even 1 milliseocnds itself right dude if eys if
> that cna eb verified we dont need to even wait till 300 ms right ?"

Follow-up (the increment question this plan answers):

> "suppsoe in 5 ms if the data is not fetchabale then hwow ill you incrment the
> tiem and hwo will you fix that as the start time dude okay?"

## Design

**Problem.** The per-minute REST legs wait a HARD-CODED, never-measured beat
after each minute close before the first poll:

| Constant | Today | Origin |
|---|---|---|
| `SPOT_1M_REST_FIRE_DELAY_MS` | 300 | doc comment admits: "docs do NOT document just-closed-minute availability latency" — a guess |
| `GROWW_CHAIN_1M_FIRE_DELAY_MS` | 300 | same guess |
| `GROWW_SPOT_1M_TWO_WAVE_FIRE_DELAY_MS` | 1_350 | OUR rate-limit wave separation, not a vendor requirement |

Worst case, spot decision data lands ~1.55 s after close. The operator needs
it under ~800 ms, ideally at the vendor's true seal latency.

**Nobody has measured that latency.** `close_to_data_ms` is recorded as a
histogram but is NOT shipped to CloudWatch — verified 2026-07-31 by querying
both the metric namespace and the log group: zero events. So today the 300 ms
is a guess, and any hand-picked replacement would be another guess.

**Approach — three parts, in dependency order:**

1. **Move the first attempt to +5 ms; keep the old 300 ms as a LADDER RUNG.**
   The bounded in-minute re-poll ladder already handles "candle not there
   yet" — offsets are measured FROM THE FIRST ATTEMPT, so the whole mechanism
   works unchanged. Denser early rungs probe the unknown region:

   | Attempt | Today | Proposed |
   |---|---|---|
   | 1 | +300 ms | **+5 ms** |
   | 2 | +1_000 ms | +50 ms |
   | 3 | +1_800 ms | +150 ms |
   | 4 | +3_300 ms | **+300 ms** (today's start, now a fallback) |
   | 5 | +6_300 ms | +700 ms |
   | 6 | — | +1_500 ms |
   | 7 | — | +3_000 ms |

   Worst case is NO WORSE than today: attempt 4 lands exactly where we fire
   now. We only add three earlier chances.

2. **Measure which rung actually won.** New histogram
   `tv_rest1m_first_success_offset_ms{feed,leg}` recorded on the FIRST
   successful attempt of each (minute, target). This is the number that has
   never existed.

3. **Self-tune the start offset from the measurement.** An `OffsetTuner`
   (pure core, the existing `RpsTuner` pattern) keeps a bounded ring of recent
   first-success offsets and proposes
   `new_start = clamp(p95 - SAFETY_MARGIN_MS, FLOOR_MS, CEILING_MS)`,
   applied at most once per trading day. Converges on the vendor's real
   behaviour instead of a constant anyone hand-picked.

**O(1) discipline (operator mandate).** Every hot-path operation stays
constant-time and zero-allocation:

| Operation | Complexity | Note |
|---|---|---|
| record a sample | **O(1)** | fixed-size ring, index write, no alloc |
| ladder rung lookup | **O(1)** | const array index |
| jitter computation | **O(1)** | `slot * step`, pure arithmetic |
| propose new offset | **O(k)**, k = ring capacity (const 256) | runs ONCE per day on the cold path, never per fire — flagged honestly, not relabelled O(1) |

Ring capacity is a compile-time const, so the p95 scan is a fixed bounded
cost with no allocation. It is NOT claimed O(1); it is const-bounded cold-path
work.

## Plan Items

- [ ] Item 1 — Early-fire ladder constants
  - Files: `crates/common/src/constants.rs`
  - Tests: `test_fire_delay_is_five_ms`, `test_retry_ladder_strictly_increasing`,
    `test_ladder_worst_case_fits_inside_minute`,
    `test_ladder_covers_old_three_hundred_ms_rung`

- [ ] Item 2 — First-success-offset histogram
  - Files: `crates/common/src/constants.rs` (metric name const),
    `crates/app/src/spot_1m_rest_boot.rs`, `crates/app/src/groww_spot_1m_boot.rs`
  - Tests: `test_first_success_offset_recorded_once_per_target`,
    `test_no_record_when_all_rungs_miss`

- [ ] Item 3 — `OffsetTuner` pure core
  - Files: `crates/core/src/cadence/offset_tuner.rs` (new), `crates/core/src/cadence/mod.rs`
  - Tests: `test_p95_of_known_sample_set`, `test_clamped_to_floor`,
    `test_clamped_to_ceiling`, `test_empty_ring_proposes_no_change`,
    `test_ring_wraps_without_alloc`, `test_at_most_one_adjust_per_day`

- [ ] Item 4 — Wire the tuner into both legs (daily, cold path)
  - Files: `crates/app/src/spot_1m_rest_boot.rs`, `crates/app/src/groww_spot_1m_boot.rs`
  - Tests: `test_tuner_applied_once_per_trading_day`, `test_tuner_never_below_floor`

- [ ] Item 5 — DHAT zero-alloc proof for the record path
  - Files: `crates/core/tests/dhat_offset_tuner.rs` (new)
  - Tests: `test_record_sample_allocates_zero`

- [ ] Item 6 — Rule-file record + ratchet
  - Files: `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md` (dated
    subsection under §8/§9), `crates/storage/tests/rest_1m_early_fire_guard.rs` (new)
  - Tests: `test_rule_file_records_the_early_fire_directive`,
    `test_fire_delay_constants_pinned`

## Edge Cases

| # | Case | Handling |
|---|---|---|
| 1 | Vendor never serves at 5 ms | Rungs 2-7 catch it; tuner converges the start upward toward the real value. Cost: ~12 extra requests/min (4% of the 300/min budget). |
| 2 | Vendor serves at 5 ms every time | Tuner holds the start at the floor; data lands ~295 ms earlier than today. |
| 3 | 429 storm | Existing `record_429` → `RpsTuner` step-down is UNCHANGED. The offset tuner never raises request COUNT — rung count is fixed. |
| 4 | Clock skew / NTP step | Offsets are measured from the leg's own monotonic fire instant, never wall clock. |
| 5 | All rungs miss | No sample recorded (item 2 test) — a missing minute must never poison the tuner toward a false-fast offset. |
| 6 | First day, empty ring | `propose()` returns `None` — the configured start stands. |
| 7 | Mid-session respawn | Ring is run-scoped; tuner restarts from the configured start. Documented, not hidden. |
| 8 | Vendor latency changes intraday (expiry day) | At most one adjust/day by design — an intraday swing does not thrash the schedule. |

## Failure Modes

| Mode | Detection | Response |
|---|---|---|
| Early rungs burn budget with no gain | `tv_rest1m_first_success_offset_ms` p50 stays high | Tuner raises the start automatically; no human action |
| Tuner proposes an absurd offset | Clamp to `[FLOOR_MS, CEILING_MS]` — const-asserted | Impossible by construction |
| Extra rungs trip vendor rate limits | Existing 429 counters + `RpsTuner` | Existing step-down; rung count never increases |
| Ladder overruns the minute | `test_ladder_worst_case_fits_inside_minute` const-assert | Build fails |
| Tuner drifts the start below the floor | `test_tuner_never_below_floor` | Build fails |

## Test Plan

- Unit: pure `OffsetTuner` (p95, clamps, ring wrap, once-per-day gate)
- Property: proptest — any sample sequence yields an offset inside `[FLOOR, CEILING]`
- DHAT: `record_sample` allocates zero
- Boundary: empty ring, single sample, full ring, wrapped ring
- Const-assert: ladder strictly increasing; worst case < 60 s; floor ≤ start ≤ ceiling
- Ratchet: rule-file record + pinned constants
- Scoped run: `cargo test -p tickvault-common -p tickvault-core -p tickvault-app`

## Rollback

Single-constant revert: set `SPOT_1M_REST_FIRE_DELAY_MS` back to 300 and the
ladder to its previous four offsets; the tuner is gated by a config flag
(`[rest_1m_tuning] enabled`, serde default **false**) so disabling it restores
today's fixed-offset behaviour exactly. No schema change, no data migration,
no table touched — nothing to un-migrate.

## Observability

| Signal | Type | Purpose |
|---|---|---|
| `tv_rest1m_first_success_offset_ms{feed,leg}` | histogram | the measurement that has never existed |
| `tv_rest1m_offset_tuned_total{feed,leg,direction}` | counter | how often the start moved, and which way |
| `tv_rest1m_ladder_rung_used{feed,leg,rung}` | counter | which rung wins in practice |
| coded `info!` on each daily adjust | log | names old → new offset + the p95 it came from |

No new Telegram page, no new CloudWatch alarm — this is a tuning signal, not
a failure class. It rides the existing SPOT1M-01 / CHAIN-02 runbooks.

## Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: the
ladder is const-asserted strictly-increasing and proven to fit inside the
minute; the tuner is a pure function with clamps proven by unit + property
tests; the record path is DHAT-proven zero-alloc; worst-case timing is no
worse than today because the current 300 ms start survives as rung 4.
**NOT claimed:** that the vendor actually serves at 5 ms — that is precisely
what the 5 ms rung MEASURES; the first live session is the probe. **NOT
claimed:** that `propose()` is O(1) — it is a const-bounded O(k) scan on the
daily cold path, stated plainly rather than relabelled. **NOT claimed:** any
change to decision-making — the §38.8 decision-freshness gate is untouched
and no strategy consumes these tables.

## Per-Item Guarantee Matrix

Canonical definition: `.claude/rules/project/per-wave-guarantee-matrix.md`.
Filled in for THIS plan (every row names a real artefact, or `N/A — <reason>`).

### 15-row "100% everything" matrix

| Demand | Proof artefact for this plan |
|---|---|
| 100% code coverage | `OffsetTuner` is a pure module — unit + proptest cover every branch (p95, both clamps, empty ring, wrap, once-per-day gate); coverage delta ≥ 0 against the ratcheted core/common floors |
| 100% audit coverage | N/A — no new persisted event. The legs' existing `rest_fetch_audit` row already names each minute's outcome; this plan changes WHEN we ask, never WHAT we record |
| 100% testing coverage | Categories exercised: 1 smoke · 2 happy-path · 3 error · 4 edge · 5 boundary · 6 property · 7 DHAT · 13 panic-safety · 22 integration |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan, all pre-push |
| 100% code performance | DHAT proves `record_sample` allocates zero; `propose()` is const-bounded O(k=256) on the DAILY cold path — labelled O(k), never relabelled O(1) |
| 100% monitoring | 3 new signals: `tv_rest1m_first_success_offset_ms{feed,leg}` histogram · `tv_rest1m_offset_tuned_total{feed,leg,direction}` · `tv_rest1m_ladder_rung_used{feed,leg,rung}` |
| 100% logging | one coded `info!` per daily adjust, naming old → new offset + the p95 it came from |
| 100% alerting | N/A — deliberate. This is a tuning signal, not a failure class; it rides the existing SPOT1M-01 / CHAIN-02 runbooks. No new page, no new alarm |
| 100% security | No new network surface, no new credential path, no new endpoint — only the timing of existing authorized calls changes |
| 100% security hardening | N/A — no attack-surface delta |
| 100% bugs fixing | Adversarial 3-agent pass (hot-path + security + hostile) before and after implementation |
| 100% scenarios covering | 8 edge cases enumerated in Edge Cases above, each with its handling |
| 100% functionalities covering | Every new pub fn gets a test AND a call site (pre-push gates 6 + 11) |
| 100% code review | 3-agent pass on the diff, both directions |
| 100% extreme check | 4 const-asserts fail the BUILD on regression: ladder strictly increasing · worst case < 60 s · floor ≤ start ≤ ceiling · old 300 ms rung still present |

### 7-row Resilience matrix

| Demand | Honest envelope for this plan |
|---|---|
| Zero ticks lost | No new drop path. Rung COUNT is bounded and the ladder still gives up cleanly; a missed minute is counted, never silently skipped |
| WS never disconnects | N/A — REST legs only; no WebSocket touched |
| Never slow/locked/hanged | `record_sample` is O(1) zero-alloc (DHAT-proven); the daily `propose()` runs on the cold path, never inside a fire |
| QuestDB never fails | N/A — no persistence path touched; no table, no DEDUP key, no schema |
| O(1) latency | record / rung-lookup / jitter are O(1); `propose()` is const-bounded O(k) and is FLAGGED as such, not claimed O(1) |
| Uniqueness + dedup | N/A — no new persisted row; existing `(ts, security_id, exchange_segment, feed)` keys untouched |
| Real-time proof | The `first_success_offset_ms` histogram IS the proof — it is the measurement that has never existed in this system |
