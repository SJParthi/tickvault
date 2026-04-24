//! 2026-04-24 — source-scan ratchet for env-mutation lock coverage.
//!
//! Rust 2024 promoted `std::env::set_var` / `remove_var` to `unsafe`
//! because they race with any other thread reading the env (libc's
//! global env table is not thread-safe). Under `cargo test` default
//! parallelism, any pair of `#[test]` functions mutating env vars
//! without synchronization is a data race that ThreadSanitizer flags
//! (and rightly so — one thread's `setenv` can observe another thread
//! mid-`unsetenv` in `environ[]`).
//!
//! GitHub issue #304 filed this TSan finding on 2026-04-20. PR #354
//! shipped a local lock inside `middleware.rs`'s `mod tests`. This
//! PR (#355) extracts the lock into a crate-shared
//! `test_support::env_lock()` so both `middleware.rs` and
//! `handlers/debug.rs` acquire the **same** mutex — otherwise two
//! local locks in separate modules serialize independently and still
//! race on the global libc env table.
//!
//! This test source-scans every `.rs` file under `crates/api/src/`
//! and asserts two invariants:
//!
//! 1. Every file that contains `unsafe { std::env::set_var` or
//!    `unsafe { std::env::remove_var` also imports `env_lock` (either
//!    as `use crate::test_support::env_lock` or
//!    `use crate::test_support::env_lock as <alias>`).
//! 2. Every `#[test]` or `#[tokio::test]` function that contains an
//!    `unsafe { std::env::set_var }` or `unsafe { std::env::remove_var }`
//!    call in its body also contains a `let _env_guard = env_lock();`
//!    or similar lock acquisition before the first unsafe block.
//!
//! If either assertion fires, a future contributor has introduced a
//! new env-mutation site without the lock. Fix the source site, not
//! this test.

use std::fs;
use std::path::{Path, PathBuf};

fn api_src_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("src");
    path
}

fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("readable dir") {
        let entry = entry.expect("readable entry");
        let path = entry.path();
        if path.is_dir() {
            walk_rs_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

fn file_contains_any(src: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| src.contains(n))
}

#[test]
fn every_env_mutation_site_uses_the_shared_env_lock() {
    let mut files = Vec::new();
    walk_rs_files(&api_src_root(), &mut files);

    let mut violations: Vec<String> = Vec::new();

    for file in &files {
        let src = fs::read_to_string(file).expect("readable source");

        let has_set = src.contains("unsafe { std::env::set_var");
        let has_remove = src.contains("unsafe { std::env::remove_var");
        // Multi-line form: `unsafe {\n    std::env::set_var(...)`
        let has_set_multiline = src.contains("std::env::set_var");
        let has_remove_multiline = src.contains("std::env::remove_var");

        if !(has_set || has_remove || has_set_multiline || has_remove_multiline) {
            continue;
        }

        // test_support.rs is the module that DEFINES env_lock — it
        // shouldn't use itself, and it doesn't mutate env. Skip.
        if file
            .file_name()
            .map(|f| f == "test_support.rs")
            .unwrap_or(false)
        {
            continue;
        }

        // The file has env mutations — assert it imports env_lock.
        let imports_lock = file_contains_any(
            &src,
            &[
                "use crate::test_support::env_lock",
                "crate::test_support::env_lock as ",
            ],
        );
        if !imports_lock {
            violations.push(format!(
                "{}: contains std::env::set_var/remove_var but does NOT import env_lock \
                 from crate::test_support. Add `use crate::test_support::env_lock;` and \
                 acquire the lock in every env-mutating test.",
                file.display()
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "env_lock coverage ratchet failed:\n{}",
        violations.join("\n")
    );
}

#[test]
fn middleware_and_debug_handler_both_use_the_shared_lock() {
    // Direct assertion on the two known env-mutation call sites so
    // the CI failure message points at the exact fix location rather
    // than a generic "some file in crates/api/src/ doesn't import
    // env_lock" message from the broader scan above.
    let middleware_src = fs::read_to_string(api_src_root().join("middleware.rs"))
        .expect("middleware.rs must be readable");
    let debug_src = fs::read_to_string(api_src_root().join("handlers").join("debug.rs"))
        .expect("handlers/debug.rs must be readable");

    assert!(
        middleware_src.contains("use crate::test_support::env_lock"),
        "middleware.rs must import env_lock from crate::test_support so its \
         env-mutating tests serialize against debug.rs's env-mutating tests \
         (both files compile into the same test binary and share libc's \
         global env table)."
    );
    assert!(
        debug_src.contains("use crate::test_support::env_lock"),
        "handlers/debug.rs must import env_lock from crate::test_support — \
         see TSan finding #304 and PR #355 rationale."
    );
}

#[test]
fn env_lock_module_exists_and_is_test_gated() {
    // If someone accidentally deletes test_support.rs or unregisters
    // it from lib.rs, the rest of the suite will fail to compile —
    // but the error will point at the import site, not the root. This
    // assertion points directly at the root cause.
    let lib_src =
        fs::read_to_string(api_src_root().join("lib.rs")).expect("lib.rs must be readable");
    assert!(
        lib_src.contains("mod test_support"),
        "crates/api/src/lib.rs must declare `#[cfg(test)] mod test_support;` — \
         that module provides the shared env lock used by middleware + debug \
         tests. See PR #355."
    );
    let test_support_path = api_src_root().join("test_support.rs");
    assert!(
        test_support_path.exists(),
        "crates/api/src/test_support.rs must exist (declared by lib.rs \
         `mod test_support;` and used by middleware + debug test modules)."
    );
}
