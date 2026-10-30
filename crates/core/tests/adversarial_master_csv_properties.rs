//! Adversarial properties of the Dhan detailed instrument-master CSV parser.
//!
//! Surface: `tickvault_core::instrument::master_csv::{parse_master_csv,
//! split_csv_line, parse_expiry_ymd, parse_strike_paise, parse_lot_size}`,
//! returning `MasterRow` / `MasterParseError`. The CSV is downloaded from
//! the vendor every morning and is the only source of instrument identity.
//!
//! Properties:
//!
//! 1. Arbitrary text never panics; it is `Ok` only with at least
//!    `MIN_PLAUSIBLE_MASTER_ROWS` rows (arbitrary text never reaches it).
//! 2. Round trip: generated rows (random column order, extra unknown
//!    columns, quoted fields carrying commas and quotes, mixed-case header
//!    names and enum words, vendor decimal spellings like `75.0`) serialize
//!    and parse back to exactly the generated `MasterRow`s, in order.
//! 3. CRLF vs LF, a UTF-8 BOM, and blank / whitespace-only lines anywhere
//!    give identical rows (the parser documents all three).
//! 4. Dropping any non-empty subset of the six mandatory header columns is
//!    refused with `MissingColumn(<first missing in resolution order>)`.
//! 5. The bad-row tolerance and the plausibility floor refuse with the exact
//!    `TooManyBadRows { bad, total }` / `ImplausiblyFewRows { rows, min }`
//!    counts; whitespace-only input is `Empty`.
//! 6. `split_csv_line` round-trips any field list written with RFC-4180
//!    quoting, and its field count equals one plus the commas outside
//!    quotes on arbitrary lines.
//! 7. `parse_expiry_ymd`, `parse_strike_paise`, `parse_lot_size` agree with
//!    references written here (chrono's calendar for dates, integer
//!    arithmetic for paise and lots) on generated numeric strings, and on
//!    arbitrary strings never panic and never return a value outside their
//!    contract (a real calendar date in 1970..=2999 or 0; paise >= 0 and
//!    below 2^53; a lot that the text actually spells, or 0).
//!
//! Honest limits: a field containing a raw newline is out of the parser's
//! contract (documented) and is not generated in the round trip. Each
//! round-trip case pads to the 1,000-row floor with fixed filler rows, so a
//! case is ~1,000 rows; case counts are set to keep the file fast in debug.
//! `parse_lot_size` accepts a decimal spelling whose fraction is below
//! `f64` resolution (`75.000000000000001` reads as 75); generated spellings
//! stay within 6 fractional digits, and that edge is pinned by the ignored
//! `bug_parse_lot_size_refuses_every_non_zero_fraction`.

use chrono::{Datelike, NaiveDate};
use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use tickvault_core::instrument::master_csv::{
    InstrumentClass, MAX_BAD_ROW_FRACTION, MIN_PLAUSIBLE_MASTER_ROWS, MasterParseError, MasterRow,
    OptionLeg, parse_expiry_ymd, parse_lot_size, parse_master_csv, parse_strike_paise,
    split_csv_line,
};

/// The six mandatory columns, in the order the parser resolves them (so the
/// first missing one in this order is the one named in the error).
const MANDATORY: [&str; 6] = [
    "SECURITY_ID",
    "ISIN",
    "SYMBOL_NAME",
    "EXCH_ID",
    "SEGMENT",
    "SERIES",
];

/// The six optional derivative columns.
const OPTIONAL: [&str; 6] = [
    "INSTRUMENT",
    "SM_EXPIRY_DATE",
    "STRIKE_PRICE",
    "OPTION_TYPE",
    "UNDERLYING_SYMBOL",
    "LOT_SIZE",
];

const ALL_COLUMNS: usize = MANDATORY.len() + OPTIONAL.len();

/// One generated record: the TEXT written for each of the twelve columns
/// (in `MANDATORY ++ OPTIONAL` order) and the row it must parse to.
#[derive(Clone, Debug)]
struct Record {
    cells: [String; ALL_COLUMNS],
    expected: MasterRow,
}

