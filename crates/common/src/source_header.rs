//! Rust header disambiguation shared by the two source-audit tools.

pub(super) fn trim_header_whitespace(rest: &str) -> &str {
    rest.trim_start_matches(|ch| {
        matches!(
            ch,
            '\u{0009}'
                ..='\u{000d}'
                    | ' '
                    | '\u{0085}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{2028}'
                    | '\u{2029}'
        )
    })
}

/// Rust's shebang/attribute boundary uses Pattern_White_Space and non-doc
/// comments. The scan is O(header bytes), with no allocation or recursion.
pub(super) fn rust_inner_attribute_follows(mut rest: &str) -> bool {
    loop {
        rest = trim_header_whitespace(rest);
        if rest.starts_with('[') {
            return true;
        }
        if rest.starts_with("//") {
            if rest.starts_with("//!") || (rest.starts_with("///") && !rest.starts_with("////")) {
                return false;
            }
            let Some(newline) = rest.find('\n') else {
                return false;
            };
            rest = &rest[newline + 1..];
            continue;
        }
        if !rest.starts_with("/*")
            || rest.starts_with("/*!")
            || (rest.starts_with("/**") && !rest.starts_with("/***") && !rest.starts_with("/**/"))
        {
            return false;
        }
        let bytes = rest.as_bytes();
        let mut cursor = 2;
        let mut depth = 1usize;
        while cursor + 1 < bytes.len() {
            match &bytes[cursor..cursor + 2] {
                b"/*" => {
                    depth += 1;
                    cursor += 2;
                }
                b"*/" => {
                    depth -= 1;
                    cursor += 2;
                    if depth == 0 {
                        break;
                    }
                }
                _ => cursor += 1,
            }
        }
        if depth != 0 {
            return false;
        }
        rest = &rest[cursor..];
    }
}
