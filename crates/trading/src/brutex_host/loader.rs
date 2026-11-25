//! Loads the newest approved version (and, for a dynamic strategy, the newest
//! usable parameter and universe snapshots) from a local copy of one strategy's
//! published folder, runs every check, and decides what is live.
//!
//! Folder layout (contract §3): `<root>/v<N>/` with `params/<date>/` inside it,
//! and `<root>/universe/<date>/`. Any refusal keeps the previous accepted
//! version and snapshots live, writes a `Refused` and a `KeptPrevious` audit
//! event, and sends one alert. Cold: runs once before the open.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use std::path::{Path, PathBuf};

use super::audit::{AlertSink, AuditEvent, AuditKind};
use super::check_files::{parse_candles, parse_signals, parse_values};
use super::engine::StrategyEngine;
use super::manifest::{VerifiedFolder, verify_folder};
use super::recipe::{
    CHECK_CANDLES, CHECK_SIGNALS, CHECK_VALUES, GATES_FILE, GatesFile, PARAMS_FILE, ParamSnapshot,
    RECIPE_FILE, Recipe, UNIVERSE_FILE, UniverseSnapshot, parse_json, parse_recipe,
};
use super::replay::{ReplayReport, replay};
use super::select::{pick_version, usable_snapshot_dates};
use super::{InstrumentKey, Refusal, gates};

/// What is live for one strategy after a load.
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    pub strategy: String,
    pub version: u32,
    pub recipe: Recipe,
    pub params: Option<(String, ParamSnapshot)>,
    pub universe_date: Option<String>,
    pub instruments: Vec<InstrumentKey>,
    pub replayed: ReplayReport,
}

/// The inputs a load needs from the running process.
pub struct LoadContext<'a> {
    /// The brutex commit tickvault was built against (the Cargo `rev`).
    pub built_commit: &'a str,
    /// Trading day `YYYY-MM-DD`; snapshots dated after it are ignored.
    pub trading_day: &'a str,
    /// Wall time for audit lines, `YYYY-MM-DDTHH:MM:SS+05:30`.
    pub at_ist: &'a str,
    /// Whether tickvault knows an instrument today.
    pub is_known: &'a dyn Fn(InstrumentKey) -> bool,
}

#[derive(Debug)]
pub struct LoadOutcome {
    /// What is live now: the new version if it passed, else the previous one.
    pub live: Option<Accepted>,
    /// True when `live` is newly accepted (not the previous one kept).
    pub changed: bool,
    pub events: Vec<AuditEvent>,
}

pub fn load<E: StrategyEngine + ?Sized>(
    root: &Path,
    ctx: &LoadContext<'_>,
    engine: &mut E,
    previous: Option<&Accepted>,
    alerts: &mut dyn AlertSink,
) -> LoadOutcome {
    let strategy = root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut events = Vec::new();
    let ev = |kind,
              version,
              params_date: Option<String>,
              universe_date: Option<String>,
              detail: String| AuditEvent {
        at_ist: ctx.at_ist.to_owned(),
        kind,
        strategy: strategy.clone(),
        version,
        params_date,
        universe_date,
        detail,
    };
    match try_load(root, ctx, engine, previous, &strategy) {
        Ok((acc, note)) => {
            let unchanged = previous == Some(&acc);
            events.push(ev(
                AuditKind::Loaded,
                Some(acc.version),
                acc.params.as_ref().map(|(d, _)| d.clone()),
                acc.universe_date.clone(),
                format!(
                    "{} bars, {} values, {} signals replayed bit for bit{note}",
                    acc.replayed.bars, acc.replayed.values_checked, acc.replayed.signals_checked
                ),
            ));
            LoadOutcome {
                live: Some(acc),
                changed: !unchanged,
                events,
            }
        }
        Err((version, what, refusal)) => {
            let reason = refusal.to_string();
            events.push(ev(
                AuditKind::Refused,
                version,
                None,
                None,
                format!("{what}: {reason}"),
            ));
            alerts.refused(&strategy, &what, &reason);
            if let Some(p) = previous {
                events.push(ev(
                    AuditKind::KeptPrevious,
                    Some(p.version),
                    p.params.as_ref().map(|(d, _)| d.clone()),
                    p.universe_date.clone(),
                    "previous version and snapshots stay live".to_owned(),
                ));
            }
            LoadOutcome {
                live: previous.cloned(),
                changed: false,
                events,
            }
        }
    }
}

