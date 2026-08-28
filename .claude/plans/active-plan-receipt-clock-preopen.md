# Implementation Plan: Receipt-clock OHLCV, 09:00 candles, per-minute ATM re-fit, percentage columns

**Status:** APPROVED
**Date:** 2026-08-28
**Approved by:** Parthiban (operator) — verbatim, this session: *"Yes fox and resolve everything always ensure to start eelvery candles starting at 9 am dude okay"* / *"Meanwhile ensure to achieve this ohlcv based on one and only received at dude okay?"* / *"See clealry ensure whenever the pre marketbope ticks and candles get finished ensure to provide this fucking atm plus minus depths also dude and starting 9.16 am every one minute resubscribe also right dude of current atm and what about pre oopem marketbpercentage change and even percentage change also dude okay?"*
**Authority:** `.claude/rules/project/websocket-connection-scope-lock.md` § "2026-08-28 — CANDLES FROM 09:00, OHLCV ON THE RECEIPT CLOCK…" (landed before this plan, per the rule-file-first law)
**Crates touched:** `tickvault-storage`, `tickvault-core`, `tickvault-trading`, `tickvault-app`, `tickvault-common`

---

## Design

Five workstreams. Ordered so each is independently revertible and so the
prerequisite lands before the change that depends on it.

### W1 — `TVW3`: make `received_at` truthful across replay  *(prerequisite for W2)*

The true receipt instant is stamped in `FrameSink::accept` (`pool_supervisor.rs`)
**before** the WAL append, then discarded because the WAL record carries only
`ws_type`, `frame_seq` and the frame bytes. `refold_wal_frames` therefore
re-stamps with `Utc::now()` — measured to place 9.1% of a session's ticks 9–20
hours away from their true receipt.

Mint WAL magic `TVW3` = `[magic(4)][ws_type:u8][frame_seq:u64][received_at_nanos:i64][len:u32][frame][crc32:u32]`,
following the exact precedent by which `TVW2` added `frame_seq` beside `TVW1`.
Replay accepts all three magics; `TVW1`/`TVW2` records replay with `0`, which the
persistence layer already maps to NULL — a missing timestamp, never a false one.
No data migration. Cost: 8 bytes/frame and one clock read per frame.

### W2 — Bucket every candle on `received_at`

`AggregatorCell::fold` currently buckets on `tick.exchange_timestamp`. Switch to
the receipt clock, converted to IST seconds-of-day. This is a single bucketing
call plus the gates and watermarks that read the same field.

Receipt is monotone per drain, and replay runs strictly before any socket opens,
so the late-arrival/refold policy becomes structurally unreachable. It is
**retained and counted, not deleted** — a policy that flatlines is evidence the
switch worked; a deleted one leaves no way to detect a violated assumption.

### W3 — Candle session opens at 09:00

Move `MARKET_OPEN_SECS_OF_DAY_IST` 33_300 → 32_400 in the trading crate only.
Sever the compile-time assert binding it to `MARKET_OPEN_IST_NANOS`, deliberately
and with a comment, so the day-OHLC and tick-persistence gates stay at 09:15 and
the operator's own 2026-08-25 pre-open rule is not breached.

### W4 — Percentage columns

Four columns already exist on every `candles_<tf>` table, are written on every
row, and all carry `0.0`; the component that computed them was deleted 2026-07-19.
Both inputs — `prev_day_close` and `session_open` — are already populated per tick
into the same `LiveCandleState`. Compute at seal time, guarded against a zero or
non-finite previous close.

### W5 — Per-minute ATM re-fit from 09:16, additive-only

A 60-second timer on the attach task recomputes the ATM ±25 selection, diffs
against what was sent, and forwards **only additions** through the existing
top-up channel. Never a swap: the transport has no `send_unsubscribe`, the
subscribe guard's instrument set is the reconnect replay, and error 805 is a
disconnect.

Measured reality, recorded so the mechanism is understood: median strike spacing
2.63% of price, average intraday drift 0.8 strikes, worst-case 6 strikes against
a 25-strike half-window, and 39% of ladders already fully covered. On a normal
day this sends nothing.

---

## Edge Cases

