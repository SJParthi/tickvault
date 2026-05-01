# Ratchet Test Audit & Proposal: Movers + Prev_Close + 22-Timeframe Redesign

**Date:** 2026-04-28  
**Branch:** `claude/debug-grafana-issues-LxiLH`  
**Scope:** Movers multiframe table architecture + prev_close routing + IST midnight resets  
**Owner:** Claude Code Research

---

## TASK 1: Existing Ratchet Test Inventory

### 1.1 Prev_Close Routing (Dhan Ticket #5525125)

| File | Line | Test Function Name | Pinned Behavior | Verdict |
|------|------|-------------------|-----------------|---------|
| `crates/core/tests/prev_close_routing_5525125_guard.rs` | 71 | `test_prev_close_routing_idx_i_from_code6_bytes_8_to_11` | IDX_I (indices) recv prev_close from dedicated code-6 packet at bytes 8-11 (f32 LE) | **STILL CORRECT** — redesign preserves IST-midnight reset semantics |
| `crates/core/tests/prev_close_routing_5525125_guard.rs` | 126 | `test_prev_close_routing_nse_eq_from_quote_close_field_bytes_38_to_41` | NSE_EQ recv prev_close from Quote packet code-4 at bytes 38-41 (f32 LE) | **STILL CORRECT** — segment routing unchanged |
| `crates/core/tests/prev_close_routing_5525125_guard.rs` | 175 | `test_prev_close_routing_nse_fno_from_full_close_field_bytes_50_to_53` | NSE_FNO recv prev_close from Full packet code-8 at bytes 50-53 (f32 LE) | **STILL CORRECT** — segment routing unchanged |
| `crates/core/tests/prev_close_routing_5525125_guard.rs` | 223 | `test_prev_close_routing_offsets_are_distinct_per_ticket_5525125` | PREV_CLOSE_OFFSET_PRICE=8, QUOTE_OFFSET_CLOSE=38, FULL_OFFSET_CLOSE=50 stay distinct (no accidental aliasing) | **STILL CORRECT** — constant defense-in-depth continues |

**Summary:** All 4 prev_close routing tests remain valid and **WILL NOT NEED UPDATE**. They test parser-layer extraction (no downstream movers knowledge required).

---

### 1.2 Movers Persistence & DEDUP (Wave 3-A Item 10)

| File | Line | Test Function Name | Pinned Behavior | Verdict |
|------|------|-------------------|-----------------|---------|
| `crates/app/tests/preopen_movers_persistence_e2e.rs` | 185 | `test_preopen_movers_e2e_snapshot_persists_phase_preopen_to_questdb` | PreopenMoversTracker snapshot → StockMoversWriter → ILP → FakeQuestDb with zero drops | **WILL NEED UPDATE** — new movers_Ts tables replace snapshot architecture |
| `crates/app/tests/preopen_movers_persistence_e2e.rs` | 288 | `test_preopen_movers_e2e_questdb_unreachable_falls_into_rescue_ring` | When QuestDB offline, rows buffer in rescue ring (zero drops, zero loss) | **WILL NEED UPDATE** — rescope from single-snapshot to per-timeframe writers |

**Summary:** 2 existing movers tests. Both test rescue-ring + DEDUP idempotency (CORRECT pattern). Will need rescoped to new multi-table architecture.

---

### 1.3 First-Seen-Set IST Midnight Reset

| File | Line | Test Function Name | Pinned Behavior | Verdict |
|------|------|-------------------|-----------------|---------|
| `crates/core/src/pipeline/first_seen_set.rs` (unit tests not found in separate file) | — | `test_init_global_returns_arc` (mentioned as EXEMPT) | `init_global()` initializes `Arc<FirstSeenSet>` exactly once | **STILL CORRECT** — reset semantics unchanged by movers redesign |
| `crates/core/src/pipeline/first_seen_set.rs` | 124 | `secs_until_next_ist_midnight(now_unix_secs)` pure fn | Calculates seconds until next IST midnight from Unix UTC | **STILL CORRECT** — pure function, no side effects, redesign independent |

**Summary:** FirstSeenSet is correctly used by tick_processor for prev_close dedup. Redesign does NOT change IST-midnight reset—it remains the source-of-truth clock. **NO RATCHET TEST UPDATES REQUIRED** (reset task is implicitly tested by prev_close persistence behavior).

