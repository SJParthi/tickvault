//! Every CloudWatch alarm must watch a metric that can actually ARRIVE.
//!
//! ## The blind spot this closes
//!
//! An alarm is configured in terraform; the metric it watches is produced
//! somewhere else entirely. Nothing in this repository connected the two, so
//! an alarm could be added on an app metric whose name never reached the
//! CloudWatch agent's selector — and it would sit in the console looking
//! exactly like a healthy alarm forever. With `treat_missing_data =
//! "notBreaching"` it reads a confident green; with the default it reads
//! "insufficient data", which operators learn to ignore. Either way the thing
//! it was built to catch goes uncaught.
//!
//! This repo has retired that exact class twice already (`ws-reinject-01`,
//! `tick-conserve-01` — filters that could never match). The audit of
//! 2026-08-22 found the current state CLEAN: all 31 alarm metrics have a real
//! route. This guard is what keeps it that way, because "clean today" and
//! "cannot regress" are different claims and only one of them is enforceable.
//!
//! ## The four legitimate routes
//!
//! 1. **The EMF selector** — the CloudWatch agent's `metric_selectors` regex
//!    in `user-data.sh.tftpl`. This is how ordinary app metrics travel.
//! 2. **A log metric filter** — CloudWatch derives the metric from a log line
//!    instead. Four alarms use this deliberately; their filter names say
//!    `fallback`, because the log line survives even when the metrics
//!    pipeline is the thing that is broken.
//! 3. **A Lambda calling PutMetricData** — published from outside the box
//!    entirely, so the agent is not involved. The deploy watchdog does this.
//! 4. **Declared dormant** — pre-wired ahead of its emitter ("arm on
//!    arrival"). Allowed, but it must be named in the list below, so that
//!    dormancy is a decision on the record rather than an accident nobody
//!    noticed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Alarms deliberately wired before their metric exists.
///
/// An entry here is a PROMISE that the emitter is coming, not a place to
/// silence an inconvenient failure. Each carries the reason inline so the
/// next reader can judge whether the promise is still live.
const DORMANT_BY_DESIGN: &[(&str, &str)] = &[(
    "tv_order_fill_lag_seconds",
    "arm-on-arrival: the emitter ships with the Phase-1 order path; pinned \
     separately by cloudwatch_dormant_alarms_guard, which requires the alarm \
     description to carry the DORMANT markers",
)];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/common parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn terraform_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tf"))
        .collect();
    out.sort();
    out
}

