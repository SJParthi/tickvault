//! Which spilled tables this process may POST over ILP (2026-11-07).
//!
//! # Why this exists (measured on the live box, 2026-11-05 to 2026-11-07)
//!
//! QuestDB creates a table on the first ILP row for a name it does not know
//! (`line.auto.create.new.tables`, default true). That table gets the shape
//! ILP auto-create gives it: designated timestamp named `timestamp`,
//! `PARTITION BY DAY`, no DEDUP key. It can never take our DEDUP key, because
//! every key here starts with `ts`.
//!
//! The spill replay (`tick_spill_replay`, `depth_spill_frame`) is spawned in
//! `main` BEFORE the Dhan lane runs `ensure_market_depth_book_table`, and the
//! book ensure runs only with `[depth_storage] array_rows` on. Outside 08:00
//! to 15:40 IST the depth replay posts its first round at once, so a framed
//! `market_depth_book` record reached a QuestDB that had no such table (a
//! fresh volume after a rebuild, or a dropped table) and QuestDB created it
//! with `timestamp`. Every later boot then refused array rows (LH-01), and
//! the 6 Nov session wrote 672,830,990 `market_depth` level rows (~49 GB)
//! instead of book rows, which took the 150 GB root disk to 98%.
//!
//! # The rule
//!
//! A spilled row for one of the five tables the replay writes is POSTed only
//! once this process has ensured that table (its CREATE and DEDUP statements
//! were all accepted). Until then the file is HELD: nothing is posted from
//! it, its resume offset stays where it was, every byte stays on disk, and
//! the next round tries again. Nothing is set aside or deleted, so no row is
//! lost; a hold only waits.
//!
//! A table that exists with the wrong shape fails its ensure (its DEDUP key
//! is refused), so its rows are held too, instead of being poured into a
//! table that the boot self-heal can rename aside only while it is EMPTY.
//!
//! # Cost
//!
//! Cold path only (the replay task, once per chunk of up to 8 MiB). One
//! Acquire load per round ([`EnsuredTables::current`]), one Release
//! `fetch_or` per successful ensure, and a scan of each line's measurement
//! name (bytes up to the first `,` or space) on the text tier. No allocation.
//! Never on the frame drain, never per tick.
//!
//! # Honest limits
//!
//! * A measurement name that is none of the five is NOT held: no writer here
//!   spills another table, and holding an unknown name (for example the front
//!   of a torn line) would wedge its file for good. Such a line goes to
//!   QuestDB as before, and a malformed one is set aside by line isolation.
//! * The flags are per process and are never cleared: a table DROPPED while
//!   the app runs reads as still ensured. Every deletion runbook here stops
//!   the app first.
//! * In a binary ILP v2 framed record only the FIRST row's name is read (a
//!   binary double can hold a newline byte). Every framed record is one
//!   writer's buffer, so it holds rows of one table.
//! * With `array_rows` off the book table is never ensured, so framed book
//!   records left from an earlier on-period are held until it is turned on
//!   again (kept on disk, counted, never posted into an auto-created table).
//! * A refused book ensure is retried in the background on the doubling
//!   delay (30 s to 10 min, `candle_ddl_boot::DdlTables::MarketDepthBook`),
//!   which sets the flag on success; the book WRITER stays armed only from
//!   the boot's outcome. A table with the wrong shape keeps refusing outside
//!   08:00 to 15:40 IST (the self-heal runs only inside it), so its records
//!   wait for the next in-window boot or retry.
//! * A file held here does NOT keep the after-close drain hold up
//!   ([`file_waits_only_on_unensured_table`], read by `drain_hold`): a held
//!   file cannot drain whatever the box does, and counting it held the box
//!   to 07:30 IST every night (~14 paid hours). The remaining cost: its rows
//!   are not in the database that night; they stay in the spill folder and
//!   replay once the table is ensured (the next boot that ensures it). At
//!   most [`HELD_FILES_MAX`] files are recorded; a file past it counts as
//!   work, the direction that keeps the box up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

/// The tables the spill replay writes. Every name is the writer's own
/// constant, so a rename follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatedTable {
    /// `ticks`.
    Ticks,
    /// `tick_quotes` (same writer and spill as `ticks`).
    TickQuotes,
    /// `feed_aux_packets` (same writer and spill as `ticks`).
    FeedAuxPackets,
    /// `market_depth` (level rows).
    MarketDepth,
    /// `market_depth_book` (array rows, ILP v2).
    MarketDepthBook,
}

impl GatedTable {
    /// Every gated table.
    pub const ALL: [Self; 5] = [
        Self::Ticks,
        Self::TickQuotes,
        Self::FeedAuxPackets,
        Self::MarketDepth,
        Self::MarketDepthBook,
    ];

