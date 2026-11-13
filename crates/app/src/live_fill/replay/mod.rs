//! `tv-live-fill-replay`: replays one recorded IST day through the live
//! paper fill chain, offline (plan item LF-8).
//!
//! ```text
//! raw WAL segments ──packet walk──▶ live candle fold ──verdict──▶ tap
//!  (local folder, or               (the drain's own   (the drain's    │
//!   raw-frames/<D>/ in the bucket,  live_fold)         fold_verdict)  ▼
//!   List + Get only)                          live paper fill chain (LF-1, LF-2)
//!                                             scripted TEST schedule, never a strategy
//!                                                     │
//!                       decisions.csv, events.csv, the LF-7 report, summary.txt
//! ```
//!
//! # Why raw segments and not the archived `ticks` CSV
//!
//! The `ticks` table has no best bid/ask, which the worst-case fill model
//! prices from, and no "new trade" flag; its receipts are microseconds and it
//! is archived days later. The raw WAL segment is the frame exactly as it came
//! off the socket, with its receipt to the nanosecond, so the replay walks it
//! with the live drain's own code.
//!
//! # Usage
//!
//! ```text
//! tv-live-fill-replay --day 2026-11-18 --wal-dir <dir> --schedule s.toml \
//!     --universe u.csv --out <new dir> [--staging <dir>] [--config config/base.toml]
//! tv-live-fill-replay --s3-date 2026-11-18 --staging <dir> ... [--max-download-gib 5] [--yes]
//! ```
//!
//! Other flags: `--contracts-artifact <dhan-contracts-<D>.json>` (lot sizes
//! cross-checked against the rows whose expiry is on or after the day: a
//! derivative whose lot differs, that the artifact does not hold, or that it
//! lists on two such rows with different lots, exits 2), `--boot-ist HH:MM:SS` (when the live process booted
//! that day; see "Boot" below), `--backup-history <file>` (the main-feed backup
//! publications, so a second copy is dropped as live dropped it),
//! `--compare <live decisions csv>` (writes `diff.csv`), `--wait-ms`,
//! `--open-wait-ms`, `--band-bps`, `--participation-bps`,
//! `--max-fill-seconds`, `--pass-us`, `--allow-partial`,
//! `--allow-non-trading-day`.
//!
//! The schedule is TOML, one `[[intent]]` per ask:
//!
//! ```text
//! [[intent]]
//! security_id = 2885
//! segment = "NSE_EQ"
//! frame_secs = 60
//! bar_open_ist = "09:20:00"   # or: every_bar = true
//! side = "BUY"                # or SELL
//! lots = 1
//! kind = "entry"              # or exit
//! ```
//!
//! The universe is CSV: `security_id,segment,tick_paise,lot_size,symbol`.
//!
//! A `bar_open_ist` must be on its frame's bar grid (bars are cut from IST
//! midnight) and inside the session (open at or after 09:15:00, end by
//! 15:30:00), and an `every_bar` frame must have at least one bar inside the
//! session; a rule no bar can match is refused rather than replayed to a
//! silent day with no orders.
//!
//! `--wal-dir` reads a local folder by the bucket's rule: the segments NAMED
//! on the day (IST), plus the newest one named on the previous date (only
//! that date, as the bucket lists only `raw-frames/<D-1>/`, so after a
//! weekend none); segments of other days are never read (the count left out
//! is printed). A day with no segment named on it is refused (exit 2) in
//! both sources, the bucket's before any download. A segment that is read
//! and held twice (plain and `.gz`) is refused (exit 2), and a
//! `*.wal.sha-<hex>.gz` second copy of a segment that is read (or the same
//! copy unzipped, `*.wal.sha-<hex>`) is refused as the bucket's is (exit 3);
//! a duplicate or sidecar of a segment of another day is ignored. A chosen
//! `.gz` segment with no `--staging` is refused (exit 2) before `--out` is
//! made or any segment is read.
//! `--staging` must be a new or empty folder: the tool removes only the files
//! it unzips or downloads there, and refuses to create one whose name exists.
//! `--compare` is read and checked before any segment is read. An empty path
//! for any path flag is refused (exit 2): `--out ""` would otherwise write
//! into the current folder.
//!
//! # Boot
//!
//! Live builds the chain at BOOT, before any socket is dialled, and hands the
//! fold over with a provisional capture start that its first live tick
//! confirms at its own receipt; the replay builds the chain at `--boot-ist`
//! (or the day's first frame when that came earlier) and hands the fold over
//! the same way, so the fold's opening rules read the day's first receipt,
//! as live's do. The WAL does not record the boot, so without `--boot-ist`
//! the scheduled 08:30 start is assumed, and a day whose first frame came
//! after 09:10 then reads incomplete: a boot after 09:15 changes the chain's
//! `chain_start` taint (the 09:10 line is conservative). A frame received
//! before a given `--boot-ist` (an earlier process's) reads incomplete too.
//!
//! Exit codes: 0 done; 2 bad input, no segment named on the day, no frame of
//! the day or records with no receipt (both never accepted by
//! `--allow-partial`), a partial day (the first frame after the 09:15 open,
//! the last before 15:30, a silence inside the session, no decision made, a
//! first frame after 09:10 with no `--boot-ist`, a frame before `--boot-ist`),
//! a non-trading day, a lot-size mismatch, or a segment held twice; 3 the
//! worst-case self-check failed (a
//! stored row worse than its model, or an order the LF-7 re-pricing from the
//! replay's own 1 s bars finds booked worse than the worst case; each printed
//! to stderr) or two different copies of a segment; 4 an I/O failure.
//!
//! # Safety
//!
//! PAPER ONLY and offline: nothing here places an order, calls a broker,
//! writes to a database or writes to (or deletes from) the bucket. A source
//! scan (`crates/app/tests/live_fill_replay_isolation_guard.rs`) fails the
//! build if any of that appears here, if any live module names this one, or
//! if `live_fill::select_engine` names the test schedule. The recipe it
//! builds has status `fixture`, which the approved-strategy loader never
//! accepts.
//!
//! # Honest limits
//!
//! See [`out::HONEST_LIMITS`], printed in every `summary.txt`.
//!
//! # Cost
//!
//! O(bytes) per segment read (and unzip for a `.gz`), only the day's segments
//! and the one before it (O(f log f) to list and order the f files of the
//! folder); memory about twice one segment, plus the fold sized once to
//! `AGGREGATOR_MAX_SLOTS` (about 68 MiB), plus the universe's 1 s bars,
//! O(universe instruments × seconds they traded), never pruned: about 80 B
//! per kept second (an 8 B key and a 32 B `SecondBar` in a `BTreeMap` whose
//! leaves, filled in ascending order, split 6/5 and sit about 55% full, plus
//! internal nodes and allocator overhead), and a liquid contract keeps about
//! 23,100 seconds from 09:15 to 15:40, so about 3.5 to 4 GB for 2,000 liquid
//! contracts over a full day (Assumed from the types, not measured),
//! plus the day's order rows, O(events); O(packets) per frame; the fold's
//! O(`TF_COUNT`) per tick; one chain pass per grid instant (about 24 million
//! at 1 ms for a day); release O(pending + k log k) at a boundary; the report
//! O(rows log rows). The summary's "largest segment read" is the unzipped
//! size, what was held.

