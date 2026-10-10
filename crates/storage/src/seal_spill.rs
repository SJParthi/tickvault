//! Wave 6 Sub-PR #1 item 1.2b — sealed-candle disk-spill primitive.
//!
//! Mirrors the disk-spill machinery in
//! `crates/storage/src/tick_persistence.rs::TickPersistenceWriter`:
//! when the in-memory ring (`crates/trading/src/candles/seal_ring.rs`,
//! merged via PR #557) overflows, the evicted oldest entries flow
//! through this module and land in
//! `data/spill/seals_v4-YYYY-MM-DD.bin` as fixed-size 128-byte binary
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
//!   - IST-date file rotation (`seals_v4-2026-05-10.bin`), on a LONG-LIVED
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
//! | 0    | 4 | **v5+:** `record_crc32: u32`, CRC-32 (IEEE) of bytes 4..128. **v4 and older:** `security_id` low-32 (legacy/Dhan; full u64 at 120-128) |
//! | 4    | 1 | `exchange_segment_code: u8`     |
//! | 5    | 1 | `tf_ordinal: u8` (0..=8 per `TfIndex` — the operator's nine frames since 2026-09-19; the ordinal space has been renumbered twice, so the byte is meaningless without `format_version`) |
//! | 6    | 1 | `feed_index: u8` (`Feed::index()` — 0=Dhan, 1=Groww; pre-feed records read 0=Dhan) |
//! | 7    | 1 | `format_version: u8` (=5 since 2026-10-01). Every reader accepts `SEAL_SPILL_OLDEST_READABLE_VERSION..=SEAL_SPILL_FORMAT_VERSION` (4..=5) and REFUSES every other version, older or newer: 3 → 4 renumbered `tf_ordinal`, so an older byte decodes into the wrong frame silently. 4 → 5 renumbered nothing, which is why 4 is still read |
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
//! | 80   | 8 | `bucket_open_prev_close: f64` (**v1 and v3+**). ⚠ **v2 wrote `net_volume_signed: i64` here** — read byte 7 before these bytes; see the version notes below |
//! | 88   | 4 | `total_buy_qty: u32`            |
//! | 92   | 4 | `total_sell_qty: u32`           |
//! | 96   | 8 | `open_pct: f64` (§31 Option 2)  |
//! | 104  | 8 | `change_pct: f64` (2026-06-02)  |
//! | 112  | 8 | `open_gap_pct: f64` (2026-06-02)|
//! | 120  | 8 | `security_id: u64` full (2026-06-29; zero in legacy records → low-32 at 0-4) |
//!
//! ## Format version 2 (2026-09-10) — bytes 80..88 change MEANING
//!
//! Version 1 wrote `bucket_open_prev_close: f64` at bytes 80..88. That field
//! was the OLD net-volume sign baseline (the previous sealed bar's close), and
//! the 2026-09-10 tick-rule rewrite stopped reading it: `net_volume` is now
//! accumulated per tick, and `close_pct_from_prev_day` is drawn from
//! `prev_day_close`, never from this field. A reference scan found **no
//! production reader anywhere** — it was written, spilled, and consumed by
//! nothing.
//!
//! Those 8 bytes are therefore reclaimed for `net_volume_signed: i64`, which
//! is what lets a spill-replayed bar report real flow instead of NULL. The
//! record does NOT grow.
//!
//! **Why this needs the version bump, and why the bump is safe.** A v1 record's
//! bytes 80..88 hold an `f64` price; decoding those bits as an `i64` would
//! produce a colossal fabricated net volume (24200.10_f64 reads as
//! 4_673_285_811_324_502_016). The decoder therefore reads byte 7 FIRST: a
//! record below version 2 reports "not classified" (SQL NULL), which is
//! exactly the behaviour those records had when they were written. The load
//! gate refuses only version 0, so v1 records still replay — nothing on disk
//! is orphaned by the bump.
//!
//! `i64::MIN` is the "not classified" sentinel for v2 records. It cannot
//! collide with a real value: `LiveCandleState::net_volume` clamps to
//! `±volume`, so the reachable range is `[-i64::MAX, i64::MAX]` and `i64::MIN`
//! sits strictly outside it.
//!
//! ## Format version 3 (2026-09-18) — bytes 80..88 return to their v1 meaning
//!
//! The operator's 2026-09-18 directive removed `net_volume` from the candle
//! tables entirely: one column, `volume`, carrying the GROSS magnitude with a
//! SIGN derived from `close < bucket_open_prev_close`
//! (`websocket-connection-scope-lock.md`, "2026-09-18 (FOURTH)"). The tick-rule
//! flow value stops being persisted anywhere, so the eight bytes version 2
//! spent on `net_volume_signed` are free again — and the field the new sign
//! needs is `bucket_open_prev_close: f64`, which is **exactly what version 1
//! wrote at that same offset**.
//!
//! So the record returns to its v1 meaning at v3. **No stride change, no
//! growth**: the alternative — appending a field — would have moved every
//! `.bin` file's record boundary, which the "no spare room left" note below
//! spells out.
//!
//! **Why the version bump is load-bearing, and why it is safe.** A v2 record's
//! bytes 80..88 hold an `i64` quantity; decoding those bits as an `f64` yields
//! a denormal or a nonsense magnitude, and feeding that to the sign rule would
//! flip the sign of every replayed bar whose close sits under it. The decoder
//! therefore reads byte 7 FIRST and reads a price only at
//! `SEAL_SPILL_FIRST_PREV_CLOSE_VERSION` or later; anything older decodes
//! `0.0`, which `LiveCandleState::signed_volume()` treats as "no baseline" and
//! reports positive — the same answer those records carried when written. The
//! load gate still refuses only version 0, so v1 and v2 records replay
//! unchanged; nothing on disk is orphaned by the bump.
//!
//! `net_volume_signed` / `net_volume_classified` survive on `LiveCandleState`
//! ALONE — the in-memory accumulator is retained so restoring the flow value
//! later is purely additive. They are gone from BOTH persisted shapes:
//! `SerializedSeal` (the const-assert on `SEAL_SPILL_RECORD_SIZE` forces it —
//! see the RETIRED note at the field site) and `SealDlqRecord`, whose NDJSON
//! carries `bucket_open_prev_close` in their place. A spill-replayed or
//! DLQ-replayed seal therefore decodes `0` / `false` for the flow pair.
//!
//! ## Format version 5 (2026-10-01) — every record carries a checksum
//!
//! Audit PR41c (row 136). Until v5 a record had no way to say it was intact:
//! a flipped bit in a price, a volume or a timeframe byte decoded into a
//! plausible seal and was written to the candle table as if it were true. The
//! record is byte-for-byte full, so the checksum takes the one range that
//! carries nothing of its own: bytes 0..4, the legacy low-32 `security_id`,
//! whose full value has been at bytes 120..128 since 2026-06-29. A v5 record
//! therefore reads its id from 120..128 only, and bytes 0..4 hold a CRC-32
//! (IEEE) of bytes 4..128 — everything else in the record, the version byte
//! included, so a flipped version byte is caught as well.
//!
//! No `tf_ordinal` moved, so a v4 record still decodes exactly as it did and
//! every reader accepts it during the rollout (a spill written by the build
//! before this one is read by this one). A v4 record has no checksum to check;
//! that is the one gap the rollout leaves, and it closes when the last v4 file
//! is archived. A rollback to a v4 build refuses v5 records, counts them and
//! keeps their bytes in `archive/`, the same as any other unknown version.
//! Nothing re-reads `archive/`, so a candle spilled as v5 and drained by a
//! rolled-back build is not written by that build or by a later one; its bytes
//! stay on disk for a manual re-ingest. That is the price of writing the
//! checksum from this release rather than a release later.
//!
//! A record whose checksum does not hold is refused and counted as
//! undecodable, and the reader moves on to the next record: the record
//! boundary is unaffected by what is inside the record, so one bad record
//! costs one seal, never the rest of the file.
//!
//! Total: 128 bytes, and **there is no spare room left**.
//!
//! ⚠ **CORRECTED 2026-09-18 — this paragraph used to read "the trailing 8-byte
//! padding region (bytes 120..128) is reserved for future field additions
//! WITHOUT a file-format break". That is FALSE, and the layout table four
//! paragraphs above is the half that is right:** bytes 120..128 have carried
//! `security_id: u64` since 2026-06-29, when the id was widened past 32 bits.
//! The sentence survived that change unedited, so the module advertised a spare
//! field range that the same module's own table had already assigned.
//!
//! It is stale in the direction that costs work: a reader planning a new field
//! would take the free 8 bytes, find them occupied, and only then discover that
//! any additional field means the RECORD GROWS — which changes the stride of
//! every `.bin` file on disk and is a v3 format break, not the free extension
//! this paragraph promised. That consideration became REAL on 2026-09-18 and
//! is now RESOLVED: the stored close-vs-close volume sign needs the previous
//! bar's close, which is exactly the `bucket_open_prev_close: f64` that v2 had
//! reclaimed at bytes 80..88 — and version 3 takes those bytes back rather
//! than appending, so the record still does not grow. See the v3 note above.
//!
//! What the paragraph got RIGHT and is kept: older records decode cleanly
//! because the ranges added later are zero in them. Pre-§31 records have zero at
//! bytes 96..104, so they decode `open_pct = 0.0`; pre-2026-06-02 records have
//! zero at bytes 104..120, so they decode `change_pct = 0.0` /
//! `open_gap_pct = 0.0`; pre-2026-06-29 records have zero at bytes 120..128, so
//! the decoder falls back to the low-32 id at bytes 0..4 (all
//! backward-compatible).

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use tracing::{error, info, warn};

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_trading::candles::{BufferedSeal, TfIndex};

use crate::raw_frame_upload::CopyGate;
use crate::seal_spill_ledger::{LiveCommit, QueueProgress, SpillLedger, SpillVerdict};

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
/// **Bumped 2 → 3 on 2026-09-18**: bytes 80..88 return to
/// `bucket_open_prev_close: f64` — exactly the meaning version 1 gave them.
/// The 2026-09-18 directive signs the persisted `volume` from this bar's close
/// against the PREVIOUS bar's close of the same timeframe, and that previous
/// close is this field; the tick-rule `net_volume` column it replaces is
/// deleted, so the `i64` v2 put there has no consumer left. A replayed bar
/// without the baseline would report `+gross` for every down bar, which is the
/// defect the bump removes. No stride change and no growth — the record has
/// been byte-for-byte full since 2026-06-29.
///
/// **Bumped 3 → 4 on 2026-09-19** — the SAME class of hazard the C2 note
/// above records, at four times the scale. The operator's 2026-09-18
/// timeframe directive collapsed `TF_COUNT` 24 → 9, and only `M1`..`M15`
/// keep their ordinals: `S1` was 5 and is now **4**, `S5` was 9 and is now
/// **6**, `M30` was 22 and is now **7**, `M60` was 23 and is now **8**. A
/// version-3 record replayed against the new table therefore decodes into
/// the WRONG FRAME **silently** — a byte in range is a byte in range, and
/// nothing downstream can tell an `S1` seal filed as `S3` from a real one.
/// Worse than the C2 case: fifteen frames were RETIRED outright, so a v3
/// record naming one of them would ILP-auto-create a candle table the
/// retired-table sweep has just dropped, with no dedup key. `read_all`
/// therefore refuses EVERY record whose version is not this one (older OR
/// newer — the gate is `!=`, not `<`) rather than trying to
/// recover the byte-stable `M1`..`M15` subset — the C2 precedent refused
/// wholesale for the same reason, and the loss is bounded to one deploy
/// boot's worth of spilled seals.
///
/// ⚠ CORRECTED 2026-09-22: this said task #15's per-window receipt stamps
/// "ride this same bump". They do not — the 128-byte record is full and carries
/// NO receipt field. A seal replayed from spill or the DLQ therefore writes all
/// six delay columns as NULL (never a fabricated zero): the delays of a replayed
/// bar are unknown, and the row says so.
///
/// **Bumped 4 → 5 on 2026-10-01** (audit PR41c): bytes 0..4 carry a CRC-32 of
/// bytes 4..128 instead of the legacy low-32 id. No ordinal moved, so version
/// 4 stays readable — see [`SEAL_SPILL_OLDEST_READABLE_VERSION`].
pub const SEAL_SPILL_FORMAT_VERSION: u8 = 5;

/// Oldest format version every reader still accepts.
///
/// It is the oldest version that shares the CURRENT `tf_ordinal` space. A bump
/// that renumbers `TfIndex` (as 3 → 4 did) must raise this to the new version
/// in the same change, so the older bytes are refused instead of filed under
/// the wrong timeframe; a bump that changes only how a record is checked (as
/// 4 → 5 did) leaves it where it is, so the previous build's spill is read.
pub const SEAL_SPILL_OLDEST_READABLE_VERSION: u8 = 4;

/// First format version whose bytes 0..4 carry `record_crc32` (2026-10-01).
pub const SEAL_SPILL_FIRST_CHECKSUM_VERSION: u8 = 5;

const _: () = assert!(
    SEAL_SPILL_OLDEST_READABLE_VERSION <= SEAL_SPILL_FORMAT_VERSION
        && SEAL_SPILL_FIRST_CHECKSUM_VERSION <= SEAL_SPILL_FORMAT_VERSION,
    "the readable range must include the version this build writes"
);

/// `true` when a spill or dead-letter record stamped `version` can be read by
/// this build. One range check, O(1).
#[must_use]
pub const fn seal_spill_version_is_readable(version: u8) -> bool {
    version >= SEAL_SPILL_OLDEST_READABLE_VERSION && version <= SEAL_SPILL_FORMAT_VERSION
}

/// Re-stamp the checksum of a record a test edited by hand. Test-only.
#[cfg(test)]
pub(crate) fn reseal_record_for_test(buf: &mut [u8; SEAL_SPILL_RECORD_SIZE]) {
    let crc = record_checksum(buf);
    buf[0..4].copy_from_slice(&crc.to_le_bytes());
}

/// What a reader may do with one 128-byte spill record (audit PR41c).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SpillRecordRead {
    /// The record is intact and decodes.
    Seal(SerializedSeal),
    /// Written under a version this build does not read (byte 7).
    OtherVersion,
    /// A v5+ record whose checksum does not match its bytes.
    ChecksumMismatch,
}

/// CRC-32 (IEEE) of bytes 4..128, the value a v5+ record holds at 0..4.
/// O(record size), zero allocation.
fn record_checksum(buf: &[u8; SEAL_SPILL_RECORD_SIZE]) -> u32 {
    crate::wal_applied_watermark::crc32_ieee(&buf[4..])
}

/// `true` when `buf` is a v5+ record whose checksum holds, or an older record
/// that is consistent with itself.
///
/// An older record carries no checksum, but every version-4 writer put the low
/// 32 bits of the id at 0..4 AND the full id at 120..128, so the two must
/// agree. That check is what catches a v5 record whose version byte flipped
/// to 4: its bytes 0..4 hold a checksum, which matches the id's low half only
/// by a 1-in-2^32 chance. A record with a zero full id predates the widening
/// and has nothing to compare.
fn record_checksum_holds(buf: &[u8; SEAL_SPILL_RECORD_SIZE]) -> bool {
    if buf[7] >= SEAL_SPILL_FIRST_CHECKSUM_VERSION {
        return u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) == record_checksum(buf);
    }
    let full_id_low = [buf[120], buf[121], buf[122], buf[123]];
    let full_id_high = [buf[124], buf[125], buf[126], buf[127]];
    (full_id_low == [0; 4] && full_id_high == [0; 4]) || buf[0..4] == full_id_low
}

/// Classify and decode one spill record: the version gate, then the checksum,
/// then the fields. Every spill reader goes through this, so the three cannot
/// disagree about which records they trust. O(record size), zero allocation.
#[must_use]
pub fn decode_spill_record(buf: &[u8; SEAL_SPILL_RECORD_SIZE]) -> SpillRecordRead {
    if !seal_spill_version_is_readable(buf[7]) {
        return SpillRecordRead::OtherVersion;
    }
    match SerializedSeal::from_bytes(buf) {
        Some(seal) => SpillRecordRead::Seal(seal),
        None => SpillRecordRead::ChecksumMismatch,
    }
}

/// ⚠ **RETIRED 2026-09-18.** Bytes 80..88 no longer carry a net volume in any
/// version this writer produces, so nothing reads this sentinel off the wire.
/// It is kept as a named value rather than deleted because a v2 record on disk
/// still HOLDS it, and a future reader that ever wants to interpret those
/// bytes needs to know what they meant. Nothing in the decode path consults it
/// today — see [`SEAL_SPILL_FIRST_PREV_CLOSE_VERSION`].
pub const SEAL_SPILL_NET_VOLUME_UNCLASSIFIED: i64 = i64::MIN;

/// ⚠ **RETIRED 2026-09-18**, for the same reason as the sentinel above: the
/// decoder no longer reads `net_volume_signed` from the binary record at all,
/// in any version. Kept as the record of what version 2 meant.
pub const SEAL_SPILL_FIRST_NET_VOLUME_VERSION: u8 = 2;

/// First format version whose bytes 80..88 carry `bucket_open_prev_close`
/// **again** (version 1 also did; version 2 did not).
///
/// The decoder checks byte 7 against this BEFORE reading byte 80, for the same
/// reason the v2 bump did in the other direction: a v2 record's bytes 80..88
/// hold an `i64` net volume, and reinterpreting those bits as an `f64` price
/// yields a fabricated baseline that would sign real bars the wrong way. A
/// record below this version therefore reports NO baseline (`0.0`), which
/// `LiveCandleState::signed_volume` treats as "nothing to compare against" and
/// signs positive — the documented default, not a guess.
///
/// Version 1 records are NOT read here even though they hold the right field:
/// distinguishing v1 from v2 at byte 7 is exactly what the version byte is
/// for, and a v1 spill is long gone by the time this ships. Treating them as
/// baseline-less costs one session's replayed bars their sign and can never
/// fabricate one.
pub const SEAL_SPILL_FIRST_PREV_CLOSE_VERSION: u8 = 3;

