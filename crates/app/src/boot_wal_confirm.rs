//! STAGE-C: what the boot WAL replay may archive from `replaying/`.
//!
//! `confirm_replayed` moves every segment in `<wal>/replaying/` to
//! `<wal>/archive/`, which no replay ever reads again. It is correct only
//! when the boot pass READ those segments and no lane refold is still coming
//! for their frames.
//!
//! # The defect this closes (audit 2026-11-07, upheld)
//!
//! K2 (2026-10-26) made the boot pass fail CLOSED: when a segment folder
//! cannot be listed it returns `Err` and stages and archives nothing
//! (`ws_frame_spill::replay_all_with_report_guarded`). `main.rs` then logged
//! the error and fell through to its last branch, which called
//! `confirm_replayed`: the leftovers an earlier boot staged in `replaying/`
//! (a crash mid catch-up) were archived UNREAD whenever the live folder could
//! not be listed but `replaying/` could. Archived segments are never replayed,
//! so their rows were lost from the database.
//!
//! The decision now lives here as a pure function, and only a pass that READ
//! its segments, with no lane refold pending, archives. A failed or refused
//! pass leaves `replaying/` exactly as found ("keep more"): the lane's WAL
//! catch-up drain or the next boot reads it.
//!
//! # Complexity
//!
//! [`stage_c_confirm_decision`] is O(1). [`settle_boot_replay_staging`] is
//! O(staged segments) on the archive arm only (one listing and one rename
//! each), once per boot, cold path, never on the frame drain.

use std::path::Path;

use tickvault_common::error_code::ErrorCode;
use tickvault_storage::ws_frame_spill::WalReplayBatch;
use tracing::{info, warn};

/// `tv_wal_boot_replay_confirm_total{outcome}`: what the STAGE-C confirm step
/// did with `replaying/`, once per boot. Local `/metrics` only (no alarm, no
/// EMF name); every outcome is registered at 0 on each call.
pub const BOOT_REPLAY_CONFIRM_COUNTER: &str = "tv_wal_boot_replay_confirm_total";

/// How the STAGE-C boot replay pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootReplayPass {
    /// The pass listed and read its segments (it may have returned no frames).
    Read,
    /// The pass was refused before reading (disk floor or per-boot frame cap)
    /// and touched nothing.
    Refused,
    /// The pass returned an error (a segment folder could not be listed) and
    /// touched nothing.
    Failed,
}

impl BootReplayPass {
    /// Classifies the boot pass's result. O(1).
    #[must_use]
    pub fn of(result: &anyhow::Result<WalReplayBatch>) -> Self {
        match result {
            Ok(batch) if batch.stopped_for_disk || batch.stopped_for_frame_cap => Self::Refused,
            Ok(_) => Self::Read,
            Err(_) => Self::Failed,
        }
    }
}

/// What STAGE-C does with the segments staged in `replaying/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageCConfirm {
    /// Frames were read and the Dhan lane will fold them; the lane confirms
    /// after its refold and flush.
    HeldForLaneRefold,
    /// The pass was refused; nothing was read, so nothing is archived.
    HeldRefused,
    /// The pass failed; nothing was read, so nothing is archived.
    HeldFailed,
    /// The pass read everything and no refold is coming: `replaying/` is
    /// archived (otherwise the same segments would re-stage every boot).
    Archived,
}

impl StageCConfirm {
    /// Stable label for [`BOOT_REPLAY_CONFIRM_COUNTER`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeldForLaneRefold => "held_for_lane_refold",
            Self::HeldRefused => "held_refused",
            Self::HeldFailed => "held_failed",
            Self::Archived => "archived",
        }
    }

    /// Every outcome, for the zero registration.
    pub const ALL: [Self; 4] = [
        Self::HeldForLaneRefold,
        Self::HeldRefused,
        Self::HeldFailed,
        Self::Archived,
    ];
}

