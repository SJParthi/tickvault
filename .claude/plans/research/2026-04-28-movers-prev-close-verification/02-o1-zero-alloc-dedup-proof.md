# O(1) + Zero-Alloc + Dedup + Uniqueness Proof — Movers + Prev_Close Hot Path

**Branch:** `claude/debug-grafana-issues-LxiLH` (now merged to main)
**Date:** 2026-04-28
**Agent:** hot-path-reviewer

## Per-Function Verdicts

| Function | File:line | Verdict | Lookup type | Locks | Allocations | DHAT | Criterion bench | Bench budget pinned |
|---|---|---|---|---|---|---|---|---|
| Quote arm `ParsedFrame::Tick` | `tick_processor.rs:1036-1272` | NEEDS-VERIFICATION | composite `HashMap<(u32,u8),_>` via `top_movers.update_prev_close` + `try_insert_global` (papaya O(1)) | none | `format!` per parse_error (cold), `Vec` collect for hex dump (cold/rate-limited); `try_insert_global → try_record_global` is alloc-free | absent | `pipeline/batch_100_mixed` (≤100ns) | yes (`pipeline = 100`) |
| Full arm `ParsedFrame::TickWithDepth` | `tick_processor.rs:1273-1521` | NEEDS-VERIFICATION | same as Quote; Greeks enrich monomorphized; depth seq tracker is papaya | none | none on steady-state | absent | covered by `pipeline` bench | yes (`pipeline`) |
| PrevClose code-6 arm | `tick_processor.rs:1533-1628` | PASS (cold by design — ~28 IDX_I/session) | `HashMap<u32,f32>` (`index_prev_close_cache`, IDX_I-only) + `Bytes::from(serde_json::to_string(...))` enqueue | none | `serde_json::to_string` per IDX_I packet → `Bytes::from`. Cold path, documented at lines 1583-1599. | absent | absent | n/a |
| `TopMoversTracker::update` | `top_movers.rs:163-210` | PASS | `HashMap<(u32,u8), SecurityState>` O(1) composite | none | none — pure arithmetic + HashMap get/insert; capacity pre-sized 30 000 (line 134) | absent | absent | n/a |
| `top_movers.rs::update_prev_close` | `top_movers.rs:148-153` | PASS | `HashMap<(u32,u8), f32>` O(1) composite | none | none | absent | absent | n/a |
| `prev_close_persist.rs::try_record_global` | `:191-208` | PASS | `OnceLock` get + `mpsc::Sender::try_send` (O(1) wait-free) | none | zero — `PrevCloseRecord` is `Copy` (line 51) | absent (gap) | absent (gap) | not pinned |
| `prev_close_writer.rs::try_enqueue_global` | `:158-167` | PASS | `OnceLock` get + `mpsc::Sender::try_send` | none | zero — caller-owned `Bytes` (Arc handoff, no copy) | absent (gap) | YES — `prev_close_writer/try_enqueue_*` (`benches/prev_close_writer.rs:46,71`) | NOT pinned in `benchmark-budgets.toml` (gap — should be ≤200ns) |
| `first_seen_set.rs::try_insert_global` | `:158-164` | PASS | `papaya::HashMap<(u32,ExchangeSegment),()>` lock-free O(1); pre-sized 25 000 (line 40) | papaya is lock-free (epoch GC) | none — `pin().insert(...)`; key composite `(u32, ExchangeSegment)` (PASS I-P1-11) | absent (gap) | absent (gap) | not pinned |

## DEDUP UPSERT KEYS — Verbatim

| Table | Constant | Value | Composite I-P1-11? |
|---|---|---|---|
| `previous_close` | `DEDUP_KEY_PREVIOUS_CLOSE` | `"security_id, segment"` (`previous_close_persistence.rs:60`) — DDL: `KEYS(ts, security_id, segment)` (line 161) | YES — pinned by `test_previous_close_dedup_key_includes_segment_per_i_p1_11` |
| `stock_movers` | `DEDUP_KEY_MOVERS` | `"security_id, category, segment"` (`movers_persistence.rs:66`) | YES |
| `option_movers` | `DEDUP_KEY_MOVERS` (shared) | `"security_id, category, segment"` (line 66) | YES |
| `top_movers` (proposed unified) | `DEDUP_KEY_TOP_MOVERS` | `"timeframe, bucket, rank_category, rank, security_id, segment"` (`movers_persistence.rs:242`) | YES — pattern proven |

