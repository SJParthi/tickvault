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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

/// Set once `evening_drain::claim_planned_boot` has claimed this process as
/// the evening drain's planned restart (2026-11-11, noise lock §2.17 note).
/// Never cleared: one process is one boot. It exists because the boot's
/// STAGE-C replay runs BEFORE the catch-up drain raises the frame cap, so
/// the cap alone cannot tell the STAGE-C pass it is on the planned boot.
static PLANNED_EVENING_DRAIN_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Records `planned` into `slot` (only a `true` is stored; never cleared).
/// O(1).
fn note_claim_in(slot: &AtomicBool, planned: bool) {
    if planned {
        slot.store(true, Ordering::Release);
    }
}

/// Records the evening drain's planned-restart verdict for this process, the
/// verdict `claim_planned_boot` took from one read-and-clear of the mark
/// before the WAL was locked. Called by that claim, once. O(1).
pub fn note_planned_evening_drain_claim(planned: bool) {
    note_claim_in(&PLANNED_EVENING_DRAIN_CLAIMED, planned);
}

/// Whether a boot is the planned evening-drain restart: its frame cap is the
/// evening allowance, or its claim said so (before the cap is raised). Pure,
/// O(1).
const fn planned_boot_in(claimed: bool, cap: u64) -> bool {
    claimed || cap_is_planned_evening_drain(cap)
}

/// `true` on the boot the evening drain marked as its own planned restart
/// (noise lock §2.17 and its 2026-11-11 note): the replay frame cap in force
/// is the evening allowance, or the claim recorded it. Every other boot
/// (deploy, crash, manual, Muhurat, an unreadable state file) is `false`.
/// Two atomic loads, O(1).
#[must_use]
pub fn planned_evening_drain_boot() -> bool {
    planned_boot_in(
        PLANNED_EVENING_DRAIN_CLAIMED.load(Ordering::Acquire),
        replay_frame_cap_this_boot(),
    )
}

/// Whether a replay-limit line (the frame cap, the catch-up deferral, a
/// byte-budget deferral) pages on this boot (noise lock §2.17 and §2.20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayLimitVerdict {
    /// Pages: an `error!` the `ws-spill-01` / `ws-spill-02` filters match.
    Page,
    /// The evening drain's planned restart (§2.17): a coded `warn!`,
    /// `source` suffix `_planned`.
    PlannedRestart,
    /// The waiting backlog is strictly lower than the previous start's
    /// (§2.20, owner card "Page only if stuck (Recommended)"): a coded
    /// `warn!`, `source` suffix `_shrinking`.
    BacklogShrinking { previous: u64, current: u64 },
}

/// The §2.17 / §2.20 decision: the planned restart wins, then a shrinking
/// backlog; anything else (no judgement, first start, unknown or unchanged
/// or grown count) pages. Pure, O(1).
#[must_use]
pub const fn replay_limit_verdict(
    planned: bool,
    trend: Option<crate::wal_backlog_trend::BacklogTrend>,
) -> ReplayLimitVerdict {
    if planned {
        return ReplayLimitVerdict::PlannedRestart;
    }
    match trend {
        Some(crate::wal_backlog_trend::BacklogTrend::Shrinking { previous, current }) => {
            ReplayLimitVerdict::BacklogShrinking { previous, current }
        }
        _ => ReplayLimitVerdict::Page,
    }
}

/// This boot's verdict: [`planned_evening_drain_boot`] and the backlog trend
/// `main` judged before the boot replay
/// (`wal_backlog_trend::boot_backlog_trend`). Two atomic loads and one
/// `OnceLock` read, O(1).
#[must_use]
pub fn replay_limit_verdict_this_boot() -> ReplayLimitVerdict {
    replay_limit_verdict(
        planned_evening_drain_boot(),
        crate::wal_backlog_trend::boot_backlog_trend(),
    )
}

/// `(previous, current)` backlog counts for a log line; `(-1, -1)` when the
/// verdict carries none. O(1).
const fn backlog_fields(verdict: ReplayLimitVerdict) -> (i64, i64) {
    match verdict {
        ReplayLimitVerdict::BacklogShrinking { previous, current } => {
            // APPROVED: cast -- a segment count is far below i64::MAX.
            (previous as i64, current as i64)
        }
        _ => (-1, -1),
    }
}

