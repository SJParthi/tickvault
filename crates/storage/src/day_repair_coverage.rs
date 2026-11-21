//! The packet coverage check of the one-off 2026-11-20 day repair (daily-
//! universe Quote 63, plan item UTC-REPAIR; added 2026-11-21 after the first
//! verify).
//!
//! The first verify compared row COUNTS: the rebuild wrote 58,477,292
//! `ticks` + `tick_quotes` + `feed_aux_packets` rows where the repair dropped
//! 59,303,664. A count cannot say whether a packet is missing, because two
//! things move counts legitimately: a replay run with the corrected time
//! writes a second row for a packet whose route changed (counted twice in the
//! dropped copies), and the exact-repeat skip of the `no_trade_today` /
//! `connect_snapshot` kind (Quote 61) depends on the live run's gap history.
//!
//! This module reads the dropped copies' `capture_seq` (one per packet, part
//! of every DEDUP key of the three tables) so the repair can check, packet by
//! packet, that every dropped packet is in the rebuilt day. Rows of the two
//! repeat-skip kinds are counted apart (the skip may leave them out by rule).
//!
//! O(bytes) to read a copy, O(n log n) to sort the numbers; cold, offline,
//! once per verify. Deleted with the rest of the one-off tool.

use std::path::Path;

use anyhow::{Context as _, Result, bail};

/// Aux kinds the exact-repeat skip (Quote 61) may leave out of a run.
pub const REPEAT_SKIP_KINDS: [&str; 2] = ["no_trade_today", "connect_snapshot"];

/// The fields of one CSV line as QuestDB's `/exp` writes it: comma-separated,
/// a field holding a comma or quote is quoted, a quote inside doubled. Pure,
/// O(line).
#[must_use]
pub fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            '\r' | '\n' if !quoted => {}
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// The index of a column in a CSV header line, or `None`. Pure.
#[must_use]
pub fn column_index(header: &str, name: &str) -> Option<usize> {
    csv_fields(header).iter().position(|c| c == name)
}

/// What one dropped copy held, by packet.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CopySeqs {
    /// `capture_seq` of every row that must be in the rebuilt day.
    pub checked: Vec<u64>,
    /// `capture_seq` of every repeat-skip-kind row (may be absent by rule).
    pub repeat_kind: Vec<u64>,
    /// Data rows read.
    pub rows: u64,
    /// `security_id|segment` of every contract with a repeat-skip-kind row:
    /// the rebuild must keep at least one such row per contract (a bound on
    /// the exemption; 2026-11-21 review).
    pub repeat_contracts: std::collections::BTreeSet<String>,
}

/// Where the needed columns sit in one copy's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Columns {
    /// `capture_seq`.
    pub seq: usize,
    /// `kind` (the aux table only).
    pub kind: Option<usize>,
    /// `security_id`.
    pub sid: Option<usize>,
    /// `segment`.
    pub seg: Option<usize>,
}

impl Columns {
    /// Reads the indexes from a header line; `None` without `capture_seq`.
    /// Pure.
    #[must_use]
    pub fn from_header(header: &str) -> Option<Self> {
        Some(Self {
            seq: column_index(header, "capture_seq")?,
            kind: column_index(header, "kind"),
            sid: column_index(header, "security_id"),
            seg: column_index(header, "segment"),
        })
    }
}

