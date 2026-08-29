//! A NaN gauge cannot publish, so it must not be paid for or charted.
//!
//! # The defect, measured
//!
//! On 2026-08-29 a `cloudwatch list-metrics` sweep of `Tickvault/Prod`
//! returned `[]` for `tv_subsystem_memory_estimated_bytes` — the series has
//! never existed in the account — while the name sat in the EMF selector
//! (~$0.30/mo) and drew a line on the operator dashboard's "Resource
//! headroom" panel.
//!
//! It is not a wiring slip. Every one of the six per-component gauges is
//! initialised to `f64::NAN` by design (L124), and the ONLY thing that ever
//! writes a real number is a closure handed to
//! `SubsystemMemorySampler::register_source`. That function has ZERO
//! production call sites: the two it once had died with the
//! TickStorage/PrevDayCache sweep on 2026-07-19, which is the same reason the
//! `tick_storage` component was dropped from the allowlist that day. NaN is
//! correctly dropped by the CloudWatch agent, so the metric was
//! *structurally incapable* of drawing a point.
//!
//! # Why the existing guards could not catch it
//!
//! `emf_selector_producer_guard` proves the NAME exists in source — and it
//! does, the `gauge!(...)` handle is built at boot. `dashboard_live_lane_
//! visibility_guard` proves selected and charted agree with each other — and
//! they did, both halves lined up. Neither asks whether anything ever emits a
//! VALUE.
//!
//! That is the same class this session fixed twice already in the counter
//! world: building a `metrics` handle registers a key with the recorder, it
//! does not emit a sample. Here it arrives in its gauge form, where the tell
//! is different and worse — a counter at least publishes once someone
//! increments it, whereas a gauge pinned at NaN publishes never, no matter
//! how long the process runs.
//!
//! # The invariant
//!
//! The gauge may be EMF-selected (and therefore billed and charted) only when
//! at least one production source is registered. Wire a source and the name
//! may come back, with its own dated cost note; leave it sourceless and it
//! stays off the paid surface. Enforced in BOTH directions so neither half
//! can drift.
//!
//! An empty line on a panel titled "Resource headroom" is not a neutral
//! absence — it reads as "memory is fine", which is the false-OK this
//! repository has spent three separate incidents learning to refuse.

const SAMPLER: &str = include_str!("../src/subsystem_memory.rs");
const GAUGE: &str = "tv_subsystem_memory_estimated_bytes";

fn repo_root() -> std::path::PathBuf {
    // tests run from the crate dir; the workspace root is two levels up.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root must exist")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Production `register_source` call sites: every mention outside this
/// module's own definition and outside its `#[cfg(test)]` region.
///
/// The sampler module is excluded wholesale because it contains BOTH the
/// definition and a test module that exercises it — counting either would
/// let the guard pass while nothing in production registers anything.
/// A line that genuinely CALLS `register_source`, not one that mentions it.
fn is_real_call(line: &str) -> bool {
    if !line.contains(".register_source(") {
        return false;
    }
    let trimmed = line.trim_start();
    !(trimmed.starts_with("//") || trimmed.starts_with('*'))
}

fn production_register_source_sites() -> Vec<String> {
    let mut sites = Vec::new();
    for dir in ["crates/app/src", "crates/core/src", "crates/trading/src"] {
        let root = repo_root().join(dir);
        let mut stack = vec![root];
        while let Some(path) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                if p.file_name().is_some_and(|n| n == "subsystem_memory.rs") {
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(&p) else {
                    continue;
                };
                // Strip the test region so a test helper cannot satisfy this,
                // and ignore comment lines so a doc comment that merely NAMES
                // the call cannot silently switch this guard off — the exact
                // false-positive shape that would make it worthless.
                let production = body.split("\n#[cfg(test)]").next().unwrap_or(&body);
                if production.lines().any(is_real_call) {
                    sites.push(p.display().to_string());
                }
            }
        }
    }
    sites
}

fn selector_contains_gauge() -> bool {
    read("deploy/aws/cloudwatch-agent.json").contains(GAUGE)
}

fn dashboard_charts_gauge() -> bool {
    // The quoted form is what CloudWatch actually renders; a prose mention in
    // a `#` comment is not a line on a chart. This mirrors the matching rule
    // in `dashboard_live_lane_visibility_guard`.
    read("deploy/aws/terraform/dashboard.tf").contains(&format!("\"{GAUGE}\""))
}

