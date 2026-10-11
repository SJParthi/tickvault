//! Audit CR-5 (2026-10-11): `unsafe` is denied by the COMPILER, not only by
//! the commit-time text scan in `banned-pattern-scanner.sh`.
//!
//! Every crate root (each `lib.rs`, `main.rs` and `bin/*.rs` under
//! `crates/*/src`) carries `#![cfg_attr(not(test), deny(unsafe_code))]`, so
//! production code that writes `unsafe` fails to build. The one production
//! opt-out is the `affinity` module in `crates/core/src/websocket/reader_runtime.rs`
//! (the two CPU-affinity syscalls, each block bounds-checked with a SAFETY
//! note). Test code (Rust 2024 `env::set_var` in `#[cfg(test)]` modules) is
//! outside the lint by the `cfg_attr(not(test), ...)` form.
//!
//! These two tests stop the lint being removed quietly, and stop a second
//! opt-out being added without this file changing with it.

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
fn allow_unsafe_code_appears_only_on_the_affinity_module() {
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
        let lines: Vec<&str> = body.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("allow(unsafe_code") || code.contains("expect(unsafe_code") {
                let next = lines
                    .iter()
                    .skip(i + 1)
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                sites.push((rel.display().to_string(), next.to_string()));
            }
        }
    }
    assert_eq!(
        sites,
        vec![(
            "crates/core/src/websocket/reader_runtime.rs".to_string(),
            "mod affinity {".to_string()
        )],
        "the only production opt-out from deny(unsafe_code) is the affinity module; \
         a new one needs its own SAFETY notes and this test updated in the same change"
    );
}