    /// The QuestDB table name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ticks => tickvault_common::constants::QUESTDB_TABLE_TICKS,
            Self::TickQuotes => crate::tick_quotes_persistence::TICK_QUOTES_TABLE,
            Self::FeedAuxPackets => crate::feed_aux_persistence::FEED_AUX_PACKETS_TABLE,
            Self::MarketDepth => crate::depth_persistence::MARKET_DEPTH_TABLE,
            Self::MarketDepthBook => crate::depth_persistence::MARKET_DEPTH_BOOK_TABLE,
        }
    }

    const fn bit(self) -> u8 {
        match self {
            Self::Ticks => 1,
            Self::TickQuotes => 1 << 1,
            Self::FeedAuxPackets => 1 << 2,
            Self::MarketDepth => 1 << 3,
            Self::MarketDepthBook => 1 << 4,
        }
    }

    /// The gated table an ILP measurement name names, or `None`. O(5)
    /// byte-slice compares.
    #[must_use]
    pub fn from_measurement(name: &[u8]) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name().as_bytes() == name)
    }
}

/// Bit per [`GatedTable`]: set once that table's ensure succeeded in this
/// process. Never cleared.
static ENSURED: AtomicU8 = AtomicU8::new(0);

/// Records that `table`'s ensure (CREATE plus DEDUP) was accepted by QuestDB
/// in this process. Called by the ensure functions only. O(1).
pub fn mark_table_ensured(table: GatedTable) {
    ENSURED.fetch_or(table.bit(), Ordering::Release);
}

/// Whether `ddl` is one of the statements that key a table: its CREATE or its
/// `DEDUP ENABLE`. The `ADD COLUMN IF NOT EXISTS` self-heals are best-effort
/// (ILP adds a missing column itself; a refused one is a gap, not a table that
/// dedups wrongly forever), the same split as the cross-verify tables'
/// `xverify_ddl_is_key_bearing`. Pure.
#[must_use]
pub fn ddl_is_key_bearing(ddl: &str) -> bool {
    ddl.starts_with("CREATE") || ddl.contains("DEDUP ENABLE")
}

/// Records that every KEY-BEARING statement of `table`'s ensure (see
/// [`ddl_is_key_bearing`]) was accepted, even if a column self-heal was
/// refused. Sets the same bit as [`mark_table_ensured`]: the gates (live
/// writers and spill replay) only need the table to exist with its key.
///
/// B1 review (2026-11-13): before this, the bit needed EVERY statement, so one
/// refused `ADD COLUMN` on an already-keyed table shut the live tick writer
/// and the spill replay for the whole session. O(1).
pub fn mark_table_keyed(table: GatedTable) {
    ENSURED.fetch_or(table.bit(), Ordering::Release);
}

/// A snapshot of which gated tables are ensured. `Copy`, so a replay round
/// judges every file against one reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnsuredTables(u8);

impl EnsuredTables {
    /// Every table ensured (test views, which predate the gate).
    pub const ALL: Self = Self(0b1_1111);
    /// No table ensured (a process before its boot ensures ran).
    pub const NONE: Self = Self(0);

    /// The process's reading now. One Acquire load.
    #[must_use]
    pub fn current() -> Self {
        Self(ENSURED.load(Ordering::Acquire))
    }

    /// Whether `table` is ensured in this snapshot.
    #[must_use]
    pub const fn contains(self, table: GatedTable) -> bool {
        self.0 & table.bit() != 0
    }

    /// This snapshot with `table` also ensured.
    #[must_use]
    pub const fn with(self, table: GatedTable) -> Self {
        Self(self.0 | table.bit())
    }
}

/// The measurement name of one ILP line: the bytes before the first
/// unescaped `,` or space (a leading `\` escapes the next byte). O(name).
#[must_use]
pub fn measurement_name(line: &[u8]) -> &[u8] {
    let mut at = 0_usize;
    while let Some(&b) = line.get(at) {
        match b {
            b'\\' => at = at.saturating_add(2),
            b',' | b' ' | b'\n' => return line.get(..at).unwrap_or(line),
            _ => at = at.saturating_add(1),
        }
    }
    line
}

/// The first gated table named by a line of the text chunk `chunk` that is
/// not in `ensured`, or `None` when every line may be posted. O(chunk).
#[must_use]
pub fn first_unensured_in_lines(chunk: &[u8], ensured: EnsuredTables) -> Option<GatedTable> {
    chunk
        .split(|b| *b == b'\n')
        .filter_map(|line| GatedTable::from_measurement(measurement_name(line)))
        .find(|table| !ensured.contains(*table))
}

