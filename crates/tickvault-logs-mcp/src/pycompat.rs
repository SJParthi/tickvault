//! Small CPython-semantics helpers so tool outputs match server.py
//! byte-for-byte (after the harness's documented normalization).

/// Python `s[:n]` — slice by UNICODE CHARACTERS, not bytes.
pub fn py_slice_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Python `s[-n:]` — last `n` characters.
pub fn py_tail_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let skip = count - n;
    match s.char_indices().nth(skip) {
        Some((idx, _)) => &s[idx..],
        None => s,
    }
}

/// Python `str.splitlines()` for the terminators that appear in real log
/// files: `\n`, `\r\n`, `\r`. (CPython also splits on `\v`, `\f`, `\x1c`,
/// `\x1d`, `\x1e`, `\x85`, ` `, ` ` — those never appear in the
/// tickvault log sinks; documented bounded-parity deviation.)
pub fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push(&s[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                out.push(&s[start..i]);
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        out.push(&s[start..]);
    }
    out
}

/// Python `line.rstrip("\n")` — strip trailing `\n` chars only.
// WIRING-EXEMPT: parity-shim helper landed dormant on main via #1644 (phase 2c, pre-cutover — call sites arrive with the MCP cutover PR); annotated 2026-07-18 because the local wiring guard diffs the whole merge range and flags main's own fn.
pub fn py_rstrip_newline(s: &str) -> &str {
    s.trim_end_matches('\n')
}

/// Python `int(x)` over a JSON value: numbers truncate toward zero,
/// strings parse as base-10 integers. Returns Err(msg) with a
/// `ValueError`-shaped message when it cannot coerce (the transcript
/// avoids these; the message shape is a documented deviation).
pub fn py_int(v: &serde_json::Value) -> Result<i64, String> {
    match v {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if let Some(f) = n.as_f64() {
                Ok(f.trunc() as i64)
            } else {
                Err(format!("invalid literal for int(): {n}"))
            }
        }
        serde_json::Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid literal for int() with base 10: '{s}'")),
        serde_json::Value::Bool(b) => Ok(i64::from(*b)),
        other => Err(format!("int() argument invalid: {other}")),
    }
}

/// `urllib.parse.quote(s)` with the default `safe="/"`: percent-encode the
/// UTF-8 bytes of `s`, leaving unreserved chars (ALPHA / DIGIT / `_.-~`)
/// and `/` literal. Uppercase hex, matching CPython.
pub fn py_urllib_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => {
                out.push(*b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// `fnmatch.fnmatch(name, pattern)` (posix, case-sensitive): `*` matches
/// any run, `?` one char, `[seq]` a char class (`[!seq]` negated).
pub fn py_fnmatch(name: &str, pattern: &str) -> bool {
    fn matches(n: &[char], p: &[char]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        match p[0] {
            '*' => {
                // Collapse consecutive stars.
                let rest = &p[1..];
                (0..=n.len()).any(|k| matches(&n[k..], rest))
            }
            '?' => !n.is_empty() && matches(&n[1..], &p[1..]),
            '[' => {
                if n.is_empty() {
                    return false;
                }
                // Find closing ']' (a ']' immediately after '[' or '[!' is
                // literal, per fnmatch/glob convention).
                let negate = p.len() > 1 && p[1] == '!';
                let class_start = if negate { 2 } else { 1 };
                let mut close = None;
                let mut j = class_start;
                while j < p.len() {
                    if p[j] == ']' && j > class_start {
                        close = Some(j);
                        break;
                    }
                    j += 1;
                }
                let Some(close) = close else {
                    // Unterminated class: '[' is literal.
                    return n[0] == '[' && matches(&n[1..], &p[1..]);
                };
                let class = &p[class_start..close];
                let mut hit = false;
                let mut k = 0;
                while k < class.len() {
                    if k + 2 < class.len() && class[k + 1] == '-' {
                        if n[0] >= class[k] && n[0] <= class[k + 2] {
                            hit = true;
                        }
                        k += 3;
                    } else {
                        if n[0] == class[k] {
                            hit = true;
                        }
                        k += 1;
                    }
                }
                if hit != negate {
                    matches(&n[1..], &p[close + 1..])
                } else {
                    false
                }
            }
            c => !n.is_empty() && n[0] == c && matches(&n[1..], &p[1..]),
        }
    }
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    matches(&n, &p)
}

/// Decode bytes as UTF-8 with Python's `errors="ignore"` — invalid byte
/// sequences are DROPPED (not replaced).
pub fn decode_utf8_ignore(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY-free: valid_up_to guarantees valid UTF-8 prefix.
                if let Ok(s) = std::str::from_utf8(&rest[..valid]) {
                    out.push_str(s);
                }
                let skip = e.error_len().unwrap_or(rest.len() - valid);
                rest = &rest[valid + skip..];
                if rest.is_empty() {
                    break;
                }
            }
        }
    }
    out
}

