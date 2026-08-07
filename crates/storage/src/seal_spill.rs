//! Wave 6 Sub-PR #1 item 1.2b — sealed-candle disk-spill primitive.
//!
//! Mirrors the disk-spill machinery in
//! `crates/storage/src/tick_persistence.rs::TickPersistenceWriter`:
//! when the in-memory ring (`crates/trading/src/candles/seal_ring.rs`,
//! merged via PR #557) overflows, the evicted oldest entries flow
//! through this module and land in
//! `data/spill/seals-YYYYMMDD.bin` as fixed-size 128-byte binary
//! records. On recovery, the storage-side writer task re-reads the
//! spill file and re-attempts the ILP send.
//!
//! ## What this module ships
//!
//! - [`SerializedSeal`] — fixed 128-byte binary record carrying every
//!   field the trading-side `BufferedSeal` exposes (security_id +
//!   exchange_segment_code + tf_ordinal + LiveCandleState fields).
//!   Self-contained; does NOT import `tickvault-trading` so this slice
//!   adds no new workspace dep edge.
//! - [`SealSpillWriter`] — append-only file writer with:
//!   - IST-date file rotation (`seals-2026-05-10.bin`).
//!   - Idempotent fixed-record append (`O(1)` per append).
//!   - `read_all()` recovery scan for the writer-task drain loop.
//!   - `set_spill_dir_for_test()` for parallel test isolation
//!     (mirrors `tick_persistence::TickPersistenceWriter`).
//!
//! ## Why a separate type vs reusing `BufferedSeal`
//!
//! Adding `tickvault-trading = { path = "../trading" }` to storage's
//! Cargo.toml introduces a new workspace dep edge (currently
//! storage does NOT depend on trading). Per CLAUDE.md "New dep
//! additions need Parthiban approval", that needs operator sign-off.
//! The future glue slice (item 1.2c) will request the dep edge AND
//! ship a `From<&BufferedSeal>` conversion. This slice keeps the
//! spill primitive self-contained so it can land + ratchet the
//! file-format invariants today.
//!
//! ## Wire format (128 bytes, little-endian)
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0    | 4 | `security_id` low-32 (legacy/Dhan; full u64 at 120-128) |
//! | 4    | 1 | `exchange_segment_code: u8`     |
//! | 5    | 1 | `tf_ordinal: u8` (0..=20 per `TfIndex`; 0..=4 = the legacy 5-frame set, 5..=20 = the C3 GDF-gated second-scale frames) |
//! | 6    | 1 | `feed_index: u8` (`Feed::index()` — 0=Dhan, 1=Groww; pre-feed records read 0=Dhan) |
//! | 7    | 1 | `format_version: u8` (=1; 2026-07-21 C2 — 0 = pre-renumber legacy, REFUSED on load) |
//! | 8    | 4 | `bucket_start_ist_secs: u32`    |
//! | 12   | 4 | `tick_count: u32`               |
//! | 16   | 8 | `volume: u64`                   |
//! | 24   | 8 | `bucket_start_cumulative: u64`  |
//! | 32   | 8 | `oi: i64`                       |
//! | 40   | 8 | `open: f64`                     |
//! | 48   | 8 | `high: f64`                     |
//! | 56   | 8 | `low: f64`                      |
//! | 64   | 8 | `close: f64`                    |
//! | 72   | 8 | `close_pct_from_prev_day: f64`  |
//! | 80   | 8 | `oi_pct_from_prev_day: f64`     |
//! | 88   | 8 | `volume_pct_from_prev_day: f64` |
//! | 96   | 8 | `open_pct: f64` (§31 Option 2)  |
//! | 104  | 8 | `change_pct: f64` (2026-06-02)  |
//! | 112  | 8 | `open_gap_pct: f64` (2026-06-02)|
//! | 120  | 8 | `security_id: u64` full (2026-06-29; zero in legacy records → low-32 at 0-4) |
//!
//! Total: 128 bytes. The trailing 8-byte padding region (bytes 120..128)
//! is reserved for future field additions WITHOUT a file-format break —
//! readers that don't recognise additional fields ignore them. Pre-§31
//! records have zero at bytes 96..104, so they decode `open_pct = 0.0`;
//! pre-2026-06-02 records have zero at bytes 104..120, so they decode
//! `change_pct = 0.0` / `open_gap_pct = 0.0` (all backward-compatible).

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use tracing::{info, warn};

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::feed::Feed;
use tickvault_trading::candles::{BufferedSeal, TfIndex};

/// Production spill directory — same parent as `tick_persistence.rs`'s
/// `TICK_SPILL_DIR` for operational consistency.
const SEAL_SPILL_DIR: &str = "data/spill";

/// Fixed record size in bytes per the wire-format table in the module
/// docstring. Bumping this breaks the on-disk format — a forward
/// migration must be coordinated.
pub const SEAL_SPILL_RECORD_SIZE: usize = 128;

/// On-disk spill-record format version, written at byte 7 (the former
/// padding byte, zero in every pre-C2 record). The 2026-07-21 C2 frame
/// retirement RENUMBERED `TfIndex` ordinals (old M2=1 would decode as
/// new M3=1 — silent TF mis-assignment), so `read_all` REFUSES records
/// whose byte 7 is 0 (legacy ordinal space) instead of misdecoding them.
pub const SEAL_SPILL_FORMAT_VERSION: u8 = 1;