/// The whole rule, as a pure function so every branch is testable without
/// mutating the repository under the test.
fn verdict(has_source: bool, selected: bool, charted: bool) -> Result<(), String> {
    if has_source {
        // A source exists, so the gauge can publish. Selecting and charting it
        // is then legitimate — this guard has nothing to say.
        return Ok(());
    }
    if selected {
        return Err(format!(
            "{GAUGE} is in the CloudWatch agent's EMF selector (~$0.30/mo) but \
             no production code calls SubsystemMemorySampler::register_source, \
             so every component gauge stays f64::NAN and the agent drops every \
             sample. The series can never exist. Either wire a real source \
             closure in the same change, or leave the name out of the selector."
        ));
    }
    if charted {
        return Err(format!(
            "{GAUGE} is charted on the operator dashboard but no production \
             code calls register_source, so the line is structurally incapable \
             of drawing a point. An always-empty line on a resource panel \
             reads as 'memory is fine' — strictly worse than no line at all."
        ));
    }
    Ok(())
}

#[test]
fn a_sourceless_subsystem_memory_gauge_is_never_billed_or_charted() {
    let has_source = !production_register_source_sites().is_empty();
    if let Err(why) = verdict(
        has_source,
        selector_contains_gauge(),
        dashboard_charts_gauge(),
    ) {
        panic!("{why}");
    }
}

#[test]
fn the_rule_holds_in_every_direction() {
    // Sourceless: neither surface may carry it.
    assert!(verdict(false, false, false).is_ok());
    assert!(
        verdict(false, true, false).is_err(),
        "billed but sourceless"
    );
    assert!(
        verdict(false, false, true).is_err(),
        "charted but sourceless"
    );
    assert!(verdict(false, true, true).is_err());

    // Sourced: the gauge can publish, so both surfaces are legitimate again
    // and this guard must step aside rather than block the re-add.
    assert!(verdict(true, false, false).is_ok());
    assert!(verdict(true, true, false).is_ok());
    assert!(verdict(true, false, true).is_ok());
    assert!(
        verdict(true, true, true).is_ok(),
        "wiring a real source must UNBLOCK selecting and charting the gauge — \
         a guard that can only ever say no would be an obstacle, not a rule"
    );
}

#[test]
fn the_gauge_really_is_nan_until_a_source_writes_it() {
    // Non-vacuity, and the load-bearing premise of the test above: if the
    // gauges were seeded with a real number at construction the metric would
    // publish on its own and none of this would apply.
    assert!(
        SAMPLER.contains("f64::NAN"),
        "subsystem_memory.rs must still initialise its component gauges to \
         NaN — if that changed, this guard's premise is stale and the \
         selector decision must be revisited rather than left as-is"
    );
    assert!(
        SAMPLER.contains("pub fn register_source"),
        "register_source must still be the only path that writes a real \
         value; if the write path moved, re-derive what this guard scans for"
    );
}

#[test]
fn the_call_site_scan_cannot_pass_vacuously() {
    // Bite-proof the scanner itself: it must find a real call in a file that
    // has one, and must not be fooled by a mention inside a test region.
    let production_only = "fn wire() { sampler.register_source(\"registry\", || None); }";
    assert!(
        production_only
            .split("\n#[cfg(test)]")
            .next()
            .unwrap_or(production_only)
            .lines()
            .any(is_real_call),
        "scanner must detect a genuine production call site"
    );

    let test_only = "fn wire() {}\n#[cfg(test)]\nmod t { fn x() { s.register_source(\"registry\", || None); } }";
    assert!(
        !test_only
            .split("\n#[cfg(test)]")
            .next()
            .unwrap_or(test_only)
            .lines()
            .any(is_real_call),
        "scanner must NOT count a call site that lives inside a test module"
    );

    // A doc comment naming the call is NOT a call. Without this the guard
    // could be switched off by prose, which is how a guard becomes decorative.
    assert!(
        !"/// see sampler.register_source(\"registry\", ...) for the contract"
            .lines()
            .any(is_real_call),
        "scanner must NOT count a comment that merely mentions the call"
    );
}
