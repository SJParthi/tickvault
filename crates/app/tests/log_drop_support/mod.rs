//! Shared by `log_drop_alarm_shipped_sinks_guard.rs` and
//! `alarmed_counters_are_seeded_guard.rs` (noise lock §2.9-i, 2026-10-10).
//!
//! `zero_registered_names()` in the seeding guard reads metric NAMES only, so
//! it cannot tell whether a labelled counter is seeded for the particular
//! label values a log metric filter slices on. This parser can.

use std::collections::BTreeSet;

/// Every `label` value for which `metric` is registered at zero in `src`.
///
/// `src` must already have its Rust comments stripped, so a commented-out
/// seed does not count. Two shapes are recognised:
///
/// - the literal form, which may wrap across lines:
///   `metrics::counter!("tv_x", "sink" => "app_log").increment(0);`
/// - the loop form, with the registration inside the next three lines:
///   `for sink in ["app_log", "errors_jsonl"] {`
///   `    metrics::counter!("tv_x", "sink" => sink).increment(0);`
///
/// Anything else (a value bound elsewhere, a helper function) is not seen,
/// so a seed written another way reads as missing: the guard fails closed.
pub fn seeded_label_values(src: &str, metric: &str, label: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let quoted_metric = format!("\"{metric}\"");

    // Literal form. Each `counter!(` span up to its `;`, whitespace removed so
    // a wrapped registration reads the same as a one-line one.
    let literal_key = format!("\"{label}\"=>\"");
    for span in src.split("counter!(").skip(1) {
        let head = span.split(';').next().unwrap_or_default();
        let compact: String = head.chars().filter(|c| !c.is_whitespace()).collect();
        if !compact.starts_with(&quoted_metric) || !compact.contains(".increment(0)") {
            continue;
        }
        if let Some((_, rest)) = compact.split_once(&literal_key)
            && let Some(value) = rest.split('"').next()
        {
            out.insert(value.to_owned());
        }
    }

    // Loop form.
    let lines: Vec<&str> = src.lines().collect();
    for (at, line) in lines.iter().enumerate() {
        let Some(rest) = line.trim().strip_prefix("for ") else {
            continue;
        };
        let Some((ident, tail)) = rest.split_once(" in [") else {
            continue;
        };
        let Some((array, _)) = tail.split_once(']') else {
            continue;
        };
        let ident = ident.trim();
        let body_end = (at + 4).min(lines.len());
        let body: String = lines[at + 1..body_end]
            .concat()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let wanted = format!("counter!({quoted_metric},\"{label}\"=>{ident}).increment(0)");
        if body.contains(&wanted) {
            out.extend(array.split('"').skip(1).step_by(2).map(str::to_owned));
        }
    }
    out
}