/// Self-contained binary record for spilled sealed bars.
///
/// Field layout matches the wire-format table above. The
/// `tf_ordinal` field is the `TfIndex::as_ordinal()` value (0..=8)
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
    /// The PREVIOUS sealed bar's close for this (instrument, timeframe) — the
    /// baseline the persisted `volume`'s SIGN is measured against. Bytes
    /// 80..88, **format version 3 onward** (and version 1, which held the same
    /// field before version 2 reclaimed the bytes).
    ///
    /// `0.0` means "no baseline", which is a real state and not a defect: the
    /// session's first bucket has no previous bar, and a record written by an
    /// older format carries none. `LiveCandleState::signed_volume` signs those
    /// bars POSITIVE, which is the documented default rather than a claim that
    /// the close rose.
    ///
    /// ⚠ Bytes 80..88 held `net_volume_signed: i64` in version 2. Reading
    /// those bits as an `f64` fabricates a baseline that would sign real bars
    /// the wrong way, which is why [`Self::from_bytes`] checks the version byte
    /// against [`SEAL_SPILL_FIRST_PREV_CLOSE_VERSION`] before this field.
    pub bucket_open_prev_close: f64,
    // RETIRED 2026-09-18: `net_volume_signed: i64` + `net_volume_classified:
    // bool` used to sit here. They are gone from this struct, not merely
    // unwritten, and the reason is a hard constraint rather than tidiness:
    // `SerializedSeal` is const-asserted to fit `SEAL_SPILL_RECORD_SIZE`
    // in memory, and the record was already byte-for-byte full — keeping nine
    // bytes of a field the format can no longer carry, beside the field that
    // replaced it, breaks that assert. The live fold still accumulates flow in
    // memory on `LiveCandleState`; nothing persists it.
    /// Vendor pending buy-order total, bytes 88..92 — the low half of the
    /// 8 bytes vacated by `volume_pct_from_prev_day` (same 2026-05-28 removal,
    /// same always-zero guarantee, same backward compatibility).
    pub total_buy_qty: u32,
    /// Vendor pending sell-order total, bytes 92..96 — the high half.
    pub total_sell_qty: u32,
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
        // Bytes 0..4 are written LAST: from version 5 they hold the checksum
        // of everything else (audit PR41c). The id is at 120..128.
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
        // Bytes 80..88: the bucket's PREVIOUS-BAR CLOSE (v3, 2026-09-18).
        //
        // This is the field version 1 wrote here, returning to its original
        // meaning after version 2 spent the same eight bytes on
        // `net_volume_signed`. The record is byte-for-byte full, so reclaiming
        // the freed field rather than appending is what keeps the stride at
        // `SEAL_SPILL_RECORD_SIZE` and every existing `.bin` on disk readable.
        //
        // It is carried because the persisted `volume` is now SIGNED
        // (`LiveCandleState::signed_volume()`), and the sign is derived from
        // `close < bucket_open_prev_close`. A replayed seal that lost this
        // baseline would report every bar positive — a silently wrong sign on
        // exactly the rows a rescue exists to save.
        buf[80..88].copy_from_slice(&self.bucket_open_prev_close.to_le_bytes());
        buf[88..92].copy_from_slice(&self.total_buy_qty.to_le_bytes());
        buf[92..96].copy_from_slice(&self.total_sell_qty.to_le_bytes());
        // §31 Option 2: open_pct in the first 8 reserved bytes.
        buf[96..104].copy_from_slice(&self.open_pct.to_le_bytes());
        // Operator request 2026-06-02: change_pct + open_gap_pct.
        buf[104..112].copy_from_slice(&self.change_pct.to_le_bytes());
        buf[112..120].copy_from_slice(&self.open_gap_pct.to_le_bytes());
        // bytes 120-128: full u64 security_id (2026-06-29 widening). Reading
        // back: non-zero here → full u64; zero → legacy/Dhan record, fall back
        // to the low-32 at bytes 0-4.
        buf[120..128].copy_from_slice(&self.security_id.to_le_bytes());
        let crc = record_checksum(&buf);
        buf[0..4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Deserialise from a fixed 128-byte little-endian record.
    /// Returns `None` if the buffer is shorter than the record size
    /// (truncated tail) — caller treats this as end-of-file — or if it is a
    /// version-5-or-later record whose checksum does not hold (audit PR41c).
    /// It does NOT apply the version gate; [`decode_spill_record`] does both.
    #[must_use]
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        let buf: &[u8; SEAL_SPILL_RECORD_SIZE] =
            buf.get(..SEAL_SPILL_RECORD_SIZE)?.try_into().ok()?;
        if !record_checksum_holds(buf) {
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
        // Bytes 80..88 mean three different things across the three formats,
        // so the VERSION byte is read before them. This is the only field in
        // the record whose decode is not positional, and getting it wrong is
        // not a small error in either direction:
        //
        //   v1  `bucket_open_prev_close: f64`  — a price
        //   v2  `net_volume_signed: i64`       — a signed quantity (RETIRED)
        //   v3  `bucket_open_prev_close: f64`  — a price again
        //
        // A v2 `i64` reinterpreted as an `f64` is a denormal or a nonsense
        // magnitude, and feeding it to the sign rule would flip the sign of
        // every replayed bar whose close sits under it. So only a record that
        // says v3 or later is read as a price; anything older reports 0.0,
        // which `signed_volume()` treats as "no baseline" and reports the bar
        // positive — the same answer that record carried when it was written.
        let prev_close = if buf[7] >= SEAL_SPILL_FIRST_PREV_CLOSE_VERSION {
            f64::from_le_bytes([
                buf[80], buf[81], buf[82], buf[83], buf[84], buf[85], buf[86], buf[87],
            ])
        } else {
            0.0
        };

        // Full u64 security_id from the reserved 120-128 region (2026-06-29
        // widening). A legacy/Dhan record has zero there → fall back to the
        // low-32 at bytes 0-4 (Dhan ids fit u32; security_id is never 0).
        let security_id_full = u64::from_le_bytes([
            buf[120], buf[121], buf[122], buf[123], buf[124], buf[125], buf[126], buf[127],
        ]);
        // From version 5 bytes 0..4 are the checksum, so the full id is the
        // only id the record holds.
        let security_id = if security_id_full != 0 || buf[7] >= SEAL_SPILL_FIRST_CHECKSUM_VERSION {
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
            // Bytes 80..88, decoded above: the previous-bar close (v3+), or
            // 0.0 for an older record. See the version table there.
            bucket_open_prev_close: prev_close,
            total_buy_qty: u32::from_le_bytes([buf[88], buf[89], buf[90], buf[91]]),
            total_sell_qty: u32::from_le_bytes([buf[92], buf[93], buf[94], buf[95]]),
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
    /// Field-by-field copy. The seal-time stamped fields
    /// (`close_pct_from_prev_day` / `total_buy_qty` / `total_sell_qty`)
    /// are carried through unchanged —
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
            // The baseline the SIGN of the persisted volume is derived from.
            // Carried since v3 (2026-09-18) so a replayed bar reports the same
            // sign it would have reported live.
            bucket_open_prev_close: b.state.bucket_open_prev_close,
            total_buy_qty: b.state.total_buy_qty,
            total_sell_qty: b.state.total_sell_qty,
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
        // The baseline `signed_volume()` reads. Restoring it is what makes a
        // replayed bar carry the same sign the live fold would have given it;
        // a v1/v2 record decodes 0.0 here, which reports the bar positive —
        // the same answer it carried when it was written.
        state.bucket_open_prev_close = self.bucket_open_prev_close;
        // RETIRED at v3 (2026-09-18): `net_volume_signed` /
        // `net_volume_classified` are NOT restored here, because
        // `SerializedSeal` no longer carries them. A replayed seal therefore
        // keeps `LiveCandleState`'s own defaults (0 / false) — the same answer
        // the pre-v2 records gave, and the only honest one now that no format
        // on disk records the value.
        state.total_buy_qty = self.total_buy_qty;
        state.total_sell_qty = self.total_sell_qty;
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

/// The day file name the live spill writer opens at `now_unix_secs` (UTC):
/// the one file in the spill root that may be held open, so neither the
/// retention sweep nor the cold-bucket uploader touches it. Cold path.
#[must_use]
pub fn live_spill_file_name(now_unix_secs: i64) -> String {
    ist_date_filename(now_unix_secs)
}

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
    dt.format("seals_v4-%Y-%m-%d.bin").to_string()
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
/// Syncs a directory, so the entries created in it (a new day file, a new
/// sub-directory) survive a power cut. Cold: once per created file, off any
/// append lock.
pub(crate) fn sync_directory(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

pub(crate) fn ist_day_number(now_unix_secs: i64) -> i64 {
    now_unix_secs
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .div_euclid(86_400)
}

/// The currently-open daily spill file plus the IST day it belongs to.
struct OpenSpillFile {
    ist_day: i64,
    file: File,
}

/// Everything the append lock guards (audit PR41a added the ledger and the
/// mirror buffer to the handle it always guarded).
struct SpillState {
    /// Long-lived append handle for the current IST day (2026-08-10).
    open: Option<OpenSpillFile>,
    /// The previous IST day's handle, closed by a rotation and not yet
    /// synced. The rotation only moves it here (the append path never waits
    /// for the device); the escalation thread syncs and drops it in
    /// [`SealSpillWriter::sync_open_file`]. At most one: a second rotation
    /// before any sync closes the older one unsynced, which is the same
    /// exposure the open file has until its own first sync.
    rotated: Option<File>,
    /// The fullest copy of each bar on disk and in the database. See
    /// [`crate::seal_spill_ledger`].
    ledger: SpillLedger,
    /// Reused buffers for the live copies appended by
    /// [`SealSpillWriter::note_live_commits`]: the bytes written, and the
    /// copies to record once the write succeeded.
    mirror_scratch: Vec<u8>,
    mirror_seals: Vec<SerializedSeal>,
    /// Spill epoch (Z6): every append records the current one, and staging
    /// the live files for the mid-session replay closes it. Starts at 1.
    epoch: u32,
    /// The newest epoch whose every append is in a staged file: set when a
    /// staging moved every live file. `0` until then.
    staged_through: u32,
}

/// Bars the spill ledger tracks at once (Z6: one entry per bar, `(slot,
/// bucket)`, no longer one per slot). The aggregator's slot ceiling times
/// the timeframe count, i.e. one whole round of every slot's bars. Entries are
/// forgotten once the mid-session replay has consumed their copies; past this
/// bound the ledger fails toward writing data (see the ledger's module docs).
/// About 30 MiB of address space, allocated once per writer; only the control
/// bytes (~512 KiB) are touched until a seal is spilled.
pub const SEAL_SPILL_LEDGER_CAPACITY: usize = tickvault_trading::candles::SEAL_BUFFER_CAPACITY;

/// Bars the boot drain records at most (Z6). Grown on demand, once per boot,
/// only when files are staged. Past it a recovered copy is written untracked
/// and counted (`boot_untracked`).
pub const SEAL_BOOT_WRITTEN_CAPACITY: usize = 4 * SEAL_SPILL_LEDGER_CAPACITY;

/// Counter for the spill ledger (audit PR41a, Z6). One series per `kind`:
/// `mirrored` (a fuller live copy appended after an older copy on disk),
/// `mirrored_overflow` (a live copy appended because the ledger was full and
/// could not tell), `older_not_written` (an older copy of a bar the spill or
/// the database already holds a fuller copy of), `replay_older_skipped` (the
/// mid-session replay dropped an older copy), `mirror_failed` (a live copy
/// could not be appended; a later replay may write the older copy over it)
/// and `untracked` (written past the ledger's capacity).
pub const SEAL_SPILL_SUPERSEDED_COUNTER: &str = "tv_seal_spill_superseded_total";

/// Pre-resolved handles for [`SEAL_SPILL_SUPERSEDED_COUNTER`]. `untracked`
/// and `older_not_written` are reachable from the frame drain's inline
/// fallback, where the counter macro is banned.
struct SupersededCounters {
    mirrored: metrics::Counter,
    mirrored_overflow: metrics::Counter,
    older_not_written: metrics::Counter,
    replay_older_skipped: metrics::Counter,
    mirror_failed: metrics::Counter,
    untracked: metrics::Counter,
}

impl SupersededCounters {
    fn resolve() -> Self {
        Self {
            mirrored: metrics::counter!(SEAL_SPILL_SUPERSEDED_COUNTER, "kind" => "mirrored"),
            mirrored_overflow: metrics::counter!(
                SEAL_SPILL_SUPERSEDED_COUNTER,
                "kind" => "mirrored_overflow"
            ),
            older_not_written: metrics::counter!(
                SEAL_SPILL_SUPERSEDED_COUNTER,
                "kind" => "older_not_written"
            ),
            replay_older_skipped: metrics::counter!(
                SEAL_SPILL_SUPERSEDED_COUNTER,
                "kind" => "replay_older_skipped"
            ),
            mirror_failed: metrics::counter!(
                SEAL_SPILL_SUPERSEDED_COUNTER,
                "kind" => "mirror_failed"
            ),
            untracked: metrics::counter!(SEAL_SPILL_SUPERSEDED_COUNTER, "kind" => "untracked"),
        }
    }
}

/// Append-only spill writer. One instance lives in the writer task;
/// `append_seal` is the single producer entry point.
pub struct SealSpillWriter {
    /// Spill directory — production uses `SEAL_SPILL_DIR`; tests
    /// override via `with_spill_dir_for_test`.
    spill_dir: PathBuf,
    /// The append handle, the spill ledger and the mirror buffer.
    ///
    /// `Mutex` because `append_seal` takes `&self` (the absorption
    /// pipeline's `escalate_evicted` rescue path is `&self`) yet must mutate
    /// the cached handle. Uncontended: the seal writer task is the single
    /// producer, so this is an uncontended lock/unlock pair, not a wait.
    state: Mutex<SpillState>,
    /// `true` while the ledger tracks anything or has overflowed. Read
    /// without the lock, so a writer cycle with nothing spilled never takes it.
    ledger_active: AtomicBool,
    /// Seals queued to the escalation thread and not yet finished with (Z6:
    /// owned here so every escalator over this spill shares one count, and
    /// the ledger can tell whether an older copy may still be on its way).
    escalation_pending: Arc<AtomicUsize>,
    /// Seals the escalation thread has finished with (written, sent to the
    /// DLQ, or reported lost). Raised BEFORE `escalation_pending` is lowered,
    /// so `pending + finished`, read in that order, never undercounts.
    escalation_finished: AtomicU64,
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
    /// Pre-resolved handle for a failed `sync_data` (PR17). Only the
    /// escalation thread syncs, but the handle is resolved once like the two
    /// above rather than per failure.
    err_sync: metrics::Counter,
    /// `true` while the last sync of the open day file failed, so a failing
    /// disk logs one `error!` per episode rather than one per sync (PR17).
    sync_failing: AtomicBool,
    /// A day file (or the spill directory itself) was created and its
    /// directory entry is not synced yet. Set by `open_append_handle`
    /// (one `fstat` per open, no sync), cleared by `sync_open_file`, which
    /// syncs the directory off the lock (2026-10-04: a new day file's entry
    /// was never synced, so a power cut could lose the whole file even after
    /// its data was synced).
    dir_unsynced: AtomicBool,
    /// The open day file was appended to since the last sync. Set under the
    /// append lock by `current_file`, taken by `sync_open_file`, so a writer
    /// that calls `sync_open_file` every cycle (the seal writer task, after
    /// an outage spilled) syncs only when there is something to sync.
    data_unsynced: AtomicBool,
    /// The spill directory itself was created: its parent's entry too.
    parent_unsynced: AtomicBool,
    superseded: SupersededCounters,
}

/// Name of the spill-write failure counter. Both label values are
/// compile-time literals, so the handle set is enumerable up front.
const SPILL_WRITE_ERRORS_COUNTER: &str = "tv_seal_spill_write_errors_total";

/// Failed `sync_data` calls on the seal spill day file (PR17). A failure
/// means seals the spill reported written may be only in the page cache.
pub const SEAL_SPILL_SYNC_FAILED_COUNTER: &str = "tv_seal_spill_sync_failed_total";

impl SealSpillWriter {
    /// Production constructor. Uses `data/spill/`.
    #[must_use]
    pub fn new() -> Self {
        Self::in_dir(PathBuf::from(SEAL_SPILL_DIR))
    }

    fn in_dir(spill_dir: PathBuf) -> Self {
        Self {
            spill_dir,
            state: Mutex::new(SpillState {
                open: None,
                rotated: None,
                ledger: SpillLedger::with_capacity(SEAL_SPILL_LEDGER_CAPACITY),
                mirror_scratch: Vec::new(),
                mirror_seals: Vec::new(),
                epoch: 1,
                staged_through: 0,
            }),
            ledger_active: AtomicBool::new(false),
            escalation_pending: Arc::new(AtomicUsize::new(0)),
            escalation_finished: AtomicU64::new(0),
            err_no_handle: metrics::counter!(SPILL_WRITE_ERRORS_COUNTER, "stage" => "no_handle"),
            err_write: metrics::counter!(SPILL_WRITE_ERRORS_COUNTER, "stage" => "write"),
            err_sync: metrics::counter!(SEAL_SPILL_SYNC_FAILED_COUNTER),
            sync_failing: AtomicBool::new(false),
            data_unsynced: AtomicBool::new(false),
            dir_unsynced: AtomicBool::new(false),
            parent_unsynced: AtomicBool::new(false),
            superseded: SupersededCounters::resolve(),
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
        Self::in_dir(dir)
    }

    /// Locks the append state, treating a poisoned mutex as the value it
    /// holds. A panic in another thread while holding this lock cannot
    /// leave the spill writer permanently dead — the worst case is a stale
    /// handle, which the day check and the write-error path both correct.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, SpillState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Syncs the open day file to the device (PR17: the seal spill was never
    /// synced, so a power loss could take seals it had reported written).
    ///
    /// Called by the escalation thread (`tv-seal-escalate`) after it writes,
    /// and by the seal writer task after every cycle (2026-10-04: seals it
    /// spilled during a database outage were never synced), never by
    /// `append_seal`, which the frame drain's inline fallback can reach. The
    /// lock is held only to take the previous day's handle a rotation left
    /// behind, to duplicate the open one (`try_clone`, one `dup`) when it was
    /// appended to since the last sync, and to take the directory flags,
    /// never across a sync, so an append is never made to wait for the
    /// device. Then the directory entries of a day file or spill directory
    /// created since the last call are synced too. Nothing pending is one
    /// lock and three atomic swaps.
    ///
    /// Returns `true` when there was nothing to sync or every sync succeeded.
    /// A failure is counted on `tv_seal_spill_sync_failed_total` and logged
    /// once per failing episode.
    pub(crate) fn sync_open_file(&self) -> bool {
        let (rotated, handle, dir, parent) = {
            let mut state = self.lock_state();
            let rotated = state.rotated.take();
            // The flags are set under this lock (by `current_file` and the
            // `open_append_handle` it calls), so taking them here pairs each
            // with the handle it was set for. A file with no append since the
            // last sync is not synced again.
            let handle = if self.data_unsynced.swap(false, Ordering::Relaxed) {
                state.open.as_ref().map(|open| open.file.try_clone())
            } else {
                None
            };
            let dir = self.dir_unsynced.swap(false, Ordering::Relaxed);
            let parent = self.parent_unsynced.swap(false, Ordering::Relaxed);
            (rotated, handle, dir, parent)
        };
        if rotated.is_none() && handle.is_none() && !dir && !parent {
            return true;
        }
        // The previous day's file first: its last seals are the oldest
        // unsynced ones. It is dropped (closed) here, off the lock.
        let rotated_outcome = rotated.map_or(Ok(()), |file| file.sync_data());
        let open_outcome = handle
            .map_or(Ok(()), |file| file.and_then(|file| file.sync_data()))
            .inspect_err(|_| {
                self.data_unsynced.store(true, Ordering::Relaxed);
            });
        // Then the directory entries of a file (or directory) created since
        // the last call. A failure is put back for the next call.
        let dir_outcome = if dir {
            sync_directory(&self.spill_dir).inspect_err(|_| {
                self.dir_unsynced.store(true, Ordering::Relaxed);
            })
        } else {
            Ok(())
        };
        let parent_outcome = match self.spill_dir.parent() {
            Some(parent_dir) if parent => sync_directory(parent_dir).inspect_err(|_| {
                self.parent_unsynced.store(true, Ordering::Relaxed);
            }),
            _ => Ok(()),
        };
        let outcome = rotated_outcome
            .and(open_outcome)
            .and(dir_outcome)
            .and(parent_outcome);
        match outcome {
            Ok(()) => {
                self.sync_failing.store(false, Ordering::Relaxed);
                true
            }
            Err(err) => {
                self.err_sync.increment(1);
                if !self.sync_failing.swap(true, Ordering::Relaxed) {
                    error!(
                        code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                        spill_dir = ?self.spill_dir,
                        ?err,
                        "seal spill: syncing the day file failed; seals reported written may \
                         be lost on a power cut until a sync succeeds"
                    );
                }
                false
            }
        }
    }

    /// Test probe: is anything written or created since the last sync?
    #[cfg(test)]
    // TEST-EXEMPT: test-only probe, exercised by the sync tests
    pub(crate) fn has_unsynced_writes(&self) -> bool {
        let state = self.lock_state();
        state.rotated.is_some()
            || self.data_unsynced.load(Ordering::Relaxed)
            || self.dir_unsynced.load(Ordering::Relaxed)
            || self.parent_unsynced.load(Ordering::Relaxed)
    }

    /// Opens (creating as needed) the append handle for `path`.
    ///
    /// Still calls `create_dir_all` — the chaos suite injects "spill disk
    /// dead" by placing a regular FILE at the spill-dir path, which makes
    /// this call fail deterministically on every OS and forces the tier-2 →
    /// tier-3 DLQ escalation (`chaos_seal_disk_full_dlq_capture.rs`). Moving
    /// it off the per-append path did NOT move it off the per-OPEN path.
    fn open_append_handle(&self, path: &Path) -> Result<File> {
        if !self.spill_dir.is_dir() {
            self.parent_unsynced.store(true, Ordering::Relaxed);
        }
        std::fs::create_dir_all(&self.spill_dir)
            .with_context(|| format!("failed to create spill dir {:?}", self.spill_dir))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open spill file {path:?}"))?;
        // An empty file was (almost always) just created, so its directory
        // entry must reach the device too. A false positive costs one extra
        // directory sync. Once per open (once per IST day), never per seal.
        if file.metadata().map_or(true, |meta| meta.len() == 0) {
            self.dir_unsynced.store(true, Ordering::Relaxed);
        }
        Ok(file)
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
        let mut state = self.lock_state();
        let SpillState {
            open,
            rotated,
            ledger,
            epoch,
            ..
        } = &mut *state;

        // Audit PR41a, Z6: the spill or the database already holds a fuller
        // copy of this bar, so writing this one would let a replay put the
        // older copy back over it. The fuller copy is on disk or committed,
        // which is what `Ok` promises.
        let verdict = ledger.verdict(seal);
        if verdict == SpillVerdict::OlderNotWritten {
            self.superseded.older_not_written.increment(1);
            return Ok(());
        }

        // Rotate only when the IST day actually changed (or nothing is open
        // yet, incl. after a write error dropped the handle). The check is an
        // integer compare — no filename is built on the steady-state path.
        let current = self.current_file(open, rotated, now_unix_secs)?;

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
            // Audit PR41c: a failed write may have left part of this record
            // behind. Cut the file back to its last whole record BEFORE
            // dropping the handle, as the batch path does, so the next record
            // starts on its 128-byte boundary. Error path only: the steady
            // state pays nothing for this.
            let cut = cut_back_to_whole_records(&current.file);
            // Drop the possibly-broken handle so the next call reopens —
            // mirrors `ws_frame_spill::persist_record_resilient`. The error
            // still propagates, so the absorption pipeline escalates THIS
            // seal to the tier-3 DLQ exactly as before.
            *open = None;
            self.err_write.increment(1);
            let path = self.spill_path(now_unix_secs);
            return match cut {
                Ok(_) => Err(err).with_context(|| format!("failed to write seal to {path:?}")),
                Err(cut_err) => {
                    let aside = set_aside_torn_file(&path);
                    Err(err).with_context(|| {
                        format!(
                            "failed to write seal to {path:?}, cutting the torn tail back ALSO \
                             failed ({cut_err}); file set aside: {aside:?}"
                        )
                    })
                }
            };
        }
        self.note_spilled(ledger, seal, verdict, *epoch);
        Ok(())
    }

    /// The append handle for the IST day of `now_unix_secs`, opening it when
    /// the day changed or nothing is open. Shared by every append path.
    fn current_file<'s>(
        &self,
        open: &'s mut Option<OpenSpillFile>,
        rotated: &mut Option<File>,
        now_unix_secs: i64,
    ) -> Result<&'s mut OpenSpillFile> {
        let day = ist_day_number(now_unix_secs);
        if open.as_ref().is_none_or(|current| current.ist_day != day) {
            // PR17: the previous day's last seals must reach the device, but
            // this runs under the append lock and can be reached from the
            // drain's inline fallback, so it never syncs here. The handle is
            // moved to `rotated` (no syscall) and the escalation thread syncs
            // and closes it off the lock in `sync_open_file`. Until then the
            // rotation holds two descriptors.
            if let Some(previous) = open.take() {
                *rotated = Some(previous.file);
            }
            let path = self.spill_path(now_unix_secs);
            let mut file = self.open_append_handle(&path)?;
            // Audit PR41c: a file left with a torn tail (a crash mid-write,
            // or the third failure in a row on the write paths) would put
            // every record appended now off its 128-byte boundary, and every
            // reader would refuse them all. Checked once per open.
            match cut_back_to_whole_records(&file) {
                Ok(0) => {}
                Ok(cut) => warn!(
                    ?path,
                    cut_bytes = cut,
                    "seal spill: cut a torn partial record off the end of the day file before \
                     appending"
                ),
                Err(err) => {
                    drop(file);
                    match set_aside_torn_file(&path) {
                        Ok(aside) => {
                            warn!(
                                ?path,
                                ?err,
                                ?aside,
                                "seal spill: the day file ends mid-record and cannot be cut \
                                 back; set aside, appending to a fresh file"
                            );
                            file = self.open_append_handle(&path)?;
                        }
                        Err(aside_err) => {
                            // Appending to this file would put every record
                            // off its boundary and report it written. Refuse,
                            // so each seal escalates to the dead-letter tier.
                            self.err_write.increment(1);
                            error!(
                                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                                ?path,
                                ?err,
                                ?aside_err,
                                "seal spill: the day file ends mid-record, cannot be cut back \
                                 and cannot be set aside; refusing to append to it"
                            );
                            anyhow::bail!(
                                "seal spill file {path:?} ends mid-record and can be neither cut \
                                 back ({err}) nor set aside ({aside_err})"
                            );
                        }
                    }
                }
            }
            *open = Some(OpenSpillFile { ist_day: day, file });
        }
        match open.as_mut() {
            Some(current) => {
                // Every append goes through here, under the append lock: mark
                // the open file as holding bytes the next `sync_open_file` must
                // sync. One relaxed store, no syscall.
                self.data_unsynced.store(true, Ordering::Relaxed);
                Ok(current)
            }
            None => {
                // Structurally unreachable: the branch above either populated
                // the slot or returned Err. Refuse loudly rather than assume.
                self.err_no_handle.increment(1);
                anyhow::bail!(
                    "seal spill handle missing after open — refusing to claim a durable write"
                )
            }
        }
    }

    /// Record a copy the spill now holds (audit PR41a, Z6). O(1): one hash
    /// probe and two atomic loads.
    fn note_spilled(
        &self,
        ledger: &mut SpillLedger,
        seal: &SerializedSeal,
        verdict: SpillVerdict,
        epoch: u32,
    ) {
        if verdict == SpillVerdict::OlderNotWritten {
            return;
        }
        let (_, mark) = self.queue_progress();
        if !ledger.record_on_disk(seal, epoch, false, false, mark) {
            self.superseded.untracked.increment(1);
        }
        self.ledger_active
            .store(ledger.is_active(), Ordering::Release);
    }

    /// Escalation-queue progress now, and the mark an entry updated now must
    /// wait for before it may be forgotten (Z6).
    ///
    /// `pending` is read BEFORE `finished`, and the escalation thread raises
    /// `finished` before it lowers `pending`, so a seal moving from one to the
    /// other between the two loads is counted twice, never zero times: the
    /// mark can only be late, which keeps an entry longer, never shorter.
    ///
    /// # Complexity
    /// Two atomic loads.
    fn queue_progress(&self) -> (QueueProgress, u64) {
        let pending = self.escalation_pending.load(Ordering::SeqCst);
        let finished = self.escalation_finished.load(Ordering::SeqCst);
        let progress = QueueProgress {
            written: finished,
            idle: pending == 0,
        };
        let pending = u64::try_from(pending).unwrap_or(u64::MAX);
        (progress, finished.saturating_add(pending))
    }

    /// The count of seals queued to the escalation thread and not yet
    /// finished with. Every escalator over this spill shares it (Z6), so the
    /// ledger always sees the queue the escalation thread drains.
    #[must_use]
    pub(crate) fn escalation_pending(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.escalation_pending)
    }

    /// The escalation thread finished with `count` queued seals (written to
    /// the spill or the DLQ, or reported lost). Raises the finished count
    /// first, then lowers the pending one; see [`Self::queue_progress`].
    ///
    /// # Complexity
    /// Two atomic read-modify-writes. Escalation thread only.
    pub(crate) fn note_escalation_finished(&self, count: usize) {
        self.escalation_finished
            .fetch_add(u64::try_from(count).unwrap_or(u64::MAX), Ordering::SeqCst);
        self.escalation_pending.fetch_sub(count, Ordering::SeqCst);
    }

    /// Z6: a copy went to the dead-letter file instead of the spill. Recorded
    /// so a fuller copy committed later is mirrored to the spill, and the
    /// boot drain, which keeps the fullest copy of each bar across the spill
    /// and dead-letter files, ends on it.
    ///
    /// # Complexity
    /// O(1): the append lock, one hash probe and two atomic loads. Reached on
    /// the dead-letter path only, which has already paid a file write.
    pub(crate) fn note_dead_lettered(&self, seal: &SerializedSeal) {
        let mut state = self.lock_state();
        let SpillState { ledger, epoch, .. } = &mut *state;
        let (_, mark) = self.queue_progress();
        if !ledger.record_on_disk(seal, *epoch, true, false, mark) {
            self.superseded.untracked.increment(1);
        }
        self.ledger_active
            .store(ledger.is_active(), Ordering::Release);
    }

    /// Append several serialised seals with ONE `write(2)` (audit PR15).
    ///
    /// The escalation thread's batch path. Every seal in `seals` is filed
    /// under the IST day of `now_unix_secs`, exactly as [`Self::append_seal`]
    /// would file each one; the caller groups by day before calling.
    ///
    /// `scratch` is the caller's reusable byte buffer, so a steady stream of
    /// batches allocates nothing after the first one of the largest size.
    ///
    /// # Failure is all-or-nothing, as far as the filesystem allows
    ///
    /// A failed `write_all` may have written part of the batch. The file is
    /// then cut back to the length it had before the write, so the caller can
    /// escalate every seal of the batch one by one without writing any of
    /// them twice and, more importantly, without leaving a torn record that
    /// would shift every later record off its 128-byte boundary. Readers stop
    /// only at a SHORT read, so a torn record in the middle of a file would
    /// make every record after it unreadable, not just itself.
    ///
    /// If the cut ALSO fails, the file is renamed aside (`<name>.<n>`, a name
    /// both drains still read) with the handle already closed, so the tear is
    /// the last thing in that file and reads as its end; the next append
    /// starts a fresh file. If the rename fails too, the torn tail stays in
    /// the day file until the next open of it, which cuts it back before the
    /// first append (audit PR41c); this batch path also checks the length it
    /// starts from and cuts a partial record off first.
    ///
    /// # Complexity
    /// O(seals) to serialise, one `write(2)` in steady state, plus one
    /// `fstat` for the rollback length. Runs on the `tv-seal-escalate`
    /// thread, never on the frame drain.
    pub fn append_seals<'a, I>(
        &self,
        seals: I,
        scratch: &mut Vec<u8>,
        now_unix_secs: i64,
    ) -> Result<()>
    where
        I: IntoIterator<Item = &'a SerializedSeal>,
        I::IntoIter: Clone,
    {
        let seals = seals.into_iter();
        let mut state = self.lock_state();
        let SpillState {
            open,
            rotated,
            ledger,
            epoch,
            ..
        } = &mut *state;

        // Audit PR41a: serialised under the lock, because which copies are
        // written depends on what the spill already holds. A copy of a bucket
        // the spill holds a fuller copy of is left out.
        scratch.clear();
        for seal in seals.clone() {
            if ledger.verdict(seal) == SpillVerdict::OlderNotWritten {
                self.superseded.older_not_written.increment(1);
                continue;
            }
            scratch.extend_from_slice(&seal.to_bytes());
        }
        if scratch.is_empty() {
            return Ok(());
        }
        let current = self.current_file(open, rotated, now_unix_secs)?;
        let before = match current.file.metadata() {
            // Audit PR41c: the batch starts on a record boundary. The day
            // file was cut back when it was opened and every failed write
            // cuts its own tail, so this is the guard for a case those miss;
            // the cut-back below then restores this length, never a torn one.
            Ok(meta) if meta.len() % SEAL_SPILL_RECORD_SIZE as u64 != 0 => {
                match cut_back_to_whole_records(&current.file) {
                    Ok(cut) => meta.len().saturating_sub(cut),
                    Err(err) => {
                        *open = None;
                        self.err_write.increment(1);
                        let path = self.spill_path(now_unix_secs);
                        let aside = set_aside_torn_file(&path);
                        return Err(err).with_context(|| {
                            format!(
                                "seal spill file {path:?} ends mid-record and cannot be cut \
                                 back; set aside ({aside:?}) before the batch"
                            )
                        });
                    }
                }
            }
            Ok(meta) => meta.len(),
            Err(err) => {
                *open = None;
                self.err_write.increment(1);
                let path = self.spill_path(now_unix_secs);
                return Err(err).with_context(|| format!("failed to stat spill file {path:?}"));
            }
        };
        if let Err(err) = current.file.write_all(scratch) {
            // Cut the torn tail before dropping the handle. Best effort: a
            // failure here is reported in the error chain, not swallowed.
            let cut = current.file.set_len(before);
            *open = None;
            self.err_write.increment(1);
            let path = self.spill_path(now_unix_secs);
            return match cut {
                Ok(()) => Err(err).with_context(|| {
                    format!("failed to write a seal batch to {path:?} (torn tail cut back)")
                }),
                Err(cut_err) => {
                    // The torn tail stays, and every record appended after it
                    // would sit off its 128-byte boundary: readers would
                    // refuse all of them as if they were another format. Move
                    // the file out of rotation instead, under a name both
                    // drains still read, so the records before the tear are
                    // recovered, the tear ends that file, and the next append
                    // starts a clean one. The handle is already closed (held
                    // lock, `*open = None` above), so nothing follows the move.
                    let aside = set_aside_torn_file(&path);
                    Err(err).with_context(|| {
                        format!(
                            "failed to write a seal batch to {path:?}, cutting the torn tail \
                             back ALSO failed ({cut_err}); file set aside: {aside:?}"
                        )
                    })
                }
            };
        }
        // Every copy left out above is less full than the one the ledger
        // holds, so recording it changes nothing.
        for seal in seals {
            let verdict = ledger.verdict(seal);
            self.note_spilled(ledger, seal, verdict, *epoch);
        }
        Ok(())
    }

    /// Audit PR41a, Z6: record what the live writer committed, and append to
    /// the spill every committed copy that is fuller than a copy of the same
    /// bar already on disk (spill or dead-letter), so the boot drain, which
    /// keeps the fullest copy of each bar, ends on it.
    ///
    /// While the escalation queue holds anything, a committed copy of a bar
    /// with no entry is recorded too, so an older copy still in the queue is
    /// refused when it reaches the spill. While the ledger is overflowed,
    /// every committed copy of an untracked bar is appended, because an older
    /// copy may be on disk untracked.
    ///
    /// Called after a live flush succeeded, with the seals it committed.
    /// Returns how many copies were appended.
    ///
    /// # Complexity
    /// Two atomic loads when the ledger is empty and the queue idle (the
    /// steady state). Otherwise O(committed): at most two hash probes per
    /// seal, and one `write(2)` when any copy is appended. Runs on the seal
    /// writer task, never on the frame drain. No allocation after the mirror
    /// buffers have grown to the largest batch.
    pub fn note_live_commits(&self, committed: &[BufferedSeal], now_unix_secs: i64) -> usize {
        if committed.is_empty() {
            return 0;
        }
        let queue_busy = self.escalation_pending.load(Ordering::SeqCst) > 0;
        if !queue_busy && !self.ledger_active.load(Ordering::Acquire) {
            return 0;
        }
        let (_, mark) = self.queue_progress();
        let mut state = self.lock_state();
        let SpillState {
            open,
            rotated,
            ledger,
            mirror_scratch,
            mirror_seals,
            epoch,
            ..
        } = &mut *state;
        mirror_scratch.clear();
        mirror_seals.clear();
        let mut overflow_mirrored = 0usize;
        for seal in committed {
            let serialized = SerializedSeal::from(seal);
            match ledger.on_live_commit(&serialized, queue_busy, *epoch, mark) {
                LiveCommit::Nothing => continue,
                LiveCommit::Mirror => {}
                LiveCommit::MirrorOverflow => overflow_mirrored += 1,
            }
            mirror_scratch.extend_from_slice(&serialized.to_bytes());
            mirror_seals.push(serialized);
        }
        self.ledger_active
            .store(ledger.is_active(), Ordering::Release);
        let appended = mirror_seals.len();
        if appended == 0 {
            return 0;
        }
        let written = self
            .current_file(open, rotated, now_unix_secs)
            .and_then(|current| {
                current
                    .file
                    .write_all(mirror_scratch)
                    .context("failed to append fuller live copies to the seal spill")
            });
        if let Err(err) = written {
            // The handle may be broken; the next append reopens it, and the
            // open cuts a torn tail back to a whole record (audit PR41c).
            *open = None;
            self.err_write.increment(1);
            self.superseded
                .mirror_failed
                .increment(u64::try_from(appended).unwrap_or(u64::MAX));
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?err,
                copies = appended,
                "seal spill: a fuller live copy of a spilled candle could not be appended — \
                 a later replay of the spill may write the older copy over the stored row"
            );
            return 0;
        }
        for serialized in mirror_seals.iter() {
            if !ledger.record_on_disk(serialized, *epoch, false, true, mark) {
                self.superseded.untracked.increment(1);
            }
        }
        self.ledger_active
            .store(ledger.is_active(), Ordering::Release);
        self.superseded.mirrored.increment(
            u64::try_from(appended.saturating_sub(overflow_mirrored)).unwrap_or(u64::MAX),
        );
        self.superseded
            .mirrored_overflow
            .increment(u64::try_from(overflow_mirrored).unwrap_or(u64::MAX));
        appended
    }

    /// Audit PR41a, Z6: `true` when `seal`, read back from the spill, is less
    /// full than a copy of the same bar already committed (live, or by an
    /// earlier step of this replay). The mid-session replay drops it (and
    /// counts it here), so a parked file resumed late can never put an older
    /// copy back over a fuller one, whatever order the files replay in.
    ///
    /// # Complexity
    /// One atomic load when the ledger is empty, otherwise one hash probe
    /// under the append lock.
    pub fn replay_is_superseded(&self, seal: &BufferedSeal) -> bool {
        if !self.ledger_active.load(Ordering::Acquire) {
            return false;
        }
        let older = self
            .lock_state()
            .ledger
            .replay_is_older(&SerializedSeal::from(seal));
        if older {
            self.superseded.replay_older_skipped.increment(1);
        }
        older
    }

    /// Z6: the mid-session replay committed `seals`. Recorded as committed,
    /// so an older copy of the same bar replayed after them is dropped.
    ///
    /// # Complexity
    /// One atomic load when the ledger is empty, otherwise O(seals): one hash
    /// probe each under the append lock. Seal writer task, cold.
    pub fn note_replay_commits(&self, seals: &[BufferedSeal]) {
        if seals.is_empty() || !self.ledger_active.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.lock_state();
        for seal in seals {
            state.ledger.on_replay_commit(&SerializedSeal::from(seal));
        }
    }

    /// Z6: the mid-session replay has consumed every spill file staged up to
    /// the last clean staging. Forget every bar whose copies can no longer be
    /// written (see [`SpillLedger::forget_replayed`]). Returns how many bars
    /// were forgotten.
    ///
    /// # Complexity
    /// O(ledger capacity) under the append lock: one pass over the map. Seal
    /// writer task, at most once per replay scan (30 s); every appender,
    /// including the frame drain's inline fallback, waits for it.
    pub fn forget_replayed(&self) -> usize {
        if !self.ledger_active.load(Ordering::Acquire) {
            return 0;
        }
        let mut state = self.lock_state();
        let through = state.staged_through;
        if through == 0 {
            return 0;
        }
        let (progress, _) = self.queue_progress();
        let forgotten = state.ledger.forget_replayed(through, progress);
        self.ledger_active
            .store(state.ledger.is_active(), Ordering::Release);
        forgotten
    }

    /// Run `f` while no append can reach the spill file (audit PR15).
    ///
    /// Holds the append lock for the duration of `f` and closes the cached
    /// handle first, so `f` may rename or move the day's file: the next
    /// append reopens by NAME and creates a fresh file, and no append can land
    /// in the moved inode. This is what lets the mid-session replay take the
    /// live file without losing a seal appended at the same instant.
    ///
    /// `f` returns its result and whether it moved EVERY live spill file
    /// (Z6). The pause closes the spill epoch: every append made before it
    /// carries this epoch or an older one, and every append after it a newer
    /// one. When `f` moved every live file, the epoch is recorded as staged,
    /// and [`Self::forget_replayed`] may forget its copies once the replay has
    /// consumed the staged files.
    ///
    /// Every appender — the escalation thread, the drain's inline fallback
    /// and the writer's own rescue — waits on this lock while `f` runs, so
    /// `f` must be short: a directory listing and a few renames.
    pub fn with_appends_paused<R>(&self, f: impl FnOnce() -> (R, bool)) -> R {
        let mut state = self.lock_state();
        state.open = None;
        let (result, moved_every_live_file) = f();
        let closed = state.epoch;
        state.epoch = closed.saturating_add(1);
        if moved_every_live_file {
            state.staged_through = closed;
        }
        drop(state);
        result
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
        self.lock_state().open = None;
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
        let mut checksum_refused: usize = 0;
        let mut buf = [0u8; SEAL_SPILL_RECORD_SIZE];
        loop {
            match read_full_record(&mut reader, &mut buf) {
                Ok(true) => {
                    // Format-version gate. Byte 7 is the record's
                    // `SEAL_SPILL_FORMAT_VERSION`, and ANY value other than the live
                    // constant is refused — never partially recovered. `!=`, not `<` (2026-09-22):
                    // a NEWER record is equally unreadable, and a deploy rollback makes one.
                    //
                    // 2026-07-21 (C2, version 0 → 1): a byte-7 of 0 marks a
                    // pre-renumber record whose tf_ordinal lives in the OLD 12-frame
                    // ordinal space (old M2=1 would misdecode as new M3=1).
                    //
                    // 2026-09-19 (version 3 → 4): the operator's timeframe directive
                    // collapsed `TF_COUNT` 24 → 9 and only `M1`..`M15` kept their
                    // ordinals — `S1` moved 5 → 4, `S5` 9 → 6, `M30` 22 → 7, `M60`
                    // 23 → 8 — so a version-3 record decodes into the WRONG FRAME
                    // silently, a byte in range being a byte in range. Fifteen
                    // frames were RETIRED outright, so such a record can also name
                    // a table the retired-table sweep has just dropped, which ILP
                    // would auto-create with no dedup key. Hence WHOLESALE refusal
                    // rather than recovering the byte-stable `M1`..`M15` subset.
                    //
                    // Refuse the record, keep draining — a daily file can
                    // legitimately mix older + current records via append across a
                    // deploy boundary. The loss is bounded to one deploy boot's
                    // worth of spilled seals.
                    //
                    // 2026-10-01 (version 4 → 5, audit PR41c): no ordinal moved,
                    // so the gate is the readable RANGE, 4..=5, not one value.
                    // A v5 record whose checksum fails is refused and the read
                    // goes on: the boundary does not depend on the contents.
                    match decode_spill_record(&buf) {
                        SpillRecordRead::Seal(seal) => all.push(seal),
                        SpillRecordRead::OtherVersion => legacy_refused += 1,
                        SpillRecordRead::ChecksumMismatch => checksum_refused += 1,
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
                current_format_version = SEAL_SPILL_FORMAT_VERSION,
                "refused spill records written under a DIFFERENT format version, \
                 older or newer (their tf_ordinal belongs to another TfIndex \
                 ordinal space, so decoding them would file a seal under the \
                 wrong timeframe; deleted with the file after drain)"
            );
        }
        if checksum_refused > 0 {
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?path,
                checksum_refused,
                "refused spill records whose checksum does not match their bytes \
                 (damaged on disk); each refused record is one candle not replayed"
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

/// Cut `file` back to its last whole 128-byte record and return how many
/// bytes were cut (0 when it already ends on a boundary). One `fstat`, plus
/// one `ftruncate` only when there is a partial record to remove. The bytes
/// removed are part of ONE record that was never written whole, so no
/// complete record is ever cut (audit PR41c).
fn cut_back_to_whole_records(file: &File) -> std::io::Result<u64> {
    let len = file.metadata()?.len();
    let torn = len % SEAL_SPILL_RECORD_SIZE as u64;
    if torn != 0 {
        file.set_len(len - torn)?;
    }
    Ok(torn)
}

/// Move a spill file whose tail is torn to `<name>.<n>` in the same
/// directory, the collision-suffix form the boot drain and the mid-session
/// replay both read as a spill file (audit PR15).
///
/// Returns the new path, or the rename error. Bounded at 10,000 probes, the
/// same bound as the drain's own collision naming.
fn set_aside_torn_file(path: &Path) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    for suffix in 1..10_000u32 {
        let candidate = dir.join(format!("{name}.{suffix}"));
        if !candidate.exists() {
            std::fs::rename(path, &candidate)?;
            return Ok(candidate);
        }
    }
    Err(std::io::Error::other(
        "no free set-aside name for a torn spill file",
    ))
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
    /// Aged files deleted from `archive/` (audit PR40b). Counted apart from
    /// [`Self::deleted`], which covers the top level and `replaying/`, where a
    /// deleted record was never re-ingested.
    ///
    /// NOT "no loss" (corrected 2026-10-02): `archive/` also holds files the
    /// replay finished WITHOUT ingesting every record — undecodable records
    /// and seals skipped as unrecovered stay there, and those files are their
    /// only copy. A delete is lossless only with a verified cold copy; see
    /// [`Self::archive_deleted_without_copy`].
    /// Since PR40b-f (2026-10-04) a file with such a record moves to
    /// `refused/` instead and is never deleted; files `archive/` held before
    /// that keep the rule above.
    pub archive_deleted: usize,
    /// Of every delete this sweep (any folder), how many had a verified copy
    /// in the cold bucket (`[raw_frame_archive] require_upload_before_prune`
    /// on). Those lose nothing.
    pub deleted_with_copy: usize,
    /// Non-empty `archive/` files deleted with NO verified copy (only
    /// possible with the copy gate off). May hold undecodable or skipped
    /// seals; reported as possible loss.
    pub archive_deleted_without_copy: usize,
    /// Aged files the sweep would have deleted but KEPT because no verified
    /// cold copy is recorded for them yet (operator Quotes 27 + 28). Counted
    /// as `tv_seal_spill_prune_refused_not_uploaded_total`.
    pub refused_not_uploaded: usize,
    /// Bytes held by `refused_not_uploaded`.
    pub refused_not_uploaded_bytes: u64,
    /// Aged files at the top level or in `replaying/` KEPT because this
    /// process has not yet run the boot drain over this spill directory
    /// (PR40b-f). Before that, an aged file is a file the replay has not
    /// tried yet, not a file it failed to drain.
    pub held_before_boot_drain: usize,
    /// Files in `refused/` (PR40b-f): never deleted at any age, because they
    /// hold seals a recovery path gave up on and are those seals' only copy.
    /// Their bytes are in `bytes_after`.
    pub refused_kept: usize,
}

/// Gauge: files in the spill folder's `refused/` (PR40b-f). They hold seals
/// the boot drain or the replay gave up on and are never deleted; each was
/// already paged when it was moved there. Set by every retention sweep.
pub const SEAL_SPILL_REFUSED_FILES_GAUGE: &str = "tv_seal_spill_refused_files";

/// Counter: aged seal-spill files the retention sweep KEPT because no
/// verified cold copy is recorded for them (plan item 45e-1). The sweep logs
/// one coded STORAGE-GAP-04 line per pass that refuses any.
pub const SEAL_SPILL_PRUNE_REFUSED_NOT_UPLOADED_COUNTER: &str =
    "tv_seal_spill_prune_refused_not_uploaded_total";

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
///
/// # The copy gate (plan item 45e-1, operator Quotes 27 + 28)
///
/// With `gate` = [`CopyGate::Required`] an aged file is deleted only when its
/// marker in `<its folder>/uploaded/` records its current length and mtime —
/// i.e. a verified gzip copy sits in the cold bucket under `seal-spill/`.
/// Otherwise it is kept and counted in `refused_not_uploaded`; the uploader
/// copies it on its next pass and the following sweep deletes it.
#[must_use]
pub fn prune_spill_files_at(
    spill_dir: &Path,
    max_age_secs: u64,
    now: std::time::SystemTime,
    gate: CopyGate,
) -> SpillPruneOutcome {
    prune_spill_files_inner(spill_dir, max_age_secs, now, gate, false)
}

/// [`prune_spill_files_at`] with the PR40b-f boot-drain hold: when
/// `hold_unreplayed` is set, aged files at the top level and in `replaying/`
/// are kept and counted in `held_before_boot_drain` instead of deleted.
fn prune_spill_files_inner(
    spill_dir: &Path,
    max_age_secs: u64,
    now: std::time::SystemTime,
    gate: CopyGate,
    hold_unreplayed: bool,
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
    let cutoff = std::time::Duration::from_secs(max_age_secs);
    prune_dir(
        spill_dir,
        Some(live_name.as_str()),
        PrunedKind::Unreplayed,
        cutoff,
        now,
        gate,
        hold_unreplayed,
        &mut outcome,
    );
    // Audit PR40b: the two folders the replay moves files into. Until then
    // neither was pruned, so every file ever staged or archived stayed on the
    // volume for good, outside both this sweep and the `tv_seal_spill_bytes`
    // figure. `replaying/` holds staged files NOT yet re-ingested, so an aged
    // one there is unreplayed data exactly like an aged top-level file and is
    // counted as lost. `archive/` holds files the replay finished with: their
    // seals are in QuestDB, or were skipped (undecodable, or paged as
    // `seal_unrecovered`) — and for those skipped seals the archived file is
    // the only copy, so an archive delete is lossless only with a verified
    // cold copy (2026-10-02).
    //
    // No live-writer guard below: the writer only ever opens TODAY's file at
    // the top level, and a file moved into either folder was already closed
    // by `with_appends_paused`.
    prune_dir(
        &spill_dir.join(crate::seal_writer_task::SEAL_REPLAYING_SUBDIR),
        None,
        PrunedKind::Unreplayed,
        cutoff,
        now,
        gate,
        hold_unreplayed,
        &mut outcome,
    );
    prune_dir(
        &spill_dir.join(crate::seal_writer_task::SEAL_ARCHIVE_SUBDIR),
        None,
        PrunedKind::Archived,
        cutoff,
        now,
        gate,
        false,
        &mut outcome,
    );
    // PR40b-f: `refused/` is never pruned, at any age and whatever the copy
    // gate says. It is only measured, so `tv_seal_spill_bytes` sees it.
    count_kept_refused(
        &spill_dir.join(crate::seal_writer_task::SEAL_REFUSED_SUBDIR),
        &mut outcome,
    );
    outcome
}

/// Counts the files in `refused/` and their bytes, deleting nothing
/// (PR40b-f). O(files), on the cold retention sweep.
fn count_kept_refused(dir: &Path, outcome: &mut SpillPruneOutcome) {
    // O(1) EXEMPT: periodic cold retention sweep, never the per-seal append
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // missing dir: nothing was ever refused
    };
    for entry in entries.flatten() {
        let is_record = entry.file_name().to_str().is_some_and(is_seal_file_name);
        if !is_record {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            outcome.refused_kept += 1;
            outcome.bytes_after = outcome.bytes_after.saturating_add(meta.len());
        }
    }
}

/// `name` without the copy suffixes the spill tier adds when it renames a
/// file: every trailing `.<digits>` (`set_aside_torn_file`, and `free_path`
/// on a name collision, which can stack: `.bin.1.1`) and `.overflow`
/// (`free_path`'s last resort). PR40b-f. O(suffixes), cold.
#[must_use]
pub(crate) fn strip_copy_suffixes(name: &str) -> &str {
    let mut base = name;
    while let Some((head, tail)) = base.rsplit_once('.') {
        let is_copy_suffix =
            tail == "overflow" || (!tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()));
        if !is_copy_suffix {
            break;
        }
        base = head;
    }
    base
}

/// `true` for a seal spill record file name: `*.bin`, or a renamed copy of
/// one (`*.bin.<digits>`, `*.bin.overflow`, stacked; see
/// [`strip_copy_suffixes`]). PR40b-f: the prune, the cold uploader and the
/// replay staging use the same suffix rule, so they agree on which files
/// hold spilled seals; until 2026-10-04 the first two matched `*.bin` only.
#[must_use]
pub(crate) fn is_spill_record_name(name: &str) -> bool {
    strip_copy_suffixes(name).ends_with(".bin")
}

/// Seal spill files still waiting to reach QuestDB for one IST day, as the
/// 1-minute cross-check's readiness wait reads them (plan item 51d).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpillStaged {
    /// Record files for the day at the spill top level (written, not yet
    /// staged) or in `replaying/` (staged, not yet confirmed and archived).
    pub staged: usize,
    /// Files the mid-session replay holds parked in this process, any day
    /// (it keeps no per-day count).
    pub parked: usize,
}

