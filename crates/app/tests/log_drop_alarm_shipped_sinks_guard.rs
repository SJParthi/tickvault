//! Lockstep ratchet — the log-lines-dropped page reads only the log files that
//! are shipped to CloudWatch (noise lock §2.9-i, 2026-10-10, stage 1).
//!
//! # Why
//!
//! `tv_log_lines_dropped_total` is published per sink, labelled with the
//! name each writer registers under in `main.rs`. Only two files reach
//! CloudWatch (`deploy/aws/cloudwatch-agent.json` `collect_list`): the app
//! log and `errors.jsonl`. A drop on one of those can hide a coded ERROR
//! from its log filter, which is what the page is for. A drop on errors.log,
//! the category files or stdout loses lines nobody reads in CloudWatch.
//!
//! The filter `log_lines_dropped_shipped` in `data-at-risk-alarms.tf` slices
//! the metrics log to `sink = app_log || sink = errors_jsonl`. Three things
//! must stay true together, and each can drift silently:
//!
//! 1. The sliced labels equal the files the agent ships. Shipping a new file
//!    without widening the slice leaves its drops unpaged; widening the slice
//!    to an unshipped sink brings back the false pages.
//! 2. Each sliced label is the label its writer actually registers under.
//!    Renaming `"app_log"` in `main.rs` would leave the filter matching
//!    nothing and the page dead.
//! 3. Both labels are registered at 0 at boot, so the CloudWatch agent's
//!    dropped first sample is a zero, not the first drop.
//!
//! # Honest limit
//!
//! This reads source and config. Whether the CloudWatch agent writes the
//! `sink` label as a top-level field in `/tickvault/<env>/metrics` is NOT
//! proven here or anywhere in the repo; §2.9-i gates the alarm switch on a
//! measured match for exactly that reason.

mod log_drop_support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tickvault_app::observability::{
    APP_LOG_PREFIX, CATEGORY_CANDLES_DIR, CATEGORY_CANDLES_PREFIX, CATEGORY_LIVE_TICKS_DIR,
    CATEGORY_LIVE_TICKS_PREFIX, ERRORS_JSONL_PREFIX, LOG_LINES_DROPPED_METRIC, MACHINE_LOGS_DIR,
};

/// Shipped file prefix → the sink label its writer registers under.
const SHIP_MAP: &[(&str, &str)] = &[
    (APP_LOG_PREFIX, "app_log"),
    (ERRORS_JSONL_PREFIX, "errors_jsonl"),
];

/// Sink label → the appender function whose writer carries it, and the file
/// prefix that function must open.
const WRITERS: &[(&str, &str, &str)] = &[
    ("app_log", "init_app_log_appender", "APP_LOG_PREFIX"),
    (
        "errors_jsonl",
        "init_errors_jsonl_appender",
        "ERRORS_JSONL_PREFIX",
    ),
];

/// The labels the filter slices on.
const SLICED: [&str; 2] = ["app_log", "errors_jsonl"];

/// The filter's derived metric. Never the raw name: the raw name is still
/// EMF-selected, so reusing it would count the same drops twice.
const DERIVED: &str = "tv_log_lines_dropped_shipped_total";

const FILTER_RESOURCE: &str =
    r#"resource "aws_cloudwatch_log_metric_filter" "log_lines_dropped_shipped" {"#;
const TF: &str = "deploy/aws/terraform/data-at-risk-alarms.tf";
const AGENT: &str = "deploy/aws/cloudwatch-agent.json";
const MAIN: &str = "crates/app/src/main.rs";
const OBSERVABILITY: &str = "crates/app/src/observability.rs";

/// Lines after an appender's init call within which its drop counter must
/// be registered.
const REGISTER_LOOKAHEAD_LINES: usize = 12;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/app -> repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

/// Removes Rust `//` and `/* */` comments, string-literal aware, so a
/// commented-out registration or seed is not counted.
fn strip_rust_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b as char);
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

/// Cuts an HCL line at a `#` comment outside a string literal.
fn strip_hcl_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (at, c) in line.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '#' => return &line[..at],
            _ => {}
        }
    }
    line
}

/// The filter resource body, comments removed, or an error naming why not.
fn filter_block(tf: &str) -> Result<String, String> {
    let start = tf
        .lines()
        .position(|l| l.starts_with(FILTER_RESOURCE))
        .ok_or_else(|| format!("{TF} no longer declares `{FILTER_RESOURCE}` at column 0"))?;
    let mut body = String::new();
    for line in tf.lines().skip(start + 1) {
        if line == "}" {
            return Ok(body);
        }
        body.push_str(strip_hcl_comment(line));
        body.push('\n');
    }
    Err("the filter resource has no closing `}` at column 0".to_owned())
}

