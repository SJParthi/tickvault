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
//!   - IST-date file rotation (`seals-2026-05-10.bin`), on a LONG-LIVED
//!     handle: the file is opened once per IST day, not once per seal
//!     (2026-08-10 — see [`SealSpillWriter::append_seal`]).
//!   - Idempotent fixed-record append (`O(1)` per append, ONE `write(2)`).
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

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use tracing::{error, info, warn};

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

/// IST calendar-day number (days since the IST-shifted epoch) for a UTC
/// unix timestamp. This is the ROTATION identity: two timestamps share a
/// spill file iff they share this number.
///
/// Deliberately integer-only — O(1), zero allocation, no `chrono` formatting
/// — so the per-seal hot check costs an add + a divide instead of building a
/// `String` filename. Equivalent BY CONSTRUCTION to the day component of
/// [`ist_date_filename`] (which formats the same IST-shifted epoch as a UTC
/// calendar date); pinned by
/// `test_ist_day_number_agrees_with_ist_date_filename_across_boundaries`.
fn ist_day_number(now_unix_secs: i64) -> i64 {
    now_unix_secs
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .div_euclid(86_400)
}

/// The currently-open daily spill file plus the IST day it belongs to.
struct OpenSpillFile {
    ist_day: i64,
    file: File,
}

/// Append-only spill writer. One instance lives in the writer task;
/// `append_seal` is the single producer entry point.
pub struct SealSpillWriter {
    /// Spill directory — production uses `SEAL_SPILL_DIR`; tests
    /// override via `with_spill_dir_for_test`.
    spill_dir: PathBuf,
    /// Long-lived append handle for the current IST day (2026-08-10).
    ///
    /// `Mutex` because `append_seal` takes `&self` (the absorption
    /// pipeline's `escalate_evicted` rescue path is `&self`) yet must mutate
    /// the cached handle. Uncontended: the seal writer task is the single
    /// producer, so this is an uncontended lock/unlock pair, not a wait.
    open: Mutex<Option<OpenSpillFile>>,
    /// Pre-resolved handles for the two `append_seal` failure counters.
    ///
    /// `append_seal` is reachable from the FRAME-DRAIN task: the escalation
    /// offload's `Full`/`Disconnected` arm runs the inline cascade on the
    /// caller (see `SealOverflow::escalate`), and that caller is the per-tick
    /// fold. The bare `metrics::counter!` macro is banned there — it builds a
    /// `Key` and takes a sharded-registry lock per call, where a resolved
    /// handle is a plain atomic add.
    ///
    /// Honest magnitude: both sites are ERROR paths, so in a healthy session
    /// neither fires at all, and when they do the syscall that preceded them
    /// dwarfs the lookup. They are resolved anyway because "a failing disk" is
    /// exactly the state in which every one of these fires per seal, on the
    /// thread that empties the socket — the moment the registry lock is least
    /// affordable.
    err_no_handle: metrics::Counter,
    err_write: metrics::Counter,
}

/// Name of the spill-write failure counter. Both label values are
/// compile-time literals, so the handle set is enumerable up front.
const SPILL_WRITE_ERRORS_COUNTER: &str = "tv_seal_spill_write_errors_total";