/// Counts the seal spill record files for `date` (IST) under `dir`: the top
/// level and `replaying/`, never `archive/` or `refused/` (plan item 51d).
/// The day comes from the file name (`seals_v4-YYYY-MM-DD.bin` and its
/// renamed copies, [`is_spill_record_name`]). A missing folder is no files.
///
/// **Not specific to `candles_1m`:** a spill file holds sealed bars of every
/// timeframe, so a staged 5-minute bar holds a 1-minute reader back too.
///
/// # Errors
/// A folder that exists and cannot be listed.
///
/// # Complexity
/// O(files in the two folders), cold: once per readiness check.
pub fn staged_spill_records_for_day(
    dir: &Path,
    date: chrono::NaiveDate,
) -> std::io::Result<SpillStaged> {
    let want = format!(
        "{}{}.bin",
        crate::seal_writer_task::SEAL_FILE_PREFIX,
        date.format("%Y-%m-%d")
    );
    let mut staged = 0_usize;
    for folder in [
        dir.to_path_buf(),
        dir.join(crate::seal_writer_task::SEAL_REPLAYING_SUBDIR),
    ] {
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if is_spill_record_name(name)
                && strip_copy_suffixes(name) == want
                && entry.file_type().is_ok_and(|t| t.is_file())
            {
                staged = staged.saturating_add(1);
            }
        }
    }
    Ok(SpillStaged {
        staged,
        parked: crate::seal_writer_task::parked_replay_files(),
    })
}

