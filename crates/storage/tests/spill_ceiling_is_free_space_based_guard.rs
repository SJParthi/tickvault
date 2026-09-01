//! Guard: neither spill tier may refuse a rescue on TOTAL volume size alone.
//!
//! # The incident this pins (MEASURED in production, 2026-09-01)
//!
//! Both spill writers derived their ceiling as `volume_total_bytes / 32` —
//! 3.1% of the disk, a figure that never moves because the volume's total
//! size never moves. On a 322 GB volume that is `10_063_871_360` bytes, and
//! the live log recorded the refusal verbatim:
//!
//! ```text
//! "spill_error": "depth spill dir at or past its 10063871360-byte cap",
//! "cap_bytes":   10063871360
//! ```
//!
//! It fired with **146,511,863,808 bytes (136 GB) free** on a disk that was
//! 53% used, and the rows behind it were discarded permanently:
//!
//! | counter | value |
//! |---|---|
//! | `tv_ticks_dropped_total` | 33,419,938 |
//! | `tv_ticks_spilled_total` | 28,277,958 |
//! | **ticks lost with nowhere to go** | **5,141,980** |
//! | `tv_depth_rows_dropped_total` | 312,708,433 |
//! | `tv_depth_rows_spilled_total` | 74,092,933 |
//! | **depth rows lost with nowhere to go** | **238,615,500** |
//!
//! # What this guard does and does NOT assert
//!
//! It does NOT say the ceiling is wrong. A rail that stops the rescue tier
//! from starving the database it rescues from is correct and is kept. What it
//! pins is the QUANTITY the refusal is measured against: total size cannot
//! threaten QuestDB, only FREE space can — so a refusal must consult a
//! free-space probe, and may not be reached by size comparison alone.
//!
//! The scan is deliberately structural rather than a literal match on the
//! error text: an error string can be reworded while the defect returns.
#![allow(clippy::expect_used, clippy::panic)]

/// Reads a source file and returns only its PRODUCTION half.
///
/// Splitting at `#[cfg(test)]` is not cosmetic. A previous guard in this
/// repository scanned a whole file and passed while the real call site was
/// deleted, because the file's own test module quoted the very literal the
/// scan was looking for. A scan that can be satisfied by a string inside a
/// test is not a guard.
fn production_half(path: &str) -> String {
    let whole = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let prod = whole
        .split_once("#[cfg(test)]")
        .map_or(whole.clone(), |(p, _)| p.to_string());
    assert!(
        prod.len() > 1_000,
        "{path}: the production half scanned as {} bytes — the split marker \
         moved and this guard is checking nothing",
        prod.len()
    );
    prod
}

/// Everything between the ceiling comparison and the end of its block.
///
/// Returns `None` when the comparison itself is gone, which callers treat as
/// a failure rather than a pass: a missing rail is a different defect, not an
/// acceptable one.
fn ceiling_block<'a>(src: &'a str, compare: &str) -> Option<&'a str> {
    let at = src.find(compare)?;
    // 3,000 bytes is comfortably longer than either match arm set and short
    // enough that it cannot reach an unrelated free-space check further down
    // the function (the live-headroom guard, which is a separate mechanism).
    let end = (at + 3_000).min(src.len());
    Some(&src[at..end])
}

#[test]
fn the_tick_spill_ceiling_refuses_only_against_free_space() {
    let src = production_half("src/tick_persistence.rs");
    let block = ceiling_block(&src, "let ceiling = tick_spill_max_bytes();")
        .expect("the tick soft-ceiling comparison must still exist");

    assert!(
        block.contains("classify_spill_ceiling("),
        "the tick spill ceiling decides without consulting \
         `classify_spill_ceiling`. On 2026-09-01 a size-only refusal \
         discarded 5,141,980 ticks with 136 GB free. A ceiling derived from \
         the volume's TOTAL size is a constant; it cannot tell you whether \
         QuestDB has room."
    );
    assert!(
        block.contains("spill_free_bytes("),
        "the decision must be fed REAL free space, not a placeholder"
    );
    assert!(
        block.contains("SPILL_SOFT_CEILING_FREE_RESERVE_BYTES"),
        "the refusal must be measured against the database reserve, not \
         against the size rail alone"
    );
    assert!(
        block.contains("tv_tick_spill_over_soft_ceiling_total"),
        "growth past the soft rail must be COUNTED. The 2026-09-01 loss was \
         invisible precisely because nothing counted the refusals"
    );
}