impl SealSpillWriter {
    /// Production constructor. Uses `data/spill/`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            spill_dir: PathBuf::from(SEAL_SPILL_DIR),
            open: Mutex::new(None),
            err_no_handle: metrics::counter!(SPILL_WRITE_ERRORS_COUNTER, "stage" => "no_handle"),
            err_write: metrics::counter!(SPILL_WRITE_ERRORS_COUNTER, "stage" => "write"),
        }
    }

    /// The directory this writer appends into.
    ///
    /// Exposed so the absorption pipeline can ask the FILESYSTEM about free
    /// space before it escalates. The pipeline owns that decision because a
    /// refusal here would only redirect the bytes to the DLQ on the SAME
    /// volume — see `SealAbsorptionPipeline::escalate_evicted`.
    #[must_use]
    pub fn spill_dir(&self) -> &std::path::Path {
        &self.spill_dir
    }

    /// Test constructor. Tests pass an isolated `tempdir` to allow
    /// parallel execution.
    #[must_use]
    // TEST-EXEMPT: test-only helper used as construction source by every test in this module (test_append_seal_then_read_all_roundtrip, test_seal_spill_writer_clear_*, test_seal_spill_writer_truncated_tail_*, etc.). Separate name-matched test would be redundant.
    pub fn with_spill_dir_for_test(dir: PathBuf) -> Self {
        Self {
            spill_dir: dir,
            open: Mutex::new(None),
            err_no_handle: metrics::counter!(SPILL_WRITE_ERRORS_COUNTER, "stage" => "no_handle"),
            err_write: metrics::counter!(SPILL_WRITE_ERRORS_COUNTER, "stage" => "write"),
        }
    }

    /// Locks the cached-handle slot, treating a poisoned mutex as the value
    /// it holds. A panic in another thread while holding this lock cannot
    /// leave the spill writer permanently dead — the worst case is a stale
    /// handle, which the day check and the write-error path both correct.
    fn lock_open(&self) -> std::sync::MutexGuard<'_, Option<OpenSpillFile>> {
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Opens (creating as needed) the append handle for `path`.
    ///
    /// Still calls `create_dir_all` — the chaos suite injects "spill disk
    /// dead" by placing a regular FILE at the spill-dir path, which makes
    /// this call fail deterministically on every OS and forces the tier-2 →
    /// tier-3 DLQ escalation (`chaos_seal_disk_full_dlq_capture.rs`). Moving
    /// it off the per-append path did NOT move it off the per-OPEN path.
    fn open_append_handle(&self, path: &Path) -> Result<File> {
        std::fs::create_dir_all(&self.spill_dir)
            .with_context(|| format!("failed to create spill dir {:?}", self.spill_dir))?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open spill file {path:?}"))
    }

    /// Returns the path of the spill file for the given UTC unix
    /// timestamp (used to derive IST date). Pure helper.
    #[must_use]
    pub fn spill_path(&self, now_unix_secs: i64) -> PathBuf {
        self.spill_dir.join(ist_date_filename(now_unix_secs))
    }

    /// Append one serialised seal to the daily spill file.
    /// O(1) work per call and, in steady state, **exactly ONE syscall per
    /// seal** (`write(2)` on a long-lived append handle).
    ///
    /// ## What changed 2026-08-10 (and what deliberately did NOT)
    ///
    /// Before: the `BufWriter` was constructed and dropped INSIDE this
    /// function, so every seal paid `create_dir_all` + `open` + `write_all` +
    /// `flush` (+ the `close` on drop) — 3-4 syscalls, and the buffer
    /// coalesced only the writes of a single 128-byte record, i.e. nothing.
    /// The 2026-08-09 doc correction said so honestly; this is the code fix.
    ///
    /// Now: the file is opened ONCE per IST day and cached
    /// ([`OpenSpillFile`]). `create_dir_all` + `open` + `close` amortise to
    /// once per rotation instead of once per seal.
    ///
    /// **Cross-seal buffering is deliberately NOT introduced**, and that is a
    /// durability decision rather than an oversight. The absorption pipeline
    /// treats an `Ok` here as "tier 2 accepted it" and returns
    /// `SubmitOutcome::Spilled`; an `Err` is what escalates that specific
    /// seal to the tier-3 DLQ. A user-space buffer would (a) make seals that
    /// are reported `Spilled` vanish on a process kill — precisely the
    /// recovery `chaos_seal_sigkill_spill_replay.rs` asserts — and (b) surface
    /// a write failure at flush time, long after the seal that caused it has
    /// been reported absorbed and can no longer be escalated. The
    /// `ws_frame_spill` channel+writer-thread shape can batch because its
    /// producer contract is fire-and-forget (`AppendOutcome::Spilled` means
    /// "queued"); this one's is synchronous and error-returning. Copying the
    /// shape without copying the contract would trade 1 syscall for a silent
    /// loss path.
    ///
    /// LATENCY is still not bounded — filesystem syscalls are not — but this
    /// is tier 2 of the ring → spill → DLQ chain and runs off the socket read
    /// path.
    ///
    /// Per locked decision L-C1, this is the SECOND tier of the
    /// ring → spill → DLQ chain. Failures bubble up to the caller
    /// (writer task) which then escalates to the DLQ tier (next
    /// slice) and on triple failure logs
    /// `error!(code = AGGREGATOR-DROP-01)`.
    pub fn append_seal(&self, seal: &SerializedSeal, now_unix_secs: i64) -> Result<()> {
        let bytes = seal.to_bytes();
        let day = ist_day_number(now_unix_secs);
        let mut open = self.lock_open();

        // Rotate only when the IST day actually changed (or nothing is open
        // yet, incl. after a write error dropped the handle). The check is an
        // integer compare — no filename is built on the steady-state path.
        let stale = match open.as_ref() {
            Some(current) => current.ist_day != day,
            None => true,
        };
        if stale {
            // Close the previous day's handle BEFORE opening the next, so a
            // rotation never holds two descriptors.
            *open = None;
            let path = self.spill_path(now_unix_secs);
            let file = self.open_append_handle(&path)?;
            *open = Some(OpenSpillFile { ist_day: day, file });
        }
        let Some(current) = open.as_mut() else {
            // Structurally unreachable: the branch above either populated the
            // slot or returned Err. Refuse loudly rather than assume.
            self.err_no_handle.increment(1);
            anyhow::bail!(
                "seal spill handle missing after open — refusing to claim a durable write"
            );
        };

        // ONE `write(2)`. The file is unbuffered by design: the previous
        // implementation's `BufWriter::flush()` bought exactly this syscall
        // (a 128-byte record never fills an 8 KiB buffer, so without the
        // flush nothing reached the kernel at all). Writing through keeps the
        // SAME durability contract — once `append_seal` returns `Ok`, the
        // record is in the page cache and survives a process kill, which is
        // what `chaos_seal_sigkill_spill_replay.rs` recovers. Introducing a
        // cross-seal user-space buffer WOULD regress that; see the
        // module-level note on why it is deliberately not done.
        if let Err(err) = current.file.write_all(&bytes) {
            // Drop the possibly-broken handle so the next call reopens —
            // mirrors `ws_frame_spill::persist_record_resilient`. The error
            // still propagates, so the absorption pipeline escalates THIS
            // seal to the tier-3 DLQ exactly as before.
            *open = None;
            self.err_write.increment(1);
            let path = self.spill_path(now_unix_secs);
            return Err(err).with_context(|| format!("failed to write seal to {path:?}"));
        }
        Ok(())
    }

    /// Closes the cached append handle, if any.
    ///
    /// MUST be called whenever the underlying file is unlinked or replaced:
    /// on POSIX a descriptor keeps the removed inode alive, so appending
    /// through a stale handle would write seals into a file no `read_all`
    /// can ever see — a silent loss that the per-call-open version could not
    /// have. Pinned by
    /// `test_append_after_clear_reopens_and_is_visible_to_read_all`.
    fn close_open_handle(&self) {
        *self.lock_open() = None;
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
        // Invalidate FIRST and unconditionally: after the unlink, a cached
        // descriptor would keep appending into an orphaned inode that no
        // `read_all` can reach. Doing it before the `exists()` check also
        // covers the "already gone" path, where a stale handle is just as
        // wrong.
        self.close_open_handle();
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

/// Outcome of one spill-retention sweep.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SpillPruneOutcome {
    /// Spill files deleted (older than the retention window).
    pub deleted: usize,
    /// Of those, how many still held records — i.e. seals that were never
    /// replayed into QuestDB. NON-ZERO IS AN INCIDENT, not routine cleanup.
    pub deleted_non_empty: usize,
    /// Total unreplayed records in the deleted files (bytes / record size).
    pub records_lost: u64,
    /// Files that should have been deleted but could not be.
    pub failed: usize,
    /// Bytes still on disk after the sweep.
    pub bytes_after: u64,
    /// Files skipped because they are TODAY's file — the one the live writer
    /// may hold an open descriptor to. Never deleted at any age.
    pub skipped_live: usize,
}

