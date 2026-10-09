//! `tv-s3-old-data-delete` — the one-time Quote 29e delete of the
//! pre-2026-10-09 market-data copies in `s3://tv-prod-cold`.
//!
//! Run only by `.github/workflows/s3-old-data-delete-2026-10-09.yml`, under the
//! one-off role `tv-prod-s3-old-data-delete-2026-10-09`. Scope, run window and
//! the report are in `tickvault_app::s3_old_data_delete`.
//!
//! ```text
//! tv-s3-old-data-delete --mode dry   [--report <file>]
//! tv-s3-old-data-delete --mode apply --confirm DELETE-S3-BEFORE-2026-10-09 [--report <file>]
//! ```
//!
//! `dry` lists every version and delete marker under the 19 prefixes and
//! reports what would go. `apply` does the same listing, deletes the in-scope
//! entries 1,000 at a time by version id (so nothing becomes a delete marker),
//! then lists again and exits non-zero unless nothing in scope remains and
//! every delete succeeded.
//!
//! Exit codes: 0 done; 1 refused, failed, or something in scope remains.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stderr, clippy::dbg_macro)]
#![allow(clippy::print_stdout)] // APPROVED: one-off CLI; its output is the job log

use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use chrono::{FixedOffset, Utc};
use tickvault_app::s3_old_data_delete::{
    BUCKET, DELETE_BATCH_MAX, DeletePlan, VersionEntry, list_prefixes, render_report, run_refusal,
};

const IST_OFFSET_SECS: i32 = 5 * 3600 + 30 * 60;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

async fn list_all(client: &aws_sdk_s3::Client) -> Result<Vec<VersionEntry>> {
    let mut out = Vec::new();
    for prefix in list_prefixes() {
        let mut key_marker: Option<String> = None;
        let mut version_marker: Option<String> = None;
        loop {
            let resp = client
                .list_object_versions()
                .bucket(BUCKET)
                .prefix(&prefix)
                .set_key_marker(key_marker.clone())
                .set_version_id_marker(version_marker.clone())
                .send()
                .await
                .map_err(|e| anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))
                .with_context(|| format!("listing versions under {prefix}"))?;
            for v in resp.versions() {
                out.push(VersionEntry {
                    key: v.key().unwrap_or_default().to_string(),
                    version_id: v.version_id().unwrap_or("null").to_string(),
                    bytes: v.size().unwrap_or(0),
                    is_delete_marker: false,
                });
            }
            for m in resp.delete_markers() {
                out.push(VersionEntry {
                    key: m.key().unwrap_or_default().to_string(),
                    version_id: m.version_id().unwrap_or("null").to_string(),
                    bytes: 0,
                    is_delete_marker: true,
                });
            }
            if resp.is_truncated() != Some(true) {
                break;
            }
            key_marker = resp.next_key_marker().map(str::to_string);
            version_marker = resp.next_version_id_marker().map(str::to_string);
            if key_marker.is_none() && version_marker.is_none() {
                return Err(anyhow!(
                    "listing under {prefix} was truncated with no next marker"
                ));
            }
        }
    }
    Ok(out)
}

/// Deletes one batch by version id. Returns the per-key errors S3 reported.
async fn delete_batch(client: &aws_sdk_s3::Client, batch: &[VersionEntry]) -> Result<Vec<String>> {
    let mut objects = Vec::with_capacity(batch.len());
    for e in batch {
        objects.push(
            ObjectIdentifier::builder()
                .key(&e.key)
                .version_id(&e.version_id)
                .build()
                .context("building an object identifier")?,
        );
    }
    let delete = Delete::builder()
        .set_objects(Some(objects))
        .quiet(true)
        .build()
        .context("building the delete request")?;
    let resp = client
        .delete_objects()
        .bucket(BUCKET)
        .delete(delete)
        .send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))?;
    Ok(resp
        .errors()
        .iter()
        .map(|err| {
            format!(
                "{} (version {}): {} {}",
                err.key().unwrap_or_default(),
                err.version_id().unwrap_or_default(),
                err.code().unwrap_or_default(),
                err.message().unwrap_or_default()
            )
        })
        .collect())
}