/// Every `"tv_..."` string that appears inside a block of the given resource
/// type, keyed by the position of the type marker.
///
/// Brace-depth scanning rather than a line regex: an alarm that uses metric
/// math nests its `metric_name` inside a `metric_query` block, and a line-wise
/// scan would either miss those or, worse, attribute them to whichever
/// resource happened to be above them in the file.
fn metrics_in_resource_blocks(content: &str, resource_type: &str) -> BTreeSet<String> {
    let marker = format!("resource \"{resource_type}\"");
    let mut found = BTreeSet::new();
    let mut search_from = 0usize;

    while let Some(rel) = content[search_from..].find(&marker) {
        let start = search_from + rel;
        let Some(open_rel) = content[start..].find('{') else {
            break;
        };
        let body_start = start + open_rel;
        let mut depth = 0usize;
        let mut end = body_start;
        for (i, ch) in content[body_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = body_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let block = &content[body_start..=end.min(content.len() - 1)];
        let key = if resource_type.contains("log_metric_filter") {
            "name"
        } else {
            "metric_name"
        };
        for m in assigned_tv_values(block, key) {
            found.insert(m);
        }
        search_from = end.max(body_start + 1);
    }
    found
}

/// Strip `#` comment lines. Terraform in this repo carries long dated
/// rationale in comments, and those comments NAME retired metrics: the first
/// draft of this guard reported three phantom dead monitors that were nothing
/// but historical prose and an IAM role identifier. A guard that cries wolf
/// gets muted, so the scan reads assignments only.
fn strip_comments(block: &str) -> String {
    block
        .lines()
        .map(|l| match l.find('#') {
            Some(at) => &l[..at],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Values assigned to `key = "tv_..."` within the slice.
fn assigned_tv_values(block: &str, key: &str) -> Vec<String> {
    let cleaned = strip_comments(block);
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = cleaned[from..].find(key) {
        let at = from + rel;
        // The key must be a whole word, not the tail of `alarm_metric_name`.
        let prev_ok = at == 0
            || !cleaned.as_bytes()[at - 1].is_ascii_alphanumeric()
                && cleaned.as_bytes()[at - 1] != b'_';
        let rest = &cleaned[at + key.len()..];
        let trimmed = rest.trim_start();
        if prev_ok && trimmed.starts_with('=') {
            let after_eq = trimmed[1..].trim_start();
            if let Some(stripped) = after_eq.strip_prefix('"')
                && let Some(close) = stripped.find('"')
            {
                let val = &stripped[..close];
                if val.starts_with("tv_") {
                    out.push(val.to_string());
                }
            }
        }
        from = at + key.len();
    }
    out
}

/// Pull every `tv_<name>` token out of a slice.
///
/// Byte comparison, never a slice, to test the prefix: these files carry em
/// dashes and other multi-byte characters in their comments, and slicing at
/// `i..i+3` to compare would panic the moment `i` landed inside one. The
/// resulting `hay[i..j]` is safe because the span it covers is ASCII by
/// construction.
fn tv_names(hay: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = hay.as_bytes();
    let mut i = 0usize;
    while i + 3 <= bytes.len() {
        if bytes[i] == b't' && bytes[i + 1] == b'v' && bytes[i + 2] == b'_' {
            let mut j = i + 3;
            while j < bytes.len()
                && (bytes[j].is_ascii_lowercase() || bytes[j].is_ascii_digit() || bytes[j] == b'_')
            {
                j += 1;
            }
            if j > i + 3 {
                out.push(hay[i..j].to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Names carried by the CloudWatch agent's EMF `metric_selectors` regex.
///
/// Reads `deploy/aws/cloudwatch-agent.json` — the file the box actually loads.
/// Until 2026-08-25 this content was ALSO embedded in
/// `deploy/aws/terraform/user-data.sh.tftpl`, and this guard read that copy.
/// The duplicate was ~1.6 KB and it pinned the user-data template at exactly
/// its 15,872-byte budget with zero bytes free, so it was removed: the
/// template now writes a minimal host-only fallback and copies this file into
/// place after the Step 5 clone. `cw_agent_selector_lockstep_guard.rs` pins
/// that the copy still happens.
fn emf_selected(root: &Path) -> BTreeSet<String> {
    let path = root.join("deploy/aws/cloudwatch-agent.json");
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path:?}: {e}"));
    let at = content
        .find("\"metric_selectors\"")
        .expect("the deployed CloudWatch agent config carries a metric_selectors key");
    let tail = &content[at..];
    let end = tail.find(']').expect("metric_selectors list is closed");
    tv_names(&tail[..end]).into_iter().collect()
}

#[test]
fn every_alarm_metric_can_actually_arrive() {
    let root = repo_root();
    let tf_dir = root.join("deploy/aws/terraform");
    let files = terraform_files(&tf_dir);
    assert!(
        files.len() > 5,
        "terraform directory looks empty — the guard would pass vacuously"
    );

    let selector = emf_selected(&root);
    assert!(
        selector.len() > 50,
        "EMF selector parsed to only {} names — parser drift, not a real \
         shrink; refusing to judge alarms against a broken read",
        selector.len()
    );

    let lambda_published: BTreeSet<String> = {
        let dir = root.join("crates/aws-lambdas/src");
        let mut set = BTreeSet::new();
        for entry in std::fs::read_dir(&dir).expect("aws-lambdas src").flatten() {
            if entry.path().extension().is_some_and(|x| x == "rs")
                && let Ok(body) = std::fs::read_to_string(entry.path())
            {
                set.extend(tv_names(&body));
            }
        }
        set
    };

    let mut alarm_metrics = BTreeSet::new();
    let mut log_filter_metrics = BTreeSet::new();
    for f in &files {
        let content = std::fs::read_to_string(f).unwrap_or_else(|e| panic!("read {f:?}: {e}"));
        alarm_metrics.extend(metrics_in_resource_blocks(
            &content,
            "aws_cloudwatch_metric_alarm",
        ));
        log_filter_metrics.extend(metrics_in_resource_blocks(
            &content,
            "aws_cloudwatch_log_metric_filter",
        ));
    }

    assert!(
        alarm_metrics.len() > 20,
        "only {} alarm metrics found — the block scanner is broken, and a \
         broken scanner passes this test for the wrong reason",
        alarm_metrics.len()
    );

    let dormant: BTreeSet<&str> = DORMANT_BY_DESIGN.iter().map(|(m, _)| *m).collect();
    let mut unroutable = Vec::new();
    for m in &alarm_metrics {
        let routed = selector.contains(m)
            || log_filter_metrics.contains(m)
            || lambda_published.contains(m)
            || dormant.contains(m.as_str());
        if !routed {
            unroutable.push(m.clone());
        }
    }

    assert!(
        unroutable.is_empty(),
        "these alarm metrics have NO route to CloudWatch — the alarms exist \
         but can never fire, which reads in the console exactly like health:\n  {}\n\
         Fix by one of: add the name to the EMF selector in user-data.sh.tftpl \
         (and its twin cloudwatch-agent.json), derive it from a log metric \
         filter, publish it from a Lambda, or — if the emitter is genuinely \
         still to come — add it to DORMANT_BY_DESIGN with the reason.",
        unroutable.join("\n  ")
    );
}

#[test]
fn dormant_entries_do_not_outlive_their_reason() {
    let root = repo_root();
    let selector = emf_selected(&root);

    for (metric, reason) in DORMANT_BY_DESIGN {
        assert!(
            !reason.trim().is_empty(),
            "{metric}: a dormant entry without a reason is just a silenced alarm"
        );
        assert!(
            !selector.contains(*metric),
            "{metric} is listed as DORMANT but the EMF selector now carries it. \
             The emitter arrived — remove the dormant entry so the guard checks \
             the real route instead of waving it through."
        );
    }
}

#[test]
fn the_block_scanner_finds_nested_and_ignores_neighbours() {
    // Metric math nests the name two levels down; a line-wise scan misses it.
    let nested = r#"
resource "aws_cloudwatch_metric_alarm" "math" {
  alarm_name = "x"
  metric_query {
    metric {
      metric_name = "tv_nested_metric_total"
    }
  }
}
resource "aws_cloudwatch_log_metric_filter" "other" {
  metric_transformation {
    name = "tv_from_a_log_line"
  }
}
"#;
    let alarms = metrics_in_resource_blocks(nested, "aws_cloudwatch_metric_alarm");
    assert!(
        alarms.contains("tv_nested_metric_total"),
        "a nested metric_name must be attributed to its alarm"
    );
    assert!(
        !alarms.contains("tv_from_a_log_line"),
        "a neighbouring resource's metric must NOT leak into the alarm set — \
         that would let a log filter silently satisfy the check for an alarm \
         that has no route at all"
    );

    let filters = metrics_in_resource_blocks(nested, "aws_cloudwatch_log_metric_filter");
    assert!(filters.contains("tv_from_a_log_line"));
    assert!(!filters.contains("tv_nested_metric_total"));
}

#[test]
fn the_guard_reports_its_own_coverage() {
    // Non-vacuity, printed rather than asserted at an exact number: the counts
    // move whenever an alarm is added, and a guard that must be edited on
    // every legitimate change gets edited without being read.
    let root = repo_root();
    let files = terraform_files(&root.join("deploy/aws/terraform"));
    let mut alarms = BTreeSet::new();
    let mut filters = BTreeSet::new();
    for f in &files {
        let c = std::fs::read_to_string(f).expect("read tf");
        alarms.extend(metrics_in_resource_blocks(
            &c,
            "aws_cloudwatch_metric_alarm",
        ));
        filters.extend(metrics_in_resource_blocks(
            &c,
            "aws_cloudwatch_log_metric_filter",
        ));
    }
    let selector = emf_selected(&root);
    println!(
        "alarm metrics checked = {}; EMF-selected = {}; log-filter-derived = {}; dormant = {}",
        alarms.len(),
        selector.len(),
        filters.len(),
        DORMANT_BY_DESIGN.len()
    );
    assert!(alarms.len() >= 20, "coverage collapsed to {}", alarms.len());
}
