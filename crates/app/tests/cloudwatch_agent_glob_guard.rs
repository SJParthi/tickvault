//! Never-again ratchet for the 2026-07-06 17:19 IST log-shipper incident:
//! the app moved its machine logs to `data/logs/machine/` (the "one human
//! log file" reorg, observability.rs `MACHINE_LOGS_DIR`) while the on-box
//! CloudWatch agent kept tailing the OLD top-level globs — the app stayed
//! healthy, the shipper went silently dead, and `/tickvault/prod/app`
//! ingested NOTHING with zero operator signal.
//!
//! This guard fails the build if:
//!
//!  1. The reference agent config (`deploy/aws/cloudwatch-agent.json`)
//!     collect_list globs drift from the Rust observability constants
//!     (`MACHINE_LOGS_DIR` / `APP_LOG_PREFIX` / `ERRORS_JSONL_PREFIX`) —
//!     the exact drift class of the incident.
//!  2. The DEPLOYED inline config (`user-data.sh.tftpl`) drifts from the
//!     reference config's app-log globs.
//!  3. Any collect_list glob regresses to the dead top-level
//!     `data/logs/` dir (without `machine/`).
//!  4. The stream names (`{instance_id}/errors-jsonl` + `{instance_id}/app`)
//!     change — dashboards + the deploy smoke check key on them.
//!  5. The `app_log_ingestion_silent` alarm (log-retention.tf) loses its
//!     detection model (`IncomingLogEvents` + `treat_missing_data =
//!     "breaching"` + `actions_enabled = false`) or falls out of the
//!     market-hours gate Lambda's ALARM_NAMES join.
//!  6. The deploy workflow loses the LOG-INGESTION-SMOKE step (or oidc.tf
//!     loses the `logs:FilterLogEvents` grant it needs).
//!  7. The market-hours gate Lambda loses its weekday-NSE-holiday safety
//!     (2026-07-07 round-1 review fix): holiday-gate.sh SELF-STOPS the box
//!     on weekday NSE holidays while the gate's open cron is holiday-blind
//!     MON-FRI — a blind 09:20 enable + OK reset would false-page both
//!     breaching-on-missing gated alarms (~09:25 / ~09:35 IST) every
//!     weekday holiday. The open path must verify the instance is up
//!     (ec2:DescribeInstances, fail-open) before enabling.
//!  8. Any link of the holiday-stop MARKER chain breaks (2026-07-07 round-3
//!     review fix): the round-1 single instance-state sample was RACY —
//!     two holiday-blind self-healers (start-watchdog 08:45 IST check,
//!     aws-autopilot every 15 min) kept restarting the holiday-stopped box
//!     all day, and a 1-3 min up-burst bracketing the 09:20 sample restored
//!     the false page. holiday-gate.sh now stamps today's IST date into the
//!     /tickvault/<env>/holiday-stop-date SSM param BEFORE its stop; both
//!     restarters skip their self-start on marker == today (the war ends at
//!     the source), and the gate Lambda checks the marker FIRST (race-proof)
//!     with the instance-state sample as the second line.

#![cfg(test)]

use std::path::PathBuf;

use tickvault_app::observability::{APP_LOG_PREFIX, ERRORS_JSONL_PREFIX, MACHINE_LOGS_DIR};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/app parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display())) // APPROVED: test
}

/// The two app-log globs, DERIVED from the Rust observability constants so
/// a rename of `MACHINE_LOGS_DIR` / either prefix breaks this guard instead
/// of silently killing the shipper. The `.2*` suffix pins the hourly
/// `errors.jsonl.YYYY-MM-DD-HH` / `app.YYYY-MM-DD-HH` files while EXCLUDING
/// the `errors.jsonl` compat symlink and the 0-byte `app.log` placeholder.
fn expected_errors_glob() -> String {
    format!("/opt/tickvault/{MACHINE_LOGS_DIR}/{ERRORS_JSONL_PREFIX}.2*")
}

fn expected_app_glob() -> String {
    format!("/opt/tickvault/{MACHINE_LOGS_DIR}/{APP_LOG_PREFIX}.2*")
}

