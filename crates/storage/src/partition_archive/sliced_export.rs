//! Ordered partition export in time slices (2026-11-15, open limit of the
//! owner Quote 60 export).
//!
//! An ordered export (the daily Brutex export, and the archiver whenever an
//! object already sits at the canonical key outside the no-sort window) used
//! to send ONE `/exp` query with `ORDER BY <DEDUP key>` for the whole hour or
//! day partition, and that one request had to finish inside the export
//! client's timeout (`ARCHIVE_EXPORT_TIMEOUT_SECS`, 600 s). The largest
//! `market_depth_book` hour seen is 607,945,897 B gzipped; its sort has never
//! been measured.
//!
//! Every export DEDUP key starts with `ts` (checked when this crate is
//! compiled, [`order_key_leads_with_ts`]). So the partition exported as
//! consecutive half-open `ts` slices `[t0, t1)`, `[t1, t2)`, ... each with the
//! same `ORDER BY`, with the CSV header written once, is the same byte stream
//! as the single ordered query: a row's slice is fixed by its `ts`, and inside
//! a slice the key orders it exactly as the whole query would. The slices are
//! [`ORDERED_EXPORT_HOUR_SLICE_SECS`] (10 minutes) for an hour partition and
//! [`ORDERED_EXPORT_DAY_SLICE_SECS`] (1 hour) for a day partition. Each slice
//! is its own request with its own timeout (the export client's 600 s); the
//! whole sliced export stops at [`ORDERED_EXPORT_TOTAL_TIMEOUT_SECS`].
//!
//! [`SliceJoiner`] keeps the first slice's header, checks every later
//! slice's header is the same, drops it, and passes every data byte on. The
//! joined bytes go through the same gzip encoder, SHA-256 and quote-aware
//! record count as before, so the object, its checksum and the row count are
//! the ones the single query gave (pinned by
//! `test_sliced_ordered_export_is_byte_identical_to_the_single_query`).
//!
//! The unordered export is one request through the same code with the joiner
//! in pass-through mode: every byte is passed on unchanged.
//!
//! Cold path: the archiver's pass and the daily export, never the frame
//! drain. O(bytes) per export, as before, plus one request per slice (6 for
//! an hour, 24 for a day).

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;

use super::{
    ARCHIVE_EXPORT_TIMEOUT_SECS, CsvRecordCounter, ExportedCsv, PartitionArchiver, Sha256Writer,
    TS_LITERAL_FMT, base64_encode, build_export_sql_ordered, capture_rest_error_body, hex_encode,
    partition_range_naive, read_body_capped, remove_temp_file,
};

/// Width of one slice of an HOUR partition's ordered export: 10 minutes, so
/// an hour is 6 requests. Each sorts about a sixth of the hour's rows.
pub(crate) const ORDERED_EXPORT_HOUR_SLICE_SECS: i64 = 600;

/// Width of one slice of a DAY partition's ordered export (the candle
/// tables): 1 hour, so a day is 24 requests.
pub(crate) const ORDERED_EXPORT_DAY_SLICE_SECS: i64 = 3_600;

/// Longest a whole sliced ordered export may run: three times the old
/// single-request limit (`ARCHIVE_EXPORT_TIMEOUT_SECS`). Each slice still has
/// the export client's own 600 s; this bounds the sum, so the daily export's
/// worst case per partition stays a fixed number
/// (`daily_export::EXPORT_PARTITION_WORST_SECS`).
pub(crate) const ORDERED_EXPORT_TOTAL_TIMEOUT_SECS: u64 = 1_800;

/// Counter: `/exp` requests QuestDB refused with 429 (its export connection
/// limit) that were waited out and sent again (2026-11-17). Local `/metrics`
/// only, registered at 0 by the archiver and the daily export; no page,
/// alarm, metric filter or EMF name (noise lock).
pub const EXPORT_BUSY_RETRIES_COUNTER: &str = "tv_partition_archive_export_busy_retries_total";

/// First wait after a `/exp` 429, seconds ([`ExportBusyBackoff::PRODUCTION`]).
const EXPORT_BUSY_FIRST_WAIT_SECS: u64 = 1;
/// Longest single wait after a `/exp` 429, seconds.
const EXPORT_BUSY_STEP_CAP_SECS: u64 = 15;
/// Most one export call waits for a free `/exp` slot, seconds.
const EXPORT_BUSY_TOTAL_WAIT_SECS: u64 = 120;
/// How long a spent budget makes this archiver's later 429s fail at once.
const EXPORT_BUSY_HOLD_SECS: u64 = 600;

