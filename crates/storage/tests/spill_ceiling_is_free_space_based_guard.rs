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
    let block = ceiling_block(&src, "if spill_dir_bytes(dir) >= ceiling {")
        .expect("the tick soft-ceiling comparison must still exist");

    assert!(
        block.contains("probe_disk_free_bytes"),
        "the tick spill ceiling refuses without consulting free space. On \
         2026-09-01 that exact shape discarded 5,141,980 ticks with 136 GB \
         free. A ceiling derived from the volume's TOTAL size is a constant; \
         it cannot tell you whether QuestDB has room."
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
    let block = ceiling_block(&src, "if depth_spill_dir_bytes(dir) >= cap_bytes {")
        .expect("the depth soft-cap comparison must still exist");

    assert!(
        block.contains("probe_disk_free_bytes"),
        "the depth spill cap refuses without consulting free space. This is \
         the writer that discarded 238,615,500 rows on 2026-09-01 — 46x the \
         tick loss — because depth carries ~24x the tick row volume"
    );
    assert!(
        block.contains("SPILL_SOFT_CEILING_FREE_RESERVE_BYTES"),
        "the depth refusal must use the SAME database reserve as the tick \
         writer. Two spill tiers on one volume with different reserves would \
         let whichever is looser starve the other"
    );
    assert!(
        block.contains("tv_depth_spill_over_soft_cap_total"),
        "growth past the soft cap must be COUNTED"
    );
}

/// A probe that FAILS must refuse, not allow.
///
/// This is the direction the fix could most easily be got wrong in: having
/// established that free space licenses the write, an unreadable probe looks
/// like a case for optimism. It is the opposite — an unknown free-space
/// number is exactly when unbounded growth is least affordable.
#[test]
fn a_failed_free_space_probe_still_refuses_on_both_writers() {
    for (path, compare) in [
        (
            "src/tick_persistence.rs",
            "if spill_dir_bytes(dir) >= ceiling {",
        ),
        (
            "src/depth_persistence.rs",
            "if depth_spill_dir_bytes(dir) >= cap_bytes {",
        ),
    ] {
        let src = production_half(path);
        let block = ceiling_block(&src, compare).expect("comparison must exist");
        let probe_failed = block
            .find("ProbeFailed")
            .unwrap_or_else(|| panic!("{path}: the ProbeFailed arm is gone"));
        let after = &block[probe_failed..];
        assert!(
            after.contains("return Err("),
            "{path}: the ProbeFailed arm must REFUSE. Allowing on an \
             unreadable probe trades a bounded tick loss for an unbounded \
             disk, which is the outage this whole tier exists to avoid"
        );
    }
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
