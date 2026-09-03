//! Both OFFLOAD writer sinks must carry the bounded flush retry.
//!
//! # The defect this pins (measured on production, 2026-09-03)
//!
//! A bounded one-shot retry for the transport failure class was added to
//! `DepthWriter`'s **synchronous** flush on 2026-08-25, after
//! `io: Interrupted system call (os error 4)` cost 8,810 rows. Its reasoning is
//! written out at the site and is sound.
//!
//! Three days later `split_for_offload` moved every production depth flush onto
//! a dedicated writer thread — and the new sink shipped **without the retry**.
//! The tick writer had moved the same way on 2026-08-25 and had never had one at
//! all, on either path.
//!
//! So the fix was sitting on the code path that had been superseded. Measured on
//! 2026-09-03, in one session:
//!
//! | Code | Count | Reason on every line |
//! |---|---:|---|
//! | `HOT-PATH-02` (tick flush) | **58,746** | `io: Connection reset by peer (os error 104)` |
//! | `TICK-FLUSH-01` (depth flush) | **41,204** | same class |
//!
//! Each rescued roughly 120 rows to the spill tier, so the great majority of the
//! day's ticks were on disk rather than in QuestDB: `SELECT count() FROM ticks
//! WHERE ts IN today()` read **2,310,693** against roughly 82M ingested. Nothing
//! was lost. Almost nothing was queryable.
//!
//! A reset is precisely the class the buffer survives — which is why `rescue`
//! can spill the same bytes — and retrying is idempotent by construction,
//! because both DEDUP keys carry `capture_seq`, unique per received frame.
//!
//! # Why a SOURCE scan and not a behavioural test
//!
//! The defect was never "the retry is wrong". It was "the retry is on the wrong
//! path". A behavioural test of the sync path passed throughout and proved
//! nothing about the path that runs. What has to be pinned is *which function*
//! carries it, and that is a structural claim.

/// The body of `impl <Sink> { pub fn write(...) }`, up to the `rescue` helper
/// that follows it in both files.
fn offload_write_body(source: &str, rescue_sig: &str) -> String {
    let production = source
        .split_once("\n#[cfg(test)]")
        .map_or(source, |(prod, _)| prod);
    let start = production
        .find("pub fn write(&mut self, batch:")
        .expect("the offload sink must expose `pub fn write(&mut self, batch: ...)`");
    let rest = &production[start..];
    let end = rest
        .find(rescue_sig)
        .unwrap_or_else(|| panic!("the offload sink's `write` must be followed by `{rescue_sig}`"));
    rest[..end].to_string()
}

fn assert_carries_bounded_retry(body: &str, who: &str, counter: &str) {
    // Two flushes, not one — the retry itself.
    let flushes = body.matches("sender.flush(&mut batch.buffer)").count();
    assert_eq!(
        flushes, 2,
        "{who}: the offload sink must flush TWICE — one first attempt and one \
         bounded retry. Found {flushes}. A single flush is the shape that cost \
         58,746 rescued tick batches on 2026-09-03."
    );

    // Gated on the transport class, never on every error.
    assert!(
        body.contains("flush_failure_is_retryable"),
        "{who}: the retry must be gated on the transport failure class. \
         Retrying a rejected payload re-sends bytes the server already refused."
    );

    // Gated on the first failure being FAST — the guard that keeps a 5s
    // request_timeout from becoming a 10s stall on the writer thread, which
    // overflows the hand-off queue and rescues the rows the retry was for.
    assert!(
        body.contains("DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW"),
        "{who}: the retry must be gated on the first failure returning inside \
         the fast-failure window. Without it a timed-out flush is retried and \
         the worst case doubles to ten seconds."
    );

    // A retryable-but-slow failure is COUNTED, so "the retry is not firing" is
    // answerable without a guess.
    assert!(
        body.contains(counter),
        "{who}: the retry must increment {counter} so its rate is measurable."
    );
    assert!(
        body.contains("_skipped_total"),
        "{who}: a retryable-but-too-slow failure must be counted separately, or \
         a quiet retry counter cannot be distinguished from a quiet failure rate."
    );
}

#[test]
fn the_tick_offload_sink_carries_the_bounded_retry() {
    let source = include_str!("../src/tick_persistence.rs");
    let body = offload_write_body(source, "fn rescue(&mut self, batch: &mut FlushBatch");
    assert_carries_bounded_retry(&body, "TickWriterSink", "tv_tick_flush_retries_total");
}

#[test]
fn the_depth_offload_sink_carries_the_bounded_retry() {
    let source = include_str!("../src/depth_persistence.rs");
    let body = offload_write_body(source, "fn rescue(&mut self, batch: &mut DepthFlushBatch");
    assert_carries_bounded_retry(&body, "DepthWriterSink", "tv_depth_flush_retries_total");
}

/// The offload path must remain the one production actually uses — otherwise
/// this guard pins a retry on a path that has itself been superseded, which is
/// the exact failure it was written to catch.
#[test]
fn the_offload_path_is_still_the_production_path() {
    for (name, source) in [
        ("tick", include_str!("../src/tick_persistence.rs")),
        ("depth", include_str!("../src/depth_persistence.rs")),
    ] {
        let production = source
            .split_once("\n#[cfg(test)]")
            .map_or(source, |(prod, _)| prod);
        assert!(
            production.contains("split_for_offload"),
            "{name}: `split_for_offload` is gone. If the writer moved to a third \
             path, the retry must move with it — that displacement is the whole \
             defect this file exists to prevent."
        );
    }
}

/// The classifier must stay narrow. Widening it to every error code would retry
/// payloads the server deliberately rejected.
#[test]
fn the_retryable_class_is_transport_only() {
    let source = include_str!("../src/depth_persistence.rs");
    let start = source
        .find("pub(crate) fn flush_failure_is_retryable")
        .expect("the shared classifier must exist");
    let body = &source[start..start + 240];
    assert!(
        body.contains("ErrorCode::SocketError"),
        "the classifier must match the transport class"
    );
    assert_eq!(
        body.matches("questdb::ErrorCode::").count(),
        1,
        "the classifier must match EXACTLY ONE error code. Widening it retries \
         payloads QuestDB refused on their merits, which turns one rejection \
         into two."
    );
}
