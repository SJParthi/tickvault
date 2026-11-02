//! Regression: 2026-11-02 — integrity and recovery boundaries for evening state.
//! Independent adversarial cases; local filesystem fixtures only.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tickvault_app::evening_drain::{EveningState, load_state, load_state_for_evening};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tv-independent-evening-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("reserve new owned directory without overwrite");
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove owned fixture");
    }
}
fn fixture(path: &Path, bytes: &[u8], when: SystemTime) {
    std::fs::write(path, bytes).expect("write fixture");
    std::fs::File::open(path)
        .expect("open fixture")
        .set_modified(when)
        .expect("set real mtime");
    assert_eq!(
        std::fs::metadata(path)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        when,
        "filesystem must preserve the precise timestamp for this boundary test"
    );
}
// 1970-01-01 10:10 UTC = 15:40 IST. No production clock is consulted.
const BOUNDARY: i64 = 36_600;
const NOW: i64 = BOUNDARY + 600;
const BROKEN: &[u8] = b"v2 torn-and-untrusted";

#[test]
fn independent_one_nanosecond_before_boundary_is_the_only_recoverable_case() {
    let base = Temp::new();
    let boundary = SystemTime::UNIX_EPOCH + Duration::from_secs(BOUNDARY as u64);
    for (tag, when, recover) in [
        ("before", boundary - Duration::from_nanos(1), true),
        ("exact", boundary, false),
        ("after", boundary + Duration::from_nanos(1), false),
        ("future", boundary + Duration::from_secs(86_400), false),
    ] {
        let dir = base.0.join(tag);
        std::fs::create_dir(&dir).expect("case dir");
        let path = dir.join("evening-drain.state");
        fixture(&path, BROKEN, when);
        let result = load_state_for_evening(&path, NOW);
        let quarantine = dir.join("evening-drain.state.bad-36600").join("state");
        if recover {
            assert_eq!(result, Some(EveningState::default()));
            assert!(!path.exists());
            assert_eq!(
                std::fs::read(&quarantine).expect("retained quarantined bytes"),
                BROKEN
            );
        } else {
            assert_eq!(result, None);
            assert_eq!(
                std::fs::read(&path).expect("preserved active bytes"),
                BROKEN
            );
            assert!(!quarantine.exists());
        }
    }
}

#[test]
fn independent_existing_quarantine_blocks_reset_without_overwriting_any_evidence() {
    let dir = Temp::new();
    let path = dir.0.join("evening-drain.state");
    fixture(
        &path,
        BROKEN,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1),
    );
    let quarantine_dir = dir.0.join("evening-drain.state.bad-36600");
    std::fs::create_dir(&quarantine_dir).expect("existing quarantine");
    let evidence = quarantine_dir.join("state");
    std::fs::write(&evidence, b"previous evidence").expect("evidence");
    assert_eq!(load_state_for_evening(&path, NOW), None);
    assert_eq!(std::fs::read(&path).expect("original"), BROKEN);
    assert_eq!(
        std::fs::read(&evidence).expect("previous evidence"),
        b"previous evidence"
    );
}

#[test]
fn independent_legacy_migration_uses_mtime_and_never_the_untrusted_day_fields() {
    let dir = Temp::new();
    let path = dir.0.join("evening-drain.state");
    let legacy = b"v1 2147483647 0 0 0 0 0\n";
    fixture(
        &path,
        legacy,
        SystemTime::UNIX_EPOCH + Duration::from_secs(BOUNDARY as u64),
    );
    assert_eq!(load_state(&path), None);
    assert_eq!(load_state_for_evening(&path, NOW), None);
    assert_eq!(
        std::fs::read(&path).expect("preserved current legacy"),
        legacy
    );
    assert_eq!(
        load_state_for_evening(&path, NOW + 86_400),
        Some(EveningState::default())
    );
    assert_eq!(
        std::fs::read(dir.0.join("evening-drain.state.bad-123000/state")).expect("legacy retained"),
        legacy
    );
}

