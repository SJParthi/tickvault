//! Read-only local namespace preflight. This is deliberately not an issuer,
//! ownership claim, retained-WAL scan, migration, or full namespace proof.

use std::path::Path;

use serde::Serialize;

/// Local observations only. Even a positive result needs an offline writer
/// fence and verified database/history scope before deployment or migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SequenceNamespaceState {
    FreshEmpty,
    ValidDurableAuthority,
    MigrationRequired,
    IncompleteAuthority,
    CorruptAuthority,
    UnsafePath,
    Unverified,
}

#[derive(Debug, Serialize)]
pub struct SequenceNamespaceInspection {
    pub mode: &'static str,
    pub namespace_state: SequenceNamespaceState,
    pub reason: String,
    pub wal_directory: String,
    pub root_wal_files: usize,
    pub replaying_wal_files: usize,
    pub archive_wal_files: usize,
    pub quarantine_wal_files: usize,
    pub other_history_entries: usize,
    pub authority_structurally_valid: bool,
    /// Decimal text preserves exact integers in JSON clients.
    pub reserved_through: Option<String>,
    pub metadata_entries_examined: usize,
    pub exclusive_directory_lock_acquired: bool,
    pub writer_exclusion_verified: bool,
    pub retained_wal_validation: &'static str,
    pub complete_namespace_verified: bool,
    pub migration_applied: bool,
    pub sequence_authority_written: bool,
    pub deployment_authorized: bool,
}

impl SequenceNamespaceInspection {
    fn new(path: &Path) -> Self {
        Self {
            mode: "inspect",
            namespace_state: SequenceNamespaceState::Unverified,
            reason: String::new(),
            wal_directory: path.display().to_string(),
            root_wal_files: 0,
            replaying_wal_files: 0,
            archive_wal_files: 0,
            quarantine_wal_files: 0,
            other_history_entries: 0,
            authority_structurally_valid: false,
            reserved_through: None,
            metadata_entries_examined: 0,
            exclusive_directory_lock_acquired: false,
            writer_exclusion_verified: false,
            retained_wal_validation: "not_run",
            complete_namespace_verified: false,
            migration_applied: false,
            sequence_authority_written: false,
            deployment_authorized: false,
        }
    }

    /// Passes only the local preflight. This is never permission to start a
    /// writer or evidence that a complete capture namespace was verified.
    #[must_use]
    pub fn local_preflight_passed(&self) -> bool {
        matches!(
            self.namespace_state,
            SequenceNamespaceState::FreshEmpty | SequenceNamespaceState::ValidDurableAuthority
        )
    }
}

