# Phase 3 candle review — 2026-09-14

Candidate base: `af77fb0698aeb4414136767e4907593e90ac1ddf`; local follow-up additions are in `crates/trading/src/candles/aggregator_cell.rs` and `multi_tf_aggregator.rs` only. This is a source review. No current AWS/database access or candidate Rust execution was available to this agent. `git diff --check -- crates/trading/src/candles` passed. The two newly added Rust tests have **not been compiled or run**.

## Confirmed behavior and one local fix

| Item | Before phase 3 | Local candidate | Assurance limit |
|---|---|---|---|
| Official 09:15 open after pre-open activity | Every ordinary bucket rollover cleared the day-open arm and hardcoded `use_day_open=false`. A 09:00 → 09:01 → 09:15 sequence therefore left the 09:15 open at its first observed LTP even when the official open was present. | Preserve the arm through pre-open rollovers; consume it only in the bucket containing 09:15, or expire it after that bucket. | Source change; Rust execution pending. |
| Published opening high/low after pre-open predecessor | Ordinary rollover hardcoded `first_bucket_of_day=false`; reopening after catch-up also required no previous sealed bucket. The first regular bucket's published high/low could be omitted on its first tick. | Determine ownership by the bucket containing 09:15, independent of pre-open seals. Later buckets still do not inherit the whole day's range. | Same pending validation. No attempt to manufacture unavailable intrabar prices. |
| Counter-reset carry repair from previous candidate | Candidate already settles gross, signed and unknown-side carry before changing counter axes; catch-up-sealed predecessor may be amended. | Preserved. No additional reset changes in this phase. | Previous candidate's 48 reset traces and repeated-reset trace remain unexecuted Rust tests. |
| Absolute accuracy for every timestamp | Late carry deliberately shifts accepted volume into an available neighboring/current bucket. | Unchanged, explicitly disclosed. | Total conservation is not proof of exact bucket attribution. |

Concrete counterexample to the old opening branch: admit a 09:00 packet with `day_open=0`, a 09:01 packet, and a 09:15 packet with LTP 105, official open 98, high 110, low 95. The old M1 rollover branch cleared the arm at 09:01 and opened09:15 with open 105/high 105/low 105. The corrected branch's intended result is open 98/high 110/low 95/close 105. A successful compiler/test run is still required to certify this behavior.

Change references in the current files: `aggregator_cell.rs:1117–1121` (after catch-up), `:1276–1291` and `:1332–1342` (rollover), `:2712` (96-trace regression), `multi_tf_aggregator.rs:1734` (real ingest regression).

New test selectors:

- `all_frames_preserve_the_official_open_after_preopen_rollovers_and_catch_up`: 24 frames × catch-up on/off × official-open immediate/delayed = **96 input traces**. Checks opening open/high/low/close and ensures later bars do not inherit official day open or whole-day extremes.
- `preopen_ingest_preserves_the_official_open_in_all_24_frames`: uses real `MultiTfAggregator::consume_tick`, receipt timestamps, 09:00/09:05/09:14 packets and a 09:15 packet, checks all 24 states.

These are test definitions, **not 96 or 97 passing tests**. They add two test functions.

## Every timeframe and timestamp

All24 variants are computed for each accepted tick. Live emission is gated to 12 at the app seal sinks. The remaining 12 are computed but not emitted by that lane; table existence is separate from row population. Existing source comments still contain stale 21/13-frame and 09:15-grid statements; executable values below are authoritative for this review.