## Composite-Key (I-P1-11) Verification

| Collection | Key type | Verdict |
|---|---|---|
| `TopMoversTracker::securities` | `HashMap<(u32, u8), SecurityState>` | GOOD — composite (`top_movers.rs:114`) |
| `TopMoversTracker::prev_close_prices` | `HashMap<(u32, u8), f32>` | GOOD — composite (`top_movers.rs:117`) |
| `OptionMoversTracker::options` | `HashMap<(u32, u8), OptionState>` | GOOD — composite (`option_movers.rs:143`) |
| `FirstSeenSet::seen` | `papaya::HashMap<(u32, ExchangeSegment), ()>` | GOOD — composite + typed enum (`first_seen_set.rs:49`) |
| `prev_close_writer` global state | `OnceLock<PrevCloseWriter>` (sender only — no key map) | GOOD — drain-by-recipient |
| `prev_close_persist` global state | `OnceLock<GlobalHandle>` — `PrevCloseRecord` carries `(security_id, exchange_segment_code)` together as Copy struct | GOOD |
| `index_prev_close_cache` (`tick_processor.rs:854`) | `HashMap<u32, f32>` | ACCEPTABLE — IDX_I-only by gating on `if exchange_segment_code == 0` at write site (line 1579/1612). Should add `// APPROVED I-P1-11:` doc-comment |

## Lock Contention Check

| Site | Lock | Verdict |
|---|---|---|
| `try_insert_global` | papaya (lock-free, epoch GC) | OK |
| `try_record_global` | none — `OnceLock::get` + `mpsc::try_send` | OK |
| `try_enqueue_global` | same | OK |
| `top_movers.update` | none — `&mut self` exclusive | OK |
| `SharedTopMoversSnapshot` | `Arc<RwLock<Option<…>>>` (`top_movers.rs:22`) | COLD — read every 5s by API; written by tick processor every 5s; not in per-tick path |

## Gaps / Recommendations (Severity Ordered)

| Severity | Finding | Suggested fix |
|---|---|---|
| MEDIUM | No DHAT zero-alloc test for `try_record_global`, `try_enqueue_global`, `try_insert_global` | Add `dhat_prev_close_hot_path.rs` exercising the three; assert `total_blocks == 0` after N=10 000 |
| MEDIUM | `prev_close_writer/try_enqueue_*` Criterion bench exists but NOT pinned in `quality/benchmark-budgets.toml` — no 5% regression gate fires | Add `prev_close_writer = 200` to `[budgets]` |
| LOW | No Criterion bench for `try_record_global` or `try_insert_global` | Add `bench_first_seen_set_try_insert` + `bench_prev_close_persist_try_record` ≤100ns |
| LOW | `index_prev_close_cache` (tick_processor.rs:854) `HashMap<u32, f32>` — IDX_I-only by construction, lacks `// APPROVED:` doc | Add doc-comment justifying single-segment safety |

## Locked-Facts Cross-Checks

- `previous_close` schema self-heal (`ALTER TABLE … ADD COLUMN IF NOT EXISTS source SYMBOL`) pinned by `test_schema_self_heal_adds_source_column` (`previous_close_persistence.rs:425`)
- IST-offset on `received_at` pinned by `test_critical_received_at_includes_ist_offset` (line 456)
- `ist_midnight_nanos_for_received_at` dedup-on-same-day pinned by `test_ist_midnight_nanos_for_received_at_dedupes_same_day_writes` (line 351)

## Verdict

All five hot-path functions in scope satisfy zero-alloc + O(1) on steady-state. Composite-key I-P1-11 is honored everywhere except `index_prev_close_cache` (acceptable but missing doc-comment). DEDUP keys all include `segment` and pass meta-guard. Two MEDIUM gaps: missing DHAT for new entry points + missing bench-budget pin. Both are additions, not blockers.
