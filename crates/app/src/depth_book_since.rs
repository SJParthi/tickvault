//! When depth array rows began (plan item 49e step 3).
//!
//! With `[depth_storage] array_rows = true` the depth writer sends a frame to
//! `market_depth_book` only when the frame was received at or after the
//! instant stored here; an older frame keeps going to `market_depth`. The
//! instant is recorded once, at the first boot that has the setting on, so a
//! replay of frames an earlier process captured (the boot WAL catch-up, the
//! after-close depth pass) writes them into the table their first copy went
//! to, never into both.
//!
//! The file holds one line, `since_received_utc_nanos=<n>`. With the setting
//! off the file is removed, so turning it on again starts a new period.
//!
//! Cold path: one small read, and at most one write, per lane start.
//!
//! **Honest limits.** (a) If the write fails the instant still applies to
//! this process, and the next boot records a later one, so frames from this
//! process replayed after that go to `market_depth` (both copies exist then,
//! one per table; nothing is lost). (b) A wall clock stepped back across the
//! first-on boot can send a few of an earlier process's REPLAYED frames to
//! the new table. A frame this process takes off a socket always goes to the
//! new table, whatever the clock reads, and a stored instant later than the
//! boot is re-recorded as the boot's own. (c) After the setting is turned
//! off and on again, frames from the first period that are replayed later
//! go to `market_depth`. (d) Turning the setting OFF removes the instant, so
//! frames from the on-period replayed afterwards (the boot catch-up, the
//! after-close pass) go to `market_depth` while their siblings sit in
//! `market_depth_book`: nothing is lost, but one minute can then be split
//! across the two tables.

use std::io::{Read, Write};
use std::path::Path;

/// Where the instant lives, relative to the app working directory like the
/// sibling `data/state` files.
pub const DEPTH_BOOK_SINCE_PATH: &str = "data/state/depth_book_since";

/// The line prefix.
const SINCE_PREFIX: &str = "since_received_utc_nanos=";

/// The most bytes the reader looks at; a real file is about 45.
const SINCE_READ_CAP_BYTES: u64 = 256;

/// Resolves the array-row start instant for this lane, or `None` when array
/// rows are off. `now_utc_nanos` is the lane's start, before any socket dials.
#[must_use]
pub fn resolve_depth_book_since(array_rows: bool, now_utc_nanos: i64) -> Option<i64> {
    tickvault_storage::off_worker::off_worker(|| {
        resolve_in(Path::new(DEPTH_BOOK_SINCE_PATH), array_rows, now_utc_nanos)
    })
}

fn resolve_in(path: &Path, array_rows: bool, now_utc_nanos: i64) -> Option<i64> {
    if !array_rows {
        if path.exists() {
            match std::fs::remove_file(path) {
                Ok(()) => tracing::info!(
                    path = %path.display(),
                    "depth array rows are off — the start instant was removed, so depth \
                     goes to market_depth"
                ),
                Err(err) => tracing::warn!(
                    path = %path.display(),
                    %err,
                    "depth array rows are off but the old start instant could not be \
                     removed — depth still goes to market_depth; a later turn-on would \
                     reuse the old instant"
                ),
            }
        }
        return None;
    }
    if let Some(since) = read_since(path) {
        if since <= now_utc_nanos {
            tracing::info!(
                since,
                "depth array rows on — frames received since then go to market_depth_book"
            );
            return Some(since);
        }
        // An instant later than this boot (a clock that ran ahead when it was
        // saved, or a damaged file) would send every replayed frame up to it
        // to market_depth. Re-recorded as this boot's instant, below; live
        // frames go to market_depth_book whatever this says.
        tracing::warn!(
            path = %path.display(),
            stored = since,
            now = now_utc_nanos,
            "depth array-row start instant is later than this boot's clock — it is \
             re-recorded as this boot's instant"
        );
    }
    if let Err(err) = write_since(path, now_utc_nanos) {
        tracing::warn!(
            path = %path.display(),
            %err,
            since = now_utc_nanos,
            "depth array rows on but the start instant could not be saved — it applies \
             to this process only; a later boot records a later one"
        );
    } else {
        tracing::info!(
            since = now_utc_nanos,
            "depth array rows turned on — frames received from now go to market_depth_book"
        );
    }
    Some(now_utc_nanos)
}