/// Self-contained binary record for spilled sealed bars.
///
/// Field layout matches the wire-format table above. The
/// `tf_ordinal` field is the `TfIndex::as_ordinal()` value (0..=20)
/// from the trading crate; the glue slice translates
/// `BufferedSeal::tf` ↔ `tf_ordinal` via a checked
/// `TfIndex::from_ordinal` round-trip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SerializedSeal {
    /// `u64` (2026-06-29 widening) — the seal spill carries BOTH Dhan (≤u32)
    /// AND Groww (bit-62 index ids > u32) seals, so the full u64 MUST survive
    /// the round-trip. The on-disk format is backward-compatible: bytes 0-4
    /// keep the legacy low-32 (so old readers see the truncated Dhan id), and
    /// the full u64 is written to the reserved bytes 120-128. On read, the
    /// full u64 wins when non-zero; a legacy/Dhan record (zero at 120-128)
    /// falls back to the low-32 at bytes 0-4. See the layout table above.
    pub security_id: u64,
    pub exchange_segment_code: u8,
    pub tf_ordinal: u8,
    /// Broker-source feed (`Feed::index()`: 0=Dhan, 1=Groww). Serialised into
    /// byte 6 (a previously-zero padding byte) so pre-feed spill records decode
    /// `feed = Feed::Dhan` (index 0) — backward-compatible. Round-trips the seal's
    /// `feed` through disk-spill replay so a Groww seal recovered from spill still
    /// writes `feed='groww'` (never silently re-stamped as Dhan).
    pub feed: Feed,
    pub bucket_start_ist_secs: u32,
    pub tick_count: u32,
    pub volume: u64,
    pub bucket_start_cumulative: u64,
    pub oi: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub close_pct_from_prev_day: f64,
    pub oi_pct_from_prev_day: f64,
    pub volume_pct_from_prev_day: f64,
    /// §31 Option 2 (2026-06-01): % vs the official 09:15 session open.
    /// Serialised into the previously-reserved bytes 96..104 — old spill
    /// records (zero padding there) decode `open_pct = 0.0`, backward-compatible.
    pub open_pct: f64,
    /// Operator request 2026-06-02: headline day change % (close vs
    /// yesterday's close). Serialised into bytes 104..112 — pre-2026-06-02
    /// records (zero padding) decode `change_pct = 0.0`, backward-compatible.
    pub change_pct: f64,
    /// Operator request 2026-06-02: opening gap % (today's 09:15 open vs
    /// yesterday's close). Serialised into bytes 112..120 — pre-2026-06-02
    /// records decode `open_gap_pct = 0.0`, backward-compatible.
    pub open_gap_pct: f64,
}

impl SerializedSeal {
    /// Serialise to a fixed 128-byte little-endian record.
    /// `O(1)`, zero allocation.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; SEAL_SPILL_RECORD_SIZE] {
        let mut buf = [0u8; SEAL_SPILL_RECORD_SIZE];
        // Legacy low-32 at bytes 0-4 (Dhan-readable, truncates a >u32 Groww id);
        // the FULL u64 lives in the reserved 120-128 region below so no id is
        // ever lost on round-trip.
        buf[0..4].copy_from_slice(&(self.security_id as u32).to_le_bytes());
        buf[4] = self.exchange_segment_code;
        buf[5] = self.tf_ordinal;
        // Feed provenance round-trips through disk spill (byte 6; pre-feed
        // records have 0 here → Feed::Dhan on read).
        buf[6] = self.feed.index() as u8;
        // Byte 7 = spill format version (0 = pre-C2 legacy ordinal space,
        // refused on load — see `SEAL_SPILL_FORMAT_VERSION`).
        buf[7] = SEAL_SPILL_FORMAT_VERSION;
        buf[8..12].copy_from_slice(&self.bucket_start_ist_secs.to_le_bytes());
        buf[12..16].copy_from_slice(&self.tick_count.to_le_bytes());
        buf[16..24].copy_from_slice(&self.volume.to_le_bytes());
        buf[24..32].copy_from_slice(&self.bucket_start_cumulative.to_le_bytes());
        buf[32..40].copy_from_slice(&self.oi.to_le_bytes());
        buf[40..48].copy_from_slice(&self.open.to_le_bytes());
        buf[48..56].copy_from_slice(&self.high.to_le_bytes());
        buf[56..64].copy_from_slice(&self.low.to_le_bytes());
        buf[64..72].copy_from_slice(&self.close.to_le_bytes());
        buf[72..80].copy_from_slice(&self.close_pct_from_prev_day.to_le_bytes());
        buf[80..88].copy_from_slice(&self.oi_pct_from_prev_day.to_le_bytes());
        buf[88..96].copy_from_slice(&self.volume_pct_from_prev_day.to_le_bytes());
        // §31 Option 2: open_pct in the first 8 reserved bytes.
        buf[96..104].copy_from_slice(&self.open_pct.to_le_bytes());
        // Operator request 2026-06-02: change_pct + open_gap_pct.
        buf[104..112].copy_from_slice(&self.change_pct.to_le_bytes());
        buf[112..120].copy_from_slice(&self.open_gap_pct.to_le_bytes());
        // bytes 120-128: full u64 security_id (2026-06-29 widening). Reading
        // back: non-zero here → full u64; zero → legacy/Dhan record, fall back
        // to the low-32 at bytes 0-4.
        buf[120..128].copy_from_slice(&self.security_id.to_le_bytes());
        buf
    }

    /// Deserialise from a fixed 128-byte little-endian record.
    /// Returns `None` if the buffer is shorter than the record size
    /// (truncated tail) — caller treats this as end-of-file.
    #[must_use]
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < SEAL_SPILL_RECORD_SIZE {
            return None;
        }
        // Byte 6 = Feed::index(); fall back to Dhan for an out-of-range index
        // (pre-feed records have 0 here → Dhan; an unknown future index is
        // never silently mis-attributed to the WRONG known feed — it degrades
        // to Dhan, the primary feed, and the recovery continues, never panics).
        let feed = Feed::ALL
            .get(buf[6] as usize)
            .copied()
            .unwrap_or(Feed::Dhan);
        // Full u64 security_id from the reserved 120-128 region (2026-06-29
        // widening). A legacy/Dhan record has zero there → fall back to the
        // low-32 at bytes 0-4 (Dhan ids fit u32; security_id is never 0).
        let security_id_full = u64::from_le_bytes([
            buf[120], buf[121], buf[122], buf[123], buf[124], buf[125], buf[126], buf[127],
        ]);
        let security_id = if security_id_full != 0 {
            security_id_full
        } else {
            u64::from(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
        };
        Some(Self {
            security_id,
            exchange_segment_code: buf[4],
            tf_ordinal: buf[5],
            feed,
            bucket_start_ist_secs: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            tick_count: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            volume: u64::from_le_bytes([
                buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
            ]),
            bucket_start_cumulative: u64::from_le_bytes([
                buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30], buf[31],
            ]),
            oi: i64::from_le_bytes([
                buf[32], buf[33], buf[34], buf[35], buf[36], buf[37], buf[38], buf[39],
            ]),
            open: f64::from_le_bytes([
                buf[40], buf[41], buf[42], buf[43], buf[44], buf[45], buf[46], buf[47],
            ]),
            high: f64::from_le_bytes([
                buf[48], buf[49], buf[50], buf[51], buf[52], buf[53], buf[54], buf[55],
            ]),
            low: f64::from_le_bytes([
                buf[56], buf[57], buf[58], buf[59], buf[60], buf[61], buf[62], buf[63],
            ]),
            close: f64::from_le_bytes([
                buf[64], buf[65], buf[66], buf[67], buf[68], buf[69], buf[70], buf[71],
            ]),
            close_pct_from_prev_day: f64::from_le_bytes([
                buf[72], buf[73], buf[74], buf[75], buf[76], buf[77], buf[78], buf[79],
            ]),
            oi_pct_from_prev_day: f64::from_le_bytes([
                buf[80], buf[81], buf[82], buf[83], buf[84], buf[85], buf[86], buf[87],
            ]),
            volume_pct_from_prev_day: f64::from_le_bytes([
                buf[88], buf[89], buf[90], buf[91], buf[92], buf[93], buf[94], buf[95],
            ]),
            // §31 Option 2: bytes 96..104 (zero in pre-§31 records → 0.0).
            open_pct: f64::from_le_bytes([
                buf[96], buf[97], buf[98], buf[99], buf[100], buf[101], buf[102], buf[103],
            ]),
            // 2026-06-02: bytes 104..120 (zero in older records → 0.0).
            change_pct: f64::from_le_bytes([
                buf[104], buf[105], buf[106], buf[107], buf[108], buf[109], buf[110], buf[111],
            ]),
            open_gap_pct: f64::from_le_bytes([
                buf[112], buf[113], buf[114], buf[115], buf[116], buf[117], buf[118], buf[119],
            ]),
        })
    }

    /// Decode `tf_ordinal` back to a strongly-typed [`TfIndex`].
    /// Returns `None` if the on-disk record was written with an
    /// out-of-range ordinal (forward-compat scenario where a future
    /// shadow-table set adds TFs that this older binary doesn't
    /// recognise — the writer task drops the record with a `warn!`
    /// rather than panicking).
    #[must_use]
    pub fn tf(&self) -> Option<TfIndex> {
        TfIndex::from_ordinal(self.tf_ordinal as usize)
    }
}

