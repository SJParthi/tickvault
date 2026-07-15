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

/// Hard cap on any SYMBOL value written via QuestDB ILP. Audit-2026-05-03
/// H3: every legitimate symbol/category/segment/phase value is ≤ ~32
/// bytes today; 256 bytes is generous and well below QuestDB ILP's
/// implicit limits. Caps prevent an unbounded upstream label (e.g. a
/// malformed CSV row, hostile payload) from producing an oversized ILP
/// row that the server accepts as garbage or rejects entirely.
pub const ILP_SYMBOL_MAX_BYTES: usize = 256;

/// Canonical ILP SYMBOL sanitiser — single source of truth for every
/// movers/audit/persistence writer that calls `Buffer::symbol(...)` or
/// `Buffer::column_str(...)` on potentially untrusted input.
///
/// Three guarantees:
///
/// 1. **Strip ILP-structural delimiters**: `\n`, `\r`, `,`, `=`, plus
///    every ASCII control character. The upstream `questdb-rs` crate
///    rejects some of these but the precise rejected-character set has
///    drifted across library versions; strip defensively before they
///    reach the buffer.
///
/// 2. **UTF-8-safe length cap** at `ILP_SYMBOL_MAX_BYTES`. Truncates at
///    the last char boundary ≤ the cap so multi-byte UTF-8 sequences
///    (e.g. `é`, `😀`) are never split mid-codepoint.
///
/// 3. **Borrow-friendly**: returns `Cow::Borrowed(input)` on the common
///    clean+within-cap path (zero allocation). `Cow::Owned(_)` only when
///    sanitisation actually happened.
///
/// Pure function, O(n) over input length, runs at the cold ILP-build
/// cadence (typically ≤ a few dozen rows/second) so allocation on the
/// dirty path is acceptable.
///
/// # Examples
///
/// ```
/// use tickvault_common::sanitize::sanitize_ilp_symbol;
///
/// // Clean input is borrowed (zero alloc).
/// let clean = "NIFTY";
/// match sanitize_ilp_symbol(clean) {
///     std::borrow::Cow::Borrowed(s) => assert_eq!(s, "NIFTY"),
///     std::borrow::Cow::Owned(_) => panic!("clean input should not allocate"),
/// }
///
/// // Embedded comma is stripped.
/// let dirty = "RELIANCE,INDUSTRIES";
/// let out = sanitize_ilp_symbol(dirty);
/// assert_eq!(out, "RELIANCEINDUSTRIES");
/// ```
#[must_use]
pub fn sanitize_ilp_symbol(input: &str) -> std::borrow::Cow<'_, str> {
    let needs_strip = input
        .chars()
        .any(|c| c == '\n' || c == '\r' || c == ',' || c == '=' || c.is_control());
    let needs_truncate = input.len() > ILP_SYMBOL_MAX_BYTES;
    if !needs_strip && !needs_truncate {
        return std::borrow::Cow::Borrowed(input);
    }
    let cleaned: String = input
        .chars()
        .filter(|c| !(*c == '\n' || *c == '\r' || *c == ',' || *c == '=' || c.is_control()))
        .collect();
    // Truncate to ILP_SYMBOL_MAX_BYTES respecting UTF-8 char boundaries.
    let bounded = if cleaned.len() > ILP_SYMBOL_MAX_BYTES {
        let mut end = ILP_SYMBOL_MAX_BYTES;
        while end > 0 && !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned[..end].to_string()
    } else {
        cleaned
    };
    std::borrow::Cow::Owned(bounded)
}