| # | Case | Handling |
|---|---|---|
| E1 | Legacy `TVW1`/`TVW2` record replayed after W1 | `received_at = 0` → NULL, never a synthesized time. Counted separately from live frames. |
| E2 | Frame received 15:29 yesterday, replayed at tomorrow's boot | Carries yesterday's receipt → targets a bucket sealed a day earlier. Replay runs before sockets open, so it cannot race live traffic; the late policy decides, and the outcome is counted. |
| E3 | Multi-socket stamp/send interleave | Skew is microseconds; a later-stamped frame can enter the ring first. Within one bucket, so bucketing is unaffected. |
| E4 | Equity with no pre-open auction print | 09:00–09:14 buckets never open → **no rows**. Not a flat bar. Downstream must read absence as normal. |
| E5 | Pre-open tick spends `volume_baseline_seeded` | The 09:07 auction print becomes the volume baseline, so its own bar reports 0 volume. Pre-existing behaviour, now reachable 15 minutes earlier. |
| E6 | `prev_day_close` is 0 or non-finite | Percentage stays `0.0` and increments a refusal counter. Never Inf/NaN. |
| E7 | ATM re-fit when the ladder is already fully covered (39% of names) | Delta is empty; nothing sent; costs one set-difference. |
| E8 | Re-fit delta exceeds the per-minute cap | Fails CLOSED — stops adding, counts, logs. Window frozen at its current width, never silently narrowed. |
| E9 | Aggregator slot ceiling reached mid-session | Existing fail-closed refusal + counter. Re-fit stops adding. |
| E10 | Top-up channel disarmed on `Disconnected` | Existing bug: `topup = None` is permanent. Must be held for the session or minute 2 is silently ignored. |
| E11 | Muhurat / delayed open | Every constant here is a fixed IST second. Out of scope for this plan and recorded as an open risk. |
| E12 | Cross-verify sees pre-open live rows | Must EXCLUDE them, not count them `missing_rest`. |

---

## Failure Modes

| Mode | Blast radius | Detection |
|---|---|---|
| `TVW3` writer ships without the replay reader | Replay rejects every new record; WAL frames unrecoverable | CRC/magic mismatch counter; replay-parse ratchet covering all three magics |
| Clock switched without W1 | Measured: 8.8% of a session misfiled, 4 phantom bars, 4,319 ticks on the wrong day | Ordering guard: W2 must not merge before W1 |
| Both session gates moved together | Pre-open ticks enter day high/low/close — breaches the 2026-08-25 operator rule | The day-gate tests are explicitly NOT re-blessed; they fail if the gate moves |
| Cross-verify window widened to 09:00 | Every instrument reports 09:00–09:14 `missing_rest` daily; ground truth destroyed | Its 385-minute asserts are left intact and will fail |
| Percentage divides by a zero close | Inf/NaN into a stored column, and this codebase has a documented NaN-poisoning incident | Guard + refusal counter + a test feeding 0.0 and non-finite |
| Re-fit runs on the reader task | Keepalive stalls while subscribing → socket drop → tick loss | Re-fit lives on the attach task; only the delta crosses the channel |
| Re-fit sends full sets | 230 messages/minute unpaced → error 805 disconnect | Delta-only + 25 ms inter-batch pacing |
| Slot reuse without cell reset | Foreign volume baseline → the documented 9.2× volume corruption | Not in scope: this plan never releases slots |

---

## Test Plan

| # | Test | Pins |
|---|---|---|
| T1 | `tvw3_roundtrip_preserves_received_at` | Write `TVW3`, replay, assert the receipt nanos survive exactly |
| T2 | `tvw1_and_tvw2_records_replay_with_null_receipt` | Legacy records yield `0`/NULL, never `now()` |
| T3 | `replay_never_restamps_receipt_with_now` | Source-scan: `refold_wal_frames` contains no `Utc::now()` on the receipt path |
| T4 | `candles_bucket_on_receipt_not_exchange_time` | A tick whose two clocks straddle a minute boundary lands in the receipt bucket |
| T5 | `a_replayed_tick_lands_in_its_original_minute` | The regression the whole of W1 exists to prevent |
| T6 | `candle_session_opens_at_0900` | 09:00:00 accepted, 08:59:59 refused |
| T7 | `day_ohlc_gate_still_opens_at_0915` | **Must NOT be re-blessed** — proves the two gates were severed |
| T8 | `crossverify_window_still_starts_at_0915` | Ground truth intact |
| T9 | `preopen_minute_with_no_ticks_emits_no_row` | Absence is normal, not a synthetic bar |
| T10 | `pct_change_computed_from_prev_close` | Known inputs → known percentage |
| T11 | `pct_change_refuses_zero_or_nonfinite_prev_close` | Stays 0.0, counter increments, no Inf/NaN |
| T12 | `open_gap_pct_is_the_preopen_equilibrium_gap` | Uses `session_open`, not the first traded price |
| T13 | `atm_refit_sends_only_additions` | Never emits an unsubscribe or a full set |
| T14 | `atm_refit_fails_closed_at_the_per_minute_cap` | Stops adding, counts, does not narrow |
| T15 | `atm_refit_does_not_run_on_the_reader_task` | Source-scan: the timer is on the attach task |
| T16 | `dhat_receipt_bucketing_zero_alloc` | The clock switch adds no hot-path allocation |

