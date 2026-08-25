//! Ratchet: the two live-lane counter alarms must not be dead on arrival.
//!
//! ## The defect this exists to prevent (2026-08-14)
//!
//! The CloudWatch agent computes a counter's alarm value as the DELTA between
//! consecutive samples. For a series it has never seen there is no previous
//! sample, so it DROPS the first one as the baseline.
//!
//! Both live-lane counter alarms watch counters that increment ONLY on the bad
//! event:
//!
//! | Counter | Increments when |
//! |---|---|
//! | `tv_dhan_ws_park_total` | a socket parks permanently — at most once per connection per session |
//! | `tv_ticks_dropped_total` | buffered rows are discarded on a flush failure |
//!
//! So without a zero published up front, the FIRST park and the FIRST drop
//! episode ARE the dropped baseline samples: they publish no datapoint at all,
//! and `tv-<env>-dhan-socket-parked` / `tv-<env>-ticks-dropped` (both threshold
//! 1 over a single 300s period) never fire for them. The alarms would be dead
//! precisely for the single-episode case, which is the normal shape of both
//! incidents.
//!
//! Those two alarms shipped that way. The repo had already learned this lesson
//! twice — `seal-drop-alarm.tf` documents it at length and `main.rs` carries
//! the seal-writer and order-side registrations — and the live-lane PR did not
//! apply it.
//!
//! ## Why the label enumeration is load-bearing
//!
//! The agent baselines per Prometheus SERIES (one per label combination), and
//! the EMF processor folds the labels to `{host}` afterwards by SUMMING the
//! per-series deltas. A series left unregistered therefore contributes nothing
//! to that sum on the sample where it is born — so registering one endpoint,
//! or one feed, and not the rest leaves exactly those combinations invisible.
//! That is why the park registration iterates BOTH `DhanEndpointType::ALL` and
//! `ParkReason::ALL` rather than picking a representative pair.
//!
//! ## What this file learned about guarding, the hard way
//!
//! The first version of this guard was itself audited and **one of its five
//! tests was proven vacuous**: it sliced from `PoolSupervisor::new` to
//! `PoolSupervisor::admit` and looked for `PARK_METRIC` anywhere in between,
//! so extracting the loop into a helper placed between those two functions —
//! and deleting the call from `new` — left both alarms dead and every test
//! green. Three more tests were proven BRITTLE: they pinned rustfmt's trailing
//! comma, the loop's variable NAMES (`endpoint`/`reason`), and terraform fmt's
//! column alignment, so cosmetic edits with zero behavioural change turned
//! them red.
//!
//! Both failure modes are corrected below, and the rules that came out of it
//! are worth stating because they generalise:
//!
//! - Scope an assertion to the **brace-matched body** of the function that
//!   must contain the code. A slice between two landmarks proves nothing about
//!   what sits inside either one.
//! - Assert on **semantic tokens** (`ParkReason::ALL`, `.increment(0)`), never
//!   on formatting or identifier choices a refactor may legitimately change.
//! - When the guard cannot follow a refactor, fail with a message that says
//!   what to do about it. A false positive that reads "you moved this, update
//!   the guard" is recoverable; a false negative is not.

use std::fs;
use std::path::{Path, PathBuf};

/// Everything above the `#[cfg(test)] mod tests` boundary.
///
/// Deliberately NOT "everything above the first `#[cfg(test)]`". That was the
/// original implementation and it was wrong in a way that mattered:
/// `tick_persistence.rs` carries a `#[cfg(test)]` helper *inside a production
/// impl* at line ~631, so truncating there left the guard reading 12 KB of a
/// 68 KB file — under a fifth of it. Moving the registration below that line,
/// an edit with no behavioural meaning, would have passed while the baseline
/// was gone.
fn production_text(path: &Path) -> String {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let body = match raw.find("mod tests") {
        Some(idx) => &raw[..idx],
        None => &raw[..],
    };
    strip_line_comments(body)
}

