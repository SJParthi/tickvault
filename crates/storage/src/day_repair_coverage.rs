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

/// The S3 key prefix every quarantine copy of one partition starts with: the
/// canonical `<partition>.csv.gz` and any content-addressed
/// `<partition>.sha-<hex>.csv.gz` (a drop-time re-export with other bytes).
/// The trailing dot keeps `T09` from matching `T09x`. Pure.
#[must_use]
pub fn partition_copy_prefix(table: &str, partition: &str) -> String {
    format!(
        "{}/partitions/{table}/{partition}.",
        crate::partition_archive::quarantine_copy::QUARANTINE_PREFIX
    )
}

/// The keys among `listed` that are a copy of the partition `prefix` names:
/// exactly `<prefix>csv.gz` or `<prefix>sha-<hex>.csv.gz`. Sorted, deduped.
/// Pure, O(listed log listed).
#[must_use]
pub fn copies_under_prefix(listed: &[String], prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = listed
        .iter()
        .filter(|k| {
            let Some(rest) = k.strip_prefix(prefix) else {
                return false;
            };
            rest == "csv.gz"
                || rest
                    .strip_prefix("sha-")
                    .and_then(|r| r.strip_suffix(".csv.gz"))
                    .is_some_and(|hex| {
                        !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())
                    })
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The S3 prefix every quarantine copy of `table` starts with. Pure.
#[must_use]
pub fn table_copy_prefix(table: &str) -> String {
    format!(
        "{}/partitions/{table}/",
        crate::partition_archive::quarantine_copy::QUARANTINE_PREFIX
    )
}

/// The partition a quarantine copy key under [`table_copy_prefix`] belongs
/// to (`<partition>.csv.gz` or `<partition>.sha-<hex>.csv.gz`), or `None`
/// for a key of another shape. Pure.
#[must_use]
pub fn partition_of_copy_key(table: &str, key: &str) -> Option<String> {
    let rest = key.strip_prefix(&table_copy_prefix(table))?;
    let (partition, _) = rest.split_once('.')?;
    let prefix = partition_copy_prefix(table, partition);
    (!partition.is_empty() && !copies_under_prefix(&[key.to_owned()], &prefix).is_empty())
        .then(|| partition.to_owned())
}

/// Every key under `prefix` in the cold bucket (ListObjectsV2, all pages).
///
/// # Errors
/// The listing failed.
// TEST-EXEMPT: one paginated AWS SDK call through S3Cold::list_keys; the filter over it is copies_under_prefix.
pub async fn list_copy_keys(s3: &crate::s3_cold::S3Cold, prefix: &str) -> Result<Vec<String>> {
    Ok(s3
        .list_keys(prefix)
        .await?
        .into_iter()
        .map(|(k, _)| k)
        .collect())
}

/// The stored value columns of a `ticks` / `tick_quotes` row, in key order
/// (the two tables share them). The exchange time is apart.
pub const TICK_VALUE_COLS: [&str; 13] = [
    "ltp",
    "open",
    "high",
    "low",
    "close",
    "volume",
    "oi",
    "oi_day_high",
    "oi_day_low",
    "avg_price",
    "last_trade_qty",
    "total_buy_qty",
    "total_sell_qty",
];

/// The stored value columns of a `feed_aux_packets` row, in key order. The
/// exchange time is apart.
pub const AUX_VALUE_COLS: [&str; 13] = [
    "kind",
    "oi",
    "prev_close",
    "prev_oi",
    "ltp",
    "volume",
    "day_open",
    "day_high",
    "day_low",
    "day_close",
    "oi_day_high",
    "oi_day_low",
    "reason_code",
];

/// Which family of table a row comes from: the two tick tables share one
/// row shape, the aux table has its own. A packet is matched only within its
/// family (a cross-family pair is left unmatched, so the check refuses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowClass {
    /// `ticks` or `tick_quotes`.
    Tick,
    /// `feed_aux_packets`.
    Aux,
}

/// Everything two copies of one vendor packet share as stored: the table
/// family, the instrument, every value column, and the exchange time. Two
/// copies of one packet (the backup socket's copy, which the live drain and
/// the replay may keep differently) differ only in `capture_seq`,
/// `received_at` and, in `tick_quotes`, the arrival-time `ts`; those are the
/// only columns left out, with `feed`, `contract` and the never-written
/// `payload_hash` (2026-11-21 second review: a six-column key let a different
/// packet with the same price and volume stand in for a lost one).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContentKey {
    /// The table family.
    pub class: RowClass,
    /// `security_id`.
    pub sid: i64,
    /// `segment`.
    pub seg: String,
    /// The value columns in key order, each normalized: a number as its
    /// `f64` bits in hex (so `5.5` and `5.50` agree), NULL as empty, other
    /// text as written.
    pub values: Vec<String>,
    /// The exchange time in µs (`ts` in `ticks`, `exchange_timestamp` in
    /// `tick_quotes`, `exchange_ts` in the aux table).
    pub exch_micros: Option<i64>,
}

