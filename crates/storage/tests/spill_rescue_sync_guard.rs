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
        // H4 (2026-10-03): the one unsynced call lives in `spill_on_drain`,
        // shared by `discard_pending` and the park's thread-gone arm.
        assert!(
            body_of(&src, "fn spill_on_drain(").contains("SPILL_UNSYNCED_ON_DRAIN"),
            "{file}: the unsynced call must be the drain's spill_on_drain"
        );
        let discard = body_of(&src, "pub fn discard_pending(");
        assert!(
            discard.contains("self.spill_on_drain("),
            "{file}: discard_pending must spill through spill_on_drain"
        );
        for drain_fn in [
            "pub fn discard_pending(",
            "fn retry_parked_rescue(",
            "fn park_rescue(",
            "fn spill_parked_on_drain(",
        ] {
            assert!(
                !body_of(&src, drain_fn).contains("SPILL_SYNC_OFF_DRAIN"),
                "{file}: {drain_fn} runs on the drain and must never block on an fsync"
            );
        }
    }
}

/// H4 (2026-10-03): the drain PARKS a rescue it cannot hand off instead of
/// writing it to disk, and never WAITS to hand a parked batch on. Only the
/// shutdown close may block on the send.
#[test]
fn the_drain_parks_a_refused_rescue_and_never_waits_to_retry_it() {
    for (file, max) in [
        (
            "tick_persistence.rs",
            "pub const PARKED_RESCUE_MAX_BATCHES: usize = 8;",
        ),
        (
            "depth_persistence.rs",
            "pub const DEPTH_PARKED_RESCUE_MAX_BATCHES: usize = 8;",
        ),
    ] {
        let src = production(file);
        assert!(src.contains(max), "{file}: tick and depth share one bound");
        let discard = body_of(&src, "pub fn discard_pending(");
        let retry = discard
            .find("self.retry_parked_rescue();")
            .unwrap_or_else(|| panic!("{file}: discard_pending retries the park"));
        let send = discard.find("try_send(batch)").expect("hand-off");
        assert!(retry < send, "{file}: older parked batches go first");
        let park = discard
            .find("self.park_rescue(returned);")
            .unwrap_or_else(|| panic!("{file}: a full queue parks"));
        let inline = discard.find("self.spill_on_drain(").expect("inline");
        assert!(
            park < inline,
            "{file}: the park is tried before the inline spill"
        );
        let retry_body = body_of(&src, "fn retry_parked_rescue(");
        assert!(retry_body.contains("try_send(batch)"));
        assert!(
            !retry_body.contains(".send(batch)"),
            "{file}: the drain must never wait on the rescue queue"
        );
        assert!(
            body_of(&src, "pub fn flush(&mut self)").contains("self.retry_parked_rescue();"),
            "{file}: every flush retries the park"
        );
        let close = body_of(&src, "pub fn close_rescue_offload(");
        assert!(
            close.contains("tx.send(batch)"),
            "{file}: shutdown hands every parked batch to the rescue thread"
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

/// O(1) sweep, 2026-10-03: the frame drain calls `flush` BARE, because each
/// writer moves the tokio worker aside itself at the only steps that block.
/// This pins those steps, so a refactor that drops one puts a blocking file
/// write or HTTP round trip back on a worker the runtime cannot reclaim.
#[test]
fn every_blocking_writer_step_runs_off_the_worker() {
    let helper = production("off_worker.rs");
    let body = body_of(&helper, "pub fn off_worker<T>(");
    assert!(
        body.contains("block_in_place") && body.contains("RuntimeFlavor::MultiThread"),
        "off_worker must move the worker on a multi-thread runtime and only there"
    );
    for file in ["tick_persistence.rs", "depth_persistence.rs"] {
        let src = production(file);
        assert!(
            body_of(&src, "fn spill_on_drain(").contains("off_worker(||"),
            "{file}: the drain's inline spill write must run off the worker"
        );
        let mut producer_flushes = 0;
        for line in src.lines().filter(|l| l.contains("sender.flush(")) {
            if line.trim_start().starts_with("//")
                || line.contains("sender.flush(&mut batch.buffer)")
            {
                // Comments, and the sink thread's own flush (a plain OS thread).
                continue;
            }
            producer_flushes += 1;
            assert!(
                line.contains("off_worker(||"),
                "{file}: a synchronous ILP round trip on the producer half runs on \
                 the worker: `{}`",
                line.trim()
            );
        }
        assert!(
            producer_flushes >= 1,
            "{file}: no producer-half ILP flush found; the scan is stale"
        );
    }
}