Scope: `cargo test -p tickvault-storage -p tickvault-core -p tickvault-trading -p tickvault-app`, escalating to `--workspace` because `tickvault-common` constants move.

---

## Rollback

Each workstream is independently revertible, and W1 is forward-compatible in both
directions:

- **W1** — replay accepts `TVW1`/`TVW2`/`TVW3`, so reverting the writer leaves
  every existing record readable. No migration to undo.
- **W2** — one constant selecting the clock source; revert restores exchange
  bucketing. Rows already written keep their stamps, so a revert produces a
  seam in the data, not corruption.
- **W3** — one constant, 32_400 → 33_300.
- **W4** — the columns already exist and already carry `0.0`; reverting returns
  them to `0.0`, which is the current production state.
- **W5** — config-gated off; the timer simply does not arm.

The riskiest combination is W2 without W1, which is why the ordering is a
gate rather than a preference.

---

## Observability

| Signal | Kind | Purpose |
|---|---|---|
| `tv_wal_records_replayed_total{magic}` | counter | Proves `TVW3` is actually being written and read |
| `tv_wal_replay_null_receipt_total` | counter | Legacy records replayed with no receipt — should fall to zero within a day |
| `tv_candle_late_bucket_total{policy}` | counter | Should FLATLINE after W2. A non-zero value means receipt is not monotone — the assumption is violated and we want to know |
| `tv_candle_preopen_bars_total{segment}` | counter | How much the 09:00 window actually yields, per segment |
| `tv_pct_refused_total{reason}` | counter | Zero/non-finite previous close |
| `tv_atm_refit_runs_total{outcome}` | counter | `no_change` / `added` / `capped` — measured expectation is overwhelmingly `no_change` |
| `tv_atm_refit_strikes_added_total` | counter | Real intraday drift, measured in production rather than modelled |

Dashboard: the three counters that can indicate a broken assumption
(`late_bucket`, `replay_null_receipt`, `atm_refit{capped}`) go on the operator
dashboard. Alarms are a separate change and are NOT bundled here — a new pager
needs its own dated authorization under the noise lock.

---

## Plan Items

- [ ] **W1** `TVW3` WAL record carries the true receipt time; replay restores it
  - Files: `crates/storage/src/ws_frame_spill.rs`, `crates/core/src/websocket/pool_supervisor.rs`, `crates/app/src/dhan_feed_stack.rs`
  - Tests: T1, T2, T3

- [ ] **W2** Candles bucket on `received_at`
  - Files: `crates/trading/src/candles/aggregator_cell.rs`, `crates/trading/src/candles/multi_tf_aggregator.rs`, `crates/trading/src/candles/tf_index.rs`
  - Tests: T4, T5, T16

- [ ] **W3** Candle session opens 09:00; day-OHLC and cross-verify stay 09:15
  - Files: `crates/trading/src/candles/tf_index.rs`, `crates/app/src/tf_consistency_boot.rs`
  - Tests: T6, T7, T8, T9

- [ ] **W4** Compute the four percentage columns at seal time
  - Files: `crates/trading/src/candles/aggregator_cell.rs`, `crates/trading/src/candles/live_candle_state.rs`, `crates/storage/src/shadow_seal_columns.rs`
  - Tests: T10, T11, T12

- [ ] **W5** Per-minute additive ATM re-fit from 09:16
  - Files: `crates/app/src/dhan_feed_stack.rs`, `crates/core/src/websocket/pool_supervisor.rs`, `crates/common/src/config.rs`, `config/base.toml`
  - Tests: T13, T14, T15

---

## Scenarios

| # | Scenario | Expected |
|---|---|---|
| 1 | Normal session, no spill | Candles identical to today (measured: 0 of 83,871 ticks change bucket) |
| 2 | Disk saturates, frames spill and replay | Ticks land in their ORIGINAL minute, not at 18:00/20:00/05:00 |
| 3 | Boot replays yesterday's WAL | Bars stamped yesterday; late policy decides; counted |
| 4 | Equity with no pre-open print | No rows 09:00–09:14; one bar at the auction print |
| 5 | Index from 09:00 | Full coverage across every emitted timeframe |
| 6 | Stock with a zero previous close | Percentages stay 0.0; refusal counted; no NaN |
| 7 | Quiet day, ATM does not drift | Re-fit sends nothing, 375 times |
| 8 | Stock gaps 15% intraday | Re-fit adds ~6 strikes, inside cap and inside slot headroom |
| 9 | Slot ceiling reached | Re-fit stops adding, counts, logs; existing subscriptions unaffected |
| 10 | W2 merged without W1 | Blocked by the ordering gate; if forced, scenario 2 regresses to the measured 8.8% loss |

