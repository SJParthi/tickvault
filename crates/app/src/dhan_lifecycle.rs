//! `instrument_lifecycle` rows for the Dhan master — the never-delete record
//! of what exists, what expired, and when.
//!
//! Operator directive, 2026-08-19: *"see emanhwiel don rmvoe or dleetd any
//! expired data or any expired isntuments dud eokay just amkr it as expired as
//! y or n and then absed on it we wil lclelary knwow hwtehe ti si ready for
//! trading in live or not so that we hsodu lneevr ever dleete ro remove
//! naythign"* — never delete an instrument, mark it expired, and let the flag
//! decide whether it is tradeable today.
//!
//! # The gap this closes
//!
//! The table, its DDL, its audit twin and its SEBI never-delete contract all
//! existed. What did not exist was anyone writing a `feed='dhan'` row: the
//! Dhan instrument chain was deleted on 2026-07-13 and the feed came back on
//! 2026-08-09 without it. So an expired Dhan contract was simply *not
//! selected* — correct behaviour, and no record at all of what expired or when.
//!
//! # Why this writes rows instead of running UPDATEs
//!
//! `instrument_lifecycle` dedups on `(ts, security_id, exchange_segment, feed)`
//! with the designated `ts` pinned to a constant (I-P1-08). Writing a row for
//! an instrument therefore UPSERTS it in place — the table holds one row per
//! instrument ever seen, not one per instrument per day.
//!
//! That matters at this scale. The alternative — one `UPDATE … WHERE
//! security_id = …` per instrument — is ~150,000 HTTP round trips against
//! QuestDB every morning. One ILP batch is the same information in a few
//! seconds, and it is the mechanism the table was designed around.
//!
//! # Complexity
//!
//! O(rows) to classify, one hash lookup per previously-known instrument to
//! find the drop-outs. Cold path: once per trading day, on the daily rider,
//! never on the tick path.

use std::collections::{HashMap, HashSet};

use tickvault_core::instrument::master_csv::{InstrumentClass, MasterRow, OptionLeg};
use tickvault_storage::instrument_lifecycle_persistence::{
    InstrumentLifecycleAuditRow, InstrumentLifecycleRow, LIFECYCLE_FEED_DHAN, LifecycleState,
    TransitionKind, append_instrument_lifecycle_audit_rows, append_instrument_lifecycle_rows,
    ensure_instrument_lifecycle_audit_table, ensure_instrument_lifecycle_table,
};

/// Seconds a QuestDB `/exec` read of the known-instrument set may take.
///
/// Longer than the 10s the other cold-path readers use: this one returns a
/// row per instrument the table has EVER held — six figures at the 25,000
/// target once a few months of expiries have accumulated — where the others
/// return a day of one table. A timeout here costs the absence pass only.
const KNOWN_INSTRUMENTS_QUERY_TIMEOUT_SECS: u64 = 30;

/// Rows per ILP batch.
///
/// The master is ~150,000 rows at roughly 250 bytes of line protocol each —
/// ~37 MB if built as one buffer. Chunking keeps peak memory bounded and
/// flat regardless of how much the master grows, which is the property that
/// matters on a fixed-size box.
const LIFECYCLE_WRITE_CHUNK: usize = 10_000;

/// What one lifecycle write did, and to what.
///
/// Counted per class rather than as a total, because "we wrote 150,000 rows"
/// answers nothing an operator asks. "4,300 contracts expired today" does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LifecycleTally {
    /// Rows written as tradeable today.
    pub active: usize,
    /// Rows written as expired because their expiry date has passed.
    pub expired_by_date: usize,
    /// Rows written as expired because they vanished from today's master
    /// while the table still had them active.
    pub expired_by_absence: usize,
    /// Master rows skipped for having no segment we track.
    pub skipped_unknown_segment: usize,
    /// Master rows skipped for carrying no usable identity.
    pub skipped_no_identity: usize,
}

impl LifecycleTally {
    /// Total rows this write will send.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.active + self.expired_by_date + self.expired_by_absence
    }
}

/// The lifecycle state a master row should carry today.
///
/// # The rule, in one line
///
/// A derivative is expired once its expiry date is behind us; a spot or index
/// present in today's master is active.
///
/// # Why an index and a stock are never `ExpiredContract`
///
/// The three expired states are not interchangeable. `ExpiredContract` means
/// a dated contract reached its date — a thing that only happens to futures
/// and options. A stock leaving the F&O list is `ExpiredFromFno` and an index
/// that stops being published is `ExpiredIndex`, and both of those are
/// detected by ABSENCE from the master rather than by a date, because neither
/// carries one.
#[must_use]
pub fn lifecycle_state_for(row: &MasterRow, today_ymd: u32) -> LifecycleState {
    if (row.class.is_future() || row.class.is_option())
        && row.expiry_ymd != 0
        && row.expiry_ymd < today_ymd
    {
        return LifecycleState::ExpiredContract;
    }
    LifecycleState::Active
}

/// The state a previously-known instrument takes when it leaves the master.
///
/// Absence is the only signal available here — a spot has no expiry date to
/// check — so the class decides which flavour of expired it becomes. An
/// unclassifiable row becomes `ExpiredFromFno`, which is the broadest of the
/// three and the least likely to assert something untrue.
#[must_use]
pub fn absence_state_for(class: InstrumentClass) -> LifecycleState {
    match class {
        InstrumentClass::Index => LifecycleState::ExpiredIndex,
        InstrumentClass::IndexFuture
        | InstrumentClass::StockFuture
        | InstrumentClass::IndexOption
        | InstrumentClass::StockOption => LifecycleState::ExpiredContract,
        _ => LifecycleState::ExpiredFromFno,
    }
}

/// The `exchange_segment` label for a master row.
///
/// Returns `None` for anything outside the segments this product tracks —
/// currency and commodity — so they are skipped and COUNTED rather than
/// written under a guessed segment. A wrong segment is a wrong instrument
/// identity (I-P1-11), and a wrong identity in a never-delete table is
/// permanent.
#[must_use]
pub fn lifecycle_segment_for(row: &MasterRow) -> Option<&'static str> {
    match (row.exch_id.as_str(), row.class) {
        (_, InstrumentClass::Index) => Some("IDX_I"),
        ("NSE", InstrumentClass::Equity) => Some("NSE_EQ"),
        ("BSE", InstrumentClass::Equity) => Some("BSE_EQ"),
        ("NSE", c) if c.is_future() || c.is_option() => Some("NSE_FNO"),
        ("BSE", c) if c.is_future() || c.is_option() => Some("BSE_FNO"),
        _ => None,
    }
}

/// The master's own word for an instrument class, for the `instrument_type`
/// column. Empty when the column was absent from the header.
#[must_use]
pub const fn instrument_type_label(class: InstrumentClass) -> &'static str {
    match class {
        InstrumentClass::Index => "INDEX",
        InstrumentClass::Equity => "EQUITY",
        InstrumentClass::IndexFuture => "FUTIDX",
        InstrumentClass::StockFuture => "FUTSTK",
        InstrumentClass::IndexOption => "OPTIDX",
        InstrumentClass::StockOption => "OPTSTK",
        InstrumentClass::Other => "",
    }
}

/// `CE` / `PE` / empty, for the `option_type` column.
#[must_use]
pub const fn option_type_label(leg: OptionLeg) -> &'static str {
    match leg {
        OptionLeg::Call => "CE",
        OptionLeg::Put => "PE",
        OptionLeg::None => "",
    }
}

