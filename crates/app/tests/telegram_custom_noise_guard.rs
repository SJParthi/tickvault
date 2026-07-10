//! Telegram noise-cut guard (2026-07-10, F2 + F4).
//!
//! Build-failing ratchets that pin the two invariants of the
//! `Custom` → `CustomStatus` reclassification:
//!
//!   (a) every INFORMATIONAL status ping is constructed via
//!       `NotificationEvent::CustomStatus` (Severity::Low → batched, no SMS),
//!       and every genuinely ACTIONABLE alert stays on
//!       `NotificationEvent::Custom` (Severity::High → paged + SMS); and
//!   (b) no `Custom`/`CustomStatus` body carries implementation/library jargon
//!       (commandment #2) or a redundant in-body `WARNING:`/`CRITICAL:` prefix
//!       (commandment #10 — dispatch already prepends the severity emoji).
//!
//! The scanner is string-aware: it strips `//` line comments (respecting
//! string literals so a `://` inside a body is never mistaken for a comment),
//! then extracts ONLY the `message:` string literal of each
//! `NotificationEvent::Custom`/`CustomStatus` construction. It therefore never
//! matches this test file's own assertion literals (it scans the two target
//! source files via `include_str!`, not itself), nor the `warn!`/`error!` log
//! lines that legitimately mention the jargon.

const MAIN_RS: &str = include_str!("../src/main.rs");
const ORPHAN_RS: &str = include_str!("../src/orphan_position_watchdog_boot.rs");

/// A single extracted Custom/CustomStatus message body.
#[derive(Debug, Clone)]
struct CustomBody {
    /// `true` for `CustomStatus` (informational, Low), `false` for `Custom`.
    is_status: bool,
    /// The raw source text of the `message:` string literal (jargon-scanned).
    literal: String,
}