/// Sanitiser for a value bound to a QuestDB **ILP STRING column**
/// (`Buffer::column_str`), as opposed to a SYMBOL tag.
///
/// Why a third sanitiser:
/// * [`sanitize_audit_string`] targets SQL string literals — it strips `;`/`--`
///   and DOUBLES single quotes. Both are WRONG for ILP (no SQL quoting; the
///   `questdb-rs` encoder escapes the value itself), and quote-doubling would
///   corrupt the stored text.
/// * [`sanitize_ilp_symbol`] strips the ILP TAG delimiters `,` and `=`. That is
///   correct for a SYMBOL (tag) but WRONG for a STRING field: commas/equals are
///   literal-safe inside an ILP quoted string, and stripping them corrupts JSON
///   payloads (e.g. the audit `field_deltas` `{"lot_size":[1000,200]}`).
///
/// This helper preserves `,` `=` `'` `;` `-` `"` verbatim — `questdb-rs`
/// `column_str` escapes the genuinely ILP-special `"` / `\` / newline itself —
/// while still stripping ASCII/C1 control chars and Unicode BiDi-override /
/// zero-width / BOM characters (display-spoofing + grep/jq breakage, per the
/// Wave-2-D audit), and caps length at [`MAX_AUDIT_STR_LEN`].
#[must_use]
pub fn sanitize_ilp_string(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    input
        .chars()
        .take(MAX_AUDIT_STR_LEN)
        .filter(|ch| {
            let cp = *ch as u32;
            // Keep everything EXCEPT control / C1 / BiDi / zero-width / BOM.
            !(cp < 0x20
                || cp == 0x7f
                || (0x80..=0x9f).contains(&cp)
                || ('\u{202a}'..='\u{202e}').contains(ch)
                || ('\u{2066}'..='\u{2069}').contains(ch)
                || ('\u{200b}'..='\u{200f}').contains(ch)
                || *ch == '\u{feff}')
        })
        .collect()
}

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

/// Hard cap on a captured REST error-response body (DHAN-REST-400,
/// 2026-06-10). Dhan's error envelope (`errorType` / `errorCode` /
/// `errorMessage`) fits comfortably in 300 chars; a WAF HTML block page is
/// truncated to its identifying prefix.
pub const REST_BODY_CAPTURE_MAX_CHARS: usize = 300;

/// Widened cap for the once-per-day RAW-BODY SAMPLE of a 2xx-but-empty
/// Dhan charts response (2026-07-14 — the account-condition vs
/// envelope-drift discriminator: ≥14 days of 2xx responses carrying zero
/// parseable candles for every SID while option-chain + WS work, and no
/// 2xx body was ever logged anywhere). 600 chars shows the full columnar
/// envelope shape (or its absence / an HTML shell prefix) while staying a
/// bounded one-line log field. Same redaction pipeline as the 300-char
/// error capture.
pub const REST_RAW_BODY_SAMPLE_MAX_CHARS: usize = 600;

/// Minimum run length of JWT-alphabet characters after `eyJ` for a substring
/// to be treated as a JWT and redacted. Real Dhan JWTs are hundreds of chars;
/// 20 keeps false positives (e.g. the literal word "eyJ" in prose) implausible
/// while never letting a real token through.
const JWT_MIN_TAIL_LEN: usize = 20;