/// Reports the catch-up drain's once-per-boot `WS-SPILL-01` deferral line
/// (`source = "replay_deferred"`). On the planned evening-drain boot the
/// line is a coded `warn!` with `source = "replay_deferred_planned"`, which
/// the `ws-spill-01` filter (`ERROR` only) does not match: the boot's slice
/// ending with segments left is expected, and nothing is lost (a deferred
/// segment stays a `*.wal` file). Every other boot keeps the `error!` and the
/// page, with the same fields and text as before; only the log `target`
/// moved (now this module, was `tickvault_app::dhan_feed_stack`), and no
/// alarm filter keys on it (2026-11-11, noise lock §2.17 note; Verified live
/// 2026-11-09: every planned restart paged on this line). Cold, at most once
/// per boot.
///
/// 2026-11-11 (noise lock §2.20): on any boot whose waiting backlog is
/// strictly lower than the previous start's, a coded `warn!` with `source =
/// "replay_deferred_shrinking"` (no page).
pub fn report_catchup_replay_deferred(
    verdict: ReplayLimitVerdict,
    deferred_segments: u64,
    bytes_replayed: u64,
    budget_bytes: u64,
    rounds: u32,
) {
    if let ReplayLimitVerdict::BacklogShrinking { .. } = verdict {
        let (previous_backlog_segments, backlog_segments) = backlog_fields(verdict);
        warn!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "replay_deferred_shrinking",
            deferred_segments,
            bytes_replayed,
            budget_bytes,
            rounds,
            previous_backlog_segments,
            backlog_segments,
            "WAL replay left segments for the next boot, and the waiting backlog is \
             smaller than at the previous start, so it is draining. Nothing is stranded \
             (a deferred segment stays a `*.wal` file). Reported once per boot."
        );
        return;
    }
    if verdict == ReplayLimitVerdict::PlannedRestart {
        warn!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "replay_deferred_planned",
            deferred_segments,
            bytes_replayed,
            budget_bytes,
            rounds,
            "WAL replay left segments for the next boot: the planned evening restart \
             reached its RAM budget for this boot. Nothing is stranded (a deferred segment \
             stays a `*.wal` file), and the evening check decides whether to restart \
             again. Reported once per boot."
        );
        return;
    }
    error!(
        code = ErrorCode::WsSpill01WriterRespawn.code_str(),
        source = "replay_deferred",
        deferred_segments,
        bytes_replayed,
        budget_bytes,
        rounds,
        "WAL replay DEFERRED segments past its RAM budget — nothing is stranded \
         (a deferred segment stays a `*.wal` file and is re-globbed next round or \
         next boot), but those frames are on disk rather than in the database \
         until then. A deferral on every boot means the WAL is growing faster \
         than replay drains it: a capacity problem, not a recovery one. Reported \
         once per boot."
    );
}

/// What one replay pass left behind on its byte or memory budget.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplayBudgetDeferral {
    pub(crate) deferred_segments: usize,
    pub(crate) consumed_segments: usize,
    pub(crate) bytes_held: usize,
    pub(crate) budget_bytes: usize,
    pub(crate) stopped_for_memory: bool,
    pub(crate) stop_pct: u64,
}