/// Every collect_list entry of an agent config body, as
/// `(file_path, log_stream_name)` pairs, pulled from the parsed JSON in
/// the reference config. For the tftpl (not valid JSON because of the
/// `$${ENVIRONMENT}` escapes) use [`tftpl_file_paths`] instead.
fn reference_collect_list() -> Vec<(String, String)> {
    let body = read("deploy/aws/cloudwatch-agent.json");
    let json: serde_json::Value =
        serde_json::from_str(&body).expect("cloudwatch-agent.json must parse as JSON"); // APPROVED: test
    let list = json["logs"]["logs_collected"]["files"]["collect_list"]
        .as_array()
        .expect("logs.logs_collected.files.collect_list must be an array"); // APPROVED: test
    list.iter()
        .map(|entry| {
            (
                entry["file_path"]
                    .as_str()
                    .expect("collect_list entry file_path") // APPROVED: test
                    .to_string(),
                entry["log_stream_name"]
                    .as_str()
                    .expect("collect_list entry log_stream_name") // APPROVED: test
                    .to_string(),
            )
        })
        .collect()
}

/// Line-scan the `"file_path":` values out of the user-data template's
/// inline agent config (the file `amazon-cloudwatch-agent-ctl` actually
/// loads on the box). Only `"file_path"` lines are considered, so comments
/// / mkdir lines mentioning `data/logs` can never false-fail the guard.
fn tftpl_file_paths() -> Vec<String> {
    let body = read("deploy/aws/terraform/user-data.sh.tftpl");
    body.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("\"file_path\":")?;
            let start = rest.find('"')? + 1;
            let end = rest[start..].find('"')? + start;
            Some(rest[start..end].to_string())
        })
        .collect()
}

/// Collapse whitespace runs so terraform-body pins survive `terraform fmt`
/// realignment (`treat_missing_data  = "..."` vs `treat_missing_data = "..."`).
fn normalized(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract one `resource "<type>" "<name>" { ... }` block from a terraform
/// body (everything from the resource header to the next `resource ` header
/// or EOF — coarse but sufficient for containment pins).
fn terraform_resource_block<'a>(body: &'a str, header: &str) -> &'a str {
    let start = body
        .find(header)
        .unwrap_or_else(|| panic!("terraform block `{header}` not found")); // APPROVED: test
    let rest = &body[start..];
    match rest[header.len()..].find("\nresource ") {
        Some(end) => &rest[..header.len() + end],
        None => rest,
    }
}

// ---------------------------------------------------------------------------
// 1. Reference config globs == observability constants
// ---------------------------------------------------------------------------

#[test]
fn test_reference_agent_config_globs_match_observability_constants() {
    let entries = reference_collect_list();
    let mut paths: Vec<String> = entries.iter().map(|(p, _)| p.clone()).collect();
    paths.sort();
    let mut expected = vec![expected_errors_glob(), expected_app_glob()];
    expected.sort();
    assert_eq!(
        paths, expected,
        "NEVER-AGAIN ratchet (2026-07-06 shipper incident): the collect_list \
         file_path globs in deploy/aws/cloudwatch-agent.json must be EXACTLY the \
         two constants-derived machine-dir globs. If observability.rs moved the \
         machine log dir or renamed a prefix, update the agent config in the SAME \
         PR — a drift here means the app logs healthy while CloudWatch ingests \
         nothing."
    );
}

// ---------------------------------------------------------------------------
// 2. Deployed inline config (user-data.sh.tftpl) == reference config
// ---------------------------------------------------------------------------

#[test]
fn test_userdata_inline_config_globs_match_reference() {
    // The tftpl also ships /var/log/messages to the system group — that
    // entry is allowlisted; the remaining app-log globs must byte-match the
    // reference config (which test 1 pins to the observability constants).
    let mut app_paths: Vec<String> = tftpl_file_paths()
        .into_iter()
        .filter(|p| p != "/var/log/messages")
        .collect();
    app_paths.sort();
    let mut expected = vec![expected_errors_glob(), expected_app_glob()];
    expected.sort();
    assert_eq!(
        app_paths, expected,
        "Z+ L3 RECONCILE drift-guard: user-data.sh.tftpl is the config the box \
         ACTUALLY loads (`amazon-cloudwatch-agent-ctl -a fetch-config`); its \
         app-log collect_list globs drifted from deploy/aws/cloudwatch-agent.json \
         / the observability constants. The 2026-07-06 incident was exactly this \
         file tailing globs the app no longer writes."
    );
}