/// Where the [`ContentKey`] columns sit in one CSV header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentCols {
    /// The table family.
    pub class: RowClass,
    /// `capture_seq`.
    pub seq: usize,
    /// `security_id`.
    pub sid: usize,
    /// `segment`.
    pub seg: usize,
    /// The value columns, in key order (`None` = not in this header; a
    /// header must hold every one, see [`ContentCols::from_header`]).
    pub values: Vec<usize>,
    /// The exchange time column.
    pub exch: usize,
}

impl ContentCols {
    /// Reads the indexes from a header line. The family is the aux table
    /// when the header has `kind`. `None` when any key column is absent (a
    /// row whose value columns cannot all be compared is never matched).
    /// The exchange time is `exchange_ts` (aux), else `exchange_timestamp`
    /// (`tick_quotes`), else `ts` (`ticks`, whose `ts` is the exchange time).
    /// Pure.
    #[must_use]
    pub fn from_header(header: &str) -> Option<Self> {
        let names = csv_fields(header);
        let at = |n: &str| names.iter().position(|c| c == n);
        let (class, cols): (RowClass, &[&str]) = if at("kind").is_some() {
            (RowClass::Aux, &AUX_VALUE_COLS)
        } else {
            (RowClass::Tick, &TICK_VALUE_COLS)
        };
        let values = cols.iter().map(|c| at(c)).collect::<Option<Vec<_>>>()?;
        let exch = match class {
            RowClass::Aux => at("exchange_ts")?,
            RowClass::Tick => at("exchange_timestamp").or_else(|| at("ts"))?,
        };
        Some(Self {
            class,
            seq: at("capture_seq")?,
            sid: at("security_id")?,
            seg: at("segment")?,
            values,
            exch,
        })
    }
}

/// A QuestDB CSV timestamp (`2026-11-20T09:15:00.123456Z`) in µs, `None`
/// when empty. Pure.
///
/// # Errors
/// Text that is not an RFC 3339 time.
pub fn parse_ts_micros(text: &str) -> Result<Option<i64>> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let dt = chrono::DateTime::parse_from_rfc3339(t)
        .with_context(|| format!("{t:?} is not a timestamp"))?;
    Ok(Some(dt.timestamp_micros()))
}

/// One value normalized for comparison: a number as its `f64` bits in hex,
/// NULL as empty, other text as written. Pure.
#[must_use]
pub fn normalize_value(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return String::new();
    }
    match t.parse::<f64>() {
        Ok(v) => format!("{:016x}", v.to_bits()),
        Err(_) => t.to_owned(),
    }
}

