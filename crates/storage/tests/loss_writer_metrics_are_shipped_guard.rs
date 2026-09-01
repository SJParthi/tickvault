//! Guard: every metric the two LOSS-BEARING writers emit must reach CloudWatch.
//!
//! # The gap this closes (adversarial review, 2026-09-01)
//!
//! Three guards already police the metric pipeline, and all three run the
//! same direction — from the EMF selector outward:
//!
//! | guard | asserts | catches an unshipped counter? |
//! |---|---|---|
//! | `dashboard_live_lane_visibility_guard` | selected -> charted or alarmed | no |
//! | `emf_selector_producer_guard` | selected -> has a producer | no |
//! | `loss_counter_visibility_guard` | loss-SHAPED name -> shipped | only if the name ends in a loss suffix |
//!
//! So **produced -> selected was unguarded**, and a new counter could be
//! added, emitted, and reach nobody with an entirely green build. That is
//! not hypothetical: `tv_tick_spill_over_soft_ceiling_total` and
//! `tv_depth_spill_over_soft_cap_total` were added earlier the same day and
//! shipped to nothing. Neither ends in a loss suffix, so the third guard
//! could not see them either.
//!
//! # Why scoped to these two files rather than the workspace
//!
//! A workspace-wide produced -> selected rule would need a large allowlist
//! (plenty of counters are legitimately local-only), and a guard whose first
//! act is a hundred-entry allowlist teaches the reader to add a hundred and
//! first. These two modules are different: they are the ONLY writers that can
//! permanently destroy market data, so a metric of theirs that reaches no
//! operator is a specific, serious defect rather than a stylistic one.
//!
//! Every exemption must carry a written reason. An unexplained gap here is
//! the state that let a spill rail miscalibrate for weeks with nothing to see.
#![allow(clippy::expect_used, clippy::panic)]

/// Metrics these writers emit that deliberately do NOT ship, with the reason.
///
/// Shrink-only in spirit: adding a row is a decision that must be justified
/// in writing, not a way to make the build green.
const DELIBERATELY_LOCAL_ONLY: &[(&str, &str)] = &[
    (
        "tv_tick_volume_saturated_total",
        "UNREACHABLE today, by construction. `saturate_volume_to_i64` narrows a \
     u64 cumulative volume onto the LONG column, and every live source is \
     narrower or equal (`ParsedTick::volume` is u32; Groww and TrueData day \
     volume are i64), so the saturating arm cannot be reached from \
     `TickRow::from_parsed_tick`. It is a boundary guard held for a future \
     u64-wide feed. Shipping it would pay ~$0.30/mo for a series that is \
     structurally incapable of moving — the inverse of the defect this guard \
     exists to catch, and just as wasteful. Ship it in the same change that \
     introduces a u64-wide volume source, not before.",
    ),
    (
        "tv_spill_free_probe_blind_total",
        "CORRECTED 2026-09-01 (adversarial review). The previous text rested \
     this whole exemption on the claim that the floor `deliberately fails OPEN \
     there ... so the write proceeds`. That is FALSE for half the surface. \
     Ticks fail OPEN (the write proceeds and this counter marks it), but DEPTH \
     fails CLOSED — the rows are refused and permanently dropped — and as of \
     this change BOTH tiers also increment it at the soft-ceiling arm, where \
     both refuse. So the counter now means `wrote blind` for one tier and \
     `dropped rows` for the other, and an exemption argued from the wrong half \
     is exactly the stale-claim class this repository has twice recorded \
     manufacturing false findings. \
     STILL HELD LOCAL, on a narrower and honest argument: the LOSS is already \
     shipped and alarmed (`tv_ticks_dropped_total`, \
     `tv_depth_rows_dropped_total`, `tv_ticks_lost_total`), and every blind \
     refusal already writes a coded ERROR naming the cause verbatim — `the \
     free-space probe failed — refusing rather than growing blind` — so the \
     cause IS discoverable today, in logs. What is not available is the \
     ability to ALARM on the cause. \
     THE BLOCKER IS A BUDGET LEVER, NOT A JUDGEMENT CALL. \
     `cloudwatch_app_alarms_wiring::test_emf_metric_selectors_name_count_is_pinned` \
     records the maximal month at ~$123.88 against the automatic \
     STOP_EC2_INSTANCES line at $117.00 and the operator's $125 hard cap — \
     under $1.20 of room — and states in terms that the next addition of ANY \
     size must come with a LEVER rather than a cost note. The lever exists and \
     is already operator-approved in principle: the Quote 10 Elastic IP \
     release (-$3.60/mo), bundled with an instance recreate. Ship this name in \
     the change that takes that lever, or in any change that otherwise returns \
     the maximal month below $117 — not before, because a name that stops the \
     trading box mid-month costs more than the blindness it cures.",
    ),
];

