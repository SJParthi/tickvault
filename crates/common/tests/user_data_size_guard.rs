//! Guard: the EC2 user-data template must fit inside AWS's hard 16 KB cap.
//!
//! ## Why this exists
//!
//! `aws_instance.user_data` is capped by AWS at **16,384 bytes**, and the
//! terraform provider refuses the PLAN above it:
//!
//! ```text
//! Error: expected length of user_data to be in the range (0 - 16384), got #!/bin/bash...
//! ```
//!
//! On 2026-08-10 the rendered template measured **16,382 bytes** — *two bytes*
//! of headroom — and nothing anywhere checked it. Adding an inline kernel-tuning
//! block took it to 21,116 and broke `terraform plan` for the whole workspace,
//! including every unrelated resource in the same run.
//!
//! The failure mode is nasty for two reasons. It is **invisible while writing**:
//! the template is a shell script and every editor, linter and `bash -n` check
//! passes happily at any size. And it is **discovered late** — not at commit, not
//! at unit test, but in CI against real AWS state, after a state lock has been
//! taken.
//!
//! ## What this guard enforces
//!
//! The template, rendered exactly as terraform renders it, must fit with a
//! deliberate safety margin. The margin exists because a guard that passes at
//! 16,383 bytes is theatre: the next one-line change breaks the plan again.
//!
//! ## If this test fails
//!
//! Do NOT shave comments off unrelated blocks to squeeze under. Put the new
//! content in a file under `deploy/aws/` and `cp` it into place AFTER the Step 5
//! repo clone — that is what `deploy/aws/sysctl/99-tickvault-net.conf` and
//! `deploy/aws/EMF-METRIC-SELECTOR-NOTES.md` did. user-data should carry only
//! what must run BEFORE the repo exists on disk.

use std::path::PathBuf;
use std::process::Command;

/// AWS's hard limit on `user_data`, in bytes. Not ours to change.
const EC2_USER_DATA_LIMIT_BYTES: usize = 16_384;

/// Required free space below the limit.
///
/// 512 bytes ~= a dozen lines of shell. Chosen so that a routine one-line
/// addition cannot silently re-create the 2-byte-headroom state that caused the
/// 2026-08-10 outage — the guard should fail while there is still room to think,
/// not at the moment the plan breaks.
const REQUIRED_MARGIN_BYTES: usize = 512;

const TEMPLATE: &str = "deploy/aws/terraform/user-data.sh.tftpl";

fn repo_root() -> PathBuf {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("user_data_size_guard: `git rev-parse` failed (needs a git checkout)");
    assert!(
        out.status.success(),
        "user_data_size_guard: not a git checkout"
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Render the template the way `templatefile()` does for this call site.
///
/// `main.tf` passes exactly two variables:
/// ```hcl
/// user_data = templatefile("${path.module}/user-data.sh.tftpl", {
///   environment = var.environment
///   region      = var.aws_region
/// })
/// ```
///
/// `$${` is terraform's escape for a literal `${` (shell parameter expansion in
/// the script body), so it renders one byte shorter.
///
/// Substitution values are the live prod ones. `region` is the longer of the two
/// substitutions (`${region}` -> `ap-south-1` GROWS by one byte per occurrence),
/// so this is not a free under-count: the guard measures real growth, and the
/// 512-byte margin covers a longer environment name.
fn render(template: &str) -> String {
    template
        .replace("${environment}", "prod")
        .replace("${region}", "ap-south-1")
        .replace("$${", "${")
}

#[test]
fn user_data_template_fits_the_ec2_limit_with_margin() {
    let root = repo_root();
    let raw = std::fs::read_to_string(root.join(TEMPLATE))
        .unwrap_or_else(|e| panic!("user_data_size_guard: cannot read {TEMPLATE}: {e}"));
    let rendered = render(&raw);
    let size = rendered.len();
    let budget = EC2_USER_DATA_LIMIT_BYTES - REQUIRED_MARGIN_BYTES;

    assert!(
        size <= budget,
        "EC2 USER-DATA OVER BUDGET: {TEMPLATE} renders to {size} bytes.\n\
         AWS hard limit is {EC2_USER_DATA_LIMIT_BYTES}; this guard requires \
         {REQUIRED_MARGIN_BYTES} bytes of margin, so the budget is {budget} \
         ({} over).\n\n\
         terraform will refuse the PLAN above the hard limit, failing every \
         unrelated resource in the same run — this happened on 2026-08-10 when \
         the template sat at 16382 bytes (two bytes spare) and an inline block \
         was added.\n\n\
         DO NOT fix this by shaving comments off unrelated blocks. Put the new \
         content in a file under deploy/aws/ and `cp` it into place AFTER the \
         Step 5 repo clone. user-data should carry only what must run BEFORE the \
         repo exists on disk.",
        size.saturating_sub(budget)
    );
}

#[test]
fn render_matches_terraform_templatefile_semantics() {
    // Substitution of both variables.
    assert_eq!(
        render("env=${environment} rgn=${region}"),
        "env=prod rgn=ap-south-1"
    );

    // `$${` is the terraform escape for a literal `${` — it must SHRINK by one
    // byte, exactly as templatefile does. Getting this backwards would make the
    // guard under-count every shell variable in the script.
    assert_eq!(render("echo $${HOME}"), "echo ${HOME}");
    assert!(render("echo $${HOME}").len() < "echo $${HOME}".len());

    // `$(...)` command substitution is NOT a terraform construct and must pass
    // through untouched.
    assert_eq!(render("d=$(date -u)"), "d=$(date -u)");

    // A bare `$VAR` (no braces) is likewise untouched.
    assert_eq!(render("echo $PATH"), "echo $PATH");
}

#[test]
fn guard_reports_the_real_margin_for_humans() {
    // Not an assertion about size — this makes the current headroom visible in
    // test output so a reviewer can see how close the template is running.
    let root = repo_root();
    let raw = std::fs::read_to_string(root.join(TEMPLATE)).expect("template readable");
    let size = render(&raw).len();
    println!(
        "user-data rendered = {size} bytes; AWS limit {EC2_USER_DATA_LIMIT_BYTES}; \
         free = {} bytes",
        EC2_USER_DATA_LIMIT_BYTES.saturating_sub(size)
    );
    assert!(size > 0, "template rendered empty — substitution is broken");
}