// O(1) EXEMPT: begin: cold offline CLI, never on the live path; see the module docs for the cost.
pub mod drive;
pub mod out;
pub mod scripted;
pub mod source;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use tickvault_common::config::{ApplicationConfig, LiveFillConfig};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_storage::raw_frame_fetch::{
    DayListing, fetch_raw_frame_segment, file_sha256_hex, list_raw_frame_day,
};
use tickvault_storage::ws_frame_spill::SegmentRead;
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::fixture::FIXTURE_COMMIT;
use tickvault_trading::brutex_host::recipe::{RECIPE_SCHEMA, Recipe};

use crate::live_fill::chain::{ChainParams, ChainSetup, ContractSpec};
use drive::ReplayDriver;
use scripted::{parse_schedule, parse_segment};

/// The recipe name the replay's in-memory recipe carries.
pub const REPLAY_RECIPE_NAME: &str = "lf8-replay-fixture";
/// The replay recipe's status: never `approved`.
pub const REPLAY_RECIPE_STATUS: &str = "fixture";

/// Exit: done.
pub const EXIT_OK: i32 = 0;
/// Exit: bad input, partial day, non-trading day, no receipts, lot mismatch.
pub const EXIT_BAD_INPUT: i32 = 2;
/// Exit: the self-check failed, or two copies of a segment.
pub const EXIT_SELF_CHECK: i32 = 3;
/// Exit: an I/O failure.
pub const EXIT_IO: i32 = 4;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const DEFAULT_CONFIG_PATHS: [&str; 2] = ["config/base.toml", "../../config/base.toml"];

fn say(line: &str) {
    let mut o = std::io::stdout().lock();
    let _ok = writeln!(o, "{line}");
}

fn warn_line(line: &str) {
    let mut o = std::io::stderr().lock();
    let _ok = writeln!(o, "{line}");
}

/// The parsed command line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayArgs {
    pub day: Option<String>,
    pub wal_dir: Option<PathBuf>,
    pub s3_date: Option<String>,
    pub staging: Option<PathBuf>,
    pub max_download_gib: u64,
    pub yes: bool,
    pub schedule: Option<PathBuf>,
    pub universe: Option<PathBuf>,
    pub contracts_artifact: Option<PathBuf>,
    pub backup_history: Option<PathBuf>,
    pub compare: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub wait_ms: Option<u64>,
    pub open_wait_ms: Option<u64>,
    pub band_bps: Option<u32>,
    pub participation_bps: Option<u32>,
    pub max_fill_seconds: Option<u32>,
    pub pass_us: u64,
    /// `--boot-ist HH:MM:SS`: the live boot, seconds of the IST day.
    pub boot_sod: Option<i64>,
    pub allow_partial: bool,
    pub allow_non_trading_day: bool,
}

/// A numeric flag value. O(len).
fn num<T: std::str::FromStr>(flag: &str, v: &str) -> Result<T, String> {
    v.parse()
        .map_err(|_| format!("{flag} needs a number, got {v:?}"))
}

/// `--boot-ist HH:MM:SS` as seconds of the day. O(len).
///
/// # Errors
/// Not a `HH:MM:SS` time.
pub fn boot_sod(v: &str) -> Result<i64, String> {
    use chrono::Timelike as _;
    chrono::NaiveTime::parse_from_str(v, "%H:%M:%S")
        .map(|t| i64::from(t.num_seconds_from_midnight()))
        .map_err(|_| format!("--boot-ist needs HH:MM:SS, got {v:?}"))
}

