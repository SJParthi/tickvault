//! Telegram noise-cut REGRESSION-LOCK (2026-07-10, F2 + F4 + FIX-5).
//!
//! These ratchets are a REGRESSION-LOCK of the specific reclassification and
//! wording changes this PR made — NOT a full enforcement of Telegram
//! commandment #2. They pin:
//!
//!   (a) every INFORMATIONAL status ping is constructed via
//!       `NotificationEvent::CustomStatus` OR `NotificationEvent::CustomStatusUrgent`
//!       (both Severity::Low → never SMS; CustomStatus batches, CustomStatusUrgent
//!       ships instantly), and every genuinely ACTIONABLE alert stays on
//!       `NotificationEvent::Custom` (Severity::High → paged + SMS); and
//!   (b) no `Custom`/`CustomStatus`/`CustomStatusUrgent` body re-introduces one of
//!       the SPECIFIC library/implementation phrases this PR removed (commandment
//!       #2 regression-lock), nor a redundant in-body `WARNING:`/`CRITICAL:`
//!       prefix (commandment #10 — dispatch already prepends the severity emoji).
//!
//! This is a lock against the exact jargon we removed, so a stronger/complete
//! commandment-#2 checker (all bodies, all phrasings) remains a separate,
//! broader effort — do not read this as proving every Telegram body is clean.
//!
//! The scanner is string-aware: it strips `//` line comments (respecting string
//! literals so a `://` inside a body is never mistaken for a comment), then
//! extracts ONLY the `message:` string literal of each
//! `NotificationEvent::Custom`/`CustomStatus`/`CustomStatusUrgent` construction.
//! It therefore never matches this test file's own assertion literals (it scans
//! the two target source files via `include_str!`, not itself), nor the
//! `warn!`/`error!` log lines that legitimately mention the jargon.

const MAIN_RS: &str = include_str!("../src/main.rs");
const ORPHAN_RS: &str = include_str!("../src/orphan_position_watchdog_boot.rs");

/// A single extracted Custom/CustomStatus/CustomStatusUrgent message body.
#[derive(Debug, Clone)]
struct CustomBody {
    /// `true` for the Low variants (`CustomStatus` / `CustomStatusUrgent` —
    /// informational, never SMS); `false` for `Custom` (High, actionable).
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

/// Collapse a string to a canonical jargon-matching form: lowercase, with every
/// non-alphanumeric character removed. So "ring buffer", "ring-buffer" and
/// "ringbuffer" all collapse to "ringbuffer"; "web socket"/"web-socket" →
/// "websocket"; "quest db" → "questdb". A banned phrase is squished the same
/// way, so a single needle catches every spacing/hyphenation variant.
fn squish(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Bound the `message:` FIELD EXPRESSION: the text after `message:` up to the
/// first `,` or closing `}` at paren/brace/bracket depth 0 (string-aware).
/// Prevents the literal search from bleeding past the field into a LATER
/// string in the same window (e.g. a following `metrics::counter!("...")`
/// literal being misread as the message body).
fn message_expression(after_msg: &str) -> &str {
    let bytes = after_msg.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        let c = b as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                if depth == 0 {
                    return &after_msg[..i];
                }
                depth -= 1;
            }
            ',' if depth == 0 => return &after_msg[..i],
            _ => {}
        }
    }
    after_msg
}

/// Resolve ONE level of same-file wording-helper indirection: for
/// `message: some_helper(...)`, find `fn some_helper(` in the (comment-
/// stripped) source and read the FIRST string literal inside a bounded
/// window of its body — the wording the helper returns.
///
/// 2026-07-14 re-home: the orphan-watchdog degraded alert bodies moved from
/// inline `message: "..."` literals (the shape this guard was written
/// against at orphan boot 201/222) into pure wording fns
/// (`degraded_no_session_message()` / `degraded_no_token_message()` /
/// `degraded_fetch_failed_message()`) so the wording is unit-pinnable. The
/// alerts' meaning, severity (Custom/High), and the pinned
/// "open-position check" fragment are unchanged — this resolution keeps the
/// guard scanning the REAL message text instead of going blind on (or
/// misreading) helper-routed bodies.
fn resolve_helper_literal(src: &str, ident: &str) -> Option<String> {
    let sig = format!("fn {ident}(");
    let pos = src.find(&sig)?;
    let after = &src[pos..];
    let window = &after[..after.len().min(600)];
    let qrel = window.find('"')?;
    Some(read_string_literal(&window[qrel + 1..]))
}

