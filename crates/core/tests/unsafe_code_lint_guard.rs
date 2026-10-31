//! Audit CR-5 (2026-10-11): `unsafe` is denied by the COMPILER, not only by
//! the commit-time text scan in `banned-pattern-scanner.sh`.
//!
//! Every crate root (each `lib.rs`, `main.rs` and `bin/*.rs` under
//! `crates/*/src`) carries `#![cfg_attr(not(test), deny(unsafe_code))]`, so
//! production code that writes `unsafe` fails to build. The production
//! opt-outs are the `affinity` module in `crates/core/src/websocket/reader_runtime.rs`
//! (the two CPU-affinity syscalls, each block bounds-checked with a SAFETY
//! note) and, since 2026-10-12 (deep audit DA-12), the `statvfs_probe` module
//! in `crates/storage/src/disk_health_watcher.rs` (one `statvfs` call that
//! replaced a forked `df` process on the rescue free-space probe). Test code (Rust 2024 `env::set_var` in `#[cfg(test)]` modules) is
//! outside the lint by the `cfg_attr(not(test), ...)` form.
//!
//! These two tests stop the lint being removed quietly, and stop a second
//! opt-out being added without this file changing with it.
//!
//! **Corrected 2026-10-11 (review of CR-5).** The opt-out scan first matched
//! only the text `allow(unsafe_code` / `expect(unsafe_code`, so a list with
//! another lint first (`#[allow(dead_code, unsafe_code)]`, or the same inside
//! `cfg_attr`) or a `warn(unsafe_code)` downgrade slipped past it. It now
//! flags any `allow`, `expect` or `warn` list that names the `unsafe_code`
//! token, across lines, and `unsafe_code_opt_out_detector_bites` proves it.
//! **Not covered by the deny:** `build.rs` files (only `crates/common/build.rs`
//! today) are build-script crate roots and do not carry the deny line; the
//! opt-out scan still reads them, so an opt-out added there fails this test.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves") // APPROVED: test
}

/// Every crate root under `crates/*/src`: `lib.rs`, `main.rs`, `bin/*.rs`.
fn crate_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let crates = fs::read_dir(root.join("crates")).expect("crates/ is readable"); // APPROVED: test
    for entry in crates.flatten() {
        let src = entry.path().join("src");
        for name in ["lib.rs", "main.rs"] {
            let p = src.join(name);
            if p.is_file() {
                roots.push(p);
            }
        }
        if let Ok(bins) = fs::read_dir(src.join("bin")) {
            for bin in bins.flatten() {
                let p = bin.path();
                if p.extension().is_some_and(|e| e == "rs") {
                    roots.push(p);
                }
            }
        }
    }
    roots.sort();
    roots
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }
}

#[test]
fn every_crate_root_denies_unsafe_code() {
    let root = workspace_root();
    let roots = crate_roots(&root);
    // 8 lib/main crates plus the app and lambda binaries: far more than 10.
    assert!(
        roots.len() >= 10,
        "found only {} crate roots, the walk is broken",
        roots.len()
    );
    let missing: Vec<String> = roots
        .iter()
        .filter(|p| {
            let body = fs::read_to_string(p).unwrap_or_default();
            !body
                .lines()
                .any(|l| l.trim() == "#![cfg_attr(not(test), deny(unsafe_code))]")
        })
        .map(|p| p.strip_prefix(&root).unwrap_or(p).display().to_string())
        .collect();
    assert!(
        missing.is_empty(),
        "these crate roots do not deny unsafe_code; add \
         `#![cfg_attr(not(test), deny(unsafe_code))]` next to the other deny lines: {missing:?}"
    );
}