/// Drop whole-line `//` comments so a commented-out registration cannot
/// satisfy a `contains` check.
///
/// Only lines whose FIRST non-whitespace is `//` are dropped — not everything
/// after any `//` on a line. The looser form truncated at the `//` inside a
/// `"http://…"` string literal, silently deleting real code from the text the
/// assertions run against.
fn strip_line_comments(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The brace-matched body of the first item whose signature contains `needle`.
///
/// This is the whole correction to the vacuity described in the module docs:
/// an assertion scoped to a real function body cannot be satisfied by code
/// that merely sits nearby.
fn fn_body<'a>(text: &'a str, needle: &str, path_hint: &str) -> &'a str {
    let start = text.find(needle).unwrap_or_else(|| {
        panic!("{path_hint}: `{needle}` not found — this guard needs updating to match the rename")
    });
    let open = text[start..]
        .find('{')
        .unwrap_or_else(|| panic!("{path_hint}: no opening brace after `{needle}`"))
        + start;

    let bytes = text.as_bytes();
    let mut depth = 0usize;
    for (offset, byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("{path_hint}: unbalanced braces after `{needle}`")
}

/// Whitespace-compact, so assertions survive rustfmt line breaks, trailing
/// commas and column alignment.
fn compact(text: &str) -> String {
    text.split_whitespace().collect::<String>()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/app -> repo root")
        .to_path_buf()
}

#[test]
fn park_counter_baseline_is_inside_the_constructor_and_covers_every_label() {
    let src = repo_root().join("crates/core/src/websocket/pool_supervisor.rs");
    let text = production_text(&src);
    // Scoped to the constructor's own braces: a helper defined elsewhere in
    // the file, or code sitting between `new` and `admit`, cannot satisfy this.
    let ctor = compact(fn_body(
        &text,
        "pub fn new() -> Self {",
        "pool_supervisor.rs",
    ));

    for (token, why) in [
        (
            "DhanEndpointType::ALL",
            "an endpoint that is not pre-registered publishes NOTHING on its \
             first park — tv-<env>-dhan-socket-parked is dead for that endpoint",
        ),
        (
            "ParkReason::ALL",
            "same failure sliced by reason: an unregistered reason's first park \
             is eaten as the CloudWatch delta baseline",
        ),
        (
            "PARK_METRIC",
            "the baseline no longer names the park counter at all",
        ),
        (
            "\"endpoint\"=>",
            "registering the metric without its endpoint label baselines a \
             different series than the one that actually increments",
        ),
        (
            "\"reason\"=>",
            "registering the metric without its reason label baselines a \
             different series than the one that actually increments",
        ),
        (
            ".increment(0)",
            "the loop is present but publishes nothing — no sample, no baseline",
        ),
    ] {
        assert!(
            ctor.contains(token),
            "PoolSupervisor::new no longer contains `{token}`.\n{why}.\n\
             If this moved into a helper, CALL it from new() and re-point this \
             guard at the helper's body — do not delete the assertion."
        );
    }
}

#[test]
fn ticks_dropped_baseline_is_reachable_from_both_constructors() {
    let src = repo_root().join("crates/storage/src/tick_persistence.rs");
    let text = production_text(&src);

    // The registration is deliberately factored out, because `for_test` is
    // `pub` and cannot be cfg-gated (crates/app's tests call it across the
    // crate boundary). Both constructors must reach it, or a writer exists
    // that can drop ticks having never baselined its series.
    let helper = compact(fn_body(
        &text,
        "fn register_drop_baseline(",
        "tick_persistence.rs",
    ));
    assert!(
        helper
            .contains("counter!(\"tv_ticks_dropped_total\",\"feed\"=>feed.as_str()).increment(0)"),
        "register_drop_baseline no longer publishes a zero on the labelled \
         series.\n tv_ticks_dropped_total increments ONLY on real data loss, so \
         without a zero first the session's FIRST drop episode is consumed as \
         the CloudWatch delta baseline and tv-<env>-ticks-dropped never fires \
         for it. The `feed` label is required — the agent baselines per \
         labelled series."
    );

    for ctor in [
        "pub fn new(config: &QuestDbConfig, feed: Feed) -> Self {",
        "pub fn for_test(feed: Feed) -> Self {",
    ] {
        let body = compact(fn_body(&text, ctor, "tick_persistence.rs"));
        assert!(
            body.contains("register_drop_baseline(feed)"),
            "`{ctor}` no longer registers the drop baseline.\n\
             Every constructor must, not just the production one: for_test is \
             pub and is called from crates/app's tests, where cfg(test) does \
             not reach, so a writer built that way would drop ticks with no \
             baselined series behind it."
        );
    }
}

