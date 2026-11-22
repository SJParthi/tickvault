//! Every WebSocket dial in the workspace passes an explicit config (audit J4,
//! 2026-10-26) and goes through the keepalive dial (B2 review, 2026-11-13).
//!
//! `None` as the config argument of a `tokio_tungstenite::connect_async*`
//! call inherits tungstenite's defaults: a 64 MiB message, a 16 MiB frame and
//! an unbounded write buffer. `connection.rs` had a guard for this that scanned
//! only `connection.rs`, and the live order-update socket dialled with `None`
//! for months. This test scans every `.rs` file under `crates/*/src` and fails
//! the build on:
//!
//! - any library dial (`connect_async*`, `client_async*`) outside
//!   `crates/core/src/websocket/ws_dial.rs`, with or without a config, and
//!   any `use` line that imports one there (an alias would hide the call);
//! - a `dial_ws_tls_with_keepalive` call whose second argument (the config)
//!   is `None`.
//!
//! WHY THE LIBRARY DIAL IS REFUSED OUTSIDE `ws_dial.rs` (B2 review)
//! ===============================================================
//! The library dial opens its TCP socket with a plain connect that never sets
//! `SO_KEEPALIVE`, so the kernel keepalive backstop the box tunes does
//! nothing on it. Only `ws_dial::dial_ws_tls_with_keepalive` sets it. Until
//! this review the guard still accepted `connect_async_tls_with_config(req,
//! Some(cfg), ...)` anywhere, and the only check that pinned the keepalive
//! dial read just the two files of `ws_nagle_disabled_guard.rs`'s
//! `DIAL_SITES`. A new socket module dialling the library directly would have
//! passed every guard with keepalive off. `client_async*` is refused too: a
//! caller could open its own plain `TcpStream` and hand it in.
//!
//! Honest scope: a source scan. It cannot see a dial built another way (for
//! example a raw socket wrapped with `WebSocketStream::from_raw_socket`).
//!
//! Comment lines and string literals (an occurrence preceded by `"`) are
//! skipped, so the existing guards' assertion strings do not trip it.

use std::fs;
use std::path::{Path, PathBuf};

/// The one module allowed to call the library dial: it is the keepalive
/// dial itself.
const DIAL_MODULE: &str = "crates/core/src/websocket/ws_dial.rs";

/// Library dial name prefixes. Any identifier starting with one of these is a
/// library dial (`connect_async`, `connect_async_tls_with_config`,
/// `client_async`, `client_async_tls_with_config`, ...).
const LIBRARY_DIALS: [&str; 2] = ["connect_async", "client_async"];

/// Our keepalive dial. Its second argument is the WebSocket config.
const KEEPALIVE_DIAL: &str = "dial_ws_tls_with_keepalive";

/// Names the scan looks for.
const NEEDLES: [&str; 3] = ["connect_async", "client_async", KEEPALIVE_DIAL];

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

/// Whether `path` is the keepalive dial module.
fn is_dial_module(path: &Path) -> bool {
    path.to_string_lossy()
        .replace('\\', "/")
        .ends_with(DIAL_MODULE)
}

