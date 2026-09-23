//! CloudWatch-agent metric-selector LOCKSTEP guard + Groww feed-down alarm
//! pins (operator directive 2026-07-06 — Groww feed-down alerting).
//!
//! The cw-agent `metric_selectors` regex exists in TWO tracked copies that
//! MUST stay byte-identical:
//!   1. `deploy/aws/cloudwatch-agent.json` — the LIVE copy, shipped to the
//!      box on EVERY deploy (`deploy-aws.yml` copies it + runs fetch-config)
//!   2. `deploy/aws/terraform/user-data.sh.tftpl` — the first-boot heredoc
//!
//! If the two drift, a fresh instance selects a DIFFERENT metric set than a
//! deployed one — an alarm can silently point at a metric that never reaches
//! CloudWatch (false-OK class, audit-findings Rule 11). This guard fails the
//! build on drift, on the Trap-A liveness heartbeat
//! (`tv_rest_1m_fire_heartbeat`, 2026-07-15 — the re-pointed market-hours
//! liveness alarm's gauge) disappearing from the selector, or on any of the
//! 4 retired Groww-live metric names sneaking back in.

#![cfg(test)]

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Extract ONE terraform resource block by name, bounded at ITS closing
/// brace (a top-level `\n}`) — never a fixed byte window, which overruns
/// into the neighboring resource and lets the neighbor's identical literals
/// satisfy assertions vacuously (the 2026-07-06 mutation-verified hole).
fn resource_block<'a>(tf: &'a str, block_name: &str) -> &'a str {
    let at = tf
        .find(&format!(
            "resource \"aws_cloudwatch_metric_alarm\" \"{block_name}\""
        ))
        .unwrap_or_else(|| panic!("app-alarms.tf: resource {block_name} not found"));
    let end = tf[at..].find("\n}").map_or(tf.len(), |rel| at + rel + 2);
    &tf[at..end]
}

/// Extract the `metric_selectors` regex literal (`"^(...)$"`) from a file.
fn extract_selector_regex(content: &str, name: &str) -> String {
    let marker = "\"metric_selectors\":";
    let at = content
        .find(marker)
        .unwrap_or_else(|| panic!("{name}: no metric_selectors key found"));
    let rest = &content[at + marker.len()..];
    let start = rest
        .find("\"^(")
        .unwrap_or_else(|| panic!("{name}: metric_selectors regex literal not found"));
    let tail = &rest[start + 1..];
    let end = tail
        .find(")$\"")
        .unwrap_or_else(|| panic!("{name}: metric_selectors regex not `)$`-anchored"));
    tail[..end + 2].to_string()
}

/// INVERTED 2026-08-25. There is no second copy any more, and that is the fix.
///
/// This guard used to require the EMF metric-selector alternation in
/// `user-data.sh.tftpl` to stay byte-identical to the one in
/// `deploy/aws/cloudwatch-agent.json`. Keeping two copies in sync is exactly
/// the kind of obligation a ratchet can enforce and a design should not
/// create: the duplicate was ~1.6 KB, and it pushed the user-data template to
/// EXACTLY its 15,872-byte budget with ZERO bytes free — measured, not
/// estimated — which blocked every subsequent addition to the boot script,
/// including the EMF names the observability audit wanted.
///
/// The template now emits a MINIMAL host-only fallback and copies the repo
/// file into place after the Step 5 clone. One copy cannot drift from itself.
///
/// What this test pins is that the arrangement stays that way: no selector
/// re-embedded in the template, and the post-clone install still present. If
/// the install were dropped while the selector stayed absent, every box would
/// silently run host-metrics-only — a far quieter failure than drift ever was.
#[test]
fn user_data_carries_no_second_copy_of_the_selector() {
    let root = repo_root();
    let boot = read(&root.join("deploy/aws/terraform/user-data.sh.tftpl"));

    assert!(
        !boot.contains("\"metric_selectors\""),
        "the EMF metric-selector list is back inside user-data.sh.tftpl. It is ~1.6 KB \
         and the template's byte budget has no room for it (it sat at exactly 0 bytes \
         free before this duplication was removed). The single source of truth is \
         deploy/aws/cloudwatch-agent.json, copied into place after the Step 5 clone."
    );

    // The repo copy must still be the real thing.
    let live = extract_selector_regex(
        &read(&root.join("deploy/aws/cloudwatch-agent.json")),
        "deploy/aws/cloudwatch-agent.json",
    );
    assert!(
        live.starts_with("^(tv_") && live.ends_with(")$"),
        "deploy/aws/cloudwatch-agent.json must still carry the anchored exact-name \
         alternation — a prefix wildcard would publish every metric the app has and \
         the budget auto-stops the box at 90%"
    );

    // And the boot script must actually install it, or the fallback becomes
    // the permanent config and no app metric ever reaches CloudWatch.
    assert!(
        boot.contains("deploy/aws/cloudwatch-agent.json"),
        "user-data must copy the repo CloudWatch agent config after the clone; without \
         it every box runs the host-only fallback and publishes no app metrics at all"
    );
    assert!(
        boot.contains("fetch-config"),
        "the copied config must be applied with amazon-cloudwatch-agent-ctl -a fetch-config, \
         otherwise the file is written and the running agent never reads it"
    );

    // The fallback must keep the signals the budget and disk alarms read — a
    // fallback that reports nothing is the same as no fallback.
    for measurement in ["used_percent", "mem_used_percent", "cpu_usage_active"] {
        assert!(
            boot.contains(measurement),
            "the host-only fallback must still collect `{measurement}` — it is what the \
             budget guard and the disk alarms read when the clone has failed"
        );
    }
}