---

### 1.4 Materialized Views Configuration (Current 18 Candle Views)

| File | Line | Test Function Name | Pinned Behavior | Verdict |
|------|------|-------------------|-----------------|---------|
| `crates/storage/src/materialized_views.rs` | 271 | `view_count()` (test-only helper) | 18 materialized view definitions in correct dependency order | **WILL NEED UPDATE** — +9 new candle views added for 1m sequential gaps |
| `crates/storage/src/materialized_views.rs` | 277 | `view_names()` (test-only helper) | Returns all view names in dependency order | **WILL NEED UPDATE** — dependency chain expands with new intermediate views |
| `crates/storage/src/materialized_views.rs` | 285 | `validate_dependency_order(base_table)` | Every view's source is either base table or previously-defined view | **WILL NEED UPDATE** — new views inserted into 1m→5m/10m/15m/30m/1h chain |
| `crates/common/tests/schema_validation.rs` | 89 | `schema_max_total_subscriptions_consistent` | `MAX_TOTAL_SUBSCRIPTIONS = 5000 * 5 = 25,000` | **STILL CORRECT** — subscription capacity unchanged by movers redesign |
| `crates/common/tests/schema_validation.rs` | 223 | `schema_indicator_ring_buffer_power_of_two` | `INDICATOR_RING_BUFFER_CAPACITY` is power of 2 for bitmask | **STILL CORRECT** — unrelated to movers |

**Summary:** 5 materialized-view tests. Three need rescoping (dependency order, count, names). Two subscription tests unaffected.

---

### 1.5 Capacity Assertions (Subscription Limits)

| File | Line | Test Function Name | Pinned Behavior | Verdict |
|------|------|-------------------|-----------------|---------|
| `crates/common/tests/schema_validation.rs` | 89 | `schema_max_total_subscriptions_consistent` | `MAX_TOTAL_SUBSCRIPTIONS = 25,000` (5 conns × 5K per conn) | **STILL CORRECT** — movers redesign does not affect subscription capacity |
| `crates/core/src/instrument/subscription_planner.rs` | — | (multiple tests pinning seen_ids: HashSet<(u32, ExchangeSegment)>) | Subscription planner keeps `(security_id, segment)` pairs, not duplicated | **STILL CORRECT** — I-P1-11 compliance unchanged |

**Summary:** Subscription limits remain frozen. **NO UPDATES REQUIRED**.

---

### 1.6 Movers Dedup Keys (I-P1-11 Compliance)

**Current DEDUP key in `stock_movers` table:**
```
const DEDUP_KEY_MOVERS: &str = "security_id, category, segment";
```
Prevents cross-segment collisions for same `security_id` per I-P1-11.

| File | Line | Test Function Name | Pinned Behavior | Verdict |
|------|------|-------------------|-----------------|---------|
| `crates/storage/src/movers_persistence.rs` | 66 | (constant definition, no test found) | DEDUP key includes `segment` to prevent I-P1-11 collisions | **WILL NEED RATCHET** — new movers_Ts tables MUST include segment in DEDUP key |

**Summary:** No existing ratchet for movers DEDUP. **Must create one for new tables.**

---

## TASK 2: Proposed NEW Ratchet Tests for 22-Timeframe Redesign

### Architecture Overview
- **22 movers tables:** `movers_1s`, `movers_5s`, `movers_10s`, ..., `movers_1h` (same ladder as candles)
- **Timeframe coverage:** Full universe (indices + F&O + stocks) per timeframe per snapshot per second
- **Persistence:** Background tokio task per timeframe → snapshot every T seconds → ILP UPSERT
- **DEDUP key:** `(ts, security_id, segment, timeframe)` prevents:
  - Same instrument same timeframe different second: overwritten (correct)
  - Same instrument different timeframes: distinct rows (correct)
  - Cross-segment collisions: prevented by segment in key (I-P1-11)

### 2.1 Ratchet Tests for New Movers Architecture