fn read(rel: &str) -> String {
    std::fs::read_to_string(rel).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Every `metrics::counter!("tv_...")` / `gauge!` / `histogram!` literal in a
/// source file.
///
/// Literals only, deliberately. A metric built from a `const` or a variable
/// cannot be resolved by a source scan, and pretending otherwise would make
/// this guard confidently wrong rather than honestly partial — the failure
/// mode this repository has recorded most often.
fn emitted_metric_names(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for macro_name in ["counter!(", "gauge!(", "histogram!("] {
        let mut rest = src;
        while let Some(at) = rest.find(macro_name) {
            rest = &rest[at + macro_name.len()..];
            let trimmed = rest.trim_start();
            let Some(body) = trimmed.strip_prefix('"') else {
                continue;
            };
            if let Some(end) = body.find('"') {
                let name = &body[..end];
                if name.starts_with("tv_") {
                    out.push(name.to_string());
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[test]
fn every_metric_the_loss_writers_emit_reaches_cloudwatch() {
    let selector = read("../../deploy/aws/cloudwatch-agent.json");
    assert!(
        selector.contains("metric_selectors"),
        "the EMF selector moved — this guard is reading the wrong file and \
         would pass vacuously"
    );
    // Non-vacuity: a name we KNOW is shipped must be found by the same
    // substring test the assertions below use. Without this, a selector file
    // that failed to load would make every check trivially pass.
    assert!(
        selector.contains("tv_ticks_dropped_total"),
        "the known-shipped control metric is missing from the selector — the \
         scan is broken, not the code"
    );

    let mut unshipped: Vec<(String, &str)> = Vec::new();
    for file in ["src/tick_persistence.rs", "src/depth_persistence.rs"] {
        let whole = read(file);
        // Production half only. A file's own test module can name a metric
        // it never emits, and a scan satisfied by a string inside a test is
        // not a guard.
        let prod = whole
            .split_once("#[cfg(test)]")
            .map_or(whole.clone(), |(p, _)| p.to_string());
        assert!(
            prod.len() > 1_000,
            "{file}: the production half scanned as {} bytes — the split \
             marker moved and this guard is checking nothing",
            prod.len()
        );
        let names = emitted_metric_names(&prod);
        assert!(
            names.len() >= 5,
            "{file}: found only {} emitted metric name(s). These writers emit \
             many — the extractor is broken",
            names.len()
        );
        for name in names {
            let exempt = DELIBERATELY_LOCAL_ONLY.iter().any(|(n, _)| *n == name);
            if !exempt && !selector.contains(&name) {
                unshipped.push((name, file));
            }
        }
    }

    assert!(
        unshipped.is_empty(),
        "these metrics are emitted by a writer that can PERMANENTLY DESTROY \
         market data, and reach nobody — they are not in the EMF selector, so \
         they never leave the box:\n{}\n\nAdd each to \
         deploy/aws/cloudwatch-agent.json (it costs ~$0.30/mo and, since the \
         selector moved out of user-data.sh.tftpl, ZERO user-data bytes), or \
         add it to DELIBERATELY_LOCAL_ONLY with a written reason. On \
         2026-09-01 two soft-rail counters were added to exactly these files \
         and shipped nowhere; no existing guard could see it, because all \
         three run selector-outward and this is the only one that runs \
         producer-outward.",
        unshipped
            .iter()
            .map(|(n, f)| format!("  {n}  ({f})"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Bite-proof for the extractor.
///
/// The two ways this guard could be vacuous are an extractor that finds
/// nothing (every check trivially passes) and one that finds names the file
/// does not emit. Both are exercised against fixtures, so the proof does not
/// move when the real files do.
#[test]
fn guard_self_test() {
    let src = r#"
        metrics::counter!("tv_real_one_total").increment(1);
        metrics::gauge!("tv_real_gauge").set(1.0);
        metrics::histogram!("tv_real_hist").record(1.0);
        metrics::counter!(SOME_CONST).increment(1);
        let s = "tv_not_a_metric_just_a_string";
        counter!("not_prefixed_total").increment(1);
    "#;
    let found = emitted_metric_names(src);
    assert_eq!(
        found,
        vec![
            "tv_real_gauge".to_string(),
            "tv_real_hist".to_string(),
            "tv_real_one_total".to_string()
        ],
        "the extractor must find exactly the three tv_-prefixed literals: \
         not the const-built one (unresolvable by a source scan, and claiming \
         otherwise would make this guard confidently wrong), not the bare \
         string, and not the unprefixed name"
    );
    assert!(
        emitted_metric_names("no metrics here at all").is_empty(),
        "a file with no metrics must yield none, not a phantom"
    );
}
