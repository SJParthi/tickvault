//! Offline retained-WAL identity audit. No capture authority is created.

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SequenceRetainedWalAudit {
    pub mode: &'static str,
    pub wal_directory: String,
    pub scope: [&'static str; 4],
    pub retained_wal_validation: &'static str,
    pub reason: String,
    pub root_wal_files: usize,
    pub replaying_wal_files: usize,
    pub archive_wal_files: usize,
    pub quarantine_wal_files: usize,
    pub ancillary_metadata_files: usize,
    pub files_validated: usize,
    pub bytes_validated: u64,
    pub records_validated: u64,
    pub records_without_sequence: u64,
    pub unknown_endpoint_records: u64,
    /// Exact decimal strings avoid JSON clients rounding identities above 2^53.
    /// These are absent on any refused or incomplete audit.
    pub highest_frame_seq: Option<String>,
    /// Conservative bound including every reserved packet-index position.
    /// This is not a count of packets decoded from the market-data payload.
    pub retained_capture_seq_upper_bound: Option<String>,
    pub existing_owner_lock_acquired: bool,
    pub complete_namespace_verified: bool,
    pub migration_applied: bool,
    pub sequence_authority_written: bool,
    pub deployment_authorized: bool,
}

impl SequenceRetainedWalAudit {
    fn new(path: &Path) -> Self {
        Self {
            mode: "audit-retained-wal",
            wal_directory: path.display().to_string(),
            scope: [".", "replaying", "archive", "quarantine"],
            retained_wal_validation: "unverified",
            reason: String::new(),
            root_wal_files: 0,
            replaying_wal_files: 0,
            archive_wal_files: 0,
            quarantine_wal_files: 0,
            ancillary_metadata_files: 0,
            files_validated: 0,
            bytes_validated: 0,
            records_validated: 0,
            records_without_sequence: 0,
            unknown_endpoint_records: 0,
            highest_frame_seq: None,
            retained_capture_seq_upper_bound: None,
            existing_owner_lock_acquired: false,
            complete_namespace_verified: false,
            migration_applied: false,
            sequence_authority_written: false,
            deployment_authorized: false,
        }
    }

    #[must_use]
    pub fn local_audit_passed(&self) -> bool {
        self.retained_wal_validation == "verified"
    }
}

/// Mechanical local verification is separate from the operator's recorded
/// attestation about database, prior-root and backup completeness.
#[derive(Debug, Serialize)]
pub struct SequenceReceiptVerification {
    pub mode: &'static str,
    pub wal_directory: String,
    pub receipt_verified: bool,
    pub reason: String,
    pub verified_capture_bound: Option<String>,
    pub retained_wal_bound: Option<String>,
    pub receipt_reserved_through: Option<String>,
    pub authority_reserved_through: Option<String>,
    pub caller_attested_complete_namespace: bool,
    pub external_namespace_verified: bool,
    pub existing_owner_lock_acquired: bool,
    pub sequence_authority_written: bool,
    pub deployment_authorized: bool,
}

impl SequenceReceiptVerification {
    fn new(path: &Path) -> Self {
        Self {
            mode: "verify-receipt",
            wal_directory: path.display().to_string(),
            receipt_verified: false,
            reason: String::new(),
            verified_capture_bound: None,
            retained_wal_bound: None,
            receipt_reserved_through: None,
            authority_reserved_through: None,
            caller_attested_complete_namespace: false,
            external_namespace_verified: false,
            existing_owner_lock_acquired: false,
            sequence_authority_written: false,
            deployment_authorized: false,
        }
    }
}

/// Validate strict bounded receipt JSON, byte-match it to the receipt retained
/// in the actual root, and decode the actual directory-bound authority/marker.
/// Uses the existing owner lock without creating it. No WAL payload scan or
/// sequence allocation, receipt/authority write or external scope attestation
/// occurs. An external timeout must bound filesystem operations.
#[must_use]
pub fn verify_sequence_migration_receipt(
    path: &Path,
    receipt_copy: &Path,
) -> SequenceReceiptVerification {
    #[cfg(unix)]
    {
        unix::verify_receipt(path, receipt_copy)
    }
    #[cfg(not(unix))]
    {
        let _ = receipt_copy;
        let mut report = SequenceReceiptVerification::new(path);
        report.reason =
            "existing-only inode-bound receipt verification is unsupported on this platform".into();
        report
    }
}

/// Audit all retained WAL under an existing exclusive owner lock. Missing
/// root/lock, active owner, unsafe/unknown entries, identity changes and any
/// corrupt or incomplete record refuse the audit. Legacy TVW1 records lack
/// their original sequence and cannot establish an identity bound.
///
/// Uses the production migration/recovery framing and CRC scanner. Reads all
/// WAL bytes with fixed scratch space; metadata inventory is limited to 4096
/// entries. Cost is O(B + E log E), not O(1). An external command timeout is
/// required for large scans or blocked filesystem calls. Only known WAL and
/// ancillary metadata filenames are admitted; ancillary contents are not
/// included in the retained-WAL identity bound. The existing authority, if
/// present, is separately checked by the metadata inspector.
///
/// No directory/file is created, no file content is written, and no sequence,
/// receipt, replay, pruning or migration operation is performed. The lock is
/// cooperative process exclusion, not proof about other roots/hosts, database
/// rows, rescue files, backups or a privileged non-cooperating mutator.
#[must_use]
pub fn audit_retained_wal_namespace(path: &Path) -> SequenceRetainedWalAudit {
    #[cfg(unix)]
    {
        unix::audit_with_hook(path, 4096, || {})
    }
    #[cfg(not(unix))]
    {
        let mut report = SequenceRetainedWalAudit::new(path);
        report.reason =
            "existing-only inode-bound WAL audit is unsupported on this platform".into();
        report
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs::{File, Metadata};
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use crate::ws_frame_spill::{
        ARCHIVE_SUBDIR, MAX_FRAME_SEQUENCE, MAX_PACKET_INDEX, PACKET_INDEX_BITS, QUARANTINE_SUBDIR,
        REPLAYING_SUBDIR, SEQUENCE_INITIALIZED_FILE, SEQUENCE_MANIFEST_LEN,
        SEQUENCE_RESERVATION_FILE, SequenceNamespaceState, SequenceReservations, WAL_DIR_LOCK_FILE,
        WAL_MAINTENANCE, WalDirGuard, inspect_sequence_namespace, is_open_segment,
        scan_sequence_file,
    };
    use anyhow::{Context, Result, ensure};
    use sha2::{Digest, Sha256};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Stamp {
        device: u64,
        inode: u64,
        mode: u32,
        links: u64,
        bytes: u64,
        modified: (i64, i64),
        changed: (i64, i64),
    }

    impl Stamp {
        fn of(meta: &Metadata) -> Self {
            Self {
                device: meta.dev(),
                inode: meta.ino(),
                mode: meta.mode(),
                links: meta.nlink(),
                bytes: meta.len(),
                modified: (meta.mtime(), meta.mtime_nsec()),
                changed: (meta.ctime(), meta.ctime_nsec()),
            }
        }
    }

    fn safe_metadata(path: &Path) -> Result<Metadata> {
        let meta = std::fs::symlink_metadata(path)?;
        ensure!(
            !meta.file_type().is_symlink()
                && (meta.is_dir() || (meta.is_file() && meta.nlink() == 1)),
            "audit refuses symlink, special or multiply-linked entry: {}",
            path.display()
        );
        Ok(meta)
    }

    fn existing_only_lock(path: &Path) -> Result<WalDirGuard> {
        let original = safe_metadata(path)?;
        ensure!(original.is_dir(), "audit WAL root is not a directory");
        let root = std::fs::canonicalize(path)?;
        let directory = Arc::new(File::open(&root)?);
        ensure!(
            Stamp::of(&directory.metadata()?) == Stamp::of(&original),
            "audit root changed while opening it"
        );
        let path = root.join(WAL_DIR_LOCK_FILE);
        let before = safe_metadata(&path)
            .context("audit requires the existing .wal-owner.lock; no lock file is created")?;
        ensure!(before.is_file(), "audit owner lock is not a regular file");
        // Exclusive flock works on a read-only descriptor on the supported
        // Unix targets. File::open never creates or truncates the lock file.
        let lock = File::open(&path)?;
        ensure!(
            Stamp::of(&lock.metadata()?) == Stamp::of(&before),
            "audit lock inode changed"
        );
        lock.try_lock().map_err(|error| anyhow::anyhow!(
            "offline WAL audit requires exclusive existing ownership; active writer or unavailable lock: {error}"
        ))?;
        let guard = WalDirGuard {
            _lock: Arc::new(lock),
            directory,
            path,
            valid: Arc::new(AtomicBool::new(true)),
        };
        guard.ensure_current()?;
        Ok(guard)
    }

    fn known_metadata(relative: &Path) -> bool {
        let Some(name) = relative.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if name.ends_with(".wal.confirmed") || name.ends_with(".wal.confirmed.tmp") {
            return true;
        }
        relative.parent() == Some(Path::new(""))
            && ([
                WAL_DIR_LOCK_FILE,
                SEQUENCE_RESERVATION_FILE,
                SEQUENCE_INITIALIZED_FILE,
                "applied.tvaw",
                "applied.tvaw.tmp",
            ]
            .contains(&name)
                || name
                    .strip_prefix("sequence-migration-")
                    .and_then(|rest| rest.strip_suffix(".json"))
                    .is_some_and(|digits| {
                        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
                    }))
    }

    fn inventory(root: &Path, limit: usize) -> Result<BTreeMap<PathBuf, Stamp>> {
        let mut entries = BTreeMap::new();
        entries.insert(PathBuf::new(), Stamp::of(&safe_metadata(root)?));
        for relative_dir in ["", REPLAYING_SUBDIR, ARCHIVE_SUBDIR, QUARANTINE_SUBDIR] {
            let directory = root.join(relative_dir);
            let meta = match safe_metadata(&directory) {
                Ok(meta) => meta,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                        && !relative_dir.is_empty() =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            ensure!(
                meta.is_dir(),
                "audit scope path is not a directory: {}",
                directory.display()
            );
            for entry in std::fs::read_dir(&directory)? {
                let entry = entry?;
                ensure!(
                    entries.len() < limit,
                    "audit metadata entry limit exceeded; scope incomplete"
                );
                let relative = Path::new(relative_dir).join(entry.file_name());
                let meta = safe_metadata(&entry.path())?;
                if meta.is_dir() {
                    ensure!(
                        relative_dir.is_empty()
                            && [REPLAYING_SUBDIR, ARCHIVE_SUBDIR, QUARANTINE_SUBDIR]
                                .iter()
                                .any(|name| relative == Path::new(name)),
                        "unexpected nested directory; audit scope is incomplete: {}",
                        relative.display()
                    );
                } else {
                    ensure!(
                        relative
                            .extension()
                            .is_some_and(|extension| extension == "wal")
                            || known_metadata(&relative),
                        "unrecognized file; audit scope is incomplete: {}",
                        relative.display()
                    );
                }
                entries.insert(relative, Stamp::of(&meta));
            }
        }
        Ok(entries)
    }

    fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>> {
        let before = safe_metadata(path)?;
        ensure!(
            before.is_file() && before.len() <= maximum as u64,
            "verification file is not regular or exceeds its size bound"
        );
        let file = File::open(path)?;
        ensure!(
            Stamp::of(&file.metadata()?) == Stamp::of(&before),
            "verification file changed while opening it"
        );
        let mut bytes = Vec::with_capacity(before.len() as usize);
        file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= maximum && Stamp::of(&safe_metadata(path)?) == Stamp::of(&before),
            "verification file changed during its read"
        );
        Ok(bytes)
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TypedReceipt {
        schema_version: u32,
        verified_capture_bound: u64,
        retained_wal_bound: u64,
        reserved_through: u64,
        complete_namespace_verified: bool,
        provenance: String,
    }

    pub(super) fn verify_receipt(path: &Path, receipt_copy: &Path) -> SequenceReceiptVerification {
        let mut report = SequenceReceiptVerification::new(path);
        let result = (|| -> Result<()> {
            let guard = existing_only_lock(path)?;
            report.existing_owner_lock_acquired = true;
            let root = guard.path().parent().context("receipt guard has no root")?;
            report.wal_directory = root.display().to_string();
            let copied = read_bounded(receipt_copy, 16 * 1024)?;
            let receipt: TypedReceipt = serde_json::from_slice(&copied).map_err(|_| {
                anyhow::anyhow!(
                    "receipt JSON is malformed, duplicated, incomplete or has unknown fields"
                )
            })?;
            ensure!(
                receipt.schema_version == 1,
                "unsupported receipt schema version"
            );
            ensure!(
                receipt.complete_namespace_verified,
                "receipt lacks an explicit caller completeness attestation"
            );
            ensure!(
                !receipt.provenance.trim().is_empty() && receipt.provenance.len() <= 4096,
                "receipt provenance must be nonempty and bounded"
            );
            ensure!(
                receipt.verified_capture_bound <= i64::MAX as u64
                    && receipt.retained_wal_bound <= receipt.verified_capture_bound,
                "receipt capture/WAL bounds are inconsistent or outside the signed range"
            );
            ensure!(
                receipt.reserved_through <= MAX_FRAME_SEQUENCE
                    && receipt.reserved_through & MAX_PACKET_INDEX == 0,
                "receipt reservation floor is unaligned or outside the supported range"
            );
            let next = receipt
                .reserved_through
                .checked_add(1 << PACKET_INDEX_BITS)
                .filter(|next| *next <= MAX_FRAME_SEQUENCE)
                .context("receipt leaves no representable next frame base")?;
            // The recorded floor is frame-aligned; packet-index positions can
            // lie above it. The next frame must exceed both RAW stored bounds.
            ensure!(
                next > receipt.verified_capture_bound && next > receipt.retained_wal_bound,
                "receipt reservation does not advance past its capture/WAL bounds"
            );
            let retained_path = guard.io_path(format!(
                "sequence-migration-{}.json",
                receipt.reserved_through
            ))?;
            ensure!(
                read_bounded(&retained_path, 16 * 1024)? == copied,
                "receipt copy does not byte-match the actual namespace receipt"
            );
            let manifest_path = guard.io_path(SEQUENCE_RESERVATION_FILE)?;
            let marker_path = guard.io_path(SEQUENCE_INITIALIZED_FILE)?;
            let manifest = read_bounded(&manifest_path, SEQUENCE_MANIFEST_LEN)?;
            let marker = read_bounded(&marker_path, 32)?;
            let digest: [u8; 32] = Sha256::digest(root.as_os_str().as_encoded_bytes()).into();
            ensure!(
                marker.as_slice() == digest.as_slice(),
                "initialization marker does not belong to this WAL directory"
            );
            let manifest_bytes: [u8; SEQUENCE_MANIFEST_LEN] = manifest
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid sequence authority length"))?;
            let current = SequenceReservations::decode_manifest(&manifest_bytes, &digest)?;
            ensure!(
                current >= receipt.reserved_through,
                "actual authority is behind the verified receipt floor"
            );
            ensure!(
                read_bounded(&manifest_path, SEQUENCE_MANIFEST_LEN)? == manifest
                    && read_bounded(&marker_path, 32)? == marker
                    && read_bounded(&retained_path, 16 * 1024)? == copied
                    && read_bounded(receipt_copy, 16 * 1024)? == copied,
                "receipt or authority changed during verification"
            );
            guard.ensure_current()?;
            report.verified_capture_bound = Some(receipt.verified_capture_bound.to_string());
            report.retained_wal_bound = Some(receipt.retained_wal_bound.to_string());
            report.receipt_reserved_through = Some(receipt.reserved_through.to_string());
            report.authority_reserved_through = Some(current.to_string());
            report.caller_attested_complete_namespace = true;
            report.receipt_verified = true;
            report.reason = "strict receipt copy matches the actual namespace receipt and directory-bound authority; caller's external completeness attestation was not re-proven".into();
            Ok(())
        })();
        if let Err(error) = result {
            report.reason = format!("{error:#}");
        }
        report
    }

    pub(super) fn audit_with_hook(
        path: &Path,
        limit: usize,
        between: impl FnOnce(),
    ) -> SequenceRetainedWalAudit {
        let mut report = SequenceRetainedWalAudit::new(path);
        let result = (|| -> Result<()> {
            let guard = existing_only_lock(path)?;
            report.existing_owner_lock_acquired = true;
            let _maintenance = WAL_MAINTENANCE
                .lock()
                .map_err(|_| anyhow::anyhow!("WAL maintenance lock poisoned"))?;
            let root = guard.path().parent().context("audit guard has no root")?;
            report.wal_directory = root.display().to_string();
            let preflight = inspect_sequence_namespace(root);
            ensure!(
                matches!(
                    preflight.namespace_state,
                    SequenceNamespaceState::FreshEmpty
                        | SequenceNamespaceState::MigrationRequired
                        | SequenceNamespaceState::ValidDurableAuthority
                ),
                "local authority/metadata preflight refused: {}",
                preflight.reason
            );
            let first = inventory(root, limit)?;
            let mut highest = 0_u64;
            for relative in first.keys() {
                if relative
                    .extension()
                    .is_none_or(|extension| extension != "wal")
                {
                    if relative.file_name().is_some() && known_metadata(relative) {
                        report.ancillary_metadata_files += 1;
                    }
                    continue;
                }
                match relative.parent().and_then(Path::to_str) {
                    Some("") => report.root_wal_files += 1,
                    Some(REPLAYING_SUBDIR) => report.replaying_wal_files += 1,
                    Some(ARCHIVE_SUBDIR) => report.archive_wal_files += 1,
                    Some(QUARANTINE_SUBDIR) => report.quarantine_wal_files += 1,
                    _ => anyhow::bail!("WAL entry lies outside the declared audit scope"),
                }
            }
            for (relative, expected) in &first {
                if relative
                    .extension()
                    .is_none_or(|extension| extension != "wal")
                {
                    continue;
                }
                guard.ensure_current()?;
                let file_path = guard.io_path(relative)?;
                ensure!(
                    !is_open_segment(&file_path),
                    "audit refuses an open WAL segment"
                );
                ensure!(
                    Stamp::of(&safe_metadata(&file_path)?) == *expected,
                    "WAL entry changed before its audit read"
                );
                let file = File::open(&file_path)?;
                ensure!(
                    Stamp::of(&file.metadata()?) == *expected,
                    "WAL inode changed while opening its audit read"
                );
                let scan = scan_sequence_file(file.try_clone()?, &file_path, true)?;
                ensure!(
                    Stamp::of(&file.metadata()?) == *expected,
                    "WAL file changed while being audited"
                );
                report.files_validated += 1;
                report.bytes_validated = report
                    .bytes_validated
                    .checked_add(scan.bytes)
                    .context("audit byte count overflow")?;
                report.records_validated = report
                    .records_validated
                    .checked_add(scan.records)
                    .context("audit record count overflow")?;
                report.records_without_sequence = report
                    .records_without_sequence
                    .checked_add(scan.legacy_records)
                    .context("audit legacy count overflow")?;
                report.unknown_endpoint_records = report
                    .unknown_endpoint_records
                    .checked_add(scan.unknown_endpoint_records)
                    .context("audit endpoint count overflow")?;
                highest = highest.max(scan.highest_frame_seq);
            }
            between();
            guard.ensure_current()?;
            ensure!(
                first == inventory(root, limit)?,
                "namespace changed during audit; bound is unverified"
            );
            ensure!(
                report.records_without_sequence == 0,
                "legacy TVW1 records contain no original frame sequence; retained identity scope is incomplete"
            );
            ensure!(
                report.unknown_endpoint_records == 0,
                "retained records contain an unknown endpoint; identity audit refuses unsupported state"
            );
            report.highest_frame_seq = Some(highest.to_string());
            report.retained_capture_seq_upper_bound = Some(if report.records_validated == 0 {
                "0".into()
            } else {
                (highest | MAX_PACKET_INDEX).to_string()
            });
            report.retained_wal_validation = "verified";
            report.reason = "all retained WAL records in the declared local scope passed complete framing/CRC/identity validation under the existing owner lock; database, rescue, prior roots and backup scope remain unverified".into();
            Ok(())
        })();
        if let Err(error) = result {
            report.reason = format!("{error:#}");
        }
        report
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ws_frame_spill::{
        MAX_FRAME_SEQUENCE, MAX_PACKET_INDEX, PACKET_INDEX_BITS, SEQUENCE_INITIALIZED_FILE,
        SEQUENCE_RESERVATION_FILE, SequenceMigrationEvidence, SequenceMigrationReceipt,
        SequenceReservations, WAL_DIR_LOCK_FILE, WAL_MAGIC, WalEndpoint, WalRecord, WsType,
        crc32_ieee_of, lock_wal_dir, migrate_sequence_authority, write_record,
    };
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "tv-wal-audit-{}-{:x}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join("wal")).unwrap();
            Self(root)
        }

        fn wal(&self) -> PathBuf {
            self.0.join("wal")
        }
        fn copy(&self) -> PathBuf {
            self.0.join("private-receipt.json")
        }

        fn existing_lock(&self) {
            drop(lock_wal_dir(&self.wal()).unwrap().unwrap());
        }

        fn segment(&self, relative: &str, seq: u64) -> (PathBuf, Vec<u8>) {
            let path = self.wal().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let bytes = encoded(seq);
            fs::write(&path, &bytes).unwrap();
            (path, bytes)
        }

        fn receipt(&self) -> SequenceMigrationReceipt {
            // Ahead of the fixture clock and above 2^53, with every packet bit
            // set. This exercises the real aligned-down migration floor.
            let frame = 4_000_000_000_000_000_000_u64 & !MAX_PACKET_INDEX;
            self.segment("ws-frames-1.wal", frame);
            let guard = lock_wal_dir(&self.wal()).unwrap().unwrap();
            let receipt = migrate_sequence_authority(
                &self.wal(),
                &guard,
                SequenceMigrationEvidence {
                    highest_capture_seq: frame | MAX_PACKET_INDEX,
                    complete_namespace_verified: true,
                    provenance: "test fixture complete namespace only".into(),
                },
            )
            .unwrap();
            drop(guard);
            fs::copy(&receipt.receipt_path, self.copy()).unwrap();
            receipt
        }

        fn replace_both(&self, receipt: &SequenceMigrationReceipt, bytes: &[u8]) {
            fs::write(&receipt.receipt_path, bytes).unwrap();
            fs::write(self.copy(), bytes).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn encoded(seq: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_record(
            &mut bytes,
            &WalRecord {
                ws_type: WsType::LiveFeed,
                frame_seq: seq,
                received_at_nanos: 1,
                endpoint: WalEndpoint::MainFeed,
                frame: bytes::Bytes::from_static(b"retained fixture"),
            },
        )
        .unwrap();
        bytes
    }

    fn refused(report: &SequenceRetainedWalAudit) {
        assert!(!report.local_audit_passed(), "{report:?}");
        assert!(report.highest_frame_seq.is_none());
        assert!(report.retained_capture_seq_upper_bound.is_none());
        assert!(!report.complete_namespace_verified);
        assert!(!report.migration_applied);
        assert!(!report.sequence_authority_written);
        assert!(!report.deployment_authorized);
    }

    #[test]
    fn audit_certifies_exact_retained_bound_in_all_four_scopes_without_writes() {
        let fixture = Fixture::new();
        fixture.existing_lock();
        let base = 4_000_000_000_000_000_000_u64 & !MAX_PACKET_INDEX;
        let mut originals = Vec::new();
        for (index, name) in [
            "one.wal",
            "replaying/two.wal",
            "archive/three.wal",
            "quarantine/four.wal",
        ]
        .iter()
        .enumerate()
        {
            originals.push(fixture.segment(name, base + ((index as u64) << PACKET_INDEX_BITS)));
        }
        let report = audit_retained_wal_namespace(&fixture.wal());
        assert!(report.local_audit_passed(), "{report:?}");
        assert_eq!(
            [
                report.root_wal_files,
                report.replaying_wal_files,
                report.archive_wal_files,
                report.quarantine_wal_files
            ],
            [1; 4]
        );
        assert_eq!(report.files_validated, 4);
        assert_eq!(report.records_validated, 4);
        assert_eq!(
            report.bytes_validated,
            originals.iter().map(|(_, b)| b.len() as u64).sum::<u64>()
        );
        let highest = base + (3 << PACKET_INDEX_BITS);
        assert_eq!(report.highest_frame_seq, Some(highest.to_string()));
        assert_eq!(
            report.retained_capture_seq_upper_bound,
            Some((highest | MAX_PACKET_INDEX).to_string())
        );
        assert_eq!(report.records_without_sequence, 0);
        assert!(report.existing_owner_lock_acquired);
        assert!(
            !report.complete_namespace_verified
                && !report.migration_applied
                && !report.sequence_authority_written
                && !report.deployment_authorized
        );
        for (path, bytes) in originals {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert!(!fixture.wal().join(SEQUENCE_RESERVATION_FILE).exists());
        assert!(!fixture.wal().join(SEQUENCE_INITIALIZED_FILE).exists());
        assert_eq!(fs::read_dir(fixture.wal()).unwrap().count(), 5);
    }

    #[test]
    fn audit_never_creates_a_missing_directory_or_lock_and_empty_is_only_local() {
        let fixture = Fixture::new();
        refused(&audit_retained_wal_namespace(&fixture.0.join("absent")));
        assert!(!fixture.0.join("absent").exists());
        refused(&audit_retained_wal_namespace(&fixture.wal()));
        assert_eq!(fs::read_dir(fixture.wal()).unwrap().count(), 0);
        fixture.existing_lock();
        let report = audit_retained_wal_namespace(&fixture.wal());
        assert!(report.local_audit_passed(), "{report:?}");
        assert_eq!(
            report.retained_capture_seq_upper_bound.as_deref(),
            Some("0")
        );
        assert_eq!(report.records_validated, 0);
        assert!(!report.complete_namespace_verified);
        assert_eq!(fs::read_dir(fixture.wal()).unwrap().count(), 1);
    }

    #[test]
    fn audit_refuses_an_active_owner_and_passes_after_its_release() {
        let fixture = Fixture::new();
        let (path, bytes) = fixture.segment("one.wal", 1 << PACKET_INDEX_BITS);
        let guard = lock_wal_dir(&fixture.wal()).unwrap().unwrap();
        let report = audit_retained_wal_namespace(&fixture.wal());
        refused(&report);
        assert!(!report.existing_owner_lock_acquired);
        assert_eq!(report.files_validated, 0);
        drop(guard);
        assert!(audit_retained_wal_namespace(&fixture.wal()).local_audit_passed());
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn audit_refuses_crc_corruption_torn_suffix_and_unknown_magic_without_repair() {
        for corrupt in [0, 1, 2] {
            let fixture = Fixture::new();
            fixture.existing_lock();
            let (path, mut bytes) = fixture.segment("one.wal", 1 << PACKET_INDEX_BITS);
            match corrupt {
                0 => bytes[26] ^= 1,
                1 => bytes.extend_from_slice(b"TVW"),
                _ => bytes[0] = b'X',
            }
            fs::write(&path, &bytes).unwrap();
            refused(&audit_retained_wal_namespace(&fixture.wal()));
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn audit_refuses_legacy_identity_and_crc_valid_unknown_endpoint() {
        let legacy = Fixture::new();
        legacy.existing_lock();
        let header = [WsType::LiveFeed.as_u8(), 1, 0, 0, 0];
        let crc = crc32_ieee_of(&[&header, b"x"]);
        let mut bytes = WAL_MAGIC.to_vec();
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(b"x");
        bytes.extend_from_slice(&crc.to_le_bytes());
        fs::write(legacy.wal().join("legacy.wal"), &bytes).unwrap();
        let report = audit_retained_wal_namespace(&legacy.wal());
        refused(&report);
        assert_eq!(report.records_without_sequence, 1);
        let unsupported = Fixture::new();
        unsupported.existing_lock();
        let (path, mut bytes) = unsupported.segment("one.wal", 1 << PACKET_INDEX_BITS);
        bytes[21] = 255;
        let crc_at = bytes.len() - 4;
        let crc = crc32_ieee_of(&[&bytes[4..crc_at]]);
        bytes[crc_at..].copy_from_slice(&crc.to_le_bytes());
        fs::write(path, &bytes).unwrap();
        let report = audit_retained_wal_namespace(&unsupported.wal());
        refused(&report);
        assert_eq!(report.unknown_endpoint_records, 1);
    }

    #[test]
    fn audit_refuses_unknown_nested_linked_and_incomplete_inventory_scope() {
        for state in [0, 1, 2, 3] {
            let fixture = Fixture::new();
            fixture.existing_lock();
            let (path, _) = fixture.segment("one.wal", 1 << PACKET_INDEX_BITS);
            match state {
                0 => fs::write(fixture.wal().join("rescue.ilp"), b"unknown scope").unwrap(),
                1 => fs::create_dir(fixture.wal().join("previous-root")).unwrap(),
                2 => symlink(&path, fixture.wal().join("link.wal")).unwrap(),
                _ => fs::hard_link(&path, fixture.wal().join("link.wal")).unwrap(),
            }
            refused(&audit_retained_wal_namespace(&fixture.wal()));
        }
        let fixture = Fixture::new();
        fixture.existing_lock();
        fixture.segment("one.wal", 1 << PACKET_INDEX_BITS);
        refused(&unix::audit_with_hook(&fixture.wal(), 2, || {}));
    }

    #[test]
    fn audit_refuses_detected_post_read_growth_and_owner_replacement() {
        let fixture = Fixture::new();
        fixture.existing_lock();
        let (path, _) = fixture.segment("one.wal", 1 << PACKET_INDEX_BITS);
        let report = unix::audit_with_hook(&fixture.wal(), 4096, || {
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(&encoded(2 << PACKET_INDEX_BITS))
                .unwrap();
        });
        refused(&report);
        assert_eq!(report.files_validated, 1);
        let report = unix::audit_with_hook(&fixture.wal(), 4096, || {
            fs::remove_file(fixture.wal().join(WAL_DIR_LOCK_FILE)).unwrap();
            fs::write(fixture.wal().join(WAL_DIR_LOCK_FILE), b"").unwrap();
        });
        refused(&report);
    }

    #[test]
    fn receipt_verifies_production_codec_and_aligned_floor_without_reproving_attestation() {
        let fixture = Fixture::new();
        let receipt = fixture.receipt();
        assert!(receipt.verified_capture_bound > receipt.reserved_through);
        let before = fs::read(fixture.wal().join(SEQUENCE_RESERVATION_FILE)).unwrap();
        let report = verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy());
        assert!(report.receipt_verified, "{report:?}");
        assert_eq!(report.mode, "verify-receipt");
        assert_eq!(
            report.wal_directory,
            fs::canonicalize(fixture.wal())
                .unwrap()
                .display()
                .to_string()
        );
        assert_eq!(
            report.verified_capture_bound,
            Some(receipt.verified_capture_bound.to_string())
        );
        assert_eq!(
            report.receipt_reserved_through,
            Some(receipt.reserved_through.to_string())
        );
        assert!(report.caller_attested_complete_namespace && report.existing_owner_lock_acquired);
        assert!(
            !report.external_namespace_verified
                && !report.sequence_authority_written
                && !report.deployment_authorized
        );
        assert_eq!(
            before,
            fs::read(fixture.wal().join(SEQUENCE_RESERVATION_FILE)).unwrap()
        );
        // Normal later reservations do not invalidate the original receipt.
        let guard = lock_wal_dir(&fixture.wal()).unwrap().unwrap();
        let issuer = SequenceReservations::open(&fixture.wal(), &guard).unwrap();
        drop(issuer);
        drop(guard);
        let advanced = verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy());
        assert!(advanced.receipt_verified, "{advanced:?}");
        assert!(
            advanced
                .authority_reserved_through
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > receipt.reserved_through
        );
    }

    #[test]
    fn receipt_refuses_active_writer_missing_lock_and_mismatched_copy() {
        let fixture = Fixture::new();
        let receipt = fixture.receipt();
        let guard = lock_wal_dir(&fixture.wal()).unwrap().unwrap();
        let report = verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy());
        assert!(!report.receipt_verified && !report.existing_owner_lock_acquired);
        drop(guard);
        let mut bytes = fs::read(&receipt.receipt_path).unwrap();
        bytes.push(b'\n');
        fs::write(fixture.copy(), &bytes).unwrap();
        assert!(
            !verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy()).receipt_verified
        );
        fs::remove_file(fixture.wal().join(WAL_DIR_LOCK_FILE)).unwrap();
        assert!(
            !verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy()).receipt_verified
        );
        assert!(!fixture.wal().join(WAL_DIR_LOCK_FILE).exists());
    }

    #[test]
    fn receipt_refuses_strict_schema_bounds_and_false_or_empty_provenance() {
        let fixture = Fixture::new();
        let receipt = fixture.receipt();
        let original = fs::read(&receipt.receipt_path).unwrap();
        let valid: serde_json::Value = serde_json::from_slice(&original).unwrap();
        for (key, value) in [
            ("schema_version", serde_json::json!(2)),
            ("schema_version", serde_json::json!(1.0)),
            ("complete_namespace_verified", serde_json::json!(false)),
            ("provenance", serde_json::json!(" ")),
            ("provenance", serde_json::json!("x".repeat(4097))),
            ("unexpected", serde_json::json!(true)),
            (
                "verified_capture_bound",
                serde_json::json!(i64::MAX as u64 + 1),
            ),
            (
                "verified_capture_bound",
                serde_json::json!(receipt.reserved_through + (1 << PACKET_INDEX_BITS)),
            ),
            (
                "retained_wal_bound",
                serde_json::json!(receipt.verified_capture_bound + 1),
            ),
            (
                "reserved_through",
                serde_json::json!(receipt.reserved_through + 1),
            ),
            ("reserved_through", serde_json::json!(MAX_FRAME_SEQUENCE)),
        ] {
            let mut changed = valid.clone();
            changed[key] = value;
            fixture.replace_both(&receipt, &serde_json::to_vec(&changed).unwrap());
            let report = verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy());
            assert!(!report.receipt_verified, "accepted {key}: {report:?}");
            assert!(report.verified_capture_bound.is_none());
        }
        let duplicate =
            String::from_utf8(original.clone())
                .unwrap()
                .replacen('{', "{\"schema_version\":1,", 1);
        fixture.replace_both(&receipt, duplicate.as_bytes());
        assert!(
            !verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy()).receipt_verified
        );
        fixture.replace_both(&receipt, &vec![b' '; 16 * 1024 + 1]);
        assert!(
            !verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy()).receipt_verified
        );
        fixture.replace_both(&receipt, &original);
        assert!(
            verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy()).receipt_verified
        );
    }

    #[test]
    fn receipt_refuses_corrupt_or_foreign_authority_and_authority_behind_receipt() {
        let fixture = Fixture::new();
        let receipt = fixture.receipt();
        let manifest_path = fixture.wal().join(SEQUENCE_RESERVATION_FILE);
        let manifest = fs::read(&manifest_path).unwrap();
        let mut corrupt = manifest.clone();
        corrupt[40] ^= 1;
        fs::write(&manifest_path, corrupt).unwrap();
        assert!(
            !verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy()).receipt_verified
        );
        fs::write(&manifest_path, &manifest).unwrap();
        let other = Fixture::new();
        other.existing_lock();
        for filename in [
            SEQUENCE_RESERVATION_FILE,
            SEQUENCE_INITIALIZED_FILE,
            receipt.receipt_path.file_name().unwrap().to_str().unwrap(),
        ] {
            fs::copy(fixture.wal().join(filename), other.wal().join(filename)).unwrap();
        }
        fs::copy(fixture.copy(), other.copy()).unwrap();
        assert!(!verify_sequence_migration_receipt(&other.wal(), &other.copy()).receipt_verified);
        let mut changed: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.copy()).unwrap()).unwrap();
        let ahead = receipt.reserved_through + (1 << PACKET_INDEX_BITS);
        changed["reserved_through"] = serde_json::json!(ahead);
        let bytes = serde_json::to_vec(&changed).unwrap();
        fs::write(
            fixture
                .wal()
                .join(format!("sequence-migration-{ahead}.json")),
            &bytes,
        )
        .unwrap();
        fs::write(fixture.copy(), &bytes).unwrap();
        let report = verify_sequence_migration_receipt(&fixture.wal(), &fixture.copy());
        assert!(!report.receipt_verified);
        assert!(report.reason.contains("behind"), "{report:?}");
    }
}