/// The STAGE-C confirm decision. Pure, O(1).
///
/// `lane_refolds` is true when the Dhan lane will fold this pass's live-feed
/// frames (it then confirms itself). Only a pass that READ its segments with
/// no refold coming archives; every other combination holds `replaying/`.
#[must_use]
pub const fn stage_c_confirm_decision(pass: BootReplayPass, lane_refolds: bool) -> StageCConfirm {
    match pass {
        BootReplayPass::Failed => StageCConfirm::HeldFailed,
        BootReplayPass::Refused => StageCConfirm::HeldRefused,
        BootReplayPass::Read if lane_refolds => StageCConfirm::HeldForLaneRefold,
        BootReplayPass::Read => StageCConfirm::Archived,
    }
}

/// Applies [`stage_c_confirm_decision`] to `wal_dir`: archives `replaying/`
/// only on [`StageCConfirm::Archived`], and counts and logs every outcome.
///
/// `frames` is the number of live-feed frames the pass read (for the log
/// line). Once per boot, cold path.
pub fn settle_boot_replay_staging(
    wal_dir: &Path,
    pass: BootReplayPass,
    lane_refolds: bool,
    frames: usize,
) -> StageCConfirm {
    let decision = stage_c_confirm_decision(pass, lane_refolds);
    for outcome in StageCConfirm::ALL {
        metrics::counter!(BOOT_REPLAY_CONFIRM_COUNTER, "outcome" => outcome.as_str()).increment(0);
    }
    metrics::counter!(BOOT_REPLAY_CONFIRM_COUNTER, "outcome" => decision.as_str()).increment(1);
    match decision {
        StageCConfirm::HeldForLaneRefold => info!(
            frames,
            "STAGE-C.2c: staged write-ahead-log segments are held UNCONFIRMED — the Dhan \
             lane will fold these frames and confirm them itself. A crash before that \
             replays them again next boot rather than losing them."
        ),
        StageCConfirm::HeldRefused => warn!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "boot_replay_refused_staging_kept",
            "STAGE-C: the WAL replay was refused (disk floor or per-boot frame cap), so \
             nothing staged is confirmed — `replaying/` is left exactly as found for the \
             next boot"
        ),
        StageCConfirm::HeldFailed => warn!(
            code = ErrorCode::WsSpill04SegmentUnreadable.code_str(),
            source = "boot_replay_error_staging_kept",
            wal_dir = ?wal_dir,
            "STAGE-C: the WAL replay returned an error, so nothing staged is confirmed — \
             `replaying/` is left exactly as found for the WAL catch-up drain or the next \
             boot (K2: archiving here would bury segments nobody read)"
        ),
        StageCConfirm::Archived => {
            tickvault_storage::ws_frame_spill::confirm_replayed(wal_dir);
        }
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh WAL folder with one segment staged in `replaying/` by an
    /// "earlier boot", the shape a crash mid catch-up leaves.
    fn wal_with_staged_leftover(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let wal = std::env::temp_dir().join(format!(
            "tv-boot-wal-confirm-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let replaying = wal.join("replaying");
        std::fs::create_dir_all(&replaying).expect("create replaying/");
        let seg = replaying.join("ws-frames-00000000000000000001.wal");
        std::fs::write(&seg, b"unread frames from an earlier boot").expect("write segment");
        (wal, seg)
    }

    fn archived(wal: &Path) -> usize {
        std::fs::read_dir(wal.join("archive")).map_or(0, |d| d.flatten().count())
    }

    #[test]
    fn test_boot_replay_pass_of_classifies_every_result() {
        let read: anyhow::Result<WalReplayBatch> = Ok(WalReplayBatch::default());
        assert_eq!(BootReplayPass::of(&read), BootReplayPass::Read);
        let disk = Ok(WalReplayBatch {
            stopped_for_disk: true,
            ..WalReplayBatch::default()
        });
        assert_eq!(BootReplayPass::of(&disk), BootReplayPass::Refused);
        let cap = Ok(WalReplayBatch {
            stopped_for_frame_cap: true,
            ..WalReplayBatch::default()
        });
        assert_eq!(BootReplayPass::of(&cap), BootReplayPass::Refused);
        let failed: anyhow::Result<WalReplayBatch> =
            Err(anyhow::anyhow!("WAL replay: segment listing failed"));
        assert_eq!(BootReplayPass::of(&failed), BootReplayPass::Failed);
    }

    #[test]
    fn test_stage_c_confirm_decision_archives_only_a_read_pass_with_no_refold() {
        use BootReplayPass::{Failed, Read, Refused};
        assert_eq!(
            stage_c_confirm_decision(Read, false),
            StageCConfirm::Archived
        );
        assert_eq!(
            stage_c_confirm_decision(Read, true),
            StageCConfirm::HeldForLaneRefold
        );
        for lane in [false, true] {
            assert_eq!(
                stage_c_confirm_decision(Failed, lane),
                StageCConfirm::HeldFailed
            );
            assert_eq!(
                stage_c_confirm_decision(Refused, lane),
                StageCConfirm::HeldRefused
            );
        }
        let labels: Vec<&str> = StageCConfirm::ALL.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            labels,
            [
                "held_for_lane_refold",
                "held_refused",
                "held_failed",
                "archived"
            ]
        );
    }

    /// Regression: 2026-11-07 — a boot replay that ERRORED archived the
    /// `replaying/` leftovers it never read. Drives the error arm end to end
    /// on real files: the result the storage pass returns on a listing
    /// failure, classified and settled exactly as `main` does.
    #[test]
    fn test_regression_settle_boot_replay_staging_never_archives_unread_segments_after_an_error() {
        let (wal, seg) = wal_with_staged_leftover("err");
        let result: anyhow::Result<WalReplayBatch> = Err(anyhow::anyhow!(
            "WAL replay: segment listing failed: Permission denied"
        ));
        let pass = BootReplayPass::of(&result);
        let outcome = settle_boot_replay_staging(&wal, pass, false, 0);
        assert!(
            seg.is_file(),
            "the unread staged segment must stay in replaying/ for the next pass"
        );
        assert_eq!(archived(&wal), 0, "nothing unread may reach archive/");
        assert_eq!(outcome, StageCConfirm::HeldFailed);
        drop(std::fs::remove_dir_all(&wal));
    }

    #[test]
    fn test_boot_replay_refused_keeps_staged_segments() {
        let (wal, seg) = wal_with_staged_leftover("refused");
        let outcome = settle_boot_replay_staging(&wal, BootReplayPass::Refused, false, 0);
        assert_eq!(outcome, StageCConfirm::HeldRefused);
        assert!(seg.is_file());
        assert_eq!(archived(&wal), 0);
        drop(std::fs::remove_dir_all(&wal));
    }

    #[test]
    fn test_boot_replay_pending_refold_keeps_staged_segments() {
        let (wal, seg) = wal_with_staged_leftover("refold");
        let outcome = settle_boot_replay_staging(&wal, BootReplayPass::Read, true, 12);
        assert_eq!(outcome, StageCConfirm::HeldForLaneRefold);
        assert!(seg.is_file());
        assert_eq!(archived(&wal), 0);
        drop(std::fs::remove_dir_all(&wal));
    }

    /// The control: a pass that READ its segments with no refold coming does
    /// archive, so the hold assertions above are not vacuous.
    #[test]
    fn test_boot_replay_read_pass_archives_staged_segments() {
        let (wal, seg) = wal_with_staged_leftover("read");
        let outcome = settle_boot_replay_staging(&wal, BootReplayPass::Read, false, 0);
        assert_eq!(outcome, StageCConfirm::Archived);
        assert!(!seg.exists(), "a read segment leaves replaying/");
        assert_eq!(archived(&wal), 1, "and lands in archive/");
        drop(std::fs::remove_dir_all(&wal));
    }
}