/// Parses the flags after the program name. O(args).
///
/// # Errors
/// An unknown flag, a missing or bad value, or a missing required flag.
pub fn parse_args(args: &[String]) -> Result<ReplayArgs, String> {
    let mut a = ReplayArgs {
        max_download_gib: 5,
        pass_us: 1_000,
        ..ReplayArgs::default()
    };
    let mut it = args.iter().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--day" => a.day = Some(val()?),
            "--wal-dir" => a.wal_dir = Some(PathBuf::from(val()?)),
            "--s3-date" => a.s3_date = Some(val()?),
            "--staging" => a.staging = Some(PathBuf::from(val()?)),
            "--max-download-gib" => a.max_download_gib = num(flag, &val()?)?,
            "--yes" => a.yes = true,
            "--schedule" => a.schedule = Some(PathBuf::from(val()?)),
            "--universe" => a.universe = Some(PathBuf::from(val()?)),
            "--contracts-artifact" => a.contracts_artifact = Some(PathBuf::from(val()?)),
            "--backup-history" => a.backup_history = Some(PathBuf::from(val()?)),
            "--compare" => a.compare = Some(PathBuf::from(val()?)),
            "--out" => a.out = Some(PathBuf::from(val()?)),
            "--config" => a.config = Some(PathBuf::from(val()?)),
            "--wait-ms" => a.wait_ms = Some(num(flag, &val()?)?),
            "--open-wait-ms" => a.open_wait_ms = Some(num(flag, &val()?)?),
            "--band-bps" => a.band_bps = Some(num(flag, &val()?)?),
            "--participation-bps" => a.participation_bps = Some(num(flag, &val()?)?),
            "--max-fill-seconds" => a.max_fill_seconds = Some(num(flag, &val()?)?),
            "--pass-us" => a.pass_us = num(flag, &val()?)?,
            "--boot-ist" => a.boot_sod = Some(boot_sod(&val()?)?),
            "--allow-partial" => a.allow_partial = true,
            "--allow-non-trading-day" => a.allow_non_trading_day = true,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    match (&a.wal_dir, &a.s3_date) {
        (Some(_), Some(_)) | (None, None) => {
            return Err("give exactly one of --wal-dir <dir> or --s3-date <YYYY-MM-DD>".to_owned());
        }
        (None, Some(d)) => {
            if a.staging.is_none() {
                return Err("--s3-date needs --staging <dir>".to_owned());
            }
            if a.day.as_ref().is_some_and(|x| x != d) {
                return Err("--day and --s3-date name different days".to_owned());
            }
            a.day = Some(d.clone());
        }
        (Some(_), None) => {}
    }
    if a.day.is_none() {
        return Err("--day <YYYY-MM-DD> is required with --wal-dir".to_owned());
    }
    for (name, v) in [
        ("--schedule", a.schedule.is_none()),
        ("--universe", a.universe.is_none()),
        ("--out", a.out.is_none()),
    ] {
        if v {
            return Err(format!("{name} is required"));
        }
    }
    for (name, p) in [
        ("--wal-dir", &a.wal_dir),
        ("--staging", &a.staging),
        ("--schedule", &a.schedule),
        ("--universe", &a.universe),
        ("--contracts-artifact", &a.contracts_artifact),
        ("--backup-history", &a.backup_history),
        ("--compare", &a.compare),
        ("--out", &a.out),
        ("--config", &a.config),
    ] {
        // An empty path is the current folder to `create_dir_all` and
        // `join`, so `--out ""` would write over files there.
        if p.as_ref().is_some_and(|p| p.as_os_str().is_empty()) {
            return Err(format!("{name} needs a path, got an empty one"));
        }
    }
    if a.pass_us == 0 || a.pass_us > 1_000_000 {
        return Err("--pass-us must be in 1..=1000000".to_owned());
    }
    Ok(a)
}

/// Reads the universe CSV (`security_id,segment,tick_paise,lot_size,symbol`).
/// A repeated `(security_id, segment)` is refused (I-P1-11). O(rows).
///
/// # Errors
/// A bad header or row, or a repeated key.
pub fn parse_universe(
    text: &str,
) -> Result<(Vec<InstrumentKey>, HashMap<InstrumentKey, ContractSpec>), String> {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or("").trim();
    if header != "security_id,segment,tick_paise,lot_size,symbol" {
        return Err(
            "universe header must be security_id,segment,tick_paise,lot_size,symbol".to_owned(),
        );
    }
    let mut keys = Vec::new();
    let mut specs = HashMap::new();
    for (i, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = |m: &str| format!("universe line {}: {m}", i + 2);
        let cells: Vec<&str> = line.split(',').map(str::trim).collect();
        let [sid, seg, tick, lot, symbol] = cells.as_slice() else {
            return Err(bad("want 5 cells"));
        };
        let segment = parse_segment(seg).ok_or_else(|| bad("unknown segment"))?;
        let key = InstrumentKey {
            security_id: sid.parse().map_err(|_| bad("bad security_id"))?,
            exchange_segment: segment.binary_code(),
        };
        let tick_paise: i64 = tick.parse().map_err(|_| bad("bad tick_paise"))?;
        let lot_size: u32 = lot.parse().map_err(|_| bad("bad lot_size"))?;
        if tick_paise <= 0 || lot_size == 0 {
            return Err(bad("tick_paise and lot_size must be above 0"));
        }
        let spec = ContractSpec {
            tick_paise,
            lot_size,
            symbol: (*symbol).to_owned(),
        };
        if specs.insert(key, spec).is_some() {
            return Err(bad("the same (security_id, segment) is listed twice"));
        }
        keys.push(key);
    }
    if keys.is_empty() {
        return Err("universe has no rows".to_owned());
    }
    Ok((keys, specs))
}