// ---------------------------------------------------------------------------
// 3. No glob may regress to the dead top-level data/logs/ dir
// ---------------------------------------------------------------------------

#[test]
fn test_no_collect_list_glob_points_at_top_level_logs_dir() {
    let reference_paths: Vec<String> = reference_collect_list()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    for (source, paths) in [
        ("deploy/aws/cloudwatch-agent.json", reference_paths),
        (
            "deploy/aws/terraform/user-data.sh.tftpl",
            tftpl_file_paths(),
        ),
    ] {
        for path in paths {
            if !path.contains("data/logs") {
                continue; // /var/log/messages etc.
            }
            assert!(
                path.contains("data/logs/machine/"),
                "NEVER-AGAIN ratchet: {source} collect_list glob `{path}` points at \
                 the TOP-LEVEL data/logs/ dir. That level is launcher-owned (human \
                 log + symlink only, frozen since the 2026-07-05 machine/ reorg) — \
                 the app's machine sinks live under data/logs/machine/ \
                 (observability.rs MACHINE_LOGS_DIR). A top-level glob is the exact \
                 dead-shipper regression of 2026-07-06."
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Stream names are stable (dashboards + LOG-INGESTION-SMOKE key on them)
// ---------------------------------------------------------------------------

#[test]
fn test_stream_names_are_stable() {
    // Reference config: pin the glob → stream-name pairing.
    for (path, stream) in reference_collect_list() {
        let expected_stream = if path == expected_errors_glob() {
            "{instance_id}/errors-jsonl"
        } else if path == expected_app_glob() {
            "{instance_id}/app"
        } else {
            panic!("unexpected collect_list glob `{path}` — test 1 should have caught this") // APPROVED: test
        };
        assert_eq!(
            stream, expected_stream,
            "stream-name drift for glob `{path}` in cloudwatch-agent.json — the \
             CloudWatch dashboards and the deploy LOG-INGESTION-SMOKE poll key on \
             the historical stream names"
        );
    }
    // Deployed config: both stream names must survive verbatim.
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    for stream in ["{instance_id}/errors-jsonl", "{instance_id}/app"] {
        assert!(
            user_data.contains(&format!("\"log_stream_name\": \"{stream}\"")),
            "user-data.sh.tftpl lost the stable log stream name `{stream}`"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. The ingestion-silence alarm exists, keeps its detection model, and is
//    gated by the market-hours Lambda (log-retention.tf + gate join pins)
// ---------------------------------------------------------------------------

#[test]
fn test_app_log_ingestion_silent_alarm_pinned_in_terraform() {
    let tf = read("deploy/aws/terraform/log-retention.tf");
    let block = terraform_resource_block(
        &tf,
        "resource \"aws_cloudwatch_metric_alarm\" \"app_log_ingestion_silent\"",
    );
    let norm = normalized(block);
    for (pin, why) in [
        (
            "metric_name = \"IncomingLogEvents\"",
            "the alarm must watch INGESTION COUNT (the literal predicate: zero \
             events shipped), not a value-based app metric",
        ),
        (
            "treat_missing_data = \"breaching\"",
            "AWS/Logs publishes NO IncomingLogEvents datapoint for a zero-ingestion \
             period — silence IS missing data; anything but `breaching` blinds the \
             alarm to the exact 2026-07-06 failure",
        ),
        (
            "actions_enabled = false",
            "actions must start OFF — the market-hours gate Lambda flips them ON \
             09:20-15:35 IST so the intentional nightly/weekend box stop can never \
             false-page",
        ),
        (
            "comparison_operator = \"LessThanThreshold\"",
            "silence detection = Sum(IncomingLogEvents) < 1",
        ),
        (
            "LogGroupName = aws_cloudwatch_log_group.tv_app.name",
            "the alarm must be dimensioned on the APP log group, not the \
             account-wide aggregate",
        ),
    ] {
        assert!(
            norm.contains(pin),
            "log-retention.tf app_log_ingestion_silent alarm lost `{pin}` — {why}.\n\
             Block was:\n{block}"
        );
    }
}

#[test]
fn test_alarm_is_gated_by_market_hours_lambda() {
    let tf = read("deploy/aws/terraform/market-hours-liveness-alarm.tf");
    let join_start = tf
        .find("ALARM_NAMES = join(")
        .expect("market-hours-liveness-alarm.tf must carry the ALARM_NAMES join"); // APPROVED: test
    let rest = &tf[join_start..];
    let join_end = rest
        .find("])")
        .expect("ALARM_NAMES join must close with `])`"); // APPROVED: test
    let join_body = &rest[..join_end];
    assert!(
        join_body.contains("aws_cloudwatch_metric_alarm.app_log_ingestion_silent.alarm_name"),
        "gate-membership rot: app_log_ingestion_silent fell out of the market-hours \
         gate Lambda's ALARM_NAMES join. With treat_missing_data=breaching and \
         actions permanently disabled (never re-enabled at 09:20 IST), the alarm \
         can NEVER page — it exists but is dead. Join body was:\n{join_body}"
    );
}

/// Round-1 review fix (2026-07-07): the gate Lambda's OPEN mode must be
/// weekday-NSE-holiday safe. `deploy/aws/holiday-gate.sh` self-stops the box
/// at boot on a definitive holiday verdict, while the gate's open cron is a
/// holiday-blind plain MON-FRI schedule — so a blind enable + OK reset at
/// 09:20 IST would drive both breaching-on-missing gated alarms
/// (`market_hours_liveness_missing` ~09:25, `app_log_ingestion_silent`
/// ~09:35) into a false SNS/Telegram page on EVERY weekday NSE holiday.
/// The open path must first verify the tv-app instance is up, and must
/// fail OPEN on an EC2 API error so a real trading day never loses the page.
#[test]
fn test_gate_lambda_open_is_holiday_safe() {
    // 2026-07-18 (rust-only phase 2b-1): the gate Lambda's Python heredoc was
    // PORTED to Rust (crates/aws-lambdas/src/market_hours_gate.rs) — the
    // env-var + IAM pins stay in the tf; the LOGIC pins repoint to the Rust
    // source (the budget_killswitch_wiring.rs repoint precedent).
    let tf = read("deploy/aws/terraform/market-hours-liveness-alarm.tf");
    let norm = normalized(&tf);
    for (pin, why) in [
        (
            "EC2_INSTANCE_ID = aws_instance.tv_app.id",
            "the gate Lambda must know WHICH instance to check — the cycle-free \
             env-var pattern proven by start-watchdog-lambda.tf",
        ),
        (
            "\"ec2:DescribeInstances\"",
            "the gate role must carry the DescribeInstances grant or the check \
             throws AccessDenied every open (and fail-open would blindly enable, \
             silently restoring the holiday false-page)",
        ),
    ] {
        assert!(
            norm.contains(pin),
            "market-hours-liveness-alarm.tf lost the holiday-safety pin `{pin}` — {why}. \
             Regressing this restores the weekday-NSE-holiday false page \
             (box self-stopped by holiday-gate.sh + holiday-blind MON-FRI open cron \
             + treat_missing_data=breaching)."
        );
    }
    let rs = read("crates/aws-lambdas/src/market_hours_gate.rs");
    for (pin, why) in [
        (
            ".describe_instances()",
            "the open path must ask EC2 whether the box is up before enabling \
             the breaching-on-missing alarms (holiday self-stop = box OFF)",
        ),
        (
            "fail-open, treating as up",
            "an EC2 API error must NEVER suppress the trading-day liveness page \
             — the check fails open, exactly mirroring holiday-gate.sh",
        ),
        (
            "leaving actions disabled",
            "a not-up box (NSE holiday self-stop / operator manual stop) must \
             skip BOTH the enable and the OK reset",
        ),
    ] {
        assert!(
            rs.contains(pin),
            "market_hours_gate.rs lost the holiday-safety pin `{pin}` — {why}. \
             Regressing this restores the weekday-NSE-holiday false page."
        );
    }
    // The enable call must come AFTER the instance-up guard in the Lambda
    // source (source-order scan, house pattern).
    let guard_pos = rs
        .find("if !instance_up")
        .expect("gate Lambda must carry the instance-up holiday guard arm"); // APPROVED: test
    let enable_pos = rs
        .find(".enable_alarm_actions()")
        .expect("gate Lambda must still enable alarm actions on open"); // APPROVED: test
    assert!(
        guard_pos < enable_pos,
        "the instance-up guard must run BEFORE enable_alarm_actions — enabling \
         first re-opens the holiday false-page window"
    );
}

/// Round-3 review fix (2026-07-07): the round-1 single 09:20 IST
/// instance-state sample is RACY — holiday-blind restarters (start-watchdog
/// 08:45 self-start + aws-autopilot every 15 min, incl. the 03:45 UTC ≈
/// 09:15 IST slot + GH cron jitter) fight holiday-gate.sh's self-stop all
/// day, and a 1-3 min up-burst bracketing the 09:20 sample re-arms the
/// breaching-on-missing alarms → the exact holiday false page. The gate
/// Lambda's open path must therefore consult the race-proof
/// holiday-stop-date SSM marker (stamped by holiday-gate.sh BEFORE its
/// stop) FIRST, with the instance-state sample as the second line for the
/// marker-less manual-stop case, and must fail OPEN on any SSM error.
#[test]
fn test_gate_lambda_open_checks_holiday_marker_first() {
    // 2026-07-18 (rust-only phase 2b-1): env-var + IAM pins stay in the tf;
    // the marker-read LOGIC pins repoint to the Rust port
    // (crates/aws-lambdas/src/market_hours_gate.rs).
    let tf = read("deploy/aws/terraform/market-hours-liveness-alarm.tf");
    let norm = normalized(&tf);
    for (pin, why) in [
        (
            "HOLIDAY_STOP_PARAM = \"/tickvault/${var.environment}/holiday-stop-date\"",
            "the gate Lambda must be told WHERE holiday-gate.sh stamps the \
             intentional-stop marker",
        ),
        (
            "parameter/tickvault/${var.environment}/holiday-stop-date",
            "the gate role must carry the ssm:GetParameter grant on the marker \
             param or every open throws AccessDenied and the fail-open arm \
             silently degrades back to the racy instance-state-only check",
        ),
    ] {
        assert!(
            norm.contains(pin),
            "market-hours-liveness-alarm.tf lost the round-3 marker pin `{pin}` — {why}. \
             Regressing this restores the raced holiday false page (restart-war \
             up-burst bracketing the single 09:20 instance-state sample)."
        );
    }
    let rs = read("crates/aws-lambdas/src/market_hours_gate.rs");
    for (pin, why) in [
        (
            ".get_parameter().name(&holiday_param)",
            "the open path must READ the marker — marker == today is the only \
             race-proof 'intentionally stopped today' signal",
        ),
        (
            "fail-open, not a holiday",
            "an SSM error / missing marker must NEVER suppress the trading-day \
             liveness page — the marker check fails open",
        ),
        (
            "holiday_marker_matches_today",
            "the marker compare must go through the unit-tested pure fn \
             (trim + exact-today match; stale markers never match)",
        ),
    ] {
        assert!(
            rs.contains(pin),
            "market_hours_gate.rs lost the round-3 marker pin `{pin}` — {why}."
        );
    }
    // Source order: marker check BEFORE the instance-up guard BEFORE enable
    // (the pure open_decision fn + the handle's decision match).
    let marker_pos = rs
        .find("if holiday_stop_today")
        .expect("gate Lambda open path must carry the holiday-marker guard arm"); // APPROVED: test
    let up_guard_pos = rs
        .find("if !instance_up")
        .expect("gate Lambda must keep the round-1 instance-up guard arm"); // APPROVED: test
    let enable_pos = rs
        .find(".enable_alarm_actions()")
        .expect("gate Lambda must still enable alarm actions on open"); // APPROVED: test
    assert!(
        marker_pos < up_guard_pos && up_guard_pos < enable_pos,
        "gate Lambda open ordering must be marker-check -> instance-up-check -> \
         enable; the marker is race-proof, the instance sample is not — \
         checking the sample first (or enabling first) re-opens the raced \
         holiday false-page window"
    );
    // 2026-07-18 (hostile-review r1 F2/M4): the pure-fn refactor made the
    // plain token-order scan above VACUOUS on its own — the `if
    // holiday_stop_today` / `if !instance_up` tokens live in the pure
    // `open_decision` fn near the TOP of the file, so an unconditional
    // `.enable_alarm_actions()` inserted in handle() right after client
    // construction still ordered AFTER them and passed. Strengthened:
    // (i) exactly ONE enable occurrence in the whole file (the close path
    //     uses disable_alarm_actions) — an inserted second enable fails;
    // (ii) the enable sits AFTER handle()'s `let decision = open_decision(`
    //      call site — a moved/unconditional enable before the decision
    //      fails;
    // (iii) the NEAREST PRECEDING `OpenDecision::Enable =>` match-arm token
    //       also sits after that call site — the enable can only live
    //       inside the handle() decision match's Enable arm (the first
    //       `OpenDecision::Enable =>` in the file belongs to the
    //       open_result renderer and must NOT satisfy this).
    let enable_count = rs.matches(".enable_alarm_actions()").count();
    assert_eq!(
        enable_count, 1,
        "market_hours_gate.rs must carry exactly ONE .enable_alarm_actions() \
         call (inside the OpenDecision::Enable match arm) — any additional \
         occurrence is an unconditional-enable mutation re-opening the \
         holiday false-page window"
    );
    let decision_call_pos = rs
        .find("let decision = open_decision(")
        .expect("handle() must route the open path through the pure open_decision fn"); // APPROVED: test
    assert!(
        decision_call_pos < enable_pos,
        "the single .enable_alarm_actions() call must come AFTER handle()'s \
         `let decision = open_decision(` call site — enabling before the \
         decision is computed re-opens the holiday false-page window"
    );
    let enable_arm_pos = rs[..enable_pos]
        .rfind("OpenDecision::Enable =>")
        .expect("the enable call must be preceded by an OpenDecision::Enable match arm"); // APPROVED: test
    assert!(
        decision_call_pos < enable_arm_pos,
        "the OpenDecision::Enable arm guarding .enable_alarm_actions() must \
         belong to handle()'s decision match (after `let decision = \
         open_decision(`), not the open_result renderer earlier in the file \
         — otherwise the enable is not gated by the computed decision"
    );
}

/// Round-3 review fix (2026-07-07): the holiday-stop marker chain. Every
/// link must survive — losing ANY one restores the holiday restart war:
///   writer:   holiday-gate.sh stamps the marker BEFORE stop-instances
///   reader 1: start-watchdog mode=check skips self-start on marker==today
///             (also kills the pre-existing false Critical "auto-start
///             FAILED" page every weekday holiday)
///   reader 2: aws-autopilot.sh skips its up-window self-start on
///             marker==today
///   IAM:      start-watchdog role + the GitHub OIDC deploy role (autopilot)
///             can ssm:GetParameter the marker param
#[test]
fn test_holiday_stop_marker_chain_is_wired() {
    // --- writer: holiday-gate.sh, marker put BEFORE the stop ---------------
    let gate = read("deploy/aws/holiday-gate.sh");
    assert!(
        gate.contains("holiday-stop-date"),
        "holiday-gate.sh lost the holiday-stop-date marker write — the \
         intentional stop leaves no trace and the holiday-blind restarters \
         (start-watchdog + aws-autopilot) fight it all day"
    );
    let put_pos = gate
        .find("ssm put-parameter")
        .expect("holiday-gate.sh must write the marker via `aws ssm put-parameter`"); // APPROVED: test
    let stop_pos = gate
        .find("ec2 stop-instances")
        .expect("holiday-gate.sh must still stop the instance on a holiday verdict"); // APPROVED: test
    assert!(
        put_pos < stop_pos,
        "holiday-gate.sh must stamp the marker BEFORE stop-instances — writing \
         after (or never) leaves a window where every restarter + the 09:20 \
         gate sample sees an unexplained stop"
    );

    // --- reader 1: start-watchdog handler.py -------------------------------
    let watchdog = read("deploy/aws/lambda/start-watchdog/handler.py");
    for pin in [
        "HOLIDAY_STOP_PARAM",
        "def _holiday_stop_is_today(",
        "\"skipped\": \"holiday_stop\"",
    ] {
        assert!(
            watchdog.contains(pin),
            "start-watchdog handler.py lost `{pin}` — the 08:45 IST check will \
             again self-start the holiday-stopped box AND false-page a Critical \
             'auto-start FAILED' every weekday NSE holiday"
        );
    }
    let marker_call = watchdog
        .find("if _holiday_stop_is_today(_now()):")
        .expect("check mode must consult the marker in its not-running branch"); // APPROVED: test
    let self_start_call = watchdog
        .find("self_started = _try_self_start(ec2_client, EC2_INSTANCE_ID)")
        .expect("check mode must keep its self-heal start for real trading days"); // APPROVED: test
    assert!(
        marker_call < self_start_call,
        "the marker consult must come BEFORE the self-heal start in mode=check \
         — starting first rejoins the holiday restart war"
    );

    // --- reader 2: aws-autopilot.sh ----------------------------------------
    let autopilot = read("scripts/aws-autopilot.sh");
    let ap_marker = autopilot
        .find("holiday-stop-date")
        .expect("aws-autopilot.sh must read the holiday-stop-date marker"); // APPROVED: test
    let ap_start = autopilot
        .find("started EC2 instance (was stopped during the 08:30-16:30 IST up-window)")
        .expect("aws-autopilot.sh must keep its up-window self-start for real trading days"); // APPROVED: test
    assert!(
        ap_marker < ap_start,
        "autopilot must check the marker BEFORE its up-window start-instances — \
         its 15-min cadence (03:45 UTC slot ≈ 09:15 IST + GH cron jitter) is \
         exactly the restarter that bracketed the 09:20 gate sample"
    );

    // --- IAM: both reader roles can GetParameter the marker ----------------
    let watchdog_tf = normalized(&read("deploy/aws/terraform/start-watchdog-lambda.tf"));
    assert!(
        watchdog_tf.contains("parameter/tickvault/${var.environment}/holiday-stop-date"),
        "start-watchdog-lambda.tf lost the marker ssm:GetParameter grant — the \
         fail-open marker read silently degrades and the 08:45 self-start \
         (+ false Critical page) returns every holiday"
    );
    assert!(
        watchdog_tf
            .contains("HOLIDAY_STOP_PARAM = \"/tickvault/${var.environment}/holiday-stop-date\""),
        "start-watchdog-lambda.tf lost the HOLIDAY_STOP_PARAM env var"
    );
    let oidc = normalized(&read("deploy/aws/terraform/oidc.tf"));
    assert!(
        oidc.contains("parameter/tickvault/${var.environment}/holiday-stop-date"),
        "oidc.tf lost the marker ssm:GetParameter grant — autopilot's marker \
         read AccessDenies, fails open, and its 15-min self-start rejoins the \
         holiday restart war"
    );
}

// ---------------------------------------------------------------------------
// 6. Deploy-time smoke check + its IAM grant survive
// ---------------------------------------------------------------------------

#[test]
fn test_deploy_workflow_carries_log_ingestion_smoke() {
    let workflow = read(".github/workflows/deploy-aws.yml");
    assert!(
        workflow.contains("LOG-INGESTION-SMOKE"),
        "deploy-aws.yml lost the LOG-INGESTION-SMOKE step — the per-deploy \
         independent detector for a dead log shipper (the gated alarm alone can \
         be silenced by gate-Lambda EventBridge drift, the #1404 class)"
    );
    assert!(
        workflow.contains("filter-log-events"),
        "deploy-aws.yml LOG-INGESTION-SMOKE must poll `aws logs filter-log-events` \
         for fresh app events after readiness"
    );
    let oidc = read("deploy/aws/terraform/oidc.tf");
    assert!(
        oidc.contains("logs:FilterLogEvents"),
        "oidc.tf lost the `logs:FilterLogEvents` grant — the deploy role can no \
         longer run the LOG-INGESTION-SMOKE poll (it would warn on every deploy)"
    );
}