#[test]
fn the_two_alarms_watch_the_emf_metrics_directly_not_a_derived_copy() {
    // Both counters are in the EMF allowlist, and the agent's single
    // metric_declaration folds Prometheus labels to `{host}`. So the alarms
    // read the published metric directly. A round-2 edit on 2026-08-14 briefly
    // shipped log metric filters deriving `*_host_total` copies on the false
    // premise that `{host}` carried no datapoints; that duplicated two paid
    // series and broke the seal-drop house rule. This pins the correction.
    //
    // Compacted, so `terraform fmt` re-aligning the block when a longer
    // attribute name is added elsewhere cannot turn this red.
    let tf = repo_root().join("deploy/aws/terraform/live-lane-alarms.tf");
    let text = fs::read_to_string(&tf).expect("read live-lane-alarms.tf");
    let compacted = compact(&text);

    for metric in ["tv_dhan_ws_park_total", "tv_ticks_dropped_total"] {
        assert!(
            compacted.contains(&format!("metric_name=\"{metric}\"")),
            "the alarm on {metric} no longer watches the EMF metric directly"
        );
    }
    assert!(
        !text.contains("_host_total"),
        "a derived `*_host_total` series is back in live-lane-alarms.tf.\n\
         Both source metrics are EMF-allowlisted and the agent folds labels to \
         {{host}}, so a derived copy is a second billed series for data already \
         published — the case seal-drop-alarm.tf explicitly rules out."
    );
    assert!(
        !text.contains("aws_cloudwatch_log_metric_filter"),
        "a log metric filter is back in live-lane-alarms.tf — see above"
    );
}

#[test]
fn the_emf_declaration_still_folds_labels_to_host() {
    // The whole `{host}` reasoning above depends on the agent declaring ONE
    // dimension set. If a future change adds a second declaration (as the
    // retired 2026-07-06 per-feed one did), a `{host}` alarm may stop matching
    // and this guard should force that to be thought about rather than
    // discovered from a quiet alarm.
    let root = repo_root();
    // 2026-08-25: ONE file, not two. `user-data.sh.tftpl` used to embed a
    // byte-identical copy of the agent config, and that ~1.6 KB duplicate
    // pinned the template at exactly its 15,872-byte EC2 budget with zero
    // bytes free. It now writes a minimal host-only fallback and copies this
    // file into place after the Step 5 clone, so this IS the config the box
    // loads. That the copy still happens is pinned by
    // `cw_agent_selector_lockstep_guard.rs`.
    for rel in ["deploy/aws/cloudwatch-agent.json"] {
        let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        let declarations = text.matches("\"dimensions\":").count();
        assert_eq!(
            declarations, 1,
            "{rel} no longer declares exactly one EMF dimension set.\n\
             live-lane-alarms.tf reasons that labelled metrics FOLD to {{host}} \
             because there is exactly one `\"dimensions\": [[\"host\"]]` \
             declaration. Adding another changes which dimension sets are \
             published, and the alarms that watch {{host}} must be re-checked \
             against it."
        );
        assert!(
            compact(&text).contains("\"dimensions\":[[\"host\"]]"),
            "{rel} no longer declares the host-only EMF dimension set that the \
             live-lane alarms depend on"
        );
    }
}