/// The `sink` values the filter pattern matches, or an error if the pattern
/// names `$.sink` in any shape other than `$.sink = \"x\"`.
fn sliced_sinks(block: &str) -> Result<BTreeSet<String>, String> {
    let pattern = block
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("pattern"))
        .ok_or("the filter has no `pattern`")?;
    let opener = r#"$.sink = \""#;
    let mut out = BTreeSet::new();
    for (at, _) in pattern.match_indices(opener) {
        let rest = &pattern[at + opener.len()..];
        let value = rest
            .split(r#"\""#)
            .next()
            .ok_or("an unterminated `$.sink` value")?;
        out.insert(value.to_owned());
    }
    let mentions = pattern.matches("$.sink").count();
    if mentions != out.len() || out.is_empty() {
        return Err(format!(
            "the pattern names `$.sink` {mentions} times but only {} as `$.sink = \\\"x\\\"`; \
             any other operator (`!=`, a wildcard) is outside what §2.9-i allows: {pattern}",
            out.len()
        ));
    }
    Ok(out)
}

/// Every `file_path` in the agent's `collect_list`.
fn shipped_file_paths(agent_json: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(agent_json).map_err(|e| format!("{AGENT} is not JSON: {e}"))?;
    let list = v
        .pointer("/logs/logs_collected/files/collect_list")
        .and_then(serde_json::Value::as_array)
        .ok_or("no logs.logs_collected.files.collect_list")?;
    let paths: Vec<String> = list
        .iter()
        .filter_map(|e| e.get("file_path").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    if paths.is_empty() {
        return Err("collect_list names no file_path".to_owned());
    }
    Ok(paths)
}

/// The sink labels of the files the agent ships, through [`SHIP_MAP`].
/// A shipped app log file with no mapping is an error, so shipping a new
/// file forces the slice (and a dated row) to follow.
fn shipped_labels(agent_json: &str) -> Result<BTreeSet<String>, String> {
    let dir = format!("/opt/tickvault/{MACHINE_LOGS_DIR}/");
    let mut out = BTreeSet::new();
    for path in shipped_file_paths(agent_json)? {
        let Some(rest) = path.strip_prefix(&dir) else {
            return Err(format!(
                "{path} is shipped from outside {dir}; decide whether its drops page \
                 and record it in noise lock §2.9-i"
            ));
        };
        let prefix = rest
            .strip_suffix(".2*")
            .ok_or_else(|| format!("{path}: expected a `<prefix>.2*` rotation glob"))?;
        let label = SHIP_MAP
            .iter()
            .find(|(p, _)| *p == prefix)
            .map(|(_, l)| *l)
            .ok_or_else(|| {
                format!(
                    "{path} is shipped but its prefix `{prefix}` has no sink label in SHIP_MAP. \
                     Widen the log-drop slice with a dated row in noise lock §2.9-i first."
                )
            })?;
        out.insert(label.to_owned());
    }
    Ok(out)
}

fn check_slice_matches_shipped(agent_json: &str, tf: &str) -> Result<(), String> {
    let shipped = shipped_labels(agent_json)?;
    let sliced = sliced_sinks(&filter_block(tf)?)?;
    if shipped != sliced {
        return Err(format!(
            "the log-drop filter slices {sliced:?} but the agent ships {shipped:?}"
        ));
    }
    if sliced != set(&SLICED) {
        return Err(format!("the slice is {sliced:?}, expected {SLICED:?}"));
    }
    Ok(())
}

/// The first string literal passed to `register_log_drop_counter(` within
/// [`REGISTER_LOOKAHEAD_LINES`] after the line calling `appender(`.
fn label_registered_after(main: &str, appender: &str) -> Result<String, String> {
    let call = format!("{appender}(");
    let lines: Vec<&str> = main.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains(&call))
        .ok_or_else(|| format!("main.rs no longer calls `{call}`"))?;
    let end = (at + 1 + REGISTER_LOOKAHEAD_LINES).min(lines.len());
    let line = lines[at + 1..end]
        .iter()
        .find(|l| l.contains("register_log_drop_counter("))
        .ok_or_else(|| {
            format!(
                "no register_log_drop_counter( within {REGISTER_LOOKAHEAD_LINES} lines after `{call}`"
            )
        })?;
    let (_, args) = line
        .split_once("register_log_drop_counter(")
        .ok_or("unreachable")?;
    args.split('"')
        .nth(1)
        .filter(|_| args.trim_start().starts_with('"'))
        .map(str::to_owned)
        .ok_or_else(|| format!("the label after `{call}` is not a string literal: {line}"))
}

