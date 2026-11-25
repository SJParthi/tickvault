//! Every S3 PutObject in `crates/storage/src` and `crates/app/src` is sent
//! with an unsigned payload (2026-11-16).
//!
//! ## Why this exists
//!
//! With an in-memory body, aws-sdk-s3 signs the request with
//! `SignableBody::Bytes`: a full SHA-256 of the body inside `send()`, on
//! whichever tokio worker polls it. The raw WAL segment upload sends ~64 MiB
//! gzip segments back to back after the close, and the main runtime's lag
//! probe measured 10 to 114 ms stalls there: one long poll holds the timer
//! and IO driver, so every timer on the runtime waits. The fix is
//! `customize().disable_payload_signing()` (`x-amz-content-sha256:
//! UNSIGNED-PAYLOAD`); the precomputed `x-amz-checksum-sha256` (or the SDK's
//! CRC32 where no checksum is given) still lets S3 refuse a damaged body.
//!
//! This guard fails the build when a `.put_object()` call is added (or an
//! existing one loses the customisation) without
//! `.disable_payload_signing()` before its `.send()`.
//!
//! ## Honest limits
//!
//! It is a text scan: it finds the next `.send()` after each `.put_object()`
//! and looks for `.disable_payload_signing()` in between. A request built in
//! one place and sent from a helper elsewhere (a `.send()` reached only
//! through another function) would need the customisation inside that span
//! or this guard reports it. It covers the two crates whose code runs on the
//! box's tokio runtimes; the Lambdas (`crates/aws-lambdas`) run on their own
//! runtime per invocation and are out of scope.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// For one file's text: the 1-based line of every `.put_object()` whose span
/// up to the next `.send()` has no `.disable_payload_signing()`, and the
/// number of `.put_object()` calls seen.
fn unsigned_violations(src: &str) -> (Vec<usize>, usize) {
    let mut bad = Vec::new();
    let mut seen = 0;
    let mut from = 0;
    while let Some(off) = src[from..].find(".put_object()") {
        let at = from + off;
        seen += 1;
        let rest = &src[at..];
        let span_end = rest.find(".send()").unwrap_or(rest.len());
        if !rest[..span_end].contains(".disable_payload_signing()") {
            bad.push(src[..at].lines().count().max(1));
        }
        from = at + ".put_object()".len();
    }
    (bad, seen)
}

#[test]
fn every_put_object_disables_payload_signing() {
    let root = repo_root();
    let mut files = Vec::new();
    collect_rs(&root.join("crates/storage/src"), &mut files);
    collect_rs(&root.join("crates/app/src"), &mut files);
    files.sort();
    let mut total = 0;
    let mut failures = Vec::new();
    for file in &files {
        let src = fs::read_to_string(file).expect("read source");
        let (bad, seen) = unsigned_violations(&src);
        total += seen;
        for line in bad {
            failures.push(format!(
                "{}:{line}",
                file.strip_prefix(&root).unwrap_or(file).display()
            ));
        }
    }
    // Non-vacuous: s3_cold (bytes + path) and the movers publisher today.
    assert!(
        total >= 3,
        "expected at least 3 `.put_object()` calls, found {total}: the scan is not reading the sources"
    );
    assert!(
        failures.is_empty(),
        "S3 PutObject without `.customize().disable_payload_signing()` before `.send()` \
         (an in-memory body is SHA-256 hashed on a runtime worker inside send()): {failures:?}"
    );
}

#[test]
fn the_scan_catches_a_signed_put_and_accepts_an_unsigned_one() {
    let signed = "c\n    .put_object()\n    .body(b)\n    .send()\n    .await";
    assert_eq!(unsigned_violations(signed), (vec![2], 1));
    let unsigned = "c\n    .put_object()\n    .body(b)\n    .customize()\n    .disable_payload_signing()\n    .send()";
    assert_eq!(unsigned_violations(unsigned), (Vec::new(), 1));
    // A customisation AFTER the send belongs to nothing: still a violation.
    let late = "c.put_object().send(); x.disable_payload_signing()";
    assert_eq!(unsigned_violations(late), (vec![1], 1));
    // A string literal without the leading dot is not a call.
    assert_eq!(unsigned_violations("\"put_object()\""), (Vec::new(), 0));
}
