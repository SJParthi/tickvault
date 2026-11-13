//! A Dhan support email shared as a GitHub link must never look ready while it
//! still holds template placeholders (PR C review, 2026-11-13).
//!
//! CLAUDE.md "DHAN SUPPORT COMMUNICATIONS": every `<PLACEHOLDER>` is filled
//! before the link is shared, with precise contract labels, SecurityIds,
//! microsecond IST times and verbatim rows. A draft whose evidence cannot be
//! filled yet (no database reachable when drafting) carries a VISIBLE
//! "NOT READY TO SEND" note in the rendered page (an HTML comment does not
//! render) and in its README row; the note goes once the placeholders do.
//!
//! Scope: files dated 2026-11-13 or later. Earlier files predate this guard
//! and some keep template tokens in sent text; they are history, not drafts.

use std::path::PathBuf;

const FIRST_GUARDED_DATE: &str = "2026-11-13";
const NOT_READY: &str = "NOT READY TO SEND";

fn support_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/dhan-support")
}

/// The text a reader of the rendered page sees: HTML comments removed.
fn rendered(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("<!--") {
        out.push_str(&rest[..open]);
        rest = match rest[open..].find("-->") {
            Some(close) => &rest[open + close + 3..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// A template token: `<` then an upper-case letter or `_` (the grep the
/// support workflow uses, `<[A-Z_]`).
fn has_placeholder(text: &str) -> bool {
    text.as_bytes()
        .windows(2)
        .any(|w| w[0] == b'<' && (w[1].is_ascii_uppercase() || w[1] == b'_'))
}

fn guarded_files() -> Vec<(String, String)> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(support_dir()).expect("docs/dhan-support lists") {
        let name = entry
            .expect("dir entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        let dated = name.len() > 10
            && name.as_bytes()[..10].iter().enumerate().all(|(i, b)| {
                if i == 4 || i == 7 {
                    *b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            });
        if dated && name.ends_with(".md") && name[..10] >= *FIRST_GUARDED_DATE {
            let text = std::fs::read_to_string(support_dir().join(&name)).expect("reads");
            files.push((name, text));
        }
    }
    files
}

#[test]
fn a_draft_with_placeholders_says_not_ready_where_a_reader_sees_it() {
    let files = guarded_files();
    assert!(
        files
            .iter()
            .any(|(n, _)| n == "2026-11-13-feed-latency-and-zone.md"),
        "non-vacuity: the 2026-11-13 feed latency email is scanned"
    );
    let readme = std::fs::read_to_string(support_dir().join("README.md")).expect("README reads");
    for (name, text) in files {
        let seen = rendered(&text);
        let open = has_placeholder(&seen);
        assert_eq!(
            open,
            seen.contains(NOT_READY),
            "{name}: the rendered page must carry `{NOT_READY}` exactly while it still holds \
             `<PLACEHOLDER>` tokens (an HTML comment does not render)"
        );
        if let Some(row) = readme.lines().find(|l| l.contains(&format!("`{name}`"))) {
            assert_eq!(
                open,
                row.contains(NOT_READY),
                "{name}: the README row must say `{NOT_READY}` exactly while it is a draft"
            );
        }
    }
}

/// The samples must be honest: the query the operator is told to run reads
/// BOTH tables (`ticks` keeps only packets at most 2 s after their trade, so
/// it alone hides the slow tail), drops rows whose trade time fell back to
/// the receipt time (a fake 0 delay), and samples windows across the day
/// rather than the first rows by receipt.
#[test]
fn the_feed_latency_sample_query_reads_both_tables_and_skips_fake_zero_delays() {
    let text = std::fs::read_to_string(support_dir().join("2026-11-13-feed-latency-and-zone.md"))
        .expect("the email reads");
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for needle in [
        "FROM ticks",
        "FROM tick_quotes",
        "received_at != ts",
        "exchange_timestamp AS trade_time",
        "UNION ALL",
    ] {
        assert!(
            flat.contains(needle),
            "the sample query must contain `{needle}`"
        );
    }
    assert!(
        !flat.contains("ORDER BY received_at LIMIT 5"),
        "the first five rows by receipt are not samples spread across the day"
    );
}