/// The in-memory recipe of a replay: status `fixture`, never `approved`, the
/// fixture commit, the schedule's frames. O(frames).
#[must_use]
pub fn replay_recipe(frames_secs: &[u32], universe: &[InstrumentKey]) -> Recipe {
    Recipe {
        schema: RECIPE_SCHEMA.to_owned(),
        name: REPLAY_RECIPE_NAME.to_owned(),
        version: 1,
        status: REPLAY_RECIPE_STATUS.to_owned(),
        approved_by: String::new(),
        approved_at_ist: String::new(),
        brutex_commit: FIXTURE_COMMIT.to_owned(),
        dynamic: false,
        instruments: universe.to_vec(),
        timeframes: frames_secs.iter().map(|f| format!("{f}s")).collect(),
        settings: serde_json::Value::Object(serde_json::Map::new()),
        backtest: serde_json::Value::Object(serde_json::Map::new()),
        sanity_limits: tickvault_trading::brutex_host::recipe::SanityLimits::default(),
    }
}

/// Lot sizes in the universe against the day's contract artifact (a JSON
/// array of `dhan_contract_universe::ContractRow`). Only derivative keys are
/// checked (the artifact holds derivatives only), and only against the rows
/// whose expiry is on or after the replayed day (`day_ymd`, `YYYYMMDD`): the
/// day's file can list one security id on two contracts (an expired twin
/// beside the current one, the case `expiry_scope_check` reads as
/// ambiguous), and an expired row's lot is not the day's. A derivative key
/// with no such row whose lot is above 0 is a problem, and so is one listed
/// on two such rows with DIFFERENT lots (never a silent first-wins pick).
/// Returns the problems as text. O(rows + keys).
///
/// # Errors
/// The artifact could not be parsed.
pub fn lot_size_mismatches(
    artifact_json: &str,
    specs: &HashMap<InstrumentKey, ContractSpec>,
    day_ymd: u32,
) -> Result<Vec<String>, String> {
    let rows: Vec<crate::dhan_contract_universe::ContractRow> =
        serde_json::from_str(artifact_json).map_err(|e| format!("contracts artifact: {e}"))?;
    // Each key's distinct lots on live rows, in file order (one or two in
    // practice, so the scan is O(1) per row).
    let mut lots: HashMap<(u64, &str), Vec<u32>> = HashMap::new();
    for r in &rows {
        if r.z > 0 && r.e >= day_ymd {
            let seen = lots.entry((r.i, r.x.as_str())).or_default();
            if !seen.contains(&r.z) {
                seen.push(r.z);
            }
        }
    }
    let mut out = Vec::new();
    let mut keys: Vec<&InstrumentKey> = specs.keys().collect();
    keys.sort_unstable();
    for k in keys {
        let exch = match tickvault_common::types::ExchangeSegment::from_byte(k.exchange_segment)
            .map(|s| s.as_str())
        {
            Some("NSE_FNO") => "NSE",
            Some("BSE_FNO") => "BSE",
            _ => continue,
        };
        match lots.get(&(k.security_id, exch)).map(Vec::as_slice) {
            Some(&[z]) if z != specs[k].lot_size => out.push(format!(
                "{}: universe lot {} but the artifact says {z}",
                drive::key_label(k),
                specs[k].lot_size
            )),
            Some(&[_]) => {}
            Some(many) if many.len() > 1 => out.push(format!(
                "{}: the artifact lists it on rows with different lot sizes ({}) expiring on \
                 or after the day; refusing to pick one",
                drive::key_label(k),
                many.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            // Not checked is not a pass: a derivative the artifact does not
            // hold (another day's file, a mistyped id, an expired row only,
            // or a row with no lot) would run on the universe's lot
            // unchecked.
            _ => out.push(format!(
                "{}: not in the artifact (no row with a lot size above 0 expiring on or after \
                 the day)",
                drive::key_label(k)
            )),
        }
    }
    Ok(out)
}

fn read_text(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", p.display()))
}

fn load_config(path: Option<&Path>) -> Result<ApplicationConfig, String> {
    use figment::providers::{Format, Toml};
    let chosen = match path {
        Some(p) => p.to_path_buf(),
        None => DEFAULT_CONFIG_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
            .ok_or("no config/base.toml here: pass --config <path>")?,
    };
    figment::Figment::new()
        .merge(Toml::file(&chosen))
        .extract()
        .map_err(|e| format!("config {}: {e}", chosen.display()))
}

/// The chain's tuning: `[live_fill]` from the config with the command-line
/// overrides, bounds-checked like the boot checks them. O(1).
///
/// # Errors
/// A value out of range.
pub fn chain_params(base: &LiveFillConfig, a: &ReplayArgs) -> Result<ChainParams, String> {
    let mut c = *base;
    if let Some(v) = a.wait_ms {
        c.wait_ms = v;
    }
    if let Some(v) = a.open_wait_ms {
        c.open_wait_ms = v;
    }
    if let Some(v) = a.band_bps {
        c.band_bps = v;
    }
    if let Some(v) = a.participation_bps {
        c.participation_bps = v;
    }
    if let Some(v) = a.max_fill_seconds {
        c.max_fill_seconds = v;
    }
    c.validate().map_err(|e| e.to_string())?;
    Ok(ChainParams::from(&c))
}

/// Runs the tool. `args[0]` is the program name. Returns the exit code (see
/// the module docs). Cold.
// TEST-EXEMPT: covered by live_fill_replay_golden.rs (lf8_golden_day_matches, lf8_bin_runs_and_exits_zero) and replay_tests::test_run_cli_* (every refusal path).
pub fn run_cli(args: &[String]) -> i32 {
    let a = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            warn_line(&format!("tv-live-fill-replay: {e}"));
            return EXIT_BAD_INPUT;
        }
    };
    match run(&a) {
        Ok(code) => code,
        Err((code, msg)) => {
            warn_line(&format!("tv-live-fill-replay: {msg}"));
            code
        }
    }
}