#[cfg(unix)]
#[test]
fn independent_stale_symlink_is_never_quarantined_as_a_regular_state_file() {
    let dir = Temp::new();
    let target = dir.0.join("target");
    let link = dir.0.join("evening-drain.state");
    fixture(
        &target,
        BROKEN,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1),
    );
    std::os::unix::fs::symlink(&target, &link).expect("owned symlink");
    assert_eq!(load_state_for_evening(&link, NOW), None);
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("symlink retained")
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&target).expect("target preserved"), BROKEN);
    assert!(!dir.0.join("evening-drain.state.bad-36600").exists());
}

#[test]
fn test_regression_invalid_clock_never_archives_untrusted_bytes() {
    let dir = Temp::new();
    let path = dir.0.join("evening-drain.state");
    fixture(
        &path,
        BROKEN,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1),
    );
    for now in [i64::MIN, -1, 0, i64::MAX] {
        assert_eq!(load_state_for_evening(&path, now), None, "clock {now}");
        assert_eq!(std::fs::read(&path).expect("preserved"), BROKEN);
    }
    assert_eq!(std::fs::read_dir(&dir.0).expect("directory").count(), 1);
}

#[test]
fn test_regression_midnight_uses_the_same_evening_cutoff() {
    let dir = Temp::new();
    let path = dir.0.join("evening-drain.state");
    fixture(
        &path,
        BROKEN,
        SystemTime::UNIX_EPOCH + Duration::from_secs(BOUNDARY as u64),
    );
    // 1970-01-02 02:00 IST still belongs to the prior 15:40 evening.
    assert_eq!(load_state_for_evening(&path, 73_800), None);
    assert_eq!(std::fs::read(&path).expect("preserved"), BROKEN);
}

#[test]
fn test_regression_valid_old_v2_keeps_its_cap_and_bytes() {
    let dir = Temp::new();
    let path = dir.0.join("evening-drain.state");
    let state = EveningState {
        day: 739_910,
        restarts: 3,
        gave_up: true,
        ..EveningState::default()
    };
    let bytes = state.to_line();
    fixture(
        &path,
        bytes.as_bytes(),
        SystemTime::UNIX_EPOCH + Duration::from_secs(1),
    );
    assert_eq!(load_state_for_evening(&path, NOW), Some(state));
    assert_eq!(std::fs::read(&path).expect("preserved"), bytes.as_bytes());
    assert!(!dir.0.join("evening-drain.state.bad-36600").exists());
}

#[test]
fn test_regression_oversized_state_is_refused_and_old_bytes_are_preserved() {
    let dir = Temp::new();
    let path = dir.0.join("evening-drain.state");
    let oversized = vec![b'x'; 8_192];
    fixture(
        &path,
        &oversized,
        SystemTime::UNIX_EPOCH + Duration::from_secs(BOUNDARY as u64),
    );
    assert_eq!(load_state(&path), None);
    assert_eq!(load_state_for_evening(&path, NOW), None);
    assert_eq!(std::fs::read(&path).expect("preserved"), oversized);
    std::fs::File::open(&path)
        .expect("open")
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .expect("old mtime");
    assert_eq!(
        load_state_for_evening(&path, NOW),
        Some(EveningState::default())
    );
    assert_eq!(
        std::fs::read(dir.0.join("evening-drain.state.bad-36600/state")).expect("archived"),
        oversized
    );
}

#[cfg(unix)]
#[test]
fn test_regression_symlink_temp_cannot_overwrite_an_unrelated_file() {
    let dir = Temp::new();
    let path = dir.0.join("evening-drain.state");
    let target = dir.0.join("evidence");
    std::fs::write(&target, b"preserve").expect("target");
    std::os::unix::fs::symlink(&target, path.with_extension("tmp")).expect("symlink");
    assert!(tickvault_app::evening_drain::store_state(&path, &EveningState::default()).is_err());
    assert_eq!(
        std::fs::read(&target).expect("target unchanged"),
        b"preserve"
    );
    assert!(!path.exists());
}
