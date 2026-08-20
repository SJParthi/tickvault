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

use std::collections::HashMap;

use tickvault_core::instrument::master_csv::{InstrumentClass, MasterRow, OptionLeg};
use tickvault_storage::instrument_lifecycle_persistence::{
    InstrumentLifecycleRow, LIFECYCLE_FEED_DHAN, LifecycleState, append_instrument_lifecycle_rows,
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
            lot_size: 0,
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
/// Propagates the first chunk failure. A partial write is safe by
/// construction — every row is independently idempotent, so the next day's
/// run re-sends whatever did not land.
// TEST-EXEMPT: async I/O composition (one /exec read + chunked ILP writes); the pure parts — build_lifecycle_rows, parse_known_instruments, lifecycle_state_for, absence_state_for, lifecycle_segment_for, expiry_ymd_to_ist_nanos — are each tested below.
pub async fn write_dhan_lifecycle(
    questdb: &tickvault_common::config::QuestDbConfig,
    master: &[MasterRow],
    today_ymd: u32,
    today_ist_nanos: i64,
    source_sha256: &str,
) -> anyhow::Result<LifecycleTally> {
    let known = fetch_known_instruments(questdb).await;
    let (rows, tally) =
        build_lifecycle_rows(master, &known, today_ymd, today_ist_nanos, source_sha256);

    for chunk in rows.chunks(LIFECYCLE_WRITE_CHUNK) {
        append_instrument_lifecycle_rows(questdb, chunk).await?;
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
        tracing::error!("lifecycle: HTTP client build failed — absence pass skipped");
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
            tracing::error!(status = %resp.status(), "lifecycle: known-instruments query non-2xx");
            None
        }
        Err(err) => {
            tracing::error!(?err, "lifecycle: known-instruments query failed");
            None
        }
    };
    let Some(body) = body else { return Vec::new() };
    match parse_known_instruments(&body) {
        Ok(k) => k,
        Err(err) => {
            tracing::error!(
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
                class: InstrumentClass::Equity,
            },
            KnownInstrument {
                security_id: 99,
                segment: "NSE_EQ",
                active: true,
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
