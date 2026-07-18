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
}
