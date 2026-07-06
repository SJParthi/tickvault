//! Shared feed-parameterized per-tick **consumer sequence** — C2 of the C1/C2
//! feed-convergence plan (`.claude/plans/active-plan-c1c2-feed-convergence.md`).
//!
//! ## Why this exists (the convergence)
//!
//! Before C2 the Dhan consumer loop (`tick_processor::run_tick_processor`) and
//! the Groww consumer loop (`crates/app/src/groww_bridge.rs::drain_new_data`)
//! each hand-wrote the SAME ordered per-tick consumer sequence around the
//! already-shared engine pieces (the C1 row builder underneath the writers, the
//! `MultiTfAggregator` + `route_seal` chain downstream):
//!
//! 1. **enrich** — greeks (`GreeksEnricher`) + lifecycle (`TickEnricher`),
//!    a Dhan-ONLY stage (Groww has no greeks/OI — plan edge E5);
//! 2. **persist** — the feed's writer (Dhan `TickPersistenceWriter` with
//!    ring→spill→DLQ; Groww `GrowwLiveTickWriter` with its lazy-connect
//!    buffer), both building the row via the ONE C1 `build_tick_row_for_feed`;
//! 3. **aggregate handoff** — hand the tick to the 21-TF candle stage (Dhan:
//!    the tick broadcast that Engine B subscribes to off the hot path; Groww:
//!    the inline `MultiTfAggregator::consume_tick(FeedStrategy::GROWW)` fold).
//!
//! [`consume_feed_tick`] is now the ONE place that owns that ORDER and the
//! greeks-for-Dhan-only gate, so the two loops can never silently diverge on
//! either. Everything else in the two loops is genuinely per-feed (Dhan binary
//! frame dispatch / dedup ring / persist-window gates / conservation ledger;
//! Groww NDJSON tailing / offset snapshots / ILP backpressure / archive drains)
//! and deliberately stays per-feed — forcing it shared would be dishonest.
//!
//! ## Honest envelope (operator-charter §F)
//!
//! C2 converges the ORDERED 3-stage per-tick consumer core; it does NOT unify
//! the pull/parse adapters, the validation gates, the resilience tiers, or the
//! post-fold `ConsumeStats` handling (whose per-feed counter labels are
//! operator-pinned — see `wave-6-error-codes.md` AGGREGATOR-LATE-01). The Dhan
//! `TickWithDepth` (Full-packet) arm keeps its own inline persist: its
//! aggregate handoff (the broadcast) is separated from persist by depth-frame
//! gating whose loop-`continue`s cannot be expressed through this core without
//! CHANGING which frames are broadcast — a behavior change C2 must not make.
//! That exception is pinned by `feed_consumer_convergence_guard.rs`.
//!
//! # Performance
//!
//! Monomorphized over the tick type + the three stage closures — NO `dyn`, NO
//! `.clone()`, NO heap allocation, `Feed` is a `Copy` enum matched once. The
//! whole call inlines to exactly the pre-C2 straight-line code (proven by
//! `dhat_feed_consumer_zero_alloc` + the existing tick-pipeline benches).

use tickvault_common::feed::Feed;