/// One row's `capture_seq` and [`ContentKey`], its exchange time moved by
/// `shift_micros` (a zero or absent time is never moved). Pure.
///
/// # Errors
/// A `capture_seq`, `security_id` or exchange time that does not parse.
pub fn content_of(
    f: &[String],
    cols: &ContentCols,
    shift_micros: i64,
) -> Result<(u64, ContentKey)> {
    let get = |i: usize| f.get(i).map_or("", |s| s.trim());
    let seq: u64 = get(cols.seq)
        .parse()
        .with_context(|| format!("capture_seq {:?} is not a number", get(cols.seq)))?;
    let sid: i64 = get(cols.sid)
        .parse()
        .with_context(|| format!("security_id {:?} is not a number", get(cols.sid)))?;
    // A zero (epoch) time is a packet with no trade time: never shifted.
    let exch = parse_ts_micros(get(cols.exch))?.map(|t| {
        if t == 0 {
            0
        } else {
            t.saturating_add(shift_micros)
        }
    });
    Ok((
        seq,
        ContentKey {
            class: cols.class,
            sid,
            seg: get(cols.seg).to_owned(),
            values: cols
                .values
                .iter()
                .map(|i| normalize_value(get(*i)))
                .collect(),
            exch_micros: exch,
        },
    ))
}

/// The two keys a missing packet may be found under in the rebuilt day: its
/// exchange time corrected by the repair's shift, and as stored (a time the
/// rebuild's guard left alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingContent {
    /// The key with the corrected exchange time.
    pub shifted: ContentKey,
    /// The key with the exchange time as the copy holds it.
    pub as_stored: ContentKey,
}

/// Reads the rows of one copy whose `capture_seq` is in sorted `missing`
/// into `out` (one entry per packet, however many copies hold it). O(bytes ×
/// log missing).
///
/// # Errors
/// The header lacks a key column, or a missing row does not parse.
pub fn read_missing_contents<R: std::io::BufRead>(
    mut reader: R,
    missing: &[u64],
    shift_micros: i64,
    out: &mut std::collections::BTreeMap<u64, MissingContent>,
) -> Result<()> {
    let mut header = String::new();
    if reader.read_line(&mut header).context("read the header")? == 0 {
        return Ok(());
    }
    let Some(cols) = ContentCols::from_header(&header) else {
        bail!("the header lacks a content column: {}", header.trim());
    };
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).context("read a row")? == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let f = csv_fields(&line);
        let Some(seq) = f.get(cols.seq).and_then(|s| s.trim().parse::<u64>().ok()) else {
            bail!("a row has no numeric capture_seq");
        };
        if missing.binary_search(&seq).is_err() || out.contains_key(&seq) {
            continue;
        }
        let (_, shifted) = content_of(&f, &cols, shift_micros)?;
        let (_, as_stored) = content_of(&f, &cols, 0)?;
        out.insert(seq, MissingContent { shifted, as_stored });
    }
}

/// [`read_missing_contents`] over a gzipped copy. O(bytes × log missing).
///
/// # Errors
/// As [`read_missing_contents`], or the file does not read or unzip.
pub fn read_missing_contents_gz(
    path: &Path,
    missing: &[u64],
    shift_micros: i64,
    out: &mut std::collections::BTreeMap<u64, MissingContent>,
) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let dec = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(file));
    read_missing_contents(
        std::io::BufReader::with_capacity(1 << 20, dec),
        missing,
        shift_micros,
        out,
    )
    .with_context(|| format!("read {}", path.display()))
}

/// Counts one rebuilt row as a spare copy when its `capture_seq` is not a
/// dropped packet's (sorted `dropped`): such a row can only be a packet the
/// live run did not store under that number, e.g. the backup socket's copy of
/// a packet live stored from the other socket. Pure.
///
/// # Errors
/// A field that does not parse.
pub fn add_spare(
    f: &[String],
    cols: &ContentCols,
    dropped: &[u64],
    spare: &mut std::collections::BTreeMap<ContentKey, u64>,
) -> Result<()> {
    let (seq, key) = content_of(f, cols, 0)?;
    if dropped.binary_search(&seq).is_err() {
        *spare.entry(key).or_insert(0) += 1;
    }
    Ok(())
}

