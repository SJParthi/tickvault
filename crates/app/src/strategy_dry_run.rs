//! `tickvault strategy-dry-run` (rule `brutex-strategy-host-2026-10-29.md`):
//! loads published brutex strategy folders, verifies manifests, runs the gates
//! and the bit-for-bit replay, applies the pre-open swap rule, and prints every
//! audit line. It places no order and changes nothing live. Until brutex's crate
//! is linked, the engine is the test double, so only fixtures replay cleanly.
//!
//! ```text
//! tickvault strategy-dry-run --make-fixture <dir>      write the demo fixture
//! tickvault strategy-dry-run --dir <strategy-folder>... load local folders
//! tickvault strategy-dry-run --s3 <name> --staging <dir>  read-only download, then load
//! ```
//! Optional: `--trading-day YYYY-MM-DD`, `--secs-of-day N`, `--non-trading-day`,
//! `--commit <sha>`, `--audit <file.ndjson>`.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use std::io::Write as _;
use std::path::PathBuf;

use tickvault_trading::brutex_host::audit::{AuditEvent, CollectAlerts, append};
use tickvault_trading::brutex_host::engine::ReferenceDouble;
use tickvault_trading::brutex_host::fixture::{FIXTURE_COMMIT, write_demo_fixture};
use tickvault_trading::brutex_host::loader::{LoadContext, load};
use tickvault_trading::brutex_host::swap::apply;

pub const STRATEGY_DRY_RUN_SUBCOMMAND: &str = "strategy-dry-run";

fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").unwrap_or_default();
}

#[derive(Debug, Default, PartialEq)]
struct Args {
    make_fixture: Option<PathBuf>,
    dirs: Vec<PathBuf>,
    s3: Option<String>,
    staging: Option<PathBuf>,
    trading_day: String,
    secs_of_day: u32,
    trading: bool,
    commit: String,
    audit: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        trading_day: "2026-10-30".to_owned(),
        secs_of_day: 8 * 3600 + 40 * 60,
        trading: true,
        commit: FIXTURE_COMMIT.to_owned(),
        ..Args::default()
    };
    let mut it = args.iter().skip(2);
    while let Some(flag) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--make-fixture" => a.make_fixture = Some(PathBuf::from(val()?)),
            "--dir" => a.dirs.push(PathBuf::from(val()?)),
            "--s3" => a.s3 = Some(val()?),
            "--staging" => a.staging = Some(PathBuf::from(val()?)),
            "--trading-day" => a.trading_day = val()?,
            "--secs-of-day" => {
                a.secs_of_day = val()?
                    .parse()
                    .map_err(|_| "--secs-of-day needs a number".to_owned())?
            }
            "--non-trading-day" => a.trading = false,
            "--commit" => a.commit = val()?,
            "--audit" => a.audit = Some(PathBuf::from(val()?)),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if a.make_fixture.is_none() && a.dirs.is_empty() && a.s3.is_none() {
        return Err("nothing to do: pass --make-fixture, --dir or --s3".to_owned());
    }
    if a.s3.is_some() && a.staging.is_none() {
        return Err("--s3 needs --staging <dir>".to_owned());
    }
    Ok(a)
}

/// Dispatch. `None` when `args` is not this subcommand.
pub fn run_cli(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some(STRATEGY_DRY_RUN_SUBCOMMAND) {
        return None;
    }
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            emit(&format!("strategy-dry-run: {e}"));
            return Some(2);
        }
    };
    Some(run(a))
}

fn run(mut a: Args) -> i32 {
    if let Some(dir) = &a.make_fixture {
        match write_demo_fixture(dir) {
            Ok(paths) => {
                for p in paths {
                    emit(&format!("strategy-dry-run: wrote fixture {}", p.display()));
                }
            }
            Err(e) => {
                emit(&format!(
                    "strategy-dry-run: could not write the fixture: {e}"
                ));
                return 1;
            }
        }
    }
    if let (Some(name), Some(staging)) = (a.s3.clone(), a.staging.clone()) {
        match fetch(&name, &a.trading_day, &staging) {
            Ok(dir) => a.dirs.push(dir),
            Err(e) => {
                emit(&format!(
                    "strategy-dry-run: read-only download of {name} failed: {e:#}"
                ));
                return 1;
            }
        }
    }
    let at_ist = format!(
        "{}T{:02}:{:02}:{:02}+05:30",
        a.trading_day,
        a.secs_of_day / 3600,
        a.secs_of_day / 60 % 60,
        a.secs_of_day % 60
    );
    let known = |_: tickvault_trading::brutex_host::InstrumentKey| true;
    let ctx = LoadContext {
        built_commit: &a.commit,
        trading_day: &a.trading_day,
        at_ist: &at_ist,
        is_known: &known,
    };
    let mut refused = 0usize;
    let mut all: Vec<AuditEvent> = Vec::new();
    for dir in &a.dirs {
        let mut engine = ReferenceDouble::new();
        let mut alerts = CollectAlerts::default();
        let outcome = load(dir, &ctx, &mut engine, None, &mut alerts);
        let (live, events) = apply(outcome, None, a.trading, a.secs_of_day, &at_ist);
        for e in &events {
            emit(&e.to_line());
        }
        refused += alerts.alerts.len();
        emit(&format!(
            "strategy-dry-run: {} -> {}",
            dir.display(),
            live.map_or_else(
                || "nothing live".to_owned(),
                |l| format!(
                    "v{} live ({} instruments, {} bars replayed)",
                    l.version,
                    l.instruments.len(),
                    l.replayed.bars
                )
            )
        ));
        all.extend(events);
    }
    if let Some(path) = &a.audit
        && let Err(e) = append(path, &all)
    {
        emit(&format!(
            "strategy-dry-run: audit write to {} failed: {e}",
            path.display()
        ));
        return 1;
    }
    i32::from(refused > 0)
}