/// [`first_unensured_in_lines`] for one framed depth record: a text (ILP v1)
/// payload is judged line by line, a binary (v2) payload by its first row's
/// name only (see the module's honest limits).
#[must_use]
pub fn first_unensured_in_record(
    ilp_version: u8,
    payload: &[u8],
    ensured: EnsuredTables,
) -> Option<GatedTable> {
    if ilp_version <= 1 {
        return first_unensured_in_lines(payload, ensured);
    }
    GatedTable::from_measurement(measurement_name(payload)).filter(|t| !ensured.contains(*t))
}

// ---------------------------------------------------------------------------
// The LIVE writers' gate (B1, 2026-11-13).
//
// The spill replay above was gated; the live `ticks` / `tick_quotes` /
// `feed_aux_packets` writer and the `market_depth` level writer were not.
// When the boot's bounded live-table DDL gave up (QuestDB still replaying its
// WAL), the first live flush made QuestDB create the table itself, with the
// `timestamp` shape that can never take the DEDUP key, and the background
// re-run could never repair it. Once the gate is ARMED (by the boot's
// live-table DDL step, `candle_ddl_boot::run_live_table_ddl_at_boot`, before
// its first ensure), a live writer posts only into tables this process has
// ensured; until then each batch takes the writer's existing rescue (the
// spill file, which the replay above posts once the table is ensured, or the
// WAL deferral). Nothing is dropped. One Acquire load per FLUSH, never per
// row. A process that never runs the boot DDL step (a unit test driving a
// writer against a mock server) never arms it and posts as before.
// ---------------------------------------------------------------------------

/// Set once the boot's live-table DDL step starts. Never cleared.
static LIVE_WRITE_GATE_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Arms the live writers' gate. Called by the boot's live-table DDL step
/// before its first ensure. O(1).
pub fn arm_live_write_gate() {
    LIVE_WRITE_GATE_ARMED.store(true, Ordering::Release);
}

/// The tables one live `ticks` writer batch can hold rows for.
pub const LIVE_TICK_TABLES: [GatedTable; 3] = [
    GatedTable::Ticks,
    GatedTable::TickQuotes,
    GatedTable::FeedAuxPackets,
];

/// The tables a live tick batch's TICK rows go to (B1 review 2): `ticks` and
/// `tick_quotes`. Its `feed_aux_packets` rows are judged on their own
/// ([`LIVE_TICK_AUX_TABLES`]), so an auxiliary table that cannot be keyed
/// never holds the ticks back: the writer thread sets only the aux rows aside
/// to the spill and posts the rest. B1 review 3: the writer thread now judges
/// each of the three tables on its own (`tick_quotes` rows are set aside the
/// same way), and the replay check reads this set through
/// `catchup_stop::replay_ready`, which drops an un-keyable `tick_quotes`.
pub const LIVE_TICK_MAIN_TABLES: [GatedTable; 2] = [GatedTable::Ticks, GatedTable::TickQuotes];

/// The table a live tick batch's auxiliary rows go to (B1 review 2).
pub const LIVE_TICK_AUX_TABLES: [GatedTable; 1] = [GatedTable::FeedAuxPackets];

/// Bit per [`GatedTable`]: set once this process found the table EXISTING in
/// a shape that can never take its DEDUP key (designated timestamp not `ts`:
/// QuestDB created it itself from an ILP row before our CREATE ran). No DDL
/// re-run can repair that, so the live gate stays shut for that table until
/// someone repairs it by hand (B1 review 2). Never cleared.
static UNKEYABLE: AtomicU8 = AtomicU8::new(0);

/// Whether `table` was found un-keyable in this process (see [`UNKEYABLE`]).
/// One Acquire load.
#[must_use]
pub fn table_unkeyable(table: GatedTable) -> bool {
    UNKEYABLE.load(Ordering::Acquire) & table.bit() != 0
}

/// The designated timestamp column of a table's `table_columns()` answer, or
/// `None` when it has none. O(columns), cold.
fn designated_timestamp(columns: &[crate::depth_persistence::TableColumn]) -> Option<&str> {
    // O(1) EXEMPT: boot-only, once per refused ensure, over the table's few columns.
    columns
        .iter()
        .find(|c| c.designated)
        .map(|c| c.name.as_str())
}

/// Whether an EXISTING table (non-empty `columns`) cannot take a DEDUP key on
/// `ts` because its designated timestamp is something else (or missing).
/// Pure, O(columns).
#[must_use]
pub(crate) fn columns_are_unkeyable(columns: &[crate::depth_persistence::TableColumn]) -> bool {
    !columns.is_empty() && designated_timestamp(columns) != Some("ts")
}

