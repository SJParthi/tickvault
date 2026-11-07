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
                "if every_statement_accepted {",
                &[
                    "let mut every_statement_accepted = true;",
                    "every_statement_accepted = false;",
                ],
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
