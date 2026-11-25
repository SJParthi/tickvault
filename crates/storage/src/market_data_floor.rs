//! The durable market-data floor (live audit F2/F5/F6, 2026-10-29).
//!
//! # Why it exists
//!
//! Spill replay judged each line by its time of DAY only
//! (`tick_spill_replay::line_is_positively_out_of_window`), never by its date.
//! After a wipe, a spill file still holding rows from earlier days was
//! therefore replayed in full: on 2026-10-29 about 617,000 rows from 9, 10
//! and 12 October came back into `ticks`.
//!
//! The floor is the fresh-start reset's cutoff: its first-boot marker less
//! the reset's own lag margin (`fresh_start_reset::market_data_floor_from_marker`),
//! so a row the reset kept in the fresh tables is never below it. Once the
//! reset is done, EVERY boot reads the marker and records the floor
//! (`fresh_start_reset::record_market_data_floor_at_boot`), installs it, and
//! keeps it in a small state file for a boot whose QuestDB does not answer.
//! The tick and depth spill replay set aside every line whose row timestamp
//! is below the floor, to the spill file's `quarantine/` (kept on disk, never
//! posted); the WAL refold skips a frame whose recorded receipt is below it
//! ([`wal_frame_is_below_floor`]).
//!
//! # File format
//!
//! One line, `tvmdf1 <unix nanos> <crc32 of "tvmdf1 <unix nanos>", 8 lowercase hex>\n`.
//! Written to a temp file, synced, renamed over the old one, and the folder is
//! synced. A file that does not parse exactly (a torn write, a hand edit, a
//! flipped bit) is REFUSED loudly and read as no floor: refusing to replay on
//! a guessed floor could set aside today's rows, which is worse than the
//! old behaviour.
//!
//! # Complexity
//!
//! Boot and reset only: one small file read or write. The replay pays one
//! atomic load per round ([`installed_market_data_floor_unix_nanos`]) and one
//! `i64` compare per spill line.

use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

use tickvault_common::error_code::ErrorCode;
use tracing::{error, info};

/// Where the floor lives, relative to the working directory like every other
/// `data/state/` marker.
pub const MARKET_DATA_FLOOR_PATH: &str = "data/state/market_data_floor";

/// Counter of spill-replay rows set aside because they are older than the
/// floor, labelled `dir` (`ticks` / `depth`) and `unit` (`line` for a text
/// spill line, `file` for a whole framed depth file). Local `/metrics` only;
/// no alarm, no EMF name.
pub const SPILL_REPLAY_BELOW_FLOOR_COUNTER: &str = "tv_spill_replay_below_floor_set_aside_total";

/// Counter of replayed WAL frames the refold skipped because their recorded
/// receipt is below the floor. Local `/metrics` only; no alarm, no EMF name.
/// The frames stay in their segment (archived like any replayed segment).
pub const WAL_REPLAY_BELOW_FLOOR_COUNTER: &str = "tv_wal_replay_below_floor_frames_total";

/// Counter of floor files refused at read (torn or malformed).
pub const MARKET_DATA_FLOOR_REFUSED_COUNTER: &str = "tv_market_data_floor_refused_total";

/// The first token of the file; the version is in it.
const FLOOR_MAGIC: &str = "tvmdf1";

/// "No floor" in the installed atomic.
const NO_FLOOR: i64 = i64::MIN;

/// How far ahead of this machine's wall clock a floor may be and still be
/// trusted: 5 minutes. A floor further ahead (a clock that ran fast when it
/// was written, a hand edit, a bad marker) would set aside rows captured
/// today, so it is REFUSED and read as no floor (2026-10-29 review, item 4c).
pub const MARKET_DATA_FLOOR_MAX_AHEAD_NANOS: i64 = 5 * 60 * 1_000_000_000;

