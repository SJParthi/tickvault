//! QUESTDB COMMIT MODE DOC GUARD (2026-10-12, deep audit QDB-7).
//!
//! QuestDB's default `cairo.commit.mode` is `nosync`: an ILP acknowledgement
//! means the rows reached the page cache, not the disk. That choice was
//! measured and kept (sync was 6-8x slower per flush), and crash durability
//! comes from the app: the persisted applied watermark trails acks by
//! `WATERMARK_DURABILITY_LAG_SECS`. The decision lived only in
//! `wal_applied_watermark.rs`; the compose file, where a reader tuning
//! QuestDB looks, said nothing.
//!
//! This guard keeps the compose file stating the commit mode and pointing at
//! the watermark lag, and keeps that constant existing under its name.

use std::path::PathBuf;

fn repo_file(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn compose_states_the_commit_mode_and_points_at_the_watermark_lag() {
    let compose = repo_file("deploy/docker/docker-compose.yml");
    for needle in [
        "cairo.commit.mode",
        "nosync",
        "WATERMARK_DURABILITY_LAG_SECS",
    ] {
        assert!(
            compose.contains(needle),
            "docker-compose.yml must keep `{needle}` in the tv-questdb commit-mode note \
             (deep audit QDB-7)"
        );
    }
    assert!(
        !compose.contains("QDB_CAIRO_COMMIT_MODE: \"sync\""),
        "commit mode switched to sync: update the QDB-7 note and the watermark lag \
         reasoning in wal_applied_watermark.rs in the same change"
    );
    let watermark = repo_file("crates/storage/src/wal_applied_watermark.rs");
    assert!(
        watermark.contains("pub const WATERMARK_DURABILITY_LAG_SECS"),
        "the compose note points at WATERMARK_DURABILITY_LAG_SECS; it must still exist"
    );
}
