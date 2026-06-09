//! Re-runnable latency proof for the same-day warm-resubscribe fast path.
//!
//! Measures the REAL wall-clock cost of recovering the subscription plan from
//! the on-disk snapshot after a crash: load the date-keyed snapshot file →
//! reconstruct the `DailyUniverse` → build the full subscription plan, at the
//! live production scale of 331 SIDs (3 indices + 328 F&O underlyings).
//!
//! This exists because the charter demands "continuous proof and evidence
//! generation" and "no hallucination" — the "~1ms warm resubscribe" figure
//! must be a MEASURED fact, not a claim. Run it anytime:
//!
//! ```bash
//! cargo run --example warm_resubscribe_latency \
//!     --features daily_universe_fetcher --release
//! ```
//!
//! Observed on the dev container (release): ~316 µs/iter — i.e. sub-millisecond.
//! This is O(N) in the SID count (you must build N subscriptions) but constant
//! per-SID; it replaces an O(N) + network-GET + DB-reconcile cold build
//! (batched seconds) with a single bounded disk read + in-memory rebuild.

#[cfg(not(feature = "daily_universe_fetcher"))]
fn main() {
    eprintln!(
        "warm_resubscribe_latency requires --features daily_universe_fetcher; \
         re-run with that flag."
    );
}

#[cfg(feature = "daily_universe_fetcher")]
fn main() {
    use std::time::Instant;
    use tickvault_core::instrument::csv_parser::CsvRow;
    use tickvault_core::instrument::daily_universe::{
        DailyUniverse, InstrumentRole, SubscriptionTarget,
    };
    use tickvault_core::instrument::instrument_snapshot::{
        load_plan_snapshot_for_today, write_plan_snapshot,
    };
    use tickvault_core::instrument::subscription_planner::build_subscription_plan_from_daily_universe;

    fn target(role: InstrumentRole, sid: &str, sym: &str) -> SubscriptionTarget {
        SubscriptionTarget {
            role,
            is_fno_underlying: role == InstrumentRole::FnoUnderlying,
            is_index_constituent: role == InstrumentRole::IndexConstituent,
            csv_row: CsvRow {
                security_id: sid.to_string(),
                symbol_name: sym.to_string(),
                ..CsvRow::default()
            },
        }
    }

    const INDICES: usize = 3;
    const UNDERLYINGS: usize = 328;
    let total_sids = INDICES + UNDERLYINGS;

    let mut targets = Vec::with_capacity(total_sids);
    targets.push(target(InstrumentRole::Index, "13", "NIFTY"));
    targets.push(target(InstrumentRole::Index, "25", "BANKNIFTY"));
    targets.push(target(InstrumentRole::Index, "51", "SENSEX"));
    for i in 0..UNDERLYINGS {
        targets.push(target(
            InstrumentRole::FnoUnderlying,
            &(10_000 + i).to_string(),
            &format!("STK{i}"),
        ));
    }
    let universe = DailyUniverse {
        subscription_targets: targets,
        fno_contracts: vec![],
    };

    // A unique far-future date so this never collides with a real snapshot.
    let date = "2099-12-31";
    if let Err(e) = write_plan_snapshot(&universe, date) {
        eprintln!("setup write failed: {e}");
        return;
    }

    let iters: u32 = 2000;
    // Warm up (file cache + allocator) before timing.
    for _ in 0..50 {
        let _ = load_plan_snapshot_for_today(date);
    }

    let start = Instant::now();
    let mut sink = 0usize;
    for _ in 0..iters {
        let Some(restored) = load_plan_snapshot_for_today(date) else {
            eprintln!("load returned None during measurement");
            return;
        };
        let plan = build_subscription_plan_from_daily_universe(&restored);
        sink = sink.wrapping_add(plan.summary.total);
    }
    let per = start.elapsed() / iters;

    println!("warm-resubscribe latency proof");
    println!(
        "  scale          : {total_sids} SIDs ({INDICES} indices + {UNDERLYINGS} underlyings)"
    );
    println!("  path           : load snapshot -> reconstruct universe -> build plan");
    println!("  iterations     : {iters}");
    println!("  per-iter        : {per:?}");
    println!("  plan.total/iter: {}", sink / iters as usize);

    // Best-effort cleanup of the proof artefact.
    if let Some(path) = tickvault_core::instrument::instrument_snapshot::snapshot_path_for(date) {
        let _ = std::fs::remove_file(path);
    }
}
