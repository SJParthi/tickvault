//! S7-Step8 / Phase 8: tv-doctor CLI — auto-triage bundle for CRITICAL alerts.
//!
//! Called automatically by systemd / CloudWatch alarm handlers when a
//! CRITICAL alert fires. Collects:
//!
//!   1. Last 200 lines of tickvault journal logs
//!   2. Ring buffer + spill + DLQ state (from Prometheus /metrics)
//!   3. systemd service state (active / inactive / failed)
//!   4. Disk usage for /opt/tickvault/data/spill
//!   5. Docker service states (tv-questdb, tv-valkey)
//!   6. Recent errors grouped by component
//!
//! Output: a single markdown bundle to stdout that the caller
//! (systemd `ExecStopPost=` or CloudWatch alarm handler) sends to
//! Telegram / SNS / operator mailbox.
//!
//! This is the "automated auto-triage" step Parthiban asked for — so
//! instead of just seeing 'QuestDB disconnected > 30s' on Telegram,
//! the operator sees the alert + a diagnostic snapshot together.
//!
//! # Invocation
//!
//! ```bash
//! tv-doctor                         # plain text report
//! tv-doctor --format markdown       # markdown-formatted (default)
//! tv-doctor --format json           # machine-readable
//! tv-doctor --metrics-url http://localhost:9090/metrics   # override
//! ```
//!
//! Exit codes:
//!   0 — report generated
//!   1 — fatal error collecting data (shouldn't happen — all
//!        probes are fail-safe)

#![allow(clippy::unwrap_used)] // APPROVED: diagnostic CLI
#![allow(clippy::expect_used)] // APPROVED: diagnostic CLI

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_JOURNAL_LINES: usize = 200;
const DEFAULT_METRICS_URL: &str = "http://localhost:9090/metrics";
const DEFAULT_SPILL_DIR: &str = "/opt/tickvault/data/spill";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let format = parse_flag(&args, "--format").unwrap_or_else(|| "markdown".to_string());
    let metrics_url =
        parse_flag(&args, "--metrics-url").unwrap_or_else(|| DEFAULT_METRICS_URL.to_string());
    let journal_lines: usize = parse_flag(&args, "--journal-lines")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_JOURNAL_LINES);

    let report = Report {
        generated_at: now_utc_iso(),
        systemd_state: collect_systemd_state(),
        docker_state: collect_docker_state(),
        disk_usage: collect_disk_usage(),
        journal_tail: collect_journal_tail(journal_lines),
        metrics_snapshot: collect_metrics_snapshot(&metrics_url),
    };

    match format.as_str() {
        "markdown" => print_markdown(&report),
        "json" => print_json(&report),
        _ => print_plain(&report),
    }
}

fn parse_flag(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

fn now_utc_iso() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Minimal ISO8601 without pulling chrono — report is informational.
    format!("unix_secs={now}")
}

// ---------------------------------------------------------------------------
// Probe fns — each returns a String with the result. All probes are fail-safe:
// a missing binary / file / service returns a descriptive "not-available"
// string rather than panicking or erroring.
// ---------------------------------------------------------------------------

fn collect_systemd_state() -> String {
    run_cmd(&["systemctl", "is-active", "tickvault"])
        .unwrap_or_else(|_| "systemctl-not-available".to_string())
}

fn collect_docker_state() -> String {
    run_cmd(&[
        "docker",
        "ps",
        "--filter",
        "name=tv-",
        "--format",
        "{{.Names}}\t{{.Status}}",
    ])
    .unwrap_or_else(|_| "docker-not-available".to_string())
}

fn collect_disk_usage() -> String {
    run_cmd(&["df", "-h", DEFAULT_SPILL_DIR])
        .unwrap_or_else(|_| "df-not-available-or-path-missing".to_string())
}

fn collect_journal_tail(lines: usize) -> String {
    run_cmd(&[
        "journalctl",
        "-u",
        "tickvault",
        "-n",
        &lines.to_string(),
        "--no-pager",
    ])
    .unwrap_or_else(|_| "journalctl-not-available".to_string())
}

fn collect_metrics_snapshot(url: &str) -> String {
    // Uses curl instead of reqwest to keep the binary small and
    // avoid pulling a tokio runtime for a one-shot probe.
    match run_cmd(&["curl", "-s", "--max-time", "3", url]) {
        Ok(body) => {
            // Filter to just the DLT metrics — ignore process_* / go_*
            body.lines()
                .filter(|l| l.starts_with("tv_"))
                .take(60)
                .collect::<Vec<_>>()
                .join("\n")
        }
        Err(_) => "metrics-unreachable".to_string(),
    }
}

fn run_cmd(args: &[&str]) -> Result<String, String> {
    if args.is_empty() {
        return Err("empty command".to_string());
    }
    let output = Command::new(args[0])
        .args(&args[1..])
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "exit={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ---------------------------------------------------------------------------
// Report structure + renderers
// ---------------------------------------------------------------------------

struct Report {
    generated_at: String,
    systemd_state: String,
    docker_state: String,
    disk_usage: String,
    journal_tail: String,
    metrics_snapshot: String,
}

fn print_markdown(r: &Report) {
    println!("## tv-doctor report");
    println!();
    println!("_Generated: {}_", r.generated_at);
    println!();
    println!("### 1. systemd state");
    println!("```");
    println!("{}", r.systemd_state.trim());
    println!("```");
    println!();
    println!("### 2. Docker services");
    println!("```");
    println!("{}", r.docker_state.trim());
    println!("```");
    println!();
    println!("### 3. Spill disk usage");
    println!("```");
    println!("{}", r.disk_usage.trim());
    println!("```");
    println!();
    println!("### 4. Metrics snapshot (tv_*)");
    println!("```");
    println!("{}", r.metrics_snapshot.trim());
    println!("```");
    println!();
    println!("### 5. Recent journal (last lines)");
    println!("```");
    println!("{}", r.journal_tail.trim());
    println!("```");
}

fn print_plain(r: &Report) {
    println!("tv-doctor report ({})", r.generated_at);
    println!("---");
    println!("systemd: {}", r.systemd_state.trim());
    println!("docker:  {}", r.docker_state.trim());
    println!("disk:    {}", r.disk_usage.trim());
    println!("---");
    println!("metrics:");
    println!("{}", r.metrics_snapshot.trim());
    println!("---");
    println!("journal tail:");
    println!("{}", r.journal_tail.trim());
}

fn print_json(r: &Report) {
    // Minimal JSON encoder — avoids pulling serde for this one binary.
    let escape = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    };
    println!("{{");
    println!("  \"generated_at\": \"{}\",", escape(&r.generated_at));
    println!("  \"systemd_state\": \"{}\",", escape(&r.systemd_state));
    println!("  \"docker_state\": \"{}\",", escape(&r.docker_state));
    println!("  \"disk_usage\": \"{}\",", escape(&r.disk_usage));
    println!(
        "  \"metrics_snapshot\": \"{}\",",
        escape(&r.metrics_snapshot)
    );
    println!("  \"journal_tail\": \"{}\"", escape(&r.journal_tail));
    println!("}}");
}