/// Request timeout, in seconds, of the one shape probe
/// [`diagnose_refused_key`] makes (the DDL client's own bound).
const DIAGNOSE_TIMEOUT_SECS: u64 = 10;

/// Called by an ensure whose KEY-BEARING statement was refused (B1 review 2):
/// reads `table`'s columns once and, when the table exists with a designated
/// timestamp other than `ts`, records it un-keyable and logs ONE coded error
/// per table per process saying the live feed for it is HELD and how to
/// repair it. A table that is merely missing, or an answer it cannot read, is
/// left alone (the re-run may still key it). Returns whether it is
/// un-keyable. One HTTP query, cold (only after a refusal).
pub async fn diagnose_refused_key(
    questdb_config: &tickvault_common::config::QuestDbConfig,
    table: GatedTable,
) -> bool {
    // APPROVED: QuestDB base URL, once per refused ensure
    let url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(DIAGNOSE_TIMEOUT_SECS))
        .build()
    else {
        return false;
    };
    // APPROVED: boot-only probe query
    let query = format!("SELECT * FROM table_columns('{}')", table.name());
    let Ok(resp) = client
        .get(&url)
        .query(&[("query", query.as_str())])
        .send()
        .await
    else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    let Ok(body) = resp.text().await else {
        return false;
    };
    let Some(columns) = crate::depth_persistence::parse_table_columns_answer(&body) else {
        return false;
    };
    if !columns_are_unkeyable(&columns) {
        return false;
    }
    let before = UNKEYABLE.fetch_or(table.bit(), Ordering::AcqRel);
    if before & table.bit() == 0 {
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::HotPath02WriterQueueDrop.code_str(),
            source = "live_table_unkeyable",
            table = table.name(),
            designated = designated_timestamp(&columns).unwrap_or("none"),
            "a live table exists in a shape that can never take its DEDUP key (its designated \
             timestamp is not ts: QuestDB created it itself from a row written before our \
             CREATE). No automatic re-run can repair it, so the live feed for this table is \
             HELD: its rows stay in the spill files and the frame log and are not in QuestDB \
             until the table is repaired by hand (rename it aside, then restart so the boot \
             creates it with ts and the key)."
        );
    }
    true
}

/// Pure: whether a live batch with rows for `tables` may be posted, given
/// whether the gate is `armed` and the `ensured` snapshot. O(tables).
#[must_use]
pub fn live_batch_writable(armed: bool, ensured: EnsuredTables, tables: &[GatedTable]) -> bool {
    !armed || tables.iter().all(|t| ensured.contains(*t))
}

/// Whether a live batch with rows for `tables` may be posted now. Two Acquire
/// loads; called once per flush, never per row.
#[must_use]
// WIRING-EXEMPT: called through the `table_gate` fn pointer TickWriter and DepthWriter hold (set in their constructors), never by name with parentheses.
pub fn live_tables_writable(tables: &[GatedTable]) -> bool {
    live_batch_writable(
        LIVE_WRITE_GATE_ARMED.load(Ordering::Acquire),
        EnsuredTables::current(),
        tables,
    )
}

/// Edge latch for [`note_live_batch_held`]: one line per process.
static LIVE_HOLD_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Logs, once per process, that a live writer rescued a batch because its
/// table is not yet keyed. The per-batch rescue line names each batch. Cold.
pub fn note_live_batch_held(writer: &'static str) {
    if !LIVE_HOLD_LOGGED.swap(true, Ordering::AcqRel) {
        tracing::warn!(
            code = tickvault_common::error_code::ErrorCode::HotPath02WriterQueueDrop.code_str(),
            source = "live_write_held_table_not_keyed",
            writer,
            "a live writer is not posting: its table has not been created with its DEDUP key \
             in this process yet (the boot DDL was refused). Posting now would let QuestDB \
             create the table itself in a shape that can never take the key. Its batches go \
             to the spill file (or stay in the frame log) and are written once the DDL \
             re-run keys the table. Until then those rows are on disk, NOT in QuestDB; if the \
             table already exists in the wrong shape (see source live_table_unkeyable) no \
             re-run can key it and the rows wait for a repair by hand."
        );
    }
}

/// Edge latch for [`note_replay_held_table_not_ensured`]'s log line: one bit
/// per table, one line per table per process.
static HOLD_LOGGED: AtomicU8 = AtomicU8::new(0);

/// The most held files [`HELD_FILES`] records. A spill folder holds about
/// one file per hour per tier, so a real backlog is far below this; past it a
/// held file is not recorded and counts as work (keeps the box up).
pub const HELD_FILES_MAX: usize = 4_096;

