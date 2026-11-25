//! B3 (2026-11-13, production audit finding 12) drain tests for the
//! `drain_frame_post` hot-path stage. A child of `dhan_feed_stack.rs`'s
//! `mod tests`, moved to its own file to keep `dhan_feed_stack.rs` under its
//! size pin (`source_size_ratchet_guard`).

use super::*;

/// Regression: 2026-11-13 (B3, audit finding). `drain_main_frame`,
/// `drain_main_tick` and `receipt_to_fold` stop their clock when the fold
/// returns, and the frame arm then runs the size-trigger and depth
/// hand-offs (a spare-less hand-off reserves and touches a fresh staged
/// batch on this thread) and the 1,024-frame publish, which no stage
/// timed. The drain now records `drain_frame_post` once per frame,
/// starting where `drain_main_frame` ends.
///
/// Two halves: every frame of EVERY endpoint records one sample (B3
/// review: the first version sent only main-feed frames, so a record
/// wrapped in a main-feed-only `if` passed; the stage is process-wide
/// and other drain tests share it, so the delta is checked as at least
/// one per frame, and the structural half below is the exact check), and
/// the production source starts the stage at the main-feed frame's end
/// instant and records it AFTER the size triggers and the publish, so the
/// hand-offs are inside, at the frame arm's own brace depth (never under
/// an `if` or a `match` arm).
#[tokio::test]
async fn test_regression_b3_drain_records_the_post_frame_stage_after_the_hand_offs() {
    use tickvault_storage::hot_path_telemetry::{Stage, stage_snapshot};
    const MAIN_FRAMES: u64 = 3;
    // Main feed, plus one depth-20, one depth-200 and one order-update
    // frame (no depth sink here, so the depth frames are counted
    // unconsumed; the post stage records for them all the same).
    const FRAMES: u64 = MAIN_FRAMES + 3;
    let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
    let (tx, rx) = tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(8);
    let ltt = todays_in_session_ltt();
    for (k, endpoint, bytes) in [
        (
            10_u64,
            DhanEndpointType::Depth20,
            depth20_packet(1_001, 2, 41),
        ),
        (
            11,
            DhanEndpointType::Depth200,
            depth200_packet(1_002, 2, 41, 4),
        ),
        (12, DhanEndpointType::OrderUpdate, b"{}".to_vec()),
    ] {
        tx.try_send(CapturedFrame {
            seq: 600 + k,
            endpoint,
            connection_index: 6,
            received_at: std::time::Instant::now(),
            received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(
                std::time::Instant::now(),
            ),
            wal_backed: true,
            bytes: bytes::Bytes::from(bytes),
        })
        .expect("the ring must accept a non-main frame");
    }
    for k in 0..MAIN_FRAMES {
        tx.try_send(CapturedFrame {
            seq: 600 + k,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at: std::time::Instant::now(),
            received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(
                std::time::Instant::now(),
            ),
            wal_backed: true,
            bytes: bytes::Bytes::copy_from_slice(&ticker_packet(
                13,
                23_146.45,
                ltt.saturating_add(u32::try_from(k).unwrap_or(0)),
            )),
        })
        .expect("the ring must accept a frame");
    }
    drop(tx);
    let before = stage_snapshot(Stage::DrainFramePost).count;
    let drained = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_frame_drain(
            rx,
            ingest,
            Arc::new(RingByteBudget::new(MAIN_FEED_RING_MAX_BYTES)),
            Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
            Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new()),
            tokio::sync::mpsc::channel(1).1,
        ),
    )
    .await
    .expect("the drain must end when the ring closes");
    let after = stage_snapshot(Stage::DrainFramePost).count;
    assert_eq!(drained.folded, MAIN_FRAMES, "every main-feed frame folded");
    assert_eq!(
        drained.depth_unconsumed, 2,
        "both depth frames reached the drain"
    );
    assert!(
        after.saturating_sub(before) >= FRAMES,
        "each frame of every endpoint must record one drain_frame_post \
         sample: {before} -> {after} for {FRAMES} frames"
    );

    let src = include_str!("../../dhan_feed_stack.rs");
    let prod = src.split(concat!("#[cfg(", "test)]")).next().unwrap_or(src);
    let drain = &prod[prod.find("async fn run_frame_drain(").expect("drain")..];
    let frame_end = drain
        .find("let frame_work_end = std::time::Instant::now();")
        .expect("the main-feed frame timer");
    let starts_there = drain[frame_end..]
        .find("post_start = Some(frame_work_end);")
        .expect("the post stage must start at the frame's end instant");
    let size_trigger = drain
        .find("if ingest.pending_rows() >= FLUSH_ROW_THRESHOLD {")
        .expect("the tick size trigger");
    let depth_trigger = drain
        .find("if ingest.depth_flush_due() {")
        .expect("the depth size trigger");
    let publish = drain[depth_trigger..]
        .find("publish_fold_depth(&ingest);")
        .map(|at| depth_trigger + at)
        .expect("the frame arm's publish");
    let recorded = drain
        .find("Stage::DrainFramePost,")
        .expect("the post stage is recorded in the drain");
    let start_taken = drain
        .find("let post_start = post_start.unwrap_or(frame_work_start);")
        .expect("the post stage start is fixed before the hand-offs");
    assert!(frame_end + starts_there < size_trigger);
    assert!(
        start_taken < size_trigger && size_trigger < depth_trigger,
        "the start is taken before the tick and depth hand-offs"
    );
    assert!(
        publish < recorded,
        "the post stage is recorded after the hand-offs and the publish"
    );
    assert_eq!(
        post_record_brace_depth(drain),
        Some(0),
        "the drain_frame_post record must sit at the frame arm's own brace \
         depth, outside every `if` and `match` arm, so every frame of \
         every endpoint records it"
    );
}