/// Every violation in `source`, as `line: reason`. `in_dial_module` is true
/// only for `ws_dial.rs`, the one file allowed to call the library dial.
fn violations_in(source: &str, in_dial_module: bool) -> Vec<String> {
    let mut found = Vec::new();
    for needle in NEEDLES {
        let mut search_from = 0;
        while let Some(rel) = source[search_from..].find(needle) {
            let at = search_from + rel;
            search_from = at + needle.len();

            let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
            let line_no = source[..at].matches('\n').count() + 1;
            let line = &source[line_start..];
            let line = line.split('\n').next().unwrap_or(line);
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
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

            if LIBRARY_DIALS.iter().any(|lib| ident.starts_with(lib)) {
                if in_dial_module {
                    continue;
                }
                // Outside ws_dial.rs any appearance outside a comment or a
                // string is refused: a call, an import (an alias hides the
                // call), or a function value.
                found.push(format!(
                    "{line_no}: `{ident}` is the library dial, which never sets \
                     SO_KEEPALIVE; dial through ws_dial::dial_ws_tls_with_keepalive"
                ));
                continue;
            }

            // Our keepalive dial: an import or a doc path is fine; a call
            // needs an explicit config.
            if trimmed.starts_with("use ") || ident != KEEPALIVE_DIAL {
                continue;
            }
            let Some(after_open) = rest.strip_prefix('(') else {
                continue;
            };
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
    }
    found
}

#[test]
fn every_websocket_dial_in_the_workspace_uses_the_keepalive_dial_with_a_config() {
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
    let mut dial_module_seen = false;
    let mut violations = Vec::new();
    for path in &files {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let in_dial_module = is_dial_module(path);
        dial_module_seen |= in_dial_module;
        if !in_dial_module {
            dials += source.matches("dial_ws_tls_with_keepalive(").count();
        }
        for violation in violations_in(&source, in_dial_module) {
            violations.push(format!("{}:{violation}", path.display()));
        }
    }
    // Non-vacuity: the dial module exists where the exemption names it, and
    // the two production dials (main feed + depth, order update) call it.
    assert!(
        dial_module_seen,
        "{DIAL_MODULE} was not in the scan; the exemption names a moved file"
    );
    assert!(
        dials >= 2,
        "expected at least the two production dials, saw {dials}"
    );
    assert!(
        violations.is_empty(),
        "WebSocket dials outside the keepalive dial, or without a config:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_guard_flags_a_none_config_on_the_keepalive_dial() {
    let bad = "let s = dial_ws_tls_with_keepalive(request, None, true, Some(c)).await;";
    assert_eq!(violations_in(bad, false).len(), 1);
    let multi = "let s =\n    dial_ws_tls_with_keepalive(\n        request,\n        None,\n        true,\n        Some(c),\n    );";
    assert_eq!(violations_in(multi, false).len(), 1);
    let good = "let s = dial_ws_tls_with_keepalive(request, config, true, Some(c)).await;";
    assert!(violations_in(good, false).is_empty());
}

#[test]
fn the_guard_flags_the_library_dial_outside_the_dial_module_even_with_a_config() {
    // B2 review (2026-11-13): an explicit config is no longer enough. This
    // exact call passed the guard before, with SO_KEEPALIVE off.
    let explicit =
        "let s = connect_async_tls_with_config(request, Some(config), true, Some(c)).await;";
    assert_eq!(violations_in(explicit, false).len(), 1);
    let none = "fn f() { let s = connect_async_tls_with_config(request, None, true, Some(c)); }";
    assert_eq!(violations_in(none, false).len(), 1);
    assert_eq!(
        violations_in("let s = connect_async(url).await;", false).len(),
        1
    );
    assert_eq!(
        violations_in("let s = connect_async_tls(url, c).await;", false).len(),
        1
    );
    assert_eq!(
        violations_in(
            "let s = connect_async_with_config(url, Some(cfg), true).await;",
            false
        )
        .len(),
        1
    );
    // A caller handing in its own plain TcpStream.
    let own_stream = "let s = tokio_tungstenite::client_async_tls_with_config(request, tcp, Some(cfg), None).await;";
    assert_eq!(violations_in(own_stream, false).len(), 1);
    assert_eq!(
        violations_in("let s = client_async(request, tcp).await;", false).len(),
        1
    );
    // An import, which an alias would use to hide the call.
    let alias = "use tokio_tungstenite::connect_async_tls_with_config as dial;";
    assert_eq!(violations_in(alias, false).len(), 1);
}

#[test]
fn the_guard_lets_the_dial_module_call_the_library() {
    let inside =
        "tokio_tungstenite::client_async_tls_with_config(request, stream, config, connector).await";
    assert!(violations_in(inside, true).is_empty());
    // The keepalive dial's config rule still applies there.
    let none = "dial_ws_tls_with_keepalive(request, None, true, Some(c))";
    assert_eq!(violations_in(none, true).len(), 1);
    assert!(is_dial_module(Path::new(
        "/x/tickvault/crates/core/src/websocket/ws_dial.rs"
    )));
    assert!(!is_dial_module(Path::new(
        "/x/tickvault/crates/core/src/websocket/connection.rs"
    )));
}

#[test]
fn the_guard_ignores_comments_strings_and_keepalive_imports() {
    let skipped = "use super::ws_dial::{DialedWs, dial_ws_tls_with_keepalive};\n\
                   // connect_async(url) in a comment\n\
                   assert!(!s.contains(\"connect_async(\"));\n\
                   assert!(!s.contains(\"client_async_tls_with_config(\"));\n\
                   //! every `connect_async*` call";
    assert!(
        violations_in(skipped, false).is_empty(),
        "{:?}",
        violations_in(skipped, false)
    );
}
