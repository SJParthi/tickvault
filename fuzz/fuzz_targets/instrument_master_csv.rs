//! Fuzz target: the Dhan instrument master CSV.
//!
//! Feeds arbitrary bytes (as UTF-8 text) to `parse_master_csv` and every
//! cell parser. Properties beyond "no panic":
//!
//! * a lot size is exactly what an exact decimal reference reads (digit
//!   shift over the text, no `f64`): the 2026-10-30 fix, exponents included;
//! * `parse_master_csv` refuses or returns rows; it never panics on short
//!   rows, quotes, a BOM or CRLF.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tickvault_core::instrument::master_csv::{
    parse_expiry_ymd, parse_lot_size, parse_master_csv, parse_strike_paise,
};

/// Large enough for many rows; the real file is parsed once a day.
const MAX_INPUT: usize = 256 * 1024;

/// Exact reference for `parse_lot_size`, independent of the parser: the
/// decimal text (optional `+`, digits with at most one `.`, optional
/// exponent) is shifted by its exponent over its digit string; a lot is the
/// integer before the point when every digit after it is `0` and it lies in
/// `1..=u32::MAX`. No `f64` anywhere, so a fraction below `f64` resolution
/// or hidden behind an exponent is seen.
fn ref_exact_lot(text: &str) -> u64 {
    let unsigned = text.trim().strip_prefix('+').unwrap_or(text.trim());
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], &unsigned[at + 1..]),
        None => (unsigned, "0"),
    };
    let Ok(exponent) = exponent.parse::<i64>() else {
        return 0;
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits: String = format!("{integer}{fraction}");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return 0;
    }
    // Position of the point in `digits` after the shift.
    let point = i64::try_from(integer.len())
        .unwrap_or(i64::MAX)
        .saturating_add(exponent);
    let len = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    if point < len
        && digits[usize::try_from(point.max(0)).unwrap_or(0)..]
            .bytes()
            .any(|b| b != b'0')
    {
        return 0;
    }
    // A whole number: its digits, then zeros up to the point. Anything
    // longer than 11 significant places is beyond u32.
    let whole_len = point.saturating_sub(len - i64::try_from(significant.len()).unwrap_or(0));
    if whole_len > 11 {
        return 0;
    }
    let kept = usize::try_from(point.min(len)).unwrap_or(0);
    let mut whole = digits[..kept].to_owned();
    for _ in 0..point.saturating_sub(len) {
        whole.push('0');
    }
    match whole.parse::<u64>() {
        Ok(lot) if (1..=u64::from(u32::MAX)).contains(&lot) => lot,
        _ => 0,
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = parse_master_csv(text);
    // The whole input as one cell too, so long spellings reach the oracle.
    for cell in std::iter::once(text).chain(text.split([',', '\n']).take(64)) {
        let _ = parse_expiry_ymd(cell);
        let _ = parse_strike_paise(cell);
        assert_eq!(
            u64::from(parse_lot_size(cell)),
            ref_exact_lot(cell),
            "lot size disagrees with the exact decimal reading of {cell:?}"
        );
    }
});