/// Decode bytes as UTF-8 with Python's `errors="replace"` (U+FFFD).
pub fn decode_utf8_replace(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Python `json.dumps(..., ensure_ascii=True)` (the DEFAULT) emulation —
/// review r8, 2026-07-18: post-process a serde_json dump so every char at
/// or above U+007F becomes CPython's `\uXXXX` escape (lowercase 4-digit
/// hex; astral chars as a UTF-16 surrogate PAIR: U+1D54A becomes
/// backslash-u d835 then backslash-u dd4a — both verified against
/// CPython 3.x `json.dumps`). CPython also escapes DEL (U+007F) as
/// backslash-u 007f, which serde leaves raw, so the boundary is
/// `>= 0x7f`, not strictly-greater.
///
/// Safe as a post-pass, never double-escaping: serde_json escapes only
/// `"`/`\`/controls < 0x20 (short forms `\"` `\\` `\b` `\f` `\n` `\r`
/// `\t`, else lowercase `\u00XX` — the SAME forms CPython emits), so
/// every escape sequence in serde output is pure ASCII < 0x7F, and any
/// char >= U+007F is always literal string content.
pub fn ensure_ascii(s: &str) -> String {
    if s.bytes().all(|b| b < 0x7f) {
        return s.to_string();
    }
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if (c as u32) < 0x7f {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                // Infallible: fmt::Write for String never errors.
                let _ignored = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_chars_is_unicode_aware() {
        assert_eq!(py_slice_chars("héllo", 2), "hé");
        assert_eq!(py_slice_chars("ab", 5), "ab");
        assert_eq!(py_slice_chars("", 3), "");
        let e200: String = "é".repeat(200);
        assert_eq!(py_slice_chars(&e200, 160).chars().count(), 160);
    }

    #[test]
    fn tail_chars_matches_python_negative_slice() {
        assert_eq!(py_tail_chars("abcdef", 3), "def");
        assert_eq!(py_tail_chars("ab", 5), "ab");
        assert_eq!(py_tail_chars("héllo", 4), "éllo");
    }

    #[test]
    fn splitlines_common_terminators() {
        assert_eq!(py_splitlines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\r\nb\rc"), vec!["a", "b", "c"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("x"), vec!["x"]);
    }

    #[test]
    fn int_coercion_matches_python() {
        use serde_json::json;
        assert_eq!(py_int(&json!(5)).unwrap(), 5);
        assert_eq!(py_int(&json!(5.9)).unwrap(), 5);
        assert_eq!(py_int(&json!(-5.9)).unwrap(), -5);
        assert_eq!(py_int(&json!("42")).unwrap(), 42);
        assert!(py_int(&json!("4.2")).is_err());
        assert_eq!(py_int(&json!(true)).unwrap(), 1);
    }

    #[test]
    fn urllib_quote_default_safe_slash() {
        assert_eq!(
            py_urllib_quote("SELECT count() FROM ticks"),
            "SELECT%20count%28%29%20FROM%20ticks"
        );
        assert_eq!(py_urllib_quote("a/b_c.d-e~f"), "a/b_c.d-e~f");
        assert_eq!(py_urllib_quote("é"), "%C3%A9");
    }

    #[test]
    fn fnmatch_basic() {
        assert!(py_fnmatch("server.py", "*.py"));
        assert!(!py_fnmatch("server.rs", "*.py"));
        assert!(py_fnmatch("a1", "a?"));
        assert!(py_fnmatch("ab.rs", "[ax]b.rs"));
        assert!(!py_fnmatch("bb.rs", "[!b]b.rs"));
        assert!(py_fnmatch("f3.log", "f[0-9].log"));
    }

    #[test]
    fn utf8_ignore_drops_invalid_bytes() {
        assert_eq!(decode_utf8_ignore(b"ab\xffcd"), "abcd");
        assert_eq!(decode_utf8_replace(b"ab\xffcd"), "ab\u{fffd}cd");
    }

    // Review r8 (2026-07-18): goldens below are CPython outputs, derived
    // by running `python3 -c "import json; print(json.dumps(...))"`
    // (CPython 3.x, 2026-07-18) and pasting the results verbatim.
    #[test]
    fn ensure_ascii_matches_python_json_dumps_golden() {
        // python3: json.dumps({"text": "ASCII é — 日本語 𝕊 \x01 end"},
        // indent=2) ==
        // '{\n  "text": "ASCII \\u00e9 \\u2014 \\u65e5\\u672c\\u8a9e
        //  \\ud835\\udd4a \\u0001 end"\n}'
        let v = serde_json::json!({"text": "ASCII \u{e9} \u{2014} \u{65e5}\u{672c}\u{8a9e} \u{1d54a} \u{1} end"});
        let dumped = ensure_ascii(&serde_json::to_string_pretty(&v).unwrap());
        assert_eq!(
            dumped,
            "{\n  \"text\": \"ASCII \\u00e9 \\u2014 \\u65e5\\u672c\\u8a9e \\ud835\\udd4a \\u0001 end\"\n}"
        );
    }

    #[test]
    fn ensure_ascii_del_and_boundary_chars() {
        // python3: json.dumps('~\x7f\x80\xa0') == '"~\\u007f\\u0080\\u00a0"'
        // — DEL (0x7f) IS escaped by CPython; serde leaves it raw.
        let v = serde_json::Value::String("~\u{7f}\u{80}\u{a0}".to_string());
        assert_eq!(
            ensure_ascii(&serde_json::to_string(&v).unwrap()),
            "\"~\\u007f\\u0080\\u00a0\""
        );
        // python3: json.dumps('𝕊') == '"\\ud835\\udd4a"' — lowercase
        // surrogate PAIR for an astral char.
        let astral = serde_json::Value::String("\u{1d54a}".to_string());
        assert_eq!(
            ensure_ascii(&serde_json::to_string(&astral).unwrap()),
            "\"\\ud835\\udd4a\""
        );
    }

    #[test]
    fn ensure_ascii_pure_ascii_passthrough_never_double_escapes() {
        // serde's own escapes (controls, quote, backslash) are all-ASCII
        // and identical to CPython's — python3: json.dumps('\x01\x1f"\\\t')
        // == '"\\u0001\\u001f\\"\\\\\\t"'. The post-pass must leave them
        // untouched, including an already-escaped backslash-u2014
        // 6-char literal sequence.
        let v = serde_json::Value::String("\u{1}\u{1f}\"\\\t".to_string());
        let dumped = serde_json::to_string(&v).unwrap();
        assert_eq!(dumped, "\"\\u0001\\u001f\\\"\\\\\\t\"");
        assert_eq!(ensure_ascii(&dumped), dumped);
        let already = "\"a\\u2014b\"";
        assert_eq!(ensure_ascii(already), already);
    }
}