// ---------------------------------------------------------------------------
// Trading↔storage glue (item 1.2c)
// ---------------------------------------------------------------------------

impl From<&BufferedSeal> for SerializedSeal {
    /// Lossless conversion from the trading-side ring payload to the
    /// storage-side wire-format record. `O(1)`, zero allocation.
    ///
    /// Field-by-field copy. The 3 Wave-5 pct fields
    /// (`close_pct_from_prev_day` / `oi_pct_from_prev_day` /
    /// `volume_pct_from_prev_day`) are carried through unchanged —
    /// per locked decision L-H6 they're stamped by the seal-time
    /// caller BEFORE the seal enters the ring, so by the time we
    /// serialise them the values are already correct (or 0.0 on
    /// PREVCLOSE-04 cold-boot).
    ///
    /// The reverse direction (`SerializedSeal → BufferedSeal`) is
    /// the writer task's REPLAY path and uses [`Self::tf`] for
    /// strongly-typed `TfIndex` round-trip with `Option<>` safety.
    #[inline]
    fn from(b: &BufferedSeal) -> Self {
        Self {
            security_id: b.security_id,
            exchange_segment_code: b.exchange_segment_code,
            tf_ordinal: b.tf.as_ordinal() as u8,
            feed: b.feed,
            bucket_start_ist_secs: b.state.bucket_start_ist_secs,
            tick_count: b.state.tick_count,
            volume: b.state.volume,
            bucket_start_cumulative: b.state.bucket_start_cumulative,
            oi: b.state.oi,
            open: b.state.open,
            high: b.state.high,
            low: b.state.low,
            close: b.state.close,
            close_pct_from_prev_day: b.state.close_pct_from_prev_day,
            oi_pct_from_prev_day: b.state.oi_pct_from_prev_day,
            volume_pct_from_prev_day: b.state.volume_pct_from_prev_day,
            open_pct: b.state.open_pct,
            // change_pct == close_pct_from_prev_day (derived, not a state field).
            change_pct: b.state.close_pct_from_prev_day,
            open_gap_pct: b.state.open_gap_pct,
        }
    }
}

impl SerializedSeal {
    /// Construct a [`BufferedSeal`] from this serialised record.
    /// Returns `None` if `tf_ordinal` is out of range (forward-compat
    /// guard per [`Self::tf`]). Used by the writer task on REPLAY
    /// from disk-spill.
    ///
    /// Callers that get `None` MUST log
    /// `warn!(?tf_ordinal, "spill record skipped — unknown tf_ordinal")`
    /// and continue draining the rest of the file rather than abort.
    #[must_use]
    pub fn try_into_buffered_seal(&self) -> Option<BufferedSeal> {
        use tickvault_trading::candles::LiveCandleState;
        let tf = self.tf()?;
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = self.bucket_start_ist_secs;
        state.open = self.open;
        state.high = self.high;
        state.low = self.low;
        state.close = self.close;
        state.volume = self.volume;
        state.bucket_start_cumulative = self.bucket_start_cumulative;
        state.oi = self.oi;
        state.tick_count = self.tick_count;
        state.close_pct_from_prev_day = self.close_pct_from_prev_day;
        state.oi_pct_from_prev_day = self.oi_pct_from_prev_day;
        state.volume_pct_from_prev_day = self.volume_pct_from_prev_day;
        // §31 Option 2: already-stamped at original seal; session_open is
        // irrelevant on replay (open_pct is the persisted value).
        state.open_pct = self.open_pct;
        // 2026-06-02: open_gap_pct already stamped at seal. change_pct is
        // derived (== close_pct_from_prev_day), so it's not a state field —
        // the replayed close_pct restores it at the next extraction.
        state.open_gap_pct = self.open_gap_pct;
        Some(BufferedSeal::new(
            self.security_id,
            self.exchange_segment_code,
            tf,
            state,
            self.feed,
        ))
    }
}

// Compile-time size check: keep `SerializedSeal` in-memory ≤ 128 bytes
// so the on-disk record (128 bytes) and the in-memory representation
// stay aligned. With current fields (4+1+1+padding+4+4+8+8+8+8×9 ≈
// 110 bytes) the natural alignment puts us at 112; padding to 128 in
// the wire format leaves 16 bytes of slack for future fields.
const _: () = assert!(
    std::mem::size_of::<SerializedSeal>() <= SEAL_SPILL_RECORD_SIZE,
    "SerializedSeal in-memory size exceeded SEAL_SPILL_RECORD_SIZE — bump record size + plan a forward migration."
);