/// Converts a `YYYYMMDD` expiry into IST-midnight nanos, or 0 when absent.
///
/// Days-since-epoch by the civil-from-days algorithm — no date dependency, and
/// the whole module stays pure enough to test against a table of fixtures.
#[must_use]
pub fn expiry_ymd_to_ist_nanos(ymd: u32) -> i64 {
    if ymd == 0 {
        return 0;
    }
    let y = i64::from(ymd / 10_000);
    let m = i64::from((ymd / 100) % 100);
    let d = i64::from(ymd % 100);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return 0;
    }
    // Howard Hinnant's days_from_civil. March-based years make the leap day
    // the last day of the year, which is what removes every special case.
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days.saturating_mul(86_400).saturating_mul(1_000_000_000)
}

/// An instrument the table already knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownInstrument {
    /// Its `security_id`.
    pub security_id: i64,
    /// Its `exchange_segment` label.
    pub segment: &'static str,
    /// Whether the table currently has it active.
    pub active: bool,
    /// Its class, for choosing which expired state absence implies.
    pub class: InstrumentClass,
    /// The FULL prior `lifecycle_state`, not just whether it was active.
    ///
    /// Added 2026-08-25 with the audit-trail wiring. `active` answers the one
    /// question the absence pass needs; the audit row needs the actual
    /// `from_state`, and reconstructing it from a boolean would turn every
    /// expiry kind into the same row.
    pub state: LifecycleState,
}

/// Builds the day's lifecycle rows from the master plus what the table knows.
///
/// `known` is the table's current `feed='dhan'` contents. Instruments in
/// `known` that today's master no longer carries, and that the table still has
/// active, get an expired row — that is the "never delete, mark expired" half
/// of the contract, and it is the only way a spot or index can ever be marked
/// expired, since neither carries a date.
///
/// Rows borrow from `master`, so the caller keeps it alive across the write.
#[must_use]
pub fn build_lifecycle_rows<'a>(
    master: &'a [MasterRow],
    known: &[KnownInstrument],
    today_ymd: u32,
    today_ist_nanos: i64,
    source_sha256: &'a str,
) -> (Vec<InstrumentLifecycleRow<'a>>, LifecycleTally) {
    let mut tally = LifecycleTally::default();
    let mut rows = Vec::with_capacity(master.len());
    // O(1) membership for the absence pass. Keyed on the I-P1-11 composite,
    // never the id alone — two instruments can share a numeric id across
    // segments, and marking the wrong one expired in a never-delete table is
    // a permanent, silent error.
    let mut seen: HashMap<(i64, &'static str), ()> = HashMap::with_capacity(master.len());

    for row in master {
        let Ok(security_id) = i64::try_from(row.security_id) else {
            tally.skipped_no_identity += 1;
            continue;
        };
        let Some(segment) = lifecycle_segment_for(row) else {
            tally.skipped_unknown_segment += 1;
            continue;
        };
        seen.insert((security_id, segment), ());

        let state = lifecycle_state_for(row, today_ymd);
        match state {
            LifecycleState::Active => tally.active += 1,
            _ => tally.expired_by_date += 1,
        }
        let expired_nanos = if state.is_expired() {
            today_ist_nanos
        } else {
            0
        };
        let last_active = if state.is_active() {
            today_ist_nanos
        } else {
            0
        };

        rows.push(InstrumentLifecycleRow {
            last_update_ts_nanos: today_ist_nanos,
            security_id,
            exchange_segment: segment,
            exchange_id: row.exch_id.as_str(),
            instrument_type: instrument_type_label(row.class),
            symbol_name: row.symbol_name.as_str(),
            display_name: row.symbol_name.as_str(),
            underlying_security_id: 0,
            underlying_symbol: row.underlying_symbol.as_str(),
            // From the master, not a placeholder. This column was hardcoded 0
            // until 2026-09-07, which had a consequence nobody had noticed:
            // `lifecycle_reconciler::is_stock_split` requires
            // `old_lot_size > 0 && new_lot_size > 0`, so the lot half of
            // corporate-action detection could never fire on a Dhan row.
            lot_size: i32::try_from(row.lot_size).unwrap_or(i32::MAX),
            // Still 0: the master carries TICK_SIZE but this parser does not
            // resolve that column yet, and inventing a value here would make
            // the OTHER half of split detection lie rather than abstain.
            tick_size: 0.0,
            expiry_date_nanos: expiry_ymd_to_ist_nanos(row.expiry_ymd),
            strike_price: strike_paise_to_rupees(row.strike_paise),
            option_type: option_type_label(row.option_leg),
            lifecycle_state: state,
            lifecycle_state_locked: false,
            first_seen_date_nanos: today_ist_nanos,
            last_seen_date_nanos: today_ist_nanos,
            last_active_date_nanos: last_active,
            expired_date_nanos: expired_nanos,
            prev_symbol_chain: "",
            source_csv_sha256: source_sha256,
            dry_run: false,
            feed: LIFECYCLE_FEED_DHAN,
        });
    }

    // The absence pass: still active in the table, gone from today's master.
    //
    // `last_seen_date` is deliberately NOT advanced here — the instrument was
    // not seen today, and writing today's date would erase the one fact that
    // says when it was last real.
    for k in known {
        if !k.active || seen.contains_key(&(k.security_id, k.segment)) {
            continue;
        }
        tally.expired_by_absence += 1;
        rows.push(InstrumentLifecycleRow {
            last_update_ts_nanos: today_ist_nanos,
            security_id: k.security_id,
            exchange_segment: k.segment,
            exchange_id: "",
            instrument_type: instrument_type_label(k.class),
            symbol_name: "",
            display_name: "",
            underlying_security_id: 0,
            underlying_symbol: "",
            // Correctly 0 here: this is the ABSENCE pass, which marks a row
            // the master no longer lists. There is no master row to read a lot
            // size from, and 0 is this field's documented absent sentinel.
            lot_size: 0,
            tick_size: 0.0,
            expiry_date_nanos: 0,
            strike_price: 0.0,
            option_type: "",
            lifecycle_state: absence_state_for(k.class),
            lifecycle_state_locked: false,
            first_seen_date_nanos: 0,
            last_seen_date_nanos: 0,
            last_active_date_nanos: 0,
            expired_date_nanos: today_ist_nanos,
            prev_symbol_chain: "",
            source_csv_sha256: source_sha256,
            dry_run: false,
            feed: LIFECYCLE_FEED_DHAN,
        });
    }

    (rows, tally)
}

/// Integer paise back to rupees for the `strike_price` DOUBLE column.
///
/// The conversion is exact for every real strike: paise are whole hundredths,
/// and `f64` represents those exactly far beyond any price NSE lists.
#[must_use]
#[allow(clippy::cast_precision_loss)]
// APPROVED: strikes are bounded by real option prices, orders of magnitude
// below 2^53 where f64 stops representing integers exactly.
pub fn strike_paise_to_rupees(paise: i64) -> f64 {
    paise as f64 / 100.0
}

/// The `/exec` query returning what the table currently knows for Dhan.
///
/// `LATEST ON ts PARTITION BY` is deliberately absent: the designated `ts` is
/// pinned to a constant, so there is exactly one row per instrument already
/// and a latest-on would be a sort over the whole table for nothing.
#[must_use]
pub fn build_known_instruments_query() -> String {
    format!(
        "SELECT security_id, exchange_segment, lifecycle_state, instrument_type \
         FROM {} WHERE feed = '{}' AND dry_run = false;",
        tickvault_storage::instrument_lifecycle_persistence::QUESTDB_TABLE_INSTRUMENT_LIFECYCLE,
        LIFECYCLE_FEED_DHAN,
    )
}

/// Parses the known-instruments response.
///
/// # Errors
///
/// Malformed JSON or a missing `dataset` array. An unparseable row is skipped
/// rather than failing the whole read: one bad row must not cost the absence
/// pass its knowledge of the other hundred thousand.
pub fn parse_known_instruments(body: &str) -> Result<Vec<KnownInstrument>, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err("known-instruments response is not valid JSON".to_owned());
    };
    let Some(rows) = v.get("dataset").and_then(|d| d.as_array()) else {
        return Err("known-instruments response has no `dataset` array".to_owned());
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(cols) = row.as_array() else { continue };
        if cols.len() < 4 {
            continue;
        }
        let id = match &cols[0] {
            serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
            other => other.as_i64(),
        };
        let (Some(security_id), Some(seg), Some(state)) = (id, cols[1].as_str(), cols[2].as_str())
        else {
            continue;
        };
        // The segment must be one we recognise, and it must be `'static` so
        // the row can borrow it — an unknown label is skipped rather than
        // leaked into a permanent table.
        let Some(segment) = static_segment_label(seg.trim()) else {
            continue;
        };
        let Some(state) = LifecycleState::from_wire(state.trim()) else {
            continue;
        };
        out.push(KnownInstrument {
            security_id,
            segment,
            active: state.is_active(),
            state,
            class: InstrumentClass::from_master_field(cols[3].as_str().unwrap_or("").trim()),
        });
    }
    Ok(out)
}