/// Reports a replay pass's `WS-SPILL-02` budget deferral. On the planned
/// evening-drain boot a BYTE-budget stop is a coded `warn!` with `source =
/// "replay_budget_deferred_planned"` (the `ws-spill-02` filter matches
/// `ERROR` only, so it does not page). A MEMORY-guard stop
/// (`stopped_for_memory`) keeps the `error!` and the page on every boot,
/// the planned one included (2026-11-11 review 2): its remedy differs (the
/// host cannot hold the frames; raising the budget makes it worse), and on
/// the boot running the 5x frame allowance it is the one sign the owner
/// would get. Every other boot keeps the `error!` with the same fields and
/// text as before (no `source` field; the log `target` is now this module).
/// The counter is moved by the caller on both paths. Cold, once per replay
/// pass that defers.
///
/// 2026-11-11 (noise lock §2.20): a BYTE-budget stop on a boot whose waiting
/// backlog is shrinking is a coded `warn!` with `source =
/// "replay_budget_deferred_shrinking"`; a memory-guard stop still pages.
pub(crate) fn report_replay_budget_deferred(verdict: ReplayLimitVerdict, d: ReplayBudgetDeferral) {
    let ReplayBudgetDeferral {
        deferred_segments,
        consumed_segments,
        bytes_held,
        budget_bytes,
        stopped_for_memory,
        stop_pct,
    } = d;
    if !stopped_for_memory && let ReplayLimitVerdict::BacklogShrinking { .. } = verdict {
        let (previous_backlog_segments, backlog_segments) = backlog_fields(verdict);
        warn!(
            code = ErrorCode::WsSpill02FrameDropped.code_str(),
            source = "replay_budget_deferred_shrinking",
            deferred_segments,
            consumed_segments,
            bytes_held,
            budget_bytes,
            stopped_for_memory,
            stop_pct,
            previous_backlog_segments,
            backlog_segments,
            "WAL replay left the remaining segments for the next boot (byte budget), and \
             the waiting backlog is smaller than at the previous start, so it is draining. \
             Nothing is stranded: each one stays a `*.wal` file."
        );
        return;
    }
    if verdict == ReplayLimitVerdict::PlannedRestart && !stopped_for_memory {
        warn!(
            code = ErrorCode::WsSpill02FrameDropped.code_str(),
            source = "replay_budget_deferred_planned",
            deferred_segments,
            consumed_segments,
            bytes_held,
            budget_bytes,
            stopped_for_memory,
            stop_pct,
            "WAL replay left the remaining segments for the next boot on the planned \
             evening restart (byte budget). Nothing is stranded: each one stays a \
             `*.wal` file."
        );
        return;
    }
    error!(
        code = ErrorCode::WsSpill02FrameDropped.code_str(),
        deferred_segments,
        consumed_segments,
        bytes_held,
        budget_bytes,
        stopped_for_memory,
        stop_pct,
        "WAL replay DEFERRED the remaining segments to the next boot — on \
         the MEMORY guard when stopped_for_memory=true, otherwise on the \
         byte budget. Nothing is stranded: an unconsumed segment stays a \
         `*.wal` file and is re-globbed next boot, but those frames are on \
         disk rather than in the database until then. The two have DIFFERENT \
         remedies, which is why the flag is here: a byte-budget stop means \
         raise the budget or drain more often; a memory stop means the \
         frames materialize into more rows than this host can hold at once, \
         and raising the budget would make it worse, not better."
    );
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
///
/// 2026-11-11 (noise lock §2.20): on any other boot whose waiting backlog is
/// strictly lower than the previous start's, a coded `warn!` with `source =
/// "replay_frame_cap_shrinking"` (no page).
pub(crate) fn refuse_past_frame_cap(frames_so_far: u64, cap: u64) -> bool {
    refuse_past_frame_cap_as(
        frames_so_far,
        cap,
        replay_limit_verdict(
            cap_is_planned_evening_drain(cap),
            crate::wal_backlog_trend::boot_backlog_trend(),
        ),
    )
}

/// [`refuse_past_frame_cap`] with the verdict given (tests drive every arm
/// without the process judgement). O(1).
fn refuse_past_frame_cap_as(frames_so_far: u64, cap: u64, verdict: ReplayLimitVerdict) -> bool {
    if frames_so_far < cap {
        return false;
    }
    metrics::counter!(WAL_REPLAY_FRAME_CAP_STOPS_COUNTER).increment(1);
    if let ReplayLimitVerdict::BacklogShrinking { .. } = verdict {
        let (previous_backlog_segments, backlog_segments) = backlog_fields(verdict);
        warn!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "replay_frame_cap_shrinking",
            frames_so_far,
            cap,
            previous_backlog_segments,
            backlog_segments,
            "WAL replay paused for this boot at the per-boot frame cap, and the waiting \
             backlog is smaller than at the previous start, so it is draining. Nothing is \
             stranded — every unread segment stays a `*.wal` file for the next boot."
        );
        return true;
    }
    if verdict == ReplayLimitVerdict::PlannedRestart {
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

    /// Runs `f` under the same `errors.jsonl`-shaped JSON subscriber as
    /// `refuse_and_capture` and returns every line as JSON.
    fn capture(f: impl FnOnce()) -> Vec<serde_json::Value> {
        let out = Captured::default();
        let sink = out.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8_lossy(
            &out.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each captured line is JSON"))
            .collect()
    }

    /// The `ws-spill-02` filter, read from the Terraform that ships it,
    /// evaluated on one line (same shape as `ws_spill_01_filter_matches`).
    fn ws_spill_02_filter_matches(line: &serde_json::Value) -> bool {
        let tf = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/aws/terraform/error-code-alarms.tf"
        ))
        .expect("error-code-alarms.tf is in the repository");
        let start = tf
            .find("\"ws-spill-02\" = {")
            .expect("the ws-spill-02 alarm is defined");
        let pattern_line = tf[start..]
            .lines()
            .find(|l| l.trim_start().starts_with("pattern"))
            .expect("the ws-spill-02 alarm has a pattern");
        assert_eq!(
            pattern_line.trim(),
            r#"pattern     = "{ $.code = \"WS-SPILL-02\" && $.level = \"ERROR\" }""#,
            "the ws-spill-02 filter changed; re-check noise lock §2.17 before editing this test"
        );
        line["code"] == "WS-SPILL-02" && line["level"] == "ERROR"
    }

    // Regression: 2026-11-11 — noise lock §2.17 note. Live on 2026-11-09
    // every planned evening-drain restart paged on the catch-up's
    // once-per-boot deferral line. Planned boot → coded WARN with
    // `replay_deferred_planned` (no page); any other boot → the same ERROR.
    #[test]
    fn test_regression_report_catchup_replay_deferred_warns_only_on_the_planned_boot() {
        let planned = capture(|| {
            report_catchup_replay_deferred(
                ReplayLimitVerdict::PlannedRestart,
                3,
                536_870_912,
                536_870_912,
                2,
            )
        });
        assert_eq!(planned.len(), 1, "{planned:?}");
        assert_eq!(planned[0]["level"], "WARN", "{}", planned[0]);
        assert_eq!(planned[0]["code"], "WS-SPILL-01", "{}", planned[0]);
        assert_eq!(
            planned[0]["source"], "replay_deferred_planned",
            "{}",
            planned[0]
        );
        assert_eq!(planned[0]["deferred_segments"], 3, "{}", planned[0]);
        assert!(
            !ws_spill_01_filter_matches(&planned[0]),
            "a planned boot does not page"
        );

        let unplanned = capture(|| {
            report_catchup_replay_deferred(ReplayLimitVerdict::Page, 3, 536_870_912, 536_870_912, 2)
        });
        assert_eq!(unplanned.len(), 1, "{unplanned:?}");
        assert_eq!(unplanned[0]["level"], "ERROR", "{}", unplanned[0]);
        assert_eq!(unplanned[0]["code"], "WS-SPILL-01", "{}", unplanned[0]);
        assert_eq!(
            unplanned[0]["source"], "replay_deferred",
            "{}",
            unplanned[0]
        );
        assert!(
            ws_spill_01_filter_matches(&unplanned[0]),
            "any other boot still pages"
        );
    }

    // Regression: 2026-11-11 — noise lock §2.17 note. The replay pass's
    // budget deferral (WS-SPILL-02) paged on every planned restart too.
    #[test]
    fn test_regression_report_replay_budget_deferred_warns_only_on_the_planned_boot() {
        let deferral = ReplayBudgetDeferral {
            deferred_segments: 7,
            consumed_segments: 5,
            bytes_held: 536_870_912,
            budget_bytes: 536_870_912,
            stopped_for_memory: false,
            stop_pct: 60,
        };
        let planned =
            capture(|| report_replay_budget_deferred(ReplayLimitVerdict::PlannedRestart, deferral));
        assert_eq!(planned.len(), 1, "{planned:?}");
        assert_eq!(planned[0]["level"], "WARN", "{}", planned[0]);
        assert_eq!(planned[0]["code"], "WS-SPILL-02", "{}", planned[0]);
        assert_eq!(
            planned[0]["source"], "replay_budget_deferred_planned",
            "{}",
            planned[0]
        );
        assert_eq!(planned[0]["deferred_segments"], 7, "{}", planned[0]);
        assert!(
            !ws_spill_02_filter_matches(&planned[0]),
            "a planned boot does not page"
        );

        let unplanned =
            capture(|| report_replay_budget_deferred(ReplayLimitVerdict::Page, deferral));
        assert_eq!(unplanned.len(), 1, "{unplanned:?}");
        assert_eq!(unplanned[0]["level"], "ERROR", "{}", unplanned[0]);
        assert_eq!(unplanned[0]["code"], "WS-SPILL-02", "{}", unplanned[0]);
        assert!(
            unplanned[0].get("source").is_none(),
            "unchanged: no source field"
        );
        assert!(
            ws_spill_02_filter_matches(&unplanned[0]),
            "any other boot still pages"
        );
    }

    // Regression: 2026-11-11 — PR AA-G review 2. A MEMORY-guard stop has a
    // different remedy from a byte-budget stop (the host cannot hold the
    // frames). The first version quieted it on the planned boot too, so the
    // boot running the 5x frame allowance could hit the memory guard every
    // pass with no page. It keeps the ERROR and the page on every boot.
    #[test]
    fn test_regression_memory_guard_deferral_pages_even_on_the_planned_boot() {
        let deferral = ReplayBudgetDeferral {
            deferred_segments: 9,
            consumed_segments: 1,
            bytes_held: 1_048_576,
            budget_bytes: 536_870_912,
            stopped_for_memory: true,
            stop_pct: 80,
        };
        let planned =
            capture(|| report_replay_budget_deferred(ReplayLimitVerdict::PlannedRestart, deferral));
        assert_eq!(planned.len(), 1, "{planned:?}");
        assert_eq!(planned[0]["level"], "ERROR", "{}", planned[0]);
        assert_eq!(planned[0]["code"], "WS-SPILL-02", "{}", planned[0]);
        assert_eq!(planned[0]["stopped_for_memory"], true, "{}", planned[0]);
        assert!(
            planned[0].get("source").is_none(),
            "the unchanged ERROR line, not the planned WARN"
        );
        assert!(
            ws_spill_02_filter_matches(&planned[0]),
            "a memory-guard stop pages on the planned boot too"
        );
    }

    /// The planned test: the claim (before the cap is raised) or the raised
    /// cap; never anything else. Driven on a private slot so the process
    /// flag is never set in this test binary.
    #[test]
    fn test_note_planned_evening_drain_claim_and_planned_evening_drain_boot() {
        let slot = AtomicBool::new(false);
        note_claim_in(&slot, false);
        assert!(
            !slot.load(Ordering::Acquire),
            "an unplanned claim records nothing"
        );
        assert!(!planned_boot_in(
            slot.load(Ordering::Acquire),
            WAL_REPLAY_MAX_FRAMES_PER_BOOT
        ));
        assert!(
            planned_boot_in(false, EVENING_DRAIN_REPLAY_FRAME_CAP),
            "the raised cap alone is the planned boot"
        );
        note_claim_in(&slot, true);
        assert!(
            planned_boot_in(slot.load(Ordering::Acquire), WAL_REPLAY_MAX_FRAMES_PER_BOOT),
            "the claim alone is the planned boot (STAGE-C, before the raise)"
        );
        note_claim_in(&slot, false);
        assert!(
            slot.load(Ordering::Acquire),
            "never cleared within a process"
        );
        // The process accessor (claim arm included) is pinned in its own test
        // process, where the flag may be set:
        // `tests/planned_evening_drain_claim_process.rs` (2026-11-11 review 2:
        // the equality that stood here repeated the accessor's body).
        // `note_planned_evening_drain_claim(false)` is the only process call
        // a test may make: it can never mark this binary as planned.
        note_planned_evening_drain_claim(false);
        assert!(!PLANNED_EVENING_DRAIN_CLAIMED.load(Ordering::Acquire));
    }

    /// The replay pass routes its budget deferral through the reporter with
    /// the process planned verdict (source scan; a behavioural test would
    /// have to set the process flag for every other test in this binary).
    #[test]
    fn test_replay_budget_deferral_is_reported_with_the_planned_verdict() {
        let src = include_str!("ws_frame_spill.rs");
        let prod = src.split("\n#[cfg(test)]\nmod ").next().unwrap_or(src);
        assert_eq!(
            prod.matches("crate::wal_replay_frame_cap::report_replay_budget_deferred(")
                .count(),
            1
        );
        let at = prod
            .find("crate::wal_replay_frame_cap::report_replay_budget_deferred(")
            .expect("the call");
        assert!(
            prod[at..at + 200]
                .contains("crate::wal_replay_frame_cap::replay_limit_verdict_this_boot()"),
            "it passes the process verdict (planned restart or shrinking backlog, §2.20)"
        );
        assert!(
            !prod.contains("WAL replay DEFERRED the remaining segments"),
            "the line lives only in the reporter now"
        );
    }

    // ---- noise lock §2.20 (2026-11-11, "Page only if stuck (Recommended)") ----

    use crate::wal_backlog_trend::BacklogTrend;

    const SHRINKING: ReplayLimitVerdict = ReplayLimitVerdict::BacklogShrinking {
        previous: 290,
        current: 285,
    };

    /// Every trend × planned: the planned restart wins, then only a strict
    /// shrink is quiet; no judgement at all pages.
    #[test]
    fn test_replay_limit_verdict_planned_wins_then_shrinking_else_page() {
        let trends = [
            None,
            Some(BacklogTrend::FirstStart),
            Some(BacklogTrend::Unreadable),
            Some(BacklogTrend::FutureDated),
            Some(BacklogTrend::PreviousUnknown),
            Some(BacklogTrend::CurrentUnknown),
            Some(BacklogTrend::Unchanged {
                previous: 7,
                current: 7,
            }),
            Some(BacklogTrend::Grew {
                previous: 7,
                current: 8,
            }),
            Some(BacklogTrend::Shrinking {
                previous: 8,
                current: 7,
            }),
        ];
        for trend in trends {
            assert_eq!(
                replay_limit_verdict(true, trend),
                ReplayLimitVerdict::PlannedRestart,
                "{trend:?}"
            );
            let expected = match trend {
                Some(BacklogTrend::Shrinking { previous, current }) => {
                    ReplayLimitVerdict::BacklogShrinking { previous, current }
                }
                _ => ReplayLimitVerdict::Page,
            };
            assert_eq!(replay_limit_verdict(false, trend), expected, "{trend:?}");
        }
    }

    /// No test in this binary judges the backlog, so the process verdict is
    /// never `BacklogShrinking` here: a lane started without the boot
    /// judgement pages (the planned arm may be set by the fenced-replay test).
    #[test]
    fn test_replay_limit_verdict_this_boot_without_a_judgement_is_never_shrinking() {
        assert_eq!(crate::wal_backlog_trend::boot_backlog_trend(), None);
        let v = replay_limit_verdict_this_boot();
        assert!(
            matches!(
                v,
                ReplayLimitVerdict::Page | ReplayLimitVerdict::PlannedRestart
            ),
            "{v:?}"
        );
    }

    /// The frame-cap refusal under each verdict: shrinking and planned are
    /// coded WARNs the `ws-spill-01` filter does not match; `Page` is the
    /// unchanged ERROR. Every arm still refuses.
    #[test]
    fn test_refuse_past_frame_cap_as_every_verdict() {
        let lines = capture(|| {
            assert!(refuse_past_frame_cap_as(
                3_497_726,
                WAL_REPLAY_MAX_FRAMES_PER_BOOT,
                SHRINKING
            ));
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["level"], "WARN", "{}", lines[0]);
        assert_eq!(lines[0]["code"], "WS-SPILL-01", "{}", lines[0]);
        assert_eq!(
            lines[0]["source"], "replay_frame_cap_shrinking",
            "{}",
            lines[0]
        );
        assert_eq!(lines[0]["previous_backlog_segments"], 290, "{}", lines[0]);
        assert_eq!(lines[0]["backlog_segments"], 285, "{}", lines[0]);
        assert!(!ws_spill_01_filter_matches(&lines[0]));

        let lines = capture(|| {
            assert!(refuse_past_frame_cap_as(
                3_497_726,
                WAL_REPLAY_MAX_FRAMES_PER_BOOT,
                ReplayLimitVerdict::Page
            ));
        });
        assert_eq!(lines[0]["level"], "ERROR", "{}", lines[0]);
        assert_eq!(lines[0]["source"], "replay_frame_cap", "{}", lines[0]);
        assert!(ws_spill_01_filter_matches(&lines[0]));

        let lines = capture(|| {
            assert!(refuse_past_frame_cap_as(
                EVENING_DRAIN_REPLAY_FRAME_CAP,
                EVENING_DRAIN_REPLAY_FRAME_CAP,
                ReplayLimitVerdict::PlannedRestart
            ));
        });
        assert_eq!(
            lines[0]["source"], "replay_frame_cap_planned",
            "{}",
            lines[0]
        );
        assert!(!ws_spill_01_filter_matches(&lines[0]));

        // Under the cap: nothing, whatever the verdict.
        let lines = capture(|| assert!(!refuse_past_frame_cap_as(0, 1, SHRINKING)));
        assert!(lines.is_empty(), "{lines:?}");
    }

    /// The catch-up deferral and the byte-budget deferral are quiet on a
    /// shrinking backlog; the memory-guard stop pages on every verdict.
    #[test]
    fn test_regression_rs_deferral_lines_quiet_only_when_shrinking_memory_always_pages() {
        let lines = capture(|| report_catchup_replay_deferred(SHRINKING, 3, 1, 2, 4));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["level"], "WARN", "{}", lines[0]);
        assert_eq!(
            lines[0]["source"], "replay_deferred_shrinking",
            "{}",
            lines[0]
        );
        assert_eq!(lines[0]["backlog_segments"], 285, "{}", lines[0]);
        assert!(!ws_spill_01_filter_matches(&lines[0]));

        let byte_stop = ReplayBudgetDeferral {
            deferred_segments: 7,
            consumed_segments: 5,
            bytes_held: 536_870_912,
            budget_bytes: 536_870_912,
            stopped_for_memory: false,
            stop_pct: 60,
        };
        let lines = capture(|| report_replay_budget_deferred(SHRINKING, byte_stop));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["level"], "WARN", "{}", lines[0]);
        assert_eq!(lines[0]["code"], "WS-SPILL-02", "{}", lines[0]);
        assert_eq!(
            lines[0]["source"], "replay_budget_deferred_shrinking",
            "{}",
            lines[0]
        );
        assert!(!ws_spill_02_filter_matches(&lines[0]));

        let memory_stop = ReplayBudgetDeferral {
            stopped_for_memory: true,
            ..byte_stop
        };
        for verdict in [
            SHRINKING,
            ReplayLimitVerdict::PlannedRestart,
            ReplayLimitVerdict::Page,
        ] {
            let lines = capture(|| report_replay_budget_deferred(verdict, memory_stop));
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert_eq!(lines[0]["level"], "ERROR", "{verdict:?}: {}", lines[0]);
            assert!(
                ws_spill_02_filter_matches(&lines[0]),
                "{verdict:?}: a memory-guard stop always pages"
            );
        }
    }

    // Regression: 2026-11-11 08:33 IST SOS "safety capture hit a limit". The
    // morning start reached the per-boot frame cap (the 8 Nov shape, 3.5M
    // frames after the boot pass) while its waiting backlog was below the
    // previous evening start's. It paged. Under §2.20 a smaller backlog is
    // quiet; the same backlog, or no record, still pages. The counts are the
    // shape of that morning (Assumed: the exact counts were not captured).
    // (2026-11-11 review fix: the comparison point is now the previous start
    // that was not a planned evening restart, since the planned restart saves
    // no count; the verdict below is the same for any recorded previous count.)
    #[test]
    fn test_regression_rs_0833_morning_start_with_a_shrinking_backlog_does_not_page() {
        use crate::wal_backlog_trend::{BacklogRecord, PreviousRecord, backlog_trend_verdict};
        let today = 20_768; // 2026-11-11 IST
        let last_evening = PreviousRecord::Valid(BacklogRecord {
            ist_day: today - 1,
            segments: Some(290),
        });
        let morning = |current: Option<u64>, previous: PreviousRecord| {
            let trend = backlog_trend_verdict(previous, today, current);
            let verdict = replay_limit_verdict(false, Some(trend));
            capture(|| {
                assert!(refuse_past_frame_cap_as(
                    3_497_726,
                    WAL_REPLAY_MAX_FRAMES_PER_BOOT,
                    verdict
                ));
            })
        };
        let quiet = morning(Some(285), last_evening);
        assert_eq!(quiet[0]["level"], "WARN", "{}", quiet[0]);
        assert_eq!(
            quiet[0]["source"], "replay_frame_cap_shrinking",
            "{}",
            quiet[0]
        );
        assert!(!ws_spill_01_filter_matches(&quiet[0]), "shrinking: no page");

        for (current, previous) in [
            (Some(290), last_evening),           // stuck
            (Some(291), last_evening),           // growing
            (None, last_evening),                // cannot count
            (Some(285), PreviousRecord::Absent), // first start
            (Some(285), PreviousRecord::Unreadable),
        ] {
            let lines = morning(current, previous);
            assert_eq!(lines[0]["level"], "ERROR", "{current:?} {previous:?}");
            assert_eq!(lines[0]["source"], "replay_frame_cap");
            assert!(
                ws_spill_01_filter_matches(&lines[0]),
                "stuck or unknown: pages"
            );
        }
    }
}
