//! Every literal tag value in `deploy/aws/terraform/*.tf` must use only the
//! characters AWS accepts in a tag value.
//!
//! Regression: 2026-11-11. The one-off disk-delete role (Quote 56) carried
//! the tag `Purpose = "... leftovers (Quote 56)"`. Terraform's plan accepted
//! it, and IAM `CreateRole` refused it at APPLY time, after a 40-minute
//! plan and lambda build (run 43046930995): "Value at 'tags.5.member.value'
//! failed to satisfy constraint: Member must satisfy regular expression
//! pattern: [\p{L}\p{Z}\p{N}_.:/=+\-@]*". Brackets, commas, semicolons and
//! similar punctuation are not allowed. This test moves that failure from the
//! live apply to CI.
//!
//! Scope: string literals assigned inside a `tags = { ... }` (or
//! `tags_all`, `default_tags`) block. Interpolations (`${...}`) are removed
//! before the check, since their values are names built from allowed parts.
//! Letters and digits outside ASCII are accepted (the AWS pattern allows any
//! Unicode letter or number). Values over 256 characters also fail.

use std::fs;
use std::path::PathBuf;

/// AWS tag value limit, in characters.
const MAX_TAG_VALUE_CHARS: usize = 256;

fn terraform_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/aws/terraform")
}

/// True when every character is one AWS allows in a tag value.
fn tag_value_is_valid(value: &str) -> bool {
    value.chars().count() <= MAX_TAG_VALUE_CHARS
        && value.chars().all(|c| {
            c.is_alphabetic()
                || c.is_numeric()
                || c.is_whitespace()
                || matches!(c, '_' | '.' | ':' | '/' | '=' | '+' | '-' | '@')
        })
}

/// Removes `${...}` interpolations (nested braces counted).
fn strip_interpolations(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut depth = 1usize;
            for d in chars.by_ref() {
                match d {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The literal value of a `key = "value"` line, if the line is one.
fn literal_value(line: &str) -> Option<&str> {
    let (_, rhs) = line.split_once('=')?;
    let rhs = rhs.trim();
    let inner = rhs.strip_prefix('"')?;
    let end = inner.rfind('"')?;
    Some(&inner[..end])
}

/// Every (line number, raw value) assigned as a literal inside a tags block.
fn tag_values(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut depth_in_tags: Option<i32> = None;
    for (i, raw) in src.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        match depth_in_tags {
            None => {
                let head = line.split('=').next().unwrap_or("").trim();
                let opens_tags =
                    matches!(head, "tags" | "tags_all") || line.starts_with("default_tags");
                if opens_tags && line.contains('{') && !line.contains('}') {
                    depth_in_tags = Some(1);
                }
            }
            Some(depth) => {
                let opens = line.matches('{').count() as i32;
                let closes = line.matches('}').count() as i32;
                let interp = line.matches("${").count() as i32;
                let next = depth + (opens - interp) - (closes - interp);
                if depth == 1
                    && let Some(v) = literal_value(line)
                {
                    out.push((i + 1, v.to_string()));
                }
                depth_in_tags = if next <= 0 { None } else { Some(next) };
            }
        }
    }
    out
}

fn bad_values(src: &str) -> Vec<(usize, String)> {
    tag_values(src)
        .into_iter()
        .filter(|(_, v)| !tag_value_is_valid(&strip_interpolations(v)))
        .collect()
}

#[test]
fn every_terraform_tag_value_uses_only_aws_allowed_characters() {
    let mut files = 0usize;
    let mut values = 0usize;
    let mut bad = Vec::new();
    let entries = fs::read_dir(terraform_dir()).expect("terraform dir present");
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        files += 1;
        let src = fs::read_to_string(&path).expect("read tf");
        values += tag_values(&src).len();
        for (line, v) in bad_values(&src) {
            bad.push(format!(
                "{}:{line}: {v:?}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
            ));
        }
    }
    assert!(
        files > 20 && values > 20,
        "only {files} files / {values} tag values scanned; the scan is broken"
    );
    assert!(
        bad.is_empty(),
        "tag values AWS will refuse at apply time (allowed: letters, digits, spaces, \
         _ . : / = + - @; at most 256 characters):\n{}",
        bad.join("\n")
    );
}

/// The 2026-11-11 role, as it was when IAM refused it.
const BAD_TAG_FIXTURE: &str = r#"
resource "aws_iam_role" "x" {
  name = "x"
  tags = {
    Name    = "tv-${var.environment}-x"
    Purpose = "one-off delete of the four Quote 52 leftovers (Quote 56)"
  }
}
"#;

/// Punctuation outside a tags block is allowed and must not be scanned.
const NON_TAG_FIXTURE: &str = r#"
resource "aws_cloudwatch_metric_alarm" "a" {
  alarm_description = "has (brackets), commas; fine here"
  tags = {
    Name = "ok"
  }
  dimensions = {
    X = "(not a tag)"
  }
}
"#;

#[test]
fn the_scan_bites_on_the_2026_11_11_tag() {
    let bad = bad_values(BAD_TAG_FIXTURE);
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(bad[0].1.contains("(Quote 56)"));
}

#[test]
fn allowed_values_pass() {
    for v in [
        "tv-prod-disk-delete-2026-11-11",
        "one-off delete of the four Quote 52 leftovers - Quote 56",
        "a_b.c:d/e=f+g-h@i j",
        "",
        "café 12",
    ] {
        assert!(tag_value_is_valid(v), "{v:?}");
    }
    for v in ["a(b)", "a,b", "a;b", "a!b", "a'b", "a&b", "a#b", "a*b"] {
        assert!(!tag_value_is_valid(v), "{v:?}");
    }
    assert!(!tag_value_is_valid(&"a".repeat(257)));
    assert!(tag_value_is_valid(&"a".repeat(256)));
}

#[test]
fn interpolations_are_removed_before_the_check() {
    assert_eq!(strip_interpolations("tv-${var.environment}-x"), "tv--x");
    assert_eq!(strip_interpolations("${a(b, c)}"), "");
    assert_eq!(strip_interpolations("x${f({a = 1})}y"), "xy");
}

#[test]
fn values_outside_tags_blocks_are_not_scanned() {
    assert_eq!(tag_values(NON_TAG_FIXTURE).len(), 1);
    assert!(bad_values(NON_TAG_FIXTURE).is_empty());
}