/// Interns a segment label read back from QuestDB.
fn static_segment_label(s: &str) -> Option<&'static str> {
    match s {
        "IDX_I" => Some("IDX_I"),
        "NSE_EQ" => Some("NSE_EQ"),
        "BSE_EQ" => Some("BSE_EQ"),
        "NSE_FNO" => Some("NSE_FNO"),
        "BSE_FNO" => Some("BSE_FNO"),
        _ => None,
    }
}

/// Classifies one instrument's state change into an audit transition. Pure.
///
/// Returns `None` when nothing changed — an unchanged row must not produce an
/// audit entry, or the 5-year chain becomes a daily snapshot of the whole
/// master (~150,000 rows a day) and the transitions it exists to record are
/// buried in it.
///
/// # Why this does not call `lifecycle_reconciler::classify_transition`
///
/// That classifier answers "is this instrument present in today's CSV?", which
/// is the right question for a master where expiry is signalled by a row
/// DISAPPEARING. The Dhan master is not that shape: an expired contract stays
/// in the file with a past `SM_EXPIRY_DATE`, so `lifecycle_state_for` flips it
/// to `expired_contract` while it is still present — and the reconciler
/// returns `None` for exactly that case (present + previously active + no
/// field change). Routing this through it would have silently dropped the
/// single most common transition in the product.
///
/// O(1), no allocation.
#[must_use]
pub const fn classify_lifecycle_transition(
    prev: Option<LifecycleState>,
    new: LifecycleState,
) -> Option<TransitionKind> {
    let Some(prev) = prev else {
        // Never seen before under this (security_id, segment).
        return Some(TransitionKind::Appeared);
    };
    if matches!(prev, LifecycleState::Delisted) {
        // Operator-set terminal state. Never auto-flipped, so never audited
        // as an automatic transition (§5 lock semantics).
        return None;
    }
    if prev as u8 == new as u8 {
        return None;
    }
    if new.is_active() {
        Some(TransitionKind::Reactivated)
    } else if prev.is_active() {
        Some(TransitionKind::Expired)
    } else {
        // One expired kind reclassified into another (e.g. a stock that left
        // the F&O list and later has its contracts expire). A real change,
        // and not an expiry event.
        Some(TransitionKind::Updated)
    }
}

/// Builds the day's `instrument_lifecycle_audit` rows from the lifecycle rows
/// that are about to be written plus what the table held before. Pure.
///
/// # The gap this closes
///
/// `instrument_lifecycle_audit` is the SEBI 5-year point-in-time chain — the
/// table that answers "what did this instrument look like on this date?". The
/// table DDL existed, the retention policy existed, `classify_transition`
/// existed and was tested, and `append_instrument_lifecycle_audit_row` existed.
/// **Nothing called any of it.** The question was unanswerable, and nothing
/// said so, because a table that is never written looks exactly like a table
/// with nothing to report.
///
/// Rows borrow from `rows`, which borrow from the master — no copies.
/// O(rows) with one hash lookup per row.
#[must_use]
pub fn build_lifecycle_audit_rows<'a>(
    rows: &'a [InstrumentLifecycleRow<'a>],
    known: &[KnownInstrument],
    today_ist_nanos: i64,
    source_sha256: &'a str,
) -> Vec<InstrumentLifecycleAuditRow<'a>> {
    let prev: HashMap<(i64, &'static str), LifecycleState> = known
        .iter()
        .map(|k| ((k.security_id, k.segment), k.state))
        .collect();

    let mut out = Vec::new();
    for row in rows {
        // `exchange_segment` on the row is the same `&'static str` the map is
        // keyed on — interned by `lifecycle_segment_for` / `static_segment_label`.
        let key = (row.security_id, row.exchange_segment);
        let before = prev.get(&key).copied();
        let Some(kind) = classify_lifecycle_transition(before, row.lifecycle_state) else {
            continue;
        };
        out.push(InstrumentLifecycleAuditRow {
            // The run stamp, not `now()`. The DEDUP key carries `ts`, so a
            // re-run of the same day upserts onto the same rows instead of
            // appending a second, contradictory history.
            ts_nanos_ist: today_ist_nanos,
            trading_date_ist_nanos: today_ist_nanos,
            security_id: row.security_id,
            exchange_segment: row.exchange_segment,
            from_state: before,
            to_state: row.lifecycle_state,
            transition_kind: kind,
            // Deliberately empty. The Dhan lifecycle writer reads four columns
            // back from the table (id, segment, state, type) and does not know
            // yesterday's lot size, tick size or symbol — so it cannot honestly
            // report a field delta, and inventing one would be worse than
            // omitting it. `from_state` / `to_state` carry the change that IS
            // known.
            field_deltas: "",
            source_csv_sha256: source_sha256,
            operator_note: "",
            lifecycle_state_after: row.lifecycle_state,
            lot_size_after: row.lot_size,
            tick_size_after: row.tick_size,
            expiry_date_after_nanos: row.expiry_date_nanos,
            symbol_name_after: row.symbol_name,
            dry_run: row.dry_run,
            feed: row.feed,
        });
    }
    out
}

/// A write pass stops sending after this many chunks in a row failed, and
/// counts the rest as not written.
///
/// One refused chunk (a bad row) is followed by chunks that land, so the
/// count resets and every chunk is still tried. A database that is down
/// fails every chunk, and with the client's retry budget each attempt costs
/// seconds; two in a row is the signal to stop rather than spend that on all
/// ~15 lifecycle chunks.
const LIFECYCLE_STOP_AFTER_CONSECUTIVE_FAILED_CHUNKS: usize = 2;

/// What [`write_audit_then_lifecycle`] wrote and what it did not.
#[derive(Debug, Default)]
struct LifecycleWriteReport {
    /// Audit rows the database acknowledged.
    audit_written: usize,
    /// Audit rows refused, or not sent after consecutive failures.
    audit_failed: usize,
    /// Lifecycle rows the database acknowledged.
    lifecycle_written: usize,
    /// Lifecycle rows not sent because their audit row did not land.
    lifecycle_held_back: usize,
    /// Lifecycle rows refused, or not sent after consecutive failures.
    lifecycle_failed: usize,
    /// The first chunk error, kept for the caller's report.
    first_error: Option<anyhow::Error>,
}