/// Extract every `NotificationEvent::Custom`/`CustomStatus`/`CustomStatusUrgent`
/// `{ message: ... }` body from a comment-stripped source string.
fn extract_custom_bodies(raw: &str) -> Vec<CustomBody> {
    let src = strip_line_comments(raw);
    const NEEDLE: &str = "NotificationEvent::Custom";
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = src[search_from..].find(NEEDLE) {
        let pos = search_from + rel;
        let after = &src[pos + NEEDLE.len()..];
        search_from = pos + NEEDLE.len();

        // Classify: `CustomStatusUrgent {` / `CustomStatus {` (both Low) vs
        // `Custom {` (High). Anything else (e.g. a bare `NotificationEvent::
        // Custom` in prose that is not a construction) is skipped so it can
        // never mis-attribute a distant `message:`. Check the longer prefix
        // first ("StatusUrgent" before "Status").
        let trimmed = after.trim_start();
        let (is_status, body_rest) = if let Some(r2) = trimmed.strip_prefix("StatusUrgent") {
            (true, r2.trim_start())
        } else if let Some(r2) = trimmed.strip_prefix("Status") {
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
        // Bound the search to THIS field's expression, then either read the
        // inline literal (`"..."` / `format!("...")`) or resolve one level
        // of same-file wording-helper indirection (2026-07-14 re-home).
        let expr = message_expression(after_msg);
        let literal = if let Some(qrel) = expr.find('"') {
            read_string_literal(&expr[qrel + 1..])
        } else {
            let ident: String = expr
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if ident.is_empty() {
                continue;
            }
            let Some(resolved) = resolve_helper_literal(&src, &ident) else {
                continue;
            };
            resolved
        };
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
// (a) routing: informational → Low (CustomStatus / CustomStatusUrgent),
//     actionable → Custom (High)
// ---------------------------------------------------------------------------

/// Every informational status-ping body must be built via one of the Low
/// variants (CustomStatus batched, or CustomStatusUrgent instant) so it never
/// SMS-pages. A regression back to `Custom` (High) fails here.
#[test]
fn status_pings_use_low_variant_not_custom() {
    let bodies = all_bodies();
    // Fragments unique to each informational status body (post-reword).
    const STATUS_FRAGMENTS: &[&str] = &[
        "Fast start",                        // 2752 (CustomStatusUrgent)
        "Recovered saved prices",            // 6776 (CustomStatus)
        "Price backups growing",             // 8905 (CustomStatus)
        "Background service auto-restarted", // 8962 (CustomStatus)
        "Dhan feed started",                 // 10762 (CustomStatusUrgent)
        "Dhan feed stopped",                 // 10962 (CustomStatusUrgent)
        "Market closed",                     // 11100 (CustomStatus)
    ];
    for frag in STATUS_FRAGMENTS {
        let hits: Vec<&CustomBody> = bodies.iter().filter(|b| b.literal.contains(frag)).collect();
        assert!(
            !hits.is_empty(),
            "informational status fragment {frag:?} not found in any Custom* body — did the \
             wording change without updating this guard?"
        );
        for b in hits {
            assert!(
                b.is_status,
                "status ping {frag:?} must be a Low variant (CustomStatus / CustomStatusUrgent, \
                 no SMS), but was found in a NotificationEvent::Custom (High) body: {:?}",
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
        "Price database unavailable", // main.rs 6793 — DB down at boot
        "Low disk space",             // main.rs 8885 — free disk
        // orphan boot — check Dhan before 15:30. 2026-07-14: the bodies live
        // in pure wording fns (degraded_no_*_message / degraded_fetch_failed_
        // message), reached via the helper-fn resolution in
        // `extract_custom_bodies`; the fragment + High severity are unchanged.
        "open-position check",
    ];
    for frag in ACTIONABLE_FRAGMENTS {
        let hits: Vec<&CustomBody> = bodies.iter().filter(|b| b.literal.contains(frag)).collect();
        assert!(
            !hits.is_empty(),
            "actionable fragment {frag:?} not found in any Custom* body"
        );
        for b in hits {
            assert!(
                !b.is_status,
                "actionable alert {frag:?} must stay on NotificationEvent::Custom (High, SMS), \
                 but was found in a Low-variant body: {:?}",
                b.literal
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (b) commandment #2 regression-lock (no removed jargon) + #10 (no redundant
//     severity prefix)
// ---------------------------------------------------------------------------

/// No Custom* body may re-introduce one of the SPECIFIC library/implementation
/// phrases this PR removed. Hyphen/space variants are caught via `squish`
/// (so "ring-buffer", "web socket", "quest db" are all locked out too).
/// Tokenize a string on non-alphanumeric boundaries into whole, lowercased
/// tokens. Used for the WORD-BOUNDARY ban path so a short acronym only trips
/// when it is a WHOLE word — never as a substring inside a legitimate word
/// (e.g. "sns" must not match inside "Indices NSE feed" / "…s NSE…").
fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// Two-mode regression-lock. Returns `Some(offending_needle)` if the body
/// re-introduces removed jargon, else `None`.
///
///   - TOKEN mode (whole-word equality) for short acronyms / single words
///     where an unbounded substring would over-match a legitimate word.
///   - SQUISH-substring mode for multi-word / compound phrases where interior
///     spacing/hyphenation is the evasion vector and the concatenation is
///     distinctive enough to be substring-safe.
fn banned_hit(literal: &str) -> Option<&'static str> {
    // Whole-word tokens: short acronyms + single library names. Matched only
    // as complete tokens, so "…s NSE…" can never trip "sns".
    const TOKEN_BANNED: &[&str] = &[
        "sns",
        "ilp",
        "dlq",
        "mpsc",
        "rkyv",
        "spill",
        "container",
        "buffering",
        "valkey",
        "papaya",
        "prometheus",
    ];
    // Compound phrases: squished so "ring buffer" / "ring-buffer" / "ringbuffer",
    // "web socket" / "websocket", "quest db" / "questdb" all match one needle.
    const COMPOUND_BANNED: &[&str] = &[
        "ring buffer",
        "disk spill",
        "web socket",
        "quest db",
        "arc-swap",
        "docker compose",
        "docker container",
    ];
    let toks = tokens(literal);
    for banned in TOKEN_BANNED {
        if toks.iter().any(|t| t == banned) {
            return Some(banned);
        }
    }
    let squished = squish(literal);
    for phrase in COMPOUND_BANNED {
        if squished.contains(squish(phrase).as_str()) {
            return Some(phrase);
        }
    }
    None
}

#[test]
fn custom_bodies_carry_no_impl_jargon() {
    for b in all_bodies() {
        assert!(
            banned_hit(&b.literal).is_none(),
            "Custom* body re-introduced banned impl/library jargon {:?} \
             (commandment #2 regression-lock): {:?}",
            banned_hit(&b.literal).unwrap_or(""),
            b.literal
        );
    }
}

/// No Custom* body may self-prefix a severity word — dispatch already prepends
/// the `[SEV]` emoji/tag (commandment #10).
#[test]
fn custom_bodies_have_no_redundant_severity_prefix() {
    const BANNED_PREFIXES: &[&str] = &["WARNING:", "CRITICAL:"];
    for b in all_bodies() {
        for prefix in BANNED_PREFIXES {
            assert!(
                !b.literal.contains(prefix),
                "Custom* body carries a redundant severity prefix {prefix:?} (commandment #10 — \
                 the dispatcher already prepends the severity tag): {:?}",
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
        "expected at least one Low-variant body — scanner may be broken"
    );
    assert!(
        bodies.iter().any(|b| !b.is_status),
        "expected at least one Custom (High) body — scanner may be broken"
    );
}

#[test]
fn squish_normalizes_hyphen_and_space_variants() {
    assert_eq!(squish("ring-buffer"), "ringbuffer");
    assert_eq!(squish("ring buffer"), "ringbuffer");
    assert_eq!(squish("ringbuffer"), "ringbuffer");
    assert_eq!(squish("web socket"), "websocket");
    assert_eq!(squish("Quest DB"), "questdb");
}

#[test]
fn jargon_guard_allows_legit_nse_indices_wording() {
    // FIX-A regression: squish("Indices NSE feed") == "indicesnsefeed" CONTAINS
    // "sns", but the token path must NOT flag it (whole-word equality only).
    assert_eq!(banned_hit("Indices NSE feed streaming"), None);
    assert_eq!(banned_hit("all NSE services are ready"), None);
    assert_eq!(
        banned_hit("prices are flowing and all services are ready"),
        None
    );
}

#[test]
fn jargon_guard_still_bites_bare_tokens_and_phrases() {
    // Token path still bites a bare acronym / single-word token.
    assert_eq!(banned_hit("dumped to the sns topic"), Some("sns"));
    assert_eq!(banned_hit("spill files remain on disk"), Some("spill"));
    assert_eq!(banned_hit("wrote via ILP"), Some("ilp"));
    // Compound path still bites spacing/hyphen variants.
    assert_eq!(banned_hit("stored in the ring-buffer"), Some("ring buffer"));
    assert_eq!(banned_hit("stored in the ring buffer"), Some("ring buffer"));
    assert_eq!(
        banned_hit("restarting via docker compose"),
        Some("docker compose")
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
fn extractor_classifies_all_three_variants() {
    let sample = r#"
        notifier.notify(NotificationEvent::Custom {
            message: "real alert body".to_string(),
        });
        notifier.notify(NotificationEvent::CustomStatus {
            message: "status ping body".to_string(),
        });
        notifier.notify(NotificationEvent::CustomStatusUrgent {
            message: "urgent status body".to_string(),
        });
    "#;
    let bodies = extract_custom_bodies(sample);
    assert_eq!(
        bodies.len(),
        3,
        "expected exactly three bodies, got {bodies:?}"
    );
    let custom: Vec<&CustomBody> = bodies.iter().filter(|b| !b.is_status).collect();
    let status: Vec<&CustomBody> = bodies.iter().filter(|b| b.is_status).collect();
    assert_eq!(custom.len(), 1, "exactly one High Custom expected");
    assert_eq!(
        status.len(),
        2,
        "CustomStatus + CustomStatusUrgent are both Low"
    );
    assert_eq!(custom[0].literal, "real alert body");
    assert!(status.iter().any(|b| b.literal == "status ping body"));
    assert!(status.iter().any(|b| b.literal == "urgent status body"));
}

#[test]
fn extractor_resolves_wording_helper_indirection() {
    // 2026-07-14 re-home: `message: helper_fn(),` must resolve to the
    // helper's literal (same file, one level), inheriting the construction's
    // severity classification.
    let sample = r#"
        fn my_degraded_wording() -> String {
            "resolved helper body with open-position check".to_string()
        }
        notifier.notify(NotificationEvent::Custom {
            message: my_degraded_wording(),
        });
        metrics::counter!("tv_some_counter_total").increment(1);
    "#;
    let bodies = extract_custom_bodies(sample);
    assert_eq!(bodies.len(), 1, "expected exactly one body, got {bodies:?}");
    assert!(!bodies[0].is_status, "helper-routed body must stay High");
    assert_eq!(
        bodies[0].literal,
        "resolved helper body with open-position check"
    );
}

#[test]
fn extractor_never_bleeds_past_message_field_into_later_literal() {
    // A helper call with NO resolvable fn must be SKIPPED — never misread the
    // NEXT string in the window (e.g. a following metrics counter name) as
    // the message body.
    let sample = r#"
        notifier.notify(NotificationEvent::Custom {
            message: helper_defined_elsewhere(),
        });
        metrics::counter!("tv_unrelated_counter_total").increment(1);
    "#;
    let bodies = extract_custom_bodies(sample);
    assert!(
        bodies.is_empty(),
        "unresolvable helper must be skipped, not misread: {bodies:?}"
    );
}

#[test]
fn message_expression_stops_at_field_comma_and_block_close() {
    assert_eq!(
        message_expression(" my_helper(a, b),\n});"),
        " my_helper(a, b)"
    );
    assert_eq!(
        message_expression(" \"inline, with comma\".to_string(),\n});"),
        " \"inline, with comma\".to_string()"
    );
    // Closing `}` of the construction at depth 0 also terminates.
    assert_eq!(message_expression(" ident }"), " ident ");
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