#### Test 1: Table DDL Completeness
```
Test Name: test_movers_all_22_timeframes_ddl_creates_without_error
Location: crates/storage/tests/materialized_views_movers_guard.rs
```
**Asserts:**
- 22 CREATE TABLE IF NOT EXISTS DDL statements all execute successfully
- Each table has columns: `timeframe SYMBOL, security_id LONG, segment SYMBOL, category SYMBOL, rank INT, ltp DOUBLE, prev_close DOUBLE, change_pct DOUBLE, volume LONG, ts TIMESTAMP`
- Designated timestamp is `ts TIMESTAMP`
- All tables partitioned by DAY WAL
- All use `DEDUP ENABLE UPSERT KEYS(ts, security_id, segment, timeframe)`

**Why critical:** Missing DDL or schema drift → silent data loss in new timeframes or regressions in existing views.

---

#### Test 2: DEDUP Key Includes Segment (I-P1-11)
```
Test Name: test_movers_dedup_key_includes_segment_for_cross_segment_collisions
Location: crates/storage/tests/movers_segment_dedup_guard.rs
```
**Asserts:**
- For all 22 movers tables, DEDUP key string contains `"segment"`
- For all 22 movers tables, DEDUP key string contains `"security_id"`
- For all 22 movers tables, DEDUP key string contains `"timeframe"`
- Source code comment at each table definition includes `// I-P1-11:` reference

**Why critical:** Missing segment → cross-segment collision (NIFTY id=13 IDX_I overwrites NIFTY id=13 NSE_EQ silently).

---

#### Test 3: Timeframe Ladder Completeness
```
Test Name: test_movers_all_required_timeframes_present
Location: crates/storage/tests/movers_timeframe_guard.rs
```
**Asserts:**
- All 22 timeframes exist in code constant: `["1s", "5s", "10s", "15s", "30s", "1m", "2m", "3m", "5m", "10m", "15m", "30m", "1h", "2h", "3h", "4h", "1d", "7d", "1M", "3M", "1y", "custom"]`
- Table count constant = 22
- Each timeframe maps to one table name (e.g., `1s` → `movers_1s`)
- Dependency order reflects tokio task spawn order (no synchronous waits for earlier timeframes to complete before later ones start)

**Why critical:** Missing timeframe → dashboard incomplete universe; off-by-one → silent metric skew.

---

#### Test 4: MoversWriter Per-Timeframe Isolation
```
Test Name: test_movers_writer_per_timeframe_maintains_independent_rescue_rings
Location: crates/storage/tests/movers_writer_isolation_guard.rs
```
**Asserts:**
- Each `MoversWriter<T>` instance (generic per timeframe) has its own bounded rescue ring (5K entries)
- Writing rows to `movers_1s` writer does NOT affect `movers_5s` rescue ring
- Flushing `movers_1s` does NOT flush `movers_5s` (async independent)
- When one timeframe's QuestDB write fails, other timeframes continue without backoff interference

**Why critical:** Timeframe coupling → one slow timeframe blocks entire snapshot; undetected rescue-ring crossover → data loss in unrelated timeframe.

---

#### Test 5: Full-Universe Coverage Assertion
```
Test Name: test_movers_snapshot_includes_all_24k_instruments_all_timeframes
Location: crates/storage/tests/movers_universe_guard.rs
```
**Asserts:**
- `MoversTracker::compute_snapshot()` returns exactly 24,324 rows (indices + F&O + stocks)
- Each row appears in ALL 22 timeframes with correct `timeframe` value
- Cross-segment duplicates (e.g., id=13 IDX_I + id=13 NSE_EQ) appear as distinct rows with different segment values
- No instrument missing from any timeframe

**Why critical:** Silent universe truncation → incomplete movers rankings; missing segment → inconsistent P&L.

---

#### Test 6: UPSERT Idempotency (Rescue Ring Recovery)
```
Test Name: test_movers_upsert_same_snapshot_twice_produces_idempotent_result
Location: crates/storage/tests/movers_idempotency_guard.rs
```
**Asserts:**
- Write same 100-row snapshot to `movers_5s` at ts=T
- Write identical snapshot again at ts=T
- Row count in QuestDB unchanged (UPSERT collapsed duplicates)
- Rescue ring length = 0 (all rows persisted)
- Third write at ts=T+1s succeeds, distinct row appears

**Why critical:** Restart during snapshot flush → phantom duplicate rows; UPSERT broken → rescue ring accumulates same rows forever.

---