/// Every string literal registered as a log-drop sink label in `main`, with
/// repeats: the first literal argument of each `register_log_drop_counter(`
/// and `init_lossy_non_blocking(` call.
fn registered_label_literals(main: &str) -> Vec<String> {
    let mut out = Vec::new();
    for call in ["register_log_drop_counter(", "init_lossy_non_blocking("] {
        for span in main.split(call).skip(1) {
            let head = span.split(';').next().unwrap_or_default();
            if let Some(value) = head.split('"').nth(1) {
                out.push(value.to_owned());
            }
        }
    }
    out
}

fn check_writer_labels(main_raw: &str) -> Result<(), String> {
    let main = strip_rust_comments(main_raw);
    for (label, appender, _) in WRITERS {
        let got = label_registered_after(&main, appender)?;
        if got != *label {
            return Err(format!(
                "the writer opened by `{appender}` registers its drops as \"{got}\", but the \
                 log-drop filter slices \"{label}\". The filter would match nothing and the \
                 page would be dead. Rename both together, with a dated row in noise lock §2.9-i."
            ));
        }
    }
    let literals = registered_label_literals(&main);
    for label in SLICED {
        let n = literals.iter().filter(|l| *l == label).count();
        if n != 1 {
            return Err(format!(
                "\"{label}\" is registered as a log-drop sink label {n} times in main.rs; \
                 it must be exactly one writer"
            ));
        }
    }
    Ok(())
}

fn check_filter_shape(tf: &str) -> Result<(), String> {
    let block = filter_block(tf)?;
    let compact: String = block.chars().filter(|c| !c.is_whitespace()).collect();
    let need = [
        r#"log_group_name="/tickvault/${var.environment}/metrics""#.to_owned(),
        format!(r#"name="{DERIVED}""#),
        format!(r#"value="$.{LOG_LINES_DROPPED_METRIC}""#),
        format!(r#"pattern="{{$.{LOG_LINES_DROPPED_METRIC}=*&&("#),
        "namespace=local.app_namespace".to_owned(),
    ];
    for n in &need {
        if !compact.contains(n.as_str()) {
            return Err(format!("the filter no longer carries `{n}`"));
        }
    }
    if compact.contains(&format!(r#"name="{LOG_LINES_DROPPED_METRIC}""#)) {
        return Err("the filter publishes into the RAW name; it must write the derived one".into());
    }
    if compact.contains("default_value") {
        return Err("the filter sets default_value (AWS refuses it with dimensions)".into());
    }
    let dims = compact
        .split_once("dimensions={")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(d, _)| d)
        .ok_or("the filter has no `dimensions = { .. }` block")?;
    if dims != r#"host="$.host""# {
        return Err(format!(
            "the filter dimensions must be host only, found `{dims}`"
        ));
    }
    Ok(())
}

fn check_seeded(main_raw: &str) -> Result<(), String> {
    let main = strip_rust_comments(main_raw);
    let seeded = log_drop_support::seeded_label_values(&main, LOG_LINES_DROPPED_METRIC, "sink");
    if seeded.is_empty() {
        return Err(format!(
            "found NO boot-time seed of {LOG_LINES_DROPPED_METRIC} at all in main.rs. The parser \
             has stopped matching, or the seed is gone; either way this check would be vacuous."
        ));
    }
    let missing: Vec<&str> = SLICED
        .iter()
        .copied()
        .filter(|l| !seeded.contains(*l))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{LOG_LINES_DROPPED_METRIC} is not seeded at 0 at boot for sink {missing:?} \
             (seeded: {seeded:?}); its first drop would be the agent's dropped baseline sample"
        ));
    }
    Ok(())
}

#[test]
fn the_log_drop_filter_slices_exactly_the_sinks_the_agent_ships() {
    let agent = read(AGENT);
    let tf = read(TF);
    if let Err(e) = check_slice_matches_shipped(&agent, &tf) {
        panic!("{e}");
    }
    // Every SHIP_MAP prefix is a file the agent actually ships, so the map
    // cannot carry a stale row that makes a slice look justified.
    let shipped = shipped_labels(&agent).unwrap_or_default();
    assert_eq!(
        shipped.len(),
        SHIP_MAP.len(),
        "SHIP_MAP has a row the agent does not ship"
    );
}