/// Deletes spill files older than `max_age_secs` — pure-testable core over an
/// injected `now`.
///
/// # Why this exists (2026-08-19)
///
/// `data/spill/` had **no retention of any kind**. `SPILL_FILE_MAX_AGE_SECS`
/// was defined, documented and unit-tested, but a workspace scan found ZERO
/// production consumers, and `clear_spill_for_date` — documented as "called by
/// the writer task after `read_all` is fully replayed" — has zero production
/// callers too. The writer chain only ever appends. So spill files accumulated
/// for the life of the deployment, and a QuestDB outage grew them without any
/// bound at all.
///
/// # Why this deletes LOUDLY, unlike the WAL archive sweep
///
/// The WAL archive holds frames already re-injected and durably persisted —
/// deleting an aged copy loses nothing. Spill is the opposite: it holds seals
/// that have NOT reached QuestDB. Deleting a non-empty spill file destroys
/// data.
///
/// That is why the age window is generous and every non-empty deletion is
/// counted and reported. A spill file a week old that still holds records
/// means the replay path has been broken for a week — an incident that must
/// be surfaced, never a quiet tidy-up.
///
/// The alternative — never deleting — is not the safe choice it appears to be:
/// an unbounded directory fills the volume, and a full volume stops EVERY
/// table on the box, including the live writes these seals would be replayed
/// into. Bounded-and-loud beats unbounded-and-silent.
#[must_use]
pub fn prune_spill_files_at(
    spill_dir: &Path,
    max_age_secs: u64,
    now: std::time::SystemTime,
) -> SpillPruneOutcome {
    let mut outcome = SpillPruneOutcome::default();
    // NEVER delete a file the live writer may hold open (2026-08-19, found by
    // the adversarial audit — this sweep as first written could do exactly
    // that, and the consequence is the worst failure mode in this module).
    //
    // `SealSpillWriter` caches a LONG-LIVED append descriptor. On POSIX,
    // unlinking a file an open descriptor still references keeps the inode
    // alive: the writer goes on appending, `write_all` keeps returning Ok,
    // absorption keeps reporting `Spilled` — and `read_all` opens a fresh
    // empty path that can never see any of it. Seals reported durable would
    // be silently gone. `close_open_handle`'s own doc states this contract
    // and says it MUST be called whenever the file is unlinked; this sweep
    // runs in a different task with no writer reference, so it CANNOT honour
    // that contract and must not create the situation.
    //
    // The writer only ever opens TODAY's file (IST-date rotation), so
    // excluding today's name is a complete defence, and a structural one.
    //
    // Age alone is NOT a defence, which is the part worth stating: it happens
    // to hold today only because the 7-day window exceeds the ~9-hour process
    // lifetime. That is an accidental invariant enforced by nothing — shorten
    // the window, or leave a process running across the weekend, and the
    // sweep starts deleting live files. Two unrelated constants silently
    // holding a correctness property between them is precisely the shape this
    // audit keeps finding.
    let live_name = ist_date_filename(
        now.duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0),
    );
    // O(1) EXEMPT: periodic cold retention sweep, never the per-seal append
    let Ok(entries) = std::fs::read_dir(spill_dir) else {
        return outcome; // missing dir — nothing to prune
    };
    let cutoff = std::time::Duration::from_secs(max_age_secs);
    for entry in entries.flatten() {
        let path = entry.path();
        // Only our own spill records. Anything else in the directory is left
        // strictly alone — deleting a file we did not write, to satisfy our
        // own budget, would be indefensible.
        if path.extension().and_then(|s| s.to_str()) != Some("bin") {
            continue;
        }
        // The live-writer guard. Cheap, and it fails SAFE: an unreadable file
        // name is treated as live and kept, never deleted on uncertainty.
        if path.file_name().and_then(|n| n.to_str()) != Some(live_name.as_str()) {
            // not today's file — eligible, fall through to the age check
        } else {
            outcome.skipped_live += 1;
            if let Ok(meta) = entry.metadata() {
                outcome.bytes_after = outcome.bytes_after.saturating_add(meta.len());
            }
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue; // unreadable metadata — keep, never delete on uncertainty
        };
        let len = meta.len();
        let aged_out = meta
            .modified()
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age > cutoff);
        if !aged_out {
            outcome.bytes_after = outcome.bytes_after.saturating_add(len);
            continue;
        }
        // O(1) EXEMPT: periodic cold retention sweep, never the per-seal append
        match std::fs::remove_file(&path) {
            Ok(()) => {
                outcome.deleted += 1;
                if len > 0 {
                    outcome.deleted_non_empty += 1;
                    outcome.records_lost = outcome
                        .records_lost
                        .saturating_add(len / SEAL_SPILL_RECORD_SIZE as u64);
                }
            }
            Err(err) => {
                outcome.failed += 1;
                outcome.bytes_after = outcome.bytes_after.saturating_add(len);
                warn!(
                    path = %path.display(),
                    error = %err,
                    "spill retention sweep: remove_file failed — retried next pass"
                );
            }
        }
    }
    outcome
}