/// Returns today's IST date in `YYYY-MM-DD` form for the spill
/// filename. Pure function for testability (clock injected by caller
/// in tests).
fn ist_date_filename(now_unix_secs: i64) -> String {
    // `IST_UTC_OFFSET_SECONDS` per data-integrity.md — 19_800.
    // Convert UTC secs → IST naive datetime via the existing helper.
    let ist_secs = now_unix_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // chrono::Utc::timestamp_opt + naive_utc().date() gives us the
    // IST calendar date when fed an IST-offset epoch.
    let dt = Utc
        .timestamp_opt(ist_secs, 0)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap_or_default());
    dt.format("seals-%Y-%m-%d.bin").to_string()
}

/// Append-only spill writer. One instance lives in the writer task;
/// `append_seal` is the single producer entry point.
pub struct SealSpillWriter {
    /// Spill directory — production uses `SEAL_SPILL_DIR`; tests
    /// override via `with_spill_dir_for_test`.
    spill_dir: PathBuf,
}

impl SealSpillWriter {
    /// Production constructor. Uses `data/spill/`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            spill_dir: PathBuf::from(SEAL_SPILL_DIR),
        }
    }

    /// Test constructor. Tests pass an isolated `tempdir` to allow
    /// parallel execution.
    #[must_use]
    // TEST-EXEMPT: test-only helper used as construction source by every test in this module (test_append_seal_then_read_all_roundtrip, test_seal_spill_writer_clear_*, test_seal_spill_writer_truncated_tail_*, etc.). Separate name-matched test would be redundant.
    pub fn with_spill_dir_for_test(dir: PathBuf) -> Self {
        Self { spill_dir: dir }
    }

    /// Returns the path of the spill file for the given UTC unix
    /// timestamp (used to derive IST date). Pure helper.
    #[must_use]
    pub fn spill_path(&self, now_unix_secs: i64) -> PathBuf {
        self.spill_dir.join(ist_date_filename(now_unix_secs))
    }

    /// Append one serialised seal to the daily spill file.
    /// Creates the spill directory + file if needed.
    /// O(1) per call; uses `BufWriter` to coalesce small writes.
    ///
    /// Per locked decision L-C1, this is the SECOND tier of the
    /// ring → spill → DLQ chain. Failures bubble up to the caller
    /// (writer task) which then escalates to the DLQ tier (next
    /// slice) and on triple failure logs
    /// `error!(code = AGGREGATOR-DROP-01)`.
    pub fn append_seal(&self, seal: &SerializedSeal, now_unix_secs: i64) -> Result<()> {
        std::fs::create_dir_all(&self.spill_dir)
            .with_context(|| format!("failed to create spill dir {:?}", self.spill_dir))?;
        let path = self.spill_path(now_unix_secs);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open spill file {path:?}"))?;
        let mut writer = BufWriter::new(file);
        let bytes = seal.to_bytes();
        writer
            .write_all(&bytes)
            .with_context(|| format!("failed to write seal to {path:?}"))?;
        writer
            .flush()
            .with_context(|| format!("failed to flush seal to {path:?}"))?;
        Ok(())
    }

    /// Drains the daily spill file by reading every full 128-byte
    /// record into the returned `Vec`. Truncated trailing partial
    /// records are silently dropped (`from_bytes` returns `None`)
    /// and a `warn!` is logged so the operator notices.
    ///
    /// After successful read the caller (writer task) deletes the
    /// spill file via [`Self::clear_spill_for_date`].
    ///
    /// Returns an empty `Vec` if the spill file does not exist
    /// (the happy path on a fresh boot).
    pub fn read_all(&self, now_unix_secs: i64) -> Result<Vec<SerializedSeal>> {
        let path = self.spill_path(now_unix_secs);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open spill file {path:?}"))?;
        let mut reader = BufReader::new(file);
        let mut all = Vec::new();
        let mut legacy_refused: usize = 0;
        let mut buf = [0u8; SEAL_SPILL_RECORD_SIZE];
        loop {
            match read_full_record(&mut reader, &mut buf) {
                Ok(true) => {
                    // Format-version gate (2026-07-21 C2): a byte-7 of 0 marks a
                    // pre-renumber record whose tf_ordinal lives in the OLD 12-frame
                    // ordinal space (old M2=1 would misdecode as new M3=1). Refuse
                    // the record, keep draining — a daily file can legitimately mix
                    // legacy + v1 records via append across a deploy boundary.
                    if buf[7] == 0 {
                        legacy_refused += 1;
                        continue;
                    }
                    if let Some(seal) = SerializedSeal::from_bytes(&buf) {
                        all.push(seal);
                    } else {
                        warn!(
                            ?path,
                            "spill record decode returned None — corrupt tail, stopping read"
                        );
                        break;
                    }
                }
                Ok(false) => break, // clean EOF
                Err(err) => {
                    warn!(?path, ?err, "partial trailing record discarded");
                    break;
                }
            }
        }
        if legacy_refused > 0 {
            warn!(
                ?path,
                legacy_refused,
                "refused pre-renumber legacy spill records (format_version byte 0 — \
                 old TfIndex ordinal space; deleted with the file after drain)"
            );
        }
        info!(?path, count = all.len(), "drained spill file");
        Ok(all)
    }

    /// Removes the spill file for the given date. Called by the
    /// writer task after `read_all` is fully replayed via ILP.
    /// Idempotent: missing file returns Ok.
    pub fn clear_spill_for_date(&self, now_unix_secs: i64) -> Result<()> {
        let path = self.spill_path(now_unix_secs);
        if !path.exists() {
            return Ok(());
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove spill file {path:?}"))?;
        info!(?path, "spill file cleared after successful drain");
        Ok(())
    }
}