#### Test 7: Snapshot Interval Cadence
```
Test Name: test_movers_background_task_emits_snapshot_every_t_seconds
Location: crates/storage/tests/movers_task_cadence_guard.rs
```
**Asserts:**
- Background task spawned for each timeframe takes a snapshot every T seconds (T varies: 1s for 1s/5s, 5s for 10s+, etc.)
- Snapshots carry monotonically-increasing `ts` (Unix epoch nanos)
- No two snapshots have identical `ts` (timestamp ordering respected)
- Task respects `CancellationToken` and exits cleanly on shutdown

**Why critical:** Snapshot missed → gaps in movers time series; non-monotonic ts → candle aggregation breaks; hung task → graceful shutdown fails.

---

#### Test 8: Prev_Close Routing into Movers
```
Test Name: test_movers_prev_close_value_matches_parsed_day_close
Location: crates/storage/tests/movers_prev_close_routing_guard.rs
```
**Asserts:**
- `ParsedTick::day_close` from prev_close parser flows into MoversTracker
- Movers snapshot row `prev_close` column = tick's `day_close` (preserving f32→f64 via `f32_to_f64_clean()`)
- No prev_close → row emitted with `prev_close = 0.0` (explicit, not NaN)
- Cross-verify that prev_close in movers matches `previous_close` persistence table (row identity match on same ts)

**Why critical:** Prev_close corruption → movers `change_pct` wrong; missing value → dashboard blank instead of 0; float precision loss → off-by-penny arbitrage risk.

---

### 2.2 Ratchet Tests for New Candle Views (9 New Sequential 1m Gaps)

#### Test 9: Candle View Dependency Order (Extended)
```
Test Name: test_candle_views_27_total_in_correct_dependency_order
Location: crates/storage/tests/materialized_views_guard.rs (extend existing)
```
**Asserts:**
- 27 total views (existing 18 + 9 new): `4m, 6m, 7m, 8m, 9m, 11m, 12m, 13m, 14m`
- All 27 views created in dependency order (source view must exist before dependent view)
- New 4m/6m/7m/8m/9m source from candles_1m
- New 11m/12m/13m/14m source from candles_10m (or candles_5m, depending on divisibility)
- New 9m view (9 = 3 × 3) correctly sources from candles_3m (not candles_1m directly)

**Why critical:** Wrong dependency → view creation fails silently (IF NOT EXISTS suppresses error); missing view → dashboard query errors; stale source → candles_4m gets 1s granularity instead of 1m (data explosion).

---

#### Test 10: Bucket Alignment Consistency for All Views
```
Test Name: test_all_27_candle_views_use_correct_offset_alignment
Location: crates/storage/tests/materialized_views_guard.rs (extend existing)
```
**Asserts:**
- All sub-minute views (1s through 1m) use `OFFSET '00:00'` (IST midnight alignment)
- Hourly view `candles_1h` uses `OFFSET '00:15'` (NSE market open 09:15 IST)
- All multi-hour views (2h through 1M) use `OFFSET '00:00'`
- Comment at each view definition explains alignment choice

**Why critical:** Wrong offset → cross-match join misses on 1h (live buckets at :00, historical at :15); midnight vs 00:15 → 24h off-by-one candles at daybreak.

---

### 2.3 Composite Integration Tests

#### Test 11: Prev_Close + Movers + 22 Timeframes End-to-End
```
Test Name: test_movers_prev_close_integration_all_22_timeframes_with_rescue_ring
Location: crates/app/tests/movers_prev_close_e2e_integration.rs (new file)
```
**Asserts:**
- Feed 5 ticks from same security in pre-open window (different timestamps)
- Each tick carries `day_close` from parser
- Tracker produces snapshots for all 22 timeframes
- Each timeframe's writer persists with correct `prev_close`, `timeframe`, `segment`
- Rescue ring empty post-flush (zero loss)
- Replay: restart app, previous day's snapshots still in QuestDB with correct DEDUP behavior

**Why critical:** Integration breaks → end-to-end data loss; DEDUP broken → duplicate rows accumulate; prev_close lost → movers calcs wrong on restart.

---

#### Test 12: IST Midnight Reset Fires at Correct Time
```
Test Name: test_movers_first_seen_set_resets_at_ist_midnight_affects_prev_close
Location: crates/core/tests/first_seen_set_midnight_reset_guard.rs (extend if exists)
```
**Asserts:**
- `secs_until_next_ist_midnight(now_unix_secs)` returns (0, 86400] seconds
- When system time crosses IST midnight, `FirstSeenSet::reset()` fires (monitored via log inspection)
- After reset, `try_insert(security_id, segment)` returns `true` again (re-entry allowed)
- Prev_close persistence re-fires on first post-midnight tick (one row per day, not cumulative)

