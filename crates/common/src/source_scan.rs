//! Comment-aware source-scan helpers for the repo's ratchet tests
//! (F8 continuation-review fix, 2026-07-08).
//!
//! # Why this exists
//!
//! Many build-failing ratchets scan production `.rs` sources for
//! code-shaped needles ("the teardown must bound-join the poller", "the
//! re-mint call site must exist"). Three recurring holes made several of
//! those scans silently vacuous or blind:
//!
//! 1. **Split at the first `#[cfg(test)]` literal** — an inline item-level
//!    `#[cfg(test)]` attribute (or even a doc-comment mention) EARLIER in
//!    the file truncated the "production region" mid-file, excluding real
//!    production code from exactly-once / must-exist scans
//!    (token_manager.rs lost ~100 production lines this way).
//! 2. **Prefix-only regions** — `main.rs` carries ~460 lines of PRODUCTION
//!    code AFTER its `mod tests` block (`spawn_post_market_tasks`, …; the
//!    other 2026-07-08 example, `spawn_daily_tick_conservation_task`, was
//!    retired 2026-07-18 with the tick-conservation audit); a
//!    `&src[..tests_mod_off]` prefix is blind to needles (or duplicate
//!    needles) landing there.
//! 3. **Comment matches** — `contains`/`matches` counting satisfied by a
//!    COMMENT or doc-comment mention of the needle
//!    (`LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS` appears in teardown comments),
//!    so reverting the real code to a plain abort stayed green.
//!
//! [`production_region`] closes 1+2 (splits at the `#[cfg(test)]`-gated
//! `mod tests` MODULE, brace-matched, and KEEPS the code after it);
//! [`strip_rust_comments`] closes 3. Both blank excised bytes with spaces
//! (newlines preserved) so byte offsets in the returned string line up with
//! the original file — source-ORDER assertions keep working unchanged.
//!
//! Test-support only: cold-path, never on the tick pipeline. The lexer is
//! a single O(n) pass, panic-free (returns `Option` on structural
//! failure); callers in `#[cfg(test)]` code `.expect()` loudly.

/// Byte classification produced by the single-pass lexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteClass {
    /// Plain code (identifiers, punctuation, whitespace).
    Code,
    /// Inside a `//`/`///`/`//!` line comment or `/* */` block comment
    /// (nesting-aware), INCLUDING the comment delimiters.
    Comment,
    /// Inside a string / raw-string / char / byte-string literal,
    /// INCLUDING the quotes.
    Str,
}

/// Single-pass lexer: classify every byte of `src` as code, comment, or
/// string-literal content. Handles line comments, nested block comments,
/// `"…"` strings with escapes, raw strings `r#"…"#` (any hash count),
/// byte strings, char literals, and — crucially — does NOT mistake
/// lifetimes (`&'a str`) for unterminated char literals.
fn classify_bytes(src: &str) -> Vec<ByteClass> {
    let bytes = src.as_bytes();
    let mut out = vec![ByteClass::Code; bytes.len()];
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        // Line comment.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out[i] = ByteClass::Comment;
                i += 1;
            }
            continue;
        }
        // Block comment (nesting-aware).
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let mut depth = 0usize;
            while i < bytes.len() {
                if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    depth += 1;
                    out[i] = ByteClass::Comment;
                    out[i + 1] = ByteClass::Comment;
                    i += 2;
                    continue;
                }
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    depth = depth.saturating_sub(1);
                    out[i] = ByteClass::Comment;
                    out[i + 1] = ByteClass::Comment;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                out[i] = ByteClass::Comment;
                i += 1;
            }
            continue;
        }
        // Raw string r"…" / r#"…"# (and byte-raw br#"…"#).
        if (b == b'r' || b == b'b')
            && let Some(len) = raw_string_len(&bytes[i..])
        {
            for slot in out.iter_mut().skip(i).take(len) {
                *slot = ByteClass::Str;
            }
            i += len;
            continue;
        }
        // Ordinary string "…" (and byte string b"…").
        if b == b'"' || (b == b'b' && i + 1 < bytes.len() && bytes[i + 1] == b'"') {
            let start = i;
            i += if b == b'b' { 2 } else { 1 };
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let end = i.min(bytes.len());
            for slot in out.iter_mut().take(end).skip(start) {
                *slot = ByteClass::Str;
            }
            continue;
        }
        // Char literal vs lifetime: 'x' or '\n' is a char literal; 'a in
        // `&'a str` is a lifetime (no closing quote within 2 bytes).
        if b == b'\'' {
            let is_char_literal = if i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                true // escape ⇒ always a char literal
            } else {
                // 'X' shape: closing quote exactly 2 bytes ahead (ASCII) —
                // multibyte chars are rare in needles; treat conservatively.
                i + 2 < bytes.len() && bytes[i + 2] == b'\''
            };
            if is_char_literal {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'\'' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                let end = i.min(bytes.len());
                for slot in out.iter_mut().take(end).skip(start) {
                    *slot = ByteClass::Str;
                }
                continue;
            }
            // Lifetime — plain code, fall through.
        }
        i += 1;
    }
    out
}

