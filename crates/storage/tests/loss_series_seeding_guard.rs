//! Every loss series must be SEEDED, or an absent series reads as healthy.
//!
//! # The session that proved this
//!
//! On 2026-08-28 the live box dropped rows on both the tick and the depth
//! path. Read from CloudWatch afterwards:
//!
//! | series | session total |
//! |---|---|
//! | `tv_ticks_dropped_total` | 308,818 |
//! | `tv_ticks_spilled_total` | 308,818 |
//! | `tv_depth_rows_dropped_total` | 104,540 |
//! | `tv_depth_rows_spilled_total` | *no series at all* |
//! | `tv_depth_spill_write_errors_total` | *no series at all* |
//!
//! The tick side seeds both of its series, so `dropped - spilled = 0` proved
//! every dropped tick had been RESCUED to disk and nothing was permanently
//! lost. The depth side seeded only `dropped`, so the same subtraction could
//! not be performed: 104,540 rows that were either entirely rescued or
//! entirely gone, with no way to tell which.
//!
//! The failure is specific and worth naming precisely. The CloudWatch agent
//! computes a counter's value as the DELTA between consecutive samples and
//! drops the first sample of a series it has never seen. A counter that is
//! never incremented is therefore never registered, never published, and never
//! plotted — and an ABSENT series is indistinguishable from a healthy zero
//! one. So the discriminator whose entire purpose was to separate "survivable"
//! from "permanent" was silent in exactly the case it was built for.
//!
//! This test pins the seeding rather than the counting, because the counting
//! was already correct: the rescue arm increments both names, and always did.
//! What was missing was the zero that makes the series exist before the first
//! episode.

use std::collections::BTreeSet;

/// Pull the metric names passed to `metrics::counter!(...).increment(0)`
/// inside the named function.
fn seeded_series_in(source: &str, func_signature: &str) -> BTreeSet<String> {
    let start = source
        .find(func_signature)
        .unwrap_or_else(|| panic!("{func_signature} must exist"));
    // Bounded to the function body: the next line beginning `fn ` at column 0
    // ends it, so the window cannot widen to the whole file and pass vacuously.
    let end = source[start + 1..]
        .find("\nfn ")
        .map_or(source.len(), |offset| start + 1 + offset);
    let body = &source[start..end];

    let mut found = BTreeSet::new();
    for chunk in body.split("metrics::counter!(\"").skip(1) {
        let Some((name, rest)) = chunk.split_once('"') else {
            continue;
        };
        // Only an `.increment(0)` is a SEED. An `.increment(n)` in the same
        // function would be a real count and must not be mistaken for one.
        let terminator = rest.find("metrics::counter!").unwrap_or(rest.len());
        if rest[..terminator].contains("increment(0)") {
            found.insert(name.to_owned());
        }
    }
    found
}

#[test]
fn every_depth_loss_series_is_seeded_before_the_first_episode() {
    let source = include_str!("../src/depth_persistence.rs");
    let seeded = seeded_series_in(source, "fn register_depth_drop_baseline(");

    for required in [
        "tv_depth_rows_dropped_total",
        // Without this one the 2026-08-28 session could not say whether
        // 104,540 depth rows were rescued or permanently lost.
        "tv_depth_rows_spilled_total",
        "tv_depth_spill_write_errors_total",
        "tv_depth_persist_errors_total",
    ] {
        assert!(
            seeded.contains(required),
            "{required} is not seeded in register_depth_drop_baseline. An unseeded \
             counter does not publish a zero — it publishes NOTHING, and an absent \
             series is indistinguishable from a healthy one. Seeded series were: \
             {seeded:?}"
        );
    }
}

#[test]
fn every_tick_loss_series_is_seeded_before_the_first_episode() {
    let source = include_str!("../src/tick_persistence.rs");
    let seeded = seeded_series_in(source, "fn register_drop_baseline(");

    for required in [
        "tv_ticks_dropped_total",
        "tv_ticks_spilled_total",
        "tv_tick_persist_errors_total",
    ] {
        assert!(
            seeded.contains(required),
            "{required} is not seeded in register_drop_baseline. Seeded series \
             were: {seeded:?}"
        );
    }
}

/// A drop counter and its rescue discriminator must be seeded TOGETHER.
///
/// The pair is what carries the meaning: `dropped` alone says something went
/// wrong, and only `dropped - spilled` says whether it was survivable. Seeding
/// one without the other produces the worst of both — a pager that fires and a
/// diagnosis that cannot be made — which is exactly what 2026-08-28 delivered.
#[test]
fn a_drop_counter_is_never_seeded_without_its_rescue_discriminator() {
    for (path, source, func, pairs) in [
        (
            "depth_persistence.rs",
            include_str!("../src/depth_persistence.rs"),
            "fn register_depth_drop_baseline(",
            [("tv_depth_rows_dropped_total", "tv_depth_rows_spilled_total")],
        ),
        (
            "tick_persistence.rs",
            include_str!("../src/tick_persistence.rs"),
            "fn register_drop_baseline(",
            [("tv_ticks_dropped_total", "tv_ticks_spilled_total")],
        ),
    ] {
        let seeded = seeded_series_in(source, func);
        for (dropped, spilled) in pairs {
            assert_eq!(
                seeded.contains(dropped),
                seeded.contains(spilled),
                "{path}: {dropped} and {spilled} must be seeded together — one \
                 without the other gives a pager with no diagnosis. Seeded: {seeded:?}"
            );
        }
    }
}

/// Bite-proof: the extractor must NOT count a real increment as a seed.
///
/// Without this the guard could pass on a function that counts rows but never
/// seeds, which is the precise state it exists to forbid.
#[test]
fn the_extractor_rejects_a_real_increment_as_a_seed() {
    let fixture = r#"
fn register_fake_baseline(feed: Feed) {
    metrics::counter!("tv_seeded_total", "feed" => feed.as_str()).increment(0);
    metrics::counter!("tv_counted_total", "feed" => feed.as_str()).increment(rows as u64);
}
"#;
    let seeded = seeded_series_in(fixture, "fn register_fake_baseline(");
    assert!(
        seeded.contains("tv_seeded_total"),
        "an increment(0) must be read as a seed"
    );
    assert!(
        !seeded.contains("tv_counted_total"),
        "an increment(n) is a COUNT, not a seed — reading it as one would let a \
         function that never seeds pass this guard"
    );
}