/// Spill files the replay last held, with the resume offset it held at and
/// the table it waited for. Read by `drain_hold` through
/// [`file_waits_only_on_unensured_table`]. An entry goes stale on its own
/// (the offset moves, or the table becomes ensured), so nothing removes one.
static HELD_FILES: Mutex<Option<HashMap<PathBuf, (u64, GatedTable)>>> = Mutex::new(None);

/// Records that the replay held `path` at resume offset `held_at` for
/// `table`. One lock and one hash insert per held file per round; cold.
fn record_held_file(path: &Path, held_at: u64, table: GatedTable) {
    let Ok(mut guard) = HELD_FILES.lock() else {
        return; // poisoned: the file then counts as work, the safe direction
    };
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() < HELD_FILES_MAX || map.contains_key(path) {
        map.insert(path.to_path_buf(), (held_at, table));
    }
}

/// Whether the only thing left in `path` from resume offset `resume_offset`
/// is rows the replay is holding for a table this process has not ensured.
/// `false` (counts as work) when the replay recorded no hold for it, held it
/// at another offset, or the table has since been ensured (the next round
/// will post it). One lock and one hash probe; cold (the drain-hold
/// supervisor, once a minute per spill file).
#[must_use]
pub fn file_waits_only_on_unensured_table(path: &Path, resume_offset: u64) -> bool {
    held_and_unensured(path, resume_offset, EnsuredTables::current())
}

/// [`file_waits_only_on_unensured_table`] against the snapshot `ensured`.
pub(crate) fn held_and_unensured(path: &Path, resume_offset: u64, ensured: EnsuredTables) -> bool {
    let Ok(guard) = HELD_FILES.lock() else {
        return false;
    };
    guard
        .as_ref()
        .and_then(|map| map.get(path))
        .is_some_and(|(held_at, table)| *held_at == resume_offset && !ensured.contains(*table))
}

