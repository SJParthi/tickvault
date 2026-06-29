//! §28 operator-boundary guard — `crates/trading/src/indicator/` +
//! `crates/trading/src/strategy/` are FROZEN.
//!
//! Source-scan ratchet promised by
//! `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §28
//! (operator verbatim 2026-05-27: "as of now don't even touch indicators and
//! strategies area dude okay?").
//!
//! Every `.rs` file under the two frozen directories is content-pinned by an
//! FNV-1a 64-bit hash + byte length + line count, captured 2026-06-06. ANY edit
//! to ANY of those files — even a comment or a whitespace change — flips the pin
//! and FAILS THE BUILD. That is the §28 mechanism: the boundary holds "until the
//! boundary is lifted".
//!
//! ## Lifting the boundary (the conscious ritual)
//! When the operator EXPLICITLY lifts §28 with a dated quote:
//!   1. Edit `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
//!      §28 to record the lift (dated operator quote).
//!   2. Re-bless the manifest below: run
//!      `BLESS_BOUNDARY=1 cargo test -p tickvault-storage \
//!        --test operator_boundary_indicator_strategy_guard -- --nocapture`
//!      and paste the printed manifest over `BOUNDARY_FILES`.
//! Never re-bless without step 1 — the whole point is that touching this area is
//! a deliberate, recorded operator decision.
//!
//! Why FNV-1a and not SHA-256: a new workspace dependency needs operator approval
//! (CLAUDE.md), and this is regression-evidence, not adversarial crypto. FNV-1a is
//! deterministic + version-stable across Rust releases (unlike
//! `std::hash::DefaultHasher`), so it is the correct tool for a persisted pin.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The two frozen directories, relative to repo root.
const FROZEN_DIRS: &[&str] = &[
    "crates/trading/src/indicator",
    "crates/trading/src/strategy",
];

/// `engine.rs` field §28 names specifically ("the states field unchanged").
const ENGINE_REL: &str = "crates/trading/src/indicator/engine.rs";
const ENGINE_STATES_FIELD: &str = "states: Vec<IndicatorState>,";

/// Content pin captured 2026-06-06; re-blessed 2026-06-29 for the §28.1 narrow
/// `security_id` u32→u64 widening lift (operator-approved plan
/// `active-plan-groww-security-id-u64.md`). Tuple =
/// (path-relative-to-repo-root, fnv1a64, byte_len, line_count).
const BOUNDARY_FILES: &[(&str, u64, usize, usize)] = &[
    (
        "crates/trading/src/indicator/engine.rs",
        0x28130b652738efeb,
        51829,
        1436,
    ),
    (
        "crates/trading/src/indicator/mod.rs",
        0xcd18e8edd8661e9b,
        351,
        13,
    ),
    (
        "crates/trading/src/indicator/obi.rs",
        0x2917677e0aec6fff,
        19913,
        576,
    ),
    (
        "crates/trading/src/indicator/tests.rs",
        0xc660274629c37552,
        14547,
        463,
    ),
    (
        "crates/trading/src/indicator/types.rs",
        0x00b6a1289e81f81e,
        32025,
        968,
    ),
    (
        "crates/trading/src/strategy/config.rs",
        0x1391a4e2d0fbefb4,
        41924,
        1425,
    ),
    (
        "crates/trading/src/strategy/config_tests.rs",
        0x03c7b91bff066111,
        11495,
        459,
    ),
    (
        "crates/trading/src/strategy/evaluator.rs",
        0xf20f51ffa10334c0,
        76706,
        2089,
    ),
    (
        "crates/trading/src/strategy/hot_reload.rs",
        0x5de2a85fe5a3ddca,
        36909,
        1081,
    ),
    (
        "crates/trading/src/strategy/mod.rs",
        0x6280e89d9fbbe221,
        609,
        20,
    ),
    (
        "crates/trading/src/strategy/tests.rs",
        0x9a83670568b0260f,
        15843,
        515,
    ),
    (
        "crates/trading/src/strategy/types.rs",
        0x355d2198678f1676,
        24398,
        786,
    ),
];

/// FNV-1a 64-bit. Deterministic + stable across Rust versions.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../crates/storage  →  parent().parent() = repo root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root is two levels above crates/storage")
        .to_path_buf()
}

fn read_rel(rel: &str) -> Vec<u8> {
    let path = repo_root().join(rel);
    fs::read(&path).unwrap_or_else(|err| panic!("§28 guard: cannot read frozen file {rel}: {err}"))
}

