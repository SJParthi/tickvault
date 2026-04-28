//! Credential redaction for error messages and notifications.
//!
//! Prevents accidental leakage of secrets (client IDs, PINs, TOTP codes)
//! in log files, Telegram notifications, and SNS messages.
//!
//! # How It Works
//!
//! `redact_url_params` strips URL query parameters and known credential
//! patterns from arbitrary strings. It is regex-free — uses only
//! `str::find` and character scanning.
//!
//! `sanitize_audit_string` (Wave-2-D Item 9 hardening) is the single
//! choke-point used by every audit-table writer. It (a) caps length at
//! `MAX_AUDIT_STR_LEN`, (b) strips control characters (NUL / CR / LF) so
//! a hostile diagnostic cannot inject newlines into a SQL string literal
//! or break HTTP framing, and (c) doubles single quotes per QuestDB SQL
//! escape rules.
//!
//! # Usage
//!
//! Apply at error-creation sites (token_manager.rs) and at notification
//! boundaries (events.rs) for defense-in-depth.

/// Hard cap on any free-text column written to audit tables. 1 KiB is
/// well above the longest legitimate diagnostic (typically ≤ 256 chars)
/// and well below QuestDB's 32 KiB STRING limit.
pub const MAX_AUDIT_STR_LEN: usize = 1024;

/// Centralised audit-string sanitiser. Every audit-table writer
/// (`*_audit_persistence.rs`) MUST funnel free-text columns through this
/// helper before SQL interpolation.
///
/// Four guarantees:
/// 1. Output is bounded by `MAX_AUDIT_STR_LEN` (UTF-8 safe truncation).
/// 2. Control characters (`\0`, `\r`, `\n`, ASCII < 0x20 except space)
///    are removed — protects against SQL-string-literal newline
///    injection and HTTP framing breakage.
/// 3. Single quotes are doubled per QuestDB SQL escape rule.
/// 4. **Wave 3-C adversarial review (security-reviewer, 2026-04-28):**
///    Belt-and-suspenders strip of `;` and `--` so QuestDB's `/exec`
///    endpoint (which accepts multiple semicolon-separated statements
///    in one `query=` parameter) cannot be coerced into multi-statement
///    execution even if the single-quote doubling were ever bypassed
///    by a future bug. Defense in depth: single-quote doubling already
///    contains the practical attack today, but stripping the structural
///    SQL tokens means the function is safe by construction even if a
///    caller forgets to pass through it.
///
/// Input is treated as untrusted (may originate from a Dhan error body
/// or a raw WebSocket disconnect reason). Output is safe for direct
/// `'{escaped}'` interpolation in QuestDB SQL.
#[must_use]
pub fn sanitize_audit_string(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    // UTF-8 safe truncation by char (NOT byte) to avoid splitting
    // multi-byte sequences.
    let truncated: String = input.chars().take(MAX_AUDIT_STR_LEN).collect();
    let mut out = String::with_capacity(truncated.len());
    let mut prev_was_dash = false;
    for ch in truncated.chars() {
        let cp = ch as u32;
        match ch {
            // Strip ALL ASCII control chars except plain space —
            // newlines, tabs, NUL, etc. They have no place in a
            // one-line audit diagnostic and break SQL string literals.
            _ if cp < 0x20 => {
                prev_was_dash = false;
                continue;
            }
            // ASCII DELETE.
            _ if cp == 0x7f => {
                prev_was_dash = false;
                continue;
            }
            // C1 control characters (U+0080..U+009F).
            _ if (0x80..=0x9f).contains(&cp) => {
                prev_was_dash = false;
                continue;
            }
            // Wave-2-D adversarial review (MEDIUM): strip Unicode
            // direction-override and zero-width characters that
            // (a) can reverse-render Telegram text in a way that
            // conceals malicious payload (U+202E RIGHT-TO-LEFT
            // OVERRIDE), and (b) break grep/jq parsing of audit
            // logs (zero-width spaces, BOM).
            '\u{202a}'..='\u{202e}' => {
                prev_was_dash = false;
                continue;
            } // bidi overrides
            '\u{2066}'..='\u{2069}' => {
                prev_was_dash = false;
                continue;
            } // bidi isolates
            '\u{200b}'..='\u{200f}' => {
                prev_was_dash = false;
                continue;
            } // zero-widths + LRM/RLM
            '\u{feff}' => {
                prev_was_dash = false;
                continue;
            } // BOM
            // Wave 3-C: defense-in-depth strip of SQL-structural
            // tokens. `;` ends a statement; `--` opens a line
            // comment. QuestDB `/exec` accepts multi-statement
            // `query=`, so stripping `;` removes the multi-statement
            // attack class entirely. The double-dash check uses
            // `prev_was_dash` to also strip the second dash.
            ';' => {
                prev_was_dash = false;
                continue;
            }
            '-' if prev_was_dash => {
                // Two dashes in a row: drop both. The first dash was
                // already pushed; pop it.
                let _ = out.pop();
                prev_was_dash = false;
                continue;
            }
            '-' => {
                out.push('-');
                prev_was_dash = true;
            }
            // Single-quote doubling per QuestDB SQL escape.
            '\'' => {
                out.push_str("''");
                prev_was_dash = false;
            }
            // Standard char.
            c => {
                out.push(c);
                prev_was_dash = false;
            }
        }
    }
    out
}

