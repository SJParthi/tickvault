//! A self-contained demo strategy folder for the dry run (`tickvault
//! strategy-dry-run --make-fixture <dir>`): a fixed strategy and a dynamic one,
//! with check files produced by [`super::engine::ReferenceDouble`]'s documented
//! toy rule and manifests that vouch for them. TEST FIXTURE: not a strategy.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use std::path::Path;

use super::manifest::manifest_json_for;

/// The brutex commit the fixture recipes name; the dry run builds against it.
pub const FIXTURE_COMMIT: &str = "4bbd37df547b38f47a14d8e1b970df85540a395f";

const BARS: &[(&str, u64, u8, &str, f64)] = &[
    ("2026-10-27T09:15:00", 2885, 1, "1m", 100.5),
    ("2026-10-27T09:16:00", 2885, 1, "1m", 101.25),
    ("2026-10-27T09:17:00", 2885, 1, "1m", 100.75),
    ("2026-10-27T09:18:00", 2885, 1, "1m", 100.75),
    ("2026-10-27T09:15:00", 27, 0, "5m", 24000.0),
    ("2026-10-27T09:20:00", 27, 0, "5m", 24010.5),
];

/// The check files the toy rule produces for `bars`.
#[must_use]
pub fn toy_check_files(bars: &[(&str, u64, u8, &str, f64)]) -> (String, String, String) {
    let mut c =
        String::from("ts_ist,security_id,exchange_segment,timeframe,open,high,low,close,volume\n");
    let mut v = String::from("ts_ist,security_id,exchange_segment,timeframe,close,prev_close\n");
    let mut s = String::from(
        "ts_ist,security_id,exchange_segment,timeframe,signal,side,qty_lots,entry_price,stop_loss,target\n",
    );
    let mut prev: std::collections::BTreeMap<(u64, u8, String), f64> =
        std::collections::BTreeMap::new();
    for &(ts, id, seg, tf, close) in bars {
        c.push_str(&format!(
            "{ts},{id},{seg},{tf},{close},{},{},{close},1000\n",
            close + 1.0,
            close - 1.0
        ));
        let p = prev.insert((id, seg, tf.to_owned()), close);
        v.push_str(&format!(
            "{ts},{id},{seg},{tf},{close},{}\n",
            p.unwrap_or(0.0)
        ));
        match p {
            Some(p) if close > p => s.push_str(&format!(
                "{ts},{id},{seg},{tf},entry,buy,1,0,{},{}\n",
                close - 1.0,
                close + 2.0
            )),
            Some(p) if close < p => s.push_str(&format!(
                "{ts},{id},{seg},{tf},exit,sell,1,0,{},{}\n",
                close - 1.0,
                close + 2.0
            )),
            _ => s.push_str(&format!("{ts},{id},{seg},{tf},none,,0,0,0,0\n")),
        }
    }
    (c, v, s)
}

fn write_folder(dir: &Path, files: &[(&str, String)]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut pairs: Vec<(&str, &[u8])> = Vec::new();
    for (n, b) in files {
        std::fs::write(dir.join(n), b)?;
        pairs.push((n, b.as_bytes()));
    }
    std::fs::write(dir.join("manifest.json"), manifest_json_for(&pairs))
}

fn recipe(name: &str, dynamic: bool) -> String {
    let instruments = if dynamic {
        "[]"
    } else {
        r#"[{"security_id":2885,"exchange_segment":1},{"security_id":27,"exchange_segment":0}]"#
    };
    format!(
        r#"{{"schema":"tickvault-strategy-recipe/1","name":"{name}","version":1,"status":"approved","approved_by":"fixture","approved_at_ist":"2026-10-29T18:00:00+05:30","brutex_commit":"{FIXTURE_COMMIT}","dynamic":{dynamic},"instruments":{instruments},"timeframes":["1m","5m"],"settings":{{}},"backtest":{{"note":"fixture"}}}}"#
    )
}

/// Writes `demo_fixed/` and `demo_dynamic/` under `root`. Returns their paths.
pub fn write_demo_fixture(root: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let (c, v, s) = toy_check_files(BARS);
    let fixed = root.join("demo_fixed");
    write_folder(
        &fixed.join("v1"),
        &[
            ("recipe.json", recipe("demo_fixed", false)),
            ("check_candles.csv", c.clone()),
            ("check_values.csv", v.clone()),
            ("check_signals.csv", s.clone()),
        ],
    )?;
    let dynamic = root.join("demo_dynamic");
    let vdir = dynamic.join("v1");
    write_folder(
        &vdir,
        &[
            ("recipe.json", recipe("demo_dynamic", true)),
            ("check_candles.csv", c.clone()),
            ("check_values.csv", v.clone()),
            ("check_signals.csv", s.clone()),
        ],
    )?;
    for (date, k) in [("2026-10-28", 1.0_f64), ("2026-10-29", 1.1)] {
        write_folder(
            &vdir.join("params").join(date),
            &[
                (
                    "params.json",
                    format!(r#"{{"date":"{date}","params":{{"k":{k}}}}}"#),
                ),
                (
                    "gates.json",
                    r#"{"passed":true,"window_from":"2026-07-01","window_to":"2026-10-28"}"#
                        .to_owned(),
                ),
                ("check_candles.csv", c.clone()),
                ("check_values.csv", v.clone()),
                ("check_signals.csv", s.clone()),
            ],
        )?;
    }
    write_folder(
        &dynamic.join("universe").join("2026-10-29"),
        &[("universe.json", r#"{"date":"2026-10-29","instruments":[{"security_id":2885,"exchange_segment":1},{"security_id":27,"exchange_segment":0}]}"#.to_owned())],
    )?;
    Ok(vec![fixed, dynamic])
}
// O(1) EXEMPT: end