fn write_report(path: Option<&str>, text: &str) -> Result<()> {
    if let Some(p) = path {
        std::fs::write(p, text).with_context(|| format!("writing the report to {p}"))?;
    }
    Ok(())
}

async fn run(args: &[String]) -> Result<bool> {
    let mode = flag(args, "--mode").unwrap_or_default();
    let apply = match mode.as_str() {
        "dry" => false,
        "apply" => true,
        _ => return Err(anyhow!("--mode must be dry or apply")),
    };
    let confirm = flag(args, "--confirm").unwrap_or_default();
    let report_path = flag(args, "--report");

    let ist = FixedOffset::east_opt(IST_OFFSET_SECS).ok_or_else(|| anyhow!("IST offset"))?;
    let now_ist = Utc::now().with_timezone(&ist).naive_local();
    if let Some(reason) = run_refusal(apply, &confirm, now_ist) {
        return Err(anyhow!("refused: {reason}"));
    }

    let conf = aws_config::load_from_env().await;
    let client = aws_sdk_s3::Client::new(&conf);

    let plan = DeletePlan::from_entries(list_all(&client).await?);
    let title = if apply {
        format!("APPLY — before deleting ({now_ist} IST)")
    } else {
        format!("DRY RUN — nothing deleted ({now_ist} IST)")
    };
    let mut report = render_report(&title, &plan);
    println!("{report}");
    if !apply {
        write_report(report_path.as_deref(), &report)?;
        return Ok(true);
    }

    // Record what is about to go before the first delete, so a failure later
    // in the run still leaves the "before" report in the job summary.
    write_report(report_path.as_deref(), &report)?;

    let mut errors = Vec::new();
    let mut sent = 0usize;
    for batch in plan.delete.chunks(DELETE_BATCH_MAX) {
        match delete_batch(&client, batch).await {
            Ok(errs) => errors.extend(errs),
            Err(e) => errors.push(format!("batch of {} failed: {e:#}", batch.len())),
        }
        sent += batch.len();
        println!("sent {sent} of {}", plan.delete.len());
    }

    report.push_str(&format!("\nsent {sent} deletes\n"));
    let after = match list_all(&client).await {
        Ok(entries) => Some(DeletePlan::from_entries(entries)),
        Err(e) => {
            errors.push(format!("verify listing failed: {e:#}"));
            None
        }
    };
    if let Some(after) = &after {
        let verify = render_report("AFTER — what remains in scope", after);
        println!("{verify}");
        report.push('\n');
        report.push_str(&verify);
    }
    if !errors.is_empty() {
        report.push_str(&format!("\n{} delete errors:\n", errors.len()));
        for e in &errors {
            report.push_str(&format!("  {e}\n"));
        }
        println!("{} delete errors (see report)", errors.len());
    }
    write_report(report_path.as_deref(), &report)?;
    Ok(errors.is_empty() && after.is_some_and(|a| a.delete.is_empty()))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match run(&args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            println!("FAILED: delete errors, or entries in scope remain");
            ExitCode::FAILURE
        }
        Err(e) => {
            println!("FAILED: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flag_reads_the_value_after_the_name() {
        let args: Vec<String> = ["bin", "--mode", "dry", "--report"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag(&args, "--mode").as_deref(), Some("dry"));
        assert_eq!(flag(&args, "--report"), None);
        assert_eq!(flag(&args, "--confirm"), None);
    }

    #[tokio::test]
    async fn test_run_refuses_a_bad_mode_and_an_unconfirmed_apply() {
        let bad: Vec<String> = ["bin", "--mode", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(run(&bad).await.is_err());
        let apply: Vec<String> = ["bin", "--mode", "apply"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = run(&apply).await.unwrap_err().to_string();
        assert!(err.contains("refused"), "{err}");
    }

    #[test]
    fn test_ist_offset_is_five_thirty() {
        assert_eq!(IST_OFFSET_SECS, 19_800);
    }
}
