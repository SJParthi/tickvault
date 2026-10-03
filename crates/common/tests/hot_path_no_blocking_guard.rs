//! Guard: nothing on the live market-data path may BLOCK (2026-10-02).
//!
//! Owner: "not even the single code or functions or scenarios should be
//! blocked … everything should work on its own". The socket read task, the
//! WAL hand-off, the frame drain, the candle fold and the decoders must never
//! wait on a lock, a disk, a sleep or a full blocking channel: a reader that
//! waits stops answering Dhan's ping and is disconnected, and a drain that
//! waits fills the ring.
//!
//! This test source-scans those regions for blocking calls and FAILS the
//! build when one appears without a reviewed exemption: a line
//! `// APPROVED-BLOCKING: <reason>` directly above it (or a trailing
//! comment on the same line). The exemption is visible in review, so a
//! deliberate bounded wait is a decision, never an accident.
//!
//! Scope is listed in [`TARGETS`]: whole files where the whole file is hot,
//! and named functions where a file mixes hot and cold code (the WAL module
//! also owns its writer THREAD, which may block — that is what the thread is
//! for). A target that cannot be found fails the test, so a rename cannot
//! silently shrink the scan to nothing.
//!
//! Bite-proven on 2026-10-02 by inserting `std::thread::sleep` into
//! `multi_tf_aggregator.rs` (the test failed, naming the line) and removing
//! it again.

use std::path::{Path, PathBuf};

/// One scanned region: a file, and optionally the signature prefix of one
/// function inside it (`None` = the whole file up to its first top-level
/// `#[cfg(test)]`).
struct Target {
    file: &'static str,
    function: Option<&'static str>,
}

const TARGETS: &[Target] = &[
    // The socket read task: one WAL append + one ring try_send per frame.
    Target {
        file: "crates/core/src/websocket/pool_supervisor.rs",
        function: Some("fn accept(&self, frame: Bytes)"),
    },
    Target {
        file: "crates/core/src/websocket/pool_supervisor.rs",
        function: Some("fn handle_socket_event<"),
    },
    Target {
        file: "crates/core/src/websocket/connection.rs",
        function: Some("async fn recv(&mut self) -> SocketEvent"),
    },
    Target {
        file: "crates/core/src/websocket/reader_runtime.rs",
        function: None,
    },
    // The capture-at-receipt WAL hand-off (NOT its writer thread).
    Target {
        file: "crates/storage/src/ws_frame_spill.rs",
        function: Some("pub fn append_with_seq_at("),
    },
    Target {
        file: "crates/storage/src/ws_frame_spill.rs",
        function: Some("fn refuse_over_byte_budget("),
    },
    // The telemetry recorded on every frame.
    Target {
        file: "crates/storage/src/hot_path_telemetry.rs",
        function: Some("pub fn record_stage_nanos("),
    },
    Target {
        file: "crates/storage/src/hot_path_telemetry.rs",
        function: Some("pub fn beat_at("),
    },
    // The frame drain and its per-frame decoders.
    Target {
        file: "crates/app/src/dhan_feed_stack.rs",
        function: Some("async fn run_frame_drain("),
    },
    Target {
        file: "crates/app/src/dhan_feed_stack.rs",
        function: Some("pub fn drain_main_feed_frame("),
    },
    Target {
        file: "crates/app/src/dhan_feed_stack.rs",
        function: Some("fn drain_depth_frame("),
    },
    // The candle fold, the seal ring and the binary decoders.
    Target {
        file: "crates/trading/src/candles/multi_tf_aggregator.rs",
        function: None,
    },
    Target {
        file: "crates/trading/src/candles/seal_ring.rs",
        function: None,
    },
    Target {
        file: "crates/core/src/parser/dispatcher.rs",
        function: None,
    },
    Target {
        file: "crates/core/src/parser/depth.rs",
        function: None,
    },
    Target {
        file: "crates/core/src/parser/full_packet.rs",
        function: None,
    },
    Target {
        file: "crates/core/src/parser/quote.rs",
        function: None,
    },
    Target {
        file: "crates/core/src/parser/ticker.rs",
        function: None,
    },
    // The spot price store the drain writes per spot tick.
    Target {
        file: "crates/app/src/spot_price_store.rs",
        function: None,
    },
];

/// Blocking calls, matched against code with `//` comments removed.
/// `(needle, what it is)`.
const BLOCKING: &[(&str, &str)] = &[
    ("thread::sleep", "a thread sleep"),
    ("block_on(", "a runtime block_on"),
    ("block_in_place(", "block_in_place"),
    (
        "blocking_flush(",
        "a synchronous flush wrapper (block_in_place)",
    ),
    (".lock()", "a Mutex/RwLock lock"),
    (".read()", "an RwLock read lock"),
    (".write()", "an RwLock write lock"),
    ("sync_all(", "an fsync"),
    ("sync_data(", "an fdatasync"),
    ("fsync", "an fsync"),
    ("std::fs::", "a synchronous filesystem call"),
    ("File::open(", "a synchronous file open"),
    ("File::create(", "a synchronous file create"),
    ("OpenOptions::new(", "a synchronous file open"),
    ("reqwest::blocking", "a blocking HTTP client"),
    ("recv_timeout(", "a blocking channel receive"),
    ("blocking_recv(", "a blocking channel receive"),
    ("blocking_send(", "a blocking channel send"),
];

