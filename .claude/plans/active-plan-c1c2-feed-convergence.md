# Implementation Plan: C1+C2 — Feed-Writer / Consumer-Loop Convergence

**Status:** IN_PROGRESS
**Date:** 2026-06-27
**Approved by:** pending (C1 implemented as a DRAFT PR — code shipped behind the
shared builder; C2 still design-only)

> **Scope:** This is a DESIGN document (docs-only). It plans, but does NOT yet
> implement, the convergence of the two remaining per-feed code duplications
> identified in the TickVault architecture audit:
>
> - **C1** — the raw-tick **writer** is duplicated: Dhan `TickPersistenceWriter`
>   (`crates/storage/src/tick_persistence.rs`) vs Groww `GrowwLiveTickWriter`
>   (`crates/storage/src/groww_persistence.rs`) both write the SAME shared `ticks`
>   table via parallel hand-written ILP append code.
> - **C2** — the tick-**consumer loop** is duplicated: Dhan
>   `run_tick_processor` (`crates/core/src/pipeline/tick_processor.rs`) vs Groww
>   `run_groww_bridge` (`crates/app/src/groww_bridge.rs`) both wire validate →
>   persist → aggregate around the ALREADY-shared engine
>   (`MultiTfAggregator::consume_tick(FeedStrategy)`).
>
> The model is the **already-shipped candle-writer convergence precedent**:
> `GenericCandle1mWriter(feed)` (`crates/storage/src/generic_candle_writer.rs`) —
> a feed-pinned, behavior-preserving facade over the per-seal `ShadowCandleWriter`,
> additive (no live call-site rewired in that PR), with the same wire bytes / table
> / DEDUP key. C1 and C2 follow the SAME doctrine.
>
> **Guarantee matrix:** this plan inherits the 15-row "100%" + 7-row Resilience
> matrices via cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory per CLAUDE.md
> §PLAN ENFORCEMENT; satisfies `per-item-guarantee-check.sh`). The honest 100%
> claim wording from `operator-charter-forever.md` §F is quoted in §Observability.

---

## Design-review corrections (folded into C1, 2026-06-27)

A 6-point adversarial design review of the C1 plan above produced these
corrections. They are now the authoritative C1 contract (the prose above the
line is the original DRAFT and is superseded where it conflicts):