/// How a `/exp` request that QuestDB refused with 429 waits and tries again
/// (2026-11-17).
///
/// Measured live 2026-11-17 17:36 IST: the daily Brutex export's `/exp` for
/// `candles_10m` got `429 Too Many Requests: exceeded connection limit
/// [name=http_export_connections, numOfConnections=3, connectionLimit=2]`
/// while the archiver's export was running, and the whole day attempt failed
/// (attempt 1 of 3, retried 900 s later). The limit is 2 on purpose
/// (`deploy/docker/docker-compose.yml`): the HTTP pool is that limit plus one,
/// so an export can never take the worker `/write` and `/exec` need. So a
/// third `/exp` is refused, not queued, and the answer is to wait for a slot.
///
/// The wait before retry `n` (0-based) is `first × 2^n`, capped at
/// `step_cap`, and the waits of one export call (every slice of it) add up to
/// at most `total`; never past the export's own deadline. A 429 carries no
/// CSV, so the request is simply sent again; the slices before it are kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExportBusyBackoff {
    /// The first wait.
    pub(crate) first: Duration,
    /// The longest single wait.
    pub(crate) step_cap: Duration,
    /// The most one export call waits in total for a free slot.
    pub(crate) total: Duration,
    /// After an export spent `total`, how long this archiver's later 429s
    /// fail at once instead of waiting (see [`ExportBusyHold`]).
    pub(crate) hold_after_give_up: Duration,
}

impl ExportBusyBackoff {
    /// 1 s, 2 s, 4 s, 8 s, then 15 s steps, at most 120 s in all: a fifth of
    /// one request's own 600 s timeout (`ARCHIVE_EXPORT_TIMEOUT_SECS`) and a
    /// fifteenth of a sliced export's 1,800 s
    /// (`ORDERED_EXPORT_TOTAL_TIMEOUT_SECS`), which bounds the ordered
    /// export's waits anyway. After a spent budget the same archiver fails
    /// 429s at once for 10 minutes (two daily archive ticks), then waits
    /// again.
    pub(crate) const PRODUCTION: Self = Self {
        first: Duration::from_secs(EXPORT_BUSY_FIRST_WAIT_SECS),
        step_cap: Duration::from_secs(EXPORT_BUSY_STEP_CAP_SECS),
        total: Duration::from_secs(EXPORT_BUSY_TOTAL_WAIT_SECS),
        hold_after_give_up: Duration::from_secs(EXPORT_BUSY_HOLD_SECS),
    };

    /// The wait before retry `n` (0-based): `first × 2^n`, capped at
    /// `step_cap`. Pure, O(1).
    #[must_use]
    // TEST-EXEMPT: covered by test_export_busy_backoff_production_schedule_doubles_then_caps
    pub(crate) fn step(&self, n: u32) -> Duration {
        let factor = 1u32.checked_shl(n.min(31)).unwrap_or(u32::MAX);
        self.first
            .checked_mul(factor)
            .map_or(self.step_cap, |d| d.min(self.step_cap))
    }

    /// The wait before retry `n` when `waited` has gone already and
    /// `deadline_left` is what the export has left (`None`: no deadline), or
    /// `None` when the export must give up: the total is spent, or the wait
    /// would reach the deadline (a request sent at the deadline is refused
    /// anyway). Pure, O(1).
    #[must_use]
    // TEST-EXEMPT: covered by test_export_busy_backoff_never_waits_into_the_deadline
    pub(crate) fn next_wait(
        &self,
        n: u32,
        waited: Duration,
        deadline_left: Option<Duration>,
    ) -> Option<Duration> {
        let wait = self.step(n).min(self.total.saturating_sub(waited));
        if wait.is_zero() || deadline_left.is_some_and(|left| left <= wait) {
            return None;
        }
        Some(wait)
    }
}

/// When an export of one archiver last waited its whole busy budget out
/// (`None`: never, or a request succeeded since). While a give-up is younger
/// than [`ExportBusyBackoff::hold_after_give_up`], a 429 fails at once, so a
/// pass over many partitions against a QuestDB that keeps refusing costs one
/// budget, not one per partition. One uncontended lock per `/exp` answer,
/// never held across an await. Cold.
#[derive(Debug, Default)]
pub(crate) struct ExportBusyHold(std::sync::Mutex<Option<Instant>>);