/// This machine's wall clock, Unix nanoseconds (0 if before 1970, which no
/// floor is ahead of in practice; saturates far in the future).
#[must_use]
pub(crate) fn wall_clock_unix_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Whether `floor_unix_nanos` is more than [`MARKET_DATA_FLOOR_MAX_AHEAD_NANOS`]
/// ahead of `now_unix_nanos`. Saturating. O(1).
#[must_use]
pub(crate) const fn floor_is_too_far_ahead(floor_unix_nanos: i64, now_unix_nanos: i64) -> bool {
    floor_unix_nanos > now_unix_nanos.saturating_add(MARKET_DATA_FLOOR_MAX_AHEAD_NANOS)
}

/// Logs and counts a floor refused for being too far ahead of the wall clock.
/// `origin` names where it came from (`file` or `marker`).
pub(crate) fn report_floor_ahead_of_wall_clock(
    origin: &'static str,
    path: &Path,
    floor_unix_nanos: i64,
    now_unix_nanos: i64,
) {
    metrics::counter!(MARKET_DATA_FLOOR_REFUSED_COUNTER).increment(1);
    error!(
        code = ErrorCode::TickSpill01FileQuarantined.code_str(),
        source = "market_data_floor_ahead_of_wall_clock",
        origin,
        path = %path.display(),
        floor_unix_nanos,
        now_unix_nanos,
        ahead_secs = floor_unix_nanos.saturating_sub(now_unix_nanos) / 1_000_000_000,
        "the market-data floor is more than 5 minutes ahead of this machine's clock and is \
         REFUSED: it would set aside rows captured today. It is treated as NO floor, so spill \
         and WAL replays post every row as they did before the floor existed. Check the clock \
         (chrony) and the floor file."
    );
}

/// The floor this process judges replayed lines against, Unix nanoseconds.
static INSTALLED_FLOOR_UNIX_NANOS: AtomicI64 = AtomicI64::new(NO_FLOOR);

/// Guards the once-per-process boot read ([`ensure_market_data_floor_loaded`]).
static BOOT_LOAD: std::sync::Once = std::sync::Once::new();

/// What reading the floor file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorRead {
    /// No file: no floor, the replay behaves as before.
    Absent,
    /// A valid floor, Unix nanoseconds.
    Floor(i64),
    /// A file exists but is torn, malformed or unreadable. Read as no floor.
    Refused(&'static str),
}

/// Registers every floor counter at 0, every label pair. O(1).
pub fn register_market_data_floor_baseline() {
    for dir in ["ticks", "depth"] {
        metrics::counter!(SPILL_REPLAY_BELOW_FLOOR_COUNTER, "dir" => dir, "unit" => "line")
            .increment(0);
    }
    metrics::counter!(SPILL_REPLAY_BELOW_FLOOR_COUNTER, "dir" => "depth", "unit" => "file")
        .increment(0);
    metrics::counter!(MARKET_DATA_FLOOR_REFUSED_COUNTER).increment(0);
    metrics::counter!(WAL_REPLAY_BELOW_FLOOR_COUNTER).increment(0);
}

/// Whether a replayed WAL frame whose record carries `receipt_unix_nanos` is
/// older than `floor_unix_nanos`. A receipt outside the plausible band
/// (including the pre-TVW3 "unknown" sentinel) is never judged: nothing
/// proves when that frame arrived, so it is folded as before. With no floor,
/// always `false`. O(1): a few compares.
#[must_use]
pub const fn wal_frame_is_below_floor(
    receipt_unix_nanos: i64,
    floor_unix_nanos: Option<i64>,
) -> bool {
    let receipt = crate::ws_frame_spill::plausible_receipt_nanos(receipt_unix_nanos);
    match floor_unix_nanos {
        Some(floor) => {
            receipt != crate::ws_frame_spill::WAL_RECEIPT_UNKNOWN_NANOS && receipt < floor
        }
        None => false,
    }
}

/// The body before the checksum: `tvmdf1 <nanos>`.
fn floor_body(floor_unix_nanos: i64) -> String {
    format!("{FLOOR_MAGIC} {floor_unix_nanos}")
}

/// CRC-32 of the body, as 8 lowercase hex digits.
fn floor_checksum(body: &str) -> String {
    format!(
        "{:08x}",
        crate::ws_frame_spill::crc32_ieee_of(&[body.as_bytes()])
    )
}