#[test]
fn the_depth_spill_ceiling_refuses_only_against_free_space() {
    let src = production_half("src/depth_persistence.rs");
    let block = ceiling_block(&src, "classify_spill_ceiling(")
        .expect("the depth soft-cap decision must route through the classifier");

    assert!(
        block.contains("spill_free_bytes("),
        "the depth spill cap decides without real free space. This is the \
         writer that discarded 238,615,500 rows on 2026-09-01 -- 46x the tick \
         loss -- because depth carries ~24x the tick row volume"
    );
    // INVERTED 2026-09-01, hours after it was written, and the reversal is
    // the record worth keeping. This assertion originally demanded the
    // depth writer use the SAME reserve as the tick writer, reasoning that
    // "two spill tiers on one volume with different reserves would let
    // whichever is looser starve the other".
    //
    // That reasoning was right about the mechanism and wrong about the
    // direction. Sharing one reserve does not stop starvation -- it
    // GUARANTEES it, in the worst direction: depth writes 10 rows per packet
    // against the tick tier's 1, so it exhausts the shared reserve first and
    // both tiers then refuse together. The measured result on the day this
    // was written was 84.6% tick rescue against 23.7% depth -- record-only
    // depth already taking decision-critical ticks down with it.
    //
    // The correct rule is an ORDERING, not equality: depth must reserve MORE,
    // so it refuses while ticks are still rescued. Pinned as a property over
    // the whole band in `depth_refuses_while_ticks_are_still_rescued`.
    assert!(
        block.contains("DEPTH_SPILL_FREE_RESERVE_BYTES"),
        "the depth refusal must use the DEPTH reserve, which is deliberately \
         LARGER than the tick reserve so record-only depth gives way before \
         decision-critical ticks do"
    );
    assert!(
        block.contains("tv_depth_spill_over_soft_cap_total"),
        "growth past the soft cap must be COUNTED"
    );
}

/// Both writers must share ONE decision, not two lookalike copies.
///
/// Recorded because that is how the original defect scaled: the tick and
/// depth writers each carried their own `/ 32` rail, so the same wrong idea
/// had to be found and fixed twice. A single function means a future change
/// to the rule cannot land on one writer and miss the other.
#[test]
fn both_writers_share_one_decision_function() {
    let depth = production_half("src/depth_persistence.rs");
    assert!(
        depth.contains("crate::tick_persistence::classify_spill_ceiling("),
        "the depth writer must call the SHARED classifier, not a private copy \
         of the same reasoning"
    );
    let tick = production_half("src/tick_persistence.rs");
    assert!(
        tick.contains("pub const fn classify_spill_ceiling("),
        "the shared classifier must remain PUBLIC and const -- it is the one \
         place the rule is written down, and the depth writer reaches it \
         across module boundaries"
    );
    // The old per-writer inline shape must not come back alongside it.
    //
    // `spill_free_bytes` is the ONE place allowed to touch the raw probe
    // outcome -- collapsing it to `Option<u64>` is its entire job -- so its
    // body is cut out before counting. Everywhere else, matching on
    // `DiskHealthOutcome` means a writer has started deciding for itself
    // again, which is precisely how the two rails drifted apart.
    for (name, src) in [("tick", &tick), ("depth", &depth)] {
        let outside_helper = src.replace(
            "match crate::disk_health_watcher::probe_disk_free_bytes(dir) {\n        crate::disk_health_watcher::DiskHealthOutcome::Ok { free_bytes, .. } => Some(free_bytes),\n        crate::disk_health_watcher::DiskHealthOutcome::ProbeFailed { .. } => None,\n    }",
            "<collapsed by spill_free_bytes>",
        );
        let inline = outside_helper
            .matches("DiskHealthOutcome::ProbeFailed")
            .count();
        assert_eq!(
            inline, 0,
            "{name} writer matches on the raw probe outcome at {inline} site(s) \
             OUTSIDE `spill_free_bytes`. The probe collapses to Option<u64> \
             there so the decision stays a pure function of numbers -- \
             re-inlining it is how the two writers drifted apart in the first \
             place"
        );
    }
}

