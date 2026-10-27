//! Every WebSocket dial in the workspace passes an explicit config (audit J4,
//! 2026-10-26).
//!
//! `None` as the config argument of a `tokio_tungstenite::connect_async*`
//! call inherits tungstenite's defaults: a 64 MiB message, a 16 MiB frame and
//! an unbounded write buffer. `connection.rs` had a guard for this that scanned
//! only `connection.rs`, and the live order-update socket dialled with `None`
//! for months. This test scans every `.rs` file under `crates/*/src` and fails
//! the build on:
//!
//! - a config-less helper (`connect_async(`, `connect_async_tls(`), or any
//!   other `connect_async*` call this guard does not know how to check;
//! - a `connect_async_with_config` / `connect_async_tls_with_config` call
//!   whose second argument (the config) is `None`.
//!
//! Comment lines and string literals (an occurrence preceded by `"`) are
//! skipped, so the existing guards' assertion strings do not trip it.

use std::fs;
use std::path::{Path, PathBuf};

/// `connect_async*` functions whose second argument is the config.
const CONFIG_TAKING: [&str; 2] = ["connect_async_with_config", "connect_async_tls_with_config"];

fn workspace_crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| panic!("crates dir above {}", env!("CARGO_MANIFEST_DIR")))
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Splits the argument list that starts right after an opening `(` at the
/// top level, returning the arguments and whether the closing `)` was found.
fn top_level_args(after_open: &str) -> Option<Vec<String>> {
    let mut depth = 0usize;
    let mut args = Vec::new();
    let mut current = String::new();
    for ch in after_open.chars() {
        match ch {
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' | '}' if depth == 0 => {
                args.push(current.trim().to_string());
                return Some(args);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    None
}

/// Every violation in `source`, as `line: reason`.
fn violations_in(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let needle = "connect_async";
    let mut search_from = 0;
    while let Some(rel) = source[search_from..].find(needle) {
        let at = search_from + rel;
        search_from = at + needle.len();

        let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
        let line_no = source[..at].matches('\n').count() + 1;
        let line = &source[line_start..];
        let line = line.split('\n').next().unwrap_or(line);
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("use ") {
            continue;
        }
        // Part of a longer identifier, or inside a string literal.
        let prev = source[..at].chars().next_back();
        if prev.is_some_and(|c| c == '"' || c == '_' || c.is_alphanumeric()) {
            continue;
        }
        let ident_len = source[at..]
            .find(|c: char| !(c == '_' || c.is_alphanumeric()))
            .unwrap_or(source.len() - at);
        let ident = &source[at..at + ident_len];
        let rest = source[at + ident_len..].trim_start();
        // `ident` named without a call (a path, a doc reference) is not a dial.
        let Some(after_open) = rest.strip_prefix('(') else {
            continue;
        };
        if !CONFIG_TAKING.contains(&ident) {
            found.push(format!(
                "{line_no}: `{ident}(` takes no WebSocket config, so it inherits the 64 MiB default"
            ));
            continue;
        }
        match top_level_args(after_open) {
            Some(args) if args.get(1).is_some_and(|config| config != "None") => {}
            Some(_) => found.push(format!(
                "{line_no}: `{ident}` is called with `None` (or no) config"
            )),
            None => found.push(format!(
                "{line_no}: `{ident}` call could not be parsed; pass an explicit config"
            )),
        }
    }
    found
}

#[test]
fn every_connect_async_call_in_the_workspace_passes_an_explicit_config() {
    let crates = workspace_crates_dir();
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(&crates) else {
        panic!("cannot list {}", crates.display());
    };
    for entry in entries.flatten() {
        let src = entry.path().join("src");
        if src.is_dir() {
            collect_rs_files(&src, &mut files);
        }
    }
    assert!(
        files.len() > 100,
        "the scan must reach the workspace: {} files",
        files.len()
    );

    let mut dials = 0usize;
    let mut violations = Vec::new();
    for path in &files {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        dials += source.matches("connect_async_tls_with_config(").count();
        for violation in violations_in(&source) {
            violations.push(format!("{}:{violation}", path.display()));
        }
    }
    // Non-vacuity: the two production dials (main feed + depth, order update)
    // are in the scanned set.
    assert!(
        dials >= 2,
        "expected at least the two production dials, saw {dials}"
    );
    assert!(
        violations.is_empty(),
        "WebSocket dials without an explicit config:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_guard_flags_a_none_config() {
    let src = "fn f() { let s = connect_async_tls_with_config(request, None, true, Some(c)); }";
    assert_eq!(violations_in(src).len(), 1);
    let multi = "let s =\n    connect_async_with_config(\n        request,\n        None,\n        true,\n    );";
    assert_eq!(violations_in(multi).len(), 1);
}

#[test]
fn the_guard_flags_the_config_less_helpers() {
    assert_eq!(violations_in("let s = connect_async(url).await;").len(), 1);
    assert_eq!(
        violations_in("let s = connect_async_tls(url, c).await;").len(),
        1
    );
}

#[test]
fn the_guard_accepts_an_explicit_config_and_ignores_comments_strings_and_imports() {
    let ok = "connect_async_tls_with_config(\n    request,\n    Some(order_update_websocket_config()),\n    true,\n    Some(tls_connector),\n)";
    assert!(violations_in(ok).is_empty(), "{:?}", violations_in(ok));
    let var = "connect_async_tls_with_config(request, config, true, Some(connector))";
    assert!(violations_in(var).is_empty());
    let skipped = "use tokio_tungstenite::connect_async_tls_with_config;\n\
                   // connect_async(url) in a comment\n\
                   assert!(!s.contains(\"connect_async(\"));\n\
                   //! every `connect_async*` call";
    assert!(
        violations_in(skipped).is_empty(),
        "{:?}",
        violations_in(skipped)
    );
}