| Ordinal | Enum | Display | Seconds | Computed | Live rows emitted | Bucket containing 09:15 starts IST | Nominal exclusive end IST | Table |
|---|---|---|---:|---|---|---|---|---|
| 0 | `M1` | `1m` | 60 | Yes | Yes | `09:15:00` | `09:16:00` | `candles_1m` |
| 1 | `M3` | `3m` | 180 | Yes | Yes | `09:15:00` | `09:18:00` | `candles_3m` |
| 2 | `M5` | `5m` | 300 | Yes | Yes | `09:15:00` | `09:20:00` | `candles_5m` |
| 3 | `M15` | `15m` | 900 | Yes | Yes | `09:15:00` | `09:30:00` | `candles_15m` |
| 4 | `D1` | `1d` | 86,400 | Yes | No | `09:00:00` | `next day 09:00:00` | `candles_1d` |
| 5 | `S1` | `1s` | 1 | Yes | Yes | `09:15:00` | `09:15:01` | `candles_1s` |
| 6 | `S2` | `2s` | 2 | Yes | No | `09:15:00` | `09:15:02` | `candles_2s` |
| 7 | `S3` | `3s` | 3 | Yes | No | `09:15:00` | `09:15:03` | `candles_3s` |
| 8 | `S4` | `4s` | 4 | Yes | No | `09:15:00` | `09:15:04` | `candles_4s` |
| 9 | `S5` | `5s` | 5 | Yes | Yes | `09:15:00` | `09:15:05` | `candles_5s` |
| 10 | `S6` | `6s` | 6 | Yes | No | `09:15:00` | `09:15:06` | `candles_6s` |
| 11 | `S7` | `7s` | 7 | Yes | No | `09:14:56` | `09:15:03` | `candles_7s` |
| 12 | `S8` | `8s` | 8 | Yes | No | `09:14:56` | `09:15:04` | `candles_8s` |
| 13 | `S9` | `9s` | 9 | Yes | No | `09:15:00` | `09:15:09` | `candles_9s` |
| 14 | `S10` | `10s` | 10 | Yes | Yes | `09:15:00` | `09:15:10` | `candles_10s` |
| 15 | `S11` | `11s` | 11 | Yes | No | `09:14:51` | `09:15:02` | `candles_11s` |
| 16 | `S12` | `12s` | 12 | Yes | No | `09:15:00` | `09:15:12` | `candles_12s` |
| 17 | `S13` | `13s` | 13 | Yes | No | `09:14:57` | `09:15:10` | `candles_13s` |
| 18 | `S14` | `14s` | 14 | Yes | No | `09:14:56` | `09:15:10` | `candles_14s` |
| 19 | `S15` | `15s` | 15 | Yes | Yes | `09:15:00` | `09:15:15` | `candles_15s` |
| 20 | `S30` | `30s` | 30 | Yes | Yes | `09:15:00` | `09:15:30` | `candles_30s` |
| 21 | `M2` | `2m` | 120 | Yes | Yes | `09:14:00` | `09:16:00` | `candles_2m` |
| 22 | `M30` | `30m` | 1,800 | Yes | Yes | `09:00:00` | `09:30:00` | `candles_30m` |
| 23 | `M60` | `60m` | 3,600 | Yes | Yes | `09:00:00` | `10:00:00` | `candles_60m` |

Sources: `tf_index.rs:57`, `:316–434`, `:486–562`, `:627–644`, `:710–732`; live gates in `crates/app/src/dhan_feed_stack.rs:2791`, `:3368`, `:3764`. Enabled set is compiled policy, not a runtime configuration flag.

The grid starts at **09:00 IST**, while official-session-open attribution stays at 09:15. For period `P` seconds and a valid in-session fold timestamp `t`:

`A = IST-date-midnight + 32,400`; `bucket_start = A + floor((t - A)/P) × P`; interval is `[bucket_start, bucket_start + P)`.

The fold window is **[09:00,15:40) IST**, a 24,000-second window. Exact15:40 packets do not fold. Larger final buckets can be partial: 3m15:39–15:42 has only its first minute inside the fold window;15m15:30–15:45 has10 in-window minutes;30m15:30–16:00 has10;60m15:00–16:00 has40. The final force seal emits the state actually accumulated, not a fabricated full-period candle. Daily is computed internally but suppressed from this live writer.

Clock choice (`tf_index.rs:270–291`, `multi_tf_aggregator.rs:1058–1207`):

1. Positive UTC receipt nanoseconds become IST epoch seconds by integer division by 1e9, then adding 19,800.
2. If receipt minus exchange stamp lies in **[-10,+300] seconds**, use receipt seconds.
3. Missing/nonpositive receipt, or a delta outside that band, uses exchange seconds instead.
4. Date consistency, plausible timestamp/price checks, and session gates apply before state folding.

Consequences: these are mostly **receipt-clock candles**, not strict exchange-event-time candles. Subsecond order is not represented in bucket or close timestamps; packets sharing one second are resolved by processing order because close accepts `>=`. Older packets can widen high/low but do not replace a newer close. A late snapshot outside the trusted receipt band can fall into a much older bucket. Replay fidelity depends on the original receipt being retained by capture/replay; the aggregator cannot reconstruct a missing receipt.

Row `ts` is the bucket **start**, converted from the repository's IST-shifted epoch seconds to nanoseconds, not last trade, receipt, publication, database-commit or bucket-end time (`shadow_seal_columns.rs:184–190`). The timestamp representation must be respected by readers so they do not apply another +05:30 shift. DEDUP identity is `(ts, security_id, segment, feed)` (`tf_index.rs:532–534`). Do not compare row `ts` against wall clock to infer processing latency.

## Gross and net volume

Per instrument, identity is `(feed, security_id, segment)`. `security_id` alone is not unique. `cumulative_volume_override` is used when supplied; otherwise the raw `u32` `ParsedTick.volume` is widened to `u64` (`multi_tf_aggregator.rs:1232–1284`). No lot-size division occurs in this candle calculation.

