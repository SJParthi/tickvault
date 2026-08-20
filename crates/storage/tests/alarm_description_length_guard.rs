//! Every `alarm_description` must fit AWS's 1024-character cap.
//!
//! ## Why this exists
//!
//! CloudWatch rejects an `alarm_description` longer than 1024 characters, and
//! terraform surfaces it only at `validate` time — i.e. in CI, after a push,
//! against real AWS credentials. On 2026-08-19 two new alarms were written
//! with genuinely useful multi-paragraph runbook text baked into the
//! description; both were well over the cap, and the first signal was a red
//! `Terraform plan` job.
//!
//! Nothing local would have caught it. `terraform` is not installed in the
//! dev container, so `terraform validate` cannot run in the pre-push gate,
//! and no Rust test read these files. That is the gap this closes: the cap is
//! a fixed, documented AWS limit and a plain character count is enough to
//! enforce it without terraform present.
//!
//! ## What it deliberately does NOT do
//!
//! It does not parse HCL. It scans for `alarm_description = "..."` and
//! measures the literal, which is sufficient because that is the only shape
//! this repo uses. A heredoc or a variable-interpolated description would be
//! invisible to it — recorded here rather than implied away, since a future
//! alarm written that way would pass this guard and still fail in CI.

use std::fs;
use std::path::Path;

/// AWS CloudWatch's documented maximum. Not a style preference — the API
/// rejects anything longer.
const MAX_ALARM_DESCRIPTION_CHARS: usize = 1024;

/// Terraform directory holding every alarm definition.
const TF_DIR: &str = "../../deploy/aws/terraform";

/// Extracts every `alarm_description = "..."` literal from one file.
///
/// Returns `(line_number, description)` pairs. Handles the escaped-quote case
/// so a description containing `\"` is measured whole rather than truncated at
/// the escape — measuring a fragment would under-count and let a real breach
/// through, which is the one failure mode that matters here.
fn alarm_descriptions(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (idx, line) in src.lines().enumerate() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("alarm_description") else {
            continue;
        };
        let Some(eq) = rest.find('=') else { continue };
        let after = rest[eq + 1..].trim_start();
        let Some(body) = after.strip_prefix('"') else {
            // Not a simple string literal (heredoc / interpolation). Skipped
            // by design; see the module docs' honest-limit note.
            continue;
        };
        let mut value = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        value.push(escaped);
                    }
                }
                '"' => break,
                other => value.push(other),
            }
        }
        out.push((idx + 1, value));
    }
    out
}

#[test]
fn every_alarm_description_fits_the_aws_cap() {
    let dir = Path::new(TF_DIR);
    assert!(
        dir.is_dir(),
        "terraform directory not found at {TF_DIR} — this guard is scanning \
         the wrong path and would pass vacuously"
    );

    let mut scanned = 0usize;
    let mut breaches: Vec<String> = Vec::new();

    for entry in fs::read_dir(dir).expect("read terraform dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        let Ok(src) = fs::read_to_string(&path) else {
            continue;
        };
        for (line, desc) in alarm_descriptions(&src) {
            scanned += 1;
            let len = desc.chars().count();
            if len > MAX_ALARM_DESCRIPTION_CHARS {
                breaches.push(format!(
                    "{}:{line} — {len} chars (cap {MAX_ALARM_DESCRIPTION_CHARS}, over by {})",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                    len - MAX_ALARM_DESCRIPTION_CHARS
                ));
            }
        }
    }

    // Non-vacuity: a guard that finds no descriptions would pass forever while
    // enforcing nothing. This repo has many alarms; if the count collapses,
    // the extractor has drifted from the file format.
    assert!(
        scanned >= 20,
        "only {scanned} alarm_description literals found — the extractor has \
         probably drifted from the file format and this guard is now vacuous"
    );

    assert!(
        breaches.is_empty(),
        "alarm_description exceeds AWS's {MAX_ALARM_DESCRIPTION_CHARS}-char cap \
         ({} breach(es)). CloudWatch rejects these and `terraform validate` \
         fails in CI. Put the long-form runbook text in a `#` comment above the \
         resource — comments have no cap — and keep the description to the \
         pager text.\n  {}",
        breaches.len(),
        breaches.join("\n  ")
    );
}

#[test]
fn extractor_measures_escaped_quotes_and_ignores_non_literals() {
    // Bite-test in both directions: the extractor must see a real literal and
    // measure it whole, and must not be fooled into measuring a fragment.
    let src = r#"
resource "aws_cloudwatch_metric_alarm" "a" {
  alarm_description = "plain text"
}
resource "aws_cloudwatch_metric_alarm" "b" {
  alarm_description = "has \"quoted\" words inside"
}
resource "aws_cloudwatch_metric_alarm" "c" {
  alarm_description = local.some_var
}
"#;
    let found = alarm_descriptions(src);
    assert_eq!(found.len(), 2, "two string literals, one interpolation");
    assert_eq!(found[0].1, "plain text");
    assert_eq!(
        found[1].1, r#"has "quoted" words inside"#,
        "an escaped quote must not truncate the measurement — truncating \
         under-counts, which is the direction that lets a real breach through"
    );
}