/// Replaces any JWT-shaped substring (`eyJ` followed by ≥ 20 chars of the
/// base64url/JWT alphabet `[A-Za-z0-9._-]`) with `[REDACTED-JWT]`.
///
/// Defence for REST error bodies that echo the `access-token` header back
/// (some gateways/WAFs do). Regex-free char scan, cold path.
#[must_use]
pub fn redact_jwt_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find("eyJ") {
        let (before, candidate) = rest.split_at(pos);
        out.push_str(before);
        let tail = &candidate[3..];
        // `%` included so a (partially) percent-encoded token cannot split
        // the run and leak a fragment (security review 2026-06-10, MEDIUM).
        let run_len = tail
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' || c == '%')
            })
            .unwrap_or(tail.len());
        if run_len >= JWT_MIN_TAIL_LEN {
            out.push_str("[REDACTED-JWT]");
            rest = &tail[run_len..];
        } else {
            out.push_str("eyJ");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Bounded, secret-redacted capture of a REST error-response body for error
/// logs (DHAN-REST-400 root-cause visibility, 2026-06-10).
///
/// Pipeline: URL-param / credential redaction ([`redact_url_params`]) →
/// JWT redaction ([`redact_jwt_like`]) → control-char strip (one-line log
/// field; no newline injection) → UTF-8-safe truncation to
/// [`REST_BODY_CAPTURE_MAX_CHARS`].
///
/// Guarantee (ratchet-tested): no access-token substring can survive into the
/// output. Dhan's `errorType` / `errorCode` / `errorMessage` fields DO survive
/// — they are the entire point of the capture.
#[must_use]
pub fn capture_rest_error_body(body: &str) -> String {
    capture_rest_body_bounded(body, REST_BODY_CAPTURE_MAX_CHARS)
}

/// Sibling of [`capture_rest_error_body`] with the WIDENED
/// [`REST_RAW_BODY_SAMPLE_MAX_CHARS`] bound — the once-per-day raw-body
/// sample of a 2xx-but-empty Dhan charts response (Dhan-support evidence;
/// see the constant's doc for the 2026-07-14 rationale). IDENTICAL
/// redaction pipeline: no access-token substring can survive.
#[must_use]
pub fn capture_rest_raw_body_sample(body: &str) -> String {
    capture_rest_body_bounded(body, REST_RAW_BODY_SAMPLE_MAX_CHARS)
}

/// Shared bounded pipeline behind [`capture_rest_error_body`] (300) and
/// [`capture_rest_raw_body_sample`] (600). Redaction order is
/// load-bearing — see the ORDER MATTERS comment below.
fn capture_rest_body_bounded(body: &str, max_chars: usize) -> String {
    if body.is_empty() {
        return String::new();
    }
    // ORDER MATTERS: strip control chars FIRST. A token split by an embedded
    // control char ("eyJab\ncdef…") would evade the JWT scan (short tail at
    // the control char) and then be re-joined into a leakable contiguous
    // string if stripping ran after redaction. Truncation runs LAST so a
    // token can never be half-cut into an unredacted prefix.
    let stripped: String = body.chars().filter(|c| !c.is_control()).collect();
    let mut redacted = redact_jwt_like(&redact_url_params(&stripped));
    // Non-JWT credential shapes (security review 2026-06-10, HIGH): a server
    // echoing an opaque (non-`eyJ`) credential inside a JSON field must
    // still be caught. Redact the VALUE of known credential field names.
    // `auth_token` added 2026-07-09 (Groww reject-loop hardening security
    // review, LOW): the NATS CONNECT frame carries the credential under
    // exactly this JSON key — belt-and-suspenders parity with the Python
    // sidecar's own exact-value redaction.
    for key in [
        "accessToken",
        "access_token",
        "refreshToken",
        "app_secret",
        "auth_token",
    ] {
        redacted = redact_json_string_field(&redacted, key);
    }
    redacted.chars().take(max_chars).collect()
}

/// Replaces the string VALUE of every `"<key>" : "<value>"` occurrence with
/// `[REDACTED]`. Regex-free scan; tolerant of whitespace around the colon.
/// Non-string values (numbers, null) are left untouched.
fn redact_json_string_field(input: &str, key: &str) -> String {
    let needle = format!("\"{key}\"");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(&needle) {
        let after_key = pos + needle.len();
        out.push_str(&rest[..after_key]);
        rest = &rest[after_key..];
        // Skip whitespace, one colon, whitespace.
        let trimmed = rest.trim_start();
        let Some(after_colon) = trimmed.strip_prefix(':') else {
            continue;
        };
        let value_part = after_colon.trim_start();
        let Some(value_body) = value_part.strip_prefix('"') else {
            continue;
        };
        // Copy the structural chars we just consumed, then the redaction.
        let consumed_len = rest.len() - value_body.len();
        out.push_str(&rest[..consumed_len]);
        out.push_str("[REDACTED]");
        // Skip the original value up to the closing quote (no escapes in
        // token material; a backslash would end the redacted span early but
        // never RE-EXPOSE anything — the value chars are skipped, not kept).
        let value_end = value_body.find('"').unwrap_or(value_body.len());
        rest = &value_body[value_end..];
    }
    out.push_str(rest);
    out
}

/// Redacts URL query parameters and known credential patterns from a string.
///
/// Replaces query parameters in URLs (`?key=val&...`) with `?[REDACTED]` for
/// every scheme in [`REDACT_URL_SCHEMES`] (http/https AND ws/wss — the live
/// feed URL is `wss://`). Also catches standalone credential params
/// (`token=`, `clientId=`, `dhanClientId=`, `pin=`, `totp=`, `authType=`)
/// outside of URLs as defense-in-depth.
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
///
/// // wss:// feed URL with the 24h JWT must also be fully redacted.
/// let ws = "TLS error: wss://api-feed.dhan.co?version=2&token=eyJabc&clientId=99&authType=2";
/// let safe_ws = redact_url_params(ws);
/// assert!(!safe_ws.contains("eyJabc"));
/// assert!(!safe_ws.contains("clientId=99"));
/// ```
pub fn redact_url_params(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }

    let mut result = redact_urls(input);
    // Standalone (non-URL) credential params — defense-in-depth for log lines
    // that print a bare `token=...` without a recognized URL scheme.
    result = redact_param_value(&result, "token=");
    result = redact_param_value(&result, "clientId=");
    result = redact_param_value(&result, "authType=");
    result = redact_param_value(&result, "dhanClientId=");
    result = redact_param_value(&result, "pin=");
    result = redact_param_value(&result, "totp=");
    result
}