/// A probe that FAILS must refuse, not allow.
///
/// This is the direction the fix could most easily be got wrong in: having
/// established that free space licenses the write, an unreadable probe looks
/// like a case for optimism. It is the opposite -- an unknown free-space
/// number is exactly when unbounded growth is least affordable.
///
/// Asserted against the CLASSIFIER rather than a source scan, because this is
/// a behaviour and behaviours are testable. The source scan above proves the
/// writers reach it; this proves what it decides.
#[test]
fn a_failed_free_space_probe_still_refuses() {
    use tickvault_storage::tick_persistence::{
        SPILL_SOFT_CEILING_FREE_RESERVE_BYTES, SpillCeilingVerdict, classify_spill_ceiling,
    };
    const R: u64 = SPILL_SOFT_CEILING_FREE_RESERVE_BYTES;

    // Every arm, exhaustively. None of this needs a filesystem, which is the
    // point: the arm that protects the database was previously untestable
    // because a unit test cannot manufacture a nearly-full disk.
    let cases = [
        (99_u64, 100_u64, None, SpillCeilingVerdict::UnderCeiling),
        (99, 100, Some(0), SpillCeilingVerdict::UnderCeiling),
        (100, 100, None, SpillCeilingVerdict::OverCeilingProbeFailed),
        (100, 100, Some(0), SpillCeilingVerdict::OverCeilingNoRoom),
        (100, 100, Some(R), SpillCeilingVerdict::OverCeilingNoRoom),
        (
            100,
            100,
            Some(R + 1),
            SpillCeilingVerdict::OverCeilingWithRoom,
        ),
        (
            u64::MAX,
            100,
            Some(u64::MAX),
            SpillCeilingVerdict::OverCeilingWithRoom,
        ),
        (
            u64::MAX,
            100,
            None,
            SpillCeilingVerdict::OverCeilingProbeFailed,
        ),
    ];
    for (held, ceiling, free, want) in cases {
        assert_eq!(
            classify_spill_ceiling(held, ceiling, free, R),
            want,
            "held={held} ceiling={ceiling} free={free:?} reserve={R}"
        );
    }

    // The boundary, stated on its own because it is the one a reader will
    // want to check: EXACTLY on the reserve refuses. At a boundary the safe
    // direction is the database's.
    assert_eq!(
        classify_spill_ceiling(1, 1, Some(R), R),
        SpillCeilingVerdict::OverCeilingNoRoom,
        "sitting exactly ON the reserve must refuse"
    );
}

