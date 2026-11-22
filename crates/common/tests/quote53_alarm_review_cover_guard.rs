//! QUOTE 53 ALARM REVIEW — KEPT-COVER GUARD (2026-11-11).
//!
//! Operator Quote 53 (2026-11-11) cut CloudWatch alarms but kept every
//! data-loss and loss-group page, the lane-down / socket-parked / ticks-dropped
//! pages, the token, budget, disk-full, QuestDB-down and cross-verification
//! pages (the KEPT list, `daily-universe-scope-expansion-2026-05-27.md`).
//!
//! The first draft of the review (`docs/audits/2026-11-alarm-review.md`)
//! removed five alarms that were the ONLY page for a failure on that list,
//! claiming a cover that did not hold, and stated "from December" without
//! anything enforcing it. Each test below fails on that first draft:
//!
//! 1. `errcode-stream-silent` and its two schema filters: errors.jsonl and
//!    app.log ship to the SAME log group, so `app-log-ingestion-silent`
//!    cannot see errors.jsonl alone stop (noise lock §2.3s).
//! 2. `live-universe-fallback`: the universe-collapse filter matches only
//!    `fell_back_to_indices`; every other fallback reason pages only here.
//! 3. `tick-spill-quarantined`: a whole quarantined tick file is missing from
//!    QuestDB; no other page counts whole files.
//! 4. `wal-suspension-probe-blind`: the only blind detector left for the kept
//!    `questdb-wal-suspended` page.
//! 5. `deploy-provenance-blind`: without it the kept `binary-sha-stale`
//!    reads OK when it does not know (noise lock §2.3q).
//! 6. No kept alarm's description points at a removed alarm as if it still
//!    existed (the disk pages named `disk-fill-rate-high` as their early
//!    warning).
//! 7. The review table and terraform agree: every REMOVE row is gone, every
//!    KEEP row is live.
//! 8. The date gate: terraform refuses any plan before 2026-12-01 00:00 IST.
//!    Delete test 8 together with `terraform_data.quote53_not_before_december`
//!    in the first change after 2026-12-01.
//!
//! A second review (same day) added:
//!
//! 9. The metric half: `deploy-aws.yml` and the morning boot copy
//!    `cloudwatch-agent.json` to the box with no terraform in the path, so
//!    the trimmed config may not be in the tree before 1 December. This test
//!    FAILS the build until then, by design. Delete it with test 8.
//! 10. `wal-replay-restore-failed` is a data-loss path whose only page is
//!     the coded WS-SPILL-02 ERROR and the kept `errcode-ws-spill-02`.
//! 11. The kept `boot-heartbeat-missing` ships disarmed and only its gate
//!     arms it, so the gate keeps its Errors and did-it-run alarms.
//!
//! HONEST LIMIT: these read terraform and config text; they prove the alarms
//! are declared with the right metric and shape, not that AWS evaluates them
//! as intended (no `terraform plan` runs here).

use std::path::{Path, PathBuf};

const TF_DIR: &str = "deploy/aws/terraform";
const AGENT: &str = "deploy/aws/cloudwatch-agent.json";
const REVIEW: &str = "docs/audits/2026-11-alarm-review.md";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("quote53 guard: cannot canonicalize repo root") // APPROVED: test
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel))
        .unwrap_or_else(|e| panic!("quote53 guard: cannot read {rel}: {e}")) // APPROVED: test
}

