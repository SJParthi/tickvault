//! The `//!` header of `crates/app/src/main.rs` must not describe deleted
//! boot steps as the live boot.
//!
//! Regression: 2026-10-11 (audit TRU-12). The header listed a 15-step boot
//! from 2026-05: an IP check that "BLOCKS BOOT", an F&O universe and
//! subscription plan, a tick processing pipeline task, a historical candle
//! fetch and a token renewal task. None of those run in `main` today. A
//! reader auditing the boot from the file's own header audited a fiction.
//!
//! House style keeps the old list as history, struck through with `~~`, and
//! adds a dated correction; so this guard reads the header with every
//! `~~…~~` span removed and fails if any deleted step is still stated.

use std::path::PathBuf;

fn main_rs() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// The leading block of `//!` lines, joined.
fn header(src: &str) -> String {
    src.lines()
        .take_while(|l| l.starts_with("//!"))
        .map(|l| l.trim_start_matches("//!"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn without_struck(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(open) = rest.find("~~") {
        let after = &rest[open + 2..];
        match after.find("~~") {
            Some(close) => {
                out.push_str(&rest[..open]);
                rest = &after[close + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn main_rs_header_names_no_deleted_boot_step() {
    let head = header(&main_rs());
    assert!(
        head.lines().count() >= 5,
        "main.rs lost its //! header, or the reader is broken"
    );
    let live = without_struck(&head);
    for deleted in [
        "Verify public IP against SSM static IP",
        "Build F&O universe",
        "Spawn tick processing pipeline",
        "historical candle fetch (cold path)",
        "Spawn token renewal task",
    ] {
        assert!(
            !live.contains(deleted),
            "main.rs header still states the deleted boot step {deleted:?} \
             unstruck. Strike it with ~~ and keep the dated correction that \
             lists the real boot."
        );
    }
    assert!(
        live.contains("CORRECTED 2026-10-11"),
        "main.rs header must carry the dated correction naming the real boot"
    );
}

#[test]
fn without_struck_self_test() {
    assert_eq!(without_struck("a ~~b~~ c"), "a  c");
    assert_eq!(without_struck("a ~~b"), "a ~~b");
    assert_eq!(header("//! x\n//! y\nfn z() {}"), " x\n y");
}