type Failure = (i32, String);

fn bad(m: impl Into<String>) -> Failure {
    (EXIT_BAD_INPUT, m.into())
}

fn io(m: impl Into<String>) -> Failure {
    (EXIT_IO, m.into())
}

fn run(a: &ReplayArgs) -> Result<i32, Failure> {
    let started = std::time::Instant::now();
    let day_text = a.day.clone().unwrap_or_default();
    let day = chrono::NaiveDate::parse_from_str(&day_text, "%Y-%m-%d")
        .map_err(|_| bad(format!("bad day {day_text:?}, want YYYY-MM-DD")))?;
    let out_dir = a.out.clone().unwrap_or_default();
    if out_dir.exists()
        && std::fs::read_dir(&out_dir)
            .map_err(|e| io(format!("list {}: {e}", out_dir.display())))?
            .next()
            .is_some()
    {
        return Err(bad(format!(
            "{} is not empty: give a new folder",
            out_dir.display()
        )));
    }
    let config = load_config(a.config.as_deref()).map_err(bad)?;
    let calendar = TradingCalendar::from_config(&config.trading).map_err(|e| bad(e.to_string()))?;
    if !calendar.is_trading_day(day) && !a.allow_non_trading_day {
        return Err(bad(format!(
            "{day_text} is not a trading day (pass --allow-non-trading-day to replay it anyway)"
        )));
    }
    let params = chain_params(&config.live_fill, a).map_err(bad)?;
    // Read and checked BEFORE any segment is read or downloaded, so a bad
    // path costs nothing and leaves `--out` untouched.
    let compare_text = match &a.compare {
        None => None,
        Some(p) => {
            let text = read_text(p).map_err(bad)?;
            out::compare_csv(&text, &[]).map_err(bad)?;
            Some(text)
        }
    };
    if let Some(st) = &a.staging {
        check_staging(st)?;
    }
    let schedule_text = read_text(a.schedule.as_deref().unwrap_or(Path::new(""))).map_err(bad)?;
    let (frames, engine) = parse_schedule(&schedule_text).map_err(bad)?;
    let universe_text = read_text(a.universe.as_deref().unwrap_or(Path::new(""))).map_err(bad)?;
    let (universe, contracts) = parse_universe(&universe_text).map_err(bad)?;
    if let Some(p) = &a.contracts_artifact {
        // `YYYYMMDD` of the day (`day_text` parsed as YYYY-MM-DD above).
        let day_ymd: u32 = day.format("%Y%m%d").to_string().parse().unwrap_or(0);
        let m =
            lot_size_mismatches(&read_text(p).map_err(bad)?, &contracts, day_ymd).map_err(bad)?;
        if !m.is_empty() {
            return Err(bad(format!(
                "lot sizes do not check against the artifact: {}",
                m.join("; ")
            )));
        }
    }
    let backup = match &a.backup_history {
        None => None,
        Some(p) => crate::main_feed_backup::read_backup_history(p)
            .map_err(|e| bad(format!("backup history {}: {e}", p.display())))?
            .and_then(|h| crate::main_feed_backup::ReplayBackup::new(&h)),
    };
    let setup = ChainSetup {
        recipe: replay_recipe(&frames, &universe),
        snapshot: None,
        engine_frames_secs: frames.clone(),
        universe,
        contracts,
    };
    let day_start_ist_ns =
        crate::live_fill_report::build::rows_day_start(&day_text).ok_or_else(|| bad("bad day"))?;
    let pass_ns = i64::try_from(a.pass_us)
        .unwrap_or(1_000)
        .saturating_mul(1_000);
    let boot = a.boot_sod.map_or_else(
        || drive::BootInstant::scheduled(day_start_ist_ns),
        |sod| drive::BootInstant::given(day_start_ist_ns, sod),
    );
    let mut driver = ReplayDriver::new(
        engine,
        setup,
        params,
        day_start_ist_ns,
        pass_ns,
        backup,
        boot,
    )
    .map_err(|e| bad(format!("the chain refused the setup: {e}")))?;
    // The local folder is judged before `--out` is made or any segment is
    // read, so a bad folder (no segment named on the day, a segment held
    // twice, a second copy, a `.gz` with no `--staging`) costs nothing.
    let local = match &a.wal_dir {
        Some(dir) => Some(local_day_inputs(dir, day, a.staging.as_deref())?),
        None => None,
    };
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| io(format!("create {}: {e}", out_dir.display())))?;
    let largest = if let (Some(dir), Some(inputs)) = (&a.wal_dir, local) {
        replay_local(&mut driver, dir, inputs, a.staging.as_deref())?
    } else {
        replay_bucket(&mut driver, a)?
    };
    let output = driver
        .finish()
        .map_err(|e| bad(format!("the chain stopped: {e}")))?;
    write_outputs(
        a,
        &OutputInputs {
            day_text,
            day,
            compare_text,
            largest,
            started,
        },
        &output,
    )
}