/// Counts, records for `drain_hold` and (once per table per process) logs a
/// spill file held at resume offset `held_at` because `table` is not yet
/// ensured. Cold path, once per held file per round.
pub fn note_replay_held_table_not_ensured(
    dir_label: &'static str,
    table: GatedTable,
    path: &Path,
    held_at: u64,
) {
    record_held_file(path, held_at, table);
    metrics::counter!(
        crate::tick_spill_replay::SPILL_REPLAY_HELD_COUNTER,
        "dir" => dir_label,
        "reason" => "table_not_ensured"
    )
    .increment(1);
    if HOLD_LOGGED.fetch_or(table.bit(), Ordering::AcqRel) & table.bit() == 0 {
        tracing::warn!(
            code = tickvault_common::error_code::ErrorCode::TickSpill01FileQuarantined.code_str(),
            source = "spill_replay_held_table_not_ensured",
            table = table.name(),
            path = %path.display(),
            "spill replay is holding a file: it holds rows for a table this process has not \
             yet created with its DEDUP key. Posting them now would let QuestDB create the \
             table itself, with its timestamp named `timestamp`, a shape that can never take \
             the DEDUP key. The file is kept as it is and replayed once the table is ensured; \
             nothing is lost."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A QuestDB stand-in for the un-keyable case: it refuses every
    /// `DEDUP ENABLE` the way QuestDB does for a table it created itself, and
    /// answers `table_columns` with a designated timestamp named `timestamp`.
    fn auto_created_table_server() -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut buf = [0u8; 8_192];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&request);
                let body = if head.contains("table_columns") {
                    r#"{"columns":[{"name":"column","type":"STRING"},{"name":"type","type":"STRING"},{"name":"designated","type":"BOOLEAN"}],"dataset":[["timestamp","TIMESTAMP",true],["kind","SYMBOL",false]]}"#
                } else if head.contains("DEDUP") {
                    ""
                } else {
                    "{}"
                };
                let status = if body.is_empty() {
                    "400 Bad Request"
                } else {
                    "200 OK"
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        port
    }

    /// Regression, B1 review 2 (2026-11-13): a live table QuestDB created
    /// itself (designated timestamp `timestamp`, so `DEDUP ENABLE ... (ts, ..)`
    /// is refused with "column not found [ts]") was held silently all
    /// session: the gate stayed shut, the re-run retried every 30 s, and the
    /// only line said "nothing is lost". The ensure now probes the table's
    /// shape after QuestDB refuses a key statement and records it
    /// un-keyable (one coded error per table per process).
    #[tokio::test]
    async fn test_regression_diagnose_refused_key_marks_an_auto_created_table_unkeyable() {
        let cfg = tickvault_common::config::QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: auto_created_table_server(),
            pg_port: 1,
            ilp_port: 1,
        };
        assert!(!table_unkeyable(GatedTable::FeedAuxPackets));
        assert!(!crate::feed_aux_persistence::ensure_feed_aux_table(&cfg).await);
        assert!(
            table_unkeyable(GatedTable::FeedAuxPackets),
            "the refused key on an auto-created table is recorded"
        );
    }

    /// The shape check itself: only an EXISTING table whose designated
    /// timestamp is not `ts` is un-keyable; a missing table is not.
    #[test]
    fn test_columns_are_unkeyable_only_for_an_existing_wrong_shape() {
        use crate::depth_persistence::TableColumn;
        let col = |name: &str, designated: bool| TableColumn {
            name: name.to_string(),
            ty: "TIMESTAMP".to_string(),
            designated,
        };
        assert!(
            !columns_are_unkeyable(&[]),
            "no table yet: CREATE can make it"
        );
        assert!(!columns_are_unkeyable(&[col("ts", true), col("x", false)]));
        assert!(columns_are_unkeyable(&[
            col("timestamp", true),
            col("ts", false)
        ]));
        assert!(
            columns_are_unkeyable(&[col("ts", false)]),
            "no designated timestamp"
        );
    }

    /// Regression, B1 (2026-11-13): once armed, a live batch posts only when
    /// EVERY table it can hold rows for is ensured; unarmed (no boot DDL step
    /// in this process) it posts as before.
    #[test]
    fn test_live_batch_writable_regression_armed_gate_refuses_an_unkeyed_table() {
        let none = EnsuredTables::NONE;
        assert!(!live_batch_writable(true, none, &LIVE_TICK_TABLES));
        let ticks_only = none.with(GatedTable::Ticks).with(GatedTable::TickQuotes);
        assert!(
            !live_batch_writable(true, ticks_only, &LIVE_TICK_TABLES),
            "feed_aux_packets rides the same batch, so it must be keyed too"
        );
        let all = ticks_only.with(GatedTable::FeedAuxPackets);
        assert!(live_batch_writable(true, all, &LIVE_TICK_TABLES));
        assert!(!live_batch_writable(true, all, &[GatedTable::MarketDepth]));
        assert!(live_batch_writable(false, none, &LIVE_TICK_TABLES));
        assert!(live_batch_writable(false, none, &[GatedTable::MarketDepth]));
    }

    /// B1 review: only a CREATE or a `DEDUP ENABLE` decides the keyed verdict;
    /// a column self-heal does not.
    #[test]
    fn test_ddl_is_key_bearing_splits_create_and_dedup_from_column_self_heals() {
        let ticks = crate::tick_persistence::ticks_ensure_statements();
        let keyed = ticks.iter().filter(|d| ddl_is_key_bearing(d)).count();
        assert_eq!(keyed, 2, "the CREATE and the DEDUP ENABLE");
        assert!(ticks.iter().any(|d| d.contains("ADD COLUMN")));
        assert!(!ddl_is_key_bearing(
            "ALTER TABLE ticks ADD COLUMN IF NOT EXISTS x LONG"
        ));
    }

    /// `mark_table_keyed` opens the same bit `mark_table_ensured` does.
    #[test]
    fn test_mark_table_keyed_sets_the_process_bit() {
        mark_table_keyed(GatedTable::MarketDepthBook);
        assert!(EnsuredTables::current().contains(GatedTable::MarketDepthBook));
    }

    /// The hold line is written once per process, however many batches wait.
    #[test]
    fn test_note_live_batch_held_logs_once_per_process() {
        note_live_batch_held("ticks");
        assert!(LIVE_HOLD_LOGGED.load(Ordering::Acquire));
        note_live_batch_held("market_depth");
        assert!(LIVE_HOLD_LOGGED.load(Ordering::Acquire));
    }

    #[test]
    fn test_from_measurement_reads_the_writer_constant_names() {
        let names: Vec<&str> = GatedTable::ALL.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            [
                "ticks",
                "tick_quotes",
                "feed_aux_packets",
                "market_depth",
                "market_depth_book"
            ]
        );
        for t in GatedTable::ALL {
            assert_eq!(GatedTable::from_measurement(t.name().as_bytes()), Some(t));
        }
        // A prefix or a longer name is not a gated table.
        assert_eq!(
            GatedTable::from_measurement(b"market_depth_book_lh01"),
            None
        );
        assert_eq!(GatedTable::from_measurement(b"tick"), None);
    }

    #[test]
    fn test_measurement_name_stops_at_comma_space_or_newline() {
        assert_eq!(measurement_name(b"ticks,security_id=1 a=1 5"), b"ticks");
        assert_eq!(
            measurement_name(b"market_depth_book a=1 5\n"),
            b"market_depth_book"
        );
        assert_eq!(measurement_name(b"a\\,b,c=1 x=1"), b"a\\,b");
        assert_eq!(measurement_name(b"bare"), b"bare");
        assert_eq!(measurement_name(b""), b"");
    }

    #[test]
    fn test_ensured_tables_snapshot_bits() {
        let none = EnsuredTables::NONE;
        for t in GatedTable::ALL {
            assert!(!none.contains(t));
            assert!(EnsuredTables::ALL.contains(t));
            let one = none.with(t);
            assert!(one.contains(t));
            for other in GatedTable::ALL.into_iter().filter(|o| *o != t) {
                assert!(!one.contains(other), "{t:?} must not set {other:?}");
            }
        }
        let all = GatedTable::ALL
            .into_iter()
            .fold(EnsuredTables::NONE, EnsuredTables::with);
        assert_eq!(all, EnsuredTables::ALL);
    }

    /// The process flag is set by `mark_table_ensured` and read by `current`.
    /// Other tests may set other bits concurrently, so only this table's bit
    /// is asserted.
    #[test]
    fn test_mark_table_ensured_sets_the_process_bit() {
        mark_table_ensured(GatedTable::FeedAuxPackets);
        assert!(EnsuredTables::current().contains(GatedTable::FeedAuxPackets));
    }

    #[test]
    fn test_first_unensured_in_lines_finds_the_book_line() {
        let chunk = b"market_depth,security_id=1 a=1 5\nmarket_depth_book,security_id=1 b=2 6\n";
        let only_levels = EnsuredTables::NONE.with(GatedTable::MarketDepth);
        assert_eq!(
            first_unensured_in_lines(chunk, only_levels),
            Some(GatedTable::MarketDepthBook)
        );
        assert_eq!(
            first_unensured_in_lines(chunk, only_levels.with(GatedTable::MarketDepthBook)),
            None
        );
        // Unknown names are never held (see the honest limits).
        assert_eq!(
            first_unensured_in_lines(b"other_table a=1 5\n", EnsuredTables::NONE),
            None
        );
        assert_eq!(first_unensured_in_lines(b"", EnsuredTables::NONE), None);
    }

    #[test]
    fn test_first_unensured_in_record_reads_v2_by_first_row_only() {
        // A v2 payload whose binary part holds a newline followed by bytes
        // that look like another name: only the first row is judged.
        let mut v2 = b"market_depth_book,security_id=1 bid=".to_vec();
        v2.extend_from_slice(&[0x0e, b'\n', b't', b'i', b'c', b'k', b's', b' ']);
        let ensured_book = EnsuredTables::NONE.with(GatedTable::MarketDepthBook);
        assert_eq!(first_unensured_in_record(2, &v2, ensured_book), None);
        assert_eq!(
            first_unensured_in_record(2, &v2, EnsuredTables::NONE),
            Some(GatedTable::MarketDepthBook)
        );
        // A v1 record is judged line by line.
        let v1 = b"market_depth a=1 5\nticks a=1 6\n";
        assert_eq!(
            first_unensured_in_record(1, v1, EnsuredTables::NONE.with(GatedTable::MarketDepth)),
            Some(GatedTable::Ticks)
        );
    }

    #[test]
    fn test_note_replay_held_table_not_ensured_runs() {
        // Twice: the second call takes the latched arm (no second log line).
        let path = Path::new("/x/note-held.dspl");
        note_replay_held_table_not_ensured("depth", GatedTable::MarketDepthBook, path, 7);
        note_replay_held_table_not_ensured("depth", GatedTable::MarketDepthBook, path, 7);
        assert_ne!(
            HOLD_LOGGED.load(Ordering::Acquire) & GatedTable::MarketDepthBook.bit(),
            0
        );
        // And it is recorded for the drain hold.
        assert!(held_and_unensured(path, 7, EnsuredTables::NONE));
    }

    /// A held file stops counting as night work only while it is held at its
    /// current offset for a table still not ensured.
    #[test]
    fn test_held_and_unensured_and_file_waits_only_on_unensured_table() {
        let path = Path::new("/x/held-only.dspl");
        let never = Path::new("/x/never-held.dspl");
        record_held_file(path, 100, GatedTable::MarketDepthBook);
        assert!(held_and_unensured(path, 100, EnsuredTables::NONE));
        assert!(
            !held_and_unensured(path, 101, EnsuredTables::NONE),
            "the offset moved: work"
        );
        assert!(
            !held_and_unensured(
                path,
                100,
                EnsuredTables::NONE.with(GatedTable::MarketDepthBook)
            ),
            "the table is ensured now: the next round posts it, so it is work"
        );
        assert!(!held_and_unensured(never, 0, EnsuredTables::NONE));
        // The process-global form reads the same record (the book bit may be
        // set by another test, so only the never-held path is asserted).
        assert!(!file_waits_only_on_unensured_table(never, 0));
    }

    /// The body of the function whose signature starts with `signature`:
    /// from its opening brace to the matching closing one.
    fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source.find(signature).expect(signature);
        let open = start + source[start..].find('{').expect("a body");
        &source[open..=matching_brace(source, open)]
    }

    /// The index of the `}` matching the `{` at `open`. The bodies scanned
    /// here hold no brace inside a string or comment on the paths checked.
    fn matching_brace(source: &str, open: usize) -> usize {
        let mut depth = 0_usize;
        for (i, b) in source.bytes().enumerate().skip(open) {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces from {open}");
    }

    fn compact(text: &str) -> String {
        text.split_whitespace().collect()
    }

    /// Every gated table is marked by the ensure that creates it, and ONLY on
    /// its success path. A table nobody marks would hold its spill forever; a
    /// mark outside the success block would let the replay post into a table
    /// the ensure did not create (the auto-create this gate exists to stop).
    ///
    /// Source scan, per ensure: the verdict is the real expression (the
    /// all-statements-accepted flag that every failure arm clears, or the
    /// `run_depth_ddl` result); its success block is brace-matched, and EVERY
    /// `mark_table_ensured` in the function must lie inside it and name the
    /// expected tables.
    ///
    /// Bite: move any mark out of its success block, or delete one, and this
    /// fails.
    #[test]
    fn test_every_gated_table_is_marked_by_its_ensure_after_the_verdict() {
        // (source, function signature, the verdict's success-block opener,
        //  facts the verdict must rest on, tables the block must mark)
        let cases: [(&str, &str, &str, &[&str], &[&str]); 4] = [
            (
                include_str!("tick_persistence.rs"),
                "fn ensure_ticks_table(",
                // B1 review 3: the statements run in `ensure_ticks_tables_verdict`
                // (its facts are checked below); the marks stay here.
                "if verdict.every_statement_accepted {",
                &["let verdict = ensure_ticks_tables_verdict("],
                &["GatedTable::Ticks)", "GatedTable::TickQuotes)"],
            ),
            (
                include_str!("feed_aux_persistence.rs"),
                "fn ensure_feed_aux_table(",
                "if every_statement_accepted {",
                &[
                    "let mut every_statement_accepted = true;",
                    "every_statement_accepted = false;",
                ],
                &["GatedTable::FeedAuxPackets,"],
            ),
            (
                include_str!("depth_persistence.rs"),
                "fn ensure_market_depth_table(",
                "if ensured {",
                &["let ensured = run_depth_ddl("],
                &["GatedTable::MarketDepth)"],
            ),
            (
                include_str!("depth_persistence.rs"),
                "async fn ensure_market_depth_book_table_at(",
                "if run_depth_ddl(",
                &["BookEnsureOutcome::Failed"],
                &["GatedTable::MarketDepthBook,"],
            ),
        ];
        let ticks_verdict = function_body(
            include_str!("tick_persistence.rs"),
            "async fn ensure_ticks_tables_verdict(",
        );
        for fact in [
            "let mut every_statement_accepted = true;",
            "every_statement_accepted = false;",
            "verdict.every_statement_accepted = every_statement_accepted;",
        ] {
            assert!(
                ticks_verdict.contains(fact),
                "ensure_ticks_tables_verdict: no `{fact}`"
            );
        }
        assert!(
            !ticks_verdict.contains("mark_table_"),
            "the verdict function marks nothing; only its caller does"
        );
        for (source, signature, opener, facts, tables) in cases {
            let body = function_body(source, signature);
            for fact in facts {
                assert!(body.contains(fact), "{signature}: no `{fact}`");
            }
            let anchor = body
                .find(opener)
                .unwrap_or_else(|| panic!("{signature}: no `{opener}`"));
            let open = anchor + body[anchor..].find('{').expect("a success block");
            let close = matching_brace(body, open);
            let block = &body[open..=close];
            let marks: Vec<usize> = body
                .match_indices("mark_table_ensured")
                .map(|(i, _)| i)
                .collect();
            assert!(!marks.is_empty(), "{signature} marks nothing");
            for at in marks {
                assert!(
                    open < at && at < close,
                    "{signature}: a mark at byte {at} lies outside the success block \
                     {open}..{close}"
                );
            }
            for table in tables {
                assert!(
                    compact(block).contains(&compact(&format!(
                        "mark_table_ensured(crate::ilp_table_gate::{table}"
                    ))),
                    "{signature}: the success block must mark {table}"
                );
            }
        }
    }
}