/// Every URL scheme whose query string may carry a secret. `wss://`/`ws://`
/// are CRITICAL: the Dhan live-feed URL is
/// `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2`,
/// and a TLS/handshake error string embeds that full URL — without `wss://`
/// here, the 24h JWT would pass through unredacted into CloudWatch/app.log.
const REDACT_URL_SCHEMES: [&str; 4] = ["https://", "http://", "wss://", "ws://"];

/// Replaces query parameters in URLs with `[REDACTED]`.
///
/// Scans for any scheme in [`REDACT_URL_SCHEMES`], finds the `?` delimiter,
/// and replaces everything from `?` to the next whitespace, `)`, or
/// end-of-string with `?[REDACTED]`.
///
/// Uses `str::find` for UTF-8–safe scanning (no byte-level indexing).
fn redact_urls(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut remaining = input;

    while !remaining.is_empty() {
        // Find the earliest URL prefix across ALL secret-bearing schemes.
        let url_start = match REDACT_URL_SCHEMES
            .iter()
            .filter_map(|scheme| remaining.find(scheme))
            .min()
        {
            Some(pos) => pos,
            None => {
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

    /// Regression: 2026-06-12 — `redact_urls` only matched `http(s)://`, so a
    /// TLS/handshake error embedding the live-feed `wss://...?token=<JWT>` URL
    /// leaked the 24h Dhan JWT verbatim into CloudWatch/app.log via the new WS
    /// disconnect/reconnect tracing logs. Both the scheme gap AND the missing
    /// `token=`/`clientId=` param fallbacks are covered here.
    #[test]
    fn test_redact_url_params_redacts_wss_feed_url_with_jwt() {
        let jwt = fake_jwt();
        let raw = format!(
            "Tls(HandshakeFailure) for wss://api-feed.dhan.co?version=2&token={jwt}&clientId=1106656882&authType=2"
        );
        let safe = redact_url_params(&raw);
        // The whole JWT must be gone.
        assert!(!safe.contains(&jwt), "JWT leaked: {safe}");
        // No 20-char window of the JWT survives.
        let jwt_bytes = jwt.as_bytes();
        for window in jwt_bytes.windows(20) {
            let frag = std::str::from_utf8(window).unwrap();
            assert!(
                !safe.contains(frag),
                "JWT fragment leaked: {frag} in {safe}"
            );
        }
        // The client id value must be gone too.
        assert!(!safe.contains("1106656882"), "clientId leaked: {safe}");
        // And the redaction marker must be present (the query string was hit).
        assert!(safe.contains("[REDACTED]"), "no redaction marker: {safe}");
    }

    /// `ws://` (non-TLS) scheme is also covered.
    #[test]
    fn test_redact_url_params_redacts_plain_ws_scheme() {
        let safe = redact_url_params("dial ws://host?token=eyJsecret&authType=2 failed");
        assert!(!safe.contains("eyJsecret"), "token leaked: {safe}");
        assert!(safe.contains("?[REDACTED]"), "query not redacted: {safe}");
    }

    /// Standalone (non-URL) `token=` / `clientId=` are redacted by the param
    /// fallback even with no recognized URL scheme present.
    #[test]
    fn test_redact_url_params_redacts_standalone_token_and_client_id() {
        let safe = redact_url_params("auth failed token=eyJraw clientId=42 done");
        assert!(!safe.contains("eyJraw"), "standalone token leaked: {safe}");
        assert!(
            !safe.contains("clientId=42"),
            "standalone clientId leaked: {safe}"
        );
    }

    /// Existing http(s):// redaction must STILL work (no regression).
    #[test]
    fn test_redact_url_params_still_redacts_https() {
        let safe = redact_url_params(
            "error for url (https://auth.dhan.co/app/gen?dhanClientId=123&pin=456)",
        );
        assert!(!safe.contains("123"));
        assert!(!safe.contains("456"));
        assert!(safe.contains("?[REDACTED]"));
    }

    // -----------------------------------------------------------------
    // capture_rest_error_body + redact_jwt_like (DHAN-REST-400, 2026-06-10)
    // -----------------------------------------------------------------

    /// A realistic JWT shape (3 base64url segments) for the redaction ratchet.
    fn fake_jwt() -> String {
        format!(
            "eyJ{}.eyJ{}.{}",
            "hbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "zdWIiOiIxMTA2NjU2ODgyIiwiZXhwIjoxNzgwMDAwMDAwfQ",
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
        )
    }

    /// RATCHET (task DHAN-REST-400 item 1): no token substring may EVER
    /// appear in a captured body. Feeds a body that echoes the full JWT and
    /// asserts neither the token nor any 20-char window of it survives.
    #[test]
    fn test_capture_rest_error_body_redacts_jwt() {
        let jwt = fake_jwt();
        let body = format!(
            r#"{{"errorType":"Invalid_Authentication","errorCode":"DH-901","errorMessage":"token {jwt} rejected"}}"#
        );
        let out = capture_rest_error_body(&body);
        assert!(!out.contains(&jwt), "full JWT leaked: {out}");
        for start in 0..jwt.len().saturating_sub(20) {
            let window = &jwt[start..start + 20];
            assert!(
                !out.contains(window),
                "JWT window leaked: {window} in {out}"
            );
        }
        assert!(out.contains("[REDACTED-JWT]"), "marker missing: {out}");
    }

    #[test]
    fn test_capture_rest_error_body_preserves_dhan_error_fields() {
        let body = r#"{"errorType":"Invalid_Request","errorCode":"DH-905","errorMessage":"Missing required fields"}"#;
        let out = capture_rest_error_body(body);
        assert!(out.contains("Invalid_Request"));
        assert!(out.contains("DH-905"));
        assert!(out.contains("Missing required fields"));
    }

    #[test]
    fn test_capture_rest_error_body_truncates_to_300_chars() {
        let waf_page = "<html>blocked</html>".repeat(100);
        let out = capture_rest_error_body(&waf_page);
        assert_eq!(out.chars().count(), REST_BODY_CAPTURE_MAX_CHARS);
        assert!(out.starts_with("<html>blocked"));
    }

    #[test]
    fn test_capture_rest_raw_body_sample_truncates_to_600_chars() {
        let big = "0123456789".repeat(200);
        let out = capture_rest_raw_body_sample(&big);
        assert_eq!(out.chars().count(), REST_RAW_BODY_SAMPLE_MAX_CHARS);
        assert!(out.starts_with("0123456789"));
    }

    /// The widened sample runs the SAME redaction pipeline: a JWT echoed in
    /// a 2xx body can never survive into the 600-char sample.
    #[test]
    fn test_capture_rest_raw_body_sample_redacts_jwt() {
        let jwt = fake_jwt();
        let body = format!(r#"{{"open":[],"note":"token {jwt} echoed"}}"#);
        let out = capture_rest_raw_body_sample(&body);
        assert!(!out.contains(&jwt), "full JWT leaked: {out}");
        assert!(out.contains("[REDACTED-JWT]"), "marker missing: {out}");
    }

    /// A short benign columnar body passes through unmodified (the whole
    /// point of the sample — the envelope shape must survive).
    #[test]
    fn test_capture_rest_raw_body_sample_passes_columnar_envelope_through() {
        let body = r#"{"open":[],"high":[],"low":[],"close":[],"volume":[],"timestamp":[]}"#;
        assert_eq!(capture_rest_raw_body_sample(body), body);
    }

    #[test]
    fn test_capture_rest_error_body_utf8_boundary_safe() {
        let body = "😀".repeat(REST_BODY_CAPTURE_MAX_CHARS * 2);
        let out = capture_rest_error_body(&body);
        assert_eq!(out.chars().count(), REST_BODY_CAPTURE_MAX_CHARS);
        assert!(out.chars().all(|c| c == '😀'));
    }

    #[test]
    fn test_capture_rest_error_body_strips_control_chars() {
        let out = capture_rest_error_body("line1\nline2\r\tline3\0end");
        assert!(!out.contains('\n'));
        assert!(!out.contains('\r'));
        assert!(!out.contains('\t'));
        assert!(!out.contains('\0'));
        assert_eq!(out, "line1line2line3end");
    }

    #[test]
    fn test_capture_rest_error_body_composes_url_param_redaction() {
        let body =
            "retry url https://auth.dhan.co/app/gen?dhanClientId=1106656882&pin=785478 failed";
        let out = capture_rest_error_body(body);
        assert!(!out.contains("1106656882"), "client ID leaked: {out}");
        assert!(!out.contains("785478"), "PIN leaked: {out}");
        assert!(out.contains("?[REDACTED]"));
    }

    #[test]
    fn test_capture_rest_error_body_empty_is_empty() {
        assert_eq!(capture_rest_error_body(""), "");
    }

    /// RATCHET (order-of-operations hole, found in self-review 2026-06-10):
    /// a token split by an embedded control char must NOT be re-joined into
    /// a leakable contiguous string. Control-strip runs BEFORE redaction.
    #[test]
    fn test_capture_rest_error_body_redacts_control_char_split_jwt() {
        let jwt = fake_jwt();
        let (head, tail) = jwt.split_at(8);
        let body = format!("token {head}\n{tail} rejected");
        let out = capture_rest_error_body(&body);
        assert!(!out.contains(&jwt), "re-joined JWT leaked: {out}");
        for start in 0..jwt.len().saturating_sub(20) {
            let window = &jwt[start..start + 20];
            assert!(
                !out.contains(window),
                "JWT window leaked: {window} in {out}"
            );
        }
    }

    #[test]
    fn test_redact_jwt_like_replaces_token() {
        let jwt = fake_jwt();
        let out = redact_jwt_like(&format!("token={jwt} end"));
        assert!(!out.contains(&jwt));
        assert!(out.contains("[REDACTED-JWT]"));
        assert!(out.ends_with(" end"));
    }

    /// RATCHET (security review 2026-06-10, HIGH): an OPAQUE (non-`eyJ`)
    /// credential echoed in a known JSON token field must still be redacted.
    #[test]
    fn test_capture_rest_error_body_redacts_opaque_token_json_fields() {
        let body = r#"{"errorCode":"DH-901","accessToken":"opaque-key-9f8e7d6c5b4a3210","access_token": "another-opaque-value-123456","refreshToken":"rt-aabbccddeeff00112233","app_secret":"shh-secret-value-998877"}"#;
        let out = capture_rest_error_body(body);
        for leaked in [
            "opaque-key-9f8e7d6c5b4a3210",
            "another-opaque-value-123456",
            "rt-aabbccddeeff00112233",
            "shh-secret-value-998877",
        ] {
            assert!(
                !out.contains(leaked),
                "credential leaked: {leaked} in {out}"
            );
        }
        assert!(out.contains("DH-901"), "error field must survive: {out}");
        assert!(out.contains("[REDACTED]"));
    }

    /// RATCHET (security review 2026-06-10, MEDIUM): a percent-encoded JWT
    /// fragment must not split the redaction run and leak.
    #[test]
    fn test_redact_jwt_like_handles_percent_encoded_run() {
        let body = "token eyJhbGciOiJIUzI1%2BNiIsInR5cCI6IkpXVCJ9 end";
        let out = redact_jwt_like(body);
        assert!(!out.contains("hbGciOiJIUzI1"), "encoded JWT leaked: {out}");
        assert!(out.contains("[REDACTED-JWT]"));
    }

    #[test]
    fn test_redact_jwt_like_keeps_short_eyj_prose() {
        // "eyJ" with a short tail is not a token — must survive verbatim.
        let input = "the prefix eyJabc is not a token";
        assert_eq!(redact_jwt_like(input), input);
    }

    #[test]
    fn test_redact_jwt_like_multiple_tokens_all_redacted() {
        let jwt = fake_jwt();
        let out = redact_jwt_like(&format!("a {jwt} b {jwt} c"));
        assert!(!out.contains(&jwt));
        assert_eq!(out.matches("[REDACTED-JWT]").count(), 2);
    }

    #[test]
    fn test_rest_body_capture_max_chars_is_300() {
        // Pin the constant (task: bounded ≤300 chars).
        assert_eq!(REST_BODY_CAPTURE_MAX_CHARS, 300);
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
    fn test_sanitize_ilp_string_empty_returns_empty() {
        assert_eq!(sanitize_ilp_string(""), "");
    }

    #[test]
    fn test_sanitize_ilp_string_preserves_json_commas_and_punctuation() {
        // The audit `field_deltas` JSON MUST survive intact — commas/colons/
        // brackets/quotes are literal-safe inside an ILP STRING field. (The
        // SYMBOL sanitiser would strip the commas and corrupt the JSON.)
        let json = r#"{"lot_size":[1000,200],"tick_size":[0.05,0.01]}"#;
        assert_eq!(sanitize_ilp_string(json), json);
        // Single quotes are NOT doubled (that's the SQL sanitiser's job).
        assert_eq!(sanitize_ilp_string("O'Brien & Co"), "O'Brien & Co");
        // Equals + semicolons preserved (literal-safe in a quoted string).
        assert_eq!(sanitize_ilp_string("a=b; c-d"), "a=b; c-d");
    }

    #[test]
    fn test_sanitize_ilp_string_strips_control_and_bidi() {
        // Control chars, BiDi override, zero-width, BOM are stripped.
        let dirty = "ABC\n\t\u{202e}DEF\u{200b}\u{feff}";
        assert_eq!(sanitize_ilp_string(dirty), "ABCDEF");
    }

    #[test]
    fn test_sanitize_ilp_string_truncates_to_max_len() {
        let long = "x".repeat(MAX_AUDIT_STR_LEN + 50);
        assert_eq!(
            sanitize_ilp_string(&long).chars().count(),
            MAX_AUDIT_STR_LEN
        );
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

    // -----------------------------------------------------------------
    // sanitize_ilp_symbol (Audit-2026-05-03 H3 — canonical ILP sanitiser)
    // -----------------------------------------------------------------

    #[test]
    fn test_sanitize_ilp_symbol_clean_input_borrows_zero_alloc() {
        let out = sanitize_ilp_symbol("NIFTY");
        assert_eq!(out, "NIFTY");
        let borrowed = matches!(out, std::borrow::Cow::Borrowed(_));
        assert!(borrowed, "clean input must not allocate");
    }

    #[test]
    fn test_sanitize_ilp_symbol_strips_newline() {
        let out = sanitize_ilp_symbol("NIFTY\n50");
        assert_eq!(out, "NIFTY50");
    }

    #[test]
    fn test_sanitize_ilp_symbol_strips_carriage_return() {
        let out = sanitize_ilp_symbol("BANK\rNIFTY");
        assert_eq!(out, "BANKNIFTY");
    }

    #[test]
    fn test_sanitize_ilp_symbol_strips_comma() {
        let out = sanitize_ilp_symbol("RELIANCE,INDUSTRIES");
        assert_eq!(out, "RELIANCEINDUSTRIES");
    }

    #[test]
    fn test_sanitize_ilp_symbol_strips_equals() {
        let out = sanitize_ilp_symbol("a=b=c");
        assert_eq!(out, "abc");
    }

    #[test]
    fn test_sanitize_ilp_symbol_strips_control_chars() {
        let out = sanitize_ilp_symbol("a\x00b\x07c\x1fd");
        assert_eq!(out, "abcd");
    }

    #[test]
    fn test_sanitize_ilp_symbol_preserves_unicode() {
        let clean = "ETF—NIFTY50";
        let out = sanitize_ilp_symbol(clean);
        assert_eq!(out, "ETF—NIFTY50");
    }

    #[test]
    fn test_sanitize_ilp_symbol_empty_string_borrowed() {
        let out = sanitize_ilp_symbol("");
        assert_eq!(out, "");
        let borrowed = matches!(out, std::borrow::Cow::Borrowed(_));
        assert!(borrowed, "empty input must not allocate");
    }

    #[test]
    fn test_sanitize_ilp_symbol_caps_at_max_bytes() {
        let huge = "x".repeat(ILP_SYMBOL_MAX_BYTES * 2);
        let out = sanitize_ilp_symbol(&huge);
        assert_eq!(out.len(), ILP_SYMBOL_MAX_BYTES);
    }

    #[test]
    fn test_sanitize_ilp_symbol_cap_truncation_respects_utf8_boundary() {
        // Build a string of 4-byte characters (😀 = U+1F600). If the
        // truncation cap fell mid-multi-byte sequence, the resulting
        // String would panic on UTF-8 validation. Verify it doesn't.
        let mut input = String::new();
        for _ in 0..(ILP_SYMBOL_MAX_BYTES * 2 / 4) {
            input.push('😀');
        }
        let out = sanitize_ilp_symbol(&input);
        // Output is valid UTF-8 (no panic), and bytes ≤ cap.
        assert!(out.len() <= ILP_SYMBOL_MAX_BYTES);
        // Every char must still be 😀 (4 bytes each).
        for ch in out.chars() {
            assert_eq!(ch, '😀');
        }
    }

    #[test]
    fn test_sanitize_ilp_symbol_cap_walks_back_to_char_boundary() {
        // 3-byte chars ('₹' = U+20B9): 256 % 3 == 1, so the byte cap lands
        // MID-character and the boundary walk must step `end` back — the
        // complement of the 4-byte test above where 256 % 4 == 0 lands
        // exactly on a boundary and the walk never runs.
        let input = "₹".repeat(100); // 300 bytes > ILP_SYMBOL_MAX_BYTES
        let out = sanitize_ilp_symbol(&input);
        assert_eq!(out.len(), ILP_SYMBOL_MAX_BYTES - 1); // 255 = 85 chars × 3 B
        assert!(out.chars().all(|c| c == '₹'));
    }

    #[test]
    fn test_sanitize_audit_string_strips_bidi_isolates() {
        // U+2066 LRI ..= U+2069 PDI (bidi ISOLATES — distinct from the
        // U+202A..U+202E override range) must be stripped: they can
        // reverse-render Telegram text and break grep/jq on audit logs.
        let out = sanitize_audit_string("a\u{2066}b\u{2067}c\u{2068}d\u{2069}e");
        assert_eq!(out, "abcde");
    }

    #[test]
    fn test_redact_json_field_key_without_colon_left_untouched() {
        // The key token appearing WITHOUT a following colon is not a JSON
        // field assignment — nothing may be redacted and nothing dropped.
        let input = "{\"accessToken\" \"abc\"}";
        let out = redact_json_string_field(input, "accessToken");
        assert_eq!(out, input);
    }

    #[test]
    fn test_redact_json_field_non_string_value_left_untouched() {
        // Documented contract: non-string values (numbers, null) are left
        // untouched — only string values are replaced with [REDACTED].
        let input = "{\"accessToken\": 12345}";
        let out = redact_json_string_field(input, "accessToken");
        assert_eq!(out, input);
    }

    #[test]
    fn test_ilp_symbol_max_bytes_is_256() {
        // Pin the constant so a future "let's bump to 1KB" PR is
        // forced to update tests + dashboards together.
        assert_eq!(ILP_SYMBOL_MAX_BYTES, 256);
    }
}