type Failure = (Option<u32>, String, Refusal);

fn subdirs(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

struct CheckSet {
    candles: Vec<super::check_files::CheckBar>,
    values: super::check_files::CheckValues,
    signals: Vec<super::check_files::SignalRow>,
}

fn check_set(f: &VerifiedFolder) -> Result<CheckSet, Refusal> {
    Ok(CheckSet {
        candles: parse_candles(CHECK_CANDLES, f.file(CHECK_CANDLES)?)?,
        values: parse_values(CHECK_VALUES, f.file(CHECK_VALUES)?)?,
        signals: parse_signals(CHECK_SIGNALS, f.file(CHECK_SIGNALS)?)?,
    })
}

fn run_replay<E: StrategyEngine + ?Sized>(
    engine: &mut E,
    recipe: &Recipe,
    params: Option<&ParamSnapshot>,
    set: &CheckSet,
) -> Result<ReplayReport, Refusal> {
    // The check files name the instruments they cover; configure for exactly those.
    let mut keys: Vec<InstrumentKey> = set.candles.iter().map(|b| b.key).collect();
    keys.sort_unstable();
    keys.dedup();
    engine.configure(recipe, params, &keys)?;
    replay(
        engine,
        &recipe.timeframes,
        &set.candles,
        &set.values,
        &set.signals,
    )
}

fn try_load<E: StrategyEngine + ?Sized>(
    root: &Path,
    ctx: &LoadContext<'_>,
    engine: &mut E,
    previous: Option<&Accepted>,
    strategy: &str,
) -> Result<(Accepted, String), Failure> {
    let pick = pick_version(subdirs(root).iter().map(String::as_str));
    let Some(version) = pick.newest else {
        return Err((None, "version".to_owned(), Refusal::NoVersion));
    };
    let vdir: PathBuf = root.join(format!("v{version}"));
    let at_version = |what: &str, r: Refusal| (Some(version), what.to_owned(), r);

    let vfolder = verify_folder(&vdir).map_err(|r| at_version("version folder", r))?;
    let recipe = parse_recipe(
        vfolder
            .file(RECIPE_FILE)
            .map_err(|r| at_version("recipe", r))?,
        version,
        ctx.built_commit,
    )
    .map_err(|r| at_version("recipe", r))?;
    if recipe.name != strategy {
        return Err(at_version(
            "recipe",
            Refusal::BadRecipe(format!(
                "name {:?} does not match its folder {strategy:?}",
                recipe.name
            )),
        ));
    }
    let vchecks = check_set(&vfolder).map_err(|r| at_version("version check files", r))?;

    // The approved METHOD replays with the recipe's own settings.
    let mut replayed =
        run_replay(engine, &recipe, None, &vchecks).map_err(|r| at_version("version replay", r))?;

    let mut note = String::new();
    if !pick.gaps.is_empty() {
        note.push_str(&format!("; version gap: {:?} not published", pick.gaps));
    }
    if !pick.malformed.is_empty() {
        note.push_str(&format!("; ignored malformed folders {:?}", pick.malformed));
    }

    let (params, universe_date, instruments) = if recipe.dynamic {
        // UNIVERSE snapshot.
        let udir = root.join("universe");
        let (udates, ubad) =
            usable_snapshot_dates(subdirs(&udir).iter().map(String::as_str), ctx.trading_day);
        let Some(udate) = udates.first().cloned() else {
            return Err(at_version(
                "universe snapshot",
                Refusal::Gate("no universe snapshot on or before the trading day".to_owned()),
            ));
        };
        if !ubad.is_empty() {
            note.push_str(&format!("; ignored malformed universe folders {ubad:?}"));
        }
        let ufolder =
            verify_folder(&udir.join(&udate)).map_err(|r| at_version("universe snapshot", r))?;
        let usnap: UniverseSnapshot = parse_json(
            ufolder
                .file(UNIVERSE_FILE)
                .map_err(|r| at_version("universe snapshot", r))?,
            UNIVERSE_FILE,
        )
        .map_err(|r| at_version("universe snapshot", r))?;
        if usnap.date != udate {
            return Err(at_version(
                "universe snapshot",
                Refusal::Gate(format!(
                    "universe.json date {} is in folder {udate}",
                    usnap.date
                )),
            ));
        }
        gates::universe(&usnap, ctx.is_known).map_err(|r| at_version("universe snapshot", r))?;

        // PARAMETER snapshot.
        let pdir = vdir.join("params");
        let (pdates, pbad) =
            usable_snapshot_dates(subdirs(&pdir).iter().map(String::as_str), ctx.trading_day);
        let Some(pdate) = pdates.first().cloned() else {
            return Err(at_version(
                "parameter snapshot",
                Refusal::Gate("no parameter snapshot on or before the trading day".to_owned()),
            ));
        };
        if !pbad.is_empty() {
            note.push_str(&format!("; ignored malformed params folders {pbad:?}"));
        }
        let pfolder =
            verify_folder(&pdir.join(&pdate)).map_err(|r| at_version("parameter snapshot", r))?;
        let psnap: ParamSnapshot = parse_json(
            pfolder
                .file(PARAMS_FILE)
                .map_err(|r| at_version("parameter snapshot", r))?,
            PARAMS_FILE,
        )
        .map_err(|r| at_version("parameter snapshot", r))?;
        if psnap.date != pdate {
            return Err(at_version(
                "parameter snapshot",
                Refusal::Gate(format!(
                    "params.json date {} is in folder {pdate}",
                    psnap.date
                )),
            ));
        }
        let gfile: GatesFile = parse_json(
            pfolder
                .file(GATES_FILE)
                .map_err(|r| at_version("parameter snapshot", r))?,
            GATES_FILE,
        )
        .map_err(|r| at_version("parameter snapshot", r))?;
        gates::winner_gate(&gfile).map_err(|r| at_version("parameter snapshot", r))?;
        // Sanity against the previous snapshot: the live one if it is this
        // version, else the next older dated snapshot of this version.
        let prev_params: Option<ParamSnapshot> = match previous {
            Some(p) if p.version == version => p.params.as_ref().map(|(_, s)| s.clone()),
            _ => pdates.get(1).and_then(|d| {
                let f = verify_folder(&pdir.join(d)).ok()?;
                parse_json::<ParamSnapshot>(f.file(PARAMS_FILE).ok()?, PARAMS_FILE).ok()
            }),
        };
        gates::sanity(&psnap, prev_params.as_ref(), recipe.sanity_limits)
            .map_err(|r| at_version("parameter snapshot", r))?;
        let pchecks =
            check_set(&pfolder).map_err(|r| at_version("parameter snapshot check files", r))?;
        let r2 = run_replay(engine, &recipe, Some(&psnap), &pchecks)
            .map_err(|r| at_version("parameter snapshot replay", r))?;
        replayed.bars += r2.bars;
        replayed.values_checked += r2.values_checked;
        replayed.signals_checked += r2.signals_checked;
        (Some((pdate, psnap)), Some(udate), usnap.instruments)
    } else {
        for k in &recipe.instruments {
            if !(ctx.is_known)(*k) {
                return Err(at_version(
                    "recipe",
                    Refusal::Gate(format!(
                        "instrument {}/{} is not in today's instrument list",
                        k.security_id, k.exchange_segment
                    )),
                ));
            }
        }
        (None, None, recipe.instruments.clone())
    };

    // Configure for live with exactly what was checked.
    engine
        .configure(&recipe, params.as_ref().map(|(_, p)| p), &instruments)
        .map_err(|r| at_version("configure", r))?;
    Ok((
        Accepted {
            strategy: strategy.to_owned(),
            version,
            recipe,
            params,
            universe_date,
            instruments,
            replayed,
        },
        note,
    ))
}
// O(1) EXEMPT: end