/// Feeds one read segment and marks its losses where they happened. O(frames).
///
/// The replay reads with no applied watermark, so a frame's `after_gap` can
/// only mean the reader resynced past damaged bytes just before it, the
/// segment's first frame included (damage at its head). A walk that STOPPED
/// on a bad record, or a segment unreadable whole, lost everything after its
/// last returned frame, so the loss is marked after the segment. A segment
/// whose damage was all resynced past is counted, never bumped twice.
fn feed_segment(driver: &mut ReplayDriver, read: &SegmentRead) {
    driver.note_segment();
    for f in &read.frames {
        if f.after_gap {
            driver.note_frames_skipped();
        }
        driver.feed_frame(f);
    }
    if read.damaged || read.trailing_gap {
        driver.note_segment_damage();
    }
    if read.stopped_early || read.trailing_gap {
        driver.note_frames_skipped();
    }
}

/// The segments a local replay of `day` reads from `dir`, refused (before
/// anything is read or written) when the folder cannot hold the day: no
/// segment named on it (exit 2), a chosen segment held twice (exit 2), a
/// second copy of one (exit 3), or a chosen `.gz` with no `--staging` to
/// unzip it into (exit 2, bad input, not an I/O failure). O(files log files).
fn local_day_inputs(
    dir: &Path,
    day: chrono::NaiveDate,
    staging: Option<&Path>,
) -> Result<source::DayInputs, Failure> {
    let refused = |e: source::InputsError| match e {
        source::InputsError::Io(_) => io(e.to_string()),
        source::InputsError::Duplicate(_) => bad(e.to_string()),
        source::InputsError::Sidecars(_) => (EXIT_SELF_CHECK, e.to_string()),
    };
    let folder = source::list_wal_inputs(dir).map_err(refused)?;
    let inputs = source::select_day_inputs(folder, day).map_err(refused)?;
    // Only a segment NAMED on the day counts: the previous date's segment
    // alone holds at most the day's first frames, and replaying it alone
    // would tape no frame of the day and read as a quiet day.
    if inputs.day_segments == 0 {
        return Err(bad(format!(
            "no ws-frames-*.wal segment named on {day} in {}",
            dir.display()
        )));
    }
    if staging.is_none()
        && let Some(gz) = inputs
            .paths
            .iter()
            .find(|p| p.extension().is_some_and(|x| x == "gz"))
    {
        return Err(bad(format!(
            "{} is gzipped: pass --staging <dir> to unzip it into",
            gz.display()
        )));
    }
    Ok(inputs)
}

fn replay_local(
    driver: &mut ReplayDriver,
    dir: &Path,
    inputs: source::DayInputs,
    staging: Option<&Path>,
) -> Result<u64, Failure> {
    let source::DayInputs {
        paths: inputs,
        left_out,
        ..
    } = inputs;
    if left_out > 0 {
        say(&format!(
            "tv-live-fill-replay: {left_out} segments of other days in {} are not read",
            dir.display()
        ));
    }
    if let Some(st) = staging {
        std::fs::create_dir_all(st).map_err(|e| io(format!("create {}: {e}", st.display())))?;
    }
    let mut largest = 0u64;
    for p in inputs {
        let (read, bytes) = source::read_main_feed_segment(&p, staging)
            .map_err(|e| io(format!("{}: {e}", p.display())))?;
        largest = largest.max(bytes);
        feed_segment(driver, &read);
    }
    Ok(largest)
}

/// `--staging` must be a new or empty folder: the tool removes the files it
/// unzips or downloads there, and must never overwrite or remove a file it
/// did not make (an operator's copy, or a live WAL folder's segment). O(1).
fn check_staging(staging: &Path) -> Result<(), Failure> {
    if staging.exists() {
        let mut rd = std::fs::read_dir(staging)
            .map_err(|e| io(format!("list {}: {e}", staging.display())))?;
        if rd.next().is_some() {
            return Err(bad(format!(
                "--staging {} is not empty: give a new or empty folder",
                staging.display()
            )));
        }
    }
    Ok(())
}

/// Whether a bucket day may be downloaded, judged on its listing before any
/// download, in this order: a `.sha-` second copy of a segment it reads
/// (exit 3, refusing to pick one), no segment named on the day (exit 2: the
/// previous date's segment alone holds at most the day's first frames, the
/// day may not be uploaded yet as the uploader runs after 15:40 IST, so
/// fetching it alone costs money and tapes no frame of the day), then the
/// download cap (exit 2 when the listing is over `max_download_gib` GiB and
/// `yes` is not given: data transfer, and a retrieval fee for frames older
/// than 30 days, cost money). Exactly at the cap goes ahead. Pure,
/// O(sidecars).
fn bucket_go_ahead(
    listing: &DayListing,
    date: &str,
    max_download_gib: u64,
    yes: bool,
) -> Result<(), Failure> {
    if !listing.sidecars.is_empty() {
        return Err((
            EXIT_SELF_CHECK,
            format!(
                "the bucket holds a second, different copy of a segment ({}); refusing to pick one",
                listing.sidecars.join(", ")
            ),
        ));
    }
    if listing.day_segments == 0 {
        return Err(bad(format!("no segments under raw-frames/{date}/")));
    }
    let cap = max_download_gib.saturating_mul(1024 * 1024 * 1024);
    if listing.total_bytes > cap && !yes {
        return Err(bad(format!(
            "{} bytes is over --max-download-gib {max_download_gib} (data transfer costs money); \
             pass --yes to go ahead",
            listing.total_bytes
        )));
    }
    Ok(())
}

