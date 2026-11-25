//! Cold-path WAL transitions. A segment basename identifies one capture in every
//! replay directory, so replacing a different file at that name loses evidence.

use std::{io, path::Path, sync::Mutex};

// Serialize our own cold directory transitions with the final name check and
// unlink in a verified prune. Reading or hashing segment bytes holds no lock.
static SEGMENT_NAMES: Mutex<()> = Mutex::new(());

/// Refuse ambiguous capture names before returning any frames to a caller
/// that settles by name. The source paths are already in FIFO name order.
pub(crate) fn check_replay_names(segments: &[std::path::PathBuf]) -> anyhow::Result<()> {
    // The two source directories can contain distinct captures with the same
    // name (for example after restoring a backup). A named settle cannot tell
    // them apart. Refuse before filtering, staging or returning any frames.
    // O(1) EXEMPT: cold boot replay, scan adjacent names after the required FIFO sort
    // A directory obstacle is handled by the existing failed-restore path.
    if let Some(pair) = segments.windows(2).find(|pair| {
        pair[0].file_name() == pair[1].file_name() && !pair[0].is_dir() && !pair[1].is_dir()
    }) {
        anyhow::bail!(
            "WAL replay: conflicting segment names at {:?} and {:?}; preserving both captures",
            pair[0],
            pair[1]
        );
    }

    Ok(())
}

/// Move one segment without overwriting an earlier capture. A collision keeps
/// both originals where they are so an operator can reconcile their identities.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
pub(crate) fn move_segment(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    let _names = SEGMENT_NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(io::Error::from)
}

/// Filesystems without an atomic exclusive rename can still retain both files
/// across a crash between linking and unlinking. An existing name is always held.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
pub(crate) fn move_segment(from: &Path, to: &Path) -> io::Result<()> {
    let _names = SEGMENT_NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::fs::hard_link(from, to)?;
    std::fs::remove_file(from)
}

/// A verified prune pins its file descriptor while hashing. Check that its
/// directory entry still refers to the same sealed file before deleting it.
pub(crate) fn remove_hashed_segment(path: &Path, before: &std::fs::Metadata) -> io::Result<()> {
    let _names = SEGMENT_NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = std::fs::symlink_metadata(path)?;
    #[cfg(unix)]
    let same_file = {
        use std::os::unix::fs::MetadataExt;
        (before.dev(), before.ino()) == (now.dev(), now.ino())
    };
    #[cfg(not(unix))]
    let same_file = before.created()? == now.created()?;
    if !now.is_file()
        || !same_file
        || before.len() != now.len()
        || before.modified()? != now.modified()?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WAL segment changed while verifying its archive copy",
        ));
    }
    std::fs::remove_file(path)
}

pub(crate) enum ArchiveDecision {
    AppliedWatermark,
    DirectConfirm,
    SettledAfterAck,
}

impl ArchiveDecision {
    fn label(&self) -> &'static str {
        match self {
            Self::AppliedWatermark => "applied_watermark",
            Self::DirectConfirm => "direct_confirm",
            Self::SettledAfterAck => "settled_after_ack",
        }
    }
}

/// This attribute describes the archive path taken by this exact inode. Older
/// captures and filesystems that do not record attributes have unknown provenance.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
const ARCHIVE_ATTRIBUTE: &str = "user.tickvault.replay_archive.v1";

/// Bounded read from the exact verified descriptor, including after unlink.
/// Treat old archives, unsupported attributes and future formats as unknown.
pub(crate) fn last_archive_decision(file: &std::fs::File) -> &'static str {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        let mut value = [0u8; 32];
        if let Ok(len) = rustix::fs::fgetxattr(file, ARCHIVE_ATTRIBUTE, &mut value[..]) {
            for decision in [
                ArchiveDecision::DirectConfirm,
                ArchiveDecision::AppliedWatermark,
                ArchiveDecision::SettledAfterAck,
            ] {
                if value.get(..len) == Some(decision.label().as_bytes()) {
                    return decision.label();
                }
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    let _ = file;
    "unknown"
}

/// Check the inode at the destination after the move. The source might have
/// been replaced after we opened it, or the pruner might have removed the
/// destination already. In either case no decision is written on a guess.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn record_archive_decision(
    file: &std::fs::File,
    destination: &Path,
    decision: &str,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let pinned = file.metadata()?;
    let archived = std::fs::symlink_metadata(destination)?;
    if !pinned.is_file() || (pinned.dev(), pinned.ino()) != (archived.dev(), archived.ino()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "archive inode changed",
        ));
    }
    rustix::fs::fsetxattr(
        file,
        ARCHIVE_ATTRIBUTE,
        decision.as_bytes(),
        rustix::fs::XattrFlags::empty(),
    )?;
    file.sync_all()
}

/// The descriptor pins the source inode even if the archive pruner runs after
/// the move. Only a successful exclusive archive records its decision. Metadata
/// failure never changes replay's result or the raw archive bytes.
pub(crate) fn archive_segment(from: &Path, to: &Path, decision: ArchiveDecision) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(from)
    };
    move_segment(from, to)?;
    let decision = decision.label();
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    if let Err(error) = file.and_then(|file| record_archive_decision(&file, to, decision)) {
        metrics::counter!("tv_wal_replay_archive_provenance_failed_total").increment(1);
        tracing::warn!(code = tickvault_common::error_code::ErrorCode::HotPath01SyncFsFailed.code_str(), segment = ?to, decision, %error, "Archived WAL segment has unconfirmed provenance metadata");
    }
    tracing::info!(segment = ?to, decision, "Archived WAL segment");
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn replaced_source_never_attaches_an_archive_decision_to_the_wrong_capture() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("wal-provenance-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("source");
        let replaced = dir.join("replaced");
        let archive = dir.join("archive");
        std::fs::write(&source, b"pinned capture").unwrap();
        let pinned = std::fs::File::open(&source).unwrap();
        std::fs::rename(&source, &replaced).unwrap();
        std::fs::write(&source, b"source replacement").unwrap();
        move_segment(&source, &archive).unwrap();
        let err = record_archive_decision(&pinned, &archive, "direct_confirm").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        for path in [&replaced, &archive] {
            assert_eq!(
                rustix::fs::getxattr_query(path, ARCHIVE_ATTRIBUTE),
                Err(rustix::io::Errno::NODATA)
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