/// Inspect without acquiring/creating the writer lock or changing files.
/// Two bounded metadata inventories bracket two tiny authority reads. Any
/// observed change, unsafe entry or I/O failure refuses a positive verdict.
/// Stable observations do not prove quiescence or rule out an ABA race.
/// O(E log E) metadata work; each inventory has at most 4096 entries, with no
/// WAL payload reads. The five-second cooperative budget does not bound a blocked kernel/filesystem operation;
/// an operator must also use an external command timeout.
#[must_use]
pub fn inspect_sequence_namespace(path: &Path) -> SequenceNamespaceInspection {
    #[cfg(unix)]
    {
        unix::inspect_with_hook(path, 4096, || {})
    }
    #[cfg(not(unix))]
    {
        let mut report = SequenceNamespaceInspection::new(path);
        report.reason =
            "read-only inode identity inspection is unsupported on this platform".into();
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
    use std::time::{Duration, Instant};

    use crate::ws_frame_spill::{
        ARCHIVE_SUBDIR, MAX_FRAME_SEQUENCE, PACKET_INDEX_BITS, QUARANTINE_SUBDIR, REPLAYING_SUBDIR,
        SEQUENCE_INITIALIZED_FILE, SEQUENCE_MANIFEST_LEN, SEQUENCE_RESERVATION_FILE,
        SEQUENCE_RESERVATION_FRAMES, SEQUENCE_RESERVATION_TMP, SequenceReservations,
        WAL_DIR_LOCK_FILE,
    };
    use sha2::{Digest, Sha256};

    #[derive(Debug)]
    struct Issue(SequenceNamespaceState, String);

    fn unverified(error: impl std::fmt::Display) -> Issue {
        Issue(SequenceNamespaceState::Unverified, error.to_string())
    }

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
        fn of(metadata: &Metadata) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: metadata.mode(),
                links: metadata.nlink(),
                bytes: metadata.len(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
            }
        }
    }

    type Inventory = BTreeMap<PathBuf, Stamp>;
    type AuthorityPair = (Option<Vec<u8>>, Option<Vec<u8>>);

    fn budget(deadline: Instant) -> Result<(), Issue> {
        if Instant::now() >= deadline {
            return Err(unverified("local inspection time budget exhausted"));
        }
        Ok(())
    }

    fn inspect_entry(path: &Path) -> Result<Metadata, Issue> {
        let metadata = std::fs::symlink_metadata(path).map_err(unverified)?;
        if metadata.file_type().is_symlink()
            || (!metadata.is_file() && !metadata.is_dir())
            || (metadata.is_file() && metadata.nlink() != 1)
        {
            return Err(Issue(
                SequenceNamespaceState::UnsafePath,
                format!(
                    "symlink, special file or multiply-linked file: {}",
                    path.display()
                ),
            ));
        }
        Ok(metadata)
    }

    fn inventory(root: &Path, limit: usize, deadline: Instant) -> Result<Inventory, Issue> {
        budget(deadline)?;
        let root_metadata = inspect_entry(root)?;
        if !root_metadata.is_dir() {
            return Err(Issue(
                SequenceNamespaceState::UnsafePath,
                "WAL root is not a directory".into(),
            ));
        }
        let mut entries = Inventory::new();
        entries.insert(PathBuf::new(), Stamp::of(&root_metadata));
        let mut pending = vec![PathBuf::new()];
        while let Some(parent) = pending.pop() {
            for entry in std::fs::read_dir(root.join(&parent)).map_err(unverified)? {
                budget(deadline)?;
                if entries.len() >= limit {
                    return Err(unverified("local inspection entry limit exceeded"));
                }
                let entry = entry.map_err(unverified)?;
                let relative = parent.join(entry.file_name());
                let metadata = inspect_entry(&root.join(&relative))?;
                if metadata.is_dir() {
                    if !parent.as_os_str().is_empty()
                        || ![REPLAYING_SUBDIR, ARCHIVE_SUBDIR, QUARANTINE_SUBDIR]
                            .iter()
                            .any(|name| relative == Path::new(name))
                    {
                        return Err(unverified(
                            "unexpected nested directory; history scope is unverified",
                        ));
                    }
                    pending.push(relative.clone());
                }
                entries.insert(relative, Stamp::of(&metadata));
            }
        }
        Ok(entries)
    }

    fn authority_file(
        root: &Path,
        entries: &Inventory,
        name: &str,
        expected_bytes: usize,
    ) -> Result<Option<Vec<u8>>, Issue> {
        let Some(expected) = entries.get(Path::new(name)) else {
            return Ok(None);
        };
        if expected.bytes != expected_bytes as u64 {
            return Err(Issue(
                SequenceNamespaceState::CorruptAuthority,
                format!("invalid {name} size"),
            ));
        }
        let path = root.join(name);
        let metadata = inspect_entry(&path)?;
        if !metadata.is_file() || &Stamp::of(&metadata) != expected {
            return Err(unverified("authority changed before its read"));
        }
        let file = File::open(&path).map_err(unverified)?;
        if &Stamp::of(&file.metadata().map_err(unverified)?) != expected {
            return Err(unverified("authority inode changed while opening it"));
        }
        let mut bytes = Vec::with_capacity(expected_bytes);
        file.take(expected_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(unverified)?;
        if bytes.len() != expected_bytes || &Stamp::of(&inspect_entry(&path)?) != expected {
            return Err(unverified("authority changed during its read"));
        }
        Ok(Some(bytes))
    }

    fn authority_pair(root: &Path, entries: &Inventory) -> Result<AuthorityPair, Issue> {
        Ok((
            authority_file(
                root,
                entries,
                SEQUENCE_RESERVATION_FILE,
                SEQUENCE_MANIFEST_LEN,
            )?,
            authority_file(root, entries, SEQUENCE_INITIALIZED_FILE, 32)?,
        ))
    }

    pub(super) fn inspect_with_hook(
        path: &Path,
        limit: usize,
        between: impl FnOnce(),
    ) -> SequenceNamespaceInspection {
        let mut report = SequenceNamespaceInspection::new(path);
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = (|| -> Result<(), Issue> {
            let metadata = inspect_entry(path)?;
            if !metadata.is_dir() {
                return Err(Issue(
                    SequenceNamespaceState::UnsafePath,
                    "WAL root is not a directory".into(),
                ));
            }
            let root = std::fs::canonicalize(path).map_err(unverified)?;
            report.wal_directory = root.display().to_string();
            let first = inventory(&root, limit, deadline)?;
            if first.get(Path::new("")) != Some(&Stamp::of(&metadata)) {
                return Err(unverified("WAL root identity changed before inspection"));
            }
            report.metadata_entries_examined = first.len();
            for entry in first.keys().filter(|entry| !entry.as_os_str().is_empty()) {
                if entry
                    .extension()
                    .is_some_and(|extension| extension == "wal")
                {
                    match entry.parent().and_then(Path::to_str) {
                        Some("") => report.root_wal_files += 1,
                        Some(REPLAYING_SUBDIR) => report.replaying_wal_files += 1,
                        Some(ARCHIVE_SUBDIR) => report.archive_wal_files += 1,
                        Some(QUARANTINE_SUBDIR) => report.quarantine_wal_files += 1,
                        _ => return Err(unverified("WAL entry outside the inspected scope")),
                    }
                } else if ![
                    WAL_DIR_LOCK_FILE,
                    SEQUENCE_RESERVATION_FILE,
                    SEQUENCE_INITIALIZED_FILE,
                ]
                .iter()
                .any(|name| entry == Path::new(name))
                {
                    report.other_history_entries += 1;
                }
            }
            let authority = authority_pair(&root, &first);
            between();
            budget(deadline)?;
            let second = inventory(&root, limit, deadline)?;
            if first != second || std::fs::canonicalize(path).map_err(unverified)? != root {
                return Err(unverified(
                    "namespace changed between observations; writer/read concurrency is unverified",
                ));
            }
            let authority = match (authority, authority_pair(&root, &second)) {
                (Ok(first), Ok(second)) if first == second => first,
                (Err(Issue(left, a)), Err(Issue(right, b))) if left == right && a == b => {
                    return Err(Issue(left, a));
                }
                _ => {
                    return Err(unverified(
                        "authority bytes/read outcome changed between observations",
                    ));
                }
            };
            if first.contains_key(Path::new(SEQUENCE_RESERVATION_TMP)) {
                return Err(Issue(
                    SequenceNamespaceState::IncompleteAuthority,
                    "reservation temporary exists; initialization/refill outcome is unverified"
                        .into(),
                ));
            }
            match authority {
                (None, None) => {
                    if first.keys().any(|entry| {
                        !entry.as_os_str().is_empty() && entry != Path::new(WAL_DIR_LOCK_FILE)
                    }) {
                        report.namespace_state = SequenceNamespaceState::MigrationRequired;
                        report.reason = "observed local history without durable sequence authority; offline verified migration required".into();
                    } else {
                        report.namespace_state = SequenceNamespaceState::FreshEmpty;
                        report.reason = "locally empty namespace observed; no database/history scope or writer exclusion was verified".into();
                    }
                }
                (Some(manifest), Some(marker)) => {
                    if !first.contains_key(Path::new(WAL_DIR_LOCK_FILE)) {
                        return Err(Issue(
                            SequenceNamespaceState::IncompleteAuthority,
                            "authority exists without its directory lock file".into(),
                        ));
                    }
                    let digest: [u8; 32] =
                        Sha256::digest(root.as_os_str().as_encoded_bytes()).into();
                    if marker.as_slice() != digest.as_slice() {
                        return Err(Issue(
                            SequenceNamespaceState::CorruptAuthority,
                            "initialization marker belongs to another directory".into(),
                        ));
                    }
                    let bound = SequenceReservations::decode_manifest(&manifest, &digest).map_err(
                        |error| Issue(SequenceNamespaceState::CorruptAuthority, error.to_string()),
                    )?;
                    report.authority_structurally_valid = true;
                    report.reserved_through = Some(bound.to_string());
                    if bound
                        .checked_add(SEQUENCE_RESERVATION_FRAMES << PACKET_INDEX_BITS)
                        .is_none_or(|end| end > MAX_FRAME_SEQUENCE)
                    {
                        return Err(unverified(
                            "valid authority has no reservation headroom for the next writer startup",
                        ));
                    }
                    report.namespace_state = SequenceNamespaceState::ValidDurableAuthority;
                    report.reason = "directory-bound manifest and marker structurally valid in stable local observations; durability, retained WAL and writer exclusion were not re-proven".into();
                }
                _ => {
                    return Err(Issue(
                        SequenceNamespaceState::IncompleteAuthority,
                        "only one of the sequence manifest and initialization marker exists".into(),
                    ));
                }
            }
            Ok(())
        })();
        if let Err(Issue(state, reason)) = result {
            report.namespace_state = state;
            report.reason = reason;
        }
        report
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ws_frame_spill::{
        SEQUENCE_INITIALIZED_FILE, SEQUENCE_RESERVATION_FILE, SEQUENCE_RESERVATION_TMP,
        SequenceReservations, WAL_DIR_LOCK_FILE, lock_wal_dir,
    };
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tv-wsi-{}-{:x}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn authority(&self) -> SequenceReservations {
            let guard = lock_wal_dir(&self.0).unwrap().unwrap();
            SequenceReservations::open(&self.0, &guard).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn fresh_local_directory_is_not_created_claimed_or_authorized() {
        let fixture = Fixture::new();
        let report = inspect_sequence_namespace(&fixture.0);
        assert_eq!(report.namespace_state, SequenceNamespaceState::FreshEmpty);
        assert!(report.local_preflight_passed());
        assert!(!report.writer_exclusion_verified);
        assert!(!report.deployment_authorized);
        assert!(!report.complete_namespace_verified);
        assert!(!report.sequence_authority_written);
        assert_eq!(report.retained_wal_validation, "not_run");
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
        let missing = fixture.0.join("absent");
        assert_eq!(
            inspect_sequence_namespace(&missing).namespace_state,
            SequenceNamespaceState::Unverified
        );
        assert!(!missing.exists());
    }

    #[test]
    fn lock_only_directory_can_be_inspected_while_its_owner_holds_the_lock() {
        let fixture = Fixture::new();
        let _guard = lock_wal_dir(&fixture.0).unwrap().unwrap();
        let report = inspect_sequence_namespace(&fixture.0);
        assert_eq!(report.namespace_state, SequenceNamespaceState::FreshEmpty);
        assert!(!report.exclusive_directory_lock_acquired);
        assert!(
            lock_wal_dir(&fixture.0).is_err(),
            "inspection must not release the held claim"
        );
        assert!(!fixture.0.join(SEQUENCE_RESERVATION_FILE).exists());
    }

    #[test]
    fn legacy_root_archive_replaying_and_quarantine_history_require_migration() {
        let fixture = Fixture::new();
        for directory in ["", "archive", "replaying", "quarantine"] {
            let parent = fixture.0.join(directory);
            fs::create_dir_all(&parent).unwrap();
            fs::write(
                parent.join("retained.wal"),
                b"opaque retained bytes; deliberately not parsed",
            )
            .unwrap();
        }
        let report = inspect_sequence_namespace(&fixture.0);
        assert_eq!(
            report.namespace_state,
            SequenceNamespaceState::MigrationRequired
        );
        assert!(!report.local_preflight_passed());
        assert_eq!(
            (
                report.root_wal_files,
                report.archive_wal_files,
                report.replaying_wal_files,
                report.quarantine_wal_files
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(report.retained_wal_validation, "not_run");
        for directory in ["", "archive", "replaying", "quarantine"] {
            assert_eq!(
                fs::read(fixture.0.join(directory).join("retained.wal")).unwrap(),
                b"opaque retained bytes; deliberately not parsed"
            );
        }
        assert!(!fixture.0.join(WAL_DIR_LOCK_FILE).exists());
        assert!(!fixture.0.join(SEQUENCE_RESERVATION_FILE).exists());
    }

    #[test]
    fn other_history_cannot_be_mistaken_for_a_fresh_namespace() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("applied.tvaw"), b"old applied state").unwrap();
        let report = inspect_sequence_namespace(&fixture.0);
        assert_eq!(
            report.namespace_state,
            SequenceNamespaceState::MigrationRequired
        );
        assert_eq!(report.other_history_entries, 1);
    }

    #[test]
    fn existing_authority_uses_the_real_codec_without_allocating_or_reserving() {
        let fixture = Fixture::new();
        let issuer = fixture.authority();
        let before = fs::read(fixture.0.join(SEQUENCE_RESERVATION_FILE)).unwrap();
        let bound = issuer.reserved_through.load(Ordering::Acquire);
        let cursor = issuer.cursor.load(Ordering::Acquire);
        let report = inspect_sequence_namespace(&fixture.0);
        assert_eq!(
            report.namespace_state,
            SequenceNamespaceState::ValidDurableAuthority
        );
        assert!(report.authority_structurally_valid);
        assert_eq!(report.reserved_through, Some(bound.to_string()));
        assert!(report.local_preflight_passed());
        assert!(!report.deployment_authorized);
        assert!(!report.writer_exclusion_verified);
        assert_eq!(issuer.cursor.load(Ordering::Acquire), cursor);
        assert_eq!(issuer.reserved_through.load(Ordering::Acquire), bound);
        assert_eq!(
            fs::read(fixture.0.join(SEQUENCE_RESERVATION_FILE)).unwrap(),
            before
        );
        assert!(lock_wal_dir(&fixture.0).is_err());
    }

    #[test]
    fn partial_authority_and_temporary_reservation_refuse_a_positive_verdict() {
        for missing in [
            SEQUENCE_RESERVATION_FILE,
            SEQUENCE_INITIALIZED_FILE,
            WAL_DIR_LOCK_FILE,
        ] {
            let fixture = Fixture::new();
            drop(fixture.authority());
            fs::remove_file(fixture.0.join(missing)).unwrap();
            let report = inspect_sequence_namespace(&fixture.0);
            assert_eq!(
                report.namespace_state,
                SequenceNamespaceState::IncompleteAuthority,
                "{missing}: {}",
                report.reason
            );
            assert!(!report.local_preflight_passed());
        }
        let fixture = Fixture::new();
        let _issuer = fixture.authority();
        fs::write(fixture.0.join(SEQUENCE_RESERVATION_TMP), b"uncommitted").unwrap();
        assert_eq!(
            inspect_sequence_namespace(&fixture.0).namespace_state,
            SequenceNamespaceState::IncompleteAuthority
        );
    }

    #[test]
    fn corrupt_or_wrong_directory_authority_is_not_a_migration_success() {
        let fixture = Fixture::new();
        let _issuer = fixture.authority();
        let manifest = fixture.0.join(SEQUENCE_RESERVATION_FILE);
        let mut bytes = fs::read(&manifest).unwrap();
        bytes[48] ^= 1;
        fs::write(&manifest, &bytes).unwrap();
        let report = inspect_sequence_namespace(&fixture.0);
        assert_eq!(
            report.namespace_state,
            SequenceNamespaceState::CorruptAuthority
        );
        assert!(!report.migration_applied);
        assert_eq!(fs::read(&manifest).unwrap(), bytes);

        let origin = Fixture::new();
        let _issuer = origin.authority();
        let copied = Fixture::new();
        for name in [
            WAL_DIR_LOCK_FILE,
            SEQUENCE_RESERVATION_FILE,
            SEQUENCE_INITIALIZED_FILE,
        ] {
            fs::copy(origin.0.join(name), copied.0.join(name)).unwrap();
        }
        assert_eq!(
            inspect_sequence_namespace(&copied.0).namespace_state,
            SequenceNamespaceState::CorruptAuthority
        );
        fs::write(copied.0.join(SEQUENCE_RESERVATION_FILE), b"torn").unwrap();
        assert_eq!(
            inspect_sequence_namespace(&copied.0).namespace_state,
            SequenceNamespaceState::CorruptAuthority
        );
    }

    #[test]
    fn symlink_special_file_and_hardlink_paths_are_refused_without_following_payloads() {
        let fixture = Fixture::new();
        let linked = fixture.0.with_extension("link");
        symlink(&fixture.0, &linked).unwrap();
        assert_eq!(
            inspect_sequence_namespace(&linked).namespace_state,
            SequenceNamespaceState::UnsafePath
        );
        fs::remove_file(linked).unwrap();

        let target = Fixture::new();
        symlink(&target.0, fixture.0.join("archive")).unwrap();
        assert_eq!(
            inspect_sequence_namespace(&fixture.0).namespace_state,
            SequenceNamespaceState::UnsafePath
        );
        fs::remove_file(fixture.0.join("archive")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(fixture.0.join("socket")).unwrap();
        assert_eq!(
            inspect_sequence_namespace(&fixture.0).namespace_state,
            SequenceNamespaceState::UnsafePath
        );
        fs::remove_file(fixture.0.join("socket")).unwrap();
        fs::write(target.0.join("original"), b"retained").unwrap();
        fs::hard_link(target.0.join("original"), fixture.0.join("alias.wal")).unwrap();
        assert_eq!(
            inspect_sequence_namespace(&fixture.0).namespace_state,
            SequenceNamespaceState::UnsafePath
        );
        assert_eq!(fs::read(target.0.join("original")).unwrap(), b"retained");
    }

    #[test]
    fn concurrent_history_or_authority_changes_are_unverified() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("active.wal"), b"before").unwrap();
        let report = unix::inspect_with_hook(&fixture.0, 4096, || {
            fs::write(fixture.0.join("active.wal"), b"new appended history").unwrap();
        });
        assert_eq!(report.namespace_state, SequenceNamespaceState::Unverified);
        assert_eq!(report.root_wal_files, 1);
        assert!(!report.local_preflight_passed());

        let authority = Fixture::new();
        let _issuer = authority.authority();
        let report = unix::inspect_with_hook(&authority.0, 4096, || {
            fs::write(authority.0.join(SEQUENCE_INITIALIZED_FILE), [7; 32]).unwrap();
        });
        assert_eq!(report.namespace_state, SequenceNamespaceState::Unverified);
    }

    #[test]
    fn bounded_or_unknown_directory_scope_never_reports_complete() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("one.wal"), b"one").unwrap();
        fs::write(fixture.0.join("two.wal"), b"two").unwrap();
        let report = unix::inspect_with_hook(&fixture.0, 2, || {});
        assert_eq!(report.namespace_state, SequenceNamespaceState::Unverified);
        assert!(!report.local_preflight_passed());
        fs::create_dir(fixture.0.join("unrecognized-history")).unwrap();
        assert_eq!(
            inspect_sequence_namespace(&fixture.0).namespace_state,
            SequenceNamespaceState::Unverified
        );
    }
}
