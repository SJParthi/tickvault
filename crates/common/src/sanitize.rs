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
//! # Usage
//!
//! Apply at error-creation sites (token_manager.rs) and at notification
//! boundaries (events.rs) for defense-in-depth.

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
/// use dhan_live_trader_common::sanitize::redact_url_params;
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
fn redact_urls(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Look for URL start: "http://" or "https://"
        if i + 7 < len && &input[i..i + 7] == "http://" {
            // Found http:// — find the `?` and redact
            let url_start = i;
            i = copy_url_and_redact_params(input, url_start, &mut result);
        } else if i + 8 < len && &input[i..i + 8] == "https://" {
            let url_start = i;
            i = copy_url_and_redact_params(input, url_start, &mut result);
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    result
}

/// Copies a URL up to `?`, then replaces query params with `[REDACTED]`.
/// Returns the index after the URL ends.
fn copy_url_and_redact_params(input: &str, start: usize, result: &mut String) -> usize {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = start;

    // Copy everything up to `?` or end-of-URL
    while i < len {
        let ch = bytes[i] as char;
        if ch == '?' {
            // Found query string — redact everything after `?`
            result.push_str("?[REDACTED]");
            i += 1;
            // Skip until whitespace, `)`, or end-of-string
            while i < len {
                let skip_ch = bytes[i] as char;
                if skip_ch.is_whitespace() || skip_ch == ')' {
                    break;
                }
                i += 1;
            }
            return i;
        }
        if ch.is_whitespace() || ch == ')' {
            // End of URL without query params — nothing to redact
            return i;
        }
        result.push(ch);
        i += 1;
    }

    i
}

/// Redacts the value portion of a `key=value` pattern.
///
/// Replaces `key=<value>` with `key=[REDACTED]` where `<value>` extends
/// to the next `&`, whitespace, `)`, or end-of-string.
fn redact_param_value(input: &str, key: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut search_from = 0;

    while let Some(pos) = input[search_from..].find(key) {
        let abs_pos = search_from + pos;
        // Copy everything before this match
        result.push_str(&input[search_from..abs_pos]);
        result.push_str(key);
        result.push_str("[REDACTED]");

        // Skip past the value
        let value_start = abs_pos + key.len();
        let mut value_end = value_start;
        let bytes = input.as_bytes();
        while value_end < bytes.len() {
            let ch = bytes[value_end] as char;
            if ch == '&' || ch.is_whitespace() || ch == ')' || ch == '\n' {
                break;
            }
            value_end += 1;
        }
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
    fn test_dhan_auth_full_error_with_body() {
        let input = "Dhan authentication failed: generateAccessToken request failed: error sending request for url (https://auth.dhan.co/app/generateAccessToken?dhanClientId=1106656882&pin=785478&totp=772509)";
        let output = redact_url_params(input);
        assert!(!output.contains("1106656882"));
        assert!(!output.contains("785478"));
        assert!(!output.contains("772509"));
        assert!(output.contains("Dhan authentication failed"));
    }
}
