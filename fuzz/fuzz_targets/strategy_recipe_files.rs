//! Fuzz target: the brutex strategy folder files read from
//! `s3://tv-prod-cold/strategy-recipes/` (recipe, params, gates, universe,
//! the three check files, folder and snapshot names).
//!
//! Properties beyond "no panic":
//!
//! * every refusal is the documented typed variant for that file
//!   (`BadRecipe` for JSON files, `BadCheckFile` / `EmptyCandles` for CSV);
//! * an accepted candle file has strictly valid bars (finite, low <= open,
//!   close <= high);
//! * `parse_version` only accepts the canonical `v<N>` spelling.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tickvault_trading::brutex_host::Refusal;
use tickvault_trading::brutex_host::check_files::{parse_candles, parse_signals, parse_values};
use tickvault_trading::brutex_host::fixture::FIXTURE_COMMIT;
use tickvault_trading::brutex_host::recipe::{
    GatesFile, ParamSnapshot, UniverseSnapshot, parse_json, parse_recipe,
};
use tickvault_trading::brutex_host::select::{is_iso_date, parse_version};

const MAX_INPUT: usize = 64 * 1024;

fn check_refusal(r: &Refusal) {
    assert!(
        matches!(r, Refusal::BadCheckFile { .. } | Refusal::EmptyCandles),
        "untyped check-file refusal: {r:?}"
    );
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let folder = data.first().map_or(1, |b| u32::from(*b));
    match parse_recipe(data, folder, FIXTURE_COMMIT) {
        Ok(recipe) => {
            assert_eq!(recipe.status, "approved");
            assert_eq!(recipe.version, folder);
        }
        Err(
            Refusal::BadRecipe(_)
            | Refusal::NotApproved(_)
            | Refusal::VersionMismatch { .. }
            | Refusal::CommitMismatch { .. },
        ) => {}
        Err(other) => panic!("untyped recipe refusal: {other:?}"),
    }
    let _ = parse_json::<GatesFile>(data, "gates");
    let _ = parse_json::<ParamSnapshot>(data, "params");
    let _ = parse_json::<UniverseSnapshot>(data, "universe");
    match parse_candles("check_candles.csv", data) {
        Ok(bars) => {
            for b in bars {
                assert!(b.open.is_finite() && b.close.is_finite());
                assert!(
                    b.low <= b.open && b.open <= b.high && b.low <= b.close && b.close <= b.high
                );
            }
        }
        Err(r) => check_refusal(&r),
    }
    if let Err(r) = parse_values("check_values.csv", data) {
        check_refusal(&r);
    }
    if let Err(r) = parse_signals("check_signals.csv", data) {
        check_refusal(&r);
    }
    if let Ok(name) = std::str::from_utf8(data) {
        if let Some(n) = parse_version(name) {
            assert_eq!(format!("v{n}"), name, "non-canonical version accepted");
        }
        if is_iso_date(name) {
            assert_eq!(name.len(), 10);
        }
    }
});