/// Every `.rs` file currently on disk under the frozen dirs (repo-relative).
fn frozen_files_on_disk() -> BTreeSet<String> {
    let root = repo_root();
    let mut out = BTreeSet::new();
    for dir in FROZEN_DIRS {
        let abs = root.join(dir);
        let entries = fs::read_dir(&abs)
            .unwrap_or_else(|err| panic!("§28 guard: cannot read frozen dir {dir}: {err}"));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .expect("file name is valid UTF-8");
                out.insert(format!("{dir}/{name}"));
            }
        }
    }
    out
}

const LIFT_HINT: &str = "\n\n§28 OPERATOR BOUNDARY VIOLATED — `crates/trading/src/indicator/` and \
`crates/trading/src/strategy/` are FROZEN by operator lock 2026-05-27 \
(\"don't even touch indicators and strategies area\").\n\
If the operator has NOT lifted §28: revert your change to the frozen area.\n\
If the operator HAS lifted §28 (dated quote): (1) update §28 in \
.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md, then \
(2) re-bless with `BLESS_BOUNDARY=1 cargo test -p tickvault-storage \
--test operator_boundary_indicator_strategy_guard -- --nocapture`.";

/// §28-named test: the `IndicatorEngine::states` field (and the whole engine.rs)
/// is unchanged since 2026-05-27.
#[test]
fn test_indicator_engine_states_field_unchanged_since_2026_05_27() {
    let bytes = read_rel(ENGINE_REL);
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains(ENGINE_STATES_FIELD),
        "§28 guard: `engine.rs` no longer contains the exact `{ENGINE_STATES_FIELD}` \
         field — the indicator engine's state layout changed.{LIFT_HINT}"
    );

    let (_, want_fnv, want_len, _) = BOUNDARY_FILES
        .iter()
        .find(|(p, ..)| *p == ENGINE_REL)
        .expect("engine.rs is pinned in BOUNDARY_FILES");
    assert_eq!(
        bytes.len(),
        *want_len,
        "§28 guard: engine.rs byte length changed.{LIFT_HINT}"
    );
    assert_eq!(
        fnv1a64(&bytes),
        *want_fnv,
        "§28 guard: engine.rs content hash changed.{LIFT_HINT}"
    );
}

/// Every pinned frozen file matches its captured content exactly.
#[test]
fn test_indicator_strategy_boundary_files_unchanged() {
    if std::env::var("BLESS_BOUNDARY").is_ok() {
        // Re-bless mode: print the current manifest and skip assertions.
        eprintln!("// re-blessed manifest — paste over BOUNDARY_FILES:");
        for dir in FROZEN_DIRS {
            for rel in frozen_files_on_disk().iter().filter(|r| r.starts_with(dir)) {
                let bytes = read_rel(rel);
                let lines = bytes.iter().filter(|&&b| b == b'\n').count();
                eprintln!(
                    "    (\"{rel}\", 0x{:016x}, {}, {}),",
                    fnv1a64(&bytes),
                    bytes.len(),
                    lines
                );
            }
        }
        return;
    }

    for (rel, want_fnv, want_len, want_lines) in BOUNDARY_FILES {
        let bytes = read_rel(rel);
        let got_lines = bytes.iter().filter(|&&b| b == b'\n').count();
        assert_eq!(
            bytes.len(),
            *want_len,
            "§28 guard: {rel} byte length changed ({} → {}).{LIFT_HINT}",
            want_len,
            bytes.len()
        );
        assert_eq!(
            got_lines, *want_lines,
            "§28 guard: {rel} line count changed ({want_lines} → {got_lines}).{LIFT_HINT}"
        );
        assert_eq!(
            fnv1a64(&bytes),
            *want_fnv,
            "§28 guard: {rel} content hash changed.{LIFT_HINT}"
        );
    }
}

/// The manifest neither misses a file that exists on disk (a NEW file added to
/// the frozen area) nor references a file that was deleted.
#[test]
fn test_boundary_manifest_covers_every_src_file() {
    let on_disk = frozen_files_on_disk();
    let pinned: BTreeSet<String> = BOUNDARY_FILES.iter().map(|(p, ..)| p.to_string()).collect();

    let added: Vec<&String> = on_disk.difference(&pinned).collect();
    let removed: Vec<&String> = pinned.difference(&on_disk).collect();

    assert!(
        added.is_empty(),
        "§28 guard: NEW file(s) appeared in the frozen area but are not pinned: {added:?}.{LIFT_HINT}"
    );
    assert!(
        removed.is_empty(),
        "§28 guard: pinned file(s) no longer exist on disk (deleted from frozen area): {removed:?}.{LIFT_HINT}"
    );
}