/// THE PRIORITY BAND: depth must refuse while ticks are still rescued.
///
/// This is the whole point of giving the two tiers different reserves, and it
/// is asserted as a property over a range rather than at one convenient
/// number, because a single sample can be satisfied by two reserves that
/// happen to differ without the ORDER being guaranteed.
///
/// # The rule it encodes
///
/// Ticks are decision-critical: a strategy reads folded tick state from RAM
/// and can never wait on the database. Depth is record-only -- verified zero
/// readers in the indicator, strategy and risk paths. So when disk gets
/// tight, depth is the lane that gives way.
///
/// Until 2026-09-01 both tiers shared ONE reserve, which produced exactly the
/// opposite: depth writes 10 rows per packet against the tick tier's 1, so it
/// consumed the shared reserve first and both refused together. The measured
/// consequence that day was a rescue success of 84.6% for ticks against 23.7%
/// for depth -- the record-only lane starving the decision-critical one.
#[test]
fn depth_refuses_while_ticks_are_still_rescued() {
    use tickvault_storage::tick_persistence::{
        DEPTH_SPILL_FREE_RESERVE_BYTES, SPILL_SOFT_CEILING_FREE_RESERVE_BYTES, SpillCeilingVerdict,
        classify_spill_ceiling,
    };

    assert!(
        DEPTH_SPILL_FREE_RESERVE_BYTES > SPILL_SOFT_CEILING_FREE_RESERVE_BYTES,
        "depth must reserve MORE than ticks. Equal reserves are the 2026-09-01 \
         defect: both tiers refuse at the same moment, so record-only depth \
         takes decision-critical ticks down with it"
    );

    // Sample across the whole band, not just its edges. Both tiers are past
    // their size rail here (held == ceiling), so the only variable is free
    // space -- which is exactly the question the reserves answer.
    let lo = SPILL_SOFT_CEILING_FREE_RESERVE_BYTES;
    let hi = DEPTH_SPILL_FREE_RESERVE_BYTES;
    let band = hi - lo;
    assert!(
        band >= 8 * 1024 * 1024 * 1024,
        "the band is only {band} bytes. It must be wide enough to hold a \
         session's worth of tick spill, or depth stepping aside buys ticks \
         nothing"
    );

    for step in 1..=64_u64 {
        // Strictly inside the band: (lo, hi].
        let free = lo + (band * step / 64);
        assert_eq!(
            classify_spill_ceiling(1, 1, Some(free), SPILL_SOFT_CEILING_FREE_RESERVE_BYTES),
            SpillCeilingVerdict::OverCeilingWithRoom,
            "at {free} bytes free, TICKS must still be rescued -- they are the \
             lane a trading decision depends on"
        );
        assert_eq!(
            classify_spill_ceiling(1, 1, Some(free), DEPTH_SPILL_FREE_RESERVE_BYTES),
            SpillCeilingVerdict::OverCeilingNoRoom,
            "at {free} bytes free, DEPTH must refuse -- it is record-only, and \
             its rows must not consume the room ticks need"
        );
    }

    // ABOVE the band: both are rescued. Depth stepping aside must not become
    // depth being permanently off.
    let plenty = hi + 1;
    assert_eq!(
        classify_spill_ceiling(1, 1, Some(plenty), DEPTH_SPILL_FREE_RESERVE_BYTES),
        SpillCeilingVerdict::OverCeilingWithRoom,
        "with room to spare, depth must still be rescued -- refusing here is \
         the original defect (243 million rows discarded onto a 55%-empty disk)"
    );

    // BELOW the band: both refuse. Ticks are decision-critical, not more
    // important than the database staying up -- if the database dies there is
    // nothing to decide with.
    let starved = lo.saturating_sub(1);
    for reserve in [
        SPILL_SOFT_CEILING_FREE_RESERVE_BYTES,
        DEPTH_SPILL_FREE_RESERVE_BYTES,
    ] {
        assert_eq!(
            classify_spill_ceiling(1, 1, Some(starved), reserve),
            SpillCeilingVerdict::OverCeilingNoRoom,
            "below the database reserve BOTH tiers must refuse -- ticks do not \
             outrank QuestDB having room to operate"
        );
    }
}

