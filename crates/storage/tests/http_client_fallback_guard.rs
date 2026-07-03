//! HTTP-CLIENT-01 ratchet — no panic-class `reqwest::Client::new()` fallback
//! may reappear in `crates/storage/src/` (C2 fix, 2026-07-03).
//!
//! `Client::new()` PANICS when the client cannot be constructed (TLS backend
//! init / resolver init / fd exhaustion) — the exact conditions under which
//! the `ClientBuilder::build()` `Err` arm is reachable. A panic inside a
//! tokio task is a silent task death (the suspected mechanism behind the
//! 2026-07-03 10:35 IST SLO-publisher death). The C2 fix replaced all 8
//! `unwrap_or_else(|_| Client::new())` fallbacks with typed, panic-free
//! handling (`crates/storage/src/http_client.rs`); this guard fails the
//! build if the pattern returns.
//!
//! See: `.claude/rules/project/http-client-error-codes.md`.

#![cfg(test)]

use std::path::{Path, PathBuf};

fn crate_src_dir(crate_name: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/storage — hop to the sibling crate.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ dir")
        .join(crate_name)
        .join("src")
}

fn read_rust_files(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(read_rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            out.push((path, content));
        }
    }
    out
}

/// Strip the `//`-comment tail of a line so doc/inline comments explaining
/// WHY `Client::new()` is banned don't trip the guard. Rust string literals
/// in this crate never contain the banned patterns, so the crude split is
/// safe here — EXCEPT that a URL scheme separator (`://`, common in string
/// literals like `"http://host"`) must NOT be treated as a comment start:
/// a naive `split("//")` would truncate the scan at the URL and miss a
/// banned pattern appearing LATER on the same line (false negative). Only
/// a `//` NOT immediately preceded by `:` opens a comment.
fn code_portion(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let preceded_by_colon = i > 0 && bytes[i - 1] == b':';
            if !preceded_by_colon {
                return &line[..i];
            }
            // `://` — a URL scheme separator inside a string literal, not
            // a comment. Skip past it and keep scanning.
            i += 2;
        } else {
            i += 1;
        }
    }
    line
}

/// Collect `(path, line_no, line)` violations of `needle` in CODE (comments
/// stripped), honoring an explicit `// RATCHET-ALLOW:` escape on the line.
fn scan_for(dir: &Path, needle: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (path, content) in read_rust_files(dir) {
        for (idx, line) in content.lines().enumerate() {
            if line.contains("RATCHET-ALLOW:") {
                continue;
            }
            if code_portion(line).contains(needle) {
                violations.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
            }
        }
    }
    violations
}

/// No `Client::new()` OR `Client::default()` anywhere in storage src CODE —
/// both are the panic-class constructor (`Default` delegates to `new()` and
/// panics identically on TLS/resolver/fd init failure); every site must use
/// `ClientBuilder::build()` with typed error handling (or
/// `crate::http_client`).
#[test]
fn no_client_new_fallback_in_storage_src() {
    let mut violations = scan_for(&crate_src_dir("storage"), "Client::new()");
    violations.extend(scan_for(&crate_src_dir("storage"), "Client::default()"));
    assert!(
        violations.is_empty(),
        "HTTP-CLIENT-01 ratchet: `Client::new()` / `Client::default()` panic on \
         TLS/resolver/fd init failure — use crate::http_client (typed \
         HttpClientBuildError) instead.\n  {}",
        violations.join("\n  ")
    );
}

/// No `use reqwest::Client as <Alias>` anywhere in storage src — a rename
/// makes the needle scans above blind (`Alias::new()` would not match
/// `Client::new()`). Nothing legitimate needs the alias; verified zero
/// hits at C2 time (2026-07-03).
#[test]
fn no_reqwest_client_alias_in_storage_src() {
    let violations = scan_for(&crate_src_dir("storage"), "use reqwest::Client as ");
    assert!(
        violations.is_empty(),
        "HTTP-CLIENT-01 ratchet: aliasing `reqwest::Client` blinds the \
         `Client::new()`/`Client::default()` needle scans — import it \
         unaliased.\n  {}",
        violations.join("\n  ")
    );
}

/// Self-test for the guard's own comment stripper: a URL scheme separator
/// (`://`) inside a string literal must NOT truncate the scan (the pre-fix
/// `split(\"//\")` bug produced a false negative here), while a genuine
/// `//` comment must still be stripped.
#[test]
fn code_portion_scheme_separator_does_not_hide_banned_pattern() {
    // Banned pattern AFTER a URL literal on the same line — must be DETECTED.
    let line = r#"let x = "http://host"; Client::new()"#;
    assert!(
        code_portion(line).contains("Client::new()"),
        "stripper must not treat '://' as a comment start: {:?}",
        code_portion(line)
    );
    // Banned pattern inside a genuine comment — must NOT be detected.
    let comment = "// Client::new() in a comment";
    assert!(
        !code_portion(comment).contains("Client::new()"),
        "stripper must strip genuine comments: {:?}",
        code_portion(comment)
    );
    // Inline comment after code: code kept, comment stripped.
    let mixed = r#"let url = "https://a/b"; // Client::new() explanation"#;
    let kept = code_portion(mixed);
    assert!(kept.contains(r#""https://a/b""#), "{kept:?}");
    assert!(!kept.contains("Client::new()"), "{kept:?}");
}

/// No `unwrap_or_else(|_| Client` fallback anywhere in storage src — the
/// exact C2 bug shape (builder Err → panic-class fallback).
#[test]
fn no_unwrap_or_else_client_fallback() {
    let violations = scan_for(&crate_src_dir("storage"), "unwrap_or_else(|_| Client");
    assert!(
        violations.is_empty(),
        "HTTP-CLIENT-01 ratchet: `unwrap_or_else(|_| Client...)` reintroduces the \
         panic fallback the C2 fix removed.\n  {}",
        violations.join("\n  ")
    );
}

/// boot_probe.rs MUST keep using the shared OnceLock probe client — it runs
/// on the every-5s pool-watchdog tick and the every-10s SLO scheduler tick,
/// so a per-invocation client build (TLS + resolver init thousands of times
/// per session) is both wasteful and the fd-pressure amplifier behind the
/// original incident.
#[test]
fn boot_probe_uses_shared_client() {
    let src = std::fs::read_to_string(crate_src_dir("storage").join("boot_probe.rs"))
        .expect("boot_probe.rs must exist in crates/storage/src");
    assert!(
        src.contains("shared_probe_client"),
        "boot_probe.rs must build its HTTP client via \
         crate::http_client::shared_probe_client (built once, reused across \
         every probe invocation)"
    );
}

/// Report-scope extension: the same panic-class fallback must not exist in
/// `crates/core/src` or `crates/app/src` either. Verified clean at C2 time
/// (2026-07-03) — the 8 fallbacks were storage-only; this pin keeps the
/// sibling crates clean too.
#[test]
fn no_unwrap_or_else_client_fallback_in_core_or_app() {
    let mut violations = scan_for(&crate_src_dir("core"), "unwrap_or_else(|_| Client");
    violations.extend(scan_for(&crate_src_dir("app"), "unwrap_or_else(|_| Client"));
    assert!(
        violations.is_empty(),
        "HTTP-CLIENT-01 ratchet: panic-class Client fallback found outside \
         storage.\n  {}",
        violations.join("\n  ")
    );
}