fn replay_bucket(driver: &mut ReplayDriver, a: &ReplayArgs) -> Result<u64, Failure> {
    let date = a.s3_date.clone().unwrap_or_default();
    let staging = a.staging.clone().unwrap_or_default();
    std::fs::create_dir_all(&staging)
        .map_err(|e| io(format!("create {}: {e}", staging.display())))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| io(e.to_string()))?;
    rt.block_on(async {
        let s3 = tickvault_storage::s3_cold::S3Cold::load("")
            .await
            .ok_or_else(|| bad("no cold bucket resolves (set TV_ENVIRONMENT)"))?;
        let listing = list_raw_frame_day(&s3, &date)
            .await
            .map_err(|e| io(format!("list: {e}")))?;
        say(&format!(
            "tv-live-fill-replay: {} segments, {} bytes to download",
            listing.keys.len(),
            listing.total_bytes
        ));
        bucket_go_ahead(&listing, &date, a.max_download_gib, a.yes)?;
        let mut largest = 0u64;
        for (key, _) in &listing.keys {
            let wal = fetch_raw_frame_segment(&s3, key, &staging)
                .await
                .map_err(|e| io(format!("{key}: {e}")))?;
            let read = source::read_main_feed_segment(&wal, None)
                .map_err(|e| io(format!("{}: {e}", wal.display())));
            let removed = std::fs::remove_file(&wal);
            let (read, bytes) = read?;
            removed.map_err(|e| io(format!("remove {}: {e}", wal.display())))?;
            largest = largest.max(bytes);
            feed_segment(driver, &read);
        }
        Ok(largest)
    })
}

fn write_file(dir: &Path, name: &str, body: &[u8]) -> Result<(), Failure> {
    std::fs::write(dir.join(name), body).map_err(|e| io(format!("write {name}: {e}")))
}

/// An in-session silence longer than this is named as a possible process
/// restart in the incomplete reason: a live boot (auth, database checks,
/// socket dials) takes longer than this, and the replay does not reproduce a
/// restart (see the honest limits).
pub const RESTART_HINT_GAP_NS: i64 = 30 * NANOS_PER_SEC;

/// Why the day reads incomplete, from the replay's counts: no frame of the
/// day, the first frame after the open (the head of the day is missing), a
/// first frame after 09:10 with no `--boot-ist` (the boot, which three live
/// decisions read, is unknown), a frame before a given `--boot-ist`, the
/// last frame before 15:30, a silence inside the session (one over
/// [`RESTART_HINT_GAP_NS`] named as a possible restart), records with no
/// receipt, or no decision at all (`decisions` is 0: the schedule's
/// instruments never closed a scheduled bar, so the run proves nothing).
/// Empty when the day is complete. O(1).
#[must_use]
pub fn incomplete_reasons(day_text: &str, s: &drive::ReplayStats, decisions: usize) -> Vec<String> {
    let mut incomplete = Vec::new();
    let day_start = crate::live_fill_report::build::rows_day_start(day_text).unwrap_or(0);
    let open_ns = day_start + crate::live_fill::timing::SESSION_OPEN_SOD * NANOS_PER_SEC;
    let close_ns = day_start + crate::live_fill::timing::SESSION_CLOSE_SOD * NANOS_PER_SEC;
    match s.first_receipt_ist_ns {
        None => incomplete.push("no frame of the day".to_owned()),
        Some(first) if first > open_ns.saturating_add(drive::INCOMPLETE_GAP_NS) => {
            incomplete.push(format!(
                "the first frame came at {}, after the 09:15 open",
                out::ist_text(first)
            ));
        }
        Some(_) => {}
    }
    let boot_matters_ns = day_start + drive::BOOT_MATTERS_AFTER_SOD * NANOS_PER_SEC;
    if s.boot_assumed
        && let Some(first) = s.first_receipt_ist_ns
        && first > boot_matters_ns
    {
        incomplete.push(format!(
            "the first frame came at {}, after 09:10, and the WAL does not record when the \
             live process booted: the replay assumed the scheduled 08:30 boot, and a later \
             boot changes the opening bars (pass --boot-ist HH:MM:SS)",
            out::ist_text(first)
        ));
    }
    if s.frames_before_boot > 0 {
        incomplete.push(format!(
            "{} frames came before --boot-ist (an earlier process's frames, which the replay \
             does not reproduce; it started at the first of them)",
            s.frames_before_boot
        ));
    }
    if let Some(last) = s.last_receipt_ist_ns
        && last < close_ns
    {
        incomplete.push(format!(
            "the last frame came at {}, before 15:30",
            out::ist_text(last)
        ));
    }
    if s.max_session_gap_ns > drive::INCOMPLETE_GAP_NS {
        let restart = if s.max_session_gap_ns > RESTART_HINT_GAP_NS {
            " (a silence this long can be a process restart, which the replay does not \
             reproduce: live started a new chain, fold and flat paper position at the boot)"
        } else {
            ""
        };
        incomplete.push(format!(
            "a {} ms silence inside the session{restart}",
            out::ms_text(s.max_session_gap_ns)
        ));
    }
    if s.no_receipt_frames > 0 {
        incomplete.push(format!(
            "{} frames had no receipt time",
            s.no_receipt_frames
        ));
    }
    if decisions == 0 {
        incomplete.push(
            "no decision was made (the schedule's instruments never closed a scheduled bar)"
                .to_owned(),
        );
    }
    incomplete
}