/// The exact bytes of a floor file for `floor_unix_nanos`.
#[must_use]
pub fn encode_market_data_floor(floor_unix_nanos: i64) -> String {
    let body = floor_body(floor_unix_nanos);
    let crc = floor_checksum(&body);
    format!("{body} {crc}\n")
}

/// Parses a floor file strictly. Anything but the exact shape
/// [`encode_market_data_floor`] writes is an `Err` naming the reason.
///
/// # Errors
/// `"not_utf8"`, `"no_newline"`, `"shape"`, `"magic"`, `"nanos"`,
/// `"not_positive"` or `"checksum"`.
pub fn parse_market_data_floor(bytes: &[u8]) -> Result<i64, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not_utf8")?;
    let line = text.strip_suffix('\n').ok_or("no_newline")?;
    let mut parts = line.split(' ');
    let (Some(magic), Some(nanos), Some(crc), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("shape");
    };
    if magic != FLOOR_MAGIC {
        return Err("magic");
    }
    let floor: i64 = nanos.parse().map_err(|_| "nanos")?;
    if floor <= 0 {
        return Err("not_positive");
    }
    // The body must round-trip byte for byte (no leading zeros or a `+`).
    let body = floor_body(floor);
    if body.len() != magic.len() + 1 + nanos.len() || crc != floor_checksum(&body) {
        return Err("checksum");
    }
    Ok(floor)
}

/// Reads the floor file at `path`. A refused file is logged as a coded
/// `error!` and counted; the caller treats it as no floor. A floor more than
/// [`MARKET_DATA_FLOOR_MAX_AHEAD_NANOS`] ahead of the wall clock is refused
/// too (`Refused("ahead_of_wall_clock")`).
#[must_use]
pub fn read_market_data_floor(path: &Path) -> FloorRead {
    read_market_data_floor_at(path, wall_clock_unix_nanos())
}

/// [`read_market_data_floor`] against an explicit wall clock (tests).
fn read_market_data_floor_at(path: &Path, now_unix_nanos: i64) -> FloorRead {
    match read_market_data_floor_unchecked(path) {
        FloorRead::Floor(floor) if floor_is_too_far_ahead(floor, now_unix_nanos) => {
            report_floor_ahead_of_wall_clock("file", path, floor, now_unix_nanos);
            FloorRead::Refused("ahead_of_wall_clock")
        }
        other => other,
    }
}

/// The file read and strict parse, with no clock check.
fn read_market_data_floor_unchecked(path: &Path) -> FloorRead {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return FloorRead::Absent,
        Err(err) => {
            metrics::counter!(MARKET_DATA_FLOOR_REFUSED_COUNTER).increment(1);
            error!(
                code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                source = "market_data_floor_unreadable",
                path = %path.display(),
                %err,
                "the market-data floor file exists but could not be read. It is treated as \
                 NO floor: spill replay posts every line as it did before the floor existed, \
                 so rows older than the last reset can come back into the database. Check \
                 the file's owner and mode."
            );
            return FloorRead::Refused("unreadable");
        }
    };
    match parse_market_data_floor(&bytes) {
        Ok(floor) => FloorRead::Floor(floor),
        Err(reason) => {
            metrics::counter!(MARKET_DATA_FLOOR_REFUSED_COUNTER).increment(1);
            error!(
                code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                source = "market_data_floor_refused",
                path = %path.display(),
                reason,
                bytes = bytes.len(),
                "the market-data floor file is torn or malformed and is REFUSED. It is \
                 treated as NO floor: spill replay posts every line as it did before the floor \
                 existed, so rows older than the last reset can come back into the database. \
                 The file is left as it is for inspection."
            );
            FloorRead::Refused(reason)
        }
    }
}