**Why critical:** IST midnight calculation wrong → prev_close emitted twice (one at midnight, one at end of day); reset never fires → cumulative prev_close rows (state leak).

---

## Summary Table: Update Status for Task 1 Tests

| Test Name | Current Status | Update Required? | Reason |
|-----------|----------------|------------------|--------|
| `prev_close_routing_*` (4 tests) | Passing | **NO** | Parser layer unchanged; segment routing immutable |
| `preopen_movers_persistence_e2e` (2 tests) | Passing | **YES** | Rescope to new 22-table architecture; verify each table independently |
| `FirstSeenSet` reset semantics | Implicit (no separate test file) | **NO** | IST midnight logic frozen; redesign uses same reset mechanism |
| `materialized_views` helpers (3 tests) | Passing | **YES** | Extend to 27 views; update dependency chain validation |
| `schema_*` capacity tests (2 tests) | Passing | **NO** | Subscription limits immutable; unrelated to movers |
| `subscription_planner` (implicit) | Passing | **NO** | `(security_id, segment)` pairing continues; I-P1-11 unchanged |

---

## Summary Table: Proposed NEW Ratchet Tests for Task 2

| Test Name | File | Scope | Criticality |
|-----------|------|-------|-------------|
| DDL completeness (22 tables) | `movers_materialized_views_guard.rs` | Schema | **CRITICAL** |
| DEDUP key segment inclusion | `movers_segment_dedup_guard.rs` | Dedup I-P1-11 | **CRITICAL** |
| Timeframe ladder completeness | `movers_timeframe_guard.rs` | Universe | **HIGH** |
| Per-timeframe writer isolation | `movers_writer_isolation_guard.rs` | Independence | **HIGH** |
| Full-universe coverage (24K) | `movers_universe_guard.rs` | Completeness | **CRITICAL** |
| UPSERT idempotency + rescue ring | `movers_idempotency_guard.rs` | Data integrity | **CRITICAL** |
| Background task cadence | `movers_task_cadence_guard.rs` | Timing | **HIGH** |
| Prev_close flow into movers | `movers_prev_close_routing_guard.rs` | Data accuracy | **CRITICAL** |
| 27 candle views dependency order | `materialized_views_guard.rs` (extend) | Schema | **CRITICAL** |
| Bucket alignment for 27 views | `materialized_views_guard.rs` (extend) | Cross-match | **CRITICAL** |
| E2E: prev_close + movers + 22 timeframes | `movers_prev_close_e2e_integration.rs` | Integration | **CRITICAL** |
| IST midnight reset semantics | `first_seen_set_midnight_reset_guard.rs` (extend) | Daily reset | **HIGH** |

**Total proposed:** 12 new ratchet tests

---

## Recommended Implementation Order

1. **Phase 1 (Schema Layer):** Tests 1, 9 — DDL + dependency order (unblock table creation)
2. **Phase 2 (Data Integrity):** Tests 2, 5, 6 — DEDUP + universe coverage + idempotency (unblock ILP writes)
3. **Phase 3 (Prev_Close Routing):** Test 8 — verify flow into snapshots (unblock movers accuracy)
4. **Phase 4 (Timing & Isolation):** Tests 3, 4, 7 — timeframe independence + cadence (unblock production stability)
5. **Phase 5 (Integration):** Tests 10, 11, 12 — E2E + midnight reset + view alignment (production readiness)

---

## References
- `.claude/rules/project/gap-enforcement.md` — I-P1-11 (segment-aware uniqueness)
- `.claude/rules/project/data-integrity.md` — DEDUP/idempotency rules
- `.claude/rules/project/market-hours.md` — IST midnight reset cadence
- `crates/core/tests/prev_close_routing_5525125_guard.rs` — existing ratchet template
- `crates/app/tests/preopen_movers_persistence_e2e.rs` — existing movers test pattern
- `crates/storage/src/materialized_views.rs` — existing view DDL structure
- `crates/core/src/pipeline/first_seen_set.rs` — IST midnight reset implementation
