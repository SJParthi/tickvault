//! One-off delete of the four Quote 52 leftovers (owner Quote 56, 2026-11-11).
//!
//! Run only by `.github/workflows/disk-delete-2026-11-11.yml` under the
//! one-off role `tv-prod-disk-delete-2026-11-11`. Usage: `dry` or `apply`.
//! Exit 0 only when every step did what the mode asked; 2 on a bad argument;
//! 1 otherwise. Removed with its role and workflow after the run.

#![cfg_attr(not(test), deny(unsafe_code))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
#![deny(unused_must_use)]

use std::io::Write as _;

use tickvault_app::disk_delete_2026_11_11::{SdkDiskAws, parse_mode, run};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let mut out = std::io::stdout();
    let arg = std::env::args().nth(1).unwrap_or_default();
    let Some(mode) = parse_mode(&arg) else {
        let _ = writeln!(
            out,
            "usage: tv-disk-delete-2026-11-11 dry|apply (got {arg:?})"
        );
        return std::process::ExitCode::from(2);
    };
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let aws = SdkDiskAws::new(aws_sdk_ec2::Client::new(&config));
    let now = chrono::Utc::now().timestamp();
    let report = run(&aws, mode, now).await;
    let _ = writeln!(out, "mode: {mode:?}");
    for line in &report.lines {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "result: {}", if report.ok { "OK" } else { "FAILED" });
    if report.ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