#[test]
fn bite_a_stdout_slice_or_a_newly_shipped_file_turns_the_slice_check_red() {
    let agent = read(AGENT);
    let tf = read(TF);
    let widened = tf.replacen(
        r#"$.sink = \"errors_jsonl\")"#,
        r#"$.sink = \"errors_jsonl\" || $.sink = \"stdout\")"#,
        1,
    );
    assert_ne!(widened, tf, "bite setup: the pattern text moved");
    assert!(
        check_slice_matches_shipped(&agent, &widened).is_err(),
        "adding stdout to the slice must fail the check"
    );

    let more_shipped = agent.replacen(
        r#""file_path": "/opt/tickvault/data/logs/machine/app.2*""#,
        r#""file_path": "/opt/tickvault/data/logs/machine/errors.log.2*" }, { "file_path": "/opt/tickvault/data/logs/machine/app.2*""#,
        1,
    );
    assert_ne!(
        more_shipped, agent,
        "bite setup: the collect_list text moved"
    );
    assert!(
        check_slice_matches_shipped(&more_shipped, &tf).is_err(),
        "shipping a new log file without widening the slice must fail the check"
    );
}

#[test]
fn unshipped_sinks_never_feed_the_page() {
    let block = filter_block(&read(TF)).unwrap_or_else(|e| panic!("{e}"));
    let sliced = sliced_sinks(&block).unwrap_or_else(|e| panic!("{e}"));
    for unshipped in [
        "stdout",
        "errors_log",
        CATEGORY_CANDLES_PREFIX,
        CATEGORY_LIVE_TICKS_PREFIX,
    ] {
        assert!(
            !sliced.contains(unshipped),
            "the log-drop page slices \"{unshipped}\", a sink whose file is not shipped \
             to CloudWatch (noise lock §2.9-i REJECT)"
        );
    }
    for path in shipped_file_paths(&read(AGENT)).unwrap_or_else(|e| panic!("{e}")) {
        for dir in [CATEGORY_CANDLES_DIR, CATEGORY_LIVE_TICKS_DIR] {
            assert!(
                !path.contains(dir),
                "{path} ships a category log ({dir}); its sink must then be added to the \
                 slice with a dated row, and this test updated"
            );
        }
    }
    // The excluded names above are the REAL labels, so the exclusion is not
    // vacuous: each unshipped writer still registers under that name.
    let main = strip_rust_comments(&read(MAIN));
    for needle in [
        r#"init_lossy_non_blocking(file, "errors_log")"#,
        r#"init_lossy_non_blocking(std::io::stdout(), "stdout")"#,
        "register_log_drop_counter(cat.prefix(), &writer)",
    ] {
        assert!(
            main.contains(needle),
            "main.rs no longer registers `{needle}`; re-check which labels are unshipped"
        );
    }
}