/// Adds one data line to `seqs`. `kind_idx` is the `kind` column (aux table
/// only). A line whose `capture_seq` is not a number is an error (the check
/// cannot judge it). Pure.
///
/// # Errors
/// A missing or non-numeric `capture_seq`.
pub fn add_line(seqs: &mut CopySeqs, line: &str, cols: Columns) -> Result<()> {
    let (seq_idx, kind_idx) = (cols.seq, cols.kind);
    if line.trim().is_empty() {
        return Ok(());
    }
    let f = csv_fields(line);
    let raw = f
        .get(seq_idx)
        .map(|s| s.trim())
        .context("a row has no capture_seq field")?;
    let seq: u64 = raw
        .parse()
        .with_context(|| format!("capture_seq {raw:?} is not a number"))?;
    seqs.rows = seqs.rows.saturating_add(1);
    let repeat = kind_idx
        .and_then(|k| f.get(k))
        .is_some_and(|k| REPEAT_SKIP_KINDS.contains(&k.trim()));
    if repeat {
        seqs.repeat_kind.push(seq);
        let field = |i: Option<usize>| i.and_then(|i| f.get(i)).map(|s| s.trim().to_owned());
        match (field(cols.sid), field(cols.seg)) {
            (Some(sid), Some(seg)) if !sid.is_empty() => {
                seqs.repeat_contracts.insert(format!("{sid}|{seg}"));
            }
            _ => bail!("a repeat-kind row has no security_id or segment"),
        }
    } else {
        seqs.checked.push(seq);
    }
    Ok(())
}

/// Reads one gzipped CSV copy (QuestDB `/exp` shape: a header line, then one
/// row per line) into `seqs`. O(bytes).
///
/// # Errors
/// The file does not read or unzip, the header has no `capture_seq`, or a
/// row's `capture_seq` is not a number.
pub fn read_copy_gz(path: &Path, seqs: &mut CopySeqs) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let dec = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(file));
    read_copy_lines(std::io::BufReader::with_capacity(1 << 20, dec), seqs)
        .with_context(|| format!("read {}", path.display()))
}

/// [`read_copy_gz`] over any line source (the unzipped stream). O(bytes).
///
/// # Errors
/// As [`read_copy_gz`].
pub fn read_copy_lines<R: std::io::BufRead>(mut reader: R, seqs: &mut CopySeqs) -> Result<()> {
    let mut header = String::new();
    if reader.read_line(&mut header).context("read the header")? == 0 {
        return Ok(());
    }
    let Some(cols) = Columns::from_header(&header) else {
        bail!("the header has no capture_seq column: {}", header.trim());
    };
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).context("read a row")? == 0 {
            return Ok(());
        }
        add_line(seqs, &line, cols)?;
    }
}

/// Sorts and dedups `v` in place. O(n log n).
pub fn sort_dedup(v: &mut Vec<u64>) {
    v.sort_unstable();
    v.dedup();
}

/// How many values of sorted, deduped `need` are absent from sorted, deduped
/// `have`, plus up to `sample` of them. O(need + have).
#[must_use]
pub fn missing_from(need: &[u64], have: &[u64], sample: usize) -> (u64, Vec<u64>) {
    let (mut i, mut j) = (0usize, 0usize);
    let mut missing = 0u64;
    let mut first = Vec::new();
    while i < need.len() {
        match have.get(j) {
            Some(h) if *h < need[i] => j += 1,
            Some(h) if *h == need[i] => {
                i += 1;
                j += 1;
            }
            _ => {
                missing += 1;
                if first.len() < sample {
                    first.push(need[i]);
                }
                i += 1;
            }
        }
    }
    (missing, first)
}