/// The depth reserve must stay DERIVED, never re-hardcoded.
///
/// Pinned because the number's defensibility lives entirely in its
/// derivation: depth stops rescuing while a whole session of ticks could
/// still spill on top of the database's own reserve. A literal `32 GiB` here
/// would be the same figure with none of the reasoning, and the next person
/// to change the tick reserve would silently break the ordering.
#[test]
fn the_depth_reserve_is_derived_from_the_tick_reserve() {
    use tickvault_storage::tick_persistence::{
        DEPTH_SPILL_FREE_RESERVE_BYTES, SPILL_SOFT_CEILING_FREE_RESERVE_BYTES,
        WORST_CASE_SESSION_TICK_SPILL_BYTES,
    };
    assert_eq!(
        DEPTH_SPILL_FREE_RESERVE_BYTES,
        SPILL_SOFT_CEILING_FREE_RESERVE_BYTES + WORST_CASE_SESSION_TICK_SPILL_BYTES,
        "the depth reserve must remain the SUM of the database reserve and a \
         session's worst-case tick spill -- that sum is the reason the number \
         is defensible, and a literal would keep the value while losing it"
    );
    // The measured basis: 83,446,729 ticks x 144 B = 12.0 GB on 2026-09-01.
    // The constant must cover it with real margin, not sit on top of it.
    const MEASURED_SESSION_TICK_SPILL: u64 = 83_446_729 * 144;
    assert!(
        WORST_CASE_SESSION_TICK_SPILL_BYTES > MEASURED_SESSION_TICK_SPILL,
        "the worst-case allowance ({WORST_CASE_SESSION_TICK_SPILL_BYTES}) must \
         exceed the MEASURED session ({MEASURED_SESSION_TICK_SPILL}) -- sitting \
         at or below it means a normal day exhausts the allowance"
    );

    let src = production_half("src/depth_persistence.rs");
    assert!(
        src.contains("DEPTH_SPILL_FREE_RESERVE_BYTES"),
        "the depth writer must pass the DEPTH reserve"
    );
    assert!(
        !src.contains("SPILL_SOFT_CEILING_FREE_RESERVE_BYTES"),
        "the depth writer must NOT reach for the tick reserve -- that is the \
         shared-reserve defect returning under a different name"
    );
}

/// Bite-proof for the scanner itself.
///
/// Every assertion above is a `contains` on an extracted window, so the two
/// ways this guard could be vacuous are (a) the window extractor silently
/// returning something harmless and (b) `contains` matching text that is not
/// in the window at all. Both are exercised here against fixtures rather than
/// against the real file, so the check does not move when the file does.
#[test]
fn guard_self_test() {
    let good = "if spill_dir_bytes(dir) >= ceiling {\n\
                    match probe_disk_free_bytes(dir) {\n\
                        Ok { free_bytes } if free_bytes > SPILL_SOFT_CEILING_FREE_RESERVE_BYTES => {\n\
                            counter!(\"tv_tick_spill_over_soft_ceiling_total\");\n\
                        }\n\
                        ProbeFailed { .. } => return Err(e),\n\
                    }\n\
                }";
    let bad = "if spill_dir_bytes(dir) >= ceiling {\n\
                   return Err(StorageFull);\n\
               }";

    let g = ceiling_block(good, "if spill_dir_bytes(dir) >= ceiling {").expect("found");
    assert!(g.contains("probe_disk_free_bytes"), "must see the probe");
    assert!(
        g.contains("SPILL_SOFT_CEILING_FREE_RESERVE_BYTES"),
        "must see the reserve"
    );

    let b = ceiling_block(bad, "if spill_dir_bytes(dir) >= ceiling {").expect("found");
    assert!(
        !b.contains("probe_disk_free_bytes"),
        "the pre-fix shape MUST fail this scan — if it passes, the guard is \
         decorative and the 2026-09-01 defect can return unnoticed"
    );

    // The window extractor must report a MISSING rail rather than an empty
    // pass: `None` is what the callers `.expect()` on.
    assert!(
        ceiling_block("unrelated source", "if spill_dir_bytes(dir) >= ceiling {").is_none(),
        "a missing comparison must be None, never an empty window that every \
         `contains` trivially fails against in the wrong direction"
    );
}
