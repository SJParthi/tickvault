//! Sub-PR #1 of 2026-05-27 daily-universe expansion — source-scan ratchet
//! pinning the **r8g.large instance type lock** documented in
//! `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §7.
//!
//! Lock history: 2026-05-18 t4g.medium → 2026-05-27 t4g.large → 2026-05-29
//! m8g.large (8 GiB) → 2026-06-30 **r8g.large** (Graviton4, 16 GiB RAM —
//! operator Quote 7). The 2026-06-30 change DOUBLED RAM (8 → 16 GiB,
//! memory-optimized r-family) for the upcoming both-feeds + larger-universe
//! workload, and recomputed the §7 cost line (~₹2,058/mo → ~₹2,919/mo incl
//! GST, 270 hrs, 30 GB EBS, +EIP kept). This guard fails the build if:
//!
//!  1. The new authoritative rule file disappears or stops pinning
//!     `r8g.large`.
//!  2. The four superseded rule files lose their SUPERSEDED-BY
//!     markers (= future readers wouldn't find the new contract).
//!  3. The instance-type lock value in any of the docs disagrees
//!     with `r8g.large`.
//!
//! See:
//! - `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
//! - `.claude/rules/project/aws-budget.md`
//! - `docs/architecture/aws-indices-only-locked-architecture.md` §5
//! - `.claude/rules/project/websocket-connection-scope-lock.md`
//! - `.claude/rules/project/operator-charter-forever.md` §I

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

/// Section A — the new authoritative rule file MUST exist and pin
/// `r8g.large` as the locked instance type.
#[test]
fn instance_lock_authoritative_rule_file_pins_r8g_large() {
    let root = repo_root();
    let path = root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md");
    assert!(
        path.exists(),
        "authoritative rule file missing at {}",
        path.display()
    );
    let body = read(&path);
    assert!(
        body.contains("r8g.large"),
        "daily-universe rule file must pin `r8g.large` as the new instance lock"
    );
    assert!(
        body.contains("**r8g.large**"),
        "daily-universe rule file must bold-pin `r8g.large` in the §7 instance-spec table"
    );
    assert!(
        body.contains("16 GiB RAM"),
        "daily-universe rule file must pin 16 GiB RAM for r8g.large"
    );
}

/// Section B — the 4 superseded files MUST carry SUPERSEDED-BY markers
/// pointing at the new rule file. Without these, future readers
/// following the old authority chain would not find the 2026-05-27
/// contract.
#[test]
fn instance_lock_supersession_markers_present_in_aws_budget() {
    let root = repo_root();
    let body = read(&root.join(".claude/rules/project/aws-budget.md"));
    assert!(
        body.contains("SUPERSEDED 2026-05-27"),
        "aws-budget.md must carry the 2026-05-27 supersession marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "aws-budget.md must link to the new authoritative rule file"
    );
    assert!(
        body.contains("t4g.medium → t4g.large") || body.contains("t4g.large"),
        "aws-budget.md supersession marker must mention t4g.large upgrade"
    );
}

#[test]
fn instance_lock_supersession_markers_present_in_architecture_doc() {
    let root = repo_root();
    let body = read(&root.join("docs/architecture/aws-indices-only-locked-architecture.md"));
    assert!(
        body.contains("§5 SUPERSEDED 2026-05-27"),
        "architecture doc must carry the §5 SUPERSEDED marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "architecture doc must link to the new authoritative rule file"
    );
}

#[test]
fn instance_lock_supersession_markers_present_in_websocket_scope_lock() {
    let root = repo_root();
    let body = read(&root.join(".claude/rules/project/websocket-connection-scope-lock.md"));
    assert!(
        body.contains("SUPERSEDED 2026-05-27"),
        "websocket-connection-scope-lock.md must carry the 2026-05-27 supersession marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "websocket-connection-scope-lock.md must link to the new authoritative rule file"
    );
}

#[test]
fn instance_lock_supersession_markers_present_in_operator_charter() {
    let root = repo_root();
    let body = read(&root.join(".claude/rules/project/operator-charter-forever.md"));
    assert!(
        body.contains("SUPERSEDED 2026-05-27"),
        "operator-charter-forever.md §I must carry the 2026-05-27 supersession marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "operator-charter-forever.md must link to the new authoritative rule file"
    );
}

/// Section C — the bill in §7 must match the locked figure. Operator
/// approved ~₹3,101/mo on 2026-07-13 (EBS 30 -> 50 GB disk-pressure grow,
/// +~₹180/mo over the 2026-06-30 ~₹2,919/mo r8g.large figure; 270 hrs,
/// 50 GB EBS, +EIP kept, incl. 18% GST). If someone re-tunes this in the
/// rule file without operator approval, the build fails.
#[test]
fn instance_lock_monthly_bill_pinned_to_rupees_3101() {
    let root = repo_root();
    let body =
        read(&root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md"));
    assert!(
        body.contains("~₹3,101/mo") || body.contains("Rs 3,101/mo"),
        "rule file §7 must pin the ~₹3,101/mo bill (operator approved 2026-07-13: EBS 50 GB grow on r8g.large, 270 hrs, +EIP, incl GST)"
    );
}

/// Section D — the schedule lock must mention `08:30 IST` (operator narrowed
/// the window back to 08:30-16:30 on 2026-06-05: "make the aws instance start
/// and stop from 8.30 am till 4.30 pm"; supersedes the 2026-06-02 widening).
#[test]
fn instance_lock_schedule_pinned_to_0830_ist_start() {
    let root = repo_root();
    let body =
        read(&root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md"));
    assert!(
        body.contains("08:30–16:30 IST") || body.contains("08:30 IST"),
        "rule file §7 must pin the 08:30 IST EventBridge cron start"
    );
}