impl ExportBusyHold {
    /// Whether a give-up younger than `hold` is on record. O(1).
    // TEST-EXEMPT: covered by test_export_busy_hold_records_and_forgets_a_give_up
    pub(crate) fn active(&self, hold: Duration) -> bool {
        self.0
            .lock()
            .map(|g| g.is_some_and(|at| at.elapsed() < hold))
            .unwrap_or(false)
    }

    /// Records a spent budget now. O(1).
    // TEST-EXEMPT: covered by test_export_busy_hold_records_and_forgets_a_give_up
    pub(crate) fn note_give_up(&self) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(Instant::now());
        }
    }

    /// Forgets the give-up (a request was answered). O(1).
    // TEST-EXEMPT: covered by test_export_busy_hold_records_and_forgets_a_give_up
    pub(crate) fn clear(&self) {
        if let Ok(mut g) = self.0.lock() {
            *g = None;
        }
    }
}

// Production waits stay well inside the timeouts they sit under.
const _: () = assert!(
    ExportBusyBackoff::PRODUCTION.total.as_secs() * 4 <= ARCHIVE_EXPORT_TIMEOUT_SECS
        && ExportBusyBackoff::PRODUCTION.total.as_secs() * 4 <= ORDERED_EXPORT_TOTAL_TIMEOUT_SECS
);

/// Longest header record the joiner buffers while it looks for the header's
/// line end (a real header is a few hundred bytes).
const MAX_HEADER_BYTES: usize = 1024 * 1024;

/// Whether an `ORDER BY` key starts with the designated timestamp `ts`
/// (`ts` alone, or `ts,` then more columns). Only then do `ts` slices joined
/// in time order give the whole query's order. Pure, O(1) in practice
/// (compares at most 3 bytes).
#[must_use]
pub(crate) const fn order_key_leads_with_ts(key: &str) -> bool {
    let b = key.as_bytes();
    if b.len() < 2 || b[0] != b't' || b[1] != b's' {
        return false;
    }
    b.len() == 2 || b[2] == b','
}

// Compile-time check: every export DEDUP key leads with `ts`. A key that did
// not would make slicing reorder rows, so it fails the build here (the
// runtime path also falls back to one query for such a key).
const _: () = {
    assert!(order_key_leads_with_ts(
        crate::tick_persistence::DEDUP_KEY_TICKS
    ));
    assert!(order_key_leads_with_ts(
        crate::tick_quotes_persistence::DEDUP_KEY_TICK_QUOTES
    ));
    assert!(order_key_leads_with_ts(
        crate::feed_aux_persistence::DEDUP_KEY_FEED_AUX_PACKETS
    ));
    assert!(order_key_leads_with_ts(
        crate::depth_persistence::DEDUP_KEY_MARKET_DEPTH
    ));
    assert!(order_key_leads_with_ts(
        crate::depth_persistence::DEDUP_KEY_MARKET_DEPTH_BOOK
    ));
    assert!(order_key_leads_with_ts(
        crate::shadow_persistence::DEDUP_KEY_CANDLES
    ));
};

/// The slice width for a partition name: [`ORDERED_EXPORT_HOUR_SLICE_SECS`]
/// for an hour partition (`YYYY-MM-DDTHH`), [`ORDERED_EXPORT_DAY_SLICE_SECS`]
/// otherwise. Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_ordered_export_slices_cover_the_partition_exactly
pub(crate) fn ordered_export_slice_secs(partition: &str) -> i64 {
    if partition.len() == 13 {
        ORDERED_EXPORT_HOUR_SLICE_SECS
    } else {
        ORDERED_EXPORT_DAY_SLICE_SECS
    }
}

/// The half-open `ts` slices `[start, end)` of `partition`, as `ts`
/// literals, in time order, covering the partition exactly (the last slice
/// ends at the partition's end). `None` for a malformed name. Pure,
/// O(slices).
#[must_use]
pub(crate) fn ordered_export_slices(partition: &str) -> Option<Vec<(String, String)>> {
    let (start, end) = partition_range_naive(partition)?;
    let width = chrono::TimeDelta::seconds(ordered_export_slice_secs(partition));
    let mut out = Vec::new();
    let mut lo = start;
    while lo < end {
        let hi = lo.checked_add_signed(width)?.min(end);
        out.push((
            lo.format(TS_LITERAL_FMT).to_string(),
            hi.format(TS_LITERAL_FMT).to_string(),
        ));
        lo = hi;
    }
    Some(out)
}

