// Wave 1 Item 0.e — banned-pattern hook category 7 self-test fixture
// (gap closure G12).
//
// This file is NOT a real Rust source file — it sits under
// `.claude/hooks/test-fixtures/` and is INTENTIONALLY excluded from
// the workspace (no `Cargo.toml` membership, never compiled). The
// banned-pattern-scanner.sh self-test harness
// (`scripts/test-banned-pattern-cat7.sh`) renames a copy of this file
// to `crates/core/src/pipeline/tick_processor.rs` in a dry-run mode
// and confirms category 7 reports the expected violations.
//
// If category 7 ever silently breaks (regex bitrot, comment-stripping
// regression, etc.) the self-test fails and the operator is alerted
// before the rule goes dormant.
//
// Expected violations when this fixture is scanned as
// `crates/core/src/pipeline/tick_processor.rs`:
//
//   1. std::fs::write    on hot path (line 38) — no HOT-PATH-EXEMPT
//   2. std::fs::rename   on hot path (line 39) — no HOT-PATH-EXEMPT
//   3. std::fs::create_dir_all on hot path (line 47) — no HOT-PATH-EXEMPT
//
// The scanner MUST flag all three. If only some are flagged, category 7
// has regressed — fix the scanner or update the fixture intentionally.

#[cfg(not(test))]
fn hot_path_violation_write_no_exempt() {
    // Bug: hot-path sync fs::write with no HOT-PATH-EXEMPT marker.
    // Scanner MUST flag this line.
    std::fs::write("/tmp/cat7-fixture-write.tmp", b"data").ok();
    std::fs::rename(
        "/tmp/cat7-fixture-write.tmp",
        "/tmp/cat7-fixture-write.json",
    )
    .ok();
}

#[cfg(not(test))]
fn hot_path_violation_create_dir_no_exempt() {
    // Bug: hot-path sync create_dir_all with no HOT-PATH-EXEMPT marker.
    std::fs::create_dir_all("/tmp/cat7-fixture-dir").ok();
}

// Counter-fixture: the same pattern with an explicit HOT-PATH-EXEMPT
// comment on the directly preceding line MUST be allowed by the
// scanner. If the scanner flags THIS section, the exempt-detection
// logic has regressed.
#[cfg(not(test))]
fn hot_path_with_exempt_marker() {
    // HOT-PATH-EXEMPT: this fixture exercises the allow-list path of
    // category 7; the call site is intentional and approved.
    std::fs::write("/tmp/cat7-fixture-allowed.tmp", b"data").ok();
}
