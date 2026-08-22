//! Nine ceilings, five crates, one number — and nothing making them agree.
//!
//! Found 2026-08-22. Every per-entity cap in this workspace is `25_000`, and
//! every one of them says in its own doc comment that it is deliberately the
//! same as the others:
//!
//! - `MAX_TRACKED_POSITIONS` — *"so the bounds agree rather than each inventing
//!   a number"*
//! - `day_ohlc_tracker::MAX_TRACKED_INSTRUMENTS` — *"so the three bounds agree
//!   rather than each inventing a number"*
//! - `tick_gap_tracker::MAX_TRACKED_INSTRUMENTS` — *"one number to reason about
//!   rather than two that can drift apart"*
//! - `MAX_TRACKED_ORDERS` — *"the same figure every other per-entity ceiling in
//!   this workspace uses"*
//! - `MAX_TRACKED_SIDS` — *"matching every other per-entity ceiling"*
//!
//! Five separate comments promise agreement. Nothing enforced it. The one that
//! names the risk out loud — *"two that can drift apart"* — had no mechanism
//! stopping exactly that.
//!
//! **Why drift is expensive rather than merely untidy.** The figure is not
//! arbitrary: it is the authorized main-feed capacity, 5 connections × 5,000
//! instruments. Raising the universe without raising the rest does not fail
//! loudly at the seam — each cap is independently fail-closed, so the system
//! keeps running and simply *refuses* the instruments past the lowest ceiling,
//! one subsystem at a time. Half the system would be scaled and half would not,
//! and the only evidence would be refusal counters ticking in production on
//! instruments the operator had legitimately authorized.
//!
//! This test does not decide the number. It requires the nine to move together.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common -> crates -> repo root")
        .to_path_buf()
}

/// (file, constant name) — keyed on both, because two of them share a name in
/// different crates and a name-only scan would silently check one twice.
const CEILINGS: &[(&str, &str)] = &[
    ("crates/common/src/constants.rs", "MAX_DAILY_UNIVERSE_SIZE"),
    (
        "crates/common/src/constants.rs",
        "MAX_INDICATOR_INSTRUMENTS",
    ),
    (
        "crates/trading/src/candles/multi_tf_aggregator.rs",
        "AGGREGATOR_MAX_SLOTS",
    ),
    (
        "crates/trading/src/in_mem/day_ohlc_tracker.rs",
        "MAX_TRACKED_INSTRUMENTS",
    ),
    (
        "crates/trading/src/risk/tick_gap_tracker.rs",
        "MAX_TRACKED_INSTRUMENTS",
    ),
    ("crates/trading/src/risk/engine.rs", "MAX_TRACKED_POSITIONS"),
    ("crates/trading/src/oms/engine.rs", "MAX_TRACKED_ORDERS"),
    ("crates/app/src/rest_candle_fold.rs", "FOLD_MAX_SLOTS"),
    ("crates/app/src/order_runtime.rs", "MAX_TRACKED_SIDS"),
];

/// Read `const NAME: usize = <literal>;` out of a file, underscores stripped.
fn ceiling_value(rel: &str, name: &str) -> Option<usize> {
    let body = std::fs::read_to_string(repo_root().join(rel)).ok()?;
    body.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .find_map(|l| {
            let head = format!("const {name}:");
            let idx = l.find(&head)?;
            let rhs = l[idx..].split('=').nth(1)?.trim();
            let digits: String = rhs
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '_')
                .filter(|c| *c != '_')
                .collect();
            digits.parse::<usize>().ok()
        })
}

#[test]
fn every_per_entity_ceiling_is_the_same_number() {
    let mut found: Vec<(&str, &str, usize)> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    for (file, name) in CEILINGS {
        match ceiling_value(file, name) {
            Some(v) => found.push((file, name, v)),
            None => missing.push(format!("{name} in {file}")),
        }
    }

    assert!(
        missing.is_empty(),
        "these ceilings could not be read, so this guard would have passed \
         while checking nothing:\n  {}\n\nIf a constant was renamed or moved, \
         update CEILINGS in the same change — do not delete the entry.",
        missing.join("\n  ")
    );

    let baseline = found[0].2;
    let drifted: Vec<String> = found
        .iter()
        .filter(|(_, _, v)| *v != baseline)
        .map(|(f, n, v)| format!("{n} = {v} ({f})"))
        .collect();

    assert!(
        drifted.is_empty(),
        "per-entity ceilings have drifted apart. Baseline is {baseline} \
         (from {}), and these disagree:\n  {}\n\n\
         Each cap is independently fail-closed, so drift does NOT fail loudly \
         at the seam: the system keeps running and refuses instruments past the \
         LOWEST ceiling, one subsystem at a time. Half scaled, half not, with \
         refusal counters ticking on instruments that were legitimately \
         authorized.\n\
         If the universe ceiling is being raised, raise all nine together. If \
         one genuinely must differ, say why HERE — the doc comments on five of \
         these promise they agree.",
        found[0].0,
        drifted.join("\n  ")
    );
}

#[test]
fn the_ceiling_matches_the_authorized_subscription_capacity() {
    // 25,000 is not a round number chosen for looks: the websocket scope lock
    // authorizes 5 main-feed connections at 5,000 instruments each. If that
    // authorization changes, this is the line that should make someone say so.
    let universe = ceiling_value("crates/common/src/constants.rs", "MAX_DAILY_UNIVERSE_SIZE")
        .expect("MAX_DAILY_UNIVERSE_SIZE must be readable");
    assert_eq!(
        universe,
        5 * 5_000,
        "MAX_DAILY_UNIVERSE_SIZE is {universe}, which is no longer 5 connections \
         × 5,000 instruments. That figure comes from a dated operator \
         authorization in websocket-connection-scope-lock.md; changing it needs \
         a fresh dated quote there first, not just a different literal here."
    );
}

#[test]
fn guard_self_test() {
    // The reader must actually read, and must not be fooled by a commented-out
    // constant — otherwise every assertion above passes vacuously.
    assert!(
        ceiling_value("crates/common/src/constants.rs", "MAX_DAILY_UNIVERSE_SIZE").is_some(),
        "the constant reader cannot read a constant that is demonstrably there"
    );
    assert_eq!(
        ceiling_value("crates/common/src/constants.rs", "NOT_A_REAL_CONSTANT_NAME"),
        None,
        "the constant reader invents values for names that do not exist"
    );
    assert_eq!(
        CEILINGS.len(),
        9,
        "the ceiling list changed size — that is fine, but it should be a \
         deliberate edit with the reason recorded, not a silent drift"
    );
}