#[test]
fn both_counters_are_in_the_emf_allowlist_or_the_alarms_watch_nothing() {
    // Baselining a series the agent never ships is pointless: the alarms would
    // be dead for a completely different reason. The allowlist lives in two
    // files that must agree — the committed config and the terraform template
    // that actually lands on the box.
    let root = repo_root();
    // 2026-08-25: ONE file, not two. `user-data.sh.tftpl` used to embed a
    // byte-identical copy of the agent config, and that ~1.6 KB duplicate
    // pinned the template at exactly its 15,872-byte EC2 budget with zero
    // bytes free. It now writes a minimal host-only fallback and copies this
    // file into place after the Step 5 clone, so this IS the config the box
    // loads. That the copy still happens is pinned by
    // `cw_agent_selector_lockstep_guard.rs`.
    for rel in ["deploy/aws/cloudwatch-agent.json"] {
        let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        for metric in ["tv_dhan_ws_park_total", "tv_ticks_dropped_total"] {
            assert!(
                text.contains(metric),
                "{rel} does not select {metric} for EMF publication.\n\
                 An un-shipped metric cannot alarm no matter how carefully its \
                 baseline is registered."
            );
        }
    }
}

/// The guard's own helpers, tested — because a guard whose text extraction is
/// broken reports on a file it never really read. That is precisely how the
/// first version of this file went vacuous.
#[cfg(test)]
mod self_tests {
    use super::{compact, fn_body, strip_line_comments};

    const SAMPLE: &str = r#"
fn decoy() { PARK_METRIC; }
pub fn new() -> Self {
    for e in All { counter!(PARK_METRIC).increment(0); }
    Self { x: 1 }
}
pub fn admit() { PARK_METRIC; }
"#;

    #[test]
    fn fn_body_stops_at_the_matching_brace_not_the_next_function() {
        let body = fn_body(SAMPLE, "pub fn new() -> Self {", "sample");
        assert!(body.contains("increment(0)"));
        // The nested `Self { x: 1 }` must not end the body early...
        assert!(body.contains("Self { x: 1 }"));
        // ...and neither the decoy above nor `admit` below may be included.
        assert!(!body.contains("fn decoy"));
        assert!(!body.contains("fn admit"));
    }

    #[test]
    fn fn_body_would_catch_the_extract_to_helper_regression() {
        // The exact mutation that defeated the original guard: the loop moves
        // into a helper sitting between `new` and `admit`, and `new` no longer
        // calls it. A landmark-to-landmark slice still sees PARK_METRIC; a
        // brace-matched body does not.
        const MUTATED: &str = r#"
pub fn new() -> Self { Self { x: 1 } }
fn pre_register() { counter!(PARK_METRIC).increment(0); }
pub fn admit() {}
"#;
        let body = fn_body(MUTATED, "pub fn new() -> Self {", "sample");
        assert!(
            !body.contains("PARK_METRIC"),
            "the brace-matched body must NOT see a helper defined after it — \
             this is the vacuity the rewrite exists to close"
        );
    }

    #[test]
    fn strip_line_comments_keeps_urls_inside_string_literals() {
        let src = "let u = \"http://host/exec\";\n// a real comment\ncode();";
        let stripped = strip_line_comments(src);
        assert!(
            stripped.contains("http://host/exec"),
            "truncating at any `//` deletes real code that contains a URL"
        );
        assert!(!stripped.contains("a real comment"));
        assert!(stripped.contains("code();"));
    }

    #[test]
    fn compact_is_insensitive_to_formatting_choices() {
        assert_eq!(
            compact("counter!(\n  M,\n  \"k\" => v,\n)"),
            compact("counter!(M, \"k\" => v,)")
        );
    }
}