/// The brace depth of the `drain_frame_post` record relative to the line
/// that fixes its start, or `None` when the text between them leaves the
/// block (depth goes negative) or either marker is missing. Comment text
/// after `//` is ignored. 0 means the record runs for every frame that
/// reaches the start line.
fn post_record_brace_depth(drain: &str) -> Option<i64> {
    let start_line = "let post_start = post_start.unwrap_or(frame_work_start);";
    let from = drain.find(start_line)? + start_line.len();
    let to = from + drain[from..].find("Stage::DrainFramePost,")?;
    let mut depth = 0_i64;
    for line in drain[from..to].lines() {
        let code = line.split("//").next().unwrap_or("");
        for ch in code.chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth < 0 {
                return None;
            }
        }
    }
    Some(depth)
}

/// The brace-depth check bites: wrapping the record in a main-feed-only
/// `if` (the shape that would put depth decode back in no stage) reads
/// depth 1, and a record moved outside the frame arm reads `None`.
#[test]
fn test_post_record_brace_depth_bites_on_a_main_feed_only_record() {
    let src = include_str!("../../dhan_feed_stack.rs");
    let prod = src.split(concat!("#[cfg(", "test)]")).next().unwrap_or(src);
    let drain = &prod[prod.find("async fn run_frame_drain(").expect("drain")..];
    assert_eq!(post_record_brace_depth(drain), Some(0));
    let call = "tickvault_storage::hot_path_telemetry::record_stage(\n                    \
                tickvault_storage::hot_path_telemetry::Stage::DrainFramePost,";
    assert!(drain.contains(call), "the record call's shape moved");
    let gated = drain.replacen(
        call,
        &format!("if matches!(frame.endpoint, DhanEndpointType::MainFeed) {{ {call}"),
        1,
    );
    assert_eq!(post_record_brace_depth(&gated), Some(1));
    let outside = drain.replacen(call, &format!("}} {call}"), 1);
    assert_eq!(post_record_brace_depth(&outside), None);
}

/// Regression: 2026-11-13 (B3 review round 3). A depth frame's own
/// handling (`drain_depth_frame`, O(levels), up to 200 levels, plus the
/// staged growth and the ghost resend) was timed by no stage: the post
/// stage started AFTER it with a fresh clock read. A non-main-feed frame's
/// post sample now starts at the frame timer's start, so that handling is
/// inside `drain_frame_post`. Pinned on the production source: the
/// fallback is the timer's instant, the depth arm sets no start of its
/// own, and the timer is taken before the endpoint match.
#[test]
fn test_regression_b3_depth_frame_handling_is_inside_the_post_stage() {
    let src = include_str!("../../dhan_feed_stack.rs");
    let prod = src.split(concat!("#[cfg(", "test)]")).next().unwrap_or(src);
    let drain = &prod[prod.find("async fn run_frame_drain(").expect("drain")..];
    let timer = drain
        .find("let frame_work_start = std::time::Instant::now();")
        .expect("the frame timer");
    let matched = timer
        + drain[timer..]
            .find("match frame.endpoint {")
            .expect("the endpoint match after the timer");
    let depth_arm = matched
        + drain[matched..]
            .find("DhanEndpointType::Depth20 | DhanEndpointType::Depth200 => {")
            .expect("the depth arm");
    let fallback = drain
        .find("let post_start = post_start.unwrap_or(frame_work_start);")
        .expect("a non-main frame's post stage starts at the frame timer's start");
    assert!(timer < matched && matched < depth_arm && depth_arm < fallback);
    let depth_body = &drain[depth_arm..fallback];
    assert!(
        !depth_body.contains("post_start ="),
        "the depth arm must not move the post stage start past its own handling"
    );
    assert!(
        !drain.contains("post_start.unwrap_or_else(std::time::Instant::now)"),
        "a fresh clock read after the depth handling leaves it in no stage"
    );
}