/// Matches each missing packet against the spare rebuilt rows, one spare
/// row per packet (a count, so two lost packets need two spare rows): first
/// by the corrected key, then by the stored one. Returns the packets matched
/// and the unmatched `capture_seq`. Pure, O(missing log spare).
#[must_use]
pub fn match_spares(
    missing: &std::collections::BTreeMap<u64, MissingContent>,
    spare: &mut std::collections::BTreeMap<ContentKey, u64>,
) -> (u64, Vec<u64>) {
    let mut matched = 0u64;
    let mut unmatched = Vec::new();
    let mut take = |k: &ContentKey| match spare.get_mut(k) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    };
    for (seq, m) in missing {
        if take(&m.shifted) || (m.as_stored != m.shifted && take(&m.as_stored)) {
            matched += 1;
        } else {
            unmatched.push(*seq);
        }
    }
    (matched, unmatched)
}

/// Downloads one cold-bucket object to `path` (a new file). One GetObject.
///
/// # Errors
/// The download failed.
// TEST-EXEMPT: one AWS SDK call through S3Cold::get_to_path.
pub async fn download_copy(s3: &crate::s3_cold::S3Cold, key: &str, path: &Path) -> Result<u64> {
    s3.get_to_path(key, path).await
}

/// The wire code of a stored `segment` symbol (`NSE_FNO` -> 2), or `None`
/// for a name no segment has. Pure, O(segments).
#[must_use]
pub fn segment_code_of(name: &str) -> Option<u8> {
    (0u8..=8).find(|&c| {
        tickvault_common::types::ExchangeSegment::from_byte(c).is_some_and(|s| s.as_str() == name)
    })
}

/// What [`read_kept_seqs_lines`] read from one copy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KeptRead {
    /// Data rows read.
    pub rows: u64,
    /// Rows whose contract passed the filter (their numbers were pushed).
    pub kept: u64,
}

/// The `capture_seq` of every row of one dropped copy whose
/// `(security_id, segment wire code)` passes `keep`, pushed onto `out`: the
/// numbers the live run WROTE for those contracts (2026-11-22, the second
/// verify: the rebuild judges the backup-socket copies by them). A row the
/// reader cannot judge (no number, no contract, an unknown segment) is an
/// error, so a partial set is never handed on. O(bytes).
///
/// # Errors
/// The header lacks `capture_seq`, `security_id` or `segment`, or a row's
/// fields do not parse.
pub fn read_kept_seqs_lines<R: std::io::BufRead>(
    mut reader: R,
    keep: &dyn Fn(u64, u8) -> bool,
    out: &mut Vec<u64>,
) -> Result<KeptRead> {
    let mut read = KeptRead::default();
    let mut header = String::new();
    if reader.read_line(&mut header).context("read the header")? == 0 {
        return Ok(read);
    }
    let cols = Columns::from_header(&header)
        .with_context(|| format!("the header has no capture_seq column: {}", header.trim()))?;
    let (Some(sid_idx), Some(seg_idx)) = (cols.sid, cols.seg) else {
        bail!(
            "the header has no security_id or segment column: {}",
            header.trim()
        );
    };
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).context("read a row")? == 0 {
            return Ok(read);
        }
        if line.trim().is_empty() {
            continue;
        }
        let f = csv_fields(&line);
        let field = |i: usize, what: &str| {
            f.get(i)
                .map(|s| s.trim())
                .with_context(|| format!("a row has no {what} field"))
        };
        let seq_text = field(cols.seq, "capture_seq")?;
        let seq: u64 = seq_text
            .parse()
            .with_context(|| format!("capture_seq {seq_text:?} is not a number"))?;
        let sid_text = field(sid_idx, "security_id")?;
        let sid: u64 = sid_text
            .parse()
            .with_context(|| format!("security_id {sid_text:?} is not a number"))?;
        let seg_text = field(seg_idx, "segment")?;
        let seg = segment_code_of(seg_text)
            .with_context(|| format!("segment {seg_text:?} is not a known segment"))?;
        read.rows = read.rows.saturating_add(1);
        if keep(sid, seg) {
            out.push(seq);
            read.kept = read.kept.saturating_add(1);
        }
    }
}