/// Writes the floor durably: temp file, `sync_all`, rename over `path`, then
/// the folder is synced. Cold path (once per reset).
///
/// # Errors
/// Any I/O error from creating the folder, writing, syncing or renaming.
pub fn write_market_data_floor(path: &Path, floor_unix_nanos: i64) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp_name);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(encode_market_data_floor(floor_unix_nanos).as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Installs `floor_unix_nanos` for this process. Only ever RAISES the floor
/// (`fetch_max`), so a later read can never lower one a reset just set.
pub fn install_market_data_floor(floor_unix_nanos: i64) {
    INSTALLED_FLOOR_UNIX_NANOS.fetch_max(floor_unix_nanos, Ordering::AcqRel);
}

/// The floor installed in this process, if any. One atomic load.
#[must_use]
pub fn installed_market_data_floor_unix_nanos() -> Option<i64> {
    let floor = INSTALLED_FLOOR_UNIX_NANOS.load(Ordering::Acquire);
    (floor != NO_FLOOR).then_some(floor)
}

/// Boot read: registers the counters, reads `path` and installs a valid
/// floor. Returns what it installed. An absent or refused file installs
/// nothing.
pub fn load_market_data_floor_at_boot(path: &Path) -> Option<i64> {
    register_market_data_floor_baseline();
    match read_market_data_floor(path) {
        FloorRead::Floor(floor) => {
            install_market_data_floor(floor);
            info!(
                path = %path.display(),
                floor_unix_nanos = floor,
                "market-data floor loaded: spill replay sets aside rows older than it"
            );
            Some(floor)
        }
        FloorRead::Absent | FloorRead::Refused(_) => None,
    }
}

/// [`load_market_data_floor_at_boot`] on [`MARKET_DATA_FLOOR_PATH`], at most
/// once per process. Called where the spill replay is spawned at boot.
pub fn ensure_market_data_floor_loaded() {
    BOOT_LOAD.call_once(|| {
        let _installed = load_market_data_floor_at_boot(Path::new(MARKET_DATA_FLOOR_PATH));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("tv-mdf-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    const FLOOR: i64 = 1_761_696_000_000_000_000;

    #[test]
    fn test_encode_market_data_floor_then_parse_round_trips() {
        let text = encode_market_data_floor(FLOOR);
        assert!(text.starts_with("tvmdf1 1761696000000000000 "));
        assert!(text.ends_with('\n'));
        assert_eq!(parse_market_data_floor(text.as_bytes()), Ok(FLOOR));
    }

    #[test]
    fn test_parse_market_data_floor_refuses_every_torn_prefix() {
        let text = encode_market_data_floor(FLOOR);
        let bytes = text.as_bytes();
        for cut in 0..bytes.len() {
            assert!(
                parse_market_data_floor(&bytes[..cut]).is_err(),
                "a file torn at byte {cut} must be refused"
            );
        }
    }

    #[test]
    fn test_parse_market_data_floor_refuses_a_flipped_digit_and_bad_shapes() {
        let text = encode_market_data_floor(FLOOR).replace("1761696", "1761697");
        assert_eq!(parse_market_data_floor(text.as_bytes()), Err("checksum"));
        assert_eq!(parse_market_data_floor(b"tvmdf1 5\n"), Err("shape"));
        assert_eq!(
            parse_market_data_floor(b"xxxxx1 5 00000000\n"),
            Err("magic")
        );
        let neg = encode_market_data_floor(-5);
        assert_eq!(parse_market_data_floor(neg.as_bytes()), Err("not_positive"));
        let padded = encode_market_data_floor(FLOOR).replace("tvmdf1 1", "tvmdf1 01");
        assert!(parse_market_data_floor(padded.as_bytes()).is_err());
    }

    #[test]
    fn test_write_market_data_floor_then_read_it_back() {
        let dir = temp_dir("rw");
        let path = dir.join("state").join("market_data_floor");
        assert_eq!(read_market_data_floor(&path), FloorRead::Absent);
        write_market_data_floor(&path, FLOOR).expect("write");
        assert_eq!(read_market_data_floor(&path), FloorRead::Floor(FLOOR));
        assert!(!dir.join("state").join("market_data_floor.tmp").exists());
        // A second write replaces the first.
        write_market_data_floor(&path, FLOOR + 1).expect("rewrite");
        assert_eq!(read_market_data_floor(&path), FloorRead::Floor(FLOOR + 1));
        let _cleanup = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_market_data_floor_refuses_a_torn_file_as_no_floor() {
        let dir = temp_dir("torn");
        let path = dir.join("market_data_floor");
        let text = encode_market_data_floor(FLOOR);
        std::fs::write(&path, &text.as_bytes()[..text.len() - 4]).expect("write torn");
        assert!(matches!(
            read_market_data_floor(&path),
            FloorRead::Refused(_)
        ));
        assert_eq!(load_market_data_floor_at_boot(&path), None);
        let _cleanup = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_market_data_floor_at_boot_with_no_file_installs_nothing() {
        let dir = temp_dir("absent");
        assert_eq!(
            load_market_data_floor_at_boot(&dir.join("market_data_floor")),
            None
        );
        let _cleanup = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_install_market_data_floor_only_raises() {
        // A floor of 1 ns (1970) is below every plausible row timestamp, so
        // installing it cannot change any other test's replay verdict.
        install_market_data_floor(1);
        assert!(installed_market_data_floor_unix_nanos().is_some_and(|f| f >= 1));
        install_market_data_floor(0);
        assert!(installed_market_data_floor_unix_nanos().is_some_and(|f| f >= 1));
    }

    #[test]
    fn test_installed_market_data_floor_unix_nanos_is_never_below_an_installed_floor() {
        // 1 ns (1970): below every plausible row stamp, harmless to other tests.
        install_market_data_floor(1);
        let seen = installed_market_data_floor_unix_nanos();
        assert!(seen.is_some_and(|f| f >= 1), "got {seen:?}");
    }

    #[test]
    fn test_wal_frame_is_below_floor_judges_only_a_plausible_receipt() {
        assert!(wal_frame_is_below_floor(FLOOR - 1, Some(FLOOR)));
        assert!(!wal_frame_is_below_floor(FLOOR, Some(FLOOR)));
        assert!(!wal_frame_is_below_floor(FLOOR + 1, Some(FLOOR)));
        // No floor: never below.
        assert!(!wal_frame_is_below_floor(FLOOR - 1, None));
        // The unknown sentinel and implausible receipts are never judged.
        assert!(!wal_frame_is_below_floor(
            crate::ws_frame_spill::WAL_RECEIPT_UNKNOWN_NANOS,
            Some(FLOOR)
        ));
        assert!(!wal_frame_is_below_floor(5, Some(FLOOR)));
        assert!(!wal_frame_is_below_floor(-1, Some(FLOOR)));
    }

    /// Records every counter key registered, scoped with
    /// `metrics::with_local_recorder` (never a global install).
    #[derive(Default)]
    struct KeyRecorder {
        keys: std::sync::Mutex<Vec<String>>,
    }

    impl KeyRecorder {
        fn keys(&self) -> Vec<String> {
            self.keys.lock().map(|k| k.clone()).unwrap_or_default()
        }
    }

    impl metrics::Recorder for KeyRecorder {
        fn describe_counter(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn register_counter(
            &self,
            key: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            let mut labels: Vec<String> = key
                .labels()
                .map(|l| format!("{}={}", l.key(), l.value()))
                .collect();
            labels.sort();
            if let Ok(mut keys) = self.keys.lock() {
                keys.push(format!("{}{{{}}}", key.name(), labels.join(",")));
            }
            metrics::Counter::noop()
        }
        fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
            metrics::Gauge::noop()
        }
        fn register_histogram(
            &self,
            _: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    #[test]
    fn test_register_market_data_floor_baseline_can_run_twice() {
        let recorder = KeyRecorder::default();
        metrics::with_local_recorder(&recorder, || {
            register_market_data_floor_baseline();
            register_market_data_floor_baseline();
        });
        let keys = recorder.keys();
        // Five series per call, the same five both times.
        assert_eq!(keys.len(), 10, "{keys:?}");
        let mut distinct = keys;
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 5, "{distinct:?}");
        for expected in [
            format!("{SPILL_REPLAY_BELOW_FLOOR_COUNTER}{{dir=ticks,unit=line}}"),
            format!("{SPILL_REPLAY_BELOW_FLOOR_COUNTER}{{dir=depth,unit=line}}"),
            format!("{SPILL_REPLAY_BELOW_FLOOR_COUNTER}{{dir=depth,unit=file}}"),
            format!("{MARKET_DATA_FLOOR_REFUSED_COUNTER}{{}}"),
            format!("{WAL_REPLAY_BELOW_FLOOR_COUNTER}{{}}"),
        ] {
            assert!(
                distinct.contains(&expected),
                "{expected} not registered: {distinct:?}"
            );
        }
    }

    const FIVE_MIN_NANOS: i64 = 300 * 1_000_000_000;

    #[test]
    fn test_floor_is_too_far_ahead_only_past_five_minutes() {
        assert_eq!(MARKET_DATA_FLOOR_MAX_AHEAD_NANOS, FIVE_MIN_NANOS);
        let now = FLOOR;
        assert!(!floor_is_too_far_ahead(now - 1, now), "the past is fine");
        assert!(!floor_is_too_far_ahead(now, now));
        assert!(
            !floor_is_too_far_ahead(now + FIVE_MIN_NANOS, now),
            "exactly 5 min"
        );
        assert!(floor_is_too_far_ahead(now + FIVE_MIN_NANOS + 1, now));
        assert!(floor_is_too_far_ahead(i64::MAX, now));
        // Saturating: a clock at the far end never overflows.
        assert!(!floor_is_too_far_ahead(i64::MAX, i64::MAX));
    }

    #[test]
    fn test_wall_clock_unix_nanos_is_after_2026() {
        // 2026-01-01T00:00:00Z.
        assert!(wall_clock_unix_nanos() > 1_767_225_600_000_000_000);
    }

    /// 2026-10-29 review (item 4c): a floor file dated more than 5 minutes
    /// past the wall clock is refused, counted, and read as no floor.
    #[test]
    fn test_read_market_data_floor_refuses_a_floor_ahead_of_the_wall_clock() {
        let dir = temp_dir("ahead");
        let path = dir.join("market_data_floor");
        let now = FLOOR;
        write_market_data_floor(&path, now + FIVE_MIN_NANOS + 1).expect("write");
        let recorder = KeyRecorder::default();
        let read =
            metrics::with_local_recorder(&recorder, || read_market_data_floor_at(&path, now));
        assert_eq!(read, FloorRead::Refused("ahead_of_wall_clock"));
        assert!(
            recorder
                .keys()
                .contains(&format!("{MARKET_DATA_FLOOR_REFUSED_COUNTER}{{}}")),
            "the refusal is counted"
        );
        // Up to 5 minutes ahead is accepted, and the past always is.
        write_market_data_floor(&path, now + FIVE_MIN_NANOS).expect("write");
        assert_eq!(
            read_market_data_floor_at(&path, now),
            FloorRead::Floor(now + FIVE_MIN_NANOS)
        );
        // Against the real clock a 2025 floor is in the past: accepted.
        write_market_data_floor(&path, FLOOR).expect("write");
        assert_eq!(read_market_data_floor(&path), FloorRead::Floor(FLOOR));
        // A far-future floor against the real clock: refused, and the boot
        // load installs nothing.
        write_market_data_floor(&path, wall_clock_unix_nanos() + 3_600 * 1_000_000_000)
            .expect("write");
        assert_eq!(
            read_market_data_floor(&path),
            FloorRead::Refused("ahead_of_wall_clock")
        );
        assert_eq!(load_market_data_floor_at_boot(&path), None);
        let _cleanup = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_ensure_market_data_floor_loaded_runs_once() {
        // In the test binary's working directory there is no floor file, so
        // this installs nothing; the second call is a no-op.
        ensure_market_data_floor_loaded();
        ensure_market_data_floor_loaded();
        assert!(BOOT_LOAD.is_completed());
    }
}