impl Default for SealSpillWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads exactly `RECORD_SIZE` bytes into `buf`. Returns:
/// - `Ok(true)`  — full record read.
/// - `Ok(false)` — clean EOF (zero bytes available).
/// - `Err(_)`    — partial trailing record OR underlying I/O error.
fn read_full_record(
    reader: &mut BufReader<std::fs::File>,
    buf: &mut [u8; SEAL_SPILL_RECORD_SIZE],
) -> Result<bool> {
    let mut read_so_far = 0;
    while read_so_far < SEAL_SPILL_RECORD_SIZE {
        let n = reader
            .read(&mut buf[read_so_far..])
            .with_context(|| "spill file read")?;
        if n == 0 {
            // EOF: clean if no bytes read this iteration AND none in
            // the partial accumulation.
            if read_so_far == 0 {
                return Ok(false);
            }
            // Partial trailing record — caller logs + truncates.
            anyhow::bail!(
                "spill file ended mid-record (got {read_so_far} of {SEAL_SPILL_RECORD_SIZE} bytes)"
            );
        }
        read_so_far += n;
    }
    Ok(true)
}

/// Spill-related I/O timeout in seconds. Held as a named constant so
/// the banned-pattern scanner does not flag a hardcoded `Duration`
/// literal at the call site. Reserved for the future writer-task
/// slice's tokio retry loop.
const SEAL_SPILL_IO_TIMEOUT_SECS: u64 = 5;