/// Redacts URL query parameters and known credential patterns from a string.
///
/// Replaces query parameters in URLs (`?key=val&...`) with `?[REDACTED]`.
/// Also catches standalone `dhanClientId=`, `pin=`, `totp=` patterns
/// outside of URLs.
///
/// # Performance
///
/// Auth is cold path. String allocation is acceptable.
///
/// # Examples
///
/// ```
/// use tickvault_common::sanitize::redact_url_params;
///
/// let raw = "error for url (https://auth.dhan.co/app/gen?dhanClientId=123&pin=456)";
/// let safe = redact_url_params(raw);
/// assert!(!safe.contains("123"));
/// assert!(!safe.contains("456"));
/// assert!(safe.contains("?[REDACTED]"));
/// ```
pub fn redact_url_params(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }

    let mut result = redact_urls(input);
    result = redact_param_value(&result, "dhanClientId=");
    result = redact_param_value(&result, "pin=");
    result = redact_param_value(&result, "totp=");
    result
}

/// Replaces query parameters in URLs with `[REDACTED]`.
///
/// Scans for `https://` or `http://` prefixes, finds the `?` delimiter,
/// and replaces everything from `?` to the next whitespace, `)`, or
/// end-of-string with `?[REDACTED]`.
///
/// Uses `str::find` for UTF-8–safe scanning (no byte-level indexing).
fn redact_urls(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut remaining = input;

    while !remaining.is_empty() {
        // Find the earliest URL prefix
        let url_start = match (remaining.find("http://"), remaining.find("https://")) {
            (Some(h), Some(hs)) => h.min(hs),
            (Some(h), None) => h,
            (None, Some(hs)) => hs,
            (None, None) => {
                result.push_str(remaining);
                break;
            }
        };

        // Copy everything before the URL
        result.push_str(&remaining[..url_start]);
        remaining = &remaining[url_start..];

        // Find the end of the URL (whitespace, ')' or end-of-string).
        // URLs are ASCII-only, so non-ASCII chars also terminate the URL.
        let url_end = remaining
            .find(|c: char| c.is_whitespace() || c == ')')
            .unwrap_or(remaining.len());

        let url = &remaining[..url_end];

        // Check if URL has query params
        if let Some(q_pos) = url.find('?') {
            result.push_str(&url[..q_pos]);
            result.push_str("?[REDACTED]");
        } else {
            result.push_str(url);
        }

        remaining = &remaining[url_end..];
    }

    result
}

