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

/// Counts and (once per table per process) logs a spill file held because
/// `table` is not yet ensured. Cold path, once per held file per round.
pub fn note_replay_held_table_not_ensured(
    dir_label: &'static str,
    table: GatedTable,
    path: &std::path::Path,
) {
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
        note_replay_held_table_not_ensured(
            "depth",
            GatedTable::MarketDepthBook,
            std::path::Path::new("/x/a.dspl"),
        );
        note_replay_held_table_not_ensured(
            "depth",
            GatedTable::MarketDepthBook,
            std::path::Path::new("/x/a.dspl"),
        );
        assert_ne!(
            HOLD_LOGGED.load(Ordering::Acquire) & GatedTable::MarketDepthBook.bit(),
            0
        );
    }

    /// Every gated table is marked by the ensure that creates it, and only on
    /// success. A table nobody marks would hold its spill forever; a mark
    /// placed before the verdict would let the replay post into a table the
    /// ensure did not create. Source scan: each ensure function's body,
    /// up to the next `pub async fn`, must call `mark_table_ensured` for its
    /// table, after the line that decides success.
    ///
    /// Bite: delete any of the four mark calls and this fails.
    #[test]
    fn test_every_gated_table_is_marked_by_its_ensure_after_the_verdict() {
        let cases: [(&str, &str, &str, &[&str]); 4] = [
            (
                include_str!("tick_persistence.rs"),
                "fn ensure_ticks_table(",
                "if every_statement_accepted {",
                &["GatedTable::Ticks)", "GatedTable::TickQuotes)"],
            ),
            (
                include_str!("feed_aux_persistence.rs"),
                "fn ensure_feed_aux_table(",
                "if every_statement_accepted {",
                &["GatedTable::FeedAuxPackets"],
            ),
            (
                include_str!("depth_persistence.rs"),
                "fn ensure_market_depth_table(",
                "if ensured {",
                &["GatedTable::MarketDepth)"],
            ),
            (
                include_str!("depth_persistence.rs"),
                "async fn ensure_market_depth_book_table_at(",
                "{\n        // 2026-11-07 (`ilp_table_gate`)",
                &["GatedTable::MarketDepthBook"],
            ),
        ];
        for (source, signature, verdict, marks) in cases {
            let start = source.find(signature).expect(signature);
            let rest = &source[start + signature.len()..];
            let end = rest
                .find("\nfn ")
                .into_iter()
                .chain(rest.find("\nasync fn "))
                .chain(rest.find("\npub "))
                .min()
                .unwrap_or(rest.len());
            let body = &rest[..end];
            let verdict_at = body
                .find(verdict)
                .unwrap_or_else(|| panic!("{signature}: no `{verdict}`"));
            for mark in marks {
                let needle = format!("mark_table_ensured(crate::ilp_table_gate::{mark}");
                let compact: String = body.split_whitespace().collect::<Vec<_>>().join("");
                let compact_needle: String = needle.split_whitespace().collect::<Vec<_>>().join("");
                assert!(
                    compact.contains(&compact_needle),
                    "{signature} must mark {mark}"
                );
                let at = body.find("mark_table_ensured").expect("a mark");
                assert!(
                    at > verdict_at,
                    "{signature}: the mark must follow the success check"
                );
            }
        }
    }
}
