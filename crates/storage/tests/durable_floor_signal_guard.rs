//! Guard — the durable floor's loss signals can actually reach an operator.
//!
//! Three of the four detectors the design believes it has on the frame WAL
//! were dead on 2026-08-26, each for a different reason. Two are repaired in
//! the same change as this guard; this file stops them regressing.
//!
//! | # | Signal | Was | Now |
//! |---|---|---|---|
//! | 1 | `tv_ws_frame_spill_drop_critical` → composite alarm leg | no-op handle | fixed in `main.rs` (see `metrics_recorder_install_order_guard`) |
//! | 2 | `tv_ticks_lost_total` → dedicated alarm | no-op handle | same |
//! | 3 | `error!` on channel-FULL → `{ $.code = "WS-SPILL-02" }` filter | **no `code` field** | fixed here |
//! | 4 | `tv_dhan_ws_wal_dropped_total` | alive | alive |
//!
//! Signal 3 is the one this file pins directly. It is worth stating why it
//! mattered most: the `Disconnected` arm (writer thread dead) has always
//! carried the code, while the `Full` arm (writer stalled behind a saturated
//! disk) did not — and FULL is the arm production actually reaches, because a
//! stalled writer is what a disk at its throughput ceiling produces. The arm
//! that could not page was the arm most likely to fire.

use std::path::PathBuf;

fn storage_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Body of the `match` arm beginning at `arm`, up to the next arm or EOF.
fn arm_body<'a>(src: &'a str, arm: &str) -> &'a str {
    let start = src
        .find(arm)
        .unwrap_or_else(|| panic!("arm `{arm}` not found — did the match get restructured?"));
    let rest = &src[start + arm.len()..];
    let end = rest
        .find("Err(TrySendError::")
        .or_else(|| rest.find("\n    }"))
        .unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn channel_full_drop_carries_the_ws_spill_02_code() {
    let src = storage_src("ws_frame_spill.rs");
    let body = arm_body(&src, "Err(TrySendError::Full(_)) =>");

    assert!(
        body.contains("WsSpill02FrameDropped"),
        "the channel-FULL drop arm must tag its `error!` with \
         `code = ErrorCode::WsSpill02FrameDropped.code_str()`.\n\
         \n\
         Without it the CloudWatch metric filter `{{ $.code = \"WS-SPILL-02\" }}` \
         cannot match the line, and the most likely WAL-loss mode in production \
         — a writer stalled behind a saturated disk — pages nobody.\n\
         \n\
         Its sibling `Disconnected` arm has always carried the field."
    );
}

#[test]
fn both_drop_arms_are_coded_not_just_one() {
    // The asymmetry is what let this hide: reviewers saw the code on one arm
    // and read the pair as covered. Assert BOTH, so the next reader cannot
    // make the same inference from a half-covered match.
    let src = storage_src("ws_frame_spill.rs");

    for arm in [
        "Err(TrySendError::Full(_)) =>",
        "Err(TrySendError::Disconnected(_)) =>",
    ] {
        assert!(
            arm_body(&src, arm).contains("WsSpill02FrameDropped"),
            "drop arm `{arm}` must carry the WS-SPILL-02 code — a frame lost \
             through either arm is lost the same way and must page the same way"
        );
    }
}

#[test]
fn depth_spill_ceiling_is_derived_from_the_volume_not_a_literal() {
    let src = storage_src("depth_persistence.rs");

    assert!(
        src.contains("pub fn depth_spill_max_bytes()"),
        "the depth spill ceiling must be a FUNCTION derived from the volume, \
         mirroring `tick_persistence::tick_spill_max_bytes`.\n\
         \n\
         As a fixed 512 MiB it covered ~75 seconds of outage on a stream \
         measured at ~51,000 rows/s, while the tick tier — carrying 2,706 \
         rows/s — covered ~5.2 hours. The larger stream had 250x less coverage."
    );

    // The live call sites must USE it. A derived ceiling that nothing reads is
    // the same defect wearing a better name.
    //
    // Scan EVERY occurrence, not the first: `spill_failed_depth_ilp` is both
    // defined and called in this file, and `find` returns the definition. The
    // first draft of this guard did exactly that and failed against a correct
    // tree — a false positive, which is the failure mode that gets a guard
    // allowlisted rather than fixed.
    let call_sites = src.match_indices("spill_failed_depth_ilp(").count();
    assert!(
        call_sites >= 2,
        "expected a definition plus at least one call site, found {call_sites}"
    );
    let passes_derived = src
        .match_indices("spill_failed_depth_ilp(")
        .any(|(at, _)| src[at..(at + 400).min(src.len())].contains("depth_spill_max_bytes()"));
    assert!(
        passes_derived,
        "the depth rescue must pass the DERIVED ceiling, not the literal floor"
    );
}

#[test]
fn depth_spill_floor_is_retained_so_the_ceiling_can_never_shrink() {
    let src = storage_src("depth_persistence.rs");

    assert!(
        src.contains("pub const DEPTH_SPILL_MAX_BYTES: u64 = 512 * 1024 * 1024;"),
        "the 512 MiB constant must be RETAINED as the floor and the \
         unmeasurable-volume fallback. Deriving must never be able to hand back \
         LESS headroom than the fixed cap already allowed on a small volume."
    );
    assert!(
        src.contains(".max(DEPTH_SPILL_MAX_BYTES)"),
        "the derived ceiling must be clamped up to the floor"
    );
}

#[test]
fn depth_and_tick_spill_use_the_same_volume_fraction() {
    // The defect was not the number 512 MiB. It was that one tier tracked the
    // host and the other did not, so they drifted apart silently as the volume
    // grew. Pinning the fractions equal is what stops the drift recurring.
    let depth = storage_src("depth_persistence.rs");
    let tick = storage_src("tick_persistence.rs");

    let extract = |s: &str, name: &str| -> String {
        let at = s.find(name).unwrap_or_else(|| panic!("{name} not found"));
        s[at..]
            .lines()
            .next()
            .unwrap_or_default()
            .split('=')
            .nth(1)
            .unwrap_or_default()
            .trim()
            .trim_end_matches(';')
            .to_string()
    };

    let d = extract(&depth, "pub const DEPTH_SPILL_VOLUME_FRACTION: u64");
    let t = extract(&tick, "pub const TICK_SPILL_VOLUME_FRACTION: u64");
    assert_eq!(
        d, t,
        "the depth and tick spill tiers must occupy the SAME fraction of the \
         volume ({d} vs {t}) — they share one disk, and letting the fractions \
         drift is exactly how the 250x coverage gap opened"
    );
}

#[test]
fn depth_spill_max_bytes_is_at_least_the_floor_at_runtime() {
    // Behavioural, not a source scan: whatever this container's volume looks
    // like (including an unmeasurable one), the answer is never below the
    // floor.
    let got = tickvault_storage::depth_persistence::depth_spill_max_bytes();
    assert!(
        got >= tickvault_storage::depth_persistence::DEPTH_SPILL_MAX_BYTES,
        "derived depth spill ceiling {got} fell BELOW the 512 MiB floor"
    );
}

#[test]
fn depth_spill_max_bytes_is_stable_across_calls() {
    // `OnceLock`: the enforcement and the number a log line quotes can never
    // disagree, and the `df` probe runs at most once.
    let a = tickvault_storage::depth_persistence::depth_spill_max_bytes();
    let b = tickvault_storage::depth_persistence::depth_spill_max_bytes();
    assert_eq!(a, b, "the derived ceiling must resolve exactly once");
}