1. **Headline deliverable = the shared BUILDER fn, not a facade.** C1 ships ONE
   `tick_row_builder::build_tick_row_for_feed(&mut Buffer, &RawTickFields, Feed)`
   — the single source of the `ticks` ILP wire bytes for BOTH feeds. The facade
   is incidental, NOT the precedent — only the shared row builder eliminates the
   duplicated ILP append. (The `GenericTickWriter`-facade framing in the original
   §C1 is dropped; the resilience tiers stay as-is per correction #5.)
2. **First task = define `RawTickFields` (i64 volume + f64 LTP carrier), proven
   BEFORE the builder lands.** The widen/passthrough test (E3) asserts Dhan
   `u32` volume → `i64` in `RawTickFields` and Groww `i64` volume passes through
   unchanged, and the f32→f64-clean LTP widen (E4) is proven, BEFORE asserting
   any row bytes.
3. **`received_at` is per-feed-OMITTED.** Add it to the Groww-NULL list: Dhan
   writes `received_at`; Groww does NOT. A test asserts NO `received_at=` token
   on a Groww row.
4. **Corrected column inventory (recounted against the real `tick_persistence.rs`
   source, NOT the original "19/9" claim):** the Dhan row is **2 SYMBOLs
   (`segment`, `feed`) + 16 columns** (security_id, ltp, open, high, low, close,
   volume, oi, avg_price, last_trade_qty, total_buy_qty, total_sell_qty,
   exchange_timestamp, received_at, payload_hash, capture_seq) = 18
   `new_unchecked` literals. The Groww row is **2 SYMBOLs + 5 columns**
   (security_id, ltp, volume, exchange_timestamp, capture_seq). The remaining 11
   columns are NULL for Groww.
5. **Honest envelope:** C1 removes ~25 lines of duplicated ILP-append; it does
   NOT unify the resilience tier. Dhan keeps ring→spill→DLQ; Groww keeps its
   lazy-connect buffer (durable floor = sidecar capture-at-receipt file, §32).
6. **What STAYS per-feed:** the pull/parse adapter; the `FeedStrategy` constant;
   the per-feed numeric source typing (f32→f64-clean applied for Dhan at the
   call site, skipped for Groww); the resilience tier; AND the **`capture_seq`
   SOURCE** (Dhan = WAL `frame_seq`; Groww = `next_capture_seq` seeded from
   `ts_ist_nanos`). Only the row STRING build is shared.

**C1 shipped (DRAFT PR):**
- `crates/storage/src/tick_row_builder.rs` — `RawTickFields` + `build_tick_row_for_feed`.
- `crates/storage/src/tick_persistence.rs::build_tick_row_seq` — rewired to build
  `RawTickFields` (all columns `Some`) + call the shared builder; byte-identical.
- `crates/storage/src/groww_persistence.rs::append_row` — rewired to build
  `RawTickFields` (Dhan-only columns `None` → NULL) + call the shared builder.
- Tests: `tick_row_builder::tests` (E1 NULL-not-0, E3 widen, E4 ltp-widen,
  feed-symbol, capture_seq, unchecked-name validity), the Dhan golden
  byte-preservation test in `tick_persistence.rs`, the one-builder source guard
  `crates/storage/tests/feed_tick_writer_convergence_guard.rs`, and the DHAT
  zero-alloc test `crates/storage/tests/dhat_tick_row_builder.rs`.

---

## Plan Items

- [x] **Item C1 — Sub-PR #1: unify the raw-tick WRITER row-builder (feed-parameterized)**
  - Files: `crates/storage/src/tick_row_builder.rs` (new), `crates/storage/src/tick_persistence.rs`, `crates/storage/src/groww_persistence.rs`, `crates/storage/src/lib.rs`, `crates/storage/tests/feed_tick_writer_convergence_guard.rs` (new), `crates/storage/tests/dhat_tick_row_builder.rs` (new)
  - Tests: `volume_carrier_dhan_u32_widens_and_groww_i64_passes_through`, `ltp_carrier_dhan_f32_widens_exactly`, `groww_row_omits_dhan_only_columns_null_not_zero`, `groww_row_has_no_received_at_token`, `dhan_row_contains_every_column`, `dhan_tick_row_is_byte_identical_golden`, `exactly_one_ticks_row_builder_function_exists`, `dhat_build_tick_row_for_feed_zero_alloc`, `shared_builder_unchecked_ilp_names_pass_validation`

- [ ] **Item C1 (ORIGINAL DRAFT) — Sub-PR #1: unify the raw-tick WRITER (feed-parameterized)**
  - Files: `crates/storage/src/tick_persistence.rs`, `crates/storage/src/groww_persistence.rs`, `crates/storage/src/lib.rs`, `crates/app/src/groww_bridge.rs` (call-site swap), `crates/storage/tests/feed_tick_writer_convergence_guard.rs` (new)
  - Tests: `test_generic_tick_writer_dhan_full_columns`, `test_generic_tick_writer_groww_subset_columns_null_not_zero`, `test_generic_tick_writer_dedup_key_unchanged`, `test_generic_tick_writer_dhan_uses_f32_to_f64_clean`, `test_generic_tick_writer_groww_native_f64`, `test_one_raw_tick_writer_path_guard`, DHAT `dhat_generic_tick_writer_zero_alloc`

- [ ] **Item C2 — Sub-PR #2: unify the tick CONSUMER LOOP (feed-parameterized), on top of C1**
  - Files: `crates/core/src/pipeline/tick_processor.rs`, `crates/app/src/groww_bridge.rs`, `crates/core/src/pipeline/feed_consumer.rs` (new shared loop), `crates/core/tests/feed_consumer_convergence_guard.rs` (new)
  - Tests: `test_consumer_dhan_runs_greeks_enrichment`, `test_consumer_groww_skips_greeks`, `test_consumer_persist_then_aggregate_order_preserved`, `test_consumer_null_column_policy_per_feed`, `test_one_consumer_loop_path_guard`, DHAT `dhat_feed_consumer_zero_alloc`

- [ ] **(Option, within C2) C2-LOW hardening — explicit "absent field" capability mask**
  - Files: `crates/common/src/feed.rs` (capability accessor), `crates/core/src/pipeline/feed_consumer.rs`
  - Tests: `test_feed_capability_mask_dhan_has_ohlc_oi`, `test_feed_capability_mask_groww_ltp_volume_only`

---

## Design

### The target end-state (3-line summary)

ONE feed-parameterized raw-tick **writer** + ONE feed-parameterized **consumer
loop**, each selected by a `Feed` / `FeedStrategy` parameter. The ONLY code that
stays per-feed is the **pull/parse adapter** (Dhan binary WS frame parser; Groww
NDJSON sidecar line parser). Everything downstream — persist, aggregate, the
`ticks` schema, the DEDUP key, the NULL-not-0 column policy — is common.

### What is ALREADY common (do NOT re-do)

| Already shared | Where |
|---|---|
| `Feed` enum + label / index / parse | `crates/common/src/feed.rs` |
| 21-TF candle engine | `MultiTfAggregator::consume_tick(&ParsedTick, seg, FeedStrategy, Option<u64> cum, on_seal)` (`crates/trading/src/candles/multi_tf_aggregator.rs`) |
| Per-feed late-tick / volume policy | `FeedStrategy::DHAN` / `FeedStrategy::GROWW` (`crates/trading/src/candles/aggregator_cell.rs`) |
| Shared seal-writer chain (ring→spill→DLQ for candles) | `global_seal_sender()` → `BufferedSeal{feed}` → `ShadowCandleWriter` |
| Candle writer convergence (the model) | `GenericCandle1mWriter(feed)` facade (`crates/storage/src/generic_candle_writer.rs`) |
| `ticks` table + DEDUP key | `QUESTDB_TABLE_TICKS`; `DEDUP_KEY_TICKS = "security_id, segment, capture_seq, feed"` (designated `ts`) |

### C1 — feed-parameterized raw-tick writer (target shape)

Today there are two writers into `ticks`:

| | Dhan `TickPersistenceWriter` | Groww `GrowwLiveTickWriter` |
|---|---|---|
| File | `tick_persistence.rs` (~9.7K LoC) | `groww_persistence.rs` |
| Row builder | `build_tick_row_seq` — **19 columns** | `append_row` — **9 columns** (rest NULL) |
| Numeric typing | f32→f64 via `f32_to_f64_clean` + `round_to_2dp` (Dhan WS sends f32) | native f64 `ltp`, native i64 `volume` (Groww SDK is float/int) |
| Resilience | **ring → disk spill → DLQ**, reconnect, in-flight rescue, `tv_ticks_*` counters | lazy-connect buffer ONLY (durable floor = sidecar capture-at-receipt file) |
| feed SYMBOL | `TICK_FEED_DHAN = "dhan"` | `GROWW_FEED_LABEL = "groww"` |

**Convergence approach (mirrors `GenericCandle1mWriter`, additive + behavior-
preserving):** introduce ONE `feed`-aware row-builder as the single source of the
ILP wire bytes, and have a thin facade select the column-set + typing + NULL
policy from a `Feed`:

1. Extract a single shared row-builder, e.g.
   `build_tick_row_for_feed(buffer, feed, &RawTickFields, capture_seq)`, that
   contains BOTH paths today written by hand — the Dhan 19-column path and the
   Groww 9-column subset — selected by `feed`. The Dhan branch keeps
   `f32_to_f64_clean`+`round_to_2dp`; the Groww branch keeps native f64/i64 and
   **omits** the columns Groww does not produce (open/high/low/close/oi/
   avg_price/last_trade_qty/total_buy_qty/total_sell_qty/payload_hash) so they
   stay **NULL, never 0** (QuestDB leaves an un-`column_*`-ed cell NULL — this is
   the load-bearing semantic that MUST be preserved).
2. The DEDUP key `(ts, security_id, segment, capture_seq, feed)` and the table
   name `ticks` are written by the shared builder (one place), so they cannot
   drift between feeds.
3. The Groww `GrowwLiveTickWriter` is **retired** (like `GrowwCandle1mWriter`
   was), and `run_groww_bridge` constructs the shared writer pinned to
   `Feed::Groww`. A `Feed::Dhan`-pinned instance is what the Dhan path uses.

**The KEY design decision — resilience tier stays per-feed, NOT forced onto
Groww.** The full ring→spill→DLQ machinery lives on the Dhan path because Dhan's
WS frames have no upstream durable record; Groww's durable floor is the sidecar's
capture-at-receipt NDJSON file (operator lock §32). So the converged writer is a
**facade that delegates the wire-row building** (the duplicated part) but lets the
caller choose the persistence tier:
   - **Recommended:** the converged type is `GenericTickWriter(feed)` — a thin
     facade exposing `append_tick(...)` / `flush()` that, for `Feed::Dhan`, wraps
     the existing `TickPersistenceWriter` (ring/spill/DLQ intact, ZERO behavior
     change), and for `Feed::Groww`, wraps the existing lazy-connect buffer. BOTH
     route their `column_*`/`symbol` calls through the ONE shared
     `build_tick_row_for_feed`. This makes the **duplicated ILP append code** the
     single converged surface (the audit's actual C1 gap) WITHOUT promoting
     Groww to a heavier resilience tier it does not need and WITHOUT touching the
     battle-tested Dhan ring/spill/DLQ paths.
   - This mirrors `GenericCandle1mWriter` exactly: one named feed-pinned facade,
     delegating verbatim, feed-mismatch rejected, no live call-site rewired
     except the Groww bridge swapping `GrowwLiveTickWriter::new` →
     `GenericTickWriter::new(_, Feed::Groww)`.

### C2 — feed-parameterized consumer loop (target shape)

Today there are two consumer loops:

| | Dhan `run_tick_processor<G: GreeksEnricher>` | Groww `run_groww_bridge` |
|---|---|---|
| Input adapter | mpsc of WS binary frames → `ParsedTick` (binary parser) | NDJSON file tail → `ParsedTick` (line parser) |
| Validation | persist-window + VOLUME-MONO gate + enricher | `validate_groww_tick` (finite/≤0/absurd/ts-range guards) |
| Enrichment | greeks (`GreeksEnricher`) + `TickEnricher` lifecycle | **none** (Groww has no greeks/oi) |
| Persist | `TickPersistenceWriter::append_tick_*` | `GrowwLiveTickWriter::append_row` (→ C1) |
| Aggregate | shared `MultiTfAggregator.consume_tick(FeedStrategy::DHAN)` (already) | shared `MultiTfAggregator.consume_tick(FeedStrategy::GROWW)` (already) |

**Convergence approach (on top of C1):** extract the common loop body —
`validate → persist (via the C1 writer) → fold through MultiTfAggregator →
route seals to global_seal_sender → record feed-health` — into ONE shared
`run_feed_consumer(feed, adapter, ...)` where the **adapter** is the only
per-feed piece. The adapter is a small trait/closure: "produce the next batch of
`ParsedTick` + raw fields", implemented by (a) the Dhan binary frame decoder and
(b) the Groww NDJSON line decoder. The loop then selects, by `Feed`/
`FeedStrategy`:
   - which enrichment runs (greeks ON for Dhan, OFF for Groww),
   - which writer column-set + NULL policy (delegated to the C1 writer),
   - the `FeedStrategy` (late-tick policy + volume override) passed to
     `consume_tick`.

Because the seal routing, the persist-then-aggregate ordering, the feed-health
recording, and the seal-mpsc-drop counter are byte-identical between the two
loops today, this is a pure de-duplication — the shared loop produces the SAME
observable behavior per feed.

### What STAYS per-feed (the ONLY per-feed code after convergence)

1. **Pull/parse adapter** — Dhan binary WS frame parser vs Groww NDJSON line
   parser. Different wire formats; irreducibly per-feed.
2. **The `FeedStrategy` constant** (`DHAN` vs `GROWW`) — already a per-feed value
   passed INTO the shared engine; unchanged.
3. **Numeric source typing INSIDE the shared row-builder** — the f32→f64 clean
   conversion is applied for `Feed::Dhan` (WS f32) and skipped for `Feed::Groww`
   (native f64). This is a per-feed BRANCH inside the ONE shared builder, not a
   second builder.
4. **The resilience tier** — Dhan gets ring/spill/DLQ; Groww's durable floor is
   the sidecar file. A per-feed wrapper choice, NOT a duplicated wire-row path.

### Hard invariants (MUST NOT change — stated explicitly per task brief)

- This is the **LIVE tick hot path + persistence**. The convergence MUST stay
  **zero-allocation / O(1) on the per-tick path** (no `.clone()`/`Vec::new()`/
  `format!()`/`Box`/`dyn` on the per-tick branch; the shared builder must be
  monomorphized over `Feed`, not a `dyn` dispatch). Proven by DHAT + Criterion
  bench gates.
- The convergence **MUST NOT change** the `ticks` **schema**, the **DEDUP key**
  `(ts, security_id, segment, capture_seq, feed)`, or the **NULL-not-0**
  semantics for Groww's absent columns (open/high/low/close/oi/avg_price/
  last_trade_qty/total_buy_qty/total_sell_qty/payload_hash stay NULL for Groww).
- The Dhan path's ring → spill → DLQ resilience chain MUST remain intact
  (item must not break self-heal / SubscribeRxGuard / pool watchdog — see the
  7-row resilience matrix).
- `f32_to_f64_clean` + `round_to_2dp` MUST stay on the Dhan branch
  (`data-integrity.md` price-precision rule); Groww native f64 MUST stay
  un-widened.

---

## Edge Cases

| # | Edge case | Required behavior after convergence |
|---|---|---|
| E1 | Groww NULL columns (open/high/low/close/oi/avg_price/qty fields/payload_hash) | The shared builder must NOT emit a `column_*` call for these on the `Feed::Groww` branch → cell stays **NULL**. A regression test asserts the on-wire ILP bytes for a Groww row contain NO `open=`/`oi=`/`payload_hash=` tokens, and contain no `=0` placeholder for them. |
| E2 | ms-vs-s timestamp | Both feeds already normalise: designated `ts` = full IST nanos verbatim (Groww carries ms in `ts`), Dhan `exchange_timestamp*1e9`. The shared builder takes the already-IST nanos `ts` + raw `exchange_timestamp` seconds; NO new offset is added (`data-integrity.md` WS rule "NEVER add +5:30 to ts" — preserved). |
| E3 | i64 volume (Groww) vs u32 volume (Dhan ParsedTick) | The shared row-builder must take a `volume` wide enough for i64 (Groww cum_volume exceeds u32 intraday). Dhan's `ParsedTick.volume` is u32 → widened to i64 at write (lossless). Groww's i64 passes through. NEVER funnel Groww i64 through a u32 field. |
| E4 | f32-vs-f64 LTP | Dhan branch: `round_to_2dp(f32_to_f64_clean(f32))`. Groww branch: native f64 (no `f32_to_f64_clean`, no double-rounding). The branch is selected by `feed`, proven by two tests (Dhan clean-widen, Groww native). |
| E5 | greeks only for Dhan | The shared consumer runs the `GreeksEnricher`/`TickEnricher` ONLY when `feed == Dhan` (or, cleaner, when the optional enricher is `Some`). Groww passes `None` → no greeks path executes. Test asserts a Groww consumer never calls the enricher. |
| E6 | capacity / backpressure | The seal-mpsc-drop counter `tv_seal_mpsc_dropped_total{feed}` and the Dhan ring→spill→DLQ + Groww lazy-buffer behavior are byte-preserved. The shared loop must keep the SAME drop semantics per feed; a full seal mpsc increments the counter (never panics), exactly as today. |
| E7 | capture_seq replay-stability | Both feeds stamp a monotonic, replay-stable `capture_seq` at receipt and carry it unchanged through to the row. The shared writer takes `capture_seq` as a parameter (it does NOT generate one) so the WAL-replay idempotency (TICK-SEQ-01) is preserved for both feeds. |
| E8 | feed-mismatch | Like `GenericCandle1mWriter`, a `Feed::Dhan`-pinned writer/consumer MUST reject a Groww-tagged row (and vice-versa) rather than silently mis-label the `feed` SYMBOL. Test asserts the mismatch is an `Err`, nothing buffered. |
| E9 | Groww disabled at runtime | The runtime feed-toggle gate (`feed_runtime.is_enabled(Feed::Groww)`) stays in the Groww adapter, not the shared loop body, so the live pause/resume contract (operator §32) is preserved. |

---

## Failure Modes

| Failure | Detection | Recovery | Severity envelope |
|---|---|---|---|
| QuestDB ILP down during write | Dhan branch: existing `tv_ticks_*` ring→spill→DLQ counters; Groww branch: flush `Err` logged at `error!` (audit Rule 5). | Dhan: ring→spill→DLQ absorbs (unchanged). Groww: sidecar capture-at-receipt file remains the durable floor (unchanged). | Bounded zero-loss inside the chaos envelope (per 7-row matrix). No NEW tick-drop path is introduced — this is the explicit per-item proof for "Zero ticks lost". |
| Shared builder accidentally emits `=0` for a Groww NULL column | Regression test E1 (on-wire ILP byte assertion) | Build fails — fix the branch | Would corrupt the 15:31 cross-verify + downstream candles; caught at `cargo test` (build-blocking). |
| Convergence adds a hot-path allocation | DHAT zero-alloc test on the converged per-tick path + Criterion bench (≤5% regression gate) | Build/bench-gate fails — fix to monomorphized/`&'static str` | Breaks O(1) hot path; bench-gate blocks merge. |
| `dyn`/trait-object on the per-tick path | hot-path-reviewer agent + banned-pattern scanner | Refactor to monomorphized generic over `Feed` | O(1) violation; blocked. |
| DEDUP key or schema silently changed | `dedup_segment_meta_guard.rs` + `test_*_dedup_key_unchanged` + the existing `test_dedup_key_ticks_exact_format` | Build fails | Would silently lose feed-distinct rows; build-blocking. |
| C2 merges before C1 | Serial sub-PR ordering (C1 first, C2 rebased on C1's merge) per `pr-completion-protocol.md` | N/A — sequencing | Avoids a half-converged writer under a converged loop. |
| Greeks enrichment accidentally runs for Groww | E5 test (Groww consumer never invokes enricher) | Build fails | Would compute greeks on an index/equity with no options → garbage; caught by test. |

---

## Test Plan

**Scope (per `testing-scope.md`):** C1 touches `crates/storage/` (+ `crates/app/`
call-site); C2 touches `crates/core/` + `crates/app/`. Neither touches
`crates/common/` types beyond the optional capability accessor on `Feed`, which
escalates to a `FULL_QA=1` workspace run.

**C1 (Sub-PR #1) — writer convergence:**
1. `test_generic_tick_writer_dhan_full_columns` — a Dhan tick produces all 19
   columns on the wire (open/high/low/close/oi/avg_price/qty fields/payload_hash
   present).
2. `test_generic_tick_writer_groww_subset_columns_null_not_zero` — a Groww tick
   produces the 9-column subset; the absent columns appear as **NO `column_*`
   token** on the wire (NULL), never `=0`.
3. `test_generic_tick_writer_dedup_key_unchanged` + reuse existing
   `test_dedup_key_ticks_exact_format` — `(ts, security_id, segment,
   capture_seq, feed)` is byte-identical pre/post.
4. `test_generic_tick_writer_dhan_uses_f32_to_f64_clean` — Dhan `10.20_f32` →
   `10.2` (not `10.19999980926514`).
5. `test_generic_tick_writer_groww_native_f64` — Groww `2847.55_f64` passes
   un-rounded-by-f32-path.
6. `test_one_raw_tick_writer_path_guard` (source-scan meta-guard, new file
   `feed_tick_writer_convergence_guard.rs`) — asserts there is exactly ONE
   raw-`ticks` row-builder function; `GrowwLiveTickWriter`'s hand-written
   `append_row` ILP block is gone (mirrors the `GrowwCandle1mWriter`-deleted
   guard in `groww_bridge.rs` tests).
7. DHAT `dhat_generic_tick_writer_zero_alloc` — the converged per-tick append is
   zero-alloc beyond the bounded ILP buffer growth (hot-path category).
8. Existing `chaos_index_same_value_burst_preserved.rs` re-run unchanged (proves
   the capture_seq dedup + ring/spill/DLQ survive the convergence).

**C2 (Sub-PR #2) — consumer-loop convergence:**
9. `test_consumer_dhan_runs_greeks_enrichment` / `test_consumer_groww_skips_greeks`.
10. `test_consumer_persist_then_aggregate_order_preserved` — persist precedes the
    aggregator fold for both feeds (no reordering).
11. `test_consumer_null_column_policy_per_feed` — end-to-end: a Groww tick through
    the shared loop lands a Groww row with NULL absent columns + a 21-TF seal.
12. `test_one_consumer_loop_path_guard` (source-scan) — the validate→persist→
    aggregate loop body exists in ONE place; `run_groww_bridge` delegates to it.
13. DHAT `dhat_feed_consumer_zero_alloc` + Criterion bench respecting
    `quality/benchmark-budgets.toml` (≤5% regression).
14. Property/proptest on the adapter boundary (random ParsedTick → both writers
    produce schema-valid rows).

**Adversarial 3-agent review** (operator-charter §E, `wave-4-shared-preamble.md`
§3) BEFORE and AFTER each sub-PR's implementation: hot-path-reviewer (no
alloc/`dyn` on per-tick path), security-reviewer (ILP injection / `new_unchecked`
soundness / no secret in label), general-purpose hostile (NULL-vs-0, capture_seq
replay, greeks-for-Groww, feed-mismatch, drop-counter semantics).

---

## Rollback

- **Feature flag:** the converged path ships behind a config/compile toggle
  (mirroring `stream-resilience.md` B12 + the daily-universe feature-gate
  pattern). The DEFAULT for the first deploy keeps the two existing paths; the
  flag flips to the converged writer/loop once green. Flip-to-OFF is a TESTED
  code path (`feature_flag_rollback_guard.rs`-style ratchet), not aspirational.
- **C1 rollback:** because the converged `GenericTickWriter` DELEGATES verbatim
  to the existing `TickPersistenceWriter` (Dhan) and the existing lazy buffer
  (Groww), reverting is: drop the facade + restore `GrowwLiveTickWriter::new` at
  the bridge call-site. The Dhan path is byte-unchanged, so reverting C1 cannot
  regress Dhan.
- **C2 rollback:** restore `run_groww_bridge`'s inline loop body (kept in git
  history) + revert `run_tick_processor` to call its own inline body. Serial
  ordering (C1 merged first) means C2 can be reverted independently without
  un-converging C1.
- **PR-level:** one PR at a time, auto-merge + subscribe-to-activity per
  `pr-completion-protocol.md`; if CI/post-merge health is not green, revert the
  single PR before the next item (`engineering-execution-standard-2026-06-26.md`
  §1 — post-merge validation).

---

## Observability

- **No NEW failure mode is introduced** — convergence is a de-duplication, so the
  existing telemetry is preserved per feed:
  - Dhan: `tv_ticks_*` (spilled/dropped/dlq) ring→spill→DLQ counters,
    `tv_questdb_connected` gauge, `tv_seal_mpsc_dropped_total{feed="dhan"}`.
  - Groww: `error!` on flush failure (audit Rule 5), `tv_seal_mpsc_dropped_total{feed="groww"}`, feed-health record-tick/record-candle.
- **7-layer coverage** for any genuinely-new code path (the shared builder /
  shared loop): Prom counter + gauge (reuse existing), tracing span on the loop
  entry, `error!` with `code = ErrorCode::X.code_str()` on persist failure
  (reuse existing codes — no NEW ErrorCode is needed because no new failure mode
  is added; if a convergence guard ever needs one, add a variant + a rule-file
  mention in the SAME PR per the cross-ref test), Telegram via the 5-sink error
  pipeline, `ticks` audit-of-record unchanged.
- **Source-scan meta-guards** pin the convergence so a future refactor cannot
  silently re-fork: `test_one_raw_tick_writer_path_guard` (C1) +
  `test_one_consumer_loop_path_guard` (C2), mirroring the existing
  `test_run_groww_bridge_routes_seals_through_shared_writer` guard.
- **Guarantee matrices (mandatory):** this plan carries the 15-row "100%
  everything" matrix and the 7-row Resilience Demand matrix **by cross-reference**
  to `.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical form).
  Every C1/C2 sub-PR fills the per-item gate from that matrix.

### Honest 100% claim (mandatory wording, `operator-charter-forever.md` §F)

> "100% inside the tested envelope, with ratcheted regression coverage:
> the C1/C2 convergence introduces NO new tick-drop path — the Dhan ring →
> spill → DLQ chain (`TICK_BUFFER_CAPACITY`, ratcheted by
> `crates/storage/tests/zero_tick_loss_alert_guard.rs`) is delegated verbatim,
> and the Groww durable floor (the sidecar capture-at-receipt file, §32) is
> unchanged; the converged per-tick path is DHAT zero-alloc + Criterion
> bench-gated (≤5% regression) so it stays O(1); the `ticks` schema, the
> `(ts, security_id, segment, capture_seq, feed)` DEDUP key, and the NULL-not-0
> Groww-column semantics are byte-preserved (pinned by
> `dedup_segment_meta_guard.rs` + the NULL-not-0 wire-byte test). Beyond the
> envelope, the DLQ NDJSON catches every Dhan payload as recoverable text."

---

## C2-LOW hardening option (flagged within this plan)

The audit flagged a LOW-severity hardening: today Groww's "absent field" is
expressed implicitly (the row-builder simply does not emit the `column_*` call →
NULL). A stricter, self-documenting alternative is an **explicit capability mask**
on `Feed` — e.g. `Feed::Dhan.produces_ohlc_oi() == true`,
`Feed::Groww.produces_ohlc_oi() == false` — so the shared builder asks the feed
"do you have OHLC/OI?" instead of relying on a sentinel-0 or an implicit omission.

- **Pro:** makes "Groww has no OHLC/OI" a typed, exhaustive-match invariant
  (a future feed adding OHLC can't silently get NULLs); aligns with the
  `Feed::ALL`/exhaustive-match anti-regression doctrine in `feed.rs`.
- **Con:** small added surface in `crates/common/` (escalates testing scope to
  workspace).
- **Recommendation:** implement as an OPTIONAL sub-item of **C2** (after the
  pure de-dup lands and is green), gated behind the same feature flag, with
  `test_feed_capability_mask_*` tests. It is NOT required for the core C1/C2
  convergence and can be deferred if the operator prefers the minimal diff.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan tick through converged writer | 19-column row, f32→f64-clean prices, feed=dhan, DEDUP key unchanged, ring/spill/DLQ intact |
| 2 | Groww tick through converged writer | 9-column row, native f64/i64, absent columns NULL (not 0), feed=groww, same DEDUP key |
| 3 | Dhan tick through converged consumer | greeks enrichment runs, persist-then-aggregate, 21-TF seals routed |
| 4 | Groww tick through converged consumer | NO greeks, validate→persist→aggregate, 21-TF seals routed, runtime-disable gate honored |
| 5 | Feed-mismatched row to a feed-pinned writer | rejected (`Err`), nothing buffered |
| 6 | QuestDB ILP outage burst | Dhan absorbs via ring→spill→DLQ; Groww durable floor = sidecar file; zero new loss path |
