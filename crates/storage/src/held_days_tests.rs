#![cfg(test)]
//! Tests for [`super`] (daily-universe Quote 63 held days).

use super::*;

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date")
}

/// A fresh, empty scratch folder for one test.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "tv-held-days-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }

    fn held(&self, configured: &[&str]) -> HeldDays {
        HeldDays::with_dirs(
            configured.iter().map(|s| d(s)).collect(),
            self.0.join("held-days"),
            self.0.join("day-repair"),
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn test_encode_day_marker_and_decode_day_marker_round_trip() {
    let text = encode_day_marker(
        MarkerKind::Done,
        d("2026-11-20"),
        d("2026-11-24"),
        "verified",
    );
    assert!(text.starts_with("tickvault-day-marker v1\nkind=done\nday=2026-11-20\n"));
    assert_eq!(
        decode_day_marker(MarkerKind::Done, d("2026-11-20"), &text),
        MarkerRead::Valid {
            written_on: d("2026-11-24")
        }
    );
    // A control character in the reason never breaks the line layout.
    let text = encode_day_marker(
        MarkerKind::Hold,
        d("2026-11-23"),
        d("2026-11-23"),
        "a\nb\rc",
    );
    assert_eq!(
        decode_day_marker(MarkerKind::Hold, d("2026-11-23"), &text),
        MarkerRead::Valid {
            written_on: d("2026-11-23")
        }
    );
}

#[test]
fn test_decode_day_marker_refuses_crc_torn_wrong_day_kind_and_order() {
    let day = d("2026-11-20");
    let good = encode_day_marker(MarkerKind::Done, day, d("2026-11-24"), "ok");
    // A flipped byte fails the CRC.
    let flipped = good.replace("reason=ok", "reason=ox");
    assert_eq!(
        decode_day_marker(MarkerKind::Done, day, &flipped),
        MarkerRead::Damaged
    );
    // Every truncation (a torn write) is damaged, never valid.
    for cut in 0..good.len() {
        assert_eq!(
            decode_day_marker(MarkerKind::Done, day, &good[..cut]),
            MarkerRead::Damaged,
            "cut at {cut}"
        );
    }
    // Another day or another kind.
    assert_eq!(
        decode_day_marker(MarkerKind::Done, d("2026-11-21"), &good),
        MarkerRead::Damaged
    );
    assert_eq!(
        decode_day_marker(MarkerKind::Hold, day, &good),
        MarkerRead::Damaged
    );
    // A `.done` dated before its own day is not believed.
    let early = encode_day_marker(MarkerKind::Done, day, d("2026-11-19"), "ok");
    assert_eq!(
        decode_day_marker(MarkerKind::Done, day, &early),
        MarkerRead::Damaged
    );
    // Bytes after the CRC line.
    assert_eq!(
        decode_day_marker(MarkerKind::Done, day, &format!("{good}x\n")),
        MarkerRead::Damaged
    );
}

#[test]
fn test_read_day_marker_and_write_day_marker_and_day_marker_path() {
    let s = Scratch::new("rw");
    let dir = s.0.join("held-days");
    let day = d("2026-11-20");
    // A missing folder and a missing file are Missing.
    assert_eq!(
        read_day_marker(&dir, MarkerKind::Hold, day),
        MarkerRead::Missing
    );
    write_day_marker(&dir, MarkerKind::Hold, day, day, "feed time page").expect("write");
    assert_eq!(
        day_marker_path(&dir, MarkerKind::Hold, day),
        dir.join("2026-11-20.hold")
    );
    assert_eq!(
        read_day_marker(&dir, MarkerKind::Hold, day),
        MarkerRead::Valid { written_on: day }
    );
    // No temporary file is left behind.
    let names: Vec<String> = std::fs::read_dir(&dir)
        .expect("list")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["2026-11-20.hold".to_string()]);
    // A torn file (half written by hand) is Damaged.
    let path = day_marker_path(&dir, MarkerKind::Hold, day);
    let text = std::fs::read_to_string(&path).expect("read");
    std::fs::write(&path, &text[..text.len() / 2]).expect("tear");
    assert_eq!(
        read_day_marker(&dir, MarkerKind::Hold, day),
        MarkerRead::Damaged
    );
    // An oversize file is Damaged.
    std::fs::write(&path, vec![b'a'; 5000]).expect("big");
    assert_eq!(
        read_day_marker(&dir, MarkerKind::Hold, day),
        MarkerRead::Damaged
    );
    // A folder where the file should be is Damaged (unreadable), not Missing.
    std::fs::remove_file(&path).expect("rm");
    std::fs::create_dir_all(&path).expect("dir in place of file");
    assert_eq!(
        read_day_marker(&dir, MarkerKind::Hold, day),
        MarkerRead::Damaged
    );
}

#[test]
fn test_verdict_and_is_held_from_config_hold_file_and_damaged_files() {
    let s = Scratch::new("verdict");
    let day = d("2026-11-20");
    let other = d("2026-11-23");
    let held = s.held(&["2026-11-20"]);
    // Config holds; no folder at all.
    assert_eq!(
        held.verdict(day),
        HeldVerdict::Held {
            source: HeldSource::Config,
            done_damaged: false
        }
    );
    assert!(held.is_held(day));
    assert!(!held.is_held(other));
    assert_eq!(held.verdict(other), HeldVerdict::NotHeld);
    // A `.hold` file holds a day the config does not list.
    write_day_marker(held.hold_dir(), MarkerKind::Hold, other, other, "page").expect("w");
    assert_eq!(
        held.verdict(other),
        HeldVerdict::Held {
            source: HeldSource::HoldFile,
            done_damaged: false
        }
    );
    // A damaged `.hold` file still holds (fails toward keeping).
    std::fs::write(
        day_marker_path(held.hold_dir(), MarkerKind::Hold, other),
        "torn",
    )
    .expect("tear");
    assert_eq!(
        held.verdict(other),
        HeldVerdict::Held {
            source: HeldSource::DamagedHoldFile,
            done_damaged: false
        }
    );
    // A damaged `.done` releases nothing.
    std::fs::create_dir_all(held.done_dir()).expect("done dir");
    std::fs::write(
        day_marker_path(held.done_dir(), MarkerKind::Done, day),
        "torn",
    )
    .expect("torn done");
    assert_eq!(
        held.verdict(day),
        HeldVerdict::Held {
            source: HeldSource::Config,
            done_damaged: true
        }
    );
}

#[test]
fn test_regression_done_marker_releases_hold() {
    let s = Scratch::new("release");
    let day = d("2026-11-20");
    let held = s.held(&["2026-11-20"]);
    assert!(held.is_held(day));
    write_day_marker(
        held.done_dir(),
        MarkerKind::Done,
        day,
        d("2026-11-25"),
        "verify passed",
    )
    .expect("done");
    assert_eq!(
        held.verdict(day),
        HeldVerdict::Released {
            done_on: d("2026-11-25")
        }
    );
    assert!(!held.is_held(day));
    assert!(!held.holds_partition("tick_quotes", "2026-11-20T10"));
    // A `.done` releases an auto-held day the same way.
    let auto = d("2026-11-23");
    write_day_marker(held.hold_dir(), MarkerKind::Hold, auto, auto, "page").expect("hold");
    assert!(held.is_held(auto));
    write_day_marker(
        held.done_dir(),
        MarkerKind::Done,
        auto,
        d("2026-11-26"),
        "ok",
    )
    .expect("done");
    assert!(!held.is_held(auto));
    // Removing the config entry releases a day with no `.hold` file.
    let unlisted = s.held(&[]);
    std::fs::remove_file(day_marker_path(held.done_dir(), MarkerKind::Done, day)).expect("rm");
    assert!(held.is_held(day));
    assert!(!unlisted.is_held(day));
}

#[test]
fn test_holds_partition_and_table_is_held_kind_and_partition_day() {
    let s = Scratch::new("tables");
    let held = s.held(&["2026-11-20"]);
    for table in [
        "ticks",
        "tick_quotes",
        "feed_aux_packets",
        "candles_1s",
        "candles_1m",
    ] {
        assert!(table_is_held_kind(table), "{table}");
        assert!(held.holds_partition(table, "2026-11-20T09"), "{table}");
        assert!(held.holds_partition(table, "2026-11-20"), "{table}");
        assert!(!held.holds_partition(table, "2026-11-19T09"), "{table}");
    }
    for table in [
        "market_depth",
        "market_depth_book",
        "cross_verify_1m_audit",
        "partition_archive_audit",
    ] {
        assert!(!table_is_held_kind(table), "{table}");
        assert!(!held.holds_partition(table, "2026-11-20T09"), "{table}");
    }
    assert_eq!(partition_day("2026-11-20T09"), Some(d("2026-11-20")));
    assert_eq!(partition_day("2026-11-20"), Some(d("2026-11-20")));
    assert_eq!(partition_day("2026-11"), None);
    assert_eq!(partition_day("2026-11-20x"), None);
    assert_eq!(partition_day("garbage-xx"), None);
    assert!(!held.holds_partition("ticks", "nonsense"));
}

#[test]
fn test_released_days_lists_only_valid_done_files_oldest_first() {
    let s = Scratch::new("released");
    let held = s.held(&[]);
    // Missing folder: nothing released.
    assert!(held.released_days().is_empty());
    write_day_marker(
        held.done_dir(),
        MarkerKind::Done,
        d("2026-11-20"),
        d("2026-11-25"),
        "ok",
    )
    .expect("w");
    write_day_marker(
        held.done_dir(),
        MarkerKind::Done,
        d("2026-11-18"),
        d("2026-11-19"),
        "ok",
    )
    .expect("w");
    std::fs::write(held.done_dir().join("2026-11-21.done"), "torn").expect("torn");
    std::fs::write(held.done_dir().join("notes.txt"), "x").expect("junk");
    assert_eq!(
        held.released_days(),
        vec![
            (d("2026-11-18"), d("2026-11-19")),
            (d("2026-11-20"), d("2026-11-25"))
        ]
    );
}

#[test]
fn test_parse_held_days_and_from_config_and_with_dirs_hold_dir_done_dir() {
    assert_eq!(
        parse_held_days(&[
            "2026-11-20".to_string(),
            " 2026-11-23 ".to_string(),
            "not-a-date".to_string()
        ]),
        vec![d("2026-11-20"), d("2026-11-23")]
    );
    let cfg = PartitionRetentionConfig {
        held_days: vec!["2026-11-20".to_string()],
        ..PartitionRetentionConfig::default()
    };
    let held = HeldDays::from_config(&cfg);
    assert_eq!(held.hold_dir(), Path::new(HELD_DAYS_DIR));
    assert_eq!(held.done_dir(), Path::new(DAY_REPAIR_DIR));
    assert_eq!(
        held,
        HeldDays::with_dirs(
            vec![d("2026-11-20")],
            PathBuf::from(HELD_DAYS_DIR),
            PathBuf::from(DAY_REPAIR_DIR)
        )
    );
    assert_eq!(
        HeldDays::default().verdict(d("1999-01-01")),
        HeldVerdict::NotHeld
    );
    assert_eq!(MarkerKind::Hold.as_str(), "hold");
    assert_eq!(MarkerKind::Done.as_str(), "done");
}

#[test]
fn test_install_process_held_days_feeds_process_held_days() {
    let cfg = PartitionRetentionConfig {
        held_days: vec!["2026-11-20".to_string()],
        ..PartitionRetentionConfig::default()
    };
    install_process_held_days(&cfg);
    // A second install never replaces the first (one config per process).
    install_process_held_days(&PartitionRetentionConfig::default());
    let held = process_held_days();
    assert_eq!(held.hold_dir(), Path::new(HELD_DAYS_DIR));
    assert_eq!(
        held,
        HeldDays::with_dirs(
            vec![d("2026-11-20")],
            PathBuf::from(HELD_DAYS_DIR),
            PathBuf::from(DAY_REPAIR_DIR)
        )
    );
}
