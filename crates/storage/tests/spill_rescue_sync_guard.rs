//! Guard (Z8a, 2026-10-02): a rescue counts only once its spill is on disk.
//!
//! `File::flush` is a no-op for `std::fs::File`, so until 2026-10-02 a
//! rescued payload could sit in the page cache while the applied watermark it
//! advanced was fsynced. A host crash then kept the watermark and lost the
//! rows, with nothing counting it. The fix syncs the spill on the writer and
//! rescue threads (off the frame drain) and deliberately does NOT sync the
//! drain's inline fallback, which must never block on the disk.
//!
//! This pins the shape so a refactor cannot quietly drop the sync, or move a
//! blocking sync onto the drain.
#![allow(clippy::expect_used, clippy::panic)]

fn production(file: &str) -> String {
    let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(&path).expect("source file");
    match src.find("#[cfg(test)]\nmod tests") {
        Some(at) => src[..at].to_string(),
        None => src,
    }
}

fn body_of<'a>(src: &'a str, signature: &str) -> &'a str {
    let at = src.find(signature).expect("function exists");
    // A free function closes at column 0, a method at column 4.
    let close = if src[..at].ends_with("    ") {
        "\n    }\n"
    } else {
        "\n}\n"
    };
    let end = src[at..].find(close).expect("function ends") + at;
    &src[at..end]
}

#[test]
fn the_spill_helper_fdatasyncs() {
    let ticks = production("tick_persistence.rs");
    assert!(
        body_of(&ticks, "pub(crate) fn sync_spill_data(").contains("file.sync_data()"),
        "the off-drain sync must be an fdatasync"
    );
    assert!(ticks.contains(
        "pub(crate) const SPILL_SYNC_OFF_DRAIN: Option<SpillSyncFn> = Some(sync_spill_data);"
    ));
    assert!(
        ticks.contains("pub(crate) const SPILL_UNSYNCED_ON_DRAIN: Option<SpillSyncFn> = None;")
    );
}

#[test]
fn both_spill_writers_sync_before_returning_ok() {
    for (file, sig) in [
        ("tick_persistence.rs", "fn spill_failed_ilp("),
        ("depth_persistence.rs", "fn spill_failed_depth_ilp("),
    ] {
        let src = production(file);
        let body = body_of(&src, sig);
        let sync = body
            .find("sync(&file)?;")
            .unwrap_or_else(|| panic!("{file}: no sync"));
        let ok = body.rfind("Ok(path)").expect("returns the path");
        assert!(
            sync < ok,
            "{file}: the sync must happen before the rescue reports Ok"
        );
    }
}

#[test]
fn off_drain_paths_sync_and_the_drain_inline_path_does_not() {
    for file in ["tick_persistence.rs", "depth_persistence.rs"] {
        let src = production(file);
        // The rescue thread's sink is built with the sync, and the writer
        // thread's own rescue passes it.
        let synced = src.matches("SPILL_SYNC_OFF_DRAIN").count();
        assert!(
            synced >= 2,
            "{file}: expected the rescue sink construction and the writer rescue to sync, found {synced}"
        );
        assert!(
            src.contains("self.spill_sync,"),
            "{file}: the rescue thread must pass its sink's sync"
        );
        // Exactly one unsynced call: the drain's inline fallback.
        let unsynced = src.matches("SPILL_UNSYNCED_ON_DRAIN,").count();
        assert_eq!(
            unsynced, 1,
            "{file}: only the drain inline spill is unsynced"
        );
        let discard = body_of(&src, "pub fn discard_pending(");
        assert!(
            discard.contains("SPILL_UNSYNCED_ON_DRAIN"),
            "{file}: the unsynced call must be the drain's discard_pending"
        );
        assert!(
            !discard.contains("SPILL_SYNC_OFF_DRAIN"),
            "{file}: the drain must never block on an fsync"
        );
    }
}

#[test]
fn queued_rescues_hold_and_release_a_watermark_floor() {
    for file in ["tick_persistence.rs", "depth_persistence.rs"] {
        let src = production(file);
        let discard = body_of(&src, "pub fn discard_pending(");
        let hold = discard
            .find("hold_rescue_floor(")
            .expect("drain holds a floor");
        let send = discard.find("try_send(batch)").expect("hand-off");
        assert!(hold < send, "{file}: the floor is held BEFORE the hand-off");
        assert_eq!(
            discard.matches("release_rescue_floor(floor)").count(),
            2,
            "{file}: both refused arms (Full, Disconnected) retract the floor"
        );
        let rescue = body_of(&src, "pub fn rescue(&self, batch: &");
        let outcome = rescue.find("note_rescue_outcome_").expect("outcome");
        let release = rescue
            .find("release_rescue_floor(")
            .expect("rescue thread releases");
        assert!(
            outcome < release,
            "{file}: the floor is released only after the outcome is recorded"
        );
    }
}