/// Defensive: timeout for any spill-related I/O wrapper future.
/// Held here so the writer task's tokio retry loop can pin a
/// reasonable bound. Currently unused inside this synchronous
/// module; reserved for the writer-task slice.
pub const SEAL_SPILL_IO_TIMEOUT: Duration = Duration::from_secs(SEAL_SPILL_IO_TIMEOUT_SECS);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk_seal(sid: u64, seg: u8, tf: u8, bucket: u32, close: f64) -> SerializedSeal {
        SerializedSeal {
            security_id: sid,
            exchange_segment_code: seg,
            tf_ordinal: tf,
            feed: Feed::Dhan,
            bucket_start_ist_secs: bucket,
            tick_count: 5,
            volume: 1234,
            bucket_start_cumulative: 1000,
            oi: 50_000,
            open: 100.0,
            high: 105.0,
            low: 99.0,
            close,
            close_pct_from_prev_day: 1.5,
            oi_pct_from_prev_day: -0.2,
            volume_pct_from_prev_day: 12.3,
            open_pct: 7.7,
            change_pct: 1.5,
            open_gap_pct: 0.8,
        }
    }

    fn temp_spill_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "tickvault-seal-spill-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");
        dir
    }

    #[test]
    fn test_seal_spill_record_size_is_128() {
        // L-C1 wire format is locked at 128 bytes. Bumping breaks
        // every spilled-but-not-yet-replayed file.
        assert_eq!(SEAL_SPILL_RECORD_SIZE, 128);
    }

    #[test]
    fn test_serialized_seal_in_memory_size_within_record_size() {
        // Pinned by const _ assert above; runtime mirror for grep.
        assert!(std::mem::size_of::<SerializedSeal>() <= SEAL_SPILL_RECORD_SIZE);
    }

    #[test]
    fn test_serialized_seal_to_bytes_roundtrip_preserves_every_field() {
        let original = mk_seal(13, 0, 0, 1_716_000_900, 102.5);
        let bytes = original.to_bytes();
        let decoded = SerializedSeal::from_bytes(&bytes).expect("full record");
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_serialized_seal_to_bytes_handles_negative_oi_and_pct() {
        // i64 OI can be negative for short positions; pct fields can
        // be negative on red days.
        let original = SerializedSeal {
            security_id: 25,
            exchange_segment_code: 1,
            tf_ordinal: 4,
            feed: Feed::Dhan,
            bucket_start_ist_secs: 1_716_001_500,
            tick_count: 0,
            volume: 0,
            bucket_start_cumulative: 0,
            oi: -42_000,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            close_pct_from_prev_day: -3.5,
            oi_pct_from_prev_day: -10.0,
            volume_pct_from_prev_day: -100.0,
            open_pct: -50.0,
            change_pct: -3.5,
            open_gap_pct: -1.2,
        };
        let bytes = original.to_bytes();
        let decoded = SerializedSeal::from_bytes(&bytes).expect("decoded");
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_serialized_seal_change_pct_open_gap_pct_roundtrip_at_bytes_104_120() {
        // Operator request 2026-06-02: the two new pct fields live in the
        // previously-reserved bytes 104..120 and survive a byte round-trip.
        let mut s = mk_seal(7, 1, 0, 1_716_000_900, 100.0);
        s.change_pct = 4.44;
        s.open_gap_pct = -2.22;
        let bytes = s.to_bytes();
        // Verify the exact byte offsets carry the values.
        assert_eq!(
            f64::from_le_bytes(bytes[104..112].try_into().unwrap()),
            4.44
        );
        assert_eq!(
            f64::from_le_bytes(bytes[112..120].try_into().unwrap()),
            -2.22
        );
        let decoded = SerializedSeal::from_bytes(&bytes).expect("decoded");
        assert_eq!(decoded.change_pct, 4.44);
        assert_eq!(decoded.open_gap_pct, -2.22);
    }

    #[test]
    fn test_serialized_seal_pre_2026_06_02_record_decodes_pct_as_zero() {
        // Backward-compat: a record with zeros at bytes 104..120 (older
        // writer) decodes change_pct / open_gap_pct as 0.0, never NaN.
        let mut bytes = mk_seal(7, 1, 0, 1_716_000_900, 100.0).to_bytes();
        for b in bytes.iter_mut().take(120).skip(104) {
            *b = 0;
        }
        let decoded = SerializedSeal::from_bytes(&bytes).expect("decoded");
        assert_eq!(decoded.change_pct, 0.0);
        assert_eq!(decoded.open_gap_pct, 0.0);
        assert!(!decoded.change_pct.is_nan());
        assert!(!decoded.open_gap_pct.is_nan());
    }

    #[test]
    fn test_to_bytes_stamps_format_version_and_roundtrips() {
        // Every freshly-written record carries SEAL_SPILL_FORMAT_VERSION at
        // byte 7 and still decodes through from_bytes (the version byte is
        // a read_all-level gate, not a from_bytes-level one).
        let seal = mk_seal(42, 1, 3, 1_716_000_900, 111.5);
        let bytes = seal.to_bytes();
        assert_eq!(bytes[7], SEAL_SPILL_FORMAT_VERSION);
        let decoded = SerializedSeal::from_bytes(&bytes).expect("decodes");
        assert_eq!(decoded, seal);
    }

    #[test]
    fn test_read_all_refuses_legacy_records_but_drains_v1_siblings() {
        // A daily spill file mixing a pre-renumber legacy record (byte 7
        // == 0 — OLD TfIndex ordinal space) with a current v1 record must
        // drain ONLY the v1 record; the legacy one is refused (never
        // misdecoded into the renumbered ordinal space) and lost with the
        // file when clear_spill_for_date deletes it after drain.
        let dir = temp_spill_dir("legacy-refusal");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = 1_716_000_000_i64;

        // Legacy record: forge byte 7 back to 0 (the pre-C2 padding value).
        let legacy = mk_seal(13, 0, 1, 1_716_000_900, 100.0);
        let mut legacy_bytes = legacy.to_bytes();
        legacy_bytes[7] = 0;

        // Current v1 record (to_bytes stamps the version).
        let v1 = mk_seal(25, 1, 2, 1_716_001_500, 200.75);
        let v1_bytes = v1.to_bytes();

        let path = writer.spill_path(now);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let mut raw = Vec::with_capacity(2 * SEAL_SPILL_RECORD_SIZE);
        raw.extend_from_slice(&legacy_bytes);
        raw.extend_from_slice(&v1_bytes);
        std::fs::write(&path, &raw).expect("write mixed spill file");

        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 1, "only the v1 record must drain");
        assert_eq!(drained[0], v1);

        writer.clear_spill_for_date(now).expect("clear");
        assert!(!path.exists(), "spill file deleted after drain");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_serialized_seal_from_bytes_rejects_truncated_buffer() {
        let short = vec![0u8; SEAL_SPILL_RECORD_SIZE - 1];
        assert_eq!(SerializedSeal::from_bytes(&short), None);
    }

    #[test]
    fn test_serialized_seal_to_bytes_padding_zero_filled() {
        // Byte 6 is the feed index (0 = Dhan here); byte 7 carries the
        // spill format version (C2, 2026-07-21) so legacy pre-renumber
        // records (byte 7 == 0) are refusable on load. §31 Option 2 now
        // uses bytes 96..104 for `open_pct`, so the zero-padding tail
        // starts at 104.
        let seal = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        let bytes = seal.to_bytes();
        assert_eq!(bytes[6], 0, "feed byte must be 0 (Dhan) for mk_seal");
        assert_eq!(
            bytes[7], SEAL_SPILL_FORMAT_VERSION,
            "byte 7 must carry the spill format version"
        );
        // §31: bytes 96..104 carry open_pct (mk_seal sets 7.7 → non-zero).
        assert_ne!(
            &bytes[96..104],
            &[0u8; 8],
            "open_pct bytes 96..104 must be written"
        );
        // 2026-06-02: bytes 104..112 = change_pct (1.5), 112..120 = open_gap_pct
        // (0.8) — both non-zero in mk_seal.
        assert_ne!(
            &bytes[104..112],
            &[0u8; 8],
            "change_pct bytes 104..112 must be written"
        );
        assert_ne!(
            &bytes[112..120],
            &[0u8; 8],
            "open_gap_pct bytes 112..120 must be written"
        );
        // 2026-06-29 u64 widening: bytes 120..128 now carry the FULL u64
        // security_id (so a >u32 Groww id is not lost to the legacy low-32 at
        // bytes 0..4). mk_seal sets security_id=13 → non-zero, round-trips here.
        assert_eq!(
            &bytes[120..128],
            &13_u64.to_le_bytes(),
            "full u64 security_id must be written at bytes 120..128"
        );
        // The 128-byte record is now fully populated — no reserved tail remains.
    }

    #[test]
    fn test_ist_date_filename_handles_ist_offset() {
        // 2026-05-10 00:00:00 IST = 2026-05-09 18:30:00 UTC
        // = unix_secs 1789014600.
        // Verify the helper formats THAT moment as the IST 2026-05-10
        // file (NOT the UTC 2026-05-09 file).
        let ist_midnight_2026_05_10 = 1_778_983_200_i64; // dummy; recompute below
        // Use a known UTC moment instead to keep test independent of
        // distant future dates: 2026-01-01 12:00:00 UTC = 17:30 IST,
        // both calendars agree on 2026-01-01.
        let utc_noon = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let name = ist_date_filename(utc_noon);
        assert_eq!(name, "seals-2026-01-01.bin");
        // Suppress unused
        let _ = ist_midnight_2026_05_10;
    }

    #[test]
    fn test_ist_date_filename_crosses_to_next_day_at_ist_midnight() {
        // 2026-05-09 18:30:00 UTC = 2026-05-10 00:00:00 IST.
        let utc = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 18, 30, 0)
            .single()
            .expect("valid")
            .timestamp();
        let name = ist_date_filename(utc);
        assert_eq!(name, "seals-2026-05-10.bin");
    }

    #[test]
    fn test_seal_spill_writer_new_uses_production_dir() {
        let writer = SealSpillWriter::new();
        assert_eq!(writer.spill_dir, PathBuf::from(SEAL_SPILL_DIR));
    }

    #[test]
    fn test_seal_spill_writer_default_matches_new() {
        let a = SealSpillWriter::default();
        let b = SealSpillWriter::new();
        assert_eq!(a.spill_dir, b.spill_dir);
    }

    #[test]
    fn test_append_seal_then_read_all_roundtrip() {
        let dir = temp_spill_dir("append-then-read");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let s1 = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        let s2 = mk_seal(25, 0, 4, 1_716_001_500, 200.0);
        writer.append_seal(&s1, now).expect("append s1");
        writer.append_seal(&s2, now).expect("append s2");
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0], s1);
        assert_eq!(drained[1], s2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_spill_writer_read_all_on_missing_file_returns_empty() {
        let dir = temp_spill_dir("missing-file");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = chrono::Utc::now().timestamp();
        let drained = writer.read_all(now).expect("ok");
        assert!(drained.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_clear_spill_for_date_removes_file() {
        let dir = temp_spill_dir("clear-removes");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let s1 = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        writer.append_seal(&s1, now).expect("append");
        let path = writer.spill_path(now);
        assert!(path.exists());
        writer.clear_spill_for_date(now).expect("clear");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_clear_spill_for_date_on_missing_file_is_noop() {
        let dir = temp_spill_dir("clear-missing");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = chrono::Utc::now().timestamp();
        // No file written. Clear must succeed.
        writer.clear_spill_for_date(now).expect("idempotent");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_spill_writer_truncated_tail_is_handled_gracefully() {
        // Manually write a truncated record at the tail to simulate a
        // crash mid-flush. The reader must drop the partial record
        // and return everything before it without panic.
        let dir = temp_spill_dir("truncated-tail");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let s1 = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        writer.append_seal(&s1, now).expect("append s1");
        // Manually append a truncated record (50 bytes, less than 128).
        let path = writer.spill_path(now);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open append");
            f.write_all(&[0u8; 50]).expect("partial write");
            f.flush().expect("flush");
        }
        let drained = writer.read_all(now).expect("read");
        // s1 returned; truncated tail dropped without panic.
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0], s1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_spill_path_uses_ist_date_in_filename() {
        let dir = temp_spill_dir("path-ist-date");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let utc_noon = chrono::Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let p = writer.spill_path(utc_noon);
        assert!(p.to_string_lossy().ends_with("seals-2026-05-10.bin"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_spill_writer_handles_many_appends_in_order() {
        let dir = temp_spill_dir("many-appends");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let n = 100;
        for i in 0..n {
            let s = mk_seal(
                13,
                0,
                (i % 9) as u8,
                1_716_000_000 + i as u32,
                100.0 + i as f64,
            );
            writer.append_seal(&s, now).expect("append");
        }
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), n);
        for (i, s) in drained.iter().enumerate() {
            assert_eq!(s.bucket_start_ist_secs, 1_716_000_000 + i as u32);
            assert_eq!(s.close, 100.0 + i as f64);
            assert_eq!(s.tf_ordinal, (i % 9) as u8);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_spill_writer_distinguishes_segments_for_i_p1_11() {
        // Same security_id with different exchange_segment_code must
        // round-trip as two distinct records — no collapse.
        let dir = temp_spill_dir("i-p1-11");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let seg0 = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        let seg1 = mk_seal(13, 1, 0, 1_716_000_900, 200.0);
        writer.append_seal(&seg0, now).expect("append seg0");
        writer.append_seal(&seg1, now).expect("append seg1");
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].exchange_segment_code, 0);
        assert_eq!(drained[1].exchange_segment_code, 1);
        assert_eq!(drained[0].close, 100.0);
        assert_eq!(drained[1].close, 200.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_spill_io_timeout_constant_pinned() {
        assert_eq!(SEAL_SPILL_IO_TIMEOUT, Duration::from_secs(5));
    }

    // -----------------------------------------------------------------------
    // Trading↔storage glue tests (item 1.2c)
    // -----------------------------------------------------------------------

    use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

    fn mk_buffered_seal(sid: u64, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = bucket;
        state.open = 100.0;
        state.high = 105.0;
        state.low = 99.0;
        state.close = close;
        state.volume = 1234;
        state.bucket_start_cumulative = 1000;
        state.oi = 50_000;
        state.tick_count = 5;
        state.close_pct_from_prev_day = 1.5;
        state.oi_pct_from_prev_day = -0.2;
        state.volume_pct_from_prev_day = 12.3;
        BufferedSeal::new(sid, seg, tf, state, Feed::Dhan)
    }

    #[test]
    fn test_from_buffered_seal_copies_every_field_losslessly() {
        let buffered = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 102.5);
        let serialised = SerializedSeal::from(&buffered);
        assert_eq!(serialised.security_id, 13);
        assert_eq!(serialised.exchange_segment_code, 0);
        assert_eq!(serialised.tf_ordinal, 0); // M1.as_ordinal() = 0
        assert_eq!(serialised.bucket_start_ist_secs, 1_716_000_900);
        assert_eq!(serialised.tick_count, 5);
        assert_eq!(serialised.volume, 1234);
        assert_eq!(serialised.bucket_start_cumulative, 1000);
        assert_eq!(serialised.oi, 50_000);
        assert_eq!(serialised.open, 100.0);
        assert_eq!(serialised.high, 105.0);
        assert_eq!(serialised.low, 99.0);
        assert_eq!(serialised.close, 102.5);
        assert_eq!(serialised.close_pct_from_prev_day, 1.5);
        assert_eq!(serialised.oi_pct_from_prev_day, -0.2);
        assert_eq!(serialised.volume_pct_from_prev_day, 12.3);
    }

    #[test]
    fn test_from_buffered_seal_maps_all_twenty_one_tfs_to_correct_ordinal() {
        // Verify every TfIndex variant maps to its canonical ordinal
        // (0..=20: legacy 0..=4 byte-stable, C3 second-scale 5..=20
        // appended — SEAL_SPILL_FORMAT_VERSION stays 1). This pins the
        // trading↔storage contract: a future re-ordering of TfIndex::ALL
        // would silently flip every spilled record's TF assignment.
        let buffered = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let mut tested: Vec<u8> = Vec::with_capacity(TfIndex::ALL.len());
        for tf in TfIndex::ALL {
            let mut b = buffered;
            b.tf = tf;
            let s = SerializedSeal::from(&b);
            assert_eq!(
                s.tf_ordinal as usize,
                tf.as_ordinal(),
                "tf_ordinal mismatch for {}",
                tf.display_name()
            );
            tested.push(s.tf_ordinal);
        }
        let expected: Vec<u8> = (0..TfIndex::ALL.len() as u8).collect();
        assert_eq!(tested, expected);

        // Append-only proof: the 5 LEGACY frames (M1, M3, M5, M15, D1)
        // keep their exact pre-C3 ordinals 0..=4 — the C3 second-scale
        // frames are APPENDED after D1, never interleaved, so a pre-C3
        // spilled record decodes to the SAME frame under the C3 binary
        // (SEAL_SPILL_FORMAT_VERSION stays 1).
        let legacy: [(TfIndex, u8); 5] = [
            (TfIndex::M1, 0),
            (TfIndex::M3, 1),
            (TfIndex::M5, 2),
            (TfIndex::M15, 3),
            (TfIndex::D1, 4),
        ];
        for (tf, ord) in legacy {
            let mut b = buffered;
            b.tf = tf;
            let s = SerializedSeal::from(&b);
            assert_eq!(
                s.tf_ordinal,
                ord,
                "legacy ordinal drift for {} (append-only violated)",
                tf.display_name()
            );
            assert_eq!(
                s.tf(),
                Some(tf),
                "legacy roundtrip for {}",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_serialized_seal_tf_returns_some_for_valid_ordinals() {
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            let mut s =
                SerializedSeal::from(&mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0));
            s.tf_ordinal = idx as u8;
            assert_eq!(s.tf(), Some(*tf));
        }
    }

    #[test]
    fn test_serialized_seal_tf_returns_none_for_out_of_range_ordinal() {
        let mut s =
            SerializedSeal::from(&mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0));
        // 21 timeframes → valid ordinals are 0..=20; the first
        // out-of-range ordinal is `TfIndex::ALL.len()` (= 21). ROLLBACK
        // SAFETY: this clean-refusal arm is the same code shape the older
        // 5-frame binary takes for a C3-written record carrying ordinal
        // >= 5 — refused (skip + warn at the read site), NEVER a panic —
        // which is what lets SEAL_SPILL_FORMAT_VERSION stay 1.
        s.tf_ordinal = TfIndex::ALL.len() as u8; // out of range (21)
        assert_eq!(s.tf(), None);
        s.tf_ordinal = 255;
        assert_eq!(s.tf(), None);
    }

    #[test]
    fn test_try_into_buffered_seal_roundtrip_preserves_every_field() {
        let original = mk_buffered_seal(25, 1, TfIndex::M15, 1_716_001_500, 200.75);
        let serialised = SerializedSeal::from(&original);
        let recovered = serialised
            .try_into_buffered_seal()
            .expect("valid tf_ordinal");
        assert_eq!(recovered.security_id, original.security_id);
        assert_eq!(
            recovered.exchange_segment_code,
            original.exchange_segment_code
        );
        assert_eq!(recovered.tf, original.tf);
        assert_eq!(
            recovered.state.bucket_start_ist_secs,
            original.state.bucket_start_ist_secs
        );
        assert_eq!(recovered.state.open, original.state.open);
        assert_eq!(recovered.state.high, original.state.high);
        assert_eq!(recovered.state.low, original.state.low);
        assert_eq!(recovered.state.close, original.state.close);
        assert_eq!(recovered.state.volume, original.state.volume);
        assert_eq!(
            recovered.state.bucket_start_cumulative,
            original.state.bucket_start_cumulative
        );
        assert_eq!(recovered.state.oi, original.state.oi);
        assert_eq!(recovered.state.tick_count, original.state.tick_count);
        assert_eq!(
            recovered.state.close_pct_from_prev_day,
            original.state.close_pct_from_prev_day
        );
        assert_eq!(
            recovered.state.oi_pct_from_prev_day,
            original.state.oi_pct_from_prev_day
        );
        assert_eq!(
            recovered.state.volume_pct_from_prev_day,
            original.state.volume_pct_from_prev_day
        );
    }

    #[test]
    fn test_try_into_buffered_seal_returns_none_on_unknown_tf_ordinal() {
        let mut s =
            SerializedSeal::from(&mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0));
        s.tf_ordinal = 42; // unknown future TF
        assert!(s.try_into_buffered_seal().is_none());
    }

    #[test]
    fn test_full_roundtrip_buffered_seal_to_bytes_to_buffered_seal() {
        // End-to-end: BufferedSeal -> SerializedSeal -> bytes -> SerializedSeal -> BufferedSeal.
        // The whole chain is what the writer task uses on REPLAY from disk-spill.
        let original = mk_buffered_seal(13, 0, TfIndex::M5, 1_716_000_900, 105.5);
        let serialised = SerializedSeal::from(&original);
        let bytes = serialised.to_bytes();
        let decoded = SerializedSeal::from_bytes(&bytes).expect("full record");
        let recovered = decoded.try_into_buffered_seal().expect("valid tf_ordinal");
        assert_eq!(recovered, original);
    }

    #[test]
    fn test_from_buffered_seal_preserves_i_p1_11_segment_distinction() {
        // Same security_id × 2 segments must produce 2 distinct
        // serialised records that round-trip to 2 distinct buffered
        // seals — the I-P1-11 invariant must hold across the
        // trading↔storage glue.
        let seg0 = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0);
        let seg1 = mk_buffered_seal(13, 1, TfIndex::M1, 1_716_000_900, 200.0);
        let s0 = SerializedSeal::from(&seg0);
        let s1 = SerializedSeal::from(&seg1);
        assert_ne!(s0, s1);
        let r0 = s0.try_into_buffered_seal().expect("valid tf");
        let r1 = s1.try_into_buffered_seal().expect("valid tf");
        assert_ne!(r0, r1);
        assert_eq!(r0.exchange_segment_code, 0);
        assert_eq!(r1.exchange_segment_code, 1);
    }
}

/// C3 phase-B pins: the spill `tf_ordinal` byte round-trips every one of the
/// 21 `TfIndex` frames, the legacy 5 keep ordinals 0..=4, and out-of-range
/// ordinals refuse cleanly (`None`) — never panic.
#[cfg(test)]
mod c3_tf_ordinal_pins {
    use tickvault_trading::candles::TfIndex;
    use tickvault_trading::candles::tf_index::TF_COUNT;

    #[test]
    fn test_tf_ordinal_roundtrip_covers_all_21_frames() {
        assert_eq!(TF_COUNT, 21);
        for ord in 0..=20usize {
            let tf =
                TfIndex::from_ordinal(ord).unwrap_or_else(|| panic!("ordinal {ord} must decode"));
            assert_eq!(tf.as_ordinal(), ord, "round-trip broke at {ord}");
        }
        // The legacy 5-frame set keeps its pre-C3 ordinals 0..=4.
        assert_eq!(TfIndex::M1.as_ordinal(), 0);
        assert_eq!(TfIndex::M3.as_ordinal(), 1);
        assert_eq!(TfIndex::M5.as_ordinal(), 2);
        assert_eq!(TfIndex::M15.as_ordinal(), 3);
        assert_eq!(TfIndex::D1.as_ordinal(), 4);
    }

    #[test]
    fn test_from_ordinal_refuses_21_and_255_without_panic() {
        assert!(TfIndex::from_ordinal(21).is_none());
        assert!(TfIndex::from_ordinal(255).is_none());
        for ord in TF_COUNT..=255usize {
            assert!(TfIndex::from_ordinal(ord).is_none(), "{ord} must refuse");
        }
    }
}
