//! Every metric name we SHIP to CloudWatch must have a producer in production
//! source.
//!
//! Found 2026-08-22 by diffing the EMF selector against every `tv_*` string
//! literal in `crates/*/src`. Exactly one name had no producer:
//! `tv_cadence_option_mark_unresolved_total`, deleted with the Groww feed in
//! `1e3c9533` while its name stayed in BOTH selector copies.
//!
//! It carried no alarm, so nothing was permanently green -- but the cost was
//! real and had two halves:
//!
//! 1. **A name that can never appear occupied the byte budget.** The rendered
//!    EC2 user-data sits under a HARD AWS limit of 16,384 bytes with a 512-byte
//!    guard margin, and headroom was **19 bytes**. That is why
//!    `tv_tick_spill_replayed_bytes_total` was excluded the same week -- it
//!    needed 35 and came out 16 over. A dead 39-byte entry was holding budget
//!    that a live counter was refused for.
//!
//! 2. **Its live replacement was not shipping at all.** The commit that "gave
//!    the refused option marks an operator surface" renamed the counter to
//!    `tv_chain_mark_refused_total` and did not move the selector entry, so the
//!    new counter reached `/metrics` and never reached CloudWatch. That counter
//!    is a REFUSAL signal -- when it fires, option legs cannot be marked and, in
//!    the emit site's own words, "every option on it is silently unpriced".
//!    The operator surface did not reach the operator.
//!
//! Both halves are the same defect in opposite directions, and
//! `EMF-METRIC-SELECTOR-NOTES.md` already names it: "a stale EXCLUSION hides a
//! live producer's loss signal just as effectively as a dead INCLUSION
//! advertises coverage that cannot exist." This test is the mechanical half of
//! that sentence for the INCLUSION direction.
//!
//! Deliberately NOT asserted here: the reverse direction (a producer that ought
//! to ship but does not). That is a judgement about which signals earn their
//! cost, it is argued case by case in the notes file, and a test that demanded
//! every counter ship would fail on the success/volume counters the notes
//! exclude ON PURPOSE. A guard whose first act is a false positive gets
//! allowlisted within a week.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/storage -> crates -> repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Pull the `^(a|b|c)$` selector body out of a file and split it into names.
fn selector_names(body: &str, what: &str) -> Vec<String> {
    let start = body
        .find("\"^(tv_")
        .unwrap_or_else(|| panic!("{what} carries no EMF metric selector"));
    let rest = &body[start + 1..];
    let end = rest
        .find('"')
        .unwrap_or_else(|| panic!("{what}'s selector is unterminated"));
    let inner = &rest[..end];
    let inner = inner
        .trim_start_matches("^(")
        .trim_end_matches(")$")
        .trim_end_matches('$')
        .trim_end_matches(')');
    let names: Vec<String> = inner.split('|').map(|s| s.trim().to_string()).collect();
    assert!(
        names.len() > 40,
        "{what}: parsed only {} selector names -- the parser is wrong, not the file",
        names.len()
    );
    names
}

/// Every `"tv_*"` literal in PRODUCTION source (never tests).
fn production_metric_literals() -> Vec<String> {
    let mut out = Vec::new();
    let crates_dir = repo_root().join("crates");
    let mut stack = vec![crates_dir];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                // Only descend into a crate's `src`; skip `tests`, `benches`,
                // `target`, so a name that exists ONLY in a test cannot count
                // as a producer.
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name == "target" || name == "tests" || name == "benches" || name == "fuzz" {
                    continue;
                }
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&p) else {
                continue;
            };
            let mut rest = body.as_str();
            while let Some(i) = rest.find("\"tv_") {
                let after = &rest[i + 1..];
                if let Some(j) = after.find('"') {
                    let cand = &after[..j];
                    if cand
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    {
                        out.push(cand.to_string());
                    }
                    rest = &after[j..];
                } else {
                    break;
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn every_shipped_metric_name_has_a_production_producer() {
    let producers = production_metric_literals();
    assert!(
        producers.len() > 100,
        "scanner found only {} production metric literals -- it is broken, and a \
         broken scanner would pass this test vacuously",
        producers.len()
    );

    let mut orphans = Vec::new();
    for name in selector_names(
        &read("deploy/aws/cloudwatch-agent.json"),
        "cloudwatch-agent.json",
    ) {
        if !producers.contains(&name) {
            orphans.push(name);
        }
    }

    assert!(
        orphans.is_empty(),
        "these metric names are SHIPPED to CloudWatch but nothing in production \
         source emits them:\n  {}\n\n\
         A name with no producer can never appear. It is not free: the rendered \
         EC2 user-data is under a HARD 16,384-byte AWS limit, and a dead entry \
         holds budget that a live counter gets refused for -- which is exactly \
         what happened to tv_tick_spill_replayed_bytes_total.\n\
         Either restore the producer, or remove the name from BOTH selector \
         copy (cloudwatch-agent.json, the file user-data installs after the clone) and record why in \
         deploy/aws/EMF-METRIC-SELECTOR-NOTES.md.",
        orphans.join("\n  ")
    );
}

#[test]
fn the_refusal_counter_that_lost_its_shipping_is_shipped() {
    // Regression pin for the specific defect. Named rather than left implicit,
    // because the general test above would also pass if someone deleted this
    // name instead of restoring it.
    let tftpl = read("deploy/aws/cloudwatch-agent.json");
    let agent = read("deploy/aws/cloudwatch-agent.json");
    for (what, body) in [
        ("cloudwatch-agent.json (2nd read)", &tftpl),
        ("cloudwatch-agent.json", &agent),
    ] {
        assert!(
            body.contains("tv_chain_mark_refused_total"),
            "{what} no longer ships tv_chain_mark_refused_total -- when it fires, \
             option legs cannot be marked and every option on that underlying is \
             silently unpriced. It reached /metrics but not CloudWatch for the \
             whole time its predecessor's name sat in the selector instead."
        );
        assert!(
            !body.contains("tv_cadence_option_mark_unresolved_total"),
            "{what} still carries tv_cadence_option_mark_unresolved_total, whose \
             producer was deleted with the Groww feed in 1e3c9533."
        );
    }
}

#[test]
fn guard_self_test() {
    // The parser must find a real list, and the orphan check must be able to
    // FAIL -- a scanner that silently returns nothing would pass every run.
    let names = selector_names(
        &read("deploy/aws/cloudwatch-agent.json"),
        "cloudwatch-agent.json",
    );
    assert!(
        names.iter().all(|n| n.starts_with("tv_")),
        "selector parse produced a non-metric token: {names:?}"
    );
    let producers = production_metric_literals();
    assert!(
        !producers.contains(&"tv_definitely_not_a_real_metric_name".to_string()),
        "scanner claims a name that does not exist -- it would never report an orphan"
    );
    assert!(
        producers.contains(&"tv_chain_mark_refused_total".to_string()),
        "scanner cannot see a literal that is demonstrably in production source"
    );
}