const MARKER: &str = "APPROVED-BLOCKING:";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("workspace root above crates/common")
}

/// The production part of a file: everything before the first line that is
/// exactly `#[cfg(test)]` at column 0.
fn production(src: &str) -> &str {
    src.find("\n#[cfg(test)]").map_or(src, |i| &src[..i])
}

/// The body of the function whose signature starts with `signature`,
/// found by brace matching from the first `{` after it. Braces inside string
/// and char literals and `//` comments are skipped.
fn function_body<'a>(src: &'a str, signature: &str) -> Option<(usize, &'a str)> {
    let start = src.find(signature)?;
    let bytes = src.as_bytes();
    let mut i = start + signature.len();
    // Skip to the opening brace of the body (the signature may span lines;
    // a `where` clause has no braces).
    while i < bytes.len() && bytes[i] != b'{' {
        i += 1;
    }
    let open = i;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut in_line_comment = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_line_comment {
            if c == b'\n' {
                in_line_comment = false;
            }
        } else if in_str {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            in_line_comment = true;
        } else if c == b'"' {
            in_str = true;
        } else if c == b'\'' && bytes.get(i + 2) == Some(&b'\'') {
            // A char literal such as '{' — skip it whole.
            i += 2;
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                let line = src[..start].matches('\n').count() + 1;
                return Some((line, &src[start..=i]));
            }
        }
        i += 1;
    }
    let _ = open;
    None
}

/// Every unexempted blocking call in `region`, as `(line, needle, what)`.
fn violations(region: &str, first_line: usize) -> Vec<(usize, &'static str, &'static str)> {
    let lines: Vec<&str> = region.lines().collect();
    let mut found = Vec::new();
    for (idx, raw) in lines.iter().enumerate() {
        let code = raw.split("//").next().unwrap_or("");
        for (needle, what) in BLOCKING {
            if !code.contains(needle) {
                continue;
            }
            let same_line = raw.contains(MARKER);
            let above = lines[..idx]
                .iter()
                .rev()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| l.contains(MARKER));
            if !same_line && !above {
                found.push((first_line + idx, *needle, *what));
            }
        }
    }
    found
}

fn scan(root: &Path) -> Vec<String> {
    let mut report = Vec::new();
    for target in TARGETS {
        let path = root.join(target.file);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("hot-path file {} unreadable: {e}", target.file));
        let prod = production(&src);
        let (first_line, region) = match target.function {
            None => (1, prod),
            Some(sig) => function_body(prod, sig).unwrap_or_else(|| {
                panic!(
                    "hot-path function `{sig}` not found in {} — a rename must update \
                     this guard, or the scan silently shrinks",
                    target.file
                )
            }),
        };
        assert!(
            !region.trim().is_empty(),
            "empty scan region for {}",
            target.file
        );
        for (line, needle, what) in violations(region, first_line) {
            report.push(format!(
                "{}:{line}: `{needle}` ({what}) on the live path with no \
                 `// {MARKER} <reason>` line above it",
                target.file
            ));
        }
    }
    report
}

#[test]
fn hot_path_has_no_unapproved_blocking_call() {
    let report = scan(&repo_root());
    assert!(
        report.is_empty(),
        "blocking calls on the live market-data path:\n{}\n\nMove the work off the \
         hot path (a writer thread, try_send, an atomic), or — if the wait is \
         deliberate and bounded — put `// {MARKER} <why it is safe>` on the line above.",
        report.join("\n")
    );
}

#[test]
fn the_scanner_flags_a_blocking_call_and_honours_the_marker() {
    let bad = "fn f() {\n    let g = m.lock();\n}\n";
    assert_eq!(violations(bad, 1).len(), 1);
    let approved = "fn f() {\n    // APPROVED-BLOCKING: test fixture\n    let g = m.lock();\n}\n";
    assert!(violations(approved, 1).is_empty());
    let commented = "fn f() {\n    // a comment naming std::thread::sleep is not a call\n}\n";
    assert!(violations(commented, 1).is_empty());
    let sleep = "fn f() {\n    std::thread::sleep(d);\n}\n";
    assert_eq!(violations(sleep, 1)[0].1, "thread::sleep");
}

#[test]
fn function_body_extracts_exactly_one_function() {
    let src = "fn a() {\n    let s = \"}{\";\n    if x { y(); }\n}\nfn b() {\n    z();\n}\n";
    let (line, body) = function_body(src, "fn a(").expect("found");
    assert_eq!(line, 1);
    assert!(
        body.contains("y();"),
        "a brace inside a string must not end the body"
    );
    assert!(!body.contains("fn b"));
}

#[test]
fn every_target_resolves_to_a_non_empty_region_no_panic() {
    // `scan` panics on a missing file or function; reaching the end proves
    // every target was found.
    let _ = scan(&repo_root());
}
