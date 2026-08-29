//! Shrink-only ratchet on the RiskEngine's segment-less LEGACY overloads.
//!
//! # What this guards, and why a ratchet rather than a fix
//! `RiskEngine` keys positions on the I-P1-11 composite
//! `(security_id, exchange_segment)`, but still exposes segment-less overloads
//! (`check_order`, `record_fill`, `update_market_price`, `net_lots_for`,
//! `position`) that hard-code `LEGACY_DEFAULT_SEGMENT = ExchangeSegment::IdxI`.
//! Every one of them books or reads a position on a segment the caller did not
//! choose.
//!
//! On 2026-08-29 the LIVE caller — `order_runtime.rs`, the paper-trading actor
//! that runs today with `[order_runtime] enabled = true` — was migrated to the
//! `*_in_segment` variants. Two callers were deliberately NOT migrated in that
//! change, and this guard pins them so the set can only shrink:
//!
//! | File | Why it was left | Reachability |
//! |---|---|---|
//! | `trading_pipeline.rs` | `spawn_trading_pipeline` has ZERO production call sites, pinned by `order_side_wiring_guard.rs`; the health handler reports the pipeline as `retired` | dead |
//! | `exit_execution.rs` | behind the four-gate exit-order lockout (`dhan-exit-order-lockout-2026-07-14.md`) — config default-off, dispatcher early-return, `dry_run` hardcoded, ratchet | gated |
//!
//! Migrating dormant code carries its own risk and buys nothing today. What
//! this guard buys is that the defect cannot SPREAD: a new legacy call site
//! anywhere in production source fails the build, and whoever revives either
//! dormant path has to come through here.
//!
//! # Honest limit
//! This is a source scan over the text before each file's `#[cfg(test)]`
//! marker. It cannot see a call routed through a wrapper function, and it does
//! not read `crates/trading/src/risk/engine.rs` itself (the overloads are
//! defined there and call their own `*_in_segment` bodies by construction).

use std::collections::BTreeMap;
use std::path::Path;

/// The legacy, segment-less overloads. `position` is deliberately absent: the
/// bare name collides with unrelated methods across the workspace, and its one
/// live caller (`emit_leg_pnl`) already moved to `position_in_segment`.
const LEGACY_METHODS: &[&str] = &[
    ".check_order(",
    ".record_fill(",
    ".update_market_price(",
    ".net_lots_for(",
];

/// Production call sites that exist today, and may only DECREASE.
///
/// A file whose count drops must be updated here in the same change; a file
/// that reaches zero must be removed from the list entirely, so the baseline
/// can never quietly outlive the thing it bounds.
const LEGACY_CALL_BASELINE: &[(&str, usize)] = &[
    ("crates/app/src/exit_execution.rs", 3),
    ("crates/app/src/trading_pipeline.rs", 3),
];

/// Everything before the first `#[cfg(test)]` line — this workspace puts its
/// unit tests in a trailing `mod tests`, so that marker is the production edge.
fn production_source(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(at) => &src[..at],
        None => src,
    }
}

fn count_legacy_calls(src: &str) -> usize {
    let prod = production_source(src);
    LEGACY_METHODS
        .iter()
        .map(|needle| prod.matches(needle).count())
        .sum()
}

fn scan_workspace() -> BTreeMap<String, usize> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above crates/app")
        .to_path_buf();
    let mut found = BTreeMap::new();
    for crate_dir in ["crates/app/src", "crates/trading/src"] {
        let mut stack = vec![root.join(crate_dir)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                // The overloads are DEFINED here and delegate to their own
                // `*_in_segment` bodies — counting those would be nonsense.
                if path.ends_with("risk/engine.rs") {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let n = count_legacy_calls(&src);
                if n > 0 {
                    let rel = path
                        .strip_prefix(&root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    found.insert(rel, n);
                }
            }
        }
    }
    found
}

#[test]
fn no_new_segment_less_risk_call_site() {
    let found = scan_workspace();
    let baseline: BTreeMap<&str, usize> = LEGACY_CALL_BASELINE.iter().copied().collect();

    for (file, count) in &found {
        let allowed = baseline.get(file.as_str()).copied().unwrap_or(0);
        assert!(
            *count <= allowed,
            "{file} now makes {count} segment-less RiskEngine call(s); the \
             baseline allows {allowed}.\n\nThese overloads book or read a \
             position on `ExchangeSegment::IdxI` whatever segment the caller \
             actually has, so two instruments sharing a numeric security_id in \
             different segments collapse into one row and their lots NET — a \
             position-limit check that approves an order already in breach, \
             and a daily-loss halt measuring a fabricated number.\n\nUse \
             `check_order_in_segment` / `record_fill_in_segment` / \
             `update_market_price_in_segment` / `net_lots_for_in_segment`. If \
             you genuinely need the bare-sid SUM across segments (only to \
             compare against something itself bare-sid, such as the order \
             runtime's reconcile), use `net_lots_for_any_segment` and say so at \
             the site."
        );
    }
}

#[test]
fn the_baseline_has_no_stale_entry() {
    let found = scan_workspace();
    for (file, allowed) in LEGACY_CALL_BASELINE {
        let actual = found.get(*file).copied().unwrap_or(0);
        assert!(
            actual > 0,
            "{file} is in LEGACY_CALL_BASELINE but makes ZERO segment-less \
             RiskEngine calls. Remove its row — a baseline that outlives what \
             it bounds is a ratchet nobody can trust."
        );
        assert_eq!(
            actual, *allowed,
            "{file} makes {actual} segment-less call(s) but the baseline says \
             {allowed}. If you REMOVED one, lower the baseline in the same \
             change so the ratchet keeps its bite."
        );
    }
}

#[test]
fn the_live_order_runtime_is_fully_segment_aware() {
    let found = scan_workspace();
    assert!(
        !found.contains_key("crates/app/src/order_runtime.rs"),
        "order_runtime.rs is the LIVE risk caller ([order_runtime] enabled = \
         true, paper mode). It was migrated to the composite-key variants on \
         2026-08-29 and must stay there: it is the only one of the three \
         callers a running system actually reaches."
    );
}

#[test]
fn the_scanner_is_not_vacuous() {
    // Negative control: the detector must fire on a real call, and must not
    // fire on the segment-aware form or on code below the test marker.
    assert_eq!(count_legacy_calls("risk.record_fill(1, 2, 3.0, 4);"), 1);
    assert_eq!(
        count_legacy_calls("risk.record_fill_in_segment(1, seg, 2, 3.0, 4);"),
        0,
        "the `_in_segment` form must not be counted — `.record_fill(` requires \
         the closing paren immediately after the name"
    );
    assert_eq!(
        count_legacy_calls("fn f() {}\n#[cfg(test)]\nmod t { risk.check_order(1, 2); }"),
        0,
        "calls below the test marker are not production calls"
    );
    // And the real scan must find something, or every assertion above is
    // passing over an empty set.
    assert!(
        !scan_workspace().is_empty(),
        "the workspace scan found no legacy calls at all — the scanner is \
         reading the wrong tree"
    );
}