/// Quote a field the way RFC-4180 requires; `force` quotes even when not
/// needed (always legal).
fn csv_field(text: &str, force: bool) -> String {
    if force || text.contains(',') || text.contains('"') {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// Mixed-case spelling of an ASCII word, driven by a bit mask.
fn mixed_case(word: &str, mask: u64) -> String {
    word.chars()
        .enumerate()
        .map(|(i, c)| {
            if mask >> (i % 64) & 1 == 1 {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// Upper-case ASCII text with internal punctuation, already trimmed (the
/// parser trims and upper-cases, so only such text round-trips verbatim).
fn upper_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[A-Z0-9][A-Z0-9 ,&\"\\-.]{0,14}[A-Z0-9]",
        "[A-Z0-9]{1,8}",
    ]
}

fn class_words() -> impl Strategy<Value = (&'static str, InstrumentClass)> {
    prop::sample::select(vec![
        ("INDEX", InstrumentClass::Index),
        ("EQUITY", InstrumentClass::Equity),
        ("FUTIDX", InstrumentClass::IndexFuture),
        ("FUTSTK", InstrumentClass::StockFuture),
        ("OPTIDX", InstrumentClass::IndexOption),
        ("OPTSTK", InstrumentClass::StockOption),
        ("FUTCUR", InstrumentClass::Other),
        ("", InstrumentClass::Other),
    ])
}

fn leg_words() -> impl Strategy<Value = (&'static str, OptionLeg)> {
    prop::sample::select(vec![
        ("CE", OptionLeg::Call),
        ("PE", OptionLeg::Put),
        ("XX", OptionLeg::None),
        ("", OptionLeg::None),
    ])
}

/// A real calendar date in the accepted range, written in one of the two
/// vendor shapes, or blank.
fn expiry_cell() -> impl Strategy<Value = (String, u32)> {
    prop_oneof![
        Just((String::new(), 0)),
        (0i32..(2999 - 1970) * 366, 0u8..3).prop_map(|(offset, shape)| {
            let date = NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch")
                + chrono::Duration::days(i64::from(offset));
            let date = if date.year() > 2999 {
                NaiveDate::from_ymd_opt(2999, 12, 31).expect("max")
            } else {
                date
            };
            let ymd = u32::try_from(date.year()).expect("year") * 10_000
                + date.month() * 100
                + date.day();
            let text = match shape {
                0 => date.format("%Y-%m-%d").to_string(),
                1 => date.format("%Y-%m-%d 14:30:00").to_string(),
                _ => date.format("%Y-%m-%dT09:15:00").to_string(),
            };
            (text, ymd)
        }),
    ]
}

/// A strike in paise written as rupees, `I`, `I.F` or `I.FF`.
fn strike_cell() -> impl Strategy<Value = (String, i64)> {
    prop_oneof![
        Just((String::new(), 0)),
        (1i64..100_000_000_000, 0u8..3).prop_map(|(paise, shape)| match shape {
            0 => (format!("{}", paise), paise * 100),
            1 => (format!("{}.{}", paise / 10, paise % 10), paise * 10),
            _ => (format!("{}.{:02}", paise / 100, paise % 100), paise),
        }),
    ]
}

/// A lot written as the vendor writes it (`75.0`) or as an integer.
fn lot_cell() -> impl Strategy<Value = (String, u32)> {
    prop_oneof![
        Just((String::new(), 0)),
        (1u32..=u32::MAX, any::<bool>()).prop_map(|(lot, decimal)| {
            if decimal {
                (format!("{lot}.0"), lot)
            } else {
                (lot.to_string(), lot)
            }
        }),
    ]
}

fn record() -> impl Strategy<Value = Record> {
    (
        (
            any::<u64>(),
            upper_text(),
            upper_text(),
            upper_text(),
            upper_text(),
            upper_text(),
        ),
        (
            class_words(),
            expiry_cell(),
            strike_cell(),
            leg_words(),
            upper_text(),
            lot_cell(),
        ),
        any::<u64>(),
        " {0,2}",
    )
        .prop_map(
            |(
                (security_id, isin, symbol, exch, segment, series),
                (
                    (class_word, class),
                    (expiry_text, expiry_ymd),
                    (strike_text, strike_paise),
                    (leg_word, leg),
                    underlying,
                    (lot_text, lot),
                ),
                case_mask,
                pad,
            )| {
                let cells = [
                    format!("{pad}{security_id}{pad}"),
                    isin.clone(),
                    symbol.clone(),
                    exch.clone(),
                    segment.clone(),
                    series.clone(),
                    mixed_case(class_word, case_mask),
                    format!("{pad}{expiry_text}"),
                    format!("{strike_text}{pad}"),
                    mixed_case(leg_word, case_mask >> 8),
                    underlying.clone(),
                    lot_text,
                ];
                let expected = MasterRow {
                    security_id,
                    isin,
                    symbol_name: symbol,
                    exch_id: exch,
                    segment,
                    series,
                    class,
                    expiry_ymd,
                    strike_paise,
                    option_leg: leg,
                    underlying_symbol: underlying,
                    lot_size: lot,
                };
                Record { cells, expected }
            },
        )
}

/// A fixed filler row (all twelve columns) and the row it parses to.
fn filler(index: usize) -> Record {
    let id = 900_000_000 + index as u64;
    let cells = [
        id.to_string(),
        format!("INE{index:09}"),
        format!("FILL{index}"),
        "NSE".to_string(),
        "E".to_string(),
        "EQ".to_string(),
        "EQUITY".to_string(),
        String::new(),
        String::new(),
        String::new(),
        format!("FILL{index}"),
        "1.0".to_string(),
    ];
    let expected = MasterRow {
        security_id: id,
        isin: format!("INE{index:09}"),
        symbol_name: format!("FILL{index}"),
        exch_id: "NSE".to_string(),
        segment: "E".to_string(),
        series: "EQ".to_string(),
        class: InstrumentClass::Equity,
        expiry_ymd: 0,
        strike_paise: 0,
        option_leg: OptionLeg::None,
        underlying_symbol: format!("FILL{index}"),
        lot_size: 1,
    };
    Record { cells, expected }
}

/// How a CSV document is laid out.
#[derive(Clone, Debug)]
struct Layout {
    /// Column order: indexes into the twelve, plus `None` for an unknown
    /// extra column.
    columns: Vec<Option<usize>>,
    /// Bit mask lower-casing header letters.
    header_case: u64,
    /// Quote every field even when not needed.
    force_quotes: bool,
}

fn layout() -> impl Strategy<Value = Layout> {
    (
        Just((0..ALL_COLUMNS).map(Some).collect::<Vec<_>>()).prop_shuffle(),
        prop::collection::vec(any::<prop::sample::Index>(), 0..3),
        any::<u64>(),
        any::<bool>(),
    )
        .prop_map(|(mut columns, extras, header_case, force_quotes)| {
            for at in extras {
                let index = at.index(columns.len() + 1);
                columns.insert(index, None);
            }
            Layout {
                columns,
                header_case,
                force_quotes,
            }
        })
}

fn column_name(column: usize) -> &'static str {
    if column < MANDATORY.len() {
        MANDATORY[column]
    } else {
        OPTIONAL[column - MANDATORY.len()]
    }
}

/// Serializes `records` under `layout`, skipping any column in `dropped`.
fn render(records: &[Record], layout: &Layout, dropped: &[usize]) -> Vec<String> {
    let kept: Vec<Option<usize>> = layout
        .columns
        .iter()
        .copied()
        .filter(|c| c.is_none_or(|c| !dropped.contains(&c)))
        .collect();
    let header: Vec<String> = kept
        .iter()
        .enumerate()
        .map(|(i, c)| match c {
            Some(c) => csv_field(
                &mixed_case(column_name(*c), layout.header_case),
                layout.force_quotes,
            ),
            None => format!("EXTRA_{i}"),
        })
        .collect();
    let mut lines = vec![header.join(",")];
    for (row_index, record) in records.iter().enumerate() {
        let cells: Vec<String> = kept
            .iter()
            .map(|c| match c {
                Some(c) => csv_field(&record.cells[*c], layout.force_quotes),
                None => csv_field(&format!("junk,{row_index}\"x"), false),
            })
            .collect();
        lines.push(cells.join(","));
    }
    lines
}

fn with_filler(records: Vec<Record>) -> Vec<Record> {
    let need = MIN_PLAUSIBLE_MASTER_ROWS.saturating_sub(records.len());
    let mut all = records;
    all.extend((0..need).map(filler));
    all
}

// ---------------------------------------------------------------------------
// References for the scalar parsers
// ---------------------------------------------------------------------------

/// `YYYYMMDD` when (y, m, d) is a real date in 1970..=2999, else 0 (chrono's
/// calendar, independent of the module's own `days_in_month`).
fn ref_ymd(y: i64, m: i64, d: i64) -> u32 {
    if !(1970..=2999).contains(&y) {
        return 0;
    }
    match (u32::try_from(m), u32::try_from(d)) {
        (Ok(m), Ok(d)) => match i32::try_from(y)
            .ok()
            .and_then(|y| NaiveDate::from_ymd_opt(y, m, d))
        {
            Some(_) => u32::try_from(y).unwrap_or(0) * 10_000 + m * 100 + d,
            None => 0,
        },
        _ => 0,
    }
}

fn is_valid_packed(ymd: u32) -> bool {
    let (y, m, d) = (ymd / 10_000, ymd / 100 % 100, ymd % 100);
    ref_ymd(i64::from(y), i64::from(m), i64::from(d)) == ymd
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Property 2: generated rows round-trip exactly, in order, through any
    /// column order, extra columns, quoting and header case.
    #[test]
    fn prop_generated_rows_round_trip(
        records in prop::collection::vec(record(), 1..24),
        layout in layout(),
    ) {
        let all = with_filler(records);
        let text = render(&all, &layout, &[]).join("\n");
        let parsed = parse_master_csv(&text);
        prop_assert!(parsed.is_ok(), "{:?}", parsed.err());
        let parsed = parsed.unwrap();
        prop_assert_eq!(parsed.len(), all.len());
        for (got, want) in parsed.iter().zip(&all) {
            prop_assert_eq!(got, &want.expected);
        }
    }

    /// Property 3: LF vs CRLF, a BOM, and blank / whitespace-only lines
    /// anywhere give the identical row vector.
    #[test]
    fn prop_line_endings_bom_and_blank_lines_do_not_change_rows(
        records in prop::collection::vec(record(), 1..12),
        layout in layout(),
        blank_at in prop::collection::vec((any::<prop::sample::Index>(), "[ \t]{0,3}"), 0..5),
        trailing_newline in any::<bool>(),
    ) {
        let all = with_filler(records);
        let lines = render(&all, &layout, &[]);
        let lf = lines.join("\n");
        let baseline = parse_master_csv(&lf);
        prop_assert!(baseline.is_ok(), "{:?}", baseline.err());
        let baseline = baseline.unwrap();

        let crlf = lines.join("\r\n");
        prop_assert_eq!(parse_master_csv(&crlf).ok(), Some(baseline.clone()));
        let bom = format!("\u{feff}{crlf}");
        prop_assert_eq!(parse_master_csv(&bom).ok(), Some(baseline.clone()));

        let mut spaced = lines.clone();
        for (at, blank) in blank_at {
            let index = at.index(spaced.len() + 1);
            spaced.insert(index, blank);
        }
        let mut spaced_text = spaced.join("\r\n");
        if trailing_newline {
            spaced_text.push_str("\r\n");
        }
        let spaced_text = format!("\u{feff}{spaced_text}");
        prop_assert_eq!(parse_master_csv(&spaced_text).ok(), Some(baseline.clone()));
    }

    /// Property 4: any non-empty subset of the mandatory columns dropped is
    /// refused with the first missing one in resolution order.
    #[test]
    fn prop_dropping_mandatory_columns_names_the_first_missing(
        records in prop::collection::vec(record(), 1..4),
        layout in layout(),
        drop_mask in 1u8..64,
    ) {
        let dropped: Vec<usize> = (0..MANDATORY.len()).filter(|i| drop_mask >> i & 1 == 1).collect();
        let first = MANDATORY[dropped[0]];
        let all = with_filler(records);
        let text = render(&all, &layout, &dropped).join("\n");
        prop_assert_eq!(parse_master_csv(&text), Err(MasterParseError::MissingColumn(first)));
    }

    /// Dropping OPTIONAL columns never refuses the file; each dropped one
    /// reads as its absent sentinel and every other field is unchanged.
    #[test]
    fn prop_dropping_optional_columns_reads_absent(
        records in prop::collection::vec(record(), 1..8),
        layout in layout(),
        drop_mask in 0u8..64,
    ) {
        let dropped: Vec<usize> = (0..OPTIONAL.len())
            .filter(|i| drop_mask >> i & 1 == 1)
            .map(|i| i + MANDATORY.len())
            .collect();
        let all = with_filler(records);
        let text = render(&all, &layout, &dropped).join("\n");
        let parsed = parse_master_csv(&text);
        prop_assert!(parsed.is_ok(), "{:?}", parsed.err());
        for (got, want) in parsed.unwrap().iter().zip(&all) {
            let mut want = want.expected.clone();
            for &c in &dropped {
                match column_name(c) {
                    "INSTRUMENT" => want.class = InstrumentClass::Other,
                    "SM_EXPIRY_DATE" => want.expiry_ymd = 0,
                    "STRIKE_PRICE" => want.strike_paise = 0,
                    "OPTION_TYPE" => want.option_leg = OptionLeg::None,
                    "UNDERLYING_SYMBOL" => want.underlying_symbol = String::new(),
                    "LOT_SIZE" => want.lot_size = 0,
                    other => prop_assert!(false, "not optional: {}", other),
                }
            }
            prop_assert_eq!(got, &want);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Property 5: bad rows (unparseable id, or short) are tolerated up to
    /// exactly `MAX_BAD_ROW_FRACTION` of all data rows and excluded from the
    /// output; past it the file is refused with the exact counts. Fewer than
    /// the floor of good rows is `ImplausiblyFewRows` with the exact count.
    #[test]
    fn prop_bad_row_tolerance_and_floor_are_exact(
        good in 990usize..1010,
        bad in 0usize..4,
        short_rows in any::<bool>(),
        positions in prop::collection::vec(any::<prop::sample::Index>(), 4),
    ) {
        let layout = Layout {
            columns: (0..ALL_COLUMNS).map(Some).collect(),
            header_case: 0,
            force_quotes: false,
        };
        let good_rows: Vec<Record> = (0..good).map(filler).collect();
        let mut lines = render(&good_rows, &layout, &[]);
        for (k, at) in positions.iter().take(bad).enumerate() {
            let line = if short_rows {
                format!("{}", 1 + k)
            } else {
                format!("NOT_AN_ID_{k},INE,X,NSE,E,EQ")
            };
            let index = 1 + at.index(lines.len());
            lines.insert(index, line);
        }
        let total = good + bad;
        let result = parse_master_csv(&lines.join("\n"));
        let fraction = bad as f64 / total as f64;
        if fraction > MAX_BAD_ROW_FRACTION {
            prop_assert_eq!(result, Err(MasterParseError::TooManyBadRows { bad, total }));
        } else if good < MIN_PLAUSIBLE_MASTER_ROWS {
            prop_assert_eq!(
                result,
                Err(MasterParseError::ImplausiblyFewRows { rows: good, min: MIN_PLAUSIBLE_MASTER_ROWS })
            );
        } else {
            let rows = result.expect("tolerated");
            let want: Vec<MasterRow> = good_rows.into_iter().map(|r| r.expected).collect();
            prop_assert_eq!(rows, want);
        }
    }
}

#[test]
fn test_empty_and_whitespace_only_input_is_empty() {
    for text in [
        "",
        "\n",
        "\r\n\r\n",
        "   \t  ",
        "\u{feff}",
        "\u{feff}\n  \n",
    ] {
        assert_eq!(
            parse_master_csv(text),
            Err(MasterParseError::Empty),
            "{text:?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// Property 1: arbitrary text never panics; an `Ok` is never smaller
    /// than the floor.
    #[test]
    fn prop_arbitrary_text_never_panics(text in "\\PC{0,400}") {
        if let Ok(rows) = parse_master_csv(&text) {
            prop_assert!(rows.len() >= MIN_PLAUSIBLE_MASTER_ROWS);
        }
    }

    /// CSV-shaped garbage under a valid header: never panics; when it is
    /// `Ok` (it cannot be, with fewer rows than the floor) the invariants
    /// hold; otherwise the error is one of the typed variants with
    /// consistent counts.
    #[test]
    fn prop_csv_shaped_garbage_is_refused_with_consistent_counts(
        lines in prop::collection::vec("[ -~]{0,60}", 0..40),
    ) {
        let mut text = String::from("SECURITY_ID,ISIN,SYMBOL_NAME,EXCH_ID,SEGMENT,SERIES,INSTRUMENT,SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,UNDERLYING_SYMBOL,LOT_SIZE\n");
        text.push_str(&lines.join("\n"));
        let data_lines = lines.iter().filter(|l| !l.trim().is_empty()).count();
        match parse_master_csv(&text) {
            Ok(rows) => prop_assert!(rows.len() >= MIN_PLAUSIBLE_MASTER_ROWS),
            Err(MasterParseError::TooManyBadRows { bad, total }) => {
                prop_assert_eq!(total, data_lines);
                prop_assert!(bad >= 1 && bad <= total);
            }
            Err(MasterParseError::ImplausiblyFewRows { rows, min }) => {
                prop_assert_eq!(min, MIN_PLAUSIBLE_MASTER_ROWS);
                prop_assert!(rows <= data_lines);
            }
            Err(other) => prop_assert!(false, "unexpected {:?}", other),
        }
    }

    /// Property 6a: any field list written with RFC-4180 quoting splits
    /// back to the same fields (no raw newlines: out of contract).
    #[test]
    fn prop_split_csv_line_round_trips_quoted_fields(
        fields in prop::collection::vec("[^\r\n]{0,12}", 1..10),
        force in any::<bool>(),
    ) {
        let line = fields.iter().map(|f| csv_field(f, force)).collect::<Vec<_>>().join(",");
        let mut out = Vec::new();
        split_csv_line(&line, &mut out);
        prop_assert_eq!(out, fields);
    }

    /// Property 6b: on arbitrary lines the field count is one plus the
    /// commas outside quotes (reference: a quote toggles the state, a doubled
    /// quote inside quotes stays inside), and the buffer is cleared on entry.
    #[test]
    fn prop_split_csv_line_field_count_matches_reference(
        line in "[a,\" ]{0,40}",
        stale in prop::collection::vec("[a-z]{0,3}", 0..5),
    ) {
        let mut expected = 1usize;
        let mut in_quotes = false;
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '"' if in_quotes => {
                    if chars.get(i + 1) == Some(&'"') {
                        i += 1;
                    } else {
                        in_quotes = false;
                    }
                }
                '"' => in_quotes = true,
                ',' if !in_quotes => expected += 1,
                _ => {}
            }
            i += 1;
        }
        let mut out = stale;
        split_csv_line(&line, &mut out);
        prop_assert_eq!(out.len(), expected, "{:?}", line);
    }
}

// ---------------------------------------------------------------------------
// Property 7: scalar parsers
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// Generated `Y-M-D` (padded or not, with a time suffix or not, padded
    /// with spaces) agrees with chrono's calendar, including impossible days.
    #[test]
    fn prop_parse_expiry_ymd_agrees_with_calendar(
        y in 1900i64..3100,
        m in 0i64..15,
        d in 0i64..34,
        zero_pad in any::<bool>(),
        suffix in prop::sample::select(vec!["", " 14:30:00", "T09:15:00", " 00:00"]),
        pad in " {0,2}",
    ) {
        let text = if zero_pad {
            format!("{pad}{y:04}-{m:02}-{d:02}{suffix}{pad}")
        } else {
            format!("{pad}{y}-{m}-{d}{suffix}{pad}")
        };
        prop_assert_eq!(parse_expiry_ymd(&text), ref_ymd(y, m, d), "{:?}", text);
    }

    /// Arbitrary strings: never a panic, and the result is 0 or a real date
    /// in range (a garbled field never invents an impossible expiry).
    #[test]
    fn prop_parse_expiry_ymd_never_invents_a_date(text in "\\PC{0,30}|[0-9 :T+-]{0,24}") {
        let ymd = parse_expiry_ymd(&text);
        prop_assert!(ymd == 0 || is_valid_packed(ymd), "{:?} -> {}", text, ymd);
    }

    /// Generated rupee spellings agree with integer paise arithmetic.
    #[test]
    fn prop_parse_strike_paise_agrees_with_integer_reference(
        rupees in 0i64..1_000_000_000,
        frac in 0i64..100,
        shape in 0u8..4,
        negative in any::<bool>(),
        pad in "[ \t]{0,2}",
        plus in any::<bool>(),
    ) {
        let (body, paise) = match shape {
            0 => (format!("{rupees}"), rupees * 100),
            1 => (format!("{rupees}.{}", frac / 10), rupees * 100 + frac / 10 * 10),
            2 => (format!("{rupees}.{frac:02}"), rupees * 100 + frac),
            _ => (format!("{rupees}.{frac:02}0000"), rupees * 100 + frac),
        };
        let sign = if negative { "-" } else if plus { "+" } else { "" };
        let text = format!("{pad}{sign}{body}{pad}");
        let want = if negative { 0 } else { paise };
        prop_assert_eq!(parse_strike_paise(&text), want, "{:?}", text);
    }

    /// Arbitrary strings: never a panic; the result is 0 or a positive
    /// whole paise below 2^53 that the text actually spells (to rounding).
    #[test]
    fn prop_parse_strike_paise_never_fabricates(text in "\\PC{0,30}|[0-9.eE+ -]{0,24}|(NaN|inf|-inf|infinity)") {
        let paise = parse_strike_paise(&text);
        prop_assert!((0..(1i64 << 53)).contains(&paise), "{:?} -> {}", text, paise);
        if paise > 0 {
            let spelled: f64 = text.trim().parse().expect("a positive strike spells a number");
            prop_assert!(spelled.is_finite() && spelled > 0.0);
            prop_assert!(((spelled * 100.0).round() - paise as f64).abs() < 1.0, "{:?} -> {}", text, paise);
        }
    }

    /// Generated lot spellings agree with an integer reference: whole lots
    /// in `u32` (integer or `.0`/`.00` decimal, padded, `+`) read exactly; a
    /// fraction, a negative, zero or a value past `u32::MAX` is 0.
    #[test]
    fn prop_parse_lot_size_agrees_with_integer_reference(
        lot in 0u64..=(u64::from(u32::MAX) + 10),
        shape in 0u8..6,
        frac in 1u32..1_000_000,
        pad in "[ \t]{0,2}",
    ) {
        let in_range = lot >= 1 && lot <= u64::from(u32::MAX);
        let (text, want) = match shape {
            0 => (format!("{pad}{lot}{pad}"), if in_range { lot } else { 0 }),
            1 => (format!("{lot}.0"), if in_range { lot } else { 0 }),
            2 => (format!("+{lot}.00{pad}"), if in_range { lot } else { 0 }),
            3 => (format!("{lot}.{frac:06}"), 0),
            4 => (format!("-{lot}"), 0),
            _ => (format!("-{lot}.0"), 0),
        };
        prop_assert_eq!(u64::from(parse_lot_size(&text)), want, "{:?}", text);
    }

    /// Arbitrary strings: never a panic; a non-zero lot is a number the text
    /// spells.
    #[test]
    fn prop_parse_lot_size_never_fabricates(text in "\\PC{0,30}|[0-9.eE+ -]{0,24}") {
        let lot = parse_lot_size(&text);
        if lot > 0 {
            let spelled: f64 = text.trim().parse().expect("a lot spells a number");
            prop_assert_eq!(spelled, f64::from(lot), "{:?}", text);
        }
    }
}

/// Fixed hostile spellings for the three scalar parsers, with the verdict
/// each contract demands.
#[test]
fn test_scalar_parsers_hostile_table() {
    for (text, want) in [
        ("2026-02-29", 0),
        ("2028-02-29", 20_280_229),
        ("1900-02-29", 0),
        ("2000-02-29", 20_000_229),
        ("1969-12-31", 0),
        ("3000-01-01", 0),
        ("2026-13-01", 0),
        ("2026-00-10", 0),
        ("2026-04-31", 0),
        ("2026-04-30", 20_260_430),
        ("", 0),
        ("2026", 0),
        ("2026-04", 0),
        ("99999999999-01-01", 0),
        ("2026-04-30 14:30:00", 20_260_430),
    ] {
        assert_eq!(parse_expiry_ymd(text), want, "{text:?}");
    }
    for (text, want) in [
        ("24500.999999", 2_450_100),
        ("0", 0),
        ("-0", 0),
        ("0.001", 0),
        ("0.005", 1),
        ("NaN", 0),
        ("inf", 0),
        ("1e308", 0),
        ("90071992547409.92", 0),
        ("90071992547409.91", 9_007_199_254_740_991),
        ("", 0),
    ] {
        assert_eq!(parse_strike_paise(text), want, "{text:?}");
    }
    for (text, want) in [
        ("75", 75),
        ("75.0", 75),
        ("75.5", 0),
        ("0", 0),
        ("0.0", 0),
        ("-75", 0),
        ("4294967295", u32::MAX),
        ("4294967295.0", u32::MAX),
        ("4294967296", 0),
        ("4294967296.0", 0),
        ("1e2", 100),
        ("NaN", 0),
        ("inf", 0),
        ("", 0),
    ] {
        assert_eq!(parse_lot_size(text), want, "{text:?}");
    }
}

/// BUG (low; found 2026-10-30): `parse_lot_size` documents "Only a value
/// whose fraction is exactly zero is a lot size" and "a fractional lot is
/// refused rather than truncated", but it decides the fraction on the `f64`
/// the text rounds to, so a fraction below `f64` resolution is truncated
/// away and the lot is accepted.
///
/// Minimal input: `"75.000000000000001"` returns 75 (the nearest `f64` is
/// exactly 75.0). Real vendor files carry `75.0`, so this is not reachable
/// from today's master; it is a contract gap, not a live loss.
///
/// Proposed fix: before the `f64` path, when the trimmed text has a `.`,
/// require every digit after it (up to an optional exponent) to be `0`, or
/// parse the decimal exactly (integer part, fraction digits, exponent) the
/// way `order_update::exact_decimal_i64` does.
#[test]
#[ignore = "BUG: parse_lot_size truncates a fraction below f64 resolution (75.000000000000001 -> 75)"]
fn bug_parse_lot_size_refuses_every_non_zero_fraction() {
    for text in [
        "75.000000000000001",
        "1.0000000000000001",
        "250.00000000000001",
    ] {
        assert_eq!(parse_lot_size(text), 0, "{text:?} has a non-zero fraction");
    }
}
