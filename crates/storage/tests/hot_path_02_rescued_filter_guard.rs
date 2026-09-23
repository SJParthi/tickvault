//! HOT-PATH-02 rescued-arm pager exclusion guard
//! (`dhan-rest-only-noise-lock-2026-07-14.md` §2.3x, 2026-09-23).
//!
//! A tick or depth flush that failed but whose rows were RESCUED to the spill
//! file is a degrade: the rows are on disk and re-ingestable. It paged the
//! operator anyway, because `tv-<env>-errcode-hot-path-02` matched every
//! HOT-PATH-02 ERROR. The rescued arms now carry
//! `source = RESCUED_TO_SPILL_SOURCE` and the filter excludes that source.
//!
//! This guard pins BOTH halves, because either alone is a silent failure:
//! * a rescued arm that loses its tag pages again (noise), and
//! * a LOSS arm that gains the tag stops paging (a real loss goes silent) —
//!   the expensive direction, and the one this guard exists for.
//!
//! It also pins the filter literal to the Rust const, and pins the
//! `NOT EXISTS ||` shape that keeps untagged emitters (most of them, in files
//! outside storage) paging whatever CloudWatch does with `!=` on a missing
//! field.

#![cfg(test)]

use std::path::PathBuf;

use tickvault_storage::tick_persistence::RESCUED_TO_SPILL_SOURCE;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root") // APPROVED: test
        .to_path_buf()
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel))
        .unwrap_or_else(|e| panic!("read {rel} failed: {e}"))
}

/// Production region only — everything above the first COLUMN-0
/// `#[cfg(test)]` (the test module). Item-level `#[cfg(test)]` attributes and
/// doc comments that merely mention it are indented and must not truncate the
/// scan — the first version of this cut did, and found zero arms.
fn production(src: &str) -> &str {
    src.find("\n#[cfg(test)]").map_or(src, |at| &src[..at])
}

/// Every `error!(` invocation body in `src`, bounded by paren depth with
/// string literals skipped (messages contain parentheses).
fn error_macro_bodies(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = src[search..].find("error!(") {
        let start = search + rel + "error!(".len();
        let mut depth = 1usize;
        let mut i = start;
        let mut in_str = false;
        while i < bytes.len() && depth > 0 {
            let c = bytes[i];
            if in_str {
                if c == b'\\' {
                    i += 1;
                } else if c == b'"' {
                    in_str = false;
                }
            } else if c == b'"' {
                in_str = true;
            } else if c == b'(' {
                depth += 1;
            } else if c == b')' {
                depth -= 1;
            }
            i += 1;
        }
        out.push(src[start..i.saturating_sub(1)].to_string());
        search = i;
    }
    out
}

/// A body is a HOT-PATH-02 emit.
fn is_hot_path_02(body: &str) -> bool {
    body.contains("HotPath02WriterQueueDrop")
}

/// The arm reports that rows were rescued (on disk), not lost.
fn reports_rescue(body: &str) -> bool {
    (body.contains("RESCUED") || body.contains("rescued = true"))
        && !body.contains("rescued = false")
        && !body.contains("permanently")
}

fn carries_rescued_tag(body: &str) -> bool {
    body.contains("source = RESCUED_TO_SPILL_SOURCE")
        || body.contains("source = crate::tick_persistence::RESCUED_TO_SPILL_SOURCE")
}

/// (rescued arms missing the tag, loss arms wrongly carrying it, rescued-arm count)
fn tag_offences(src: &str) -> (Vec<String>, Vec<String>, usize) {
    let mut untagged_rescues = Vec::new();
    let mut tagged_losses = Vec::new();
    let mut rescued_arms = 0;
    for body in error_macro_bodies(production(src)) {
        if !is_hot_path_02(&body) {
            continue;
        }
        let first_line = body.trim().lines().next().unwrap_or("").to_string();
        let snippet: String = body.chars().take(240).collect();
        if reports_rescue(&body) {
            rescued_arms += 1;
            if !carries_rescued_tag(&body) {
                untagged_rescues.push(snippet);
            }
        } else if carries_rescued_tag(&body) {
            tagged_losses.push(format!("{first_line} … {snippet}"));
        }
    }
    (untagged_rescues, tagged_losses, rescued_arms)
}