/// Redacts the value portion of a `key=value` pattern.
///
/// Replaces `key=<value>` with `key=[REDACTED]` where `<value>` extends
/// to the next `&`, whitespace, `)`, or end-of-string.
///
/// Uses `str::find` for UTF-8–safe scanning (no byte-level indexing).
fn redact_param_value(input: &str, key: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut search_from = 0;

    while let Some(pos) = input[search_from..].find(key) {
        let abs_pos = search_from.saturating_add(pos);
        // Copy everything before this match
        result.push_str(&input[search_from..abs_pos]);
        result.push_str(key);
        result.push_str("[REDACTED]");

        // Skip past the value using char-aware scanning
        let value_start = abs_pos.saturating_add(key.len());
        let value_end = input[value_start..]
            .find(|c: char| c == '&' || c.is_whitespace() || c == ')')
            .map(|i| value_start.saturating_add(i))
            .unwrap_or(input.len());

        search_from = value_end;
    }

    // Copy any remaining text after the last match
    result.push_str(&input[search_from..]);
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_string() {
        assert_eq!(redact_url_params(""), "");
    }

    #[test]
    fn test_no_url_no_params() {
        let input = "some plain error message";
        assert_eq!(redact_url_params(input), input);
    }

    #[test]
    fn test_url_without_query_params() {
        let input = "error for url (https://auth.dhan.co/app/generateAccessToken)";
        assert_eq!(redact_url_params(input), input);
    }

    #[test]
    fn test_url_with_query_params_redacted() {
        let input = "error sending request for url (https://auth.dhan.co/app/generateAccessToken?dhanClientId=1106656882&pin=785478&totp=782561)";
        let output = redact_url_params(input);
        assert!(
            !output.contains("1106656882"),
            "client ID must not appear: {output}"
        );
        assert!(!output.contains("785478"), "PIN must not appear: {output}");
        assert!(!output.contains("782561"), "TOTP must not appear: {output}");
        assert!(
            output.contains("?[REDACTED]"),
            "must contain redaction marker: {output}"
        );
        assert!(
            output.contains("generateAccessToken"),
            "URL path must be preserved: {output}"
        );
    }

    #[test]
    fn test_exact_reqwest_error_format() {
        // This is the exact format from the Telegram screenshot
        let input = "generateAccessToken request failed: error sending request for url (https://auth.dhan.co/app/generateAccessToken?dhanClientId=1106656882&pin=785478&totp=772509)";
        let output = redact_url_params(input);
        assert!(!output.contains("1106656882"));
        assert!(!output.contains("785478"));
        assert!(!output.contains("772509"));
        assert!(output.contains("generateAccessToken request failed"));
        assert!(output.contains("?[REDACTED]"));
    }

    #[test]
    fn test_multiple_urls_all_redacted() {
        let input = "tried https://a.com/x?secret=123 then https://b.com/y?key=456";
        let output = redact_url_params(input);
        assert!(!output.contains("123"), "first URL param leaked: {output}");
        assert!(!output.contains("456"), "second URL param leaked: {output}");
    }

    #[test]
    fn test_url_at_end_of_string() {
        let input = "error at https://api.com/path?token=secret123";
        let output = redact_url_params(input);
        assert!(
            !output.contains("secret123"),
            "param at end leaked: {output}"
        );
        assert!(output.contains("?[REDACTED]"));
    }

    #[test]
    fn test_http_url_also_redacted() {
        let input = "url (http://insecure.com/api?key=abc123)";
        let output = redact_url_params(input);
        assert!(
            !output.contains("abc123"),
            "http URL param leaked: {output}"
        );
    }

    #[test]
    fn test_standalone_dhan_client_id_redacted() {
        let input = "response contained dhanClientId=1106656882 in body";
        let output = redact_url_params(input);
        assert!(
            !output.contains("1106656882"),
            "standalone client ID leaked: {output}"
        );
        assert!(output.contains("dhanClientId=[REDACTED]"));
    }

    #[test]
    fn test_standalone_pin_redacted() {
        let input = "error: pin=785478 was invalid";
        let output = redact_url_params(input);
        assert!(
            !output.contains("785478"),
            "standalone PIN leaked: {output}"
        );
        assert!(output.contains("pin=[REDACTED]"));
    }

    #[test]
    fn test_standalone_totp_redacted() {
        let input = "TOTP rejected: totp=772509";
        let output = redact_url_params(input);
        assert!(
            !output.contains("772509"),
            "standalone TOTP leaked: {output}"
        );
        assert!(output.contains("totp=[REDACTED]"));
    }

    #[test]
    fn test_preserves_url_path() {
        let input = "error for url (https://auth.dhan.co/app/generateAccessToken?dhanClientId=123)";
        let output = redact_url_params(input);
        assert!(
            output.contains("/app/generateAccessToken"),
            "URL path lost: {output}"
        );
    }

    #[test]
    fn test_preserves_surrounding_text() {
        let input = "prefix https://x.com/a?b=c suffix";
        let output = redact_url_params(input);
        assert!(output.starts_with("prefix "), "prefix lost: {output}");
        assert!(output.ends_with(" suffix"), "suffix lost: {output}");
    }

    #[test]
    fn test_multibyte_utf8_em_dash_does_not_panic() {
        // Exact format from NotificationEvent — contains `—` (em dash, 3 bytes).
        let input = "attempt 1: Dhan authentication failed: generateAccessToken request failed: error sending request for url (https://auth.dhan.co/app/generateAccessToken?[REDACTED]) — retrying in 0s";
        let output = redact_url_params(input);
        assert!(output.contains("—"), "em dash must be preserved: {output}");
        assert!(
            output.contains("?[REDACTED]"),
            "URL must still be redacted: {output}"
        );
    }

    #[test]
    fn test_multibyte_utf8_around_url() {
        // Non-ASCII characters before and after a URL
        let input = "errör at https://x.com/api?key=secret — done";
        let output = redact_url_params(input);
        assert!(!output.contains("secret"), "param leaked: {output}");
        assert!(output.contains("errör"), "leading UTF-8 lost: {output}");
        assert!(output.contains("— done"), "trailing UTF-8 lost: {output}");
    }

    #[test]
    fn test_dhan_auth_full_error_with_body() {
        let input = "Dhan authentication failed: generateAccessToken request failed: error sending request for url (https://auth.dhan.co/app/generateAccessToken?dhanClientId=1106656882&pin=785478&totp=772509)";
        let output = redact_url_params(input);
        assert!(!output.contains("1106656882"));
        assert!(!output.contains("785478"));
        assert!(!output.contains("772509"));
        assert!(output.contains("Dhan authentication failed"));
    }

    #[test]
    fn test_both_http_and_https_urls_in_same_string() {
        // Exercises the (Some(h), Some(hs)) branch at line 64 where
        // both http:// and https:// are found in the remaining string.
        let input =
            "tried http://example.com/api?secret=abc then https://auth.dhan.co/app?token=xyz";
        let output = redact_url_params(input);
        assert!(
            !output.contains("secret=abc"),
            "http params leaked: {output}"
        );
        assert!(
            !output.contains("token=xyz"),
            "https params leaked: {output}"
        );
        assert!(output.contains("tried"), "prefix lost: {output}");
        assert!(output.contains("then"), "middle text lost: {output}");
    }

    #[test]
    fn test_http_before_https_redacts_both() {
        // http:// appears first — the min(h, hs) path picks it
        let input = "see http://a.com?k=v and https://b.com?s=t end";
        let output = redact_url_params(input);
        assert!(!output.contains("k=v"), "http param leaked: {output}");
        assert!(!output.contains("s=t"), "https param leaked: {output}");
    }

    #[test]
    fn test_https_before_http_redacts_both() {
        // https:// appears first — the min(h, hs) path picks it
        let input = "see https://b.com?s=t and http://a.com?k=v end";
        let output = redact_url_params(input);
        assert!(!output.contains("s=t"), "https param leaked: {output}");
        assert!(!output.contains("k=v"), "http param leaked: {output}");
    }

    // -----------------------------------------------------------------
    // sanitize_audit_string (Wave-2-D Item 9 hardening)
    // -----------------------------------------------------------------

    #[test]
    fn test_sanitize_audit_string_empty_returns_empty() {
        assert_eq!(sanitize_audit_string(""), "");
    }

    #[test]
    fn test_sanitize_audit_string_doubles_single_quote() {
        let out = sanitize_audit_string("RELIANCE's pre-open close was missing");
        assert!(out.contains("''"));
        assert!(!out.contains("RELIANCE's"));
    }

    #[test]
    fn test_sanitize_audit_string_strips_newlines() {
        let out = sanitize_audit_string("line1\nline2\rline3\0end");
        assert!(!out.contains('\n'));
        assert!(!out.contains('\r'));
        assert!(!out.contains('\0'));
        assert_eq!(out, "line1line2line3end");
    }

    #[test]
    fn test_sanitize_audit_string_strips_control_chars() {
        // ASCII bell (0x07) and tab (0x09) — both removed.
        let out = sanitize_audit_string("a\x07b\tc");
        assert_eq!(out, "abc");
    }

    #[test]
    fn test_sanitize_audit_string_preserves_space() {
        let out = sanitize_audit_string("hello world");
        assert_eq!(out, "hello world");
    }

    #[test]
    fn test_sanitize_audit_string_preserves_utf8() {
        let out = sanitize_audit_string("errör — done");
        assert!(out.contains("errör"));
        assert!(out.contains("—"));
    }

    #[test]
    fn test_sanitize_audit_string_truncates_to_max_len() {
        let huge = "x".repeat(MAX_AUDIT_STR_LEN * 2);
        let out = sanitize_audit_string(&huge);
        assert_eq!(out.chars().count(), MAX_AUDIT_STR_LEN);
    }

    #[test]
    fn test_sanitize_audit_string_truncation_is_utf8_char_boundary() {
        // 4-byte char (😀 = U+1F600). Build input where truncation
        // cap would fall mid-multi-byte if byte-wise.
        let mut huge = String::new();
        for _ in 0..(MAX_AUDIT_STR_LEN * 2) {
            huge.push('😀');
        }
        let out = sanitize_audit_string(&huge);
        // Truncation done by char count, so result is still valid UTF-8
        // and all chars are 😀.
        assert_eq!(out.chars().count(), MAX_AUDIT_STR_LEN);
        for ch in out.chars() {
            assert_eq!(ch, '😀');
        }
    }

    #[test]
    fn test_sanitize_audit_string_strips_ascii_delete() {
        let out = sanitize_audit_string("a\x7fb");
        assert_eq!(out, "ab");
    }

    #[test]
    fn test_sanitize_audit_string_strips_c1_controls() {
        // U+0080..U+009F C1 controls — invisible in many fonts but
        // breakable in some terminals.
        let mut input = String::from("hello");
        input.push('\u{0080}');
        input.push('\u{0085}'); // NEL — newline-equivalent in some renderers
        input.push('\u{009F}');
        input.push('w');
        input.push('o');
        input.push('r');
        input.push('l');
        input.push('d');
        let out = sanitize_audit_string(&input);
        assert_eq!(out, "helloworld");
    }

    #[test]
    fn test_sanitize_audit_string_strips_bidi_override() {
        // Wave-2-D adversarial review (MEDIUM) — U+202E RIGHT-TO-LEFT
        // OVERRIDE is the classic "Trojan Source" attack vector.
        let evil = "innocent\u{202e} -- malicious";
        let out = sanitize_audit_string(evil);
        assert!(!out.contains('\u{202e}'), "bidi override must be stripped");
        // Surrounding text preserved.
        assert!(out.contains("innocent"));
        assert!(out.contains("malicious"));
    }

    #[test]
    fn test_sanitize_audit_string_strips_zero_width_chars() {
        // ZWSP (U+200B), ZWNJ (U+200C), ZWJ (U+200D), LRM (U+200E), RLM (U+200F)
        let mut input = String::from("a");
        input.push('\u{200b}');
        input.push('\u{200c}');
        input.push('\u{200d}');
        input.push('\u{200e}');
        input.push('\u{200f}');
        input.push('b');
        let out = sanitize_audit_string(&input);
        assert_eq!(out, "ab");
    }

    #[test]
    fn test_sanitize_audit_string_strips_bom() {
        let input = "\u{feff}hello";
        let out = sanitize_audit_string(input);
        assert_eq!(out, "hello");
    }

    #[test]
    fn test_sanitize_audit_string_blocks_sql_newline_injection() {
        // Adversarial: attacker-controlled diagnostic with embedded SQL.
        let evil = "innocent\n', 0); DROP TABLE order_audit; --";
        let out = sanitize_audit_string(evil);
        assert!(!out.contains('\n'), "newline must be stripped");
        // Single quotes are doubled, breaking the injected literal.
        assert!(out.contains("''"));
        // The attempt to break out of the SQL string literal therefore
        // yields a still-quoted (and quote-doubled) literal — safe.
    }

    #[test]
    fn test_max_audit_str_len_is_1024() {
        // Pin the constant so a future "let's bump to 64KB" PR is
        // forced to update tests + dashboards together.
        assert_eq!(MAX_AUDIT_STR_LEN, 1024);
    }

    #[test]
    fn test_sanitize_audit_string_strips_semicolon_strict() {
        // Wave 3-C adversarial review (security-reviewer, 2026-04-28):
        // belt-and-suspenders SQL-structural strip.
        let out = sanitize_audit_string("a;b;c");
        assert_eq!(out, "abc");
    }

    #[test]
    fn test_sanitize_audit_string_strips_double_dash_strict() {
        let out = sanitize_audit_string("a--b");
        assert_eq!(out, "ab", "both dashes of `--` must be removed");
    }

    #[test]
    fn test_sanitize_audit_string_preserves_single_dash() {
        let out = sanitize_audit_string("ATM-strike-25");
        assert_eq!(
            out, "ATM-strike-25",
            "single dash legitimately appears in symbol/strike strings"
        );
    }

    #[test]
    fn test_sanitize_audit_string_strips_triple_dash_first_pair() {
        // `---` reads as `--` then `-`. The pair is stripped, the
        // trailing single dash is preserved.
        let out = sanitize_audit_string("a---b");
        assert_eq!(
            out, "a-b",
            "first two dashes form `--`, stripped; trailing dash retained"
        );
    }

    #[test]
    fn test_sanitize_audit_string_combined_quote_semicolon_dash_attack() {
        // Wave 3-C: quote-escape AND structural-strip both close the
        // door on a triple-vector payload.
        let evil = "ok'); DROP TABLE x; --";
        let out = sanitize_audit_string(evil);
        assert!(out.contains("''"), "quote doubled");
        assert!(!out.contains(';'), "semicolons stripped");
        assert!(!out.contains("--"), "comment markers stripped");
    }
}