/// [`staged_spill_records_for_day`] over the production seal spill folder.
///
/// # Errors
/// As [`staged_spill_records_for_day`].
pub fn staged_production_spill_records_for_day(
    date: chrono::NaiveDate,
) -> std::io::Result<SpillStaged> {
    staged_spill_records_for_day(&crate::seal_writer_runner::production_spill_dir(), date)
}

/// A spill record file, or a staged dead-letter copy (`seals_v4-*.ndjson`,
/// legacy `seals-*.ndjson`, and their renamed copies), which `archive/` and
/// `refused/` can hold: the boot drain and the replay move a staged file
/// there whatever its kind. PR40b-f. O(suffixes), cold.
#[must_use]
pub(crate) fn is_seal_file_name(name: &str) -> bool {
    is_spill_record_name(name)
        || ((name.starts_with(crate::seal_writer_task::SEAL_FILE_PREFIX)
            || name.starts_with(crate::seal_writer_task::LEGACY_SEAL_FILE_PREFIX))
            && strip_copy_suffixes(name).ends_with(".ndjson"))
}

/// Spill directories whose boot drain has run in this process (PR40b-f).
/// One entry in production; the list exists so tests over temp directories
/// cannot unblock each other. Cold: one push per boot drain, one scan per
/// retention sweep.
static BOOT_DRAINED_SPILL_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Records that the boot drain has read `spill_dir` (whatever it found), so
/// the retention sweep may delete aged unreplayed files there from now on.
pub(crate) fn note_boot_drain_ran(spill_dir: &Path) {
    let mut dirs = BOOT_DRAINED_SPILL_DIRS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !dirs.iter().any(|d| same_dir(d, spill_dir)) {
        dirs.push(spill_dir.to_path_buf());
    }
}