/// Strip `//` line comments, respecting Rust string literals so that a `//`
/// (or `://`) INSIDE a `"..."` body is preserved as code, not treated as a
/// comment start. Block comments are not used inside the scanned regions, so
/// only line comments are handled.
fn strip_line_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        // Not in a string.
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] as char == '/' {
            // Line comment — skip to end of line (keep the newline).
            while i < bytes.len() && bytes[i] as char != '\n' {
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Read a Rust string literal body starting AT the opening `"` (exclusive),
/// i.e. `rest` begins with the first character INSIDE the string. Returns the
/// literal text (escapes preserved verbatim as source) up to the closing `"`.
fn read_string_literal(rest: &str) -> String {
    let bytes = rest.as_bytes();
    let mut out = String::new();
    let mut escaped = false;
    for &b in bytes {
        let c = b as char;
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' {
            out.push(c);
            escaped = true;
            continue;
        }
        if c == '"' {
            break;
        }
        out.push(c);
    }
    out
}

/// Extract every `NotificationEvent::Custom { message: ... }` and
/// `NotificationEvent::CustomStatus { message: ... }` body from a
/// comment-stripped source string.
fn extract_custom_bodies(raw: &str) -> Vec<CustomBody> {
    let src = strip_line_comments(raw);
    const NEEDLE: &str = "NotificationEvent::Custom";
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = src[search_from..].find(NEEDLE) {
        let pos = search_from + rel;
        let after = &src[pos + NEEDLE.len()..];
        search_from = pos + NEEDLE.len();

        // Classify: `CustomStatus {` vs `Custom {`. Anything else (e.g. a bare
        // `NotificationEvent::Custom` in prose that is not a construction) is
        // skipped so it can never mis-attribute a distant `message:`.
        let trimmed = after.trim_start();
        let (is_status, body_rest) = if let Some(r2) = trimmed.strip_prefix("Status") {
            (true, r2.trim_start())
        } else {
            (false, trimmed)
        };
        if !body_rest.starts_with('{') {
            continue;
        }

        // Find the `message:` key within a bounded window of this construction
        // block (never bleeding into an unrelated later construction).
        let window_end = body_rest.len().min(400);
        let window = &body_rest[..window_end];
        let Some(mrel) = window.find("message:") else {
            continue;
        };
        let after_msg = &window[mrel + "message:".len()..];
        let Some(qrel) = after_msg.find('"') else {
            continue;
        };
        let literal = read_string_literal(&after_msg[qrel + 1..]);
        out.push(CustomBody { is_status, literal });
    }
    out
}

fn all_bodies() -> Vec<CustomBody> {
    let mut bodies = extract_custom_bodies(MAIN_RS);
    bodies.extend(extract_custom_bodies(ORPHAN_RS));
    bodies
}

// ---------------------------------------------------------------------------
// (a) routing: informational → CustomStatus, actionable → Custom
// ---------------------------------------------------------------------------

/// Every informational status-ping body must be built via `CustomStatus`
/// (Low, batched, no SMS). A regression back to `Custom` (High) fails here.
#[test]
fn status_pings_use_custom_status_not_custom() {
    let bodies = all_bodies();
    // Fragments unique to each informational status body (post-reword).
    const STATUS_FRAGMENTS: &[&str] = &[
        "Fast start",
        "Recovered saved prices",
        "Price backups growing",
        "Background service auto-restarted",
        "Dhan feed started",
        "Dhan feed stopped",
        "Market closed",
    ];
    for frag in STATUS_FRAGMENTS {
        let hits: Vec<&CustomBody> = bodies.iter().filter(|b| b.literal.contains(frag)).collect();
        assert!(
            !hits.is_empty(),
            "informational status fragment {frag:?} not found in any Custom/CustomStatus body — \
             did the wording change without updating this guard?"
        );
        for b in hits {
            assert!(
                b.is_status,
                "status ping {frag:?} must be constructed via NotificationEvent::CustomStatus \
                 (Low, no SMS), but was found in a NotificationEvent::Custom (High) body: \
                 {:?}",
                b.literal
            );
        }
    }
}

/// The genuinely actionable alerts must stay on `Custom` (High → paged + SMS).
/// Prevents over-demotion of a real operator alert.
#[test]
fn actionable_custom_sites_stay_custom_high() {
    let bodies = all_bodies();
    // Fragments unique to each kept-High actionable body.
    const ACTIONABLE_FRAGMENTS: &[&str] = &[
        "Price database unavailable", // main.rs 6793 — QuestDB down at boot
        "Low disk space",             // main.rs 8885 — free disk
        "open-position check",        // orphan boot 201/222 — check Dhan before 15:30
    ];
    for frag in ACTIONABLE_FRAGMENTS {
        let hits: Vec<&CustomBody> = bodies.iter().filter(|b| b.literal.contains(frag)).collect();
        assert!(
            !hits.is_empty(),
            "actionable fragment {frag:?} not found in any Custom/CustomStatus body"
        );
        for b in hits {
            assert!(
                !b.is_status,
                "actionable alert {frag:?} must stay on NotificationEvent::Custom (High, SMS), \
                 but was found in a CustomStatus (Low) body: {:?}",
                b.literal
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (b) commandments #2 (no jargon) + #10 (no redundant severity prefix)
// ---------------------------------------------------------------------------

/// No Custom/CustomStatus body may carry implementation/library jargon
/// (commandment #2 — plain English only).
#[test]
fn custom_bodies_carry_no_impl_jargon() {
    // Case-insensitive substrings that must never appear in an operator-facing
    // Custom/CustomStatus body.
    const BANNED: &[&str] = &[
        "docker compose",
        "docker container",
        "ring buffer",
        "disk spill",
        "spill file",
        "buffering",
        "questdb",
        "websocket",
    ];
    for b in all_bodies() {
        let lower = b.literal.to_ascii_lowercase();
        for banned in BANNED {
            assert!(
                !lower.contains(banned),
                "Custom/CustomStatus body carries banned impl jargon {banned:?} \
                 (commandment #2): {:?}",
                b.literal
            );
        }
    }
}

/// No Custom/CustomStatus body may self-prefix a severity word — dispatch
/// already prepends the `[SEV]` emoji/tag (commandment #10).
#[test]
fn custom_bodies_have_no_redundant_severity_prefix() {
    const BANNED_PREFIXES: &[&str] = &["WARNING:", "CRITICAL:"];
    for b in all_bodies() {
        for prefix in BANNED_PREFIXES {
            assert!(
                !b.literal.contains(prefix),
                "Custom/CustomStatus body carries a redundant severity prefix {prefix:?} \
                 (commandment #10 — the dispatcher already prepends the severity tag): {:?}",
                b.literal
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Scanner self-tests — prove the extractor is not vacuous.
// ---------------------------------------------------------------------------

#[test]
fn scanner_extracts_both_kinds_and_is_not_empty() {
    let bodies = all_bodies();
    assert!(
        bodies.iter().any(|b| b.is_status),
        "expected at least one CustomStatus body — scanner may be broken"
    );
    assert!(
        bodies.iter().any(|b| !b.is_status),
        "expected at least one Custom body — scanner may be broken"
    );
}

#[test]
fn strip_line_comments_preserves_url_scheme_inside_string() {
    // A `://` inside a string literal must survive; a real `//` comment must go.
    let src = "let x = \"a://b\"; // trailing comment\nlet y = 1;";
    let stripped = strip_line_comments(src);
    assert!(stripped.contains("a://b"), "URL scheme wrongly stripped");
    assert!(
        !stripped.contains("trailing comment"),
        "line comment not stripped"
    );
    assert!(stripped.contains("let y = 1;"));
}

#[test]
fn extractor_classifies_custom_vs_custom_status() {
    let sample = r#"
        notifier.notify(NotificationEvent::Custom {
            message: "real alert body".to_string(),
        });
        notifier.notify(NotificationEvent::CustomStatus {
            message: "status ping body".to_string(),
        });
    "#;
    let bodies = extract_custom_bodies(sample);
    assert_eq!(
        bodies.len(),
        2,
        "expected exactly two bodies, got {bodies:?}"
    );
    let custom = bodies.iter().find(|b| !b.is_status).unwrap();
    let status = bodies.iter().find(|b| b.is_status).unwrap();
    assert_eq!(custom.literal, "real alert body");
    assert_eq!(status.literal, "status ping body");
}

#[test]
fn extractor_ignores_bare_mention_without_construction() {
    // A prose/comment mention that is NOT a `{ ... }` construction must not be
    // treated as a body (would otherwise mis-attribute a distant `message:`).
    let sample = "// this PR uses NotificationEvent::Custom for the pings\n\
        let m: String = String::new();";
    let bodies = extract_custom_bodies(sample);
    assert!(
        bodies.is_empty(),
        "bare mention wrongly parsed as a construction: {bodies:?}"
    );
}