For an ordinary accepted, monotonic tick:

- First observation seeds baseline `B=C`; this first tick contributes 0. It does not put the whole pre-attachment day's volume into the first candle. The missing initial interval remains unknown.
- New gross increment is `Δ=max(C-B,0)`; update accepted baseline to the larger counter.
- Each timeframe's open state tracks the same counter on its current axis. Volume is the monotonic maximum of `C - bucket_start_cumulative`, with retained carry settled at a boundary/seal/reset. This avoids double counting smaller stale counters.
- `Δ=0` adds no signed volume. For `Δ>0`, compare the current price with the previous accepted price: increase→`+Δ`; decrease→`-Δ`; equal→last classified nonzero direction. Missing previous price or no direction returns unknown (`None`).
- Net accumulates those signed increments. If any positive contribution is unclassified, the bar's net is unavailable rather than a claimed balanced 0. `net_volume()` returns NULL for unclassified/no-tick/zero-volume state; otherwise it clamps the signed accumulator to ±min(gross,`i64::MAX`). The clamp is a bound, not proof the underlying classification was exact.

Sources: `multi_tf_aggregator.rs:425–477`, `:1232–1502`; `aggregator_cell.rs:425–436`, `:515–558`, `:1922–2071`; `live_candle_state.rs:327–339`.

Example: counter 1000 seeds, next 1100 at higher price contributes +100, next 1150 at lower price contributes -50. Gross 150; tick-rule net +50. If the next 1200 is at unchanged price, the last down direction contributes -50: gross 200 / net 0. This is a mathematically balanced **tick-rule estimate**, not a vendor-certified buyer/seller split. A broker snapshot can summarize multiple trades; assigning its entire cumulative delta one direction cannot recover all those trades' actual aggressor sides.

`total_buy_qty - total_sell_qty` is **not** net executed volume: those fields are resting book quantities. Candle `volume` and `net_volume` are not top-volume percentage scores, and there is no live candle `volume_pct_from_prev_day` column.

## Candle columns: source and purpose

The shared current DDL has 18 columns (`crates/storage/src/shadow_persistence.rs:234–259`). Serialization is in `shadow_candle_writer.rs:444–510`; projection is `shadow_seal_columns.rs:184–234`.

| Column | Population | Why it exists / caveat |
|---|---|---|
| `feed` | Seal's feed enum string | Separate observations from distinct providers; part of key. |
| `segment` | Exchange segment code mapped to canonical string | Prevents same numeric id from merging across segments. |
| `security_id` | Seal instrument id → signed database integer | Instrument identity with feed/segment. Saturating conversion is a representability guard, not arbitrary-u64 identity support. |
| `ts` | Bucket start IST epoch seconds ×1e9 | Stable key for replacement/amendment; not publication latency. |
| `open` | First observed price, except bucket containing 09:15 may adopt usable official `day_open` | Opening price under the repository's explicit policy. Pre-open-spanning M30/M60/D1 combine pre-open range with official regular open. |
| `high` | Highest observed LTP; usable official range in opening bucket; later day-extreme changes only when interval attribution fits same bucket | Can recover some snapshot extrema; cannot recover every missing intrabucket price. |
| `low` | Lowest observed LTP, symmetric policies | Same attribution limits as high. |
| `close` | Latest accepted fold-clock second; processing-last when equal | Raw snapshot ordering within a second remains a limitation. |
| `volume` | Incremental gross cumulative-counter span, after carry/reset policies | Raw/corrected counter units, not lots. First observation/reset gap can be unavailable. |
| `oi` | Latest nonzero open-interest reading; older value fills an empty value but cannot replace a newer nonzero one | Zero is treated as absent. A real transition to zero is therefore indistinguishable from missing data under this policy. |
| `tick_count` | Number of folded source packets; late HLC amendment increments it | Not the exchange's executed-trade count. Does not prove all upstream ticks arrived. |
| `close_pct_from_prev_day` | `(close - previous_day_close)/previous_day_close ×100`, rounded 2dp | Headline day change; missing/invalid baseline maps to0, which also means flat. |
| `open_pct` | `(close - official_session_open)/official_session_open ×100`, rounded 2dp | Intraday change from official session open; unavailable baseline also0. |
| `change_pct` | Exact copy of `close_pct_from_prev_day` | Existing compatibility/display column, not independent information. |
| `open_gap_pct` | `(official_session_open - previous_day_close)/previous_day_close ×100`, rounded 2dp | Opening gap;0 sentinel when baseline unavailable. |
| `net_volume` | Per-tick signed estimate or omitted ILP field→SQL NULL | Distinguishes unavailable from measured estimate 0; cannot certify true aggressor flow. |
| `total_buy_qty` | Latest nonzero total resting buy quantity under close-order guard | Book demand snapshot; not trades. |
| `total_sell_qty` | Latest nonzero total resting sell quantity under close-order guard | Book supply snapshot; not trades. |