/// Downloads one cold-bucket object to `path` (a new file). One GetObject.
///
/// # Errors
/// The download failed.
// TEST-EXEMPT: one AWS SDK call through S3Cold::get_to_path.
pub async fn download_copy(s3: &crate::s3_cold::S3Cold, key: &str, path: &Path) -> Result<u64> {
    s3.get_to_path(key, path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn test_csv_fields_and_column_index_handle_quotes() {
        assert_eq!(csv_fields("a,b,c\n"), vec!["a", "b", "c"]);
        assert_eq!(
            csv_fields("\"x,y\",2,\"say \"\"hi\"\"\""),
            vec!["x,y", "2", "say \"hi\""]
        );
        assert_eq!(csv_fields(""), vec![""]);
        assert_eq!(
            column_index("\"ts\",\"kind\",\"capture_seq\"\r\n", "capture_seq"),
            Some(2)
        );
        assert_eq!(column_index("ts,kind", "capture_seq"), None);
    }

    #[test]
    fn test_add_line_splits_repeat_kinds_tracks_their_contracts_and_refuses_a_bad_seq() {
        // ts,kind,capture_seq,security_id,segment
        let cols = Columns::from_header("ts,kind,capture_seq,security_id,segment").expect("cols");
        let mut s = CopySeqs::default();
        add_line(&mut s, "t,oi,10,1,NSE_FNO\n", cols).expect("oi");
        add_line(&mut s, "t,no_trade_today,11,2,NSE_FNO\n", cols).expect("ntt");
        add_line(&mut s, "t,connect_snapshot,12,2,NSE_FNO\n", cols).expect("cs");
        add_line(&mut s, "t,no_trade_today,14,3,NSE_EQ\n", cols).expect("ntt2");
        add_line(&mut s, "t,out_of_window_tick,13,1,IDX_I\n", cols).expect("oow");
        add_line(&mut s, "   \n", cols).expect("blank");
        assert_eq!(s.checked, vec![10, 13]);
        assert_eq!(s.repeat_kind, vec![11, 12, 14]);
        assert_eq!(s.rows, 5);
        let contracts: Vec<&str> = s.repeat_contracts.iter().map(String::as_str).collect();
        assert_eq!(contracts, vec!["2|NSE_FNO", "3|NSE_EQ"]);
        assert!(add_line(&mut s, "t,oi,,1,NSE_FNO\n", cols).is_err());
        assert!(add_line(&mut s, "t,oi,x,1,NSE_FNO\n", cols).is_err());
        assert!(add_line(&mut s, "t\n", cols).is_err());
        // A repeat-kind row with no contract cannot be bounded: refused.
        assert!(add_line(&mut s, "t,no_trade_today,15,,NSE_FNO\n", cols).is_err());
        // A table with no kind column checks every row.
        let q_cols = Columns::from_header("ts,capture_seq").expect("cols");
        let mut q = CopySeqs::default();
        add_line(&mut q, "t,5\n", q_cols).expect("tick");
        assert_eq!(q.checked, vec![5]);
        assert_eq!(Columns::from_header("ts,kind"), None);
    }

    #[test]
    fn test_read_copy_gz_read_copy_lines_and_from_header_read_a_quarantine_shaped_file() {
        let dir = std::env::temp_dir().join(format!(
            "tv-coverage-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("feed_aux_packets.csv.gz");
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).expect("file"),
            flate2::Compression::fast(),
        );
        gz.write_all(
            b"\"ts\",\"segment\",\"kind\",\"contract\",\"capture_seq\",\"security_id\"\n\
              2026-11-20T09:00:00.000000Z,NSE_FNO,oi,\"A, B\",7,101\n\
              2026-11-20T09:00:00.000000Z,NSE_FNO,no_trade_today,X,8,102\n\
              2026-11-20T09:00:01.000000Z,IDX_I,out_of_window_tick,,9,13\n",
        )
        .expect("write");
        gz.finish().expect("finish");
        let mut s = CopySeqs::default();
        read_copy_gz(&path, &mut s).expect("read");
        assert_eq!(
            (s.checked.clone(), s.repeat_kind.clone(), s.rows),
            (vec![7, 9], vec![8], 3)
        );
        // An empty file reads as nothing; a header without capture_seq refuses.
        let mut e = CopySeqs::default();
        read_copy_lines(std::io::Cursor::new(""), &mut e).expect("empty");
        assert_eq!(e.rows, 0);
        assert!(read_copy_lines(std::io::Cursor::new("ts,kind\n1,oi\n"), &mut e).is_err());
        assert!(read_copy_gz(&dir.join("absent.gz"), &mut e).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_sort_dedup_and_missing_from_count_and_sample() {
        let mut need = vec![5, 1, 3, 3, 9, 7];
        sort_dedup(&mut need);
        assert_eq!(need, vec![1, 3, 5, 7, 9]);
        let have = vec![1, 2, 3, 7, 10];
        assert_eq!(missing_from(&need, &have, 1), (2, vec![5]));
        assert_eq!(missing_from(&need, &need, 3), (0, vec![]));
        assert_eq!(missing_from(&need, &[], 10), (5, vec![1, 3, 5, 7, 9]));
        assert_eq!(missing_from(&[], &have, 10), (0, vec![]));
    }
}