/// Wall-clock wrapper over [`prune_spill_files_at`]. Cold path — called from
/// the periodic retention task in `main.rs`.
///
/// Reports at `error!` with a code when a deleted file still held records,
/// because that is unreplayed data leaving the box.
// TEST-EXEMPT: thin wall-clock wrapper — all deletion and accounting logic is
// covered by the six spill_sweep_* tests against prune_spill_files_at; this
// layer only supplies SystemTime::now(), emits the coded log and sets the
// gauge. Mirrors the sibling ws_frame_spill::prune_archived_segments wrapper.
#[must_use]
pub fn prune_spill_files(spill_dir: &Path, max_age_secs: u64) -> SpillPruneOutcome {
    let outcome = prune_spill_files_at(spill_dir, max_age_secs, std::time::SystemTime::now());
    if outcome.deleted_non_empty > 0 {
        error!(
            code = "SPILL-RETENTION-01",
            files = outcome.deleted_non_empty,
            records_lost = outcome.records_lost,
            max_age_secs,
            "spill files aged out while STILL HOLDING unreplayed seals — the \
             replay path has been broken for longer than the retention window. \
             This is data loss, reported rather than hidden; investigate why \
             the writer never drained these."
        );
    } else if outcome.deleted > 0 {
        info!(
            deleted = outcome.deleted,
            bytes_after = outcome.bytes_after,
            "spill retention sweep: removed aged empty spill files"
        );
    }
    metrics::gauge!("tv_seal_spill_bytes").set(outcome.bytes_after as f64);
    outcome
}

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

    // -----------------------------------------------------------------
    // 2026-08-10 — long-lived append handle (was 3-4 syscalls per seal).
    // -----------------------------------------------------------------

    #[test]
    fn test_ist_day_number_agrees_with_ist_date_filename_across_boundaries() {
        // The integer day number is the ROTATION identity that replaced
        // building a filename per seal. If it ever disagrees with the
        // filename, seals silently land in the wrong day's file — so pin the
        // equivalence directly: the filename changes EXACTLY when the day
        // number changes, swept minute-by-minute across an IST midnight.
        let ist_midnight_utc = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 18, 30, 0)
            .single()
            .expect("valid")
            .timestamp();
        for offset in -120i64..=120 {
            let a = ist_midnight_utc + offset * 60;
            let b = a + 60;
            assert_eq!(
                ist_day_number(a) == ist_day_number(b),
                ist_date_filename(a) == ist_date_filename(b),
                "day-number and filename disagreed at offset {offset} min"
            );
        }
        // The boundary itself rolls exactly once.
        assert_eq!(
            ist_day_number(ist_midnight_utc) - ist_day_number(ist_midnight_utc - 1),
            1,
            "IST midnight must advance the day number by exactly 1"
        );
        // Pre-epoch timestamps floor correctly (div_euclid, not truncation).
        assert_eq!(
            ist_day_number(-i64::from(IST_UTC_OFFSET_SECONDS)),
            0,
            "the IST-shifted epoch is day 0"
        );
        assert_eq!(
            ist_day_number(-i64::from(IST_UTC_OFFSET_SECONDS) - 1),
            -1,
            "one second earlier is the PREVIOUS day, never day 0"
        );
    }

    #[test]
    fn test_append_seal_is_durable_without_dropping_the_writer() {
        // THE durability pin. The previous implementation flushed inside
        // every call; this one writes through a cached handle. Both must mean
        // the same thing: once `append_seal` returns Ok, the bytes are in the
        // kernel, NOT in a user-space buffer that a SIGKILL would discard.
        //
        // The writer is deliberately NOT dropped and no flush is called
        // before reading — a `Drop`-time flush (which the sigkill chaos test
        // cannot distinguish) would not save a buffered implementation here.
        let dir = temp_spill_dir("durable-no-drop");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let s1 = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        writer.append_seal(&s1, now).expect("append s1");

        let raw = std::fs::read(writer.spill_path(now)).expect("read raw while writer is alive");
        assert_eq!(
            raw.len(),
            SEAL_SPILL_RECORD_SIZE,
            "the record must be on disk BEFORE the writer is dropped"
        );
        assert_eq!(SerializedSeal::from_bytes(&raw), Some(s1));

        // Still true after many appends — nothing accumulates in memory.
        for i in 1..50u32 {
            writer
                .append_seal(
                    &mk_seal(13, 0, 0, 1_716_000_900 + i, 100.0 + f64::from(i)),
                    now,
                )
                .expect("append");
        }
        let raw = std::fs::read(writer.spill_path(now)).expect("read raw");
        assert_eq!(raw.len(), 50 * SEAL_SPILL_RECORD_SIZE);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_append_seal_rotates_the_cached_handle_across_ist_day_boundary() {
        // The cached handle must follow the IST date, not outlive it —
        // otherwise every seal after midnight lands in yesterday's file.
        let dir = temp_spill_dir("day-rotation");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let day_a = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 12, 0, 0) // 17:30 IST, 2026-05-09
            .single()
            .expect("valid")
            .timestamp();
        let day_b = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 19, 0, 0) // 00:30 IST, 2026-05-10
            .single()
            .expect("valid")
            .timestamp();
        assert_ne!(writer.spill_path(day_a), writer.spill_path(day_b));

        let a = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        let b = mk_seal(25, 1, 2, 1_716_001_500, 200.0);
        writer.append_seal(&a, day_a).expect("append day A");
        writer.append_seal(&b, day_b).expect("append day B");

        let drained_a = writer.read_all(day_a).expect("read A");
        let drained_b = writer.read_all(day_b).expect("read B");
        assert_eq!(drained_a, vec![a], "day A file holds ONLY day A's seal");
        assert_eq!(drained_b, vec![b], "day B file holds ONLY day B's seal");

        // Rotating BACK (a late seal stamped with the earlier day) reopens
        // day A rather than appending into day B.
        let late = mk_seal(51, 0, 0, 1_716_002_100, 300.0);
        writer.append_seal(&late, day_a).expect("append late day A");
        assert_eq!(writer.read_all(day_a).expect("read A again"), vec![a, late]);
        assert_eq!(writer.read_all(day_b).expect("read B again"), vec![b]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_append_after_clear_reopens_and_is_visible_to_read_all() {
        // The failure mode a cached descriptor introduces: after
        // `clear_spill_for_date` unlinks the file, POSIX keeps the orphaned
        // inode alive for the open fd. Appending through a stale handle would
        // write seals nobody can ever read back — a SILENT loss the
        // per-call-open version could not produce. The handle must be
        // invalidated at clear time.
        let dir = temp_spill_dir("clear-then-append");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let first = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        writer.append_seal(&first, now).expect("append first");
        assert_eq!(writer.read_all(now).expect("read"), vec![first]);

        writer.clear_spill_for_date(now).expect("clear");
        assert!(!writer.spill_path(now).exists());

        let second = mk_seal(25, 0, 1, 1_716_001_500, 200.0);
        writer
            .append_seal(&second, now)
            .expect("append after clear");
        assert!(
            writer.spill_path(now).exists(),
            "the post-clear append must recreate the file, not write to an orphan"
        );
        assert_eq!(
            writer.read_all(now).expect("read after clear"),
            vec![second],
            "exactly the post-clear seal — no ghost, no resurrection"
        );

        // Repeated clear→append cycles stay correct (idempotent).
        writer.clear_spill_for_date(now).expect("clear 2");
        writer
            .clear_spill_for_date(now)
            .expect("clear 2 idempotent");
        writer.append_seal(&first, now).expect("append 3");
        assert_eq!(writer.read_all(now).expect("read 3"), vec![first]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_append_seal_errors_while_spill_dir_is_unusable_then_self_heals() {
        // Failure injection identical to the chaos suite's "spill disk dead"
        // (`chaos_seal_disk_full_dlq_capture.rs`): a regular FILE occupies the
        // spill-dir path, so `create_dir_all` fails on every OS.
        //
        // Two properties the caching must not break: (1) EVERY append still
        // returns Err — the absorption pipeline needs that to escalate each
        // seal to the tier-3 DLQ, so a cached-handle design must keep RETRYING
        // the open, never fail once and go quiet; (2) once the blocker is
        // removed the very next append succeeds — a dropped handle is
        // re-acquired, not permanently poisoned.
        let base = temp_spill_dir("dir-blocked");
        let blocked = base.join("spill_is_a_file");
        std::fs::write(&blocked, b"not a directory").expect("write blocker");

        let writer = SealSpillWriter::with_spill_dir_for_test(blocked.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let seal = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        for attempt in 0..5 {
            assert!(
                writer.append_seal(&seal, now).is_err(),
                "attempt {attempt} must FAIL so the caller escalates to the DLQ"
            );
        }
        // read_all over a dead spill dir is a clean empty, never a panic.
        assert!(
            writer
                .read_all(now)
                .expect("read_all on dead dir")
                .is_empty()
        );

        // Un-block: the writer recovers on the next call with no restart.
        std::fs::remove_file(&blocked).expect("remove blocker");
        writer
            .append_seal(&seal, now)
            .expect("append after recovery");
        assert_eq!(writer.read_all(now).expect("read"), vec![seal]);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn test_append_seal_is_send_sync_and_serialises_concurrent_appends() {
        // `append_seal` takes `&self` (the absorption pipeline's rescue path
        // is `&self`), so the cached handle sits behind a Mutex. Prove the
        // writer is shareable and that concurrent appends neither interleave
        // within a record nor lose one.
        let dir = temp_spill_dir("concurrent-appends");
        let writer = std::sync::Arc::new(SealSpillWriter::with_spill_dir_for_test(dir.clone()));
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        const THREADS: u32 = 4;
        const PER_THREAD: u32 = 100;
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let writer = std::sync::Arc::clone(&writer);
                scope.spawn(move || {
                    for i in 0..PER_THREAD {
                        let seal = mk_seal(
                            u64::from(t + 1),
                            0,
                            0,
                            1_716_000_000 + i,
                            f64::from(t * PER_THREAD + i),
                        );
                        writer.append_seal(&seal, now).expect("concurrent append");
                    }
                });
            }
        });
        let drained = writer.read_all(now).expect("read");
        assert_eq!(
            drained.len(),
            (THREADS * PER_THREAD) as usize,
            "every concurrently-appended seal must survive, none torn"
        );
        // Every record decoded cleanly (a torn write would break read_all's
        // fixed-size framing and truncate the tail).
        let mut per_thread = [0u32; THREADS as usize];
        for seal in &drained {
            let idx = (seal.security_id - 1) as usize;
            assert!(idx < THREADS as usize, "decoded a garbage security_id");
            per_thread[idx] += 1;
        }
        assert_eq!(per_thread, [PER_THREAD; THREADS as usize]);
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
    // ---- spill retention sweep (2026-08-19) -------------------------------

    fn spill_tmp(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("tv-spill-prune-{name}-{nanos}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    fn write_aged(dir: &Path, name: &str, bytes: usize, age_secs: u64) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![0_u8; bytes]).expect("write");
        let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        let f = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("reopen");
        f.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .expect("set mtime");
        path
    }

    #[test]
    fn spill_sweep_never_unlinks_the_live_writers_file() {
        // THE CRITICAL ONE. The writer holds a long-lived append descriptor to
        // TODAY's file. Unlinking it on POSIX keeps the inode alive, so the
        // writer keeps appending to a file `read_all` can never see — seals
        // reported durable, silently gone.
        //
        // Age is deliberately set to 0 and the file is aged far past it: even
        // then, today's file must survive. If this ever fails, the sweep has
        // gone back to relying on the accidental window-vs-process-lifetime
        // ratio that made it safe by luck rather than by construction.
        let dir = spill_tmp("live");
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let live = ist_date_filename(now_secs);
        let path = write_aged(&dir, &live, SEAL_SPILL_RECORD_SIZE * 4, 10_000_000);
        let out = prune_spill_files_at(&dir, 0, std::time::SystemTime::now());
        assert!(path.exists(), "today's file must NEVER be unlinked");
        assert_eq!(out.deleted, 0);
        assert_eq!(out.skipped_live, 1, "and it must be reported, not silent");
        assert_eq!(out.records_lost, 0, "no loss may be claimed");
    }

    #[test]
    fn spill_sweep_still_deletes_yesterdays_file() {
        // The inverse: the live-writer guard must not accidentally spare
        // EVERY file, which would silently restore the unbounded growth this
        // sweep exists to stop.
        let dir = spill_tmp("yesterday");
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let older = ist_date_filename(now_secs - 3 * 86_400);
        let path = write_aged(&dir, &older, 0, 10_000);
        let out = prune_spill_files_at(&dir, 3_600, std::time::SystemTime::now());
        assert!(!path.exists(), "an old day's file is still eligible");
        assert_eq!(out.deleted, 1);
        assert_eq!(out.skipped_live, 0);
    }

    #[test]
    fn spill_sweep_deletes_only_aged_files() {
        let dir = spill_tmp("aged");
        let old = write_aged(&dir, "seals-20260101.bin", 0, 10_000);
        let fresh = write_aged(&dir, "seals-20260819.bin", 0, 10);
        let out = prune_spill_files_at(&dir, 3_600, std::time::SystemTime::now());
        assert_eq!(out.deleted, 1);
        assert!(!old.exists(), "aged file must go");
        assert!(fresh.exists(), "fresh file must stay");
    }

    #[test]
    fn spill_sweep_counts_unreplayed_records_it_destroys() {
        // The property that matters most: deleting a NON-EMPTY spill file is
        // data loss, and it must be counted and surfaced — never silent.
        let dir = spill_tmp("nonempty");
        write_aged(
            &dir,
            "seals-20260101.bin",
            SEAL_SPILL_RECORD_SIZE * 7,
            10_000,
        );
        let out = prune_spill_files_at(&dir, 3_600, std::time::SystemTime::now());
        assert_eq!(out.deleted, 1);
        assert_eq!(out.deleted_non_empty, 1, "must flag it as non-empty");
        assert_eq!(out.records_lost, 7, "must report the exact record count");
    }

    #[test]
    fn spill_sweep_reports_zero_loss_for_aged_empty_files() {
        // The inverse: an aged EMPTY file is routine cleanup and must NOT be
        // reported as data loss, or the incident signal becomes noise.
        let dir = spill_tmp("empty");
        write_aged(&dir, "seals-20260101.bin", 0, 10_000);
        let out = prune_spill_files_at(&dir, 3_600, std::time::SystemTime::now());
        assert_eq!(out.deleted, 1);
        assert_eq!(out.deleted_non_empty, 0);
        assert_eq!(out.records_lost, 0);
    }

    #[test]
    fn spill_sweep_never_touches_foreign_files() {
        let dir = spill_tmp("foreign");
        let note = write_aged(&dir, "operator-notes.txt", 4096, 10_000);
        let out = prune_spill_files_at(&dir, 3_600, std::time::SystemTime::now());
        assert_eq!(out.deleted, 0);
        assert!(
            note.exists(),
            "a file we did not write is never ours to delete"
        );
    }

    #[test]
    fn spill_sweep_reports_remaining_bytes_and_handles_a_missing_dir() {
        let dir = spill_tmp("bytes");
        write_aged(&dir, "seals-20260819.bin", 512, 10);
        let out = prune_spill_files_at(&dir, 3_600, std::time::SystemTime::now());
        assert_eq!(out.bytes_after, 512, "surviving bytes must be reported");
        let missing = std::env::temp_dir().join("tv-spill-does-not-exist-xyz");
        let out = prune_spill_files_at(&missing, 3_600, std::time::SystemTime::now());
        assert_eq!(out, SpillPruneOutcome::default(), "missing dir is a no-op");
    }

    #[test]
    fn spill_sweep_with_a_zero_window_still_spares_fresh_writes() {
        // Extreme input. Even at max_age 0 the comparison is strictly
        // greater-than, so a file written this instant is not eligible —
        // deleting the file currently being appended would be catastrophic.
        let dir = spill_tmp("zero");
        let now = std::time::SystemTime::now();
        let path = dir.join("seals-20260819.bin");
        std::fs::write(&path, vec![0_u8; 128]).expect("write");
        let f = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        f.set_times(std::fs::FileTimes::new().set_modified(now))
            .expect("mtime");
        let out = prune_spill_files_at(&dir, 0, now);
        assert_eq!(out.deleted, 0, "a file with zero age must survive");
        assert!(path.exists());
    }

    /// The absorption pipeline asks this writer where it writes so it can ask
    /// the filesystem how much room is left. If it ever named a different
    /// directory from the one appends land in, the free-space floor would be
    /// measuring the wrong volume and would read healthy while the real one
    /// filled.
    #[test]
    fn spill_dir_reports_the_directory_appends_actually_land_in() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "tickvault-seal-spill-dir-{}-{}",
            std::process::id(),
            "accessor"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        assert_eq!(writer.spill_dir(), dir.as_path());

        // Non-vacuous: append a seal and confirm the file lands under exactly
        // the directory this accessor names.
        writer
            .append_seal(&mk_seal(13, 0, 1, 100, 24_000.5), 1_777_000_000)
            .expect("append must land");
        let entries: Vec<_> = std::fs::read_dir(writer.spill_dir())
            .expect("spill dir must exist after an append")
            .filter_map(Result::ok)
            .collect();
        assert!(
            !entries.is_empty(),
            "an appended seal must be visible in the directory spill_dir() names"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
    fn test_tf_ordinal_roundtrip_covers_all_frames() {
        // 24 since 2026-08-10 (M2/M30/M60 appended, operator Quote 13).
        // The loop bound is derived from TF_COUNT so an appended frame is
        // actually exercised instead of silently falling outside the range.
        assert_eq!(TF_COUNT, 24);
        for ord in 0..TF_COUNT {
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
    fn test_from_ordinal_refuses_past_the_end_and_255_without_panic() {
        assert!(TfIndex::from_ordinal(TF_COUNT).is_none());
        assert!(TfIndex::from_ordinal(255).is_none());
        for ord in TF_COUNT..=255usize {
            assert!(TfIndex::from_ordinal(ord).is_none(), "{ord} must refuse");
        }
    }
}

/// Source pins for the sites `append_seal` exposes to the frame-drain
/// task through the seal-escalation inline fallback.
#[cfg(test)]
mod per_tick_metrics_pins {
    use super::*;

    /// `append_seal` is reachable from the FRAME-DRAIN task.
    ///
    /// The seal-escalation offload's `Full`/`Disconnected` arm runs the inline
    /// cascade on the CALLER, and that caller is the per-tick fold — so this
    /// function inherits the fold's ban on the bare `metrics::counter!` macro
    /// (a `Key` build plus a sharded-registry lock, versus an atomic add on a
    /// resolved handle). Both sites are error paths, so no behavioural test
    /// can distinguish the two forms; a source scan is the only thing that
    /// holds it.
    #[test]
    fn append_seal_uses_pre_resolved_counter_handles() {
        let src = include_str!("seal_spill.rs");
        let start = src
            .find("pub fn append_seal(&self, seal: &SerializedSeal")
            .expect("append_seal must exist");
        let end = src[start..]
            .find("\n    /// Closes the cached append handle")
            .expect("append_seal must be followed by close_open_handle");
        let body = &src[start..start + end];
        assert!(
            !body.contains("metrics::"),
            "bare metrics macro inside append_seal — it is reachable from the frame-drain \
             task via the escalation fallback; resolve the handle once"
        );
        assert!(
            body.contains("self.err_no_handle.increment(1)")
                && body.contains("self.err_write.increment(1)"),
            "both failure sites must increment a pre-resolved handle"
        );
    }
}
