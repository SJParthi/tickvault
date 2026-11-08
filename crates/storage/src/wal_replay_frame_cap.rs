//! The per-boot WAL replay frame cap in force for THIS boot (PR RD-1,
//! 2026-11-08; owner Quote 41, 2026-10-27, card "5x for evenings
//! (Recommended)").
//!
//! Every boot may replay [`WAL_REPLAY_MAX_FRAMES_PER_BOOT`] (3,000,000) frames
//! across its STAGE-C pass and every catch-up round. A boot that the evening
//! drain marked as its own planned restart may replay up to
//! [`EVENING_DRAIN_REPLAY_FRAME_CAP`] (5 x, 15,000,000). Before this module
//! that 5 x was recorded in the rule file and never built: the cap was one
//! constant, and because a single 512 MiB boot pass reads about 3.5 million
//! frames, every catch-up round on every boot was refused at round 0 and a
//! planned restart retired at most one 5-segment batch.
//!
//! The cap is one process-wide atomic, default 3,000,000. The app's catch-up
//! raises it ONCE, before the first catch-up round, with the allowance the
//! evening-drain state decided for this boot (one read-and-clear of the
//! pending flag). It is never lowered and never goes past 15,000,000, so a
//! wrong or repeated call can at worst give this one process the evening
//! slice Quote 41 already allows; the next process starts at 3,000,000 again.
//!
//! The replay pass takes the cap as an ARGUMENT
//! (`ws_frame_spill::replay_all_with_report_at`), so it stays pure and
//! testable; only the production wrapper reads the atomic.
//!
//! Cost: one atomic load per replay pass (boot, cold), one `fetch_max` per
//! boot. Never on the frame path.

use std::sync::atomic::{AtomicU64, Ordering};

use tickvault_common::error_code::ErrorCode;
use tracing::{error, warn};

use crate::ws_frame_spill::{WAL_REPLAY_FRAME_CAP_STOPS_COUNTER, WAL_REPLAY_MAX_FRAMES_PER_BOOT};

/// How many times the standard cap a planned evening-drain boot may replay
/// (Quote 41).
pub const EVENING_DRAIN_REPLAY_FRAME_CAP_MULTIPLIER: u64 = 5;

/// The frame cap of a boot the evening drain marked as its own planned
/// restart: 5 x [`WAL_REPLAY_MAX_FRAMES_PER_BOOT`] = 15,000,000 (Quote 41).
pub const EVENING_DRAIN_REPLAY_FRAME_CAP: u64 =
    EVENING_DRAIN_REPLAY_FRAME_CAP_MULTIPLIER * WAL_REPLAY_MAX_FRAMES_PER_BOOT;

const _: () = assert!(EVENING_DRAIN_REPLAY_FRAME_CAP == 15_000_000);

/// The cap in force for this process. Starts at the standard cap.
static REPLAY_FRAME_CAP_THIS_BOOT: AtomicU64 = AtomicU64::new(WAL_REPLAY_MAX_FRAMES_PER_BOOT);

/// `cap` held inside the only two values Quote 41 allows a boot:
/// never below [`WAL_REPLAY_MAX_FRAMES_PER_BOOT`], never above
/// [`EVENING_DRAIN_REPLAY_FRAME_CAP`]. Pure, O(1).
#[must_use]
pub const fn clamp_replay_frame_cap(cap: u64) -> u64 {
    if cap < WAL_REPLAY_MAX_FRAMES_PER_BOOT {
        WAL_REPLAY_MAX_FRAMES_PER_BOOT
    } else if cap > EVENING_DRAIN_REPLAY_FRAME_CAP {
        EVENING_DRAIN_REPLAY_FRAME_CAP
    } else {
        cap
    }
}

/// Raises `slot` to `clamp_replay_frame_cap(cap)` if that is higher; never
/// lowers it. Returns the cap now in force. O(1), one `fetch_max`.
fn raise_cap_in(slot: &AtomicU64, cap: u64) -> u64 {
    let wanted = clamp_replay_frame_cap(cap);
    slot.fetch_max(wanted, Ordering::AcqRel).max(wanted)
}

/// Raises this boot's replay frame cap to `cap` (clamped to 3,000,000 ..=
/// 15,000,000) and returns the cap now in force. Called once by the catch-up
/// drain, before its first round, with the evening-drain allowance; a value
/// at or below the standard cap changes nothing. Never lowers the cap.
pub fn raise_replay_frame_cap_for_planned_drain(cap: u64) -> u64 {
    raise_cap_in(&REPLAY_FRAME_CAP_THIS_BOOT, cap)
}