#[test]
fn cw_agent_selector_ships_heartbeat_and_drops_groww_live_metrics() {
    // 2026-07-15 (Groww live-feed retirement, Trap-A lockstep): the selector
    // must ship the re-pointed liveness alarm's gauge and must NOT resurrect
    // retired names (a dead name in the EMF list implies coverage no producer
    // can ever publish again).
    //
    // ⚠ RE-POINTED 2026-09-17. This guard REQUIRED
    // `tv_rest_1m_fire_heartbeat` — correct until the operator's SOCKETS-ONLY
    // narrowing removed all three of its producers
    // (`no-rest-except-live-feed-2026-06-27.md` §12.10), at which point it was
    // asserting the exact reverse of both of its own rules at once: it
    // DEMANDED an EMF name whose producer is gone (the dead-name clause
    // directly below) while its message warned of a false-SOS the alarm can no
    // longer raise, because that alarm was re-pointed the same day.
    //
    // The RULE is unchanged and is why this was re-pointed rather than
    // deleted: `tv-<env>-market-hours-liveness-missing` is
    // `treat_missing_data = breaching` and is the ONLY ~5-minute detector in
    // the gated set, so whatever gauge it reads MUST be shipped or it pages
    // every gated window, every trading day. Only the gauge moved.
    let root = repo_root();
    let live = extract_selector_regex(
        &read(&root.join("deploy/aws/cloudwatch-agent.json")),
        "deploy/aws/cloudwatch-agent.json",
    );
    assert!(
        live.contains("tv_dhan_feed_last_tick_age_secs"),
        "cw-agent metric_selectors must ship tv_dhan_feed_last_tick_age_secs — the \
         market-hours liveness alarm (treat_missing_data=breaching) is blind \
         without it (false-SOS every gated window, every trading day)"
    );
    for retired in [
        // Lost all three producers with the per-minute REST legs (§12.10);
        // shipping the name would imply coverage nothing can publish.
        "tv_rest_1m_fire_heartbeat",
        "tv_groww_ws_active",
        "tv_feed_last_tick_age_seconds",
        "tv_feed_sidecar_stall_restart_total",
        "tv_groww_exchange_lag_p99_seconds",
        // 2026-07-17 (dashboard tidy): the 2 Dhan lag names retired with
        // the dead Dhan-lag ring/publisher chain in feed_lag_monitor.rs.
        "tv_dhan_exchange_lag_p99_seconds",
        "tv_dhan_lag_samples_excluded_total",
    ] {
        assert!(
            !live.contains(retired),
            "cw-agent metric_selectors resurrected retired live-feed-era metric \
             {retired} (producer deleted — a dead name implies \
             coverage that can never be published again)"
        );
    }
}

// RETIRED (2026-07-15 — Groww live-feed retirement):
// groww_feed_down_alarms_exist_and_are_registered died with the two alarms
// it pinned — groww_ws_inactive + groww_stall_restart_storm left
// app-alarms.tf (their gauge/counter producers, the Groww bridge + sidecar
// stall watchdog, were deleted; dated notes in app-alarms.tf).

#[test]
fn resource_block_extractor_is_bounded_at_own_closing_brace() {
    // Anti-vacuous self-test (2026-07-06): the block extractor must stop at
    // the alarm's OWN closing brace — an overrunning window that swallows
    // the neighboring resource would let the neighbor's identical
    // `treat_missing_data` / `statistic` literals satisfy the assertions
    // after the alarm's own lines were deleted.
    let root = repo_root();
    let tf = read(&root.join("deploy/aws/terraform/app-alarms.tf"));
    for block_name in ["questdb_disconnected", "disk_watcher_respawn"] {
        let block = resource_block(&tf, block_name);
        assert_eq!(
            block
                .matches("resource \"aws_cloudwatch_metric_alarm\"")
                .count(),
            1,
            "{block_name}: extracted block must contain exactly ONE resource \
             declaration (no neighbor overrun): {block}"
        );
        assert!(
            block.trim_end().ends_with('}'),
            "{block_name}: extracted block must end at its own closing brace"
        );
    }
}

#[test]
fn selector_extractor_is_not_vacuous() {
    // Anti-vacuous self-test: the extractor must actually pull a `tv_`-metric
    // alternation, not an empty string (a broken extractor comparing "" == ""
    // would pass the lockstep test forever).
    let root = repo_root();
    let live = extract_selector_regex(
        &read(&root.join("deploy/aws/cloudwatch-agent.json")),
        "deploy/aws/cloudwatch-agent.json",
    );
    assert!(
        live.starts_with("^(") && live.ends_with(")$"),
        "extractor must return the anchored regex, got: {live}"
    );
    assert!(
        live.matches("tv_").count() >= 10,
        "selector regex should carry the tv_ metric alternation, got: {live}"
    );
}