/// Writes the audit rows, then the lifecycle rows, in chunks.
///
/// Audit first, and a lifecycle row whose audit row did not land is held
/// back: the audit rows are derived from the difference between `rows` and
/// what the table held before, so a lifecycle row that lands without its
/// audit row erases the difference that would let a re-run record it.
/// Every chunk is tried (a refused chunk does not stop the rest) until
/// [`LIFECYCLE_STOP_AFTER_CONSECUTIVE_FAILED_CHUNKS`] fail in a row.
///
/// O(rows + audit rows) plus, only when an audit chunk failed, one hash set
/// of the affected keys and one filtered copy of `rows`. Each chunk is handed
/// to the writer as an owned copy (one `Vec` per chunk), which keeps the
/// writer's future free of a borrowed argument so the rider task stays
/// `Send`. Cold path, once a day.
async fn write_audit_then_lifecycle<'a, A, FA, L, FL>(
    rows: &[InstrumentLifecycleRow<'a>],
    audit_rows: &[InstrumentLifecycleAuditRow<'a>],
    chunk_rows: usize,
    mut write_audit: A,
    mut write_lifecycle: L,
) -> LifecycleWriteReport
where
    A: FnMut(Vec<InstrumentLifecycleAuditRow<'a>>) -> FA,
    FA: std::future::Future<Output = anyhow::Result<()>>,
    L: FnMut(Vec<InstrumentLifecycleRow<'a>>) -> FL,
    FL: std::future::Future<Output = anyhow::Result<()>>,
{
    let chunk_rows = chunk_rows.max(1);
    let mut report = LifecycleWriteReport::default();
    // Composite key per I-P1-11; built only when an audit chunk fails.
    let mut held: HashSet<(i64, &'a str)> = HashSet::new();

    let mut consecutive = 0_usize;
    for chunk in audit_rows.chunks(chunk_rows) {
        if consecutive >= LIFECYCLE_STOP_AFTER_CONSECUTIVE_FAILED_CHUNKS {
            report.audit_failed += chunk.len();
            held.extend(chunk.iter().map(|a| (a.security_id, a.exchange_segment)));
            continue;
        }
        match write_audit(chunk.to_vec()).await {
            Ok(()) => {
                report.audit_written += chunk.len();
                consecutive = 0;
            }
            Err(err) => {
                consecutive += 1;
                report.audit_failed += chunk.len();
                held.extend(chunk.iter().map(|a| (a.security_id, a.exchange_segment)));
                metrics::counter!("tv_instrument_lifecycle_audit_failed_total").increment(1);
                // Audit OBS-2 (2026-10-11): a failed SEBI audit write is a storage
                // audit-write failure (STORAGE-GAP-03), not a socket state (WS-GAP-03).
                tracing::error!(
                    code = tickvault_common::error_code::ErrorCode::StorageGap03AuditWriteFailed
                        .code_str(),
                    source = "lifecycle_audit_persist_failed",
                    ?err,
                    rows = chunk.len(),
                    "instrument lifecycle AUDIT rows could not be written — the 5-year \
                     point-in-time chain is missing these transitions for now. Their \
                     lifecycle rows are held back, so the table keeps yesterday's state \
                     for them and the next run records the same transitions again."
                );
                report.first_error.get_or_insert(err);
            }
        }
    }

    let kept: Vec<InstrumentLifecycleRow<'a>>;
    let to_write: &[InstrumentLifecycleRow<'a>] = if held.is_empty() {
        rows
    } else {
        kept = rows
            .iter()
            .filter(|r| !held.contains(&(r.security_id, r.exchange_segment)))
            .cloned()
            .collect();
        report.lifecycle_held_back = rows.len() - kept.len();
        &kept
    };

    consecutive = 0;
    for chunk in to_write.chunks(chunk_rows) {
        if consecutive >= LIFECYCLE_STOP_AFTER_CONSECUTIVE_FAILED_CHUNKS {
            report.lifecycle_failed += chunk.len();
            continue;
        }
        match write_lifecycle(chunk.to_vec()).await {
            Ok(()) => {
                report.lifecycle_written += chunk.len();
                consecutive = 0;
            }
            Err(err) => {
                consecutive += 1;
                report.lifecycle_failed += chunk.len();
                report.first_error.get_or_insert(err);
            }
        }
    }
    report
}

/// Writes the day's Dhan lifecycle rows.
///
/// Never deletes and never truncates: every row is an upsert on the pinned
/// designated timestamp, so an instrument that has existed once stays in the
/// table forever with whatever state it last held.
///
/// Returns the tally so the caller can log what actually changed.
///
/// # Errors
///
/// `Err` when any audit or lifecycle row did not land; the message carries
/// the counts and the first chunk's error.
///
/// **⚠ CORRECTED 2026-10-11 (review of audit QDB-2).** This said "Propagates
/// the first chunk failure ... the next day's run re-sends whatever did not
/// land". Both halves were wrong once HTTP ILP made a refused chunk visible.
/// The `?` stopped at the first refused lifecycle chunk, so the later chunks
/// AND the whole audit pass were skipped. And the audit rows are derived by
/// comparing today's rows with what the table held before the write, so a
/// transition whose lifecycle row landed and whose audit row did not was
/// never re-derived: a re-run read the new state as "known" and found
/// nothing to record. The write is now audit-first (see
/// [`write_audit_then_lifecycle`]): a lifecycle row whose audit row did not
/// land is held back, so the next run still sees yesterday's state and
/// records the same transition again.
// TEST-EXEMPT: async I/O composition (one /exec read + chunked ILP writes); the write ordering is tested through write_audit_then_lifecycle, and the pure parts — build_lifecycle_rows, parse_known_instruments, lifecycle_state_for, absence_state_for, lifecycle_segment_for, expiry_ymd_to_ist_nanos — are each tested below.
pub async fn write_dhan_lifecycle(
    questdb: &tickvault_common::config::QuestDbConfig,
    master: &[MasterRow],
    today_ymd: u32,
    today_ist_nanos: i64,
    source_sha256: &str,
) -> anyhow::Result<LifecycleTally> {
    // The audit failure counter is seeded at boot (main.rs), after the
    // metrics recorder is installed, so a scrape sees 0 before any failure.

    // --- Both SEBI tables must carry their DEDUP key before any row goes in ---
    //
    // The DDL is ensured here rather than at boot because this is the only
    // writer: ILP would otherwise auto-create the table without its DEDUP
    // key. For `instrument_lifecycle` that is the whole design: its
    // designated `ts` is a constant, so the key is the only thing that keeps
    // ONE row per instrument, and an unkeyed table gains a second copy of
    // all ~150,000 instruments on every daily run. For the audit table a
    // re-run would append a second, contradictory history for the same day.
    //
    // **⚠ CHANGED 2026-10-26 (audit J3):** until this date only the AUDIT
    // table was ensured here, and `instrument_lifecycle` had no production
    // ensure at all, so a fresh or restored QuestDB got it auto-created with
    // no key. Both are ensured now, and when either cannot be confirmed keyed
    // (QuestDB refused, still replaying its WAL, or unreachable) the whole
    // write is held back: the table keeps yesterday's state, nothing is
    // deleted, and the next run (the next boot, or tomorrow) writes it.
    // Pinned by `ensure_ddl_boot_wiring_guard::the_lifecycle_write_is_held_back_when_a_sebi_table_is_not_keyed`.
    let lifecycle_keyed = ensure_instrument_lifecycle_table(questdb).await;
    let audit_keyed = ensure_instrument_lifecycle_audit_table(questdb).await;
    if !(lifecycle_keyed && audit_keyed) {
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::StorageGap03AuditWriteFailed.code_str(),
            source = "lifecycle_ddl_not_keyed",
            lifecycle_keyed,
            audit_keyed,
            "instrument lifecycle write held back: QuestDB did not confirm the DEDUP key on \
             the SEBI lifecycle tables, and writing into a table without it would append a \
             duplicate copy of every instrument. Nothing was written or deleted; the tables \
             keep yesterday's state and the next run writes today's."
        );
        return Err(anyhow::anyhow!(
            "instrument lifecycle write held back: DEDUP key not confirmed \
             (instrument_lifecycle keyed = {lifecycle_keyed}, \
             instrument_lifecycle_audit keyed = {audit_keyed})"
        ));
    }

    let known = fetch_known_instruments(questdb).await;
    let (rows, tally) =
        build_lifecycle_rows(master, &known, today_ymd, today_ist_nanos, source_sha256);

    // --- The SEBI 5-year audit chain, written FIRST ---
    //
    // **⚠ CHANGED 2026-10-11 (review of audit QDB-2).** Until this date the
    // audit rows were written AFTER the lifecycle rows "and never allowed to
    // fail the write". That ordering lost history for good: the audit rows
    // are derived from the difference between today's rows and what the
    // table held before, so once a lifecycle row landed, a failed audit row
    // for it could never be derived again. Audit-first keeps the difference
    // alive: a lifecycle row whose audit row did not land is held back, the
    // table keeps yesterday's state for it, and the next run records the
    // transition. Rows with no transition today are never held back, so
    // one refused audit row costs one instrument's state update, not the
    // day's. (Both tables' DDL is ensured at the top of this function.)
    let audit_rows = build_lifecycle_audit_rows(&rows, &known, today_ist_nanos, source_sha256);
    let report = write_audit_then_lifecycle(
        &rows,
        &audit_rows,
        LIFECYCLE_WRITE_CHUNK,
        move |chunk| async move { append_instrument_lifecycle_audit_rows(questdb, &chunk).await },
        move |chunk| async move { append_instrument_lifecycle_rows(questdb, &chunk).await },
    )
    .await;
    metrics::counter!("tv_instrument_lifecycle_audit_rows_total")
        .increment(report.audit_written as u64);

    tracing::info!(
        audit_rows_written = report.audit_written,
        audit_rows_failed = report.audit_failed,
        lifecycle_rows_written = report.lifecycle_written,
        lifecycle_rows_held_back = report.lifecycle_held_back,
        lifecycle_rows_failed = report.lifecycle_failed,
        "instrument lifecycle audit transitions and state recorded"
    );

    if let Some(err) = report.first_error {
        return Err(err.context(format!(
            "instrument lifecycle write incomplete: {} audit row(s) not written, {} \
             lifecycle row(s) held back so the next run records their transitions again, \
             {} lifecycle row(s) not written",
            report.audit_failed, report.lifecycle_held_back, report.lifecycle_failed
        )));
    }

    tracing::info!(
        active = tally.active,
        expired_by_date = tally.expired_by_date,
        expired_by_absence = tally.expired_by_absence,
        skipped_unknown_segment = tally.skipped_unknown_segment,
        skipped_no_identity = tally.skipped_no_identity,
        known_before = known.len(),
        rows_written = tally.rows(),
        "instrument lifecycle written for dhan — nothing deleted, expired rows marked"
    );
    Ok(tally)
}

/// Reads what the table knows. Empty on any failure, said once.
///
/// An empty read degrades the ABSENCE pass only: every master row still gets
/// its correct state, and an instrument that vanished today keeps yesterday's
/// state until the next successful read. That is the safe direction — the
/// alternative would be marking instruments expired because a query failed.
async fn fetch_known_instruments(
    questdb: &tickvault_common::config::QuestDbConfig,
) -> Vec<KnownInstrument> {
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            KNOWN_INSTRUMENTS_QUERY_TIMEOUT_SECS,
        ))
        .build()
    else {
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
            source = "lifecycle_absence_pass",
            "lifecycle: HTTP client build failed — absence pass skipped"
        );
        return Vec::new();
    };
    let sql = build_known_instruments_query();
    let body = match client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp.text().await.ok(),
        Ok(resp) => {
            tracing::error!(code = tickvault_common::error_code::ErrorCode::InstrumentP1SingleRowLifecycle.code_str(), source = "known_query_non_2xx", status = %resp.status(), "lifecycle: known-instruments query non-2xx");
            None
        }
        Err(err) => {
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::InstrumentP1SingleRowLifecycle
                    .code_str(),
                source = "known_query_failed",
                ?err,
                "lifecycle: known-instruments query failed"
            );
            None
        }
    };
    let Some(body) = body else { return Vec::new() };
    match parse_known_instruments(&body) {
        Ok(k) => k,
        Err(err) => {
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::InstrumentP1SingleRowLifecycle
                    .code_str(),
                source = "known_query_unparseable",
                err,
                "lifecycle: known-instruments response unparseable — the absence pass is \
                 SKIPPED this run. Instruments that vanished from today's master keep \
                 yesterday's state rather than being marked expired on a failed read."
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of `src` from the `source = "<source>"` field back to the
    /// `error!` macro that holds it, plus everything up to the arm's end.
    fn arm_around<'a>(src: &'a str, source_field: &str) -> &'a str {
        let at = src
            .find(source_field)
            .unwrap_or_else(|| panic!("{source_field} must exist"));
        let start = src[..at]
            .rfind("Err(")
            .expect("an Err arm holds the source");
        let end = at + src[at..].find("\n        }\n").expect("the arm ends");
        &src[start..end]
    }

    // Audit OBS-2 (2026-10-11): a failed SEBI lifecycle AUDIT write was coded
    // WS-GAP-03 (a socket state), so a STORAGE-GAP-03 triage never found it,
    // and only the success counter existed.
    #[test]
    fn test_lifecycle_audit_failure_is_coded_storage_gap_03() {
        let src = include_str!("dhan_lifecycle.rs");
        let prod = src
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("non-test half");
        let arm = arm_around(prod, "source = \"lifecycle_audit_persist_failed\"");
        assert!(
            arm.contains("ErrorCode::StorageGap03AuditWriteFailed"),
            "{arm}"
        );
        assert!(!arm.contains("WsGapConnectionState"), "{arm}");
        assert!(
            arm.contains("counter!(\"tv_instrument_lifecycle_audit_failed_total\").increment(1)"),
            "{arm}"
        );
        // Seeded at boot after the recorder install, not milliseconds before
        // the write that may fail (review of OBS-2, 2026-10-11).
        assert!(
            include_str!("main.rs")
                .contains("counter!(\"tv_instrument_lifecycle_audit_failed_total\").increment(0)"),
            "the failure counter is seeded at 0 at boot"
        );
        assert!(
            !prod.contains("counter!(\"tv_instrument_lifecycle_audit_failed_total\").increment(0)"),
            "no seed right before the write: a scrape could never record that 0"
        );
    }

    // ---- Review of audit QDB-2 (2026-10-11): audit-first, every chunk tried ----

    /// Three never-seen instruments: three lifecycle rows and three
    /// `appeared` audit rows.
    fn three_master_rows() -> Vec<MasterRow> {
        vec![
            row(1, InstrumentClass::Equity, "NSE", 0, OptionLeg::None),
            row(2, InstrumentClass::Equity, "NSE", 0, OptionLeg::None),
            row(3, InstrumentClass::Equity, "NSE", 0, OptionLeg::None),
        ]
    }

    #[tokio::test]
    async fn test_write_audit_then_lifecycle_writes_audit_before_any_lifecycle_row() {
        let master = three_master_rows();
        let (rows, _) = build_lifecycle_rows(&master, &[], TODAY, TODAY_NANOS, "sha");
        let audit = build_lifecycle_audit_rows(&rows, &[], TODAY_NANOS, "sha");
        assert_eq!(audit.len(), 3);
        let order = std::cell::RefCell::new(Vec::new());
        let report = write_audit_then_lifecycle(
            &rows,
            &audit,
            1,
            |_c: Vec<_>| {
                order.borrow_mut().push("audit");
                std::future::ready(Ok(()))
            },
            |_c: Vec<_>| {
                order.borrow_mut().push("lifecycle");
                std::future::ready(Ok(()))
            },
        )
        .await;
        assert_eq!(
            order.into_inner(),
            vec![
                "audit",
                "audit",
                "audit",
                "lifecycle",
                "lifecycle",
                "lifecycle"
            ]
        );
        assert_eq!(report.audit_written, 3);
        assert_eq!(report.lifecycle_written, 3);
        assert_eq!(report.lifecycle_held_back, 0);
        assert!(report.first_error.is_none());
    }

    #[tokio::test]
    async fn test_write_audit_then_lifecycle_sends_later_chunks_after_a_refused_one() {
        let master = three_master_rows();
        let (rows, _) = build_lifecycle_rows(&master, &[], TODAY, TODAY_NANOS, "sha");
        let audit = build_lifecycle_audit_rows(&rows, &[], TODAY_NANOS, "sha");
        let sent = std::cell::RefCell::new(Vec::new());
        let report = write_audit_then_lifecycle(
            &rows,
            &audit,
            1,
            |_c: Vec<_>| std::future::ready(Ok(())),
            |c: Vec<InstrumentLifecycleRow<'_>>| {
                let id = c[0].security_id;
                sent.borrow_mut().push(id);
                std::future::ready(if id == 1 {
                    Err(anyhow::anyhow!("refused"))
                } else {
                    Ok(())
                })
            },
        )
        .await;
        assert_eq!(
            sent.into_inner(),
            vec![1, 2, 3],
            "a refused chunk must not stop the chunks after it"
        );
        assert_eq!(report.lifecycle_failed, 1);
        assert_eq!(report.lifecycle_written, 2);
        assert_eq!(report.audit_written, 3);
        assert!(report.first_error.is_some());
    }

    #[tokio::test]
    async fn test_write_audit_then_lifecycle_holds_back_a_row_whose_audit_failed() {
        let master = three_master_rows();
        let (rows, _) = build_lifecycle_rows(&master, &[], TODAY, TODAY_NANOS, "sha");
        let audit = build_lifecycle_audit_rows(&rows, &[], TODAY_NANOS, "sha");
        let written = std::cell::RefCell::new(Vec::new());
        let report = write_audit_then_lifecycle(
            &rows,
            &audit,
            1,
            |c: Vec<InstrumentLifecycleAuditRow<'_>>| {
                std::future::ready(if c[0].security_id == 2 {
                    Err(anyhow::anyhow!("audit refused"))
                } else {
                    Ok(())
                })
            },
            |c: Vec<InstrumentLifecycleRow<'_>>| {
                written.borrow_mut().extend(c.iter().map(|r| r.security_id));
                std::future::ready(Ok(()))
            },
        )
        .await;
        assert_eq!(
            written.into_inner(),
            vec![1, 3],
            "the row whose transition was not recorded keeps yesterday's state"
        );
        assert_eq!(report.audit_failed, 1);
        assert_eq!(report.lifecycle_held_back, 1);
        assert_eq!(report.lifecycle_written, 2);
        assert!(report.first_error.is_some());
    }

    #[tokio::test]
    async fn test_write_audit_then_lifecycle_stops_after_consecutive_failures() {
        let master: Vec<MasterRow> = (1..=5)
            .map(|id| row(id, InstrumentClass::Equity, "NSE", 0, OptionLeg::None))
            .collect();
        let (rows, _) = build_lifecycle_rows(&master, &[], TODAY, TODAY_NANOS, "sha");
        let audit = build_lifecycle_audit_rows(&rows, &[], TODAY_NANOS, "sha");
        let calls = std::cell::Cell::new(0_usize);
        let report = write_audit_then_lifecycle(
            &rows,
            &audit,
            1,
            |_c: Vec<_>| std::future::ready(Ok(())),
            |_c: Vec<_>| {
                calls.set(calls.get() + 1);
                std::future::ready(Err(anyhow::anyhow!("database down")))
            },
        )
        .await;
        assert_eq!(
            calls.get(),
            LIFECYCLE_STOP_AFTER_CONSECUTIVE_FAILED_CHUNKS,
            "a database that fails every chunk is not asked again and again"
        );
        assert_eq!(report.lifecycle_failed, 5, "unsent rows are counted");
        assert_eq!(report.lifecycle_written, 0);
    }

    const TODAY: u32 = 2026_08_19;
    const TODAY_NANOS: i64 = 1_755_561_600_000_000_000;

    fn row(id: u64, class: InstrumentClass, exch: &str, expiry: u32, leg: OptionLeg) -> MasterRow {
        MasterRow {
            security_id: id,
            isin: String::new(),
            symbol_name: format!("SYM{id}"),
            exch_id: exch.into(),
            segment: "D".into(),
            series: String::new(),
            class,
            expiry_ymd: expiry,
            strike_paise: 0,
            option_leg: leg,
            underlying_symbol: "NIFTY".into(),
            lot_size: 75,
        }
    }

    #[test]
    fn lifecycle_state_for_expires_a_contract_the_day_after_its_expiry() {
        let past = row(
            1,
            InstrumentClass::IndexOption,
            "NSE",
            2026_08_18,
            OptionLeg::Call,
        );
        assert_eq!(
            lifecycle_state_for(&past, TODAY),
            LifecycleState::ExpiredContract
        );
        // Expiry day itself is still tradeable — the contract trades through
        // its final session, and marking it expired at 08:30 would take it out
        // of the universe on the one day it matters most.
        let today = row(
            2,
            InstrumentClass::IndexOption,
            "NSE",
            TODAY,
            OptionLeg::Call,
        );
        assert_eq!(lifecycle_state_for(&today, TODAY), LifecycleState::Active);
        let future = row(
            3,
            InstrumentClass::IndexOption,
            "NSE",
            2026_08_26,
            OptionLeg::Call,
        );
        assert_eq!(lifecycle_state_for(&future, TODAY), LifecycleState::Active);
    }

    #[test]
    fn lifecycle_state_for_never_expires_a_spot_or_index_by_date() {
        // Neither carries an expiry, so a date rule cannot apply to them.
        // Their only route to expired is ABSENCE from the master.
        let eq = row(1, InstrumentClass::Equity, "NSE", 0, OptionLeg::None);
        let idx = row(2, InstrumentClass::Index, "NSE", 0, OptionLeg::None);
        assert_eq!(lifecycle_state_for(&eq, TODAY), LifecycleState::Active);
        assert_eq!(lifecycle_state_for(&idx, TODAY), LifecycleState::Active);
    }

    #[test]
    fn absence_state_for_picks_the_right_flavour_of_expired() {
        // The three expired states are not interchangeable: ExpiredContract
        // asserts a dated contract reached its date, which is untrue of a
        // stock and of an index.
        assert_eq!(
            absence_state_for(InstrumentClass::Index),
            LifecycleState::ExpiredIndex
        );
        assert_eq!(
            absence_state_for(InstrumentClass::Equity),
            LifecycleState::ExpiredFromFno
        );
        assert_eq!(
            absence_state_for(InstrumentClass::StockOption),
            LifecycleState::ExpiredContract
        );
        assert_eq!(
            absence_state_for(InstrumentClass::Other),
            LifecycleState::ExpiredFromFno,
            "the broadest state is the least likely to assert something untrue"
        );
    }

    #[test]
    fn test_instrument_type_label_and_option_type_label_match_the_master_vocabulary() {
        // These strings land in a never-delete table, so a drift here is a
        // permanent mislabel on every row written after it.
        assert_eq!(instrument_type_label(InstrumentClass::Index), "INDEX");
        assert_eq!(instrument_type_label(InstrumentClass::Equity), "EQUITY");
        assert_eq!(
            instrument_type_label(InstrumentClass::IndexFuture),
            "FUTIDX"
        );
        assert_eq!(
            instrument_type_label(InstrumentClass::StockFuture),
            "FUTSTK"
        );
        assert_eq!(
            instrument_type_label(InstrumentClass::IndexOption),
            "OPTIDX"
        );
        assert_eq!(
            instrument_type_label(InstrumentClass::StockOption),
            "OPTSTK"
        );
        assert_eq!(
            instrument_type_label(InstrumentClass::Other),
            "",
            "an unclassified row must write an EMPTY type, never a guessed one"
        );
        assert_eq!(option_type_label(OptionLeg::Call), "CE");
        assert_eq!(option_type_label(OptionLeg::Put), "PE");
        assert_eq!(option_type_label(OptionLeg::None), "");
    }

    #[test]
    fn lifecycle_segment_for_refuses_what_it_cannot_identify() {
        assert_eq!(
            lifecycle_segment_for(&row(1, InstrumentClass::Equity, "NSE", 0, OptionLeg::None)),
            Some("NSE_EQ")
        );
        assert_eq!(
            lifecycle_segment_for(&row(1, InstrumentClass::Equity, "BSE", 0, OptionLeg::None)),
            Some("BSE_EQ")
        );
        assert_eq!(
            lifecycle_segment_for(&row(
                1,
                InstrumentClass::StockOption,
                "NSE",
                0,
                OptionLeg::None
            )),
            Some("NSE_FNO")
        );
        assert_eq!(
            lifecycle_segment_for(&row(
                1,
                InstrumentClass::IndexOption,
                "BSE",
                0,
                OptionLeg::None
            )),
            Some("BSE_FNO")
        );
        assert_eq!(
            lifecycle_segment_for(&row(1, InstrumentClass::Index, "NSE", 0, OptionLeg::None)),
            Some("IDX_I")
        );
        // Commodity and currency are outside this product. A guessed segment
        // in a never-delete table is a permanent wrong identity.
        assert_eq!(
            lifecycle_segment_for(&row(
                1,
                InstrumentClass::StockOption,
                "MCX",
                0,
                OptionLeg::None
            )),
            None
        );
    }

    #[test]
    fn build_lifecycle_rows_writes_the_masters_real_lot_size() {
        // Hardcoded `0` until 2026-09-07, and the cost was not cosmetic:
        // `lifecycle_reconciler::is_stock_split` requires
        // `old_lot_size > 0 && new_lot_size > 0`, so the lot half of
        // corporate-action detection could never fire on a Dhan row — it was
        // comparing 0 against 0 on every instrument, every day.
        let master = vec![MasterRow {
            lot_size: 250,
            ..row(
                1,
                InstrumentClass::StockOption,
                "NSE",
                2026_08_28,
                OptionLeg::Call,
            )
        }];
        let (rows, _) = build_lifecycle_rows(&master, &[], TODAY, TODAY_NANOS, "sha");
        assert_eq!(rows[0].lot_size, 250);
    }

    #[test]
    fn build_lifecycle_rows_records_an_absent_lot_size_as_zero_not_a_guess() {
        let master = vec![MasterRow {
            lot_size: 0,
            ..row(
                1,
                InstrumentClass::StockOption,
                "NSE",
                2026_08_28,
                OptionLeg::Call,
            )
        }];
        let (rows, _) = build_lifecycle_rows(&master, &[], TODAY, TODAY_NANOS, "sha");
        // 0 is this column's documented absent sentinel, and it is exactly
        // what `is_stock_split` treats as "cannot judge" — so an unknown lot
        // size makes the classifier ABSTAIN rather than report a false split.
        assert_eq!(rows[0].lot_size, 0);
    }

    #[test]
    fn build_lifecycle_rows_marks_expired_without_dropping_anything() {
        let master = vec![
            row(1, InstrumentClass::Equity, "NSE", 0, OptionLeg::None),
            row(
                2,
                InstrumentClass::IndexOption,
                "NSE",
                2026_08_18,
                OptionLeg::Call,
            ),
            row(
                3,
                InstrumentClass::IndexOption,
                "NSE",
                2026_08_26,
                OptionLeg::Put,
            ),
        ];
        let (rows, tally) = build_lifecycle_rows(&master, &[], TODAY, TODAY_NANOS, "sha");
        assert_eq!(rows.len(), 3, "every row is written — nothing is dropped");
        assert_eq!(tally.active, 2);
        assert_eq!(tally.expired_by_date, 1);
        let expired: Vec<_> = rows
            .iter()
            .filter(|r| r.lifecycle_state.is_expired())
            .collect();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].security_id, 2);
        assert_eq!(
            expired[0].expired_date_nanos, TODAY_NANOS,
            "an expired row records WHEN it expired"
        );
    }

    /// The 5-year regulatory chain had NO writer: the table, the retention
    /// policy and the classifier all existed and nothing called any of them,
    /// so "what did this instrument look like on this date?" was
    /// unanswerable.
    ///
    /// The transition that matters most here is the one a naive reuse of
    /// `lifecycle_reconciler::classify_transition` would have silently
    /// dropped — a contract that is STILL IN the master but past its expiry
    /// date. In the Dhan master that is how expiry actually looks.
    #[test]
    fn the_audit_chain_records_every_state_change_and_nothing_else() {
        // id 1: brand new equity           -> appeared
        // id 2: option expired YESTERDAY, still in the master -> expired
        // id 3: option expiring later, unchanged and active  -> NO ROW
        // id 4: previously expired, back in the master today -> reactivated
        let master = vec![
            row(1, InstrumentClass::Equity, "NSE", 0, OptionLeg::None),
            row(
                2,
                InstrumentClass::IndexOption,
                "NSE",
                2026_08_18,
                OptionLeg::Call,
            ),
            row(
                3,
                InstrumentClass::IndexOption,
                "NSE",
                2026_08_26,
                OptionLeg::Put,
            ),
            row(4, InstrumentClass::Equity, "NSE", 0, OptionLeg::None),
        ];
        let known = vec![
            KnownInstrument {
                security_id: 2,
                segment: "NSE_FNO",
                active: true,
                state: LifecycleState::Active,
                class: InstrumentClass::IndexOption,
            },
            KnownInstrument {
                security_id: 3,
                segment: "NSE_FNO",
                active: true,
                state: LifecycleState::Active,
                class: InstrumentClass::IndexOption,
            },
            KnownInstrument {
                security_id: 4,
                segment: "NSE_EQ",
                active: false,
                state: LifecycleState::ExpiredFromFno,
                class: InstrumentClass::Equity,
            },
        ];

        let (rows, _) = build_lifecycle_rows(&master, &known, TODAY, TODAY_NANOS, "sha");
        let audit = build_lifecycle_audit_rows(&rows, &known, TODAY_NANOS, "sha");

        let kind_of = |id: i64| {
            audit
                .iter()
                .find(|a| a.security_id == id)
                .map(|a| a.transition_kind)
        };
        assert_eq!(
            kind_of(1),
            Some(TransitionKind::Appeared),
            "a never-seen instrument appears"
        );
        assert_eq!(
            kind_of(2),
            Some(TransitionKind::Expired),
            "a contract still PRESENT in the master but past its expiry date is the most \
             common transition in this product, and the CSV-absence classifier returns \
             None for it"
        );
        assert_eq!(
            kind_of(3),
            None,
            "an unchanged active instrument must produce NO audit row — otherwise the \
             5-year chain becomes a daily snapshot of ~150,000 rows and the real \
             transitions are buried in it"
        );
        assert_eq!(
            kind_of(4),
            Some(TransitionKind::Reactivated),
            "an expired instrument back in the master reactivates"
        );

        // The snapshot columns (§25) must describe the state AFTER, and the
        // stamps must be the run stamp so a re-run upserts rather than
        // appending a contradictory second history for the same day.
        let expired = audit.iter().find(|a| a.security_id == 2).expect("row 2");
        assert_eq!(expired.from_state, Some(LifecycleState::Active));
        assert_eq!(expired.to_state, expired.lifecycle_state_after);
        assert!(!expired.to_state.is_active());
        assert_eq!(expired.ts_nanos_ist, TODAY_NANOS);
        assert_eq!(expired.trading_date_ist_nanos, TODAY_NANOS);
        assert_eq!(expired.feed, LIFECYCLE_FEED_DHAN);
    }

    /// A locked/manual terminal state is never auto-flipped, so it must never
    /// be auto-audited either — an automatic row would claim the system made
    /// a change it is forbidden from making.
    #[test]
    fn a_delisted_instrument_is_never_audited_automatically() {
        assert_eq!(
            classify_lifecycle_transition(Some(LifecycleState::Delisted), LifecycleState::Active),
            None
        );
        assert_eq!(
            classify_lifecycle_transition(
                Some(LifecycleState::Delisted),
                LifecycleState::ExpiredContract
            ),
            None
        );
        // And the ordinary no-change case, in both directions.
        assert_eq!(
            classify_lifecycle_transition(Some(LifecycleState::Active), LifecycleState::Active),
            None
        );
        assert_eq!(
            classify_lifecycle_transition(
                Some(LifecycleState::ExpiredContract),
                LifecycleState::ExpiredContract
            ),
            None
        );
        // One expired kind reclassified into another is a real change.
        assert_eq!(
            classify_lifecycle_transition(
                Some(LifecycleState::ExpiredFromFno),
                LifecycleState::ExpiredContract
            ),
            Some(TransitionKind::Updated)
        );
    }

    #[test]
    fn build_lifecycle_rows_expires_an_instrument_that_vanished_from_the_master() {
        // The half no date can detect: a stock that dropped out of F&O has no
        // expiry to compare, so absence is the only signal there is.
        let master = vec![row(1, InstrumentClass::Equity, "NSE", 0, OptionLeg::None)];
        let known = vec![
            KnownInstrument {
                security_id: 1,
                segment: "NSE_EQ",
                active: true,
                state: LifecycleState::Active,
                class: InstrumentClass::Equity,
            },
            KnownInstrument {
                security_id: 99,
                segment: "NSE_EQ",
                active: true,
                state: LifecycleState::Active,
                class: InstrumentClass::Equity,
            },
        ];
        let (rows, tally) = build_lifecycle_rows(&master, &known, TODAY, TODAY_NANOS, "sha");
        assert_eq!(tally.expired_by_absence, 1);
        let gone = rows
            .iter()
            .find(|r| r.security_id == 99)
            .expect("the vanished instrument gets a row, it is never deleted");
        assert_eq!(gone.lifecycle_state, LifecycleState::ExpiredFromFno);
        assert_eq!(
            gone.last_seen_date_nanos, 0,
            "last_seen must NOT advance — it is the one fact that says when \
             the instrument was last real"
        );
    }

    #[test]
    fn build_lifecycle_rows_does_not_re_expire_an_already_expired_instrument() {
        // Writing the row again every morning would keep resetting
        // expired_date to today, so the record of WHEN it expired would
        // advance forever and never be true.
        let known = vec![KnownInstrument {
            security_id: 99,
            segment: "NSE_EQ",
            active: false,
            state: LifecycleState::ExpiredFromFno,
            class: InstrumentClass::Equity,
        }];
        let (rows, tally) = build_lifecycle_rows(&[], &known, TODAY, TODAY_NANOS, "sha");
        assert_eq!(tally.expired_by_absence, 0);
        assert!(rows.is_empty());
    }

    #[test]
    fn build_lifecycle_rows_keys_absence_on_the_composite_not_the_id() {
        // I-P1-11: the same numeric id in two segments is two instruments.
        // Keying absence on the id alone would see id 7 in today's master and
        // conclude the OTHER id 7 is still present — permanently failing to
        // expire it in a table that is never cleaned up.
        let master = vec![row(7, InstrumentClass::Equity, "NSE", 0, OptionLeg::None)];
        let known = vec![KnownInstrument {
            security_id: 7,
            segment: "BSE_EQ",
            active: true,
            state: LifecycleState::Active,
            class: InstrumentClass::Equity,
        }];
        let (_, tally) = build_lifecycle_rows(&master, &known, TODAY, TODAY_NANOS, "sha");
        assert_eq!(
            tally.expired_by_absence, 1,
            "the BSE instrument is gone even though an NSE one shares its id"
        );
    }

    #[test]
    fn build_lifecycle_rows_counts_what_it_skips() {
        let master = vec![
            row(
                1,
                InstrumentClass::StockOption,
                "MCX",
                2026_08_26,
                OptionLeg::Call,
            ),
            row(2, InstrumentClass::Other, "NSE", 0, OptionLeg::None),
        ];
        let (rows, tally) = build_lifecycle_rows(&master, &[], TODAY, TODAY_NANOS, "sha");
        assert!(rows.is_empty());
        assert_eq!(tally.skipped_unknown_segment, 2);
        assert_eq!(tally.rows(), 0);
    }

    #[test]
    fn expiry_ymd_to_ist_nanos_matches_known_epochs() {
        assert_eq!(expiry_ymd_to_ist_nanos(0), 0);
        assert_eq!(expiry_ymd_to_ist_nanos(1970_01_01), 0);
        assert_eq!(expiry_ymd_to_ist_nanos(1970_01_02), 86_400_000_000_000_i64);
        assert_eq!(
            expiry_ymd_to_ist_nanos(2000_03_01),
            951_868_800_000_000_000_i64,
            "the day after a leap day in a 400-divisible year"
        );
        // Monotonic across a leap boundary — an expiry ladder is compared by
        // this value, so a non-monotonic conversion reorders contracts.
        assert!(expiry_ymd_to_ist_nanos(2028_02_28) < expiry_ymd_to_ist_nanos(2028_02_29));
        assert!(expiry_ymd_to_ist_nanos(2028_02_29) < expiry_ymd_to_ist_nanos(2028_03_01));
        assert_eq!(expiry_ymd_to_ist_nanos(2026_13_01), 0, "month 13 refused");
    }

    #[test]
    fn strike_paise_to_rupees_is_exact_for_real_strikes() {
        assert!((strike_paise_to_rupees(2_450_000) - 24_500.0).abs() < f64::EPSILON);
        assert!((strike_paise_to_rupees(123_455) - 1_234.55).abs() < 1e-9);
        assert!((strike_paise_to_rupees(0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn build_known_instruments_query_reads_only_live_dhan_rows() {
        let sql = build_known_instruments_query();
        assert!(sql.contains("feed = 'dhan'"));
        assert!(
            sql.contains("dry_run = false"),
            "a dry-run row must never drive a real expiry decision"
        );
        assert!(
            !sql.contains("LATEST ON"),
            "the designated ts is pinned, so there is already one row per \
             instrument and a latest-on would sort the table for nothing"
        );
    }

    #[test]
    fn parse_known_instruments_skips_bad_rows_without_losing_the_good_ones() {
        let body = r#"{"dataset":[
            [2885,"NSE_EQ","active","EQUITY"],
            ["1594","NSE_EQ","expired_from_fno","EQUITY"],
            [3,"MCX_COMM","active","OPTSTK"],
            [4,"NSE_EQ","not_a_state","EQUITY"],
            [5,"NSE_EQ","active"]
        ]}"#;
        let k = parse_known_instruments(body).expect("parses");
        assert_eq!(k.len(), 2, "unknown segment, unknown state, short row");
        assert!(k.iter().any(|i| i.security_id == 2885 && i.active));
        assert!(k.iter().any(|i| i.security_id == 1594 && !i.active));
    }

    #[test]
    fn parse_known_instruments_errors_rather_than_reporting_an_empty_table() {
        // An empty result and a failed read must not look the same: an empty
        // read would expire NOTHING, while silently treating a failure as
        // "the table is empty" is the reverse — both are wrong, and only an
        // error lets the caller pick the safe one.
        assert!(parse_known_instruments("not json").is_err());
        assert!(parse_known_instruments(r#"{"columns":[]}"#).is_err());
    }
}