/// Length of a raw-string token (`r"…"`, `r#"…"#`, `br##"…"##`) starting at
/// `bytes[0]`, or `None` if `bytes` does not start one.
fn raw_string_len(bytes: &[u8]) -> Option<usize> {
    let mut j = 0usize;
    if bytes.first() == Some(&b'b') {
        j += 1;
    }
    if bytes.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let mut hashes = 0usize;
    while bytes.get(j) == Some(&b'#') {
        hashes += 1;
        j += 1;
    }
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    j += 1;
    // Scan for `"` followed by `hashes` hash marks.
    while j < bytes.len() {
        if bytes[j] == b'"' {
            let mut k = 0usize;
            while k < hashes && bytes.get(j + 1 + k) == Some(&b'#') {
                k += 1;
            }
            if k == hashes {
                return Some(j + 1 + hashes);
            }
        }
        j += 1;
    }
    Some(bytes.len())
}

/// Returns `src` with every COMMENT byte (line + doc + nested block
/// comments) replaced by a space — newlines preserved, string literals
/// kept verbatim — so needle counts can never be satisfied by a comment
/// mention, while byte offsets still line up with the original source.
#[must_use]
pub fn strip_rust_comments(src: &str) -> String {
    let classes = classify_bytes(src);
    let mut out = String::with_capacity(src.len());
    for (idx, ch) in src.char_indices() {
        if classes.get(idx) == Some(&ByteClass::Comment) && ch != '\n' {
            // A multibyte char inside a comment collapses to ONE space —
            // offsets drift by (len_utf8 - 1) per such char, which is fine:
            // needles are ASCII and ordering (not absolute offsets across
            // multibyte comment text) is what callers assert.
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Returns the PRODUCTION region of a Rust source file: the whole file
/// with its `#[cfg(test)]`-gated top-level `mod tests { … }` block blanked
/// out (spaces, newlines preserved) — INCLUDING any production code that
/// follows the test module (the main.rs trailing-code blindness, F8).
///
/// Brace matching only counts braces classified as CODE (never braces
/// inside strings or comments), so a `{` in a test string literal cannot
/// derail the block end.
///
/// Returns `None` when no `#[cfg(test)]`-gated `mod tests {` exists or the
/// braces never balance — callers (`#[cfg(test)]` ratchets) `.expect()`
/// loudly so a refactor updates the guard instead of hollowing it.
#[must_use]
pub fn production_region(src: &str) -> Option<String> {
    let classes = classify_bytes(src);
    // Find the first CODE occurrence of a top-level `mod tests {` that is
    // #[cfg(test)]-gated within the preceding 300 bytes.
    let needle = "\nmod tests {";
    let mut search_from = 0usize;
    let (mod_off, brace_off) = loop {
        let rel = src[search_from..].find(needle)?;
        let abs = search_from + rel;
        let brace = abs + needle.len() - 1; // the `{`
        let is_code = classes.get(brace) == Some(&ByteClass::Code);
        let preamble_start = abs.saturating_sub(300);
        let gated = src[preamble_start..abs].contains("#[cfg(test)]");
        if is_code && gated {
            break (abs + 1, brace); // abs+1: skip the leading '\n'
        }
        search_from = abs + needle.len();
    };
    // Brace-match from brace_off, counting CODE braces only.
    let bytes = src.as_bytes();
    let mut depth = 0i64;
    let mut end = None;
    for idx in brace_off..bytes.len() {
        if classes[idx] != ByteClass::Code {
            continue;
        }
        match bytes[idx] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(idx + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    let mut out = String::with_capacity(src.len());
    for (idx, ch) in src.char_indices() {
        if idx >= mod_off && idx < end && ch != '\n' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_rust_comments_strips_line_and_doc_but_keeps_strings() {
        let src = "let a = 1; // needle_in_comment\n/// doc needle_in_comment\nlet b = \"needle_in_string // not a comment\";\n";
        let stripped = strip_rust_comments(src);
        assert!(
            !stripped.contains("needle_in_comment"),
            "comment mentions must be blanked"
        );
        assert!(
            stripped.contains("needle_in_string // not a comment"),
            "string content (even containing //) must be preserved verbatim"
        );
        assert_eq!(
            stripped.matches('\n').count(),
            src.matches('\n').count(),
            "newlines must be preserved for line-oriented offsets"
        );
    }

    #[test]
    fn strips_nested_block_comments() {
        let src = "a /* outer /* inner needle */ still comment */ b";
        let stripped = strip_rust_comments(src);
        assert!(!stripped.contains("needle"));
        assert!(!stripped.contains("still comment"));
        assert!(stripped.contains('a') && stripped.contains('b'));
    }

    #[test]
    fn lifetimes_are_not_char_literals() {
        // A naive char-literal lexer would treat `'a` as an unterminated
        // char and swallow the rest of the file (incl. the comment) as Str.
        let src = "fn f<'a>(x: &'a str) {} // trailing needle";
        let stripped = strip_rust_comments(src);
        assert!(
            !stripped.contains("trailing needle"),
            "the comment after a lifetime must still be stripped"
        );
        assert!(stripped.contains("&'a str"), "code must be intact");
    }

    #[test]
    fn char_literal_containing_slash_slash_is_not_a_comment_start() {
        let src = "let c = '/'; let d = '/'; // real comment";
        let stripped = strip_rust_comments(src);
        assert!(stripped.contains("let d = '/'"));
        assert!(!stripped.contains("real comment"));
    }

    #[test]
    fn production_region_keeps_trailing_code_after_mod_tests() {
        let src = "fn prod_before() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { let needle_in_tests = \"{ } fn prod_after\"; assert!(needle_in_tests.len() > 0); }\n}\n\nfn prod_after() { trailing_needle(); }\n";
        let region = production_region(src).expect("gated mod tests must be found");
        assert!(region.contains("fn prod_before()"));
        assert!(
            region.contains("trailing_needle();"),
            "F8: production code AFTER the test module must stay in the region"
        );
        assert!(
            !region.contains("needle_in_tests"),
            "test-module content (incl. brace-bearing string literals) must be blanked"
        );
        assert_eq!(
            region.matches('\n').count(),
            src.matches('\n').count(),
            "newlines preserved so offsets stay line-aligned"
        );
    }

    #[test]
    fn production_region_requires_cfg_test_gate() {
        // An ungated `mod tests {` (e.g. mentioned in a string on its own
        // line) must not be treated as the production|tests seam.
        let src = "fn a() {}\nmod tests { fn not_gated() {} }\n";
        assert!(
            production_region(src).is_none(),
            "no #[cfg(test)]-gated mod tests → None (caller fails loud)"
        );
    }

    #[test]
    fn production_region_skips_string_mention_and_finds_real_module() {
        // The inner string literal contains a REAL newline followed by
        // `mod tests {`, and a comment right before it mentions
        // #[cfg(test)] — so BOTH seam preconditions are textually present
        // at the decoy, and only the byte classification (Str, not Code)
        // rejects it.
        let src = "fn a() {\n    // #[cfg(test)] decoy gate\n    let s = \"x\nmod tests { hidden_brace }\";\n    drop(s);\n}\n\n#[cfg(test)]\nmod tests {\n    fn t() { hidden_needle(); }\n}\n";
        let region = production_region(src).expect("the real gated module must be found");
        assert!(
            !region.contains("hidden_needle"),
            "the REAL test module must be excised even when a string literal \
             carries the seam text earlier"
        );
        assert!(
            region.contains("hidden_brace"),
            "the decoy string content is production-region bytes and must \
             survive untouched"
        );
        assert!(region.contains("fn a()"));
    }
}