#[test]
fn rescued_to_spill_source_literal_is_pinned() {
    assert_eq!(RESCUED_TO_SPILL_SOURCE, "rescued_to_spill");
}

#[test]
fn every_rescued_hot_path_02_arm_is_tagged_and_no_loss_arm_is() {
    let mut total_rescued = 0;
    for file in [
        "crates/storage/src/tick_persistence.rs",
        "crates/storage/src/depth_persistence.rs",
    ] {
        let (untagged, tagged_losses, rescued) = tag_offences(&read(file));
        total_rescued += rescued;
        assert!(
            untagged.is_empty(),
            "{file}: HOT-PATH-02 arms that report a RESCUE but lack \
             `source = RESCUED_TO_SPILL_SOURCE` — they will page the operator for rows \
             that are safely on disk: {untagged:#?}"
        );
        assert!(
            tagged_losses.is_empty(),
            "{file}: HOT-PATH-02 arms that report a LOSS carry the rescued tag — the \
             pager filter excludes them, so a real loss would go SILENT: {tagged_losses:#?}"
        );
    }
    // Anti-vacuity: 2 tick arms + 4 depth arms (sync, offload thread, the
    // caller-side summary, and the no-connection arm).
    assert!(
        total_rescued >= 6,
        "expected at least 6 rescued HOT-PATH-02 arms, found {total_rescued} — the \
         extractor or the wording changed"
    );
}

#[test]
fn the_pager_filter_excludes_exactly_the_rescued_source_and_nothing_else() {
    let tf = read("deploy/aws/terraform/error-code-alarms.tf");
    let at = tf
        .find("resource \"aws_cloudwatch_log_metric_filter\" \"hot_path_02\"")
        .expect("hot_path_02 filter resource"); // APPROVED: test
    let end = tf[at..].find("\n}").map_or(tf.len(), |r| at + r);
    let block = &tf[at..end];
    let pattern_line = block
        .lines()
        .find(|l| l.trim_start().starts_with("pattern"))
        .expect("pattern attribute"); // APPROVED: test
    for required in [
        r#"$.code = \"HOT-PATH-02\""#,
        r#"$.level = \"ERROR\""#,
        "$.source NOT EXISTS ||",
    ] {
        assert!(
            pattern_line.contains(required),
            "hot_path_02 pattern lost `{required}`: {pattern_line}"
        );
    }
    let exclusion = format!(r#"$.source != \"{RESCUED_TO_SPILL_SOURCE}\""#);
    assert!(
        pattern_line.contains(&exclusion),
        "hot_path_02 pattern must exclude exactly the Rust const's source \
         `{RESCUED_TO_SPILL_SOURCE}`: {pattern_line}"
    );
    assert_eq!(
        pattern_line.matches("!=").count(),
        1,
        "exactly one exclusion — a second one would silence another arm: {pattern_line}"
    );
}

/// Hostile fixture for [`the_tag_scan_bites_both_ways`]. Kept OUTSIDE the test
/// body: the workspace's assertion-free-test ratchet scans test bodies by
/// brace depth, and a raw string full of `}` and `#[cfg(test)]` inside the body
/// hides the assertions that follow it.
const TAG_SCAN_FIXTURE: &str = r#"
fn a() {
    error!(
        code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
        rescued = rows,
        "flush failed — the rows were RESCUED to the spill file (named)"
    );
    error!(
        code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
        dropped = rows,
        source = RESCUED_TO_SPILL_SOURCE,
        "flush failed AND the rescue failed — permanently lost"
    );
    error!(
        code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
        rescued = rows,
        source = RESCUED_TO_SPILL_SOURCE,
        "flush failed — RESCUED (fine)"
    );
    error!(code = ErrorCode::Other.code_str(), "RESCUED but not HOT-PATH-02");
}
#[cfg(test)]
mod tests { fn t() { error!(code = ErrorCode::HotPath02WriterQueueDrop.code_str(), "RESCUED"); } }
"#;

#[test]
fn the_tag_scan_bites_both_ways() {
    let (untagged, tagged_losses, rescued) = tag_offences(TAG_SCAN_FIXTURE);
    assert_eq!(untagged.len(), 1, "{untagged:#?}");
    assert_eq!(tagged_losses.len(), 1, "{tagged_losses:#?}");
    assert_eq!(rescued, 2, "test-module emits must not count");
}