#[test]
fn bite_slicing_a_category_sink_is_caught() {
    let tf = read(TF);
    let widened = tf.replacen(
        r#"$.sink = \"errors_jsonl\")"#,
        r#"$.sink = \"errors_jsonl\" || $.sink = \"candles\")"#,
        1,
    );
    assert_ne!(widened, tf, "bite setup: the pattern text moved");
    let sliced = sliced_sinks(&filter_block(&widened).unwrap_or_default()).unwrap_or_default();
    assert!(
        sliced.contains(CATEGORY_CANDLES_PREFIX),
        "the parser must see the added label"
    );
    assert!(check_slice_matches_shipped(&read(AGENT), &widened).is_err());

    let negated = tf.replacen(r#"$.sink = \"errors_jsonl\""#, r#"$.sink != \"stdout\""#, 1);
    assert!(
        sliced_sinks(&filter_block(&negated).unwrap_or_default()).is_err(),
        "a `$.sink != ..` clause must be refused, not ignored"
    );
}

#[test]
fn each_sliced_label_is_the_label_its_shipped_writer_registers_under() {
    if let Err(e) = check_writer_labels(&read(MAIN)) {
        panic!("{e}");
    }
    // Close the chain file -> prefix -> appender: each appender opens the
    // prefix SHIP_MAP pairs with the same label.
    let obs = read(OBSERVABILITY);
    for (label, appender, prefix_const) in WRITERS {
        let body = obs
            .split_once(&format!("pub fn {appender}("))
            .and_then(|(_, rest)| rest.split_once("\n}"))
            .map(|(b, _)| b)
            .unwrap_or_else(|| panic!("observability.rs no longer defines `{appender}`"));
        assert!(
            body.contains(&format!(
                "RollingFileAppender::new(Rotation::HOURLY, &dir, {prefix_const})"
            )),
            "`{appender}` no longer opens {prefix_const}"
        );
        let mapped = SHIP_MAP.iter().find(|(_, l)| l == label).map(|(p, _)| *p);
        let expected = match *prefix_const {
            "APP_LOG_PREFIX" => APP_LOG_PREFIX,
            _ => ERRORS_JSONL_PREFIX,
        };
        assert_eq!(
            mapped,
            Some(expected),
            "SHIP_MAP and WRITERS disagree on \"{label}\""
        );
    }
}

#[test]
fn bite_renaming_a_writer_label_is_caught() {
    let main = read(MAIN);
    let renamed = main.replacen(
        r#"register_log_drop_counter("app_log", &writer)"#,
        r#"register_log_drop_counter("app", &writer)"#,
        1,
    );
    assert_ne!(renamed, main, "bite setup: the app_log registration moved");
    assert!(
        check_writer_labels(&renamed).is_err(),
        "renaming app_log must fail"
    );

    let commented = main.replacen(
        r#"tickvault_app::observability::register_log_drop_counter("errors_jsonl", &writer);"#,
        r#"// tickvault_app::observability::register_log_drop_counter("errors_jsonl", &writer);"#,
        1,
    );
    assert_ne!(
        commented, main,
        "bite setup: the errors_jsonl registration moved"
    );
    assert!(
        check_writer_labels(&commented).is_err(),
        "a commented-out registration must not count"
    );
}

#[test]
fn the_log_drop_filter_shape_is_pinned() {
    if let Err(e) = check_filter_shape(&read(TF)) {
        panic!("{e}");
    }
}

#[test]
fn bite_a_raw_name_default_value_or_extra_dimension_is_caught() {
    let tf = read(TF);
    let raw = tf.replacen(
        &format!(r#"name      = "{DERIVED}""#),
        &format!(r#"name      = "{LOG_LINES_DROPPED_METRIC}""#),
        1,
    );
    assert_ne!(raw, tf, "bite setup: the derived name line moved");
    assert!(
        check_filter_shape(&raw).is_err(),
        "publishing into the raw name must fail"
    );

    let defaulted = tf.replacen(
        r#"    value     = "$.tv_log_lines_dropped_total""#,
        "    value     = \"$.tv_log_lines_dropped_total\"\n    default_value = 0",
        1,
    );
    assert_ne!(defaulted, tf, "bite setup: the value line moved");
    assert!(
        check_filter_shape(&defaulted).is_err(),
        "default_value must fail"
    );

    let sink_dim = tf.replacen(
        r#"      host = "$.host""#,
        "      host = \"$.host\"\n      sink = \"$.sink\"",
        1,
    );
    assert_ne!(sink_dim, tf, "bite setup: the dimensions block moved");
    assert!(
        check_filter_shape(&sink_dim).is_err(),
        "a sink dimension must fail"
    );
}

#[test]
fn the_slice_labels_are_seeded_at_boot() {
    if let Err(e) = check_seeded(&read(MAIN)) {
        panic!("{e}");
    }
}

#[test]
fn bite_an_unseeded_label_or_a_vacuous_scan_is_caught() {
    let main = read(MAIN);
    let one_label = main.replacen(
        r#"for sink in ["app_log", "errors_jsonl"] {"#,
        r#"for sink in ["app_log"] {"#,
        1,
    );
    assert_ne!(one_label, main, "bite setup: the seed loop moved");
    assert!(
        check_seeded(&one_label).is_err(),
        "dropping errors_jsonl from the seed must fail"
    );

    let no_seed = main.replacen(
        r#"metrics::counter!("tv_log_lines_dropped_total", "sink" => sink).increment(0);"#,
        r#"// metrics::counter!("tv_log_lines_dropped_total", "sink" => sink).increment(0);"#,
        1,
    );
    assert_ne!(no_seed, main, "bite setup: the seed line moved");
    let err = check_seeded(&no_seed).err().unwrap_or_default();
    assert!(
        err.contains("NO boot-time seed"),
        "a vanished seed must fail as vacuous: {err}"
    );

    // The literal form is recognised too, wrapped or not.
    let literal = "metrics::counter!(\n    \"tv_log_lines_dropped_total\",\n    \"sink\" => \"app_log\"\n).increment(0);\nmetrics::counter!(\"tv_log_lines_dropped_total\", \"sink\" => \"errors_jsonl\").increment(0);";
    assert!(
        check_seeded(literal).is_ok(),
        "the literal seed form must be recognised"
    );
}