#[test]
fn allow_unsafe_code_appears_only_on_the_listed_modules() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    let mut sites = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&root).unwrap_or(&file).to_path_buf();
        // Only source trees: tests/benches/examples are separate crates that
        // the root lint does not cover, and this file names the pattern.
        if !rel.components().any(|c| c.as_os_str() == "src") {
            continue;
        }
        let body = fs::read_to_string(&file).unwrap_or_default();
        record_opt_outs(&rel.display().to_string(), &body, &mut sites);
    }
    // build.rs files are crate roots of their own build scripts, which the
    // root lint does not cover (see the module header); an opt-out there is
    // still recorded, so one cannot be added without this test changing.
    if let Ok(crates) = fs::read_dir(root.join("crates")) {
        for entry in crates.flatten() {
            let build = entry.path().join("build.rs");
            if let Ok(body) = fs::read_to_string(&build) {
                let rel = build.strip_prefix(&root).unwrap_or(&build).to_path_buf();
                record_opt_outs(&rel.display().to_string(), &body, &mut sites);
            }
        }
    }
    sites.sort();
    assert_eq!(
        sites,
        vec![
            (
                "crates/core/src/websocket/reader_runtime.rs".to_string(),
                "mod affinity {".to_string()
            ),
            (
                "crates/storage/src/disk_health_watcher.rs".to_string(),
                "mod statvfs_probe {".to_string()
            ),
            (
                "crates/storage/src/memory_watchdog.rs".to_string(),
                "mod trim_call {".to_string()
            ),
        ],
        "the only production opt-outs from deny(unsafe_code) are the affinity, \
         statvfs_probe and trim_call modules; a new one needs its own SAFETY notes and this test \
         updated in the same change"
    );
}

/// Records `(file, first non-empty line after the attribute)` for every
/// `unsafe_code` opt-out in `body`, read with `//` comments stripped.
fn record_opt_outs(rel: &str, body: &str, sites: &mut Vec<(String, String)>) {
    let stripped: Vec<&str> = body
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect();
    let code = stripped.join("\n");
    for at in unsafe_code_opt_outs(&code) {
        // The attribute ends at the next `]`; the item it applies to is the
        // first non-empty line after that.
        let close = code[at..].find(']').map_or(code.len(), |r| at + r);
        let close_line = code[..close].matches('\n').count();
        let next = stripped
            .iter()
            .skip(close_line + 1)
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");
        sites.push((rel.to_string(), next.to_string()));
    }
}

/// Does `code` (comment-stripped source) hold an attribute that turns the
/// `unsafe_code` lint down? True for `allow`, `expect` and `warn` lists that
/// name the token anywhere in the list (`#[allow(dead_code, unsafe_code)]`,
/// `#![cfg_attr(x, allow(clippy::y, unsafe_code))]`), false for `deny` and
/// `forbid`. Returns the byte offset of each opt-out's `unsafe_code` token.
fn unsafe_code_opt_outs(code: &str) -> Vec<usize> {
    const TOKEN: &str = "unsafe_code";
    let bytes = code.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = code[from..].find(TOKEN) {
        let at = from + rel;
        from = at + TOKEN.len();
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        let after_ok = from >= bytes.len() || !is_ident(bytes[from]);
        if !before_ok || !after_ok {
            continue;
        }
        // Walk back to the open paren of the list this token sits in, then
        // read the lint level named just before it.
        let mut depth = 0usize;
        let mut level: Option<&str> = None;
        let mut i = at;
        while i > 0 {
            i -= 1;
            match bytes[i] {
                b')' => depth += 1,
                b'(' if depth > 0 => depth -= 1,
                b'(' => {
                    let mut s = i;
                    while s > 0 && is_ident(bytes[s - 1]) {
                        s -= 1;
                    }
                    level = Some(&code[s..i]);
                    break;
                }
                b'[' | b';' | b'{' | b'}' => break,
                _ => {}
            }
        }
        if matches!(level, Some("allow" | "expect" | "warn")) {
            out.push(at);
        }
    }
    out
}

#[test]
fn unsafe_code_opt_out_detector_bites() {
    for opt_out in [
        "#[allow(unsafe_code)]",
        "#[allow(dead_code, unsafe_code)]",
        "#![allow(clippy::x, unsafe_code)]",
        "#[expect(unsafe_code, reason = \"x\")]",
        "#[warn(unsafe_code)]",
        "#![cfg_attr(feature = \"x\", allow(clippy::foo, unsafe_code))]",
        "#[allow(\n    dead_code,\n    unsafe_code\n)]",
    ] {
        assert_eq!(
            unsafe_code_opt_outs(opt_out).len(),
            1,
            "must be flagged as an opt-out: {opt_out}"
        );
    }
    for fine in [
        "#![cfg_attr(not(test), deny(unsafe_code))]",
        "#![forbid(unsafe_code)]",
        "#[deny(dead_code, unsafe_code)]",
        "let not_unsafe_code_here = 1;",
        "fn unsafe_code_opt_outs(code: &str) {}",
    ] {
        assert!(
            unsafe_code_opt_outs(fine).is_empty(),
            "must not be flagged: {fine}"
        );
    }
}