OHLC serialization rounds 2dp. Fixed-width counters saturate rather than wrapping; after numerical saturation, exactness is no longer guaranteed. No per-row gap/partial/late-carry/freshness provenance fields are present in these 18 columns.

## 09:15 pressure and failure limits

| Situation | What code currently does | What must not be claimed |
|---|---|---|
| Normal tick, existing instrument | One average hash lookup, dense cell, shared price/sign/extreme calculation,24 scalar folds; no normal-path allocation. | HashMap is averageO(1), not adversarial worst-caseO(1);24-frame fanout isO(F) asF grows. |
| First sight of instrument | Allocates/grows map and dense vector as needed; initial reserve 1000, ceiling 25,000. | First-sight allocation/reallocation is not bounded to fixed nanoseconds; more than25,000 identities can be refused. |
| Empty/quiet interval | Sparse emission; no invented empty candle; catch-up sweep closes eligible open states. | Missing row does not prove zero trades. |
| Busy queue | Root candidate adds bounded frame-quantum maintenance scheduling. Candle seal callbacks may still face backpressure and rescue I/O. | Whole drain/fold latency is not hard-bounded by that quantum. |
| Catch-up | Every nominal 5s, cutoff=max observed fold second minus 2s; scans every slot×24 in the same drain. | This isO(N×F), not backgroundO(1); nominal 7s is not an absolute latency bound. |
| Smaller cumulative counter | Treat as stale; suppress negative delta and keep last accepted direction input. | A genuine small vendor reset cannot be distinguished by this rule. |
| Huge counter drop≥2^31 | Treat as restart, settle carry, rebase retained state, discard unknown crossing delta and clear direction. | Not proof of wrap detection; very old high-volume packets or vendor reset behavior can violate the magnitude assumption. |
| Day boundary | `force_seal_all` resets per-instrument volume seeding/direction and clears retained state; app close path invokes it. | If day-boundary lifecycle is skipped, a small next-day counter can be treated as stale. No new per-slot day detector added here. |
| One retained late bucket | Optional HLC amendment; gross/sign may remain carried until a later fold/seal. | Updating HLC does not immediately place all late volume at its exact original timestamp. |
| Older late bucket | Count late; keep accepted cumulative high-water carry where possible. | Conservation does not recover exact price, bucket or aggressor history. |
| Original upstream packet never delivered | No information exists locally to reconstruct that individual tick. | Never-lost-every-exchange-tick guarantee. |
| Process/disk/network failure | Depends on capture, durable WAL, replay, spill, writer and DB acknowledgment—not the candle arithmetic alone. | In-memory queue admission is not durable capture; indefinite outages cannot fit finite buffers. |
| HTTP/WebSocket reconnect | Upstream supervisor owns recovery; candle engine resumes from received data. | Candles cannot prevent all disconnects or provide an undocumented provider replay/fast-lane guarantee. |

References: `multi_tf_aggregator.rs:72–88`, `:486–504`, `:620–740`, `:1262–1283`, `:1350–1372`, `:1551–1618`, `:1637–1652`; `aggregator_cell.rs:355–403`, `:778–836`, `:1018–1055`, `:1360–1381`, `:1407–1544`; app catch-up constants `dhan_feed_stack.rs:5090–5129`.

### Additional unresolved within-day timestamp risk

A receipt at 09:20 with an exchange timestamp 15:39 on the same date falls outside the -10-second receipt-lead band, so `fold_clock_ist_secs` returns 15:39. The day gate then sees the same date, accepts it, and raises the global watermark to 15:39. Catch-up can prematurely seal other instruments' open bars. Source path: `tf_index.rs:270–291`, `multi_tf_aggregator.rs:1089–1144`, session gate`:1206–1207`, app cutoff`:3730–3742`. This is a source-level counterexample, **not an executed Rust regression**. Resolution needs a clear live-receipt future-skew policy and telemetry while retaining the raw tick row; it is reported to root, not silently mapped onto an unrelated refusal reason.

No microsecond/nanosecond benchmark from a different machine or prior commit has been treated as evidence for this candidate. A fixed-size RAM read can beO(1) in algorithmic work; reading/sorting all symbols, emittingK rows, foldingF dynamically supplied timeframes and surviving arbitrary faults cannot all beO(1) or guaranteed constant wall-clock latency.
