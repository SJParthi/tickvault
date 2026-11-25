//! Drift pin: `config/base.toml`'s `[trading] market_close_time` must equal the
//! session end the ingest gate actually enforces
//! (`TICK_PERSIST_END_SECS_OF_DAY_IST`).
//!
//! # Why this guard exists (2026-09-03)
//!
//! The session close moved 15:30 -> 15:40 IST on 2026-08-07 for the NSE closing
//! auction. That move was applied to the ingest gate and **missed
//! `market_close_time`**, which sat at 15:30 for nearly a month with two
//! production consumers, both wrong:
//!
//! 1. `main.rs` -> `daily_archive_boot::spawn_daily_partition_archive`, whose
//!    window opens at close + 2 s. At 15:30:02 the heaviest disk job of the day
//!    — S3 verify + `DROP PARTITION` across `ticks`, `market_depth` and all 24
//!    candle tables — ran for TEN MINUTES **concurrently with live capture**, at
//!    peak closing-auction volume. `window_open_secs_of_day`'s own comment
//!    justifies the 2 s buffer as making the sweep "start after in-flight ticks
//!    land"; that premise died on 2026-08-07.
//! 2. `boot_helpers::compute_market_close_sleep`, which fires the "Market closed
//!    — the live price feed has disconnected for the day" Telegram. It fired at
//!    15:30 while ingest ran on to 15:40 — wrong in the REASSURING direction, so
//!    a genuine feed death at 15:32 would arrive AFTER the all-done card.
//!
//! Neither consumer could detect the drift on its own: each parses the string,
//! gets a valid `NaiveTime`, and proceeds. `validate()` only checks that the
//! value parses and that `order_cutoff_time < market_close_time` — both true at
//! 15:30. So nothing in the build had any opinion about WHICH time it was, and a
//! comment claiming otherwise would be a claim the build does not enforce.
//!
//! This guard is that opinion. It reads the shipped config rather than a
//! fixture, because the shipped config is what boots.

use chrono::{NaiveTime, Timelike};

/// Repo-root-relative read: `CARGO_MANIFEST_DIR` is `crates/app`.
fn base_toml() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config/base.toml")
        .canonicalize()
        .expect("config/base.toml must exist");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("config/base.toml must be readable: {e}"))
}

/// The FIRST `key = "value"` line at or after the `[section]` header line.
///
/// Anchored on the real header LINE, never a bare `find(section)` — a prose
/// comment can name the section earlier in the file (the
/// `cadence_boot_wiring_guard::section_enabled_line` lesson).
fn section_string_value(toml: &str, section: &str, key: &str) -> String {
    let mut lines = toml.lines();
    lines
        .by_ref()
        .find(|l| l.trim_start().starts_with(section))
        .unwrap_or_else(|| panic!("config/base.toml must carry the {section} section"));
    let prefix = format!("{key} =");
    let line = lines
        .take(60)
        .find(|l| l.trim_start().starts_with(&prefix))
        .unwrap_or_else(|| panic!("{section} must carry a {key} key"));
    line.split('"')
        .nth(1)
        .unwrap_or_else(|| panic!("{section}.{key} must be a quoted string: {line}"))
        .to_string()
}

fn secs_of_day(hhmmss: &str) -> u32 {
    NaiveTime::parse_from_str(hhmmss, "%H:%M:%S")
        .unwrap_or_else(|e| panic!("{hhmmss} must be HH:MM:SS: {e}"))
        .num_seconds_from_midnight()
}

/// THE pin named in `config/base.toml`'s own comment.
#[test]
fn config_close_time_matches_session_end() {
    let toml = base_toml();
    let close = section_string_value(&toml, "[trading]", "market_close_time");
    let close_secs = secs_of_day(&close);

    assert_eq!(
        close_secs,
        tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST,
        "config/base.toml [trading] market_close_time = {close} ({close_secs} s) \
         must equal TICK_PERSIST_END_SECS_OF_DAY_IST ({} s = 15:40:00 IST), the \
         session end the ingest gate ACTUALLY enforces. They drifted apart for \
         a month after the 2026-08-07 close move and put the daily partition \
         archive on top of ten minutes of live closing-auction capture. If the \
         session end genuinely moves, move BOTH in the same change.",
        tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST
    );
}

/// The archive window must open AFTER the last tick can land, never during
/// capture — the consequence the pin above exists to prevent, asserted directly
/// against the code that computes the window rather than against the constant.
#[test]
fn the_daily_archive_window_opens_after_the_last_tick_can_land() {
    let toml = base_toml();
    let close = section_string_value(&toml, "[trading]", "market_close_time");

    let window_open = tickvault_app::daily_archive_boot::window_open_secs_of_day(&close)
        .expect("the shipped market_close_time must schedule an archive window");

    assert!(
        window_open >= tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST,
        "the daily partition archive opens at {window_open} s but ingest runs \
         until {} s — the sweep would DROP partitions while live ticks are \
         still landing in them.",
        tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST
    );
}

/// The order cutoff must still precede the close — re-checked HERE because the
/// close just moved later, and a `validate()` invariant that held at 15:30 is
/// not evidence about 15:40.
#[test]
fn the_order_cutoff_still_precedes_the_close() {
    let toml = base_toml();
    let close = secs_of_day(&section_string_value(
        &toml,
        "[trading]",
        "market_close_time",
    ));
    let cutoff = secs_of_day(&section_string_value(
        &toml,
        "[trading]",
        "order_cutoff_time",
    ));

    assert!(
        cutoff < close,
        "order_cutoff_time ({cutoff} s) must precede market_close_time ({close} s)"
    );
}

/// Self-test: the extractor reads the real key, not a comment that names it.
#[test]
fn section_value_extractor_self_test() {
    let fixture = "\
# a prose comment naming [trading] and market_close_time = \"99:99:99\"
[other]
market_close_time = \"01:02:03\"

[trading]
market_open_time = \"09:00:00\"
# a comment inside the section, also naming market_close_time
market_close_time = \"15:40:00\"
order_cutoff_time = \"15:29:00\"
";
    assert_eq!(
        section_string_value(fixture, "[trading]", "market_close_time"),
        "15:40:00",
        "the extractor must anchor on the [trading] header LINE and skip the \
         comment that merely mentions the key"
    );
    assert_eq!(secs_of_day("15:40:00"), 56_400);
    assert_eq!(secs_of_day("00:00:01"), 1);
}