/// The replay frame cap in force for this boot. One atomic load.
#[must_use]
pub fn replay_frame_cap_this_boot() -> u64 {
    REPLAY_FRAME_CAP_THIS_BOOT.load(Ordering::Acquire)
}

/// `true` when `cap` is the allowance of a boot the evening drain marked as
/// its own planned restart (PR RD-4, noise lock §2.17). Exact, not a guess:
/// the cap starts at [`WAL_REPLAY_MAX_FRAMES_PER_BOOT`] in every process and
/// only [`raise_replay_frame_cap_for_planned_drain`] moves it, with the
/// allowance `evening_drain::allowance_or` decided from one read-and-clear of
/// the pending flag (unreadable state, a stale flag, a deploy, crash, manual
/// or Muhurat start all give 3,000,000). So a cap above the standard one
/// exists only on that one planned boot, and dies with the process. Pure,
/// O(1).
#[must_use]
pub(crate) const fn cap_is_planned_evening_drain(cap: u64) -> bool {
    cap > WAL_REPLAY_MAX_FRAMES_PER_BOOT
}

/// `true` (and counted, and logged with the cap actually used) when a replay
/// pass must be refused because `frames_so_far` reached `cap`. Checked before
/// a pass, never during one, so a single pass can end past the cap; the next
/// pass is then refused. O(1).
///
/// PR RD-4 (noise lock §2.17, owner card "Quiet on planned restarts
/// (Recommended)", 2026-11-08): on the planned evening-drain boot reaching
/// the cap is the expected end of that boot's slice, so the line is a coded
/// `warn!` with `source = "replay_frame_cap_planned"` (the `ws-spill-01`
/// filter matches `ERROR` only, so it does not page). Every other boot keeps
/// the `error!` and the page. The counter moves on both paths.
pub(crate) fn refuse_past_frame_cap(frames_so_far: u64, cap: u64) -> bool {
    if frames_so_far < cap {
        return false;
    }
    metrics::counter!(WAL_REPLAY_FRAME_CAP_STOPS_COUNTER).increment(1);
    if cap_is_planned_evening_drain(cap) {
        warn!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "replay_frame_cap_planned",
            frames_so_far,
            cap,
            "WAL replay paused for this boot: the planned evening restart has replayed its \
             whole frame allowance. Nothing is stranded — every unread segment stays a \
             `*.wal` file for the next boot, and the evening check decides whether to \
             restart again."
        );
        return true;
    }
    error!(
        code = ErrorCode::WsSpill01WriterRespawn.code_str(),
        source = "replay_frame_cap",
        frames_so_far,
        cap,
        "WAL replay REFUSED: this boot has already replayed the per-boot frame cap. \
         Nothing is stranded — every unread segment stays a `*.wal` file for the next \
         boot — but a backlog this size is a capacity problem, not a recovery one."
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clamp_replay_frame_cap_holds_the_two_quote_41_bounds() {
        assert_eq!(clamp_replay_frame_cap(0), WAL_REPLAY_MAX_FRAMES_PER_BOOT);
        assert_eq!(
            clamp_replay_frame_cap(WAL_REPLAY_MAX_FRAMES_PER_BOOT - 1),
            WAL_REPLAY_MAX_FRAMES_PER_BOOT
        );
        assert_eq!(clamp_replay_frame_cap(9_000_000), 9_000_000);
        assert_eq!(
            clamp_replay_frame_cap(EVENING_DRAIN_REPLAY_FRAME_CAP),
            EVENING_DRAIN_REPLAY_FRAME_CAP
        );
        assert_eq!(
            clamp_replay_frame_cap(u64::MAX),
            EVENING_DRAIN_REPLAY_FRAME_CAP
        );
    }

    #[test]
    fn test_raise_cap_in_raises_once_never_lowers_never_passes_the_evening_cap() {
        let slot = AtomicU64::new(WAL_REPLAY_MAX_FRAMES_PER_BOOT);
        // A standard boot's allowance changes nothing.
        assert_eq!(
            raise_cap_in(&slot, WAL_REPLAY_MAX_FRAMES_PER_BOOT),
            WAL_REPLAY_MAX_FRAMES_PER_BOOT
        );
        // The planned evening boot gets 15,000,000.
        assert_eq!(
            raise_cap_in(&slot, EVENING_DRAIN_REPLAY_FRAME_CAP),
            EVENING_DRAIN_REPLAY_FRAME_CAP
        );
        // A later, smaller call never lowers it.
        assert_eq!(
            raise_cap_in(&slot, WAL_REPLAY_MAX_FRAMES_PER_BOOT),
            EVENING_DRAIN_REPLAY_FRAME_CAP
        );
        // A huge value is clamped.
        let fresh = AtomicU64::new(WAL_REPLAY_MAX_FRAMES_PER_BOOT);
        assert_eq!(
            raise_cap_in(&fresh, u64::MAX),
            EVENING_DRAIN_REPLAY_FRAME_CAP
        );
        assert_eq!(
            fresh.load(Ordering::Acquire),
            EVENING_DRAIN_REPLAY_FRAME_CAP
        );
    }

    #[test]
    fn test_refuse_past_frame_cap_uses_the_cap_it_is_given() {
        // The 8 Nov 2026 boot: 3,497,726 frames after the STAGE-C pass.
        let after_boot_pass = 3_497_726;
        assert!(refuse_past_frame_cap(
            after_boot_pass,
            WAL_REPLAY_MAX_FRAMES_PER_BOOT
        ));
        assert!(!refuse_past_frame_cap(
            after_boot_pass,
            EVENING_DRAIN_REPLAY_FRAME_CAP
        ));
        assert!(refuse_past_frame_cap(
            EVENING_DRAIN_REPLAY_FRAME_CAP,
            EVENING_DRAIN_REPLAY_FRAME_CAP
        ));
        assert!(!refuse_past_frame_cap(0, WAL_REPLAY_MAX_FRAMES_PER_BOOT));
    }

    /// A `std::io::Write` sink the JSON subscriber writes into.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Runs `refuse_past_frame_cap` under a JSON subscriber shaped like the
    /// production `errors.jsonl` layer (`flatten_event(true)`: `level`,
    /// `code` and `source` at the top level, which is what the CloudWatch
    /// filter reads) and returns `(refused, every line as JSON)`.
    fn refuse_and_capture(frames_so_far: u64, cap: u64) -> (bool, Vec<serde_json::Value>) {
        let out = Captured::default();
        let sink = out.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .finish();
        let refused = tracing::subscriber::with_default(subscriber, || {
            refuse_past_frame_cap(frames_so_far, cap)
        });
        let text = String::from_utf8_lossy(
            &out.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned();
        let lines = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each captured line is JSON"))
            .collect();
        (refused, lines)
    }

    /// The `ws-spill-01` CloudWatch metric filter, read from the Terraform
    /// that ships it, evaluated on one log line: `$.code = "WS-SPILL-01" &&
    /// $.level = "ERROR"`. The test first pins that the deployed pattern is
    /// exactly that, so a wider filter fails here instead of paging quietly.
    fn ws_spill_01_filter_matches(line: &serde_json::Value) -> bool {
        let tf = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/aws/terraform/error-code-alarms.tf"
        ))
        .expect("error-code-alarms.tf is in the repository");
        let start = tf
            .find("\"ws-spill-01\" = {")
            .expect("the ws-spill-01 alarm is defined");
        let pattern_line = tf[start..]
            .lines()
            .find(|l| l.trim_start().starts_with("pattern"))
            .expect("the ws-spill-01 alarm has a pattern");
        assert_eq!(
            pattern_line.trim(),
            r#"pattern     = "{ $.code = \"WS-SPILL-01\" && $.level = \"ERROR\" }""#,
            "the ws-spill-01 filter changed; re-check noise lock §2.17 before editing this test"
        );
        line["code"] == "WS-SPILL-01" && line["level"] == "ERROR"
    }

    #[test]
    fn test_cap_is_planned_evening_drain_only_above_the_standard_cap() {
        assert!(!cap_is_planned_evening_drain(0));
        assert!(!cap_is_planned_evening_drain(
            WAL_REPLAY_MAX_FRAMES_PER_BOOT
        ));
        assert!(cap_is_planned_evening_drain(
            WAL_REPLAY_MAX_FRAMES_PER_BOOT + 1
        ));
        assert!(cap_is_planned_evening_drain(EVENING_DRAIN_REPLAY_FRAME_CAP));
        // Every cap the process can hold that is not the planned one.
        assert!(!cap_is_planned_evening_drain(clamp_replay_frame_cap(0)));
    }

    /// PR RD-4 (noise lock §2.17): the planned evening-drain boot logs the
    /// cap refusal as a coded WARN with `source = "replay_frame_cap_planned"`,
    /// which the `ws-spill-01` filter does not match (no page); every other
    /// boot keeps the coded ERROR with `source = "replay_frame_cap"`, which it
    /// does match (pages). Both still refuse the pass.
    #[test]
    fn test_regression_rd4_planned_cap_refusal_warns_and_unplanned_errors() {
        // Planned evening-drain boot: 15M replayed under the 15M cap.
        let (refused, lines) = refuse_and_capture(
            EVENING_DRAIN_REPLAY_FRAME_CAP,
            EVENING_DRAIN_REPLAY_FRAME_CAP,
        );
        assert!(refused, "the planned boot still stops at its cap");
        assert_eq!(lines.len(), 1, "{lines:?}");
        let planned = &lines[0];
        assert_eq!(planned["level"], "WARN", "{planned}");
        assert_eq!(planned["code"], "WS-SPILL-01", "{planned}");
        assert_eq!(planned["source"], "replay_frame_cap_planned", "{planned}");
        assert_eq!(planned["cap"], EVENING_DRAIN_REPLAY_FRAME_CAP, "{planned}");
        assert!(
            !ws_spill_01_filter_matches(planned),
            "the planned-restart line must not page: {planned}"
        );

        // Any other boot (deploy, crash, manual, Muhurat, unreadable state):
        // the 8 Nov 2026 shape, 3.5M frames after the boot pass, 3M cap.
        let (refused, lines) = refuse_and_capture(3_497_726, WAL_REPLAY_MAX_FRAMES_PER_BOOT);
        assert!(refused);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let unplanned = &lines[0];
        assert_eq!(unplanned["level"], "ERROR", "{unplanned}");
        assert_eq!(unplanned["code"], "WS-SPILL-01", "{unplanned}");
        assert_eq!(unplanned["source"], "replay_frame_cap", "{unplanned}");
        assert!(
            ws_spill_01_filter_matches(unplanned),
            "an unplanned boot at the cap must still page: {unplanned}"
        );

        // Under the cap: no refusal, no line at all.
        let (refused, lines) = refuse_and_capture(0, EVENING_DRAIN_REPLAY_FRAME_CAP);
        assert!(!refused);
        assert!(lines.is_empty(), "{lines:?}");
    }

    /// The PRODUCTION wrapper runs under the cap in force, not the constant:
    /// after a planned evening-drain boot raised it, a boot that already
    /// replayed 3.5M frames (the 8 Nov 2026 STAGE-C pass) is not refused on
    /// the frame cap. Raises the global cap BEFORE it adds the frames, so a
    /// test running beside it never sees 3.5M frames under a 3M cap.
    #[test]
    fn test_regression_fenced_replay_runs_under_the_raised_cap_not_the_constant() {
        let dir = std::env::temp_dir().join(format!(
            "tv-rd1-fenced-cap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert_eq!(
            raise_replay_frame_cap_for_planned_drain(EVENING_DRAIN_REPLAY_FRAME_CAP),
            EVENING_DRAIN_REPLAY_FRAME_CAP
        );
        crate::ws_frame_spill::WAL_REPLAY_FRAMES_THIS_BOOT.fetch_add(3_500_000, Ordering::AcqRel);
        let batch = crate::ws_frame_spill::replay_all_with_report_fenced(
            &dir,
            crate::ws_frame_spill::WAL_REPLAY_MAX_BYTES,
        )
        .expect("a refusal is Ok, not Err");
        assert!(
            !batch.stopped_for_frame_cap,
            "3.5M frames are under the raised 15M cap; a refusal here means the wrapper \
             still passes the 3M constant"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The global cap only moves inside the two bounds, whatever other tests
    /// in this binary did to it.
    #[test]
    fn test_raise_replay_frame_cap_for_planned_drain_keeps_replay_frame_cap_this_boot_in_bounds() {
        let before = replay_frame_cap_this_boot();
        assert!(
            (WAL_REPLAY_MAX_FRAMES_PER_BOOT..=EVENING_DRAIN_REPLAY_FRAME_CAP).contains(&before)
        );
        let now = raise_replay_frame_cap_for_planned_drain(0);
        assert!(now >= before, "a call never lowers the cap");
        assert!(now <= EVENING_DRAIN_REPLAY_FRAME_CAP);
    }
}