---

## Per-Item Guarantee Matrix

Per `.claude/rules/project/per-wave-guarantee-matrix.md`. Applies to every
workstream W1–W5 in this plan.

### 15-row "100% everything" matrix

| Demand | Proof artefact for THIS plan | Status |
|---|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` per-crate floors; coverage delta ≥ 0 on the PR | enforced by CI |
| 100% audit coverage | WAL replay outcome counters + the coded rollback error (W1); `rest_fetch_audit` unchanged | W1 partial |
| 100% testing coverage | T1–T16 in the Test Plan above; the 22 categories apply to storage/core/trading/app | 6 of 16 written |
| 100% code checks | banned-pattern, pub-fn-test, pub-fn-wiring, plan-verify, secret-scan, fmt, clippy | green |
| 100% code performance | DHAT zero-alloc on the fold (T16); the TVW3 append adds 8 bytes on an already-moved struct, no allocation | T16 pending |
| 100% monitoring | the 7 counters in Observability above | 2 of 7 written |
| 100% logging | every refusal path carries `code = ErrorCode::…`; the rollback path gains one (W1 defect 2) | in progress |
| 100% alerting | deliberately NOT bundled — a new pager needs its own dated authorization under the noise lock | stated, not built |
| 100% security | no secret touches this path; the WAL carries frame bytes only | n/a |
| 100% security hardening | the receipt is banded like the exchange timestamp (W1 defect 4), so a corrupt value cannot become a designated timestamp | in progress |
| 100% bugs fixing | adversarial agent pass run BEFORE the code shipped; 2 CRITICAL + 1 MEDIUM found in my own TVW3 work and being fixed | done, fixes in progress |
| 100% scenarios covering | 10 scenarios in the Scenarios table; rollback added as #11 | in progress |
| 100% functionalities covering | every new pub fn has a test or a TEST-EXEMPT naming a real test (verified: both named tests now exist and pass) | done for W1 |
| 100% code review | adversarial pass before impl (done) and after the diff (pending) | half |
| 100% extreme check | bite-proofs: 3 run on TVW3, each observed to FAIL with the fix reverted, then restored | done for W1 |

### 7-row resilience matrix

| Demand | Honest envelope for THIS plan | Per-item proof |
|---|---|---|
| Zero ticks lost | W1 makes replayed ticks land in their ORIGINAL minute instead of the replay minute. Bounded: it cannot recover a frame the WAL never received | T1, T5; bite-proven |
| WS never disconnects | Untouched. W5 must not send subscribes on the reader task, because the keepalive is only emitted while it polls | T15 |
| Never slow/locked/hanged | TVW3 adds 8 bytes and no allocation to the append; the extra clock read is on the caller, not this path | T16 (DHAT) |
| QuestDB never fails | Untouched. The receipt reaches `received_at`, never the dedup key | existing dedup guard |
| O(1) latency | Append O(1) zero-alloc; replay parse O(bytes), boot only; percentages O(1); the ATM re-fit is O(strikes)/minute OFF the drain — flagged, not claimed O(1) | stated honestly above |
| Uniqueness + dedup | `received_at` is deliberately NOT in any DEDUP key — it is re-stamped on replay today, which is why it would create duplicate rows. W1 does not change that | `DEDUP_KEY_TICKS` unchanged |
| Real-time proof | the 7 counters above; the three that can indicate a broken assumption go on the operator dashboard | 2 of 7 written |

### Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the TVW3
record round-trips a caller-supplied receipt exactly, rejects a truncated v3
tail without panicking, detects a receipt-byte flip by CRC, and accounts its own
on-disk size — each bite-proven by reverting the fix and observing the guard
fail. v1 and v2 segments still replay, with the sentinel rather than a
synthesized timestamp. **NOT claimed:** that the receipt reaches the database —
the two stamping sites are not yet wired, so on the box today every v3 record
carries the sentinel; that a rollback to a pre-v3 binary is safe (it is not, and
the fix is in this plan); or that any workstream beyond W1 exists in code.
