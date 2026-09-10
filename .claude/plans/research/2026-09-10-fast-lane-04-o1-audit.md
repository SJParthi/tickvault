# Fast-lane research 04 — per-tick O(1) audit (2026-09-10)

Produced by the hot-path reviewer agent (read-only pass; findings transcribed by the
coordinating session because that agent had no write tool). Every row is Verified in
source unless labelled otherwise. "Gate" = the DHAT or measurement harness that fails
the build or records the number.

## 1. Per-tick stage verdicts

| Stage | Verified complexity | Alloc | Gate | Verdict |
|---|---|---|---|---|
| WS read → WAL `append_with_seq_at` (`pool_supervisor.rs:3064`, `ws_frame_spill.rs:1078`) | O(1): `Bytes::clone` is a refcount (`:3066`), 2 relaxed atomics, `try_send`. Writer `write_all` into a 256 KiB `BufWriter`; **no per-record fsync** (time-driven only, `:1399`) | none | `dhat_ws_reader_zero_alloc` | ✅ |
| Ring reserve+push (`pool_supervisor.rs:3087`) | 2 atomics; slots AND bytes capped; fail-closed | none | — | ✅ |
| `drain_main_feed_frame` (`dhan_feed_stack.rs:6150`) | O(packets per frame), cap `MAX_PACKETS_PER_FRAME = 70_000` | none | **none** | ⚠️ unmeasured end-to-end |
| decode (`parser/*`) | fixed-offset `from_le_bytes`; depth loops ≤ 200 levels (`depth.rs:454`) | none | `dhat_allocation`, `dhat_depth_packet_zero_alloc` | ✅ |
| `append_inline_depth` (`dhan_feed_stack.rs:6925`) | **10 ILP rows per Full packet**, on the drain, shed-gated | none (`Cow`) | `dhat_depth_append_zero_alloc` | ⚠️ constant, not complexity |
| `record_ws_lag` (`:7438`) | pre-resolved `[Histogram; 16]`; `to_string()` ≤ 16× at first tick only | none steady-state | `dhat_ws_lag` | ✅ |
| `SpotPriceStore::record` (`spot_price_store.rs:357`) | papaya pin + 1 probe + CAS (single writer); spot segments only; counters are plain atomics | none | ingest seam | ✅ |
| `PrevCloseStore::record` (`dhan_feed_stack.rs:1784`) | code-6 only, 1 probe | none | — | ✅ |
| `TickGapDetector::observe` (`tick_gap_detector.rs:669`) | hash → dense slot, in place | none | ingest seam | ✅ |
| `consume_tick` (`multi_tf_aggregator.rs:632`) | 1 plain-`HashMap` probe + 24 scalar folds (`:1094`) | none | `dhat_multi_tf_fold` | ✅ |
| `slot_index` growth (`multi_tf_aggregator.rs:503`) | first growth `reserve_exact`s the **full 25,000 ceiling on the drain** (~4.7 MB, once per process), then fail-closed | one-shot | — | ⚠️ one mid-session pause by design |
| `owner_of` (`contract_underlying_map.rs:567`) | arc-swap load + 1 probe | none | ingest seam | ✅ |
| `VolumeLeaderboard::observe` (`volume_leaderboard.rs:495`) | 1 probe + compare; capped, refuse-not-evict; logs throttled ^2 | none | ingest seam | ✅ |
| `append_tick_with_seq` → `append_row` (`tick_persistence.rs:2202`) | questdb-rs `Buffer`; `sanitize_ilp_symbol` → `Cow::Borrowed` (`sanitize.rs:151`); growth amortised | amortised | `dhat_live_ingest_seam` (≤ 256 blocks / 10,000 folds) | ✅ |
| `classify_raw` (`depth_subscription_view.rs:269`) | 2 arc-swap loads + ≤ 3 probes | none | — | ✅ |

**No `.clone()` / `format!` / `Vec::new()` / `collect()` / `Box` / `dyn` on the per-tick
path** — grepped `dhan_feed_stack.rs`; every hit is ≥ line 8000 (boot / steering /
tests) except the two documented one-shots above.

## 2. Same task, not per tick

| Sweep | Cadence | Measured | Harness | Status |
|---|---|---|---|---|
| `VolumeLeaderboard::rank` | 1 s + 5 s, both families | 900 µs | `rank_sweep_cost_at_the_authorized_ceiling` | Verified |
| `catch_up_seal_all` | 5 s | 9.67 ms (0.2 % duty) | `catch_up_seal_all_sweep_cost_at_the_authorized_ceiling` | Verified |
| `scan_silence` | 30 s | 69.8 µs | `scan_silence_sweep_cost_at_the_authorized_ceiling` | Verified |
| `gainer_eligible` | 5 s | harness present | `gainer_eligible_sweep_cost_at_the_authorized_ceiling` | Verified |
| `snapshot_top_volume` (≤ 500 ILP appends) | 1 s + 5 s | CLAUDE.md says "inside the measured 900 µs" — the sort harness does NOT cover the append | — | **Assumed** |
| `drain_main_feed_frame` end to end at 12,500 pkt/s | continuous | no Criterion, no DHAT | — | **Unknown** |

## 3. Stale against the code in CLAUDE.md's table

1. **`drain_main_feed_frame` row** says per packet is "one lookup, one 24-TF fold, one
   ILP append". Per packet it is now ALSO `record_ws_lag`, `record_spot_price`,
   `record_previous_close`, `detector.observe`, `observe_for_ranking`, and
   `append_inline_depth` = **10 ILP rows per Full packet**. Still O(1); the constant is
   understated ~11×.
2. **The headline "no loop" correction** covers `parser/depth.rs` only —
   `append_inline_depth` also loops 5 levels per Full packet.
3. **Aggregator slot-growth row** ("pre-sized at boot, fail-closed") is true but
   incomplete: boot pre-sizes ~865 spots against a ~23,000 peak, so one
   `reserve_exact` to the ceiling runs **on the drain mid-session by design**
   (`multi_tf_aggregator.rs:556-590`). Not in the table.
4. **No rows** for `contract_underlying_map::owner_of` or `TickGapDetector::observe` —
   both per tick.
5. **DHAT boundary understated:** `dhat_live_ingest_seam` measures `ingest_tick_at`,
   explicitly NOT the drain, so `record_spot_price` and `record_previous_close` sit under
   no DHAT gate at all.

## 4. Verdict

The per-tick path is O(1) per packet and zero-allocation in steady state, gated by five
DHAT harnesses. What is NOT held to that standard: the end-to-end drain cost at the open
burst (never measured), the one-shot 4.7 MB slot reservation mid-session, and two
per-tick stores outside every DHAT gate. None of the three is a loss path; all three are
un-numbered claims, which is what this repository's own table says it will not carry.