/// Joins the CSV bodies of an export's slices into one body: the first
/// slice's header once, then every slice's data in order.
///
/// A header is the first CSV record of a slice body: the bytes up to the
/// first `\n` outside double quotes. Every later header must equal the
/// first (line end aside); a slice whose body is empty is an error (QuestDB
/// always writes the header). When a slice's data does not end with a line
/// end and a later slice has data, the header's line end is put between
/// them, as the single query would have one between the two rows.
///
/// With one slice the joiner passes every byte through unchanged.
#[derive(Debug)]
pub(crate) struct SliceJoiner {
    /// `true` for a single-request export: every byte passes through.
    passthrough: bool,
    /// Index of the slice being read.
    slice: usize,
    /// The first slice's header, without its line end.
    first_header: Option<Vec<u8>>,
    /// The line end of the first header that had one (`\n` or `\r\n`).
    line_end: Option<Vec<u8>>,
    /// Header bytes of the current slice read so far.
    pending: Vec<u8>,
    /// Still reading the current slice's header.
    in_header: bool,
    /// Inside double quotes while reading the header.
    header_in_quotes: bool,
    /// The current slice has passed on a data byte.
    slice_data_started: bool,
    /// Anything was passed on.
    emitted_any: bool,
    /// The last byte passed on was `\n`.
    ends_at_record: bool,
}

impl SliceJoiner {
    /// A joiner for an export of `slices` requests. Pure, O(1).
    #[must_use]
    pub(crate) fn new(slices: usize) -> Self {
        Self {
            passthrough: slices <= 1,
            slice: 0,
            first_header: None,
            line_end: None,
            pending: Vec::new(),
            in_header: true,
            header_in_quotes: false,
            slice_data_started: false,
            emitted_any: false,
            ends_at_record: false,
        }
    }

    /// Starts slice `index` (0-based). O(1).
    // TEST-EXEMPT: covered by the test_slice_joiner_* tests
    pub(crate) fn begin_slice(&mut self, index: usize) {
        self.slice = index;
        self.pending.clear();
        self.in_header = true;
        self.header_in_quotes = false;
        self.slice_data_started = false;
    }