fn fetch(name: &str, trading_day: &str, staging: &std::path::Path) -> anyhow::Result<PathBuf> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let s3 = tickvault_storage::s3_cold::S3Cold::load("")
            .await
            .ok_or_else(|| anyhow::anyhow!("no cold bucket resolves (set TV_ENVIRONMENT)"))?;
        let (dir, plan) = tickvault_storage::strategy_recipe_fetch::fetch_strategy(
            &s3,
            name,
            trading_day,
            staging,
        )
        .await?;
        emit(&format!(
            "strategy-dry-run: downloaded {} files ({} bytes) of v{:?}",
            plan.files.len(),
            plan.total_bytes,
            plan.version
        ));
        Ok(dir)
    })
}

// O(1) EXEMPT: end
#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        std::iter::once("tickvault")
            .chain(v.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    fn temp(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tv-dry-run-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn test_run_cli_not_this_subcommand_returns_none() {
        assert_eq!(run_cli(&args(&["host-tuning", "verify"])), None);
        assert_eq!(run_cli(&args(&[])), None);
    }

    #[test]
    fn test_bad_flags_exit_two() {
        assert_eq!(run_cli(&args(&["strategy-dry-run"])), Some(2));
        assert_eq!(run_cli(&args(&["strategy-dry-run", "--bogus"])), Some(2));
        assert_eq!(run_cli(&args(&["strategy-dry-run", "--dir"])), Some(2));
        assert_eq!(run_cli(&args(&["strategy-dry-run", "--s3", "x"])), Some(2));
        assert_eq!(
            run_cli(&args(&[
                "strategy-dry-run",
                "--dir",
                "x",
                "--secs-of-day",
                "noon"
            ])),
            Some(2)
        );
    }

    #[test]
    fn test_dry_run_on_fixture_accepts() {
        let d = temp("ok");
        let audit = d.join("audit.ndjson");
        let code = run_cli(&args(&[
            "strategy-dry-run",
            "--make-fixture",
            d.to_str().unwrap(),
            "--dir",
            d.join("demo_fixed").to_str().unwrap(),
            "--dir",
            d.join("demo_dynamic").to_str().unwrap(),
            "--audit",
            audit.to_str().unwrap(),
        ]));
        assert_eq!(code, Some(0));
        let text = std::fs::read_to_string(&audit).unwrap();
        assert_eq!(text.matches("\"kind\":\"loaded\"").count(), 2, "{text}");
        assert_eq!(text.matches("\"kind\":\"swapped\"").count(), 2, "{text}");
    }

    #[test]
    fn test_dry_run_on_tampered_fixture_refuses() {
        let d = temp("bad");
        assert_eq!(
            run_cli(&args(&[
                "strategy-dry-run",
                "--make-fixture",
                d.to_str().unwrap()
            ])),
            Some(0)
        );
        let f = d.join("demo_fixed/v1/check_values.csv");
        let t = std::fs::read_to_string(&f)
            .unwrap()
            .replacen("101.25", "101.26", 1);
        std::fs::write(&f, t).unwrap();
        assert_eq!(
            run_cli(&args(&[
                "strategy-dry-run",
                "--dir",
                d.join("demo_fixed").to_str().unwrap()
            ])),
            Some(1)
        );
    }

    #[test]
    fn test_dry_run_mid_session_refuses_the_swap_but_loads() {
        let d = temp("mid");
        assert_eq!(
            run_cli(&args(&[
                "strategy-dry-run",
                "--make-fixture",
                d.to_str().unwrap()
            ])),
            Some(0)
        );
        let audit = d.join("a.ndjson");
        let code = run_cli(&args(&[
            "strategy-dry-run",
            "--dir",
            d.join("demo_fixed").to_str().unwrap(),
            "--secs-of-day",
            "36000",
            "--audit",
            audit.to_str().unwrap(),
        ]));
        assert_eq!(code, Some(0));
        assert!(
            std::fs::read_to_string(&audit)
                .unwrap()
                .contains("\"kind\":\"swap_refused\"")
        );
    }
}