/// [`read_kept_seqs_lines`] over one gzipped copy on disk. O(bytes).
///
/// # Errors
/// As [`read_kept_seqs_lines`], or the file does not read or unzip.
pub fn read_kept_seqs_gz(
    path: &Path,
    keep: &dyn Fn(u64, u8) -> bool,
    out: &mut Vec<u64>,
) -> Result<KeptRead> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let dec = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(file));
    read_kept_seqs_lines(std::io::BufReader::with_capacity(1 << 20, dec), keep, out)
        .with_context(|| format!("read {}", path.display()))
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

    #[test]
    fn test_partition_copy_prefix_and_copies_under_prefix_take_only_that_partition() {
        let p = partition_copy_prefix("ticks", "2026-11-20T09");
        assert_eq!(
            p,
            "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09."
        );
        let listed: Vec<String> = [
            "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.csv.gz",
            "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.sha-00ff.csv.gz",
            "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.sha-.csv.gz",
            "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.sha-zz.csv.gz",
            "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.csv.gz.tmp",
            "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T10.csv.gz",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(
            copies_under_prefix(&listed, &p),
            vec![
                "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.csv.gz",
                "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.sha-00ff.csv.gz",
            ]
        );
        // A day partition never takes an hour partition's copies.
        let day = partition_copy_prefix("feed_aux_packets", "2026-11-20");
        let hour = vec![format!("{}T09.csv.gz", day.trim_end_matches('.'))];
        assert!(copies_under_prefix(&hour, &day).is_empty());
    }

    const TICK_HDR: &str = "\"ts\",\"security_id\",\"segment\",\"ltp\",\"open\",\"high\",\"low\",\"close\",\"volume\",\"oi\",\"oi_day_high\",\"oi_day_low\",\"avg_price\",\"last_trade_qty\",\"total_buy_qty\",\"total_sell_qty\",\"received_at\",\"capture_seq\"\n";
    const QUOTE_HDR: &str = "\"ts\",\"security_id\",\"segment\",\"ltp\",\"open\",\"high\",\"low\",\"close\",\"volume\",\"oi\",\"oi_day_high\",\"oi_day_low\",\"avg_price\",\"last_trade_qty\",\"total_buy_qty\",\"total_sell_qty\",\"received_at\",\"capture_seq\",\"exchange_timestamp\"\n";
    const AUX_HDR: &str = "ts,received_at,feed,segment,kind,contract,security_id,oi,prev_close,prev_oi,ltp,exchange_ts,volume,day_open,day_high,day_low,day_close,oi_day_high,oi_day_low,reason_code,capture_seq\n";

    /// One tick-family row: exchange time `t`, received `r`, `buy` total buy
    /// quantity.
    fn tick_row(t: &str, sid: i64, ltp: &str, vol: i64, buy: i64, r: &str, seq: u64) -> String {
        format!("{t},{sid},NSE_FNO,{ltp},1,2,0.5,1.5,{vol},,,,1.25,75,{buy},9,{r},{seq}\n")
    }

    #[test]
    fn test_table_copy_prefix_and_partition_of_copy_key() {
        let t = table_copy_prefix("ticks");
        assert_eq!(t, "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/");
        assert_eq!(
            partition_of_copy_key("ticks", &format!("{t}2026-11-20T09.csv.gz")),
            Some("2026-11-20T09".to_owned())
        );
        assert_eq!(
            partition_of_copy_key("ticks", &format!("{t}2026-11-20T09.sha-0aF.csv.gz")),
            Some("2026-11-20T09".to_owned())
        );
        assert_eq!(
            partition_of_copy_key("ticks", &format!("{t}2026-11-20T09.tmp")),
            None
        );
        assert_eq!(partition_of_copy_key("ticks", &format!("{t}.csv.gz")), None);
        assert_eq!(
            partition_of_copy_key("tick_quotes", &format!("{t}2026-11-20T09.csv.gz")),
            None
        );
    }

    #[test]
    fn test_content_cols_from_header_parse_ts_micros_normalize_value_and_content_of() {
        let tick = ContentCols::from_header(TICK_HDR).expect("ticks");
        assert_eq!(tick.class, RowClass::Tick);
        assert_eq!(tick.exch, 0, "ticks: ts is the exchange time");
        let quote = ContentCols::from_header(QUOTE_HDR).expect("quotes");
        assert_eq!(quote.exch, 18, "tick_quotes: exchange_timestamp");
        let aux = ContentCols::from_header(AUX_HDR).expect("aux");
        assert_eq!((aux.class, aux.exch), (RowClass::Aux, 11));
        // A header missing a value column cannot be compared.
        assert!(ContentCols::from_header("capture_seq,security_id,segment,ts").is_none());
        assert_eq!(parse_ts_micros("").expect("empty"), None);
        assert_eq!(
            parse_ts_micros("1970-01-01T00:00:01.000002Z").expect("ts"),
            Some(1_000_002)
        );
        assert!(parse_ts_micros("nope").is_err());
        assert_eq!(normalize_value("5.5"), normalize_value("5.50"));
        assert_eq!(normalize_value("7"), normalize_value("7.0"));
        assert_eq!(normalize_value(" "), "");
        assert_eq!(normalize_value("oi"), "oi");
        let row = csv_fields(&tick_row(
            "2026-11-20T04:00:00.000000Z",
            52_411,
            "101.5",
            9_000,
            40,
            "2026-11-20T09:30:01.000000Z",
            77,
        ));
        let (seq, k) = content_of(&row, &tick, 19_800_000_000).expect("row");
        assert_eq!((seq, k.sid, k.seg.as_str()), (77, 52_411, "NSE_FNO"));
        assert_eq!(k.values.len(), TICK_VALUE_COLS.len());
        assert_eq!(k.values[0], normalize_value("101.5"));
        assert_eq!(
            k.exch_micros,
            parse_ts_micros("2026-11-20T09:30:00.000000Z").expect("ts")
        );
        // A zero time is never shifted; an empty one stays NULL.
        let zero = csv_fields(&tick_row(
            "1970-01-01T00:00:00.000000Z",
            1,
            "1",
            1,
            1,
            "",
            2,
        ));
        assert_eq!(
            content_of(&zero, &tick, 5).expect("row").1.exch_micros,
            Some(0)
        );
        let none = csv_fields(&tick_row("", 1, "1", 1, 1, "", 2));
        assert_eq!(
            content_of(&none, &tick, 5).expect("row").1.exch_micros,
            None
        );
        let bad =
            csv_fields(&tick_row("", 1, "1", 1, 1, "", 2).replace(",1,NSE_FNO", ",x,NSE_FNO"));
        assert!(content_of(&bad, &tick, 0).is_err());
    }

    #[test]
    fn test_read_missing_contents_add_spare_and_match_spares_need_every_value_column_equal() {
        let shift = 19_800_000_000i64;
        // Dropped: 10 and 11 two packets with equal price and volume but a
        // different total buy quantity; 12 kept; 13 lost.
        let copy = format!(
            "{QUOTE_HDR}{}{}{}{}\n",
            tick_row(
                "2026-11-20T09:31:00.0Z",
                7,
                "5.5",
                100,
                40,
                "2026-11-20T04:00:00.000000Z",
                10
            )
            .trim_end()
            .to_owned()
                + "\n",
            tick_row(
                "2026-11-20T09:31:01.0Z",
                7,
                "5.5",
                100,
                41,
                "2026-11-20T04:00:00.000000Z",
                11
            )
            .trim_end()
            .to_owned()
                + "\n",
            tick_row(
                "2026-11-20T09:31:02.0Z",
                7,
                "6.0",
                101,
                42,
                "2026-11-20T04:00:01.000000Z",
                12
            )
            .trim_end()
            .to_owned()
                + "\n",
            tick_row(
                "2026-11-20T09:31:03.0Z",
                8,
                "1.0",
                1,
                1,
                "2026-11-20T04:00:02.000000Z",
                13
            ),
        );
        // A quotes copy: append the exchange time column (UTC-shifted).
        let copy: String = copy
            .lines()
            .enumerate()
            .map(|(i, l)| {
                if i == 0 {
                    format!("{l}\n")
                } else if l.trim().is_empty() {
                    String::new()
                } else {
                    let r = csv_fields(l);
                    format!("{l},{}\n", r[16])
                }
            })
            .collect();
        let missing = vec![10u64, 11, 13];
        let mut out = std::collections::BTreeMap::new();
        read_missing_contents(copy.as_bytes(), &missing, shift, &mut out).expect("read");
        read_missing_contents(copy.as_bytes(), &missing, shift, &mut out).expect("again");
        assert_eq!(out.keys().copied().collect::<Vec<_>>(), vec![10, 11, 13]);
        // Rebuilt `ticks` (ts = corrected exchange time): 12 kept; 90 the
        // backup copy of packet 10 (same buy quantity 40); 91 a packet with
        // packet 11's price and volume but another buy quantity; 92 packet
        // 13 with its time as stored.
        let rebuilt = format!(
            "{TICK_HDR}{}{}{}{}",
            tick_row("2026-11-20T09:30:01.000000Z", 7, "6.0", 101, 42, "x", 12),
            tick_row("2026-11-20T09:30:00.000000Z", 7, "5.5", 100, 40, "x", 90),
            tick_row("2026-11-20T09:30:00.000000Z", 7, "5.5", 100, 99, "x", 91),
            tick_row("2026-11-20T04:00:02.000000Z", 8, "1.0", 1, 1, "x", 92),
        );
        let mut lines = rebuilt.lines();
        let cols = ContentCols::from_header(lines.next().expect("header")).expect("cols");
        let dropped = vec![10u64, 11, 12, 13];
        let mut spare = std::collections::BTreeMap::new();
        for l in lines {
            add_spare(&csv_fields(l), &cols, &dropped, &mut spare).expect("spare");
        }
        let (matched, unmatched) = match_spares(&out, &mut spare.clone());
        // 10 takes 90; 11 has no exact copy (91 differs in buy quantity);
        // 13 matches as stored.
        assert_eq!(matched, 2);
        assert_eq!(unmatched, vec![11]);
        let (m, u) = match_spares(&out, &mut std::collections::BTreeMap::new());
        assert_eq!((m, u.len()), (0, 3));
        // An aux row never matches a tick row with the same numbers.
        let aux_cols = ContentCols::from_header(AUX_HDR).expect("aux");
        let aux = csv_fields(
            "2026-11-20T09:00:00.0Z,x,dhan,NSE_FNO,out_of_window_tick,c,8,,,,1.0,2026-11-20T04:00:02.000000Z,1,,,,,,,,93\n",
        );
        let mut aux_spare = std::collections::BTreeMap::new();
        add_spare(&aux, &aux_cols, &dropped, &mut aux_spare).expect("aux spare");
        let only13: std::collections::BTreeMap<u64, MissingContent> = out
            .iter()
            .filter(|(k, _)| **k == 13)
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        assert_eq!(match_spares(&only13, &mut aux_spare), (0, vec![13]));
        let mut o = std::collections::BTreeMap::new();
        assert!(read_missing_contents("capture_seq\n1\n".as_bytes(), &[1], 0, &mut o).is_err());
    }

    #[test]
    fn test_read_missing_contents_gz_reads_a_gzipped_copy() {
        let dir = std::env::temp_dir().join(format!("tv-cov-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("c.csv.gz");
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).expect("file"),
            flate2::Compression::fast(),
        );
        let body = format!(
            "{TICK_HDR}{}{}",
            tick_row("2026-11-20T04:00:00.000000Z", 1, "1", 1, 1, "x", 5),
            tick_row("2026-11-20T04:00:00.000000Z", 2, "1", 1, 1, "x", 6)
        );
        gz.write_all(body.as_bytes()).expect("write");
        gz.finish().expect("finish");
        let mut out = std::collections::BTreeMap::new();
        read_missing_contents_gz(&path, &[6], 0, &mut out).expect("read");
        assert_eq!(out.keys().copied().collect::<Vec<_>>(), vec![6]);
        assert!(read_missing_contents_gz(&dir.join("absent.gz"), &[6], 0, &mut out).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_segment_code_of_maps_every_stored_name() {
        assert_eq!(segment_code_of("IDX_I"), Some(0));
        assert_eq!(segment_code_of("NSE_EQ"), Some(1));
        assert_eq!(segment_code_of("NSE_FNO"), Some(2));
        assert_eq!(segment_code_of("BSE_FNO"), Some(8));
        assert_eq!(segment_code_of("nse_fno"), None);
        assert_eq!(segment_code_of(""), None);
    }

    #[test]
    fn test_read_kept_seqs_lines_keeps_only_the_filtered_contracts() {
        let body = "\"ts\",\"security_id\",\"segment\",\"capture_seq\"\n\
                    2026-11-20T04:00:00.000000Z,10,NSE_FNO,100\n\
                    2026-11-20T04:00:00.000000Z,11,NSE_FNO,101\n\
                    \n\
                    2026-11-20T04:00:01.000000Z,10,NSE_EQ,102\n\
                    2026-11-20T04:00:02.000000Z,10,NSE_FNO,103\n";
        let keep = |sid: u64, seg: u8| sid == 10 && seg == 2;
        let mut out = Vec::new();
        let read = read_kept_seqs_lines(body.as_bytes(), &keep, &mut out).expect("read");
        assert_eq!(read, KeptRead { rows: 4, kept: 2 });
        assert_eq!(out, vec![100, 103]);
    }

    #[test]
    fn test_read_kept_seqs_refuses_a_row_it_cannot_judge() {
        let keep = |_: u64, _: u8| true;
        let mut out = Vec::new();
        for bad in [
            "security_id,segment,capture_seq\n10,NSE_FNO,x\n",
            "security_id,segment,capture_seq\ny,NSE_FNO,1\n",
            "security_id,segment,capture_seq\n10,MOON,1\n",
            "security_id,segment,capture_seq\n10,NSE_FNO\n",
            "segment,capture_seq\nNSE_FNO,1\n",
            "security_id,segment\n10,NSE_FNO\n",
        ] {
            assert!(
                read_kept_seqs_lines(bad.as_bytes(), &keep, &mut out).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert_eq!(
            read_kept_seqs_lines("".as_bytes(), &keep, &mut out).expect("empty"),
            KeptRead::default()
        );
    }

    #[test]
    fn test_read_kept_seqs_gz_reads_a_gzipped_copy() {
        let dir = std::env::temp_dir().join(format!("tv-cov-kept-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("k.csv.gz");
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).expect("file"),
            flate2::Compression::fast(),
        );
        gz.write_all(b"security_id,segment,capture_seq\n7,IDX_I,5\n8,IDX_I,6\n")
            .expect("write");
        gz.finish().expect("finish");
        let keep = |sid: u64, _: u8| sid == 8;
        let mut out = Vec::new();
        let read = read_kept_seqs_gz(&path, &keep, &mut out).expect("read");
        assert_eq!((read.rows, read.kept, out), (2, 1, vec![6]));
        assert!(read_kept_seqs_gz(&dir.join("absent.gz"), &keep, &mut Vec::new()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