/// Run ONE tick through the shared, ordered per-tick consumer sequence:
/// **enrich (Dhan-only) → persist → aggregate handoff**.
///
/// - `feed` selects the enrichment gate: the `enrich` stage (greeks +
///   lifecycle) runs ONLY for [`Feed::Dhan`] (plan edge E5 — Groww has no
///   greeks/OI). The match is exhaustive, so adding a future feed variant
///   forces an explicit decision here at compile time.
/// - `persist` runs BEFORE `aggregate` for every feed (plan test 10 — the
///   persist-then-aggregate order is load-bearing: a tick must reach the
///   durable `ticks` row path before the derived-candle fold).
/// - A persist failure does NOT gate the aggregate handoff: BOTH feeds handle
///   persist errors inside their `persist` closure (Dhan rescues to
///   ring→spill/WAL and still broadcasts; Groww logs and still folds), exactly
///   the pre-C2 behavior. That is why `persist` returns `()` — error policy is
///   a per-feed concern that must not be homogenized here.
///
/// `T` is the feed's tick representation at this stage (Dhan: `ParsedTick`;
/// Groww: its parsed NDJSON tick). Generic + monomorphized: zero-cost, no
/// trait object on the per-tick path. Feed-pinning/mismatch rejection (plan
/// edge E8) is owned by the C1 writers underneath `persist`, not repeated here.
///
/// # Performance
/// O(1): one `Copy`-enum match + three inlined closure calls. Zero-alloc
/// (ratcheted by `dhat_feed_consumer_zero_alloc`).
#[inline(always)]
pub fn consume_feed_tick<T>(
    feed: Feed,
    tick: &mut T,
    enrich: impl FnOnce(&mut T),
    persist: impl FnOnce(&T),
    aggregate: impl FnOnce(&T),
) {
    // E5: enrichment (greeks + lifecycle) is a Dhan-only stage. Exhaustive
    // match — a new Feed variant fails the build here until a decision is made.
    match feed {
        Feed::Dhan => enrich(tick),
        // Groww has no greeks/OI (operator lock §33 — live-feed-only, LTP +
        // cumulative volume). The enrich closure is NEVER invoked for Groww,
        // even if a caller passes a real one (defense in depth beyond
        // "Groww passes a no-op").
        Feed::Groww => {}
    }
    // Persist-then-aggregate: the load-bearing order (plan test 10/11).
    persist(tick);
    aggregate(tick);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Minimal tick stand-in — the core is generic over `T`, so a tiny Copy
    /// struct exercises the exact same monomorphization contract.
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct TestTick {
        ltp: f64,
        enriched: bool,
    }

    #[test]
    fn test_consumer_dhan_runs_greeks_enrichment() {
        // Plan test 9 (Dhan half): the enrich stage (the greeks/lifecycle
        // slot) IS invoked for Feed::Dhan, and its mutation is visible to the
        // persist + aggregate stages (enrich runs FIRST).
        let mut tick = TestTick {
            ltp: 21_000.0,
            enriched: false,
        };
        let persist_saw_enriched = Cell::new(false);
        let aggregate_saw_enriched = Cell::new(false);
        consume_feed_tick(
            Feed::Dhan,
            &mut tick,
            |t| t.enriched = true,
            |t| persist_saw_enriched.set(t.enriched),
            |t| aggregate_saw_enriched.set(t.enriched),
        );
        assert!(tick.enriched, "Dhan enrich stage must run");
        assert!(
            persist_saw_enriched.get(),
            "persist must observe the ALREADY-enriched tick (enrich precedes persist)"
        );
        assert!(
            aggregate_saw_enriched.get(),
            "aggregate must observe the ALREADY-enriched tick"
        );
    }

    #[test]
    fn test_consumer_groww_skips_greeks() {
        // Plan test 9 (Groww half) / edge E5: even when a REAL enrich closure
        // is supplied, the core never invokes it for Feed::Groww — greeks on a
        // greeks-less feed would compute garbage.
        let mut tick = TestTick {
            ltp: 2_847.55,
            enriched: false,
        };
        let enrich_invoked = Cell::new(false);
        let persist_invoked = Cell::new(false);
        let aggregate_invoked = Cell::new(false);
        consume_feed_tick(
            Feed::Groww,
            &mut tick,
            |t| {
                enrich_invoked.set(true);
                t.enriched = true;
            },
            |_| persist_invoked.set(true),
            |_| aggregate_invoked.set(true),
        );
        assert!(
            !enrich_invoked.get(),
            "Groww must NEVER run the enrich (greeks/lifecycle) stage"
        );
        assert!(!tick.enriched, "Groww tick must be untouched by enrichment");
        assert!(persist_invoked.get(), "Groww persist stage must still run");
        assert!(
            aggregate_invoked.get(),
            "Groww aggregate stage must still run"
        );
    }

    #[test]
    fn test_consumer_persist_then_aggregate_order_preserved() {
        // Plan test 10: the persist stage precedes the aggregate fold for BOTH
        // feeds — no reordering. Recorded via a stage journal (test-only
        // allocation; the production path passes zero-alloc closures).
        for feed in [Feed::Dhan, Feed::Groww] {
            let journal: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());
            let mut tick = TestTick {
                ltp: 1.0,
                enriched: false,
            };
            consume_feed_tick(
                feed,
                &mut tick,
                |_| journal.borrow_mut().push("enrich"),
                |_| journal.borrow_mut().push("persist"),
                |_| journal.borrow_mut().push("aggregate"),
            );
            let expected: &[&str] = match feed {
                Feed::Dhan => &["enrich", "persist", "aggregate"],
                Feed::Groww => &["persist", "aggregate"],
            };
            assert_eq!(
                journal.borrow().as_slice(),
                expected,
                "stage order violated for {feed:?}"
            );
        }
    }

    #[test]
    fn test_consumer_null_column_policy_per_feed() {
        // Plan test 11 / edge E1: a tick through the shared consumer builds
        // the feed's C1 `RawTickFields` with the per-feed NULL-not-0 policy —
        // Groww's persist stage supplies `None` for every Dhan-only column
        // (→ NULL cell, never 0), Dhan supplies them all. The on-wire ILP
        // byte assertion for None→omitted-token lives with the C1 builder
        // (`tick_row_builder` tests + `feed_tick_writer_convergence_guard`);
        // this test pins the policy AT the consumer boundary and that the
        // aggregate stage still receives the Groww tick (the 21-TF fold slot).
        use tickvault_storage::tick_row_builder::RawTickFields;

        let groww_fields: Cell<Option<RawTickFields>> = Cell::new(None);
        let groww_aggregated = Cell::new(false);
        let mut groww_tick = TestTick {
            ltp: 2_847.55,
            enriched: false,
        };
        consume_feed_tick(
            Feed::Groww,
            &mut groww_tick,
            |_| {},
            |t| {
                // Mirrors `groww_persistence::append_row`'s field mapping:
                // Dhan-only columns are None → NULL (E1); received_at is the
                // plausibility-gated per-wake stamp (None here — broken-clock
                // case → NULL, never 0).
                groww_fields.set(Some(RawTickFields {
                    security_id: 1_333,
                    segment: "NSE_EQ",
                    ltp: t.ltp,
                    volume: 123_456,
                    ts_ist_nanos: 1_780_000_020_123_000_000,
                    capture_seq: 42,
                    open: None,
                    high: None,
                    low: None,
                    close: None,
                    oi: None,
                    avg_price: None,
                    last_trade_qty: None,
                    total_buy_qty: None,
                    total_sell_qty: None,
                    exchange_timestamp: Some(1_780_000_020),
                    received_at_ist_nanos: None,
                    payload_hash: None,
                }));
            },
            |_| groww_aggregated.set(true),
        );
        let g = groww_fields.get().expect("Groww persist stage must run");
        assert!(
            g.open.is_none()
                && g.high.is_none()
                && g.low.is_none()
                && g.close.is_none()
                && g.oi.is_none()
                && g.avg_price.is_none()
                && g.last_trade_qty.is_none()
                && g.total_buy_qty.is_none()
                && g.total_sell_qty.is_none()
                && g.payload_hash.is_none(),
            "Groww Dhan-only columns must be None (NULL), never a 0 placeholder"
        );
        assert!(
            (g.ltp - 2_847.55).abs() < f64::EPSILON,
            "Groww native f64 LTP passes verbatim"
        );
        assert!(
            groww_aggregated.get(),
            "the Groww tick must still reach the aggregate (21-TF fold) stage"
        );

        // Dhan half: every Dhan column populated (Some) through the same core.
        let dhan_fields: Cell<Option<RawTickFields>> = Cell::new(None);
        let mut dhan_tick = TestTick {
            ltp: 21_000.5,
            enriched: false,
        };
        consume_feed_tick(
            Feed::Dhan,
            &mut dhan_tick,
            |_| {},
            |t| {
                dhan_fields.set(Some(RawTickFields {
                    security_id: 49_081,
                    segment: "NSE_FNO",
                    ltp: t.ltp,
                    volume: 50_000,
                    ts_ist_nanos: 1_740_556_500_000_000_000,
                    capture_seq: 7,
                    open: Some(20_950.0),
                    high: Some(21_020.0),
                    low: Some(20_920.0),
                    close: Some(20_900.0),
                    oi: Some(120_000),
                    avg_price: Some(20_990.0),
                    last_trade_qty: Some(75),
                    total_buy_qty: Some(25_000),
                    total_sell_qty: Some(25_000),
                    exchange_timestamp: Some(1_740_556_500),
                    received_at_ist_nanos: Some(1_740_576_300_123_456_789),
                    payload_hash: Some(-1),
                }));
            },
            |_| {},
        );
        let d = dhan_fields.get().expect("Dhan persist stage must run");
        assert!(
            d.open.is_some()
                && d.oi.is_some()
                && d.received_at_ist_nanos.is_some()
                && d.payload_hash.is_some(),
            "Dhan rows carry the full column set"
        );
    }
}