/// How many orders fail the worst-case self-check: a stored row marked worse
/// than its model, plus every order the report re-priced from the replay's
/// own 1 s bars and found booked worse than the worst case (the LF-7
/// re-pricing, `worse_than_worst` and `stored_worse`). Any above 0 exits 3.
/// O(rows).
#[must_use]
pub fn self_check_failures(
    report: &crate::live_fill_report::build::DailyReport,
    events: &[tickvault_storage::live_order_events_persistence::LiveOrderEvent],
) -> u64 {
    let rows = u64::try_from(out::worse_than_worst_rows(events).len()).unwrap_or(u64::MAX);
    rows.saturating_add(report.counts.worse_than_worst)
        .saturating_add(report.counts.stored_worse)
}

/// The tool's exit code once every file is written: 3 when the self-check
/// fails, 2 for an incomplete day (records with no receipt and no frame of
/// the day always, the rest unless `--allow-partial`), else 0. O(1).
#[must_use]
pub fn final_exit_code(
    self_check_failures: u64,
    no_receipt_frames: u64,
    no_frame_of_day: bool,
    incomplete: bool,
    allow_partial: bool,
) -> i32 {
    if self_check_failures > 0 {
        return EXIT_SELF_CHECK;
    }
    if no_receipt_frames > 0 || no_frame_of_day || (incomplete && !allow_partial) {
        return EXIT_BAD_INPUT;
    }
    EXIT_OK
}

fn write_outputs(
    a: &ReplayArgs,
    inputs: &OutputInputs,
    output: &drive::ReplayOutput,
) -> Result<i32, Failure> {
    let dir = a.out.clone().unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    write_file(&dir, "events.csv", &out::events_csv(&output.events))?;
    names.push("events.csv".to_owned());
    write_file(
        &dir,
        "decisions.csv",
        out::decisions_csv(&output.decisions).as_bytes(),
    )?;
    names.push("decisions.csv".to_owned());
    let s = &output.stats;
    // Judged before the report is built: an incomplete day is not settled,
    // so the LF-7 report files read partial, never complete (they are read
    // and shared without summary.txt).
    let incomplete = incomplete_reasons(&inputs.day_text, s, output.decisions.len());
    let report = out::day_report(&inputs.day_text, output, &incomplete);
    for (name, body) in out::report_files(&report) {
        write_file(&dir, &name, body.as_bytes())?;
        names.push(name);
    }
    if let Some(live) = &inputs.compare_text {
        let diff = out::compare_csv(live, &output.decisions).map_err(bad)?;
        write_file(&dir, "diff.csv", diff.as_bytes())?;
        names.push("diff.csv".to_owned());
    }
    let mut file_hashes = Vec::new();
    for n in &names {
        let h = file_sha256_hex(&dir.join(n)).map_err(|e| io(e.to_string()))?;
        file_hashes.push((n.clone(), h));
    }
    let summary = out::summary_text(
        output,
        &report,
        &out::SummaryInputs {
            day: inputs.day_text.clone(),
            weekday: inputs.day.format("%a").to_string(),
            incomplete_reasons: incomplete.clone(),
            largest_segment_bytes: inputs.largest,
            file_hashes,
        },
    );
    write_file(&dir, "summary.txt", summary.as_bytes())?;
    say(&summary);
    say(&format!(
        "tv-live-fill-replay: finished in {:.1} s; files in {}",
        inputs.started.elapsed().as_secs_f64(),
        dir.display()
    ));
    let failures = self_check_failures(&report, &output.events);
    if failures > 0 {
        for e in out::worse_than_worst_rows(&output.events) {
            warn_line(&format!("tv-live-fill-replay: WORSE THAN THE MODEL: {e:?}"));
        }
        for o in report
            .orders
            .iter()
            .filter(|o| o.verdict == "worse_than_worst" || o.verdict == "stored_worse")
        {
            warn_line(&format!(
                "tv-live-fill-replay: BOOKED WORSE THAN THE WORST CASE: {o:?}"
            ));
        }
    }
    let no_frame_of_day = s.first_receipt_ist_ns.is_none();
    let code = final_exit_code(
        failures,
        s.no_receipt_frames,
        no_frame_of_day,
        !incomplete.is_empty(),
        a.allow_partial,
    );
    if code == EXIT_BAD_INPUT {
        if no_frame_of_day || s.no_receipt_frames > 0 {
            warn_line(
                "tv-live-fill-replay: no frame of the day, or records with no receipt: \
                 --allow-partial does not accept that",
            );
        } else {
            warn_line(
                "tv-live-fill-replay: the day is incomplete (pass --allow-partial to accept it)",
            );
        }
    }
    Ok(code)
}

/// What [`write_outputs`] needs beyond the arguments and the replay's output.
struct OutputInputs {
    day_text: String,
    day: chrono::NaiveDate,
    /// The `--compare` file, read and checked before the replay ran.
    compare_text: Option<String>,
    largest: u64,
    started: std::time::Instant,
}
// O(1) EXEMPT: end

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