/// Terraform text with whole-line `#` comments removed, so a tombstone
/// comment naming a removed resource never satisfies a check.
fn code_only(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `.tf` file in the terraform directory, comments stripped, joined.
fn all_tf_code() -> String {
    let mut files: Vec<PathBuf> = std::fs::read_dir(root().join(TF_DIR))
        .expect("quote53 guard: terraform dir") // APPROVED: test
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tf"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|p| code_only(&std::fs::read_to_string(p).unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `resource "<kind>" "<name>" { ... }` (brace-matched), or None.
fn resource_body(code: &str, kind: &str, name: &str) -> Option<String> {
    let header = format!("resource \"{kind}\" \"{name}\" {{");
    let start = code.find(&header)? + header.len();
    let mut depth = 1usize;
    for (i, c) in code[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(code[start..start + i].to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

fn alarm(code: &str, name: &str) -> Option<String> {
    resource_body(code, "aws_cloudwatch_metric_alarm", name)
}

/// The names in the main EMF `metric_selectors` alternation.
fn shipped_emf_names() -> Vec<String> {
    let agent = read(AGENT);
    let mut out = Vec::new();
    for chunk in agent.split("\"^(").skip(1) {
        if let Some(end) = chunk.find(")$") {
            out.extend(chunk[..end].split('|').map(str::to_owned));
        }
    }
    out
}

/// Every alarm slug terraform declares (the part after `tv-${var.environment}-`),
/// with the coded-error map expanded.
fn live_alarm_slugs(code: &str) -> Vec<String> {
    let mut slugs = Vec::new();
    for line in code.lines() {
        let t = line.trim_start();
        if !t.starts_with("alarm_name") {
            continue;
        }
        if let Some(i) = t.find("\"tv-${var.environment}-") {
            let rest = &t[i + "\"tv-${var.environment}-".len()..];
            if let Some(end) = rest.find('"') {
                slugs.push(rest[..end].to_owned());
            }
        }
    }
    // Expand `errcode-${each.key}` with the error_code_alerts map keys.
    let codes = code_only(&read(&format!("{TF_DIR}/error-code-alarms.tf")));
    let keys: Vec<String> = codes
        .lines()
        .filter_map(|l| {
            let t = l.strip_prefix("    \"")?;
            let end = t.find("\" = {")?;
            Some(t[..end].to_owned())
        })
        .collect();
    slugs.retain(|s| s != "errcode-${each.key}");
    slugs.extend(keys.into_iter().map(|k| format!("errcode-{k}")));
    slugs
}

/// `(slug, decision)` for every row of the review's per-alarm table.
fn review_rows() -> Vec<(String, String)> {
    let review = read(REVIEW);
    let mut rows = Vec::new();
    for line in review.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // | `tv-<env>-slug` | file | watches | DECISION | reason |
        if cells.len() < 6 {
            continue;
        }
        let Some(slug) = cells[1]
            .strip_prefix("`tv-<env>-")
            .and_then(|s| s.strip_suffix('`'))
        else {
            continue;
        };
        let decision = cells[4];
        if decision == "KEEP" || decision == "REMOVE" {
            rows.push((slug.to_owned(), decision.to_owned()));
        }
    }
    rows
}

#[test]
fn the_coded_error_stream_watcher_and_its_two_schema_filters_stay() {
    let code = all_tf_code();
    // The reason the watcher is needed at all: both files ship to ONE group,
    // so a group-level ingestion alarm cannot see one of them stop.
    let agent = read(AGENT);
    let group_of = |file: &str| -> Option<String> {
        let line = agent.lines().find(|l| l.contains(file))?;
        let i = line.find("\"log_group_name\": \"")? + "\"log_group_name\": \"".len();
        Some(line[i..i + line[i..].find('"')?].to_owned())
    };
    let jsonl = group_of("machine/errors.jsonl").expect("errors.jsonl is shipped"); // APPROVED: test
    let app = group_of("machine/app.2").expect("app.log is shipped"); // APPROVED: test
    if jsonl != app {
        // Separate groups would let a per-group ingestion alarm see it; the
        // watcher would then be one of two covers, still required below.
        eprintln!("errors.jsonl ({jsonl}) and app.log ({app}) ship to different groups");
    }
    let body = alarm(&code, "errcode_stream_silent").expect(
        "tv-<env>-errcode-stream-silent is gone. It is the only page for errors.jsonl \
         alone ceasing to ship while app.log keeps the log group alive; every KEPT \
         $.code page would go dark with no page. Noise lock §2.3s REJECTs removing it \
         on the ground that app-log-ingestion-silent covers it.",
    ); // APPROVED: test
    for needle in [
        "tv_log_errors_flat_total",
        "tv_log_errors_nested_total",
        "FILL(flat, 0)",
        "alarm_actions = local.app_alarm_actions",
    ] {
        assert!(
            body.contains(needle),
            "errcode_stream_silent lost `{needle}`"
        );
    }
    for (name, pattern) in [
        ("log_schema_flat_errors", "$.code = *"),
        ("log_schema_nested_errors", "$.fields.code = *"),
    ] {
        let filter = resource_body(&code, "aws_cloudwatch_log_metric_filter", name)
            .unwrap_or_else(|| panic!("metric filter {name} feeding stream-silent is gone")); // APPROVED: test
        assert!(
            filter.contains(pattern),
            "{name} no longer matches `{pattern}`"
        );
    }
}

#[test]
fn every_universe_fallback_reason_the_collapse_filter_misses_still_pages() {
    let code = all_tf_code();
    let collapse = code_only(&read(&format!("{TF_DIR}/error-code-alarms.tf")));
    let pattern_line = collapse
        .lines()
        .skip_while(|l| !l.contains("\"ws-gap-03-universe-collapse\" = {"))
        .find(|l| l.trim_start().starts_with("pattern"))
        .expect("the universe-collapse entry has a pattern") // APPROVED: test
        .to_owned();
    let src = read("crates/app/src/dhan_live_universe.rs");
    let list_start = src
        .find("pub const MASTER_SOURCING_FALLBACK_REASONS: &[&str] = &[")
        .expect("MASTER_SOURCING_FALLBACK_REASONS"); // APPROVED: test
    let list_end = list_start + src[list_start..].find("];").expect("list end"); // APPROVED: test
    let reasons: Vec<&str> = src[list_start..list_end]
        .lines()
        .filter_map(|l| l.trim().strip_prefix('"')?.strip_suffix("\","))
        .collect();
    assert!(
        reasons.len() >= 4,
        "parsed only {} fallback reasons; the parser is broken",
        reasons.len()
    );
    let missed: Vec<&&str> = reasons
        .iter()
        .filter(|r| !pattern_line.contains(&format!("\"{r}\"")))
        .collect();
    assert!(
        !missed.is_empty(),
        "every reason is matched by the collapse filter?"
    );
    let body = alarm(&code, "live_universe_fallback").unwrap_or_else(|| {
        panic!(
            "tv-<env>-live-universe-fallback is gone, but the universe-collapse filter \
             does not match these fallback reasons: {missed:?}. Each leaves contracts \
             unsubscribed for the session (truncated_to_capacity is the owner's D3a \
             \"fill 25,000 by priority with a critical page\"). It is not a duplicate."
        )
    }); // APPROVED: test
    assert!(body.contains("metric_name         = \"tv_dhan_live_universe_fallback_total\""));
    assert!(body.contains("alarm_actions = local.app_alarm_actions"));
    assert!(
        shipped_emf_names()
            .iter()
            .any(|n| n == "tv_dhan_live_universe_fallback_total"),
        "tv_dhan_live_universe_fallback_total is not shipped, so the fallback page \
         reads no data"
    );
    // The counter the alarm reads is the one the app increments.
    assert!(src.contains(
        "pub const MASTER_SOURCING_FALLBACK_COUNTER: &str = \"tv_dhan_live_universe_fallback_total\";"
    ));
}

#[test]
fn a_quarantined_tick_spill_file_still_pages() {
    let code = all_tf_code();
    let body = alarm(&code, "tick_spill_quarantined").expect(
        "tv-<env>-tick-spill-quarantined is gone. A quarantined tick file is NOT in \
         QuestDB and is never replayed automatically; market-data-refused counts \
         single refused lines, not whole files, and TICK-SPILL-01 has no filter.",
    ); // APPROVED: test
    assert!(body.contains("tv_tick_spill_replay_quarantined_total"));
    assert!(body.contains("alarm_actions = local.app_alarm_actions"));
    assert!(
        shipped_emf_names()
            .iter()
            .any(|n| n == "tv_tick_spill_replay_quarantined_total"),
        "tv_tick_spill_replay_quarantined_total is not shipped"
    );
    let src = read("crates/storage/src/tick_spill_replay.rs");
    assert!(src.contains(
        "pub const REPLAY_QUARANTINED_COUNTER: &str = \"tv_tick_spill_replay_quarantined_total\";"
    ));
    // The loss group does not already cover whole-file quarantines.
    let groups = read(&format!("{TF_DIR}/metrics-log-metric-filters.tf"));
    assert!(
        !code_only(&groups).contains("tv_tick_spill_replay_quarantined_total"),
        "a loss group now reads the quarantine counter; re-check whether this page \
         is still the only cover before changing it"
    );
}

#[test]
fn the_kept_wal_suspended_page_has_a_blind_detector() {
    let code = all_tf_code();
    let kept = alarm(&code, "questdb_wal_suspended").expect("questdb_wal_suspended is KEPT"); // APPROVED: test
    assert!(kept.contains("tv_questdb_wal_suspended_tables"));
    let blind = alarm(&code, "wal_suspension_probe_blind").expect(
        "tv-<env>-wal-suspension-probe-blind is gone. When wal_tables() fails while \
         QuestDB still answers, the gauge reads -1: questdb-wal-suspended (needs at \
         least 1) and questdb-disconnected both stay green, so a suspended table \
         keeps acknowledging rows that never become visible, with no page.",
    ); // APPROVED: test
    for needle in [
        "metric_name         = \"tv_questdb_wal_suspended_tables\"",
        "comparison_operator = \"LessThanThreshold\"",
        "statistic           = \"Minimum\"",
        "threshold           = 0",
        "alarm_actions       = local.app_alarm_actions",
    ] {
        assert!(
            blind.contains(needle),
            "wal_suspension_probe_blind lost `{needle}`"
        );
    }
    assert!(
        !blind.contains("the WAL-SUSPEND-01 filter resumes"),
        "the blind alarm's text points at the removed WAL-SUSPEND-01 filter"
    );
}

#[test]
fn the_kept_stale_binary_page_has_its_blind_detector() {
    let code = all_tf_code();
    if alarm(&code, "binary_sha_stale").is_none() {
        return; // nothing to be blind about
    }
    let filter = resource_body(
        &code,
        "aws_cloudwatch_log_metric_filter",
        "deploy_provenance_blind",
    )
    .expect("the provenance-blind filter feeding the alarm is gone"); // APPROVED: test
    assert!(filter.contains("tv_deploy_provenance_blind_total"));
    let blind = alarm(&code, "deploy_provenance_blind").expect(
        "tv-<env>-deploy-provenance-blind is gone while binary-sha-stale is kept: \
         a blind watchdog makes binary-sha-stale read OK on no data (noise lock §2.3q).",
    ); // APPROVED: test
    assert!(blind.contains("tv_deploy_provenance_blind_total"));
    assert!(blind.contains("alarm_actions       = [aws_sns_topic.tv_alerts.arn]"));
}

#[test]
fn no_alarm_description_points_at_a_removed_alarm_as_if_it_existed() {
    let code = all_tf_code();
    let removed: Vec<String> = review_rows()
        .into_iter()
        .filter(|(_, d)| d == "REMOVE")
        .map(|(s, _)| s)
        .collect();
    assert!(
        removed.len() >= 30,
        "parsed only {} REMOVE rows",
        removed.len()
    );
    let mut stale = Vec::new();
    for line in code.lines() {
        let t = line.trim_start();
        if !t.starts_with("alarm_description") {
            continue;
        }
        for sentence in t.split(". ") {
            for slug in &removed {
                let named = [
                    format!("tv-<env>-{slug}"),
                    format!("tv-${{var.environment}}-{slug}"),
                ]
                .iter()
                .any(|n| {
                    sentence.match_indices(n.as_str()).any(|(i, _)| {
                        // Whole slug only: `foo` must not match `foo-bar`.
                        !sentence[i + n.len()..]
                            .chars()
                            .next()
                            .is_some_and(|c| c == '-' || c.is_ascii_alphanumeric())
                    })
                });
                if named && !sentence.contains("removed") {
                    stale.push(format!("{slug}: {sentence}"));
                }
            }
        }
    }
    assert!(
        stale.is_empty(),
        "kept alarm descriptions name a removed alarm without saying it was removed:\n{}",
        stale.join("\n")
    );
}

#[test]
fn the_review_table_and_terraform_agree() {
    let code = all_tf_code();
    let live = live_alarm_slugs(&code);
    let rows = review_rows();
    assert!(rows.len() >= 100, "parsed only {} review rows", rows.len());
    let mut wrong = Vec::new();
    for (slug, decision) in &rows {
        let is_live = live.iter().any(|s| s == slug);
        match (decision.as_str(), is_live) {
            ("REMOVE", true) => wrong.push(format!("{slug}: REMOVE row but still declared")),
            ("KEEP", false) => wrong.push(format!("{slug}: KEEP row but not declared")),
            _ => {}
        }
    }
    for slug in [
        "errcode-stream-silent",
        "live-universe-fallback",
        "tick-spill-quarantined",
        "wal-suspension-probe-blind",
        "deploy-provenance-blind",
        "boot-heartbeat-gate-errors",
        "boot-heartbeat-gate-not-invoked",
    ] {
        if !rows.iter().any(|(s, d)| s == slug && d == "KEEP") {
            wrong.push(format!("{slug}: must have a KEEP row in the review"));
        }
    }
    assert!(
        wrong.is_empty(),
        "review vs terraform:\n{}",
        wrong.join("\n")
    );
}

/// 2026-12-01 00:00 IST in UTC, from first principles, so the literal in
/// main.tf cannot drift to a wrong instant unnoticed.
fn quote53_cutover_utc() -> String {
    const IST_OFFSET_SECS: i64 = 5 * 3600 + 30 * 60;
    // Seconds from 1970-01-01 to 2026-12-01 00:00 (civil), via days.
    let days_to_2026: i64 = (1970..2026)
        .map(|y| {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                366
            } else {
                365
            }
        })
        .sum();
    let days_to_dec = 334; // Jan..Nov 2026, not a leap year
    let local = (days_to_2026 + days_to_dec) * 86_400;
    let utc = local - IST_OFFSET_SECS;
    let secs_into_day = utc.rem_euclid(86_400);
    // Day of year 304 (0-based) is 1 November; the UTC instant lands on 30 Nov.
    let day = utc.div_euclid(86_400) - days_to_2026 - 304 + 1;
    format!(
        "2026-11-{:02}T{:02}:{:02}:00Z",
        day,
        secs_into_day / 3600,
        (secs_into_day % 3600) / 60
    )
}

#[test]
fn quote53_cutover_instant_is_1_december_ist() {
    assert_eq!(quote53_cutover_utc(), "2026-11-30T18:30:00Z");
}

#[test]
fn terraform_refuses_a_quote53_plan_before_december() {
    let main = code_only(&read(&format!("{TF_DIR}/main.tf")));
    let gate = resource_body(&main, "terraform_data", "quote53_not_before_december").expect(
        "the Quote 53 date gate is gone. terraform-apply.yml applies on every push to \
         main, so without it a November merge applies the alarm cuts in November, \
         outside the owner's \"FROM DECEMBER\".",
    ); // APPROVED: test
    let condition = format!(
        "condition     = var.quote53_early_apply_approved || timecmp(plantimestamp(), \"{}\") >= 0",
        quote53_cutover_utc()
    );
    assert!(
        gate.contains("precondition {"),
        "the gate is not a precondition"
    );
    assert!(
        gate.contains(&condition),
        "the gate condition changed:\n{gate}"
    );
    let vars = code_only(&read(&format!("{TF_DIR}/variables.tf")));
    let var_start = vars
        .find("variable \"quote53_early_apply_approved\" {")
        .expect("the override variable"); // APPROVED: test
    let var_body = &vars[var_start..var_start + vars[var_start..].find('}').unwrap_or(0)];
    // 2026-11-11: the owner tapped "Apply now (Recommended)", recorded under
    // Quote 53 as "Quote 53 early go-ahead". `true` is allowed only while that
    // dated record is in the rule file; without it the default must be false.
    let early = var_body.contains("default     = true");
    let recorded =
        read("docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md")
            .contains("##### Quote 53 early go-ahead (2026-11-11");
    assert!(
        var_body.contains("default     = false") || (early && recorded),
        "the early-apply override must default to false; true needs the owner's \
         dated go-ahead recorded under Quote 53 first"
    );
}

/// The pre-December metric names: every name here is read by an alarm that
/// stays live until the December terraform apply removes it, so each must
/// still ship while terraform is gated. (Host metrics first, then EMF names,
/// then the per-class lag series.)
const SHIPPED_UNTIL_DECEMBER: &[&str] = &[
    "mem_used_percent",
    "cpu_usage_active",
    "tv_dhan_ws_worst_conn_tick_age_secs",
    "tv_dhan_ws_main_reconnect_recovery_max_ms",
    "tv_dhan_feed_ring_dwell_max_ms",
    "tv_questdb_wal_apply_lag_max",
    "tv_clock_skew_seconds",
    "tv_orders_rejected_total",
    "tv_dhan_ws_lag_max_ms_by_class",
];

/// True when the agent config text no longer ships one of the names live
/// alarms read until December, i.e. the Quote 53 metric trim is in it.
fn agent_is_trimmed(agent: &str) -> bool {
    SHIPPED_UNTIL_DECEMBER.iter().any(|n| !agent.contains(n))
}

/// Whether the trimmed agent config may be on main at `now_unix_secs`:
/// only on or after 2026-12-01 00:00 IST, or with the owner's dated early
/// go-ahead (the same `quote53_early_apply_approved` switch as terraform).
fn trimmed_agent_may_be_on_main(now_unix_secs: i64, early_approved: bool) -> bool {
    // 2026-11-30T18:30:00Z (pinned against `quote53_cutover_utc` below).
    const CUTOVER_UNIX: i64 = 1_796_063_400;
    early_approved || now_unix_secs >= CUTOVER_UNIX
}

#[test]
fn the_agent_trim_cutover_is_the_terraform_cutover() {
    // 2026-11-30T18:30:00Z as civil-date arithmetic, so the two literals agree.
    let days: i64 = (1970..2026)
        .map(|y| if y % 4 == 0 { 366 } else { 365 })
        .sum::<i64>()
        + 333; // 1 Jan .. 30 Nov 2026 (2026 is not a leap year)
    let utc = days * 86_400 + 18 * 3600 + 30 * 60;
    assert_eq!(quote53_cutover_utc(), "2026-11-30T18:30:00Z");
    assert!(!trimmed_agent_may_be_on_main(utc - 1, false));
    assert!(trimmed_agent_may_be_on_main(utc, false));
    assert!(trimmed_agent_may_be_on_main(utc - 86_400 * 20, true));
    // The detector: the pre-Quote-53 config ships every name, the trim does not.
    let wide = SHIPPED_UNTIL_DECEMBER.join("|");
    assert!(!agent_is_trimmed(&wide));
    assert!(agent_is_trimmed(&wide.replace("mem_used_percent", "")));
}

/// Test 9. The metric half of Quote 53 has no terraform in its path:
/// `deploy-aws.yml` copies `cloudwatch-agent.json` to the box on every merge
/// that touches it, and every morning boot copies the repo copy too. So the
/// terraform date gate cannot hold it back; only keeping the trimmed file off
/// main before 1 December can. Until then this test fails the build (All
/// Green) while the trimmed config is in the tree, unless the owner's dated
/// early go-ahead set `quote53_early_apply_approved` true. Delete it with
/// test 8 in the first change after 2026-12-01.
#[test]
fn the_trimmed_agent_config_is_not_merged_before_december() {
    let agent = read(AGENT);
    if !agent_is_trimmed(&agent) {
        return;
    }
    let vars = code_only(&read(&format!("{TF_DIR}/variables.tf")));
    let var_start = vars
        .find("variable \"quote53_early_apply_approved\" {")
        .expect("the override variable"); // APPROVED: test
    let var_body = &vars[var_start..var_start + vars[var_start..].find('}').unwrap_or(0)];
    let early = var_body.contains("default     = true");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970") // APPROVED: test
        .as_secs();
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    assert!(
        trimmed_agent_may_be_on_main(now, early),
        "deploy/aws/cloudwatch-agent.json carries the Quote 53 metric trim, and it is \
         before 1 December 2026 IST. deploy-aws.yml would push it to the box on merge \
         (and the morning boot copies it), with no date gate, while the terraform gate \
         keeps every alarm that reads the trimmed metrics alive; those alarms would sit \
         green on missing data all of November. Merge on or after 1 December 2026 IST, \
         or record the owner's dated early go-ahead under Quote 53 and set \
         quote53_early_apply_approved true."
    );
}

/// Test 10. `wal-replay-restore-failed` was removed, but its failure is a data-loss
/// path: a deferred segment that cannot be moved back stays in `replaying/`,
/// where the confirm step archives it unread. Its only page now is the coded
/// `WS-SPILL-02` ERROR at that site, matched by the kept `errcode-ws-spill-02`
/// filter. This pins all three links so narrowing the filter (by `source`,
/// as §2.17 does elsewhere) cannot silently drop it, and the review row
/// names that cover instead of claiming nothing is lost.
#[test]
fn the_wal_restore_failure_still_pages_through_ws_spill_02() {
    assert_eq!(
        tickvault_common::error_code::ErrorCode::WsSpill02FrameDropped.code_str(),
        "WS-SPILL-02"
    );
    // (a) The emit site logs that code at ERROR.
    let spill = read("crates/storage/src/ws_frame_spill.rs");
    let msg = spill
        .find("WAL replay could not restore a DEFERRED staged segment")
        .expect("the restore-failure line in ws_frame_spill.rs"); // APPROVED: test
    let call = spill[..msg].rfind("error!(").expect("an error! call"); // APPROVED: test
    assert!(
        spill[call..msg].contains("code = ErrorCode::WsSpill02FrameDropped.code_str()"),
        "the WAL restore-failure line must stay a coded WS-SPILL-02 ERROR: it is the \
         only page left for this data-loss path"
    );
    // (b) The kept filter matches every WS-SPILL-02 ERROR, with no narrowing.
    let codes = code_only(&read(&format!("{TF_DIR}/error-code-alarms.tf")));
    let key = codes
        .find("\"ws-spill-02\" = {")
        .expect("the errcode-ws-spill-02 alarm"); // APPROVED: test
    let entry = &codes[key..key + codes[key..].find("\n    }").unwrap_or(0)];
    assert!(
        entry.contains(
            "pattern     = \"{ $.code = \\\"WS-SPILL-02\\\" && $.level = \\\"ERROR\\\" }\""
        ),
        "errcode-ws-spill-02 must match every WS-SPILL-02 ERROR (no source filter): \
         it is the only page for a WAL segment archived unread after a failed \
         restore\n{entry}"
    );
    // (c) The review row names the real cover.
    let review = read(REVIEW);
    let row = review
        .lines()
        .find(|l| l.starts_with("| `tv-<env>-wal-replay-restore-failed`"))
        .expect("the review row"); // APPROVED: test
    assert!(
        row.contains("errcode-ws-spill-02") && !row.contains("nothing deleted"),
        "the wal-replay-restore-failed review row must name errcode-ws-spill-02 as \
         its cover and must not claim nothing is lost:\n{row}"
    );
}

/// Test 11. `boot-heartbeat-missing` (KEPT, live lane) ships disarmed and only the
/// boot-heartbeat gate arms it. Without the gate's own Errors and did-it-run
/// alarms, a failing or dropped gate leaves the morning page off with nothing
/// saying so; the first page would be the liveness page after 09:20 IST.
#[test]
fn the_kept_boot_page_has_watchers_on_the_gate_that_arms_it() {
    let code = all_tf_code();
    let page = alarm(&code, "boot_heartbeat_missing").expect("boot-heartbeat-missing is KEPT"); // APPROVED: test
    assert!(
        page.contains("actions_enabled = false"),
        "the precondition changed: boot-heartbeat-missing no longer ships disarmed, \
         so re-check whether the gate alarms are still its only blind detectors"
    );
    for (name, metric) in [
        (
            "boot_heartbeat_gate_errors",
            "metric_name         = \"Errors\"",
        ),
        (
            "boot_heartbeat_gate_not_invoked",
            "metric_name         = \"Invocations\"",
        ),
    ] {
        let body = alarm(&code, name).unwrap_or_else(|| {
            panic!("{name} must stay: it watches the gate that arms the kept boot page")
        }); // APPROVED: test
        assert!(
            body.contains(metric),
            "{name} reads the wrong metric:\n{body}"
        );
        assert!(
            body.contains(
                "FunctionName = aws_lambda_function.tv_boot_heartbeat_gate.function_name"
            ),
            "{name} must watch the boot-heartbeat gate Lambda"
        );
        assert!(
            body.contains("alarm_actions = [aws_sns_topic.tv_alerts.arn]")
                || body.contains("alarm_actions   = [aws_sns_topic.tv_alerts.arn]"),
            "{name} must page"
        );
    }
}