/// The two `collect_list` entries of an agent config, as
/// `(log_stream_name, timestamp_format, timezone)`. A missing key reads as
/// the empty string so the assertions below name it instead of panicking in
/// the extractor.
fn collect_list_time_config(body: &str) -> Vec<(String, String, String)> {
    let json: serde_json::Value =
        serde_json::from_str(body).expect("cloudwatch-agent.json must be valid JSON"); // APPROVED: test
    let list = json["logs"]["logs_collected"]["files"]["collect_list"]
        .as_array()
        .expect("logs.logs_collected.files.collect_list must be an array") // APPROVED: test
        .clone();
    list.iter()
        .map(|entry| {
            let field = |k: &str| entry[k].as_str().unwrap_or("").to_string();
            (
                field("log_stream_name"),
                field("timestamp_format"),
                field("timezone"),
            )
        })
        .collect()
}

/// Every stream that isn't stamped with its OWN event time is a problem.
/// Returns one human-readable offence per bad entry; empty means correct.
fn collect_list_time_offences(body: &str) -> Vec<String> {
    let mut offences = Vec::new();
    for (stream, format, zone) in collect_list_time_config(body) {
        if !format.starts_with("%Y-%m-%dT%H:%M:%S.%f") {
            offences.push(format!(
                "{stream}: timestamp_format `{format}` — must start with %Y-%m-%dT%H:%M:%S.%f"
            ));
        }
        if format.contains("%z") {
            offences.push(format!(
                "{stream}: %z maps to [+-]HHMM and can never match app.log's `+05:30`"
            ));
        }
        let expected_zone = if stream.ends_with("/errors-jsonl") {
            "UTC"
        } else if stream.ends_with("/app") {
            "Local"
        } else {
            ""
        };
        if expected_zone.is_empty() {
            offences.push(format!(
                "{stream}: unknown stream — decide its timezone here"
            ));
        } else if zone != expected_zone {
            offences.push(format!(
                "{stream}: timezone `{zone}`, expected `{expected_zone}`"
            ));
        }
    }
    offences
}

/// ADDED 2026-09-23 (dhan-rest-only-noise-lock-2026-07-14.md §2.3x).
///
/// With no `timestamp_format`, the agent stamps each log event with the
/// moment it READ the line, not the moment the line was written. A backlog
/// read at boot (the `*.2*` globs match yesterday's rotated files too) then
/// arrives in CloudWatch as a burst of "new" coded errors, and every metric
/// filter on the group counts them into the current window — which is how
/// yesterday's errors paged the operator before the open on 2026-09-23.
///
/// `errors.jsonl` writes UTC with a trailing `Z`, so it is parsed as UTC.
/// `app.log` writes IST with `+05:30` (IstTimer, `%:z`). The agent's `%z`
/// only matches `+0530`, so the offset cannot be parsed; the stream is
/// read as `Local` instead, which is correct only while the box stays on
/// Asia/Kolkata (set by user-data). That dependency is Assumed, not
/// enforced here.
#[test]
fn both_log_streams_are_stamped_with_their_own_event_time() {
    let root = repo_root();
    let body = read(&root.join("deploy/aws/cloudwatch-agent.json"));
    let entries = collect_list_time_config(&body);
    assert_eq!(
        entries.len(),
        2,
        "expected exactly the errors-jsonl and app streams, got {entries:?}"
    );
    let offences = collect_list_time_offences(&body);
    assert!(
        offences.is_empty(),
        "cloudwatch-agent.json log streams are not stamped with event time — a boot \
         backlog would replay old errors into today's alarm windows: {offences:#?}"
    );
}

#[test]
fn the_event_time_check_bites_on_a_missing_or_wrong_format() {
    let fixture = r#"{"logs":{"logs_collected":{"files":{"collect_list":[
        {"log_stream_name":"{instance_id}/errors-jsonl","timezone":"UTC"},
        {"log_stream_name":"{instance_id}/app","timestamp_format":"%Y-%m-%dT%H:%M:%S.%f%z","timezone":"UTC"}
    ]}}}}"#;
    let offences = collect_list_time_offences(fixture);
    assert_eq!(
        offences.len(),
        3,
        "missing format, %z, and the wrong app timezone must all be reported: {offences:#?}"
    );
    let good = r#"{"logs":{"logs_collected":{"files":{"collect_list":[
        {"log_stream_name":"{instance_id}/errors-jsonl","timestamp_format":"%Y-%m-%dT%H:%M:%S.%f","timezone":"UTC"},
        {"log_stream_name":"{instance_id}/app","timestamp_format":"%Y-%m-%dT%H:%M:%S.%f","timezone":"Local"}
    ]}}}}"#;
    assert!(collect_list_time_offences(good).is_empty());
}