    /// Feeds one chunk of the current slice's body; appends the bytes to
    /// pass on to `out`. O(chunk).
    ///
    /// # Errors
    /// A header longer than 1 MiB, or one that differs from the first.
    // TEST-EXEMPT: covered by the test_slice_joiner_* tests
    pub(crate) fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> Result<()> {
        if self.passthrough {
            if !chunk.is_empty() {
                self.emitted_any = true;
            }
            out.extend_from_slice(chunk);
            return Ok(());
        }
        let mut rest = chunk;
        if self.in_header {
            let mut cut = None;
            for (i, &b) in rest.iter().enumerate() {
                match b {
                    b'"' => self.header_in_quotes = !self.header_in_quotes,
                    b'\n' if !self.header_in_quotes => {
                        cut = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            let Some(i) = cut else {
                self.pending.extend_from_slice(rest);
                anyhow::ensure!(
                    self.pending.len() <= MAX_HEADER_BYTES,
                    "/exp slice {} header is longer than {MAX_HEADER_BYTES} bytes",
                    self.slice
                );
                return Ok(());
            };
            self.pending.extend_from_slice(&rest[..=i]);
            rest = &rest[i + 1..];
            self.finish_header(true, out)?;
        }
        if !rest.is_empty() {
            if !self.slice_data_started && self.emitted_any && !self.ends_at_record {
                // The previous slice's last row had no line end.
                let end = self.line_end.clone().unwrap_or_else(|| b"\n".to_vec());
                out.extend_from_slice(&end);
            }
            self.slice_data_started = true;
            self.emitted_any = true;
            self.ends_at_record = rest.last() == Some(&b'\n');
            out.extend_from_slice(rest);
        }
        Ok(())
    }

    /// Ends the current slice; appends any bytes to pass on to `out`. O(1)
    /// plus the header comparison.
    ///
    /// # Errors
    /// An empty slice body (no CSV header), or a header that differs.
    // TEST-EXEMPT: covered by the test_slice_joiner_* tests
    pub(crate) fn end_slice(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if self.passthrough || !self.in_header {
            return Ok(());
        }
        anyhow::ensure!(
            !self.pending.is_empty(),
            "/exp slice {} returned an empty body (no CSV header)",
            self.slice
        );
        self.finish_header(false, out)
    }

    fn finish_header(&mut self, terminated: bool, out: &mut Vec<u8>) -> Result<()> {
        self.in_header = false;
        let mut content: &[u8] = &self.pending;
        let mut end: &[u8] = &[];
        if terminated {
            let n = content.len();
            let cut = if content.ends_with(b"\r\n") {
                n - 2
            } else {
                n - 1
            };
            end = &content[cut..];
            content = &content[..cut];
        }
        match &self.first_header {
            None => {
                self.first_header = Some(content.to_vec());
                out.extend_from_slice(&self.pending);
                self.emitted_any = true;
                self.ends_at_record = terminated;
            }
            Some(first) => {
                anyhow::ensure!(
                    first.as_slice() == content,
                    "/exp slice {} header differs from the first slice's header",
                    self.slice
                );
            }
        }
        if terminated {
            match &self.line_end {
                None => self.line_end = Some(end.to_vec()),
                Some(e) => anyhow::ensure!(
                    e.as_slice() == end,
                    "/exp slice {} header line end differs from the first slice's",
                    self.slice
                ),
            }
        }
        Ok(())
    }
}

impl PartitionArchiver {
    /// Puts a shorter busy-retry schedule on this archiver (tests only, so a
    /// stub-server test does not wait the production 120 s).
    #[cfg(test)]
    #[must_use]
    // TEST-EXEMPT: covered by test_export_429_forever_gives_up_at_the_bound
    pub(crate) fn with_export_busy_backoff(mut self, backoff: ExportBusyBackoff) -> Self {
        self.export_busy_backoff = backoff;
        self
    }

    /// Exports `partition` of `table` ordered by `order_by` as consecutive
    /// `ts` slices joined into one CSV (see the module docs), through gzip
    /// into `path`. A key that does not lead with `ts`, or a malformed
    /// partition name, takes the single ordered query instead. Cold.
    ///
    /// # Errors
    /// Any slice's request, status, stream, header check or the total
    /// deadline ([`ORDERED_EXPORT_TOTAL_TIMEOUT_SECS`]); the temp file is
    /// removed.
    // TEST-EXEMPT: covered by test_sliced_ordered_export_is_byte_identical_to_the_single_query
    pub(super) async fn export_ordered_csv(
        &self,
        table: &str,
        partition: &str,
        order_by: &str,
        path: PathBuf,
    ) -> Result<ExportedCsv> {
        let (start, end) =
            partition_range_naive(partition).context("unparseable partition name")?;
        let slices = if order_key_leads_with_ts(order_by) {
            ordered_export_slices(partition).unwrap_or_default()
        } else {
            Vec::new()
        };
        let sqls: Vec<String> = if slices.is_empty() {
            vec![build_export_sql_ordered(
                table,
                &start.format(TS_LITERAL_FMT).to_string(),
                &end.format(TS_LITERAL_FMT).to_string(),
                Some(order_by),
            )]
        } else {
            slices
                .iter()
                .map(|(lo, hi)| build_export_sql_ordered(table, lo, hi, Some(order_by)))
                .collect()
        };
        let deadline = Instant::now() + Duration::from_secs(ORDERED_EXPORT_TOTAL_TIMEOUT_SECS);
        self.export_requests_csv(&sqls, Some(deadline), path).await
    }

    /// Streams the `/exp` CSV of each query in `sqls`, in order, joined by
    /// [`SliceJoiner`], through gzip into `path`, counting records from the
    /// joined RAW byte stream and hashing the compressed bytes. With one
    /// query every byte passes through unchanged (the unordered export).
    /// With a `deadline`, a request that would run past it is cut to the
    /// time left, and no request starts after it. Cold.
    // TEST-EXEMPT: covered by test_sliced_ordered_export_is_byte_identical_to_the_single_query
    pub(super) async fn export_requests_csv(
        &self,
        sqls: &[String],
        deadline: Option<Instant>,
        path: PathBuf,
    ) -> Result<ExportedCsv> {
        crate::off_worker::off_worker(|| std::fs::create_dir_all(&self.temp_dir))
            .context("create archive temp dir")?;
        let file = crate::off_worker::off_worker(|| std::fs::File::create(&path))
            .context("create archive temp file")?;
        // Review round 2: the compressed stream is SHA-256-hashed as it is
        // written — the digest is the content identity for the S3
        // reuse/conflict decision and travels on the conditional PutObject.
        let mut encoder = GzEncoder::new(
            Sha256Writer::new(std::io::BufWriter::new(file)),
            Compression::default(),
        );
        let streamed = self.stream_slices(sqls, deadline, &mut encoder).await;
        let (counter, csv_bytes) = match streamed {
            Ok(v) => v,
            Err(err) => {
                drop(encoder);
                remove_temp_file(&path);
                return Err(err);
            }
        };
        let digest = crate::off_worker::off_worker(|| {
            let hashing_writer = encoder.finish().context("gzip finish failed")?;
            let (mut writer, digest) = hashing_writer.finish();
            writer.flush().context("temp file flush failed")?;
            drop(writer);
            anyhow::Ok(digest)
        })?;
        let gzip_sha256_b64 = base64_encode(&digest);
        let gzip_sha256_hex = hex_encode(&digest);

        let records = counter.records();
        if records == 0 {
            // Not even the CSV header arrived — an empty body is an export
            // failure, never "0 rows" (audit Rule 11: no false-OK).
            remove_temp_file(&path);
            anyhow::bail!("/exp export returned an empty body (no CSV header)");
        }
        let gzip_bytes = crate::off_worker::off_worker(|| std::fs::metadata(&path))
            .context("stat archive temp file")?
            .len();
        Ok(ExportedCsv {
            // records − 1 header record = data rows (QuestDB /exp always
            // emits the header record first).
            rows: records.saturating_sub(1),
            csv_bytes,
            gzip_bytes,
            gzip_sha256_b64,
            gzip_sha256_hex,
            path,
        })
    }

    /// Requests each query in turn and writes the joined bytes into
    /// `encoder`. Returns the record counter and the CSV byte count.
    async fn stream_slices<W: Write>(
        &self,
        sqls: &[String],
        deadline: Option<Instant>,
        encoder: &mut GzEncoder<W>,
    ) -> Result<(CsvRecordCounter, u64)> {
        let mut joiner = SliceJoiner::new(sqls.len());
        // F5 (review round 1): quote-aware RECORD counting — a raw `\n`
        // count over-counts on STRING fields carrying embedded newlines.
        let mut counter = CsvRecordCounter::default();
        let mut csv_bytes: u64 = 0;
        let mut out: Vec<u8> = Vec::new();
        let client_limit = Duration::from_secs(ARCHIVE_EXPORT_TIMEOUT_SECS);
        // 2026-11-17: QuestDB's export connection limit answers 429 when
        // its slots are taken (the archiver and the daily export can each
        // hold one); wait and send the same request again, bounded.
        let backoff = self.export_busy_backoff;
        let mut busy_retries: u32 = 0;
        let mut busy_waited = Duration::ZERO;
        for (index, sql) in sqls.iter().enumerate() {
            let mut response = loop {
                let mut request = self
                    .export_client
                    .get(&self.exp_url)
                    .query(&[("query", sql)]);
                if let Some(deadline) = deadline {
                    let left = deadline.saturating_duration_since(Instant::now());
                    anyhow::ensure!(
                        !left.is_zero(),
                        "/exp ordered export ran out of its {ORDERED_EXPORT_TOTAL_TIMEOUT_SECS} s \
                         total before slice {index} of {}",
                        sqls.len()
                    );
                    // The client's own timeout bounds each slice; only a
                    // deadline nearer than that cuts the request.
                    if left < client_limit {
                        request = request.timeout(left);
                    }
                }
                let response = request.send().await.with_context(|| {
                    if sqls.len() == 1 {
                        "/exp export request failed".to_string()
                    } else {
                        format!(
                            "/exp export request failed (slice {index} of {})",
                            sqls.len()
                        )
                    }
                })?;
                if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                    if response.status().is_success() {
                        self.busy_hold.clear();
                    }
                    break response;
                }
                // A 429 is QuestDB refusing the connection before it runs the
                // query: its body is the short refusal text, never CSV, and
                // nothing of this slice has reached the joiner yet, so the
                // request is sent again from scratch.
                if self.busy_hold.active(backoff.hold_after_give_up) {
                    // An earlier export of this archiver waited its whole
                    // budget out moments ago: fail at once, as a 429 always
                    // did, so a pass over many partitions is not slowed by a
                    // QuestDB that keeps refusing.
                    break response;
                }
                let deadline_left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                let Some(wait) = backoff.next_wait(busy_retries, busy_waited, deadline_left) else {
                    // Out of waiting: fail exactly as a 429 always did. A
                    // spent budget (not a near deadline) holds later exports
                    // of this archiver off waiting for a while.
                    if busy_waited >= backoff.total {
                        self.busy_hold.note_give_up();
                    }
                    break response;
                };
                let body =
                    capture_rest_error_body(&read_body_capped(response).await.unwrap_or_default());
                if busy_retries == 0 {
                    tracing::warn!(
                        code = tickvault_common::error_code::ErrorCode::StorageGap04S3ArchiveFailed
                            .code_str(),
                        slice = index,
                        slices = sqls.len(),
                        wait_budget_secs = backoff.total.as_secs(),
                        body = %body,
                        "/exp refused with 429 (QuestDB export connection limit); waiting \
                         for a free slot and sending the same request again (logged once \
                         per export)"
                    );
                }
                metrics::counter!(EXPORT_BUSY_RETRIES_COUNTER).increment(1);
                tokio::time::sleep(wait).await;
                busy_retries = busy_retries.saturating_add(1);
                busy_waited = busy_waited.saturating_add(wait);
            };
            if !response.status().is_success() {
                let status = response.status();
                let body =
                    capture_rest_error_body(&read_body_capped(response).await.unwrap_or_default());
                if busy_retries > 0 {
                    anyhow::bail!(
                        "/exp export returned {status}: {body} (after {busy_retries} busy \
                         retries, {} s waited)",
                        busy_waited.as_secs()
                    );
                }
                anyhow::bail!("/exp export returned {status}: {body}");
            }
            joiner.begin_slice(index);
            while let Some(chunk) = response.chunk().await.context("/exp stream read failed")? {
                out.clear();
                joiner.feed(&chunk, &mut out)?;
                Self::pass_on(&out, &mut counter, &mut csv_bytes, encoder)?;
            }
            out.clear();
            joiner.end_slice(&mut out)?;
            Self::pass_on(&out, &mut counter, &mut csv_bytes, encoder)?;
        }
        if busy_retries > 0 {
            tracing::info!(
                busy_retries,
                waited_ms = u64::try_from(busy_waited.as_millis()).unwrap_or(u64::MAX),
                "/exp export completed after waiting for a free QuestDB export slot"
            );
        }
        Ok((counter, csv_bytes))
    }

    /// Counts and compresses one run of joined bytes. O(bytes).
    fn pass_on<W: Write>(
        bytes: &[u8],
        counter: &mut CsvRecordCounter,
        csv_bytes: &mut u64,
        encoder: &mut GzEncoder<W>,
    ) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        counter.update(bytes);
        *csv_bytes = csv_bytes.saturating_add(bytes.len() as u64);
        crate::off_worker::off_worker(|| encoder.write_all(bytes)).context("gzip write failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_export_busy_hold_records_and_forgets_a_give_up() {
        let hold = ExportBusyHold::default();
        let long = Duration::from_secs(600);
        assert!(!hold.active(long), "nothing on record");
        hold.note_give_up();
        assert!(hold.active(long), "a fresh give-up holds");
        assert!(!hold.active(Duration::ZERO), "a zero hold never holds");
        hold.clear();
        assert!(!hold.active(long), "an answered request lifts it");
    }

    #[test]
    fn test_export_busy_backoff_production_schedule_doubles_then_caps() {
        let b = ExportBusyBackoff::PRODUCTION;
        let steps: Vec<u64> = (0..7).map(|n| b.step(n).as_secs()).collect();
        assert_eq!(steps, vec![1, 2, 4, 8, 15, 15, 15]);
        // A huge retry number never overflows; it stays at the cap.
        assert_eq!(b.step(u32::MAX), b.step_cap);
    }

    #[test]
    fn test_export_busy_backoff_total_wait_is_bounded() {
        let b = ExportBusyBackoff::PRODUCTION;
        let mut waited = Duration::ZERO;
        let mut n = 0u32;
        while let Some(w) = b.next_wait(n, waited, None) {
            waited += w;
            n += 1;
            assert!(n < 1_000, "the schedule must end");
        }
        assert_eq!(waited, b.total, "the waits use the whole budget, no more");
        assert!(waited.as_secs() * 4 <= ARCHIVE_EXPORT_TIMEOUT_SECS);
        // 1+2+4+8 = 15, then 15 s steps up to 120: 4 + 7 = 11 retries.
        assert_eq!(n, 11);
    }

    #[test]
    fn test_export_busy_backoff_never_waits_into_the_deadline() {
        let b = ExportBusyBackoff::PRODUCTION;
        // 2 s left and a 4 s wait due: give up now.
        assert_eq!(
            b.next_wait(2, Duration::ZERO, Some(Duration::from_secs(2))),
            None
        );
        // Exactly the wait left: give up (a request sent at the deadline is refused).
        assert_eq!(
            b.next_wait(0, Duration::ZERO, Some(Duration::from_secs(1))),
            None
        );
        assert_eq!(
            b.next_wait(0, Duration::ZERO, Some(Duration::from_secs(5))),
            Some(Duration::from_secs(1))
        );
        // The last wait is cut to the budget left.
        assert_eq!(
            b.next_wait(9, Duration::from_secs(110), None),
            Some(Duration::from_secs(10))
        );
        assert_eq!(b.next_wait(10, b.total, None), None);
    }

    /// Joins `slices` (each split into chunks of `chunk`) with the joiner.
    fn join(slices: &[&[u8]], chunk: usize) -> Result<Vec<u8>> {
        let mut j = SliceJoiner::new(slices.len());
        let mut all = Vec::new();
        for (i, s) in slices.iter().enumerate() {
            j.begin_slice(i);
            for c in s.chunks(chunk.max(1)) {
                j.feed(c, &mut all)?;
            }
            j.end_slice(&mut all)?;
        }
        Ok(all)
    }

    #[test]
    fn test_order_key_leads_with_ts() {
        assert!(order_key_leads_with_ts("ts"));
        assert!(order_key_leads_with_ts("ts, security_id"));
        assert!(order_key_leads_with_ts("ts,security_id"));
        assert!(!order_key_leads_with_ts("tsx, a"));
        assert!(!order_key_leads_with_ts("security_id, ts"));
        assert!(!order_key_leads_with_ts(""));
        assert!(!order_key_leads_with_ts("t"));
    }

    #[test]
    fn test_ordered_export_slices_cover_the_partition_exactly() {
        let hour = ordered_export_slices("2026-04-01T09").expect("hour");
        assert_eq!(hour.len(), 6);
        assert_eq!(hour[0].0, "2026-04-01T09:00:00.000000Z");
        assert_eq!(hour[0].1, "2026-04-01T09:10:00.000000Z");
        assert_eq!(hour[5].1, "2026-04-01T10:00:00.000000Z");
        for w in hour.windows(2) {
            assert_eq!(w[0].1, w[1].0, "consecutive, no gap, no overlap");
        }
        let day = ordered_export_slices("2026-04-01").expect("day");
        assert_eq!(day.len(), 24);
        assert_eq!(day[0].0, "2026-04-01T00:00:00.000000Z");
        assert_eq!(day[23].1, "2026-04-02T00:00:00.000000Z");
        assert!(ordered_export_slices("garbage").is_none());
        assert_eq!(ordered_export_slice_secs("2026-04-01T09"), 600);
        assert_eq!(ordered_export_slice_secs("2026-04-01"), 3_600);
    }

    #[test]
    fn test_slice_joiner_single_slice_passes_every_byte_through() {
        for body in [
            b"h\nno line end".as_slice(),
            b"just one line",
            b"\"a\nb\",c\r\n1,2\r\n",
            b"",
        ] {
            assert_eq!(join(&[body], 3).expect("join"), body);
        }
    }

    #[test]
    fn test_slice_joiner_writes_the_header_once_and_keeps_rows() {
        let got = join(
            &[
                b"ts,v\r\n1,a\r\n",
                b"ts,v\r\n",
                b"ts,v\r\n2,\"x,\ny\"\r\n3,b\r\n",
            ],
            1,
        )
        .expect("join");
        assert_eq!(got, b"ts,v\r\n1,a\r\n2,\"x,\ny\"\r\n3,b\r\n");
    }

    #[test]
    fn test_slice_joiner_quoted_header_newline_is_not_the_header_end() {
        let got = join(&[b"\"t\ns\",v\n1,a\n", b"\"t\ns\",v\n2,b\n"], 2).expect("join");
        assert_eq!(got, b"\"t\ns\",v\n1,a\n2,b\n");
    }

    #[test]
    fn test_slice_joiner_puts_a_line_end_between_unterminated_slices() {
        let got = join(&[b"ts,v\n1,a", b"ts,v\n2,b"], 4).expect("join");
        assert_eq!(got, b"ts,v\n1,a\n2,b");
        // An unterminated header-only first slice.
        let got = join(&[b"ts,v", b"ts,v\r\n2,b\r\n"], 4).expect("join");
        assert_eq!(got, b"ts,v\r\n2,b\r\n");
    }

    #[test]
    fn test_slice_joiner_refuses_a_different_header_or_an_empty_slice() {
        assert!(join(&[b"ts,v\n1,a\n", b"ts,w\n2,b\n"], 3).is_err());
        assert!(join(&[b"ts,v\n1,a\n", b""], 3).is_err());
        assert!(join(&[b"ts,v\n1,a\n", b"ts,v\r\n2,b\r\n"], 3).is_err());
        assert!(join(&[b"", b"ts,v\n"], 3).is_err());
    }
}