/// `true` once the boot drain has run over `spill_dir` in this process.
pub(crate) fn boot_drain_ran(spill_dir: &Path) -> bool {
    BOOT_DRAINED_SPILL_DIRS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .any(|d| same_dir(d, spill_dir))
}

/// The same directory, spelled the same way or resolving to one place.
fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// What a deleted file in a swept directory held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrunedKind {
    /// Seals not yet re-ingested: a non-empty deletion is data loss.
    Unreplayed,
    /// A file the replay finished with. Its ingested seals are in QuestDB,
    /// but undecodable or skipped seals have no other copy, so a delete is
    /// lossless only with a verified cold copy (2026-10-02).
    Archived,
}

/// One directory of [`prune_spill_files_at`]. `live_name` is the file the
/// writer may hold open, never deleted at any age.
fn prune_dir(
    dir: &Path,
    live_name: Option<&str>,
    kind: PrunedKind,
    cutoff: std::time::Duration,
    now: std::time::SystemTime,
    gate: CopyGate,
    hold: bool,
    outcome: &mut SpillPruneOutcome,
) {
    // O(1) EXEMPT: periodic cold retention sweep, never the per-seal append
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // missing dir — nothing to prune
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Only our own spill records. Anything else in the directory is left
        // strictly alone — deleting a file we did not write, to satisfy our
        // own budget, would be indefensible.
        // PR40b-f: `.bin.N` (set aside, or a name collision on staging) and
        // `.bin.overflow` are spill records too; until 2026-10-04 only
        // `*.bin` matched, so those were never pruned, uploaded or counted.
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_spill_record_name)
        {
            continue;
        }
        // The live-writer guard. Cheap, and it fails SAFE: an unreadable file
        // name is treated as live and kept, never deleted on uncertainty.
        if let Some(live) = live_name
            && path.file_name().and_then(|n| n.to_str()) == Some(live)
        {
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
        if hold {
            outcome.held_before_boot_drain += 1;
            outcome.bytes_after = outcome.bytes_after.saturating_add(len);
            continue;
        }
        // Operator Quotes 27 + 28: no delete without a verified cold copy.
        if !gate.allows_delete(&path, &meta) {
            outcome.refused_not_uploaded += 1;
            outcome.refused_not_uploaded_bytes =
                outcome.refused_not_uploaded_bytes.saturating_add(len);
            outcome.bytes_after = outcome.bytes_after.saturating_add(len);
            continue;
        }
        // O(1) EXEMPT: periodic cold retention sweep, never the per-seal append
        match std::fs::remove_file(&path) {
            Ok(()) => {
                gate.forget(&path);
                let with_copy = gate.copy_in_s3();
                if with_copy {
                    outcome.deleted_with_copy += 1;
                }
                match kind {
                    PrunedKind::Archived => {
                        outcome.archive_deleted += 1;
                        if !with_copy && len > 0 {
                            outcome.archive_deleted_without_copy += 1;
                        }
                    }
                    PrunedKind::Unreplayed => {
                        outcome.deleted += 1;
                        // With a verified copy the records are in the cold
                        // bucket, not lost.
                        if len > 0 && !with_copy {
                            outcome.deleted_non_empty += 1;
                            outcome.records_lost = outcome
                                .records_lost
                                .saturating_add(len / SEAL_SPILL_RECORD_SIZE as u64);
                        }
                    }
                }
            }
            Err(err) => {
                outcome.failed += 1;
                outcome.bytes_after = outcome.bytes_after.saturating_add(len);
                error!(
                    code = ErrorCode::StorageGap05DiskPressureUnrelievable.code_str(),
                    source = "spill_retention",
                    path = %path.display(),
                    error = %err,
                    "spill retention sweep: remove_file failed — the file stays and is \
                     retried next pass"
                );
            }
        }
    }
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
pub fn prune_spill_files(
    spill_dir: &Path,
    max_age_secs: u64,
    require_upload: bool,
) -> SpillPruneOutcome {
    // PR40b-f: until this process has run the boot drain over the folder,
    // an aged unreplayed file has not been tried yet, so it is kept. The
    // sweep runs once at task start, which can come before the drain, and
    // after a week or more off it used to delete files the drain never read.
    let hold_unreplayed = !boot_drain_ran(spill_dir);
    let outcome = prune_spill_files_inner(
        spill_dir,
        max_age_secs,
        std::time::SystemTime::now(),
        CopyGate::from_config(require_upload),
        hold_unreplayed,
    );
    if outcome.held_before_boot_drain > 0 {
        info!(
            held = outcome.held_before_boot_drain,
            "spill retention sweep: aged unreplayed spill files kept until the boot drain \
             has read them"
        );
    }
    // PR40b-f: files held for good in refused/, so a growing pile is seen.
    // APPROVED: cast — a file count, far below f64 precision loss.
    #[allow(clippy::cast_precision_loss)] // APPROVED: a file count, far below 2^52
    metrics::gauge!(SEAL_SPILL_REFUSED_FILES_GAUGE).set(outcome.refused_kept as f64);
    // APPROVED: cast — a per-pass file count, always <= u64.
    metrics::counter!(SEAL_SPILL_PRUNE_REFUSED_NOT_UPLOADED_COUNTER)
        .increment(outcome.refused_not_uploaded as u64);
    if outcome.refused_not_uploaded > 0 {
        error!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            source = "spill_retention",
            files = outcome.refused_not_uploaded,
            bytes = outcome.refused_not_uploaded_bytes,
            max_age_secs,
            "aged sealed-candle spill files were KEPT, not deleted: no verified copy of \
             them is in the cold bucket yet. They are deleted once the uploader has copied \
             them; a growing count means the uploader cannot reach the bucket."
        );
    }
    if outcome.archive_deleted_without_copy > 0 {
        error!(
            code = ErrorCode::AggregatorDrop01.code_str(),
            source = "spill_retention",
            files = outcome.archive_deleted_without_copy,
            max_age_secs,
            "aged files in the spill archive were deleted with NO cold copy (the copy gate \
             is off). They may have held undecodable or skipped seals, whose only copy they \
             were — reported as possible loss."
        );
    }
    if outcome.deleted_non_empty > 0 {
        // Audit PR40b: this line carried the unregistered code
        // "SPILL-RETENTION-01", which nothing filtered on, so a deleted spill
        // file still holding seals paged nobody. It is permanent sealed-candle
        // loss, so it is now AGGREGATOR-DROP-01, which pages.
        error!(
            code = ErrorCode::AggregatorDrop01.code_str(),
            source = "spill_retention",
            files = outcome.deleted_non_empty,
            records_lost = outcome.records_lost,
            max_age_secs,
            "spill files (top level or replaying/) aged out while STILL HOLDING \
             unreplayed seals — the replay path has been broken for longer than the retention window. \
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
    if outcome.archive_deleted > 0 {
        info!(
            archive_deleted = outcome.archive_deleted,
            deleted_with_copy = outcome.deleted_with_copy,
            bytes_after = outcome.bytes_after,
            "spill retention sweep: removed aged files from archive/"
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
            bucket_open_prev_close: 98.5,
            total_buy_qty: 89_600,
            total_sell_qty: 4_800,
            open_pct: 7.7,
            change_pct: 1.5,
            open_gap_pct: 0.8,
        }
    }

    pub(super) fn temp_spill_dir(name: &str) -> PathBuf {
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
            bucket_open_prev_close: 102.0,
            total_buy_qty: 0,
            total_sell_qty: 0,
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
        reseal_record_for_test(&mut bytes);
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
    fn test_read_all_refuses_every_older_version_but_drains_current_siblings() {
        // A daily spill file mixing records from EVERY older ordinal space
        // with a current record must drain ONLY the current one.
        //
        // ⚠ WHY THIS TEST WAS REWRITTEN (2026-09-19). It used to forge one
        // byte-7 == 0 record and pair it with a record it CALLED "v1" — but
        // `to_bytes()` stamps the LIVE constant, so that sibling was never v1
        // at all; it was whatever the current version happened to be. The test
        // therefore only ever exercised the `== 0` corner and would have kept
        // passing if the gate had stayed `buf[7] == 0` while the ordinal space
        // renumbered underneath it. The version-3 record below is the case the
        // nine-frame collapse actually creates, and it is forged explicitly so
        // it cannot silently become "current" on the next bump.
        let dir = temp_spill_dir("older-version-refusal");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = 1_716_000_000_i64;

        // Pre-C2 record: byte 7 was zero padding, the 5-frame ordinal space.
        let pre_c2 = mk_seal(13, 0, 1, 1_716_000_900, 100.0);
        let mut pre_c2_bytes = pre_c2.to_bytes();
        pre_c2_bytes[7] = 0;

        // Version-3 record: the 24-frame ordinal space, where S1 was 5 and M60
        // was 23. Both are IN RANGE for today's nine-frame table, so refusing
        // on the version byte is the ONLY thing standing between this record
        // and a silent misdecode into the wrong frame.
        let v3 = mk_seal(19, 0, 3, 1_716_001_200, 150.25);
        let mut v3_bytes = v3.to_bytes();
        v3_bytes[7] = 3;
        assert!(
            v3_bytes[7] < SEAL_SPILL_FORMAT_VERSION,
            "the forged record must be genuinely older than the live constant; \
             bump this literal only together with a NEW forged version below it",
        );

        // Current record (to_bytes stamps the live version).
        let current = mk_seal(25, 1, 2, 1_716_001_500, 200.75);
        let current_bytes = current.to_bytes();
        assert_eq!(current_bytes[7], SEAL_SPILL_FORMAT_VERSION);

        let path = writer.spill_path(now);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let mut raw = Vec::with_capacity(3 * SEAL_SPILL_RECORD_SIZE);
        raw.extend_from_slice(&pre_c2_bytes);
        raw.extend_from_slice(&v3_bytes);
        raw.extend_from_slice(&current_bytes);
        std::fs::write(&path, &raw).expect("write mixed spill file");

        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 1, "only the current record must drain");
        assert_eq!(drained[0], current);

        writer.clear_spill_for_date(now).expect("clear");
        assert!(!path.exists(), "spill file deleted after drain");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_all_refuses_a_future_version_record_like_an_older_one() {
        // The gate is `!=`, not `<`: a record stamped by a NEWER binary (a
        // deploy rollback reads it) is just as unreadable as an older one,
        // because its tf_ordinal belongs to an ordinal space this binary does
        // not know. Pin the forward direction so the gate can never be
        // "simplified" back to `<` without failing the build.
        let dir = temp_spill_dir("future-version-refusal");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = 1_716_000_000_i64;

        let future = mk_seal(19, 0, 3, 1_716_001_200, 150.25);
        let mut future_bytes = future.to_bytes();
        future_bytes[7] = SEAL_SPILL_FORMAT_VERSION.wrapping_add(1);
        assert!(
            future_bytes[7] > SEAL_SPILL_FORMAT_VERSION,
            "the forged record must be genuinely NEWER than the live constant",
        );

        let current = mk_seal(25, 1, 2, 1_716_001_500, 200.75);
        let current_bytes = current.to_bytes();

        let path = writer.spill_path(now);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let mut raw = Vec::with_capacity(2 * SEAL_SPILL_RECORD_SIZE);
        raw.extend_from_slice(&future_bytes);
        raw.extend_from_slice(&current_bytes);
        std::fs::write(&path, &raw).expect("write mixed spill file");

        let drained = writer.read_all(now).expect("read");
        assert_eq!(
            drained.len(),
            1,
            "a future-version record must be refused, the current one drained"
        );
        assert_eq!(drained[0], current);

        writer.clear_spill_for_date(now).expect("clear");
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
        // spill format version (C2, 2026-07-21) so a record written under
        // an OLDER ordinal space is refusable on load — the gate is
        // `buf[7] != SEAL_SPILL_FORMAT_VERSION` in `read_all`, not a
        // `== 0` test: the 2026-09-19 nine-frame collapse renumbered the
        // ordinals a second time, so every version other than the live
        // constant (older OR newer) is refused, not just the pre-C2 zero. §31 Option 2 now
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
        assert_eq!(name, "seals_v4-2026-01-01.bin");
        assert!(name.starts_with(crate::seal_writer_task::SEAL_FILE_PREFIX));
        assert!(
            !name.starts_with(crate::seal_writer_task::LEGACY_SEAL_FILE_PREFIX),
            "a v4 spill file must be invisible to a pre-v4 binary's drain"
        );
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
        assert_eq!(name, "seals_v4-2026-05-10.bin");
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
    fn sync_open_file_is_a_no_op_with_nothing_open_and_keeps_the_handle_open() {
        // PR17: the escalation thread syncs the day file. Nothing open is a
        // no-op; an open file syncs, and the append handle stays usable.
        let dir = temp_spill_dir("sync-open-file");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        assert!(writer.sync_open_file(), "nothing open must read as synced");
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let s1 = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        let s2 = mk_seal(25, 0, 4, 1_716_001_500, 200.0);
        writer.append_seal(&s1, now).expect("append s1");
        assert!(writer.sync_open_file(), "an open day file must sync");
        writer.append_seal(&s2, now).expect("append after a sync");
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained, vec![s1, s2]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_drain_reachable_append_never_syncs_and_the_sync_holds_no_lock() {
        // PR17: `append_seal` and the day rotation in `current_file` run under
        // the append lock and are reachable from the frame drain's inline
        // fallback, so neither may wait for the device. The escalation
        // thread's `sync_open_file` is the only place that syncs, and it must
        // not hold the append lock while it does, or an inline append would
        // wait for it.
        let src = include_str!("seal_spill.rs");
        // Each body runs to the next doc comment at item indent (no brace
        // literals here: the banned-pattern scanner counts braces).
        let body = |name: &str| {
            let start = src
                .find(name)
                .unwrap_or_else(|| panic!("{name} must exist"));
            let rest = &src[start..];
            let end = rest.find("\n    ///").unwrap_or(rest.len());
            &rest[..end]
        };
        let append = body("pub fn append_seal(");
        assert!(
            !append.contains("sync_data") && !append.contains("sync_open_file"),
            "append_seal must never sync per seal"
        );
        let rotation = body("fn current_file<'s>(");
        assert!(
            !rotation.contains("sync_data") && !rotation.contains("sync_all"),
            "the day rotation runs under the append lock and must never sync"
        );
        let sync = body("pub(crate) fn sync_open_file(");
        let lock_scope_end = sync
            .find("let rotated_outcome")
            .unwrap_or_else(|| panic!("the lock must be released before the sync"));
        assert!(
            sync[lock_scope_end..].contains("sync_data"),
            "sync_data must run after the lock scope ends"
        );
        assert!(
            !sync[..lock_scope_end].contains("sync_data"),
            "sync_data must not run under the append lock"
        );
        // Every sync call in this file is the one in `sync_open_file`. The
        // needle is built at run time so this test does not count itself.
        let call = [".sync", "_data("].concat();
        assert_eq!(
            src.matches(call.as_str()).count(),
            sync[lock_scope_end..].matches(call.as_str()).count(),
            "only sync_open_file may sync the spill files"
        );
    }

    #[test]
    fn test_sync_open_file_syncs_only_new_writes_and_new_directory_entries() {
        // 2026-10-04: the seal writer task now calls `sync_open_file` every
        // cycle, so it must sync only what was written since the last call,
        // and a new day file's directory entry (and a new spill directory's)
        // must reach the device too.
        let root = temp_spill_dir("sync-dir-entries");
        let dir = root.join("spill");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        assert!(writer.sync_open_file(), "nothing written is a clean no-op");
        assert!(!writer.has_unsynced_writes());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .unwrap_or_else(|| panic!("valid"))
            .timestamp();
        writer
            .append_seal(&mk_seal(13, 0, 0, 1_716_000_900, 100.0), now)
            .unwrap_or_else(|err| panic!("append: {err}"));
        assert!(writer.data_unsynced.load(Ordering::Relaxed));
        assert!(writer.dir_unsynced.load(Ordering::Relaxed), "new day file");
        assert!(
            writer.parent_unsynced.load(Ordering::Relaxed),
            "new spill directory"
        );
        assert!(writer.sync_open_file(), "file and both entries must sync");
        assert!(
            !writer.has_unsynced_writes(),
            "a clean sync clears every flag"
        );
        assert!(writer.sync_open_file(), "nothing new: a no-op");
        // A later append to the same, non-empty file needs the file only.
        writer
            .append_seal(&mk_seal(25, 0, 4, 1_716_001_500, 200.0), now)
            .unwrap_or_else(|err| panic!("append again: {err}"));
        assert!(writer.data_unsynced.load(Ordering::Relaxed));
        assert!(!writer.dir_unsynced.load(Ordering::Relaxed));
        assert!(!writer.parent_unsynced.load(Ordering::Relaxed));
        assert!(writer.sync_open_file());
        assert!(!writer.has_unsynced_writes());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn test_sync_open_file_syncs_and_closes_the_handle_a_day_rotation_left_behind() {
        // PR17: the rotation at IST midnight moves the previous day's handle
        // aside without syncing it (it runs under the append lock); the next
        // `sync_open_file` syncs it off the lock and closes it.
        let dir = temp_spill_dir("sync-rotated");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let day_one = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .unwrap_or_else(|| panic!("valid"))
            .timestamp();
        let day_two = day_one + 86_400;
        let s1 = mk_seal(13, 0, 0, 1_716_000_900, 100.0);
        let s2 = mk_seal(25, 0, 4, 1_716_001_500, 200.0);
        writer
            .append_seal(&s1, day_one)
            .unwrap_or_else(|err| panic!("append day one: {err}"));
        assert!(writer.lock_state().rotated.is_none(), "no rotation yet");
        writer
            .append_seal(&s2, day_two)
            .unwrap_or_else(|err| panic!("append day two: {err}"));
        assert!(
            writer.lock_state().rotated.is_some(),
            "the rotation must leave the previous day's handle for the sync"
        );
        assert!(writer.sync_open_file(), "both handles must sync");
        assert!(
            writer.lock_state().rotated.is_none(),
            "the sync must close the rotated handle"
        );
        assert!(writer.lock_state().open.is_some(), "the open handle stays");
        let first = writer
            .read_all(day_one)
            .unwrap_or_else(|err| panic!("read day one: {err}"));
        let second = writer
            .read_all(day_two)
            .unwrap_or_else(|err| panic!("read day two: {err}"));
        assert_eq!(first, vec![s1]);
        assert_eq!(second, vec![s2]);
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
        assert!(p.to_string_lossy().ends_with("seals_v4-2026-05-10.bin"));
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
    fn test_append_seals_writes_a_batch_with_one_call_and_reads_back_in_order() {
        let dir = temp_spill_dir("append-seals-batch");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = 1_700_000_000_i64;
        let seals: Vec<SerializedSeal> = (0..50u32)
            .map(|i| mk_seal(13, 0, 1, 34_200 + i, 100.0 + f64::from(i)))
            .collect();
        let mut scratch = Vec::new();
        writer
            .append_seals(seals.iter(), &mut scratch, now)
            .expect("batch append");
        // An empty batch is a no-op, not an error.
        writer
            .append_seals(std::iter::empty(), &mut scratch, now)
            .expect("empty batch");
        let back = writer.read_all(now).expect("read back");
        assert_eq!(back, seals);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_append_seals_errors_while_the_spill_dir_is_unusable() {
        let dir = temp_spill_dir("append-seals-dead");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::write(&dir, b"not a directory").expect("blocker");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let seal = mk_seal(13, 0, 1, 34_200, 1.0);
        let mut scratch = Vec::new();
        assert!(
            writer
                .append_seals(std::iter::once(&seal), &mut scratch, 1_700_000_000)
                .is_err(),
            "a batch that cannot be written must say so, so the caller can escalate it"
        );
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn test_a_torn_file_set_aside_keeps_its_records_readable_and_the_next_append_starts_clean() {
        let dir = temp_spill_dir("torn-set-aside");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = 1_700_000_000_i64;
        writer
            .append_seal(&mk_seal(13, 0, 1, 1, 1.0), now)
            .expect("append");
        // Simulate the double fault: a torn half-record the cut could not remove.
        let live = writer.spill_path(now);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&live)
                .expect("open");
            f.write_all(&[7u8; 40]).expect("torn bytes");
        }
        writer.close_open_handle();
        let aside = set_aside_torn_file(&live).expect("set aside");
        let aside_name = aside
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert!(
            aside_name.ends_with(".bin.1"),
            "the set-aside name must stay a spill file for both drains: {aside_name}"
        );
        writer
            .append_seal(&mk_seal(13, 0, 1, 2, 2.0), now)
            .expect("append after");
        assert_eq!(
            std::fs::metadata(&live).expect("fresh file").len(),
            SEAL_SPILL_RECORD_SIZE as u64,
            "the next append must start a clean, aligned file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_with_appends_paused_lets_the_day_file_move_without_losing_an_append() {
        let dir = temp_spill_dir("appends-paused");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = 1_700_000_000_i64;
        writer
            .append_seal(&mk_seal(13, 0, 1, 1, 1.0), now)
            .expect("first append");
        let moved = dir.join("moved.bin");
        let live = writer.spill_path(now);
        writer.with_appends_paused(|| {
            std::fs::rename(&live, &moved).expect("rename");
            ((), true)
        });
        writer
            .append_seal(&mk_seal(13, 0, 1, 2, 2.0), now)
            .expect("second append");
        assert_eq!(
            std::fs::metadata(&moved).expect("moved").len(),
            SEAL_SPILL_RECORD_SIZE as u64,
            "no append may follow the moved file"
        );
        assert_eq!(
            writer.read_all(now).expect("read").len(),
            1,
            "the next append must open a fresh day file"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        // ABOVE both closes this helper is called with (102.5 and 24_341.95),
        // so every fixture bar signs NEGATIVE. That is the strong direction: a
        // baseline lost on the wire decodes 0.0, which signs POSITIVE, so a
        // dropped field fails the round-trip tests loudly instead of passing by
        // agreeing with the default.
        state.bucket_open_prev_close = 24_400.0;
        // Retained on the STATE and deliberately never asserted through the
        // wire: the binary record stopped carrying flow at v3, so these two
        // exist here only to prove a replayed seal does not resurrect them.
        state.net_volume_signed = -4_242;
        state.net_volume_classified = true;
        state.total_buy_qty = 89_600;
        state.total_sell_qty = 4_800;
        BufferedSeal::new(sid, seg, tf, state, Feed::Dhan)
    }

    /// The SIGN BASELINE survives a disk spill (record format v3).
    ///
    /// This test replaces `a_replayed_spill_record_carries_the_net_volume_it_
    /// was_classified_with`, which pinned the v2 arrangement: bytes 80..88
    /// carried the tick-rule accumulator so a rescued bar came back with its
    /// flow intact. The 2026-09-18 directive deleted the column that fed, so
    /// the accumulator is no longer persisted anywhere and those eight bytes
    /// are back to holding `bucket_open_prev_close`.
    ///
    /// What has to survive now is the baseline the SIGN is measured against.
    /// Losing it is not a NULL — it is a WRONG NUMBER: `signed_volume()` reads
    /// a missing baseline as "no comparison" and reports the bar POSITIVE, so
    /// a sell bar rescued to disk would come back looking like a buy bar. The
    /// fixture is deliberately sell-heavy for exactly that reason.
    #[test]
    fn a_replayed_spill_record_carries_the_sign_baseline_it_was_sealed_with() {
        let seal = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 24_341.95);
        let original = seal.state.signed_volume();
        assert!(
            original < 0,
            "fixture precondition: the bar closed below its baseline, so a \
             dropped baseline cannot pass this test by agreeing with the default"
        );

        let bytes = SerializedSeal::from(&seal).to_bytes();
        let replayed = SerializedSeal::from_bytes(&bytes)
            .expect("a well-formed record decodes")
            .try_into_buffered_seal()
            .expect("a known tf ordinal round-trips");

        assert!(
            replayed.state.volume > 0,
            "precondition: the replayed bar carries real gross volume"
        );
        assert_eq!(
            replayed.state.bucket_open_prev_close, 24_400.0,
            "the baseline itself must ride through disk"
        );
        assert_eq!(
            replayed.state.signed_volume(),
            original,
            "same signed volume after a full disk round-trip"
        );
        assert_eq!(
            replayed.state.signed_volume().unsigned_abs(),
            replayed.state.volume,
            "the magnitude is the gross volume, unaltered — only the sign is derived"
        );
    }

    /// A pre-v3 record decodes the baseline as ABSENT, never as reinterpreted
    /// bits.
    ///
    /// This is the half of the format bump that could silently corrupt, and it
    /// is now dangerous in a new direction. In v2 those bytes held an `i64`
    /// quantity; read as an `f64` they are a denormal or a nonsense magnitude,
    /// and feeding that to the sign rule would flip the sign of every replayed
    /// bar whose close sits under it. So a record below
    /// `SEAL_SPILL_FIRST_PREV_CLOSE_VERSION` reports 0.0 — "no baseline" —
    /// which signs the bar positive, exactly the answer that record carried
    /// when it was written.
    #[test]
    fn a_pre_v3_record_decodes_no_baseline_rather_than_reinterpreting_the_bytes() {
        let seal = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 24_341.95);

        for (version, label) in [(1_u8, "v1 f64 price"), (2_u8, "v2 i64 net volume")] {
            let mut bytes = SerializedSeal::from(&seal).to_bytes();
            bytes[7] = version;
            // Versions before 5 held the id's low 32 bits at 0..4, not a
            // checksum (audit PR41c).
            bytes[0..4].copy_from_slice(&13_u32.to_le_bytes());
            // Whatever the older format put in those bytes, the decoder must
            // not read it as a baseline. `-4_242` is the v2 accumulator value
            // this fixture carried; as an `f64` it is a denormal near zero,
            // which would sign every bar NEGATIVE if it were trusted.
            bytes[80..88].copy_from_slice(&(-4_242_i64).to_le_bytes());

            let decoded =
                SerializedSeal::from_bytes(&bytes).expect("an older record still decodes");
            assert_eq!(
                decoded.bucket_open_prev_close, 0.0,
                "{label}: no baseline, never the reinterpreted bits"
            );

            let replayed = decoded
                .try_into_buffered_seal()
                .expect("a known tf ordinal round-trips");
            assert_eq!(
                replayed.state.signed_volume(),
                i64::try_from(replayed.state.volume).expect("fixture volume fits"),
                "{label}: no baseline signs the bar positive — the same answer \
                 it carried when written"
            );
        }
    }

    /// A first-bucket bar (no previous close) round-trips as "no baseline".
    ///
    /// `0.0` is a real state, not a defect: the session's first bucket for an
    /// instrument has no previous bar to compare against. It must survive the
    /// spill as 0.0 and sign POSITIVE, which is the documented default rather
    /// than a claim that the close rose.
    #[test]
    fn a_bar_with_no_baseline_round_trips_as_no_baseline_and_signs_positive() {
        let mut seal = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 24_341.95);
        seal.state.bucket_open_prev_close = 0.0;

        let bytes = SerializedSeal::from(&seal).to_bytes();
        assert_eq!(
            f64::from_le_bytes([
                bytes[80], bytes[81], bytes[82], bytes[83], bytes[84], bytes[85], bytes[86],
                bytes[87],
            ]),
            0.0,
            "the absence is written as 0.0, not as a sentinel the reader must know"
        );

        let replayed = SerializedSeal::from_bytes(&bytes)
            .expect("a well-formed record decodes")
            .try_into_buffered_seal()
            .expect("a known tf ordinal round-trips");
        assert_eq!(replayed.state.bucket_open_prev_close, 0.0);
        assert_eq!(
            replayed.state.signed_volume(),
            1234,
            "no baseline signs positive"
        );
    }

    /// The binary record does NOT resurrect the retired flow value.
    ///
    /// The fixture sets `net_volume_signed = -4_242` on the STATE, so a
    /// round-trip that quietly kept carrying it would look like a success. It
    /// must come back at the state default instead: no format on disk records
    /// the value any more, and reporting one would be a fabrication.
    #[test]
    fn a_replayed_seal_does_not_resurrect_the_retired_flow_value() {
        let seal = mk_buffered_seal(13, 0, TfIndex::M1, 1_716_000_900, 24_341.95);
        assert!(
            seal.state.net_volume_classified,
            "fixture precondition: the live state DID classify this bar"
        );

        let bytes = SerializedSeal::from(&seal).to_bytes();
        let replayed = SerializedSeal::from_bytes(&bytes)
            .expect("a well-formed record decodes")
            .try_into_buffered_seal()
            .expect("a known tf ordinal round-trips");

        assert!(
            !replayed.state.net_volume_classified,
            "the binary record carries no flow, so a replayed bar must not \
             claim one was measured"
        );
        assert_eq!(replayed.state.net_volume_signed, 0);
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
        assert_eq!(serialised.bucket_open_prev_close, 24_400.0);
        assert_eq!(serialised.total_buy_qty, 89_600);
        assert_eq!(serialised.total_sell_qty, 4_800);
    }

    #[test]
    fn test_from_buffered_seal_maps_all_nine_tfs_to_correct_ordinal() {
        // Verify every TfIndex variant maps to its canonical ordinal
        // (0..=8 after the 2026-09-19 collapse to nine frames: M1/M3/M5/M15
        // stay byte-stable at 0..=3, and everything after them RENUMBERED —
        // which is exactly why that change carried a version bump to 4, unlike
        // the earlier C3 second-scale APPEND, which only added ordinals above
        // the existing ones and left every written byte meaning what it meant).
        // This pins the trading↔storage contract: a future re-ordering of
        // TfIndex::ALL would silently flip every spilled record's TF
        // assignment, and only SEAL_SPILL_FORMAT_VERSION can catch it.
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

        // The four BYTE-STABLE frames: M1, M3, M5, M15 keep ordinals 0..=3
        // across every ordinal-space change this record has seen.
        //
        // ⚠ The C3 second-scale change WAS append-only (frames added after
        // D1 at ordinal 4, never interleaved), which is why it needed no
        // version bump. The 2026-09-19 nine-frame collapse is NOT: D1 was
        // RETIRED at ordinal 4 and every survivor past M15 RENUMBERED (S1
        // 5→4, S5 9→6, M30 22→7, M60 23→8). That renumbering is exactly why
        // `SEAL_SPILL_FORMAT_VERSION` is 4 and why `read_all` refuses every
        // record below it — a stale ordinal byte is still in range, so it
        // decodes into the WRONG frame silently.
        let legacy: [(TfIndex, u8); 4] = [
            (TfIndex::M1, 0),
            (TfIndex::M3, 1),
            (TfIndex::M5, 2),
            (TfIndex::M15, 3),
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
        // 9 timeframes → valid ordinals are 0..=8; the first out-of-range
        // ordinal is `TfIndex::ALL.len()` (= 9), derived so this test cannot
        // go stale the way the surrounding prose did. ROLLBACK SAFETY: this
        // clean-refusal arm is the same code shape an older binary takes for a
        // record carrying an ordinal past the end of ITS table — refused (skip
        // + warn at the read site), NEVER a panic. Refusal is NOT sufficient on
        // its own for the 2026-09-19 collapse, though: that renumbering left
        // every retired ordinal IN RANGE for the new table, so a stale byte
        // decodes to the WRONG frame silently. SEAL_SPILL_FORMAT_VERSION = 4
        // is what catches that case; this arm only covers ordinals past the end.
        s.tf_ordinal = TfIndex::ALL.len() as u8; // out of range (9)
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
            recovered.state.bucket_open_prev_close, original.state.bucket_open_prev_close,
            "the sign baseline is a carried field since v3"
        );
        // `net_volume_signed` / `net_volume_classified` are deliberately NOT
        // asserted equal: the record retired them at v3, so they come back at
        // the state default. Pinned on its own in
        // `a_replayed_seal_does_not_resurrect_the_retired_flow_value`.
        assert_eq!(recovered.state.total_buy_qty, original.state.total_buy_qty);
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

        // The record stopped carrying the tick-rule flow value at v3
        // (2026-09-18), so a whole-struct comparison is built against an
        // expectation with those two fields cleared rather than against the
        // raw original. Zeroing them HERE, on a copy, keeps the assertion
        // whole-struct — every other field still has to match exactly, so a
        // future field that quietly fails to round-trip still fails this test.
        // That the pair comes back cleared is asserted on its own in
        // `a_replayed_seal_does_not_resurrect_the_retired_flow_value`.
        let mut expected = original;
        expected.state.net_volume_signed = 0;
        expected.state.net_volume_classified = false;
        assert_eq!(recovered, expected);
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
        let out =
            prune_spill_files_at(&dir, 0, std::time::SystemTime::now(), CopyGate::NotRequired);
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
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::NotRequired,
        );
        assert!(!path.exists(), "an old day's file is still eligible");
        assert_eq!(out.deleted, 1);
        assert_eq!(out.skipped_live, 0);
    }

    #[test]
    fn spill_sweep_deletes_only_aged_files() {
        let dir = spill_tmp("aged");
        let old = write_aged(&dir, "seals-20260101.bin", 0, 10_000);
        let fresh = write_aged(&dir, "seals-20260819.bin", 0, 10);
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::NotRequired,
        );
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
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::NotRequired,
        );
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
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::NotRequired,
        );
        assert_eq!(out.deleted, 1);
        assert_eq!(out.deleted_non_empty, 0);
        assert_eq!(out.records_lost, 0);
    }

    #[test]
    fn spill_sweep_never_touches_foreign_files() {
        let dir = spill_tmp("foreign");
        let note = write_aged(&dir, "operator-notes.txt", 4096, 10_000);
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::NotRequired,
        );
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
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::NotRequired,
        );
        assert_eq!(out.bytes_after, 512, "surviving bytes must be reported");
        let missing = std::env::temp_dir().join("tv-spill-does-not-exist-xyz");
        let out = prune_spill_files_at(
            &missing,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::NotRequired,
        );
        assert_eq!(out, SpillPruneOutcome::default(), "missing dir is a no-op");
    }

    #[test]
    fn test_prune_spill_files_at_sweeps_replaying_as_loss_and_archive_as_not() {
        // Audit PR40b: the two folders the replay moves files into were never
        // pruned and were outside the byte figure.
        let dir = spill_tmp("subdirs");
        let replaying = dir.join(crate::seal_writer_task::SEAL_REPLAYING_SUBDIR);
        let archive = dir.join(crate::seal_writer_task::SEAL_ARCHIVE_SUBDIR);
        std::fs::create_dir_all(&replaying).expect("mkdir replaying");
        std::fs::create_dir_all(&archive).expect("mkdir archive");
        let old_staged = write_aged(
            &replaying,
            "seals_v4-20260101.bin",
            SEAL_SPILL_RECORD_SIZE * 3,
            10_000,
        );
        let fresh_staged = write_aged(&replaying, "seals_v4-20260102.bin", 256, 10);
        let old_archived = write_aged(
            &archive,
            "seals_v4-20260101.bin",
            SEAL_SPILL_RECORD_SIZE * 5,
            10_000,
        );
        let fresh_archived = write_aged(&archive, "seals_v4-20260103.bin", 128, 10);
        let foreign = write_aged(&archive, "seal-unwritten.mark", 64, 10_000);

        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::NotRequired,
        );

        assert!(!old_staged.exists() && !old_archived.exists());
        assert!(fresh_staged.exists() && fresh_archived.exists() && foreign.exists());
        // An aged staged file was never re-ingested: counted as lost.
        assert_eq!(out.deleted, 1);
        assert_eq!(out.deleted_non_empty, 1);
        assert_eq!(out.records_lost, 3);
        // An aged archived file is counted apart. With the copy gate off it
        // may have held skipped seals, so a non-empty one is possible loss.
        assert_eq!(out.archive_deleted, 1);
        assert_eq!(out.archive_deleted_without_copy, 1);
        assert_eq!(out.deleted_with_copy, 0);
        // Both folders count against the disk figure.
        assert_eq!(out.bytes_after, 256 + 128);
    }

    #[test]
    fn test_prune_spill_files_at_applies_the_live_guard_only_at_the_top_level() {
        // The writer only ever opens TODAY's file at the top level. A file of
        // the same name that the replay moved into archive/ is closed and
        // follows the age rule like any other.
        let dir = spill_tmp("liveguard");
        let now = std::time::SystemTime::now();
        let secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let today = ist_date_filename(secs);
        let archive = dir.join(crate::seal_writer_task::SEAL_ARCHIVE_SUBDIR);
        std::fs::create_dir_all(&archive).expect("mkdir archive");
        let live = write_aged(&dir, &today, 128, 10_000);
        let archived_today = write_aged(&archive, &today, 128, 10_000);
        let out = prune_spill_files_at(&dir, 3_600, now, CopyGate::NotRequired);
        assert!(live.exists(), "the live file is never deleted");
        assert!(!archived_today.exists());
        assert_eq!(out.skipped_live, 1);
        assert_eq!(out.archive_deleted, 1);
    }

    #[test]
    fn test_is_spill_record_name_and_strip_copy_suffixes_match_every_renamed_copy() {
        // PR40b-f: the prune and the uploader matched `*.bin` only, so a
        // set-aside or collision copy was never pruned, uploaded or counted.
        for name in [
            "seals_v4-2026-10-01.bin",
            "seals_v4-2026-10-01.bin.1",
            "seals_v4-2026-10-01.bin.12",
            "seals_v4-2026-10-01.bin.1.3",
            "seals_v4-2026-10-01.bin.overflow",
            "seals_v4-2026-10-01.bin.overflow.2",
        ] {
            assert!(is_spill_record_name(name), "{name} holds spilled seals");
        }
        for name in [
            "seals_v4-2026-10-01.ndjson",
            "seals_v4-2026-10-01.ndjson.1",
            "seals_v4-2026-10-01.bin.tmp",
            "seal-unwritten.mark",
            "bin",
            "x.1",
        ] {
            assert!(!is_spill_record_name(name), "{name} is not a spill file");
        }
        assert_eq!(
            strip_copy_suffixes("seals_v4-2026-10-01.bin.overflow.7"),
            "seals_v4-2026-10-01.bin"
        );
    }

    #[test]
    fn spill_sweep_prunes_renamed_copies_and_counts_their_records() {
        let dir = spill_tmp("renamed");
        let archive = dir.join(crate::seal_writer_task::SEAL_ARCHIVE_SUBDIR);
        std::fs::create_dir_all(&archive).expect("mkdir archive");
        let set_aside = write_aged(
            &dir,
            "seals_v4-20260101.bin.1",
            SEAL_SPILL_RECORD_SIZE * 2,
            10_000,
        );
        let overflow = write_aged(&archive, "seals_v4-20260101.bin.overflow", 128, 10_000);
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::NotRequired,
        );
        assert!(!set_aside.exists() && !overflow.exists());
        assert_eq!(out.deleted, 1);
        assert_eq!(
            out.records_lost, 2,
            "a set-aside copy's records are counted"
        );
        assert_eq!(out.archive_deleted, 1);
    }

    #[test]
    fn spill_sweep_keeps_aged_unreplayed_files_until_the_boot_drain_has_run() {
        // PR40b-f: the sweep runs once at task start, which can come before
        // the boot drain. After a week or more off, every unreplayed file is
        // past the window and used to be deleted before the drain read it.
        let dir = spill_tmp("held");
        let replaying = dir.join(crate::seal_writer_task::SEAL_REPLAYING_SUBDIR);
        let archive = dir.join(crate::seal_writer_task::SEAL_ARCHIVE_SUBDIR);
        std::fs::create_dir_all(&replaying).expect("mkdir replaying");
        std::fs::create_dir_all(&archive).expect("mkdir archive");
        let top = write_aged(&dir, "seals_v4-20260101.bin", 128, 10_000);
        let staged = write_aged(&replaying, "seals_v4-20260102.bin", 128, 10_000);
        let archived = write_aged(&archive, "seals_v4-20260103.bin", 128, 10_000);
        let now = std::time::SystemTime::now();

        let held = prune_spill_files_inner(&dir, 3_600, now, CopyGate::NotRequired, true);
        assert!(top.exists() && staged.exists(), "unreplayed files are kept");
        assert!(
            !archived.exists(),
            "archive/ is not held: the drain is done with it"
        );
        assert_eq!(held.held_before_boot_drain, 2);
        assert_eq!((held.deleted, held.records_lost), (0, 0));
        assert_eq!(held.bytes_after, 256);

        let after = prune_spill_files_inner(&dir, 3_600, now, CopyGate::NotRequired, false);
        assert!(!top.exists() && !staged.exists());
        assert_eq!(after.held_before_boot_drain, 0);
        assert_eq!(after.deleted, 2);
    }

    #[test]
    fn test_is_seal_file_name_matches_spill_records_and_staged_dlq_copies() {
        for name in [
            "seals_v4-2026-10-01.bin",
            "seals_v4-2026-10-01.bin.2",
            "seals_v4-2026-10-01.ndjson",
            "seals_v4-2026-10-01.ndjson.1",
            "seals-2026-09-18.ndjson.overflow",
        ] {
            assert!(is_seal_file_name(name), "{name}");
        }
        for name in [
            "notes.ndjson",
            "boot-committed.summary",
            "x.wal",
            "seals_v4-x.txt",
        ] {
            assert!(!is_seal_file_name(name), "{name}");
        }
    }

    #[test]
    fn test_sync_directory_syncs_a_folder_and_errs_on_a_missing_one() {
        let dir = spill_tmp("sync-dir");
        assert!(sync_directory(&dir).is_ok());
        assert!(sync_directory(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spill_sweep_never_deletes_refused_files_and_counts_them() {
        // PR40b-f: refused/ holds files with seals a recovery path gave up
        // on; each is those seals' only copy, so no age deletes it, with the
        // copy gate on or off.
        let dir = spill_tmp("refused-kept");
        let refused = dir.join(crate::seal_writer_task::SEAL_REFUSED_SUBDIR);
        std::fs::create_dir_all(&refused).expect("mkdir refused");
        let spill = write_aged(&refused, "seals_v4-20260101.bin", 256, 10_000_000);
        let dlq_copy = write_aged(&refused, "seals_v4-20260101.ndjson.1", 64, 10_000_000);
        let other = write_aged(&refused, "notes.txt", 8, 10_000_000);
        let now = std::time::SystemTime::now();
        for gate in [CopyGate::NotRequired, CopyGate::Required] {
            let out = prune_spill_files_inner(&dir, 3_600, now, gate, false);
            assert!(spill.exists() && dlq_copy.exists() && other.exists());
            assert_eq!(out.refused_kept, 2, "both seal files are counted");
            assert_eq!(out.bytes_after, 320, "their bytes are in the total");
            assert_eq!(
                (out.deleted, out.archive_deleted, out.records_lost),
                (0, 0, 0)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_note_boot_drain_ran_and_boot_drain_ran_are_per_directory() {
        let drained = spill_tmp("drained");
        let other = spill_tmp("not-drained");
        assert!(!boot_drain_ran(&drained));
        note_boot_drain_ran(&drained);
        note_boot_drain_ran(&drained);
        assert!(boot_drain_ran(&drained));
        assert!(
            boot_drain_ran(&drained.join(".")),
            "the same folder spelled differently"
        );
        assert!(!boot_drain_ran(&other), "another folder is still held");
    }

    #[test]
    fn test_live_spill_file_name_is_the_writers_day_file() {
        // 2026-09-21T13:33:20Z = 19:03:20 IST; 18:40 UTC is the next IST day.
        assert_eq!(
            live_spill_file_name(1_790_000_000),
            "seals_v4-2026-09-21.bin"
        );
        assert_eq!(
            live_spill_file_name(1_790_016_000),
            "seals_v4-2026-09-22.bin"
        );
        assert_eq!(
            live_spill_file_name(1_790_000_000),
            ist_date_filename(1_790_000_000)
        );
    }

    #[test]
    fn test_prune_spill_files_at_keeps_an_aged_unreplayed_file_without_a_marker() {
        // Operator Quotes 27 + 28: with the copy gate on, an aged file with
        // no verified cold copy is KEPT, at the top level and in replaying/.
        let dir = spill_tmp("gate-keep");
        let replaying = dir.join(crate::seal_writer_task::SEAL_REPLAYING_SUBDIR);
        std::fs::create_dir_all(&replaying).expect("mkdir replaying");
        let top = write_aged(
            &dir,
            "seals_v4-2026-01-01.bin",
            SEAL_SPILL_RECORD_SIZE * 2,
            10_000,
        );
        let staged = write_aged(
            &replaying,
            "seals_v4-2026-01-02.bin",
            SEAL_SPILL_RECORD_SIZE,
            10_000,
        );
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::Required,
        );
        assert!(top.exists() && staged.exists());
        assert_eq!(out.refused_not_uploaded, 2);
        assert_eq!(
            out.refused_not_uploaded_bytes,
            (SEAL_SPILL_RECORD_SIZE * 3) as u64
        );
        assert_eq!(
            (out.deleted, out.deleted_non_empty, out.records_lost),
            (0, 0, 0)
        );
        assert_eq!(out.bytes_after, (SEAL_SPILL_RECORD_SIZE * 3) as u64);
    }

    #[test]
    fn test_prune_spill_files_at_deletes_an_aged_file_with_a_matching_marker() {
        let dir = spill_tmp("gate-copy");
        let top = write_aged(
            &dir,
            "seals_v4-2026-01-01.bin",
            SEAL_SPILL_RECORD_SIZE * 2,
            10_000,
        );
        crate::raw_frame_upload::write_file_marker_for_test(&top);
        let marker = dir
            .join(crate::raw_frame_upload::UPLOADED_SUBDIR)
            .join("seals_v4-2026-01-01.bin");
        assert!(marker.exists());
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::Required,
        );
        assert!(!top.exists());
        assert_eq!(out.deleted, 1);
        assert_eq!(out.deleted_with_copy, 1);
        // The records are in the cold bucket: not counted as lost.
        assert_eq!((out.deleted_non_empty, out.records_lost), (0, 0));
        assert_eq!(out.refused_not_uploaded, 0);
        assert!(!marker.exists(), "the deleted file's marker goes with it");
    }

    #[test]
    fn test_prune_spill_files_at_keeps_an_archive_file_without_a_marker() {
        let dir = spill_tmp("gate-archive");
        let archive = dir.join(crate::seal_writer_task::SEAL_ARCHIVE_SUBDIR);
        std::fs::create_dir_all(&archive).expect("mkdir archive");
        let unmarked = write_aged(&archive, "seals_v4-2026-01-01.bin", 256, 10_000);
        let marked = write_aged(&archive, "seals_v4-2026-01-02.bin", 128, 10_000);
        crate::raw_frame_upload::write_file_marker_for_test(&marked);
        // A marker for a file that changed since its upload covers nothing.
        let changed = write_aged(&archive, "seals_v4-2026-01-03.bin", 64, 10_000);
        crate::raw_frame_upload::write_file_marker_for_test(&changed);
        std::fs::write(&changed, vec![1_u8; 65]).expect("rewrite");
        let f = std::fs::File::options()
            .write(true)
            .open(&changed)
            .expect("reopen");
        f.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(10_000),
            ),
        )
        .expect("set mtime");
        let out = prune_spill_files_at(
            &dir,
            3_600,
            std::time::SystemTime::now(),
            CopyGate::Required,
        );
        assert!(unmarked.exists() && changed.exists());
        assert!(!marked.exists());
        assert_eq!(out.archive_deleted, 1);
        assert_eq!(out.archive_deleted_without_copy, 0);
        assert_eq!(out.refused_not_uploaded, 2);
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
        let out = prune_spill_files_at(&dir, 0, now, CopyGate::NotRequired);
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

/// Ordinal pins: the spill `tf_ordinal` byte round-trips every one of the
/// `TfIndex` frames (10 since 2026-09-22), the four byte-stable frames keep ordinals 0..=3, and
/// out-of-range ordinals refuse cleanly (`None`) — never panic.
///
/// ⚠ The module was named for the C3 second-scale change, which was
/// APPEND-ONLY. The 2026-09-19 nine-frame collapse was NOT: the ordinal
/// space was RENUMBERED, which is what a round-trip test cannot see on its
/// own — `from_ordinal(ord).as_ordinal() == ord` holds under ANY consistent
/// table. Only `SEAL_SPILL_FORMAT_VERSION` protects a record written under
/// the old table.
#[cfg(test)]
mod c3_tf_ordinal_pins {
    use tickvault_trading::candles::TfIndex;
    use tickvault_trading::candles::tf_index::TF_COUNT;

    #[test]
    fn test_tf_ordinal_roundtrip_covers_all_frames() {
        // 9 since 2026-09-19 (the operator's timeframe directive collapsed
        // 24 → 9 and RENUMBERED every survivor past M15 — which is why
        // `SEAL_SPILL_FORMAT_VERSION` is 4). The loop bound is derived from
        // TF_COUNT so a frame added or retired is actually exercised instead
        // of silently falling outside the range.
        assert_eq!(
            TF_COUNT, 10,
            "10 since 2026-09-22: M10 appended at ordinal 9 (a real table, not a view)"
        );
        for ord in 0..TF_COUNT {
            let tf =
                TfIndex::from_ordinal(ord).unwrap_or_else(|| panic!("ordinal {ord} must decode"));
            assert_eq!(tf.as_ordinal(), ord, "round-trip broke at {ord}");
        }
        // The four BYTE-STABLE frames keep ordinals 0..=3 across every
        // ordinal-space change this record has seen. `D1` sat at 4 until the
        // 2026-09-19 nine-frame collapse RETIRED it; `S1` took that slot.
        assert_eq!(TfIndex::M1.as_ordinal(), 0);
        assert_eq!(TfIndex::M3.as_ordinal(), 1);
        assert_eq!(TfIndex::M5.as_ordinal(), 2);
        assert_eq!(TfIndex::M15.as_ordinal(), 3);
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

#[cfg(test)]
mod pr41a_tests {
    use super::*;

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
            bucket_open_prev_close: 98.5,
            total_buy_qty: 89_600,
            total_sell_qty: 4_800,
            open_pct: 7.7,
            change_pct: 1.5,
            open_gap_pct: 0.8,
        }
    }

    fn temp_spill_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "tickvault-seal-spill-pr41a-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");
        dir
    }

    fn pr41a_now() -> i64 {
        Utc.with_ymd_and_hms(2026, 10, 1, 6, 0, 0)
            .single()
            .expect("valid")
            .timestamp()
    }

    /// One copy of a 1-minute bar with the given fullness.
    fn pr41a_copy(sid: u64, bucket: u32, ticks: u32, volume: u64) -> SerializedSeal {
        let mut seal = mk_seal(sid, 2, 0, bucket, 100.0);
        seal.tick_count = ticks;
        seal.volume = volume;
        seal
    }

    fn pr41a_buffered(seal: &SerializedSeal) -> BufferedSeal {
        seal.try_into_buffered_seal().expect("known timeframe")
    }

    #[test]
    fn pr41a_an_amended_copy_committed_live_is_appended_behind_its_spilled_original() {
        let dir = temp_spill_dir("pr41a-mirror");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = pr41a_now();
        let original = pr41a_copy(13, 1_727_760_000, 4, 40);
        writer.append_seal(&original, now).expect("spill original");

        // The live writer then commits the amended copy (a late tick added
        // one), plus an unrelated bar that was never spilled.
        let amended = pr41a_copy(13, 1_727_760_000, 5, 40);
        let unrelated = pr41a_copy(25, 1_727_760_000, 9, 90);
        let committed = [pr41a_buffered(&amended), pr41a_buffered(&unrelated)];
        assert_eq!(writer.note_live_commits(&committed, now), 1);

        let on_disk = writer.read_all(now).expect("read");
        assert_eq!(
            on_disk,
            vec![original, amended],
            "original first, fuller copy last"
        );
        // Committing the same copy again appends nothing.
        assert_eq!(writer.note_live_commits(&committed, now), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pr41a_an_older_copy_is_not_appended_after_the_fuller_one() {
        let dir = temp_spill_dir("pr41a-older");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = pr41a_now();
        let fuller = pr41a_copy(13, 1_727_760_000, 5, 40);
        let older = pr41a_copy(13, 1_727_760_000, 4, 40);
        writer.append_seal(&fuller, now).expect("spill fuller");
        // Reported as absorbed: the fuller copy is on disk.
        writer.append_seal(&older, now).expect("older is absorbed");
        let mut scratch = Vec::new();
        writer
            .append_seals([&older, &fuller], &mut scratch, now)
            .expect("batch");
        // An older BUCKET is always written: it cannot be amended any more.
        let previous_bucket = pr41a_copy(13, 1_727_759_940, 1, 1);
        writer
            .append_seal(&previous_bucket, now)
            .expect("older bucket");

        let on_disk = writer.read_all(now).expect("read");
        assert_eq!(on_disk, vec![fuller, fuller, previous_bucket]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_pr41a_replay_is_superseded_when_the_spill_holds_a_fuller_copy() {
        let dir = temp_spill_dir("pr41a-replay");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = pr41a_now();
        let original = pr41a_copy(13, 1_727_760_000, 4, 40);
        let amended = pr41a_copy(13, 1_727_760_000, 5, 40);

        // Nothing spilled yet: nothing is superseded, and no lock is taken.
        assert!(!writer.replay_is_superseded(&pr41a_buffered(&original)));

        writer.append_seal(&original, now).expect("spill original");
        assert!(!writer.replay_is_superseded(&pr41a_buffered(&original)));
        assert_eq!(
            writer.note_live_commits(&[pr41a_buffered(&amended)], now),
            1
        );
        assert!(writer.replay_is_superseded(&pr41a_buffered(&original)));
        assert!(!writer.replay_is_superseded(&pr41a_buffered(&amended)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_pr41a_note_live_commits_does_nothing_while_the_spill_has_held_nothing() {
        let dir = temp_spill_dir("pr41a-idle");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = pr41a_now();
        let committed = [pr41a_buffered(&pr41a_copy(13, 1_727_760_000, 5, 40))];
        assert_eq!(writer.note_live_commits(&committed, now), 0);
        assert!(!writer.spill_path(now).exists(), "no file is created");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_note_dead_lettered_lets_a_fuller_live_commit_mirror_to_the_spill() {
        // Z6: a copy that went to the dead-letter file is known to the ledger,
        // so the amended copy committed live afterwards reaches the spill.
        let dir = temp_spill_dir("z6-dlq-note");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = pr41a_now();
        let original = pr41a_copy(13, 1_727_760_000, 4, 40);
        let amended = pr41a_copy(13, 1_727_760_000, 5, 40);
        writer.note_dead_lettered(&original);
        assert_eq!(
            writer.note_live_commits(&[pr41a_buffered(&amended)], now),
            1
        );
        assert_eq!(writer.read_all(now).expect("read"), vec![amended]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_note_replay_commits_supersedes_an_older_replayed_copy() {
        let dir = temp_spill_dir("z6-replay-note");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = pr41a_now();
        let original = pr41a_copy(13, 1_727_760_000, 4, 40);
        let amended = pr41a_copy(13, 1_727_760_000, 5, 40);
        writer.append_seal(&original, now).expect("spill original");
        writer.note_replay_commits(&[pr41a_buffered(&amended)]);
        assert!(writer.replay_is_superseded(&pr41a_buffered(&original)));
        assert!(!writer.replay_is_superseded(&pr41a_buffered(&amended)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_escalation_pending_is_shared_and_note_escalation_finished_lowers_it() {
        let dir = temp_spill_dir("z6-escalation-count");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let pending = writer.escalation_pending();
        pending.fetch_add(3, Ordering::SeqCst);
        assert_eq!(writer.escalation_pending().load(Ordering::SeqCst), 3);
        writer.note_escalation_finished(2);
        assert_eq!(pending.load(Ordering::SeqCst), 1);
        writer.note_escalation_finished(1);
        assert_eq!(pending.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pr41a_a_failed_mirror_append_is_counted_and_reported_as_nothing_appended() {
        let dir = temp_spill_dir("pr41a-mirror-fails");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = pr41a_now();
        writer
            .append_seal(&pr41a_copy(13, 1_727_760_000, 4, 40), now)
            .expect("spill original");
        // Break the spill directory: the chaos suite's "disk dead" shape.
        writer.close_open_handle();
        std::fs::remove_dir_all(&dir).expect("remove");
        std::fs::write(&dir, b"not a directory").expect("block the dir");
        let amended = pr41a_buffered(&pr41a_copy(13, 1_727_760_000, 5, 40));
        assert_eq!(writer.note_live_commits(&[amended], now), 0);
        let _ = std::fs::remove_file(&dir);
    }
}

/// Audit PR41c: every spilled record can be checked, and no torn record
/// shifts the ones after it.
#[cfg(test)]
mod pr41c_tests {
    use super::*;
    use std::io::Write;

    fn seal(sid: u64, bucket: u32, close: f64) -> SerializedSeal {
        SerializedSeal {
            security_id: sid,
            exchange_segment_code: 2,
            tf_ordinal: 0,
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
            close_pct_from_prev_day: 0.5,
            bucket_open_prev_close: 99.5,
            total_buy_qty: 10,
            total_sell_qty: 20,
            open_pct: 0.1,
            change_pct: 0.2,
            open_gap_pct: 0.3,
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tv-seal-spill-pr41c-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn noon() -> i64 {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp()
    }

    /// The version-4 bytes the previous build wrote for `s`.
    fn v4_bytes(s: &SerializedSeal) -> [u8; SEAL_SPILL_RECORD_SIZE] {
        let mut bytes = s.to_bytes();
        bytes[0..4].copy_from_slice(&(s.security_id as u32).to_le_bytes());
        bytes[7] = 4;
        bytes
    }

    #[test]
    fn to_bytes_writes_a_checksum_of_bytes_4_to_128_at_bytes_0_to_4() {
        let s = seal(4_294_967_301, 1_716_000_900, 101.25);
        let bytes = s.to_bytes();
        assert_eq!(bytes[7], SEAL_SPILL_FORMAT_VERSION);
        assert_eq!(SEAL_SPILL_FORMAT_VERSION, SEAL_SPILL_FIRST_CHECKSUM_VERSION);
        assert_eq!(
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            crate::wal_applied_watermark::crc32_ieee(&bytes[4..])
        );
        assert_eq!(decode_spill_record(&bytes), SpillRecordRead::Seal(s));
    }

    #[test]
    fn decode_spill_record_refuses_a_flipped_bit_anywhere_in_the_record() {
        let s = seal(13, 1_716_000_900, 101.25);
        let bytes = s.to_bytes();
        for byte in 0..SEAL_SPILL_RECORD_SIZE {
            for bit in 0..8 {
                let mut damaged = bytes;
                damaged[byte] ^= 1 << bit;
                let read = decode_spill_record(&damaged);
                assert!(
                    matches!(
                        read,
                        SpillRecordRead::ChecksumMismatch | SpillRecordRead::OtherVersion
                    ),
                    "byte {byte} bit {bit}: a damaged record must never decode, got {read:?}"
                );
            }
        }
        // Byte 7 flipped from 5 to 4 reads as the previous format, whose id
        // check is what refuses it.
        let mut as_v4 = bytes;
        as_v4[7] = 4;
        assert_eq!(
            decode_spill_record(&as_v4),
            SpillRecordRead::ChecksumMismatch
        );
    }

    #[test]
    fn a_v4_record_written_by_the_previous_build_still_decodes() {
        let s = seal(4_294_967_301, 1_716_000_900, 101.25);
        let old = v4_bytes(&s);
        assert!(seal_spill_version_is_readable(4));
        assert!(seal_spill_version_is_readable(SEAL_SPILL_FORMAT_VERSION));
        assert!(!seal_spill_version_is_readable(3));
        assert!(!seal_spill_version_is_readable(
            SEAL_SPILL_FORMAT_VERSION + 1
        ));
        assert_eq!(decode_spill_record(&old), SpillRecordRead::Seal(s));
        // A v4 record from before the id widening (zero full id) has nothing
        // to compare and decodes from its low-32 id.
        let mut narrow = old;
        narrow[120..128].copy_from_slice(&[0; 8]);
        match decode_spill_record(&narrow) {
            SpillRecordRead::Seal(read) => assert_eq!(read.security_id, 5),
            other => panic!("a pre-widening v4 record must decode, got {other:?}"),
        }
    }

    #[test]
    fn test_reseal_record_for_test_restores_a_valid_checksum() {
        let mut bytes = seal(13, 1_716_000_900, 101.25).to_bytes();
        bytes[64] ^= 0xFF;
        assert_eq!(
            decode_spill_record(&bytes),
            SpillRecordRead::ChecksumMismatch
        );
        reseal_record_for_test(&mut bytes);
        assert!(matches!(
            decode_spill_record(&bytes),
            SpillRecordRead::Seal(_)
        ));
    }

    #[test]
    fn read_all_skips_a_damaged_record_and_keeps_reading() {
        let dir = dir("damaged-middle");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let now = noon();
        let (a, b, c) = (
            seal(1, 1_716_000_900, 1.0),
            seal(2, 1_716_000_900, 2.0),
            seal(3, 1_716_000_900, 3.0),
        );
        for s in [&a, &b, &c] {
            writer.append_seal(s, now).expect("append");
        }
        let path = writer.spill_path(now);
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[SEAL_SPILL_RECORD_SIZE + 64] ^= 0x01;
        std::fs::write(&path, &bytes).expect("write");
        assert_eq!(writer.read_all(now).expect("read_all"), vec![a, c]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cut_back_to_whole_records_removes_only_a_partial_record() {
        let dir = dir("cut-back");
        let path = dir.join("f.bin");
        std::fs::write(&path, vec![7u8; 2 * SEAL_SPILL_RECORD_SIZE + 50]).expect("write");
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        assert_eq!(cut_back_to_whole_records(&file).expect("cut"), 50);
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(),
            2 * SEAL_SPILL_RECORD_SIZE as u64
        );
        assert_eq!(cut_back_to_whole_records(&file).expect("cut"), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_torn_day_file_is_cut_back_when_it_is_opened() {
        let dir = dir("torn-on-open");
        let now = noon();
        let (a, b) = (seal(1, 1_716_000_900, 1.0), seal(2, 1_716_000_900, 2.0));
        let first = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        first.append_seal(&a, now).expect("append a");
        let path = first.spill_path(now);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            f.write_all(&[0xAB; 50]).expect("torn write");
        }
        // A new writer (the next process) opens the file and appends.
        let next = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        next.append_seal(&b, now).expect("append b");
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(),
            2 * SEAL_SPILL_RECORD_SIZE as u64
        );
        assert_eq!(next.read_all(now).expect("read_all"), vec![a, b]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_batch_starts_on_a_record_boundary_even_on_an_open_handle() {
        let dir = dir("batch-aligned");
        let now = noon();
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        let (a, b, c) = (
            seal(1, 1_716_000_900, 1.0),
            seal(2, 1_716_000_900, 2.0),
            seal(3, 1_716_000_900, 3.0),
        );
        writer.append_seal(&a, now).expect("append a");
        let path = writer.spill_path(now);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            f.write_all(&[0xAB; 50]).expect("torn write");
        }
        let mut scratch = Vec::new();
        writer
            .append_seals([&b, &c], &mut scratch, now)
            .expect("batch");
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(),
            3 * SEAL_SPILL_RECORD_SIZE as u64
        );
        assert_eq!(writer.read_all(now).expect("read_all"), vec![a, b, c]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Plan item 51d: the readiness wait counts a day's record files at the
    /// top level and in `replaying/`, renamed copies included, and nothing
    /// else; a missing folder counts zero.
    #[test]
    fn test_staged_spill_records_for_day_counts_only_that_days_record_files() {
        use crate::seal_writer_task::{SEAL_FILE_PREFIX, SEAL_REPLAYING_SUBDIR};
        let dir = super::tests::temp_spill_dir("staged-for-day");
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 9).unwrap_or_default();
        assert_eq!(
            staged_spill_records_for_day(&dir.join("absent"), day).ok(),
            Some(SpillStaged {
                staged: 0,
                parked: crate::seal_writer_task::parked_replay_files(),
            })
        );
        let replaying = dir.join(SEAL_REPLAYING_SUBDIR);
        std::fs::create_dir_all(&replaying).expect("replaying dir");
        let name = |d: &str, suffix: &str| format!("{SEAL_FILE_PREFIX}{d}.bin{suffix}");
        for (folder, file) in [
            (&dir, name("2026-10-09", "")),
            (&dir, name("2026-10-09", ".1")),
            (&replaying, name("2026-10-09", "")),
            (&dir, name("2026-10-08", "")),
            (&replaying, name("2026-10-10", ".overflow")),
            (&dir, format!("{SEAL_FILE_PREFIX}2026-10-09.tmp")),
            (&dir, "boot-committed.summary".to_string()),
        ] {
            std::fs::write(folder.join(file), b"x").expect("write");
        }
        // A directory carrying a record name is not a file.
        std::fs::create_dir_all(dir.join("archive").join(name("2026-10-09", ""))).expect("dir");
        std::fs::create_dir_all(replaying.join(name("2026-10-09", ".2"))).expect("dir");
        let got = staged_spill_records_for_day(&dir, day).expect("listable");
        assert_eq!(got.staged, 3, "{got:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