/// The stored instant, or `None` when the file is missing, unreadable or not
/// exactly one valid line with a positive value.
fn read_since(path: &Path) -> Option<i64> {
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::with_capacity(64);
    file.take(SINCE_READ_CAP_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let value = text.trim_end_matches('\n').strip_prefix(SINCE_PREFIX)?;
    let since = value.parse::<i64>().ok()?;
    (since > 0).then_some(since)
}

/// Temp file, `sync_all`, rename, folder sync.
fn write_since(path: &Path, since: i64) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(format!("{SINCE_PREFIX}{since}\n").as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-depth-book-since-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        dir.join("state").join("depth_book_since")
    }

    #[test]
    fn test_resolve_depth_book_since_is_none_when_array_rows_are_off() {
        let path = temp_path("off");
        assert_eq!(resolve_in(&path, false, 1_000), None);
        assert!(!path.exists(), "off must never create the file");
    }

    #[test]
    fn test_resolve_records_the_first_instant_once_and_keeps_it() {
        let path = temp_path("first");
        assert_eq!(resolve_in(&path, true, 1_000), Some(1_000));
        // A later boot keeps the first instant, so frames between the two
        // boots still route to the book table.
        assert_eq!(resolve_in(&path, true, 9_000), Some(1_000));
        assert_eq!(read_since(&path), Some(1_000));
    }

    #[test]
    fn test_turning_off_removes_the_instant_so_a_new_period_starts() {
        let path = temp_path("cycle");
        assert_eq!(resolve_in(&path, true, 1_000), Some(1_000));
        assert_eq!(resolve_in(&path, false, 2_000), None);
        assert!(!path.exists());
        assert_eq!(resolve_in(&path, true, 3_000), Some(3_000));
    }

    #[test]
    fn test_a_damaged_file_is_replaced_by_this_boots_instant() {
        let path = temp_path("damaged");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for junk in [
            "",
            "garbage",
            "since_received_utc_nanos=",
            "since_received_utc_nanos=-5",
            "since_received_utc_nanos=0",
        ] {
            std::fs::write(&path, junk).unwrap();
            assert_eq!(read_since(&path), None, "{junk:?}");
            assert_eq!(resolve_in(&path, true, 4_000), Some(4_000), "{junk:?}");
            assert_eq!(read_since(&path), Some(4_000));
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn test_an_unwritable_folder_still_applies_the_instant_to_this_process() {
        // A file where the folder should be: create_dir_all fails.
        let base = temp_path("blocked");
        let blocker = base.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        std::fs::write(&blocker, b"not a folder").unwrap();
        assert_eq!(resolve_in(&base, true, 5_000), Some(5_000));
    }

    #[test]
    fn test_resolve_depth_book_since_off_touches_nothing() {
        // The production entry with the setting off never writes. It works on
        // the relative production path, so it runs only where no such file
        // exists (it would remove one) and checks none appears.
        let real = Path::new(DEPTH_BOOK_SINCE_PATH);
        if real.exists() {
            return;
        }
        assert_eq!(resolve_depth_book_since(false, 1), None);
        assert!(!real.exists(), "off must never create the file");
    }

    #[test]
    fn test_a_stored_instant_later_than_this_boot_is_re_recorded_as_now() {
        let path = temp_path("future");
        assert_eq!(resolve_in(&path, true, 9_000), Some(9_000));
        // A later boot whose clock reads EARLIER than the stored instant.
        assert_eq!(resolve_in(&path, true, 4_000), Some(4_000));
        assert_eq!(read_since(&path), Some(4_000), "re-recorded");
        // An instant equal to the boot is kept.
        assert_eq!(resolve_in(&path, true, 4_000), Some(4_000));
        std::fs::write(&path, format!("{SINCE_PREFIX}{}\n", i64::MAX)).unwrap();
        assert_eq!(resolve_in(&path, true, 5_000), Some(5_000));
    }
}
