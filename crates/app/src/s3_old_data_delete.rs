//! One-time delete of the pre-2026-10-09 market-data copies in
//! `s3://tv-prod-cold` (operator Quote 29e, 2026-10-09).
//!
//! Authorization: `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`,
//! "Quote 29e — ONE-TIME S3 DELETE PERMISSION". The operator tapped
//! "One-time permission" on a card that said the deletion cannot be undone.
//!
//! This module holds the pure half of the `tv-s3-old-data-delete` tool: which
//! keys are in scope, which run is allowed, and the report. The network half
//! (list every version, delete in batches, list again) is in
//! `src/bin/s3_old_data_delete.rs`.
//!
//! Scope, and nothing else:
//! - `questdb-partitions/<table>/<partition dated before 2026-10-09>` for the
//!   17 market-data tables in [`MARKET_DATA_TABLES`];
//! - `raw-frames/<date before 2026-10-09>/…`;
//! - `seal-spill/<date before 2026-10-09>/…`.
//!
//! Every version and every delete marker of such a key is deleted. A key that
//! does not parse into one of those shapes is KEPT and reported, never guessed
//! at. SEBI and audit tables are not listed at all. The IAM role
//! (`deploy/aws/terraform/s3-old-data-delete-2026-10-09.tf`) grants the same
//! scope, so the code and the permission check each other.
//!
//! Removed after the run, with the role and the workflow (Quote 29e,
//! "Afterwards").
//!
//! Complexity: [`classify_key`] is O(key length); [`DeletePlan::from_entries`]
//! is O(versions). Cold, one-off, never on any live path.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime};

/// The only bucket this tool may touch.
pub const BUCKET: &str = "tv-prod-cold";

/// Keys dated strictly before this IST date are in scope.
pub const CUTOFF_DATE: &str = "2026-10-09";

/// The tool refuses every run after this IST date (Quote 29e "When").
pub const LAST_RUN_IST_DATE: &str = "2026-10-16";

/// `--confirm` value an `apply` run must carry.
pub const APPLY_CONFIRM: &str = "DELETE-S3-BEFORE-2026-10-09";

/// S3 `DeleteObjects` accepts at most 1,000 keys per request.
pub const DELETE_BATCH_MAX: usize = 1000;

/// The 17 market-data tables of Quote 29. SEBI, audit and lifecycle tables are
/// deliberately absent. Keep in lockstep with `s3_old_delete_tables` in
/// `deploy/aws/terraform/s3-old-data-delete-2026-10-09.tf`.
pub const MARKET_DATA_TABLES: [&str; 17] = [
    "candles_10m",
    "candles_15m",
    "candles_1m",
    "candles_1s",
    "candles_30m",
    "candles_3m",
    "candles_3s",
    "candles_5m",
    "candles_5s",
    "candles_60m",
    "feed_aux_packets",
    "market_depth",
    "ticks",
    "top_volume_1m",
    "top_volume_1s",
    "top_volume_3s",
    "top_volume_5s",
];

const PARTITIONS_ROOT: &str = "questdb-partitions/";
const RAW_FRAMES_ROOT: &str = "raw-frames/";
const SEAL_SPILL_ROOT: &str = "seal-spill/";

/// The only year the IAM grant covers (`2026-0?-??*`, `2026-10-0?*`). A key
/// dated in another year is kept and reported, so an `apply` never runs into
/// a refusal it could have predicted.
const GRANTED_YEAR: i32 = 2026;

/// Market close plus the deploy guard's edge: no apply in 09:00–15:45 IST.
const APPLY_BLOCKED_FROM_HHMM: (u32, u32) = (9, 0);
const APPLY_BLOCKED_UNTIL_HHMM: (u32, u32) = (15, 45);

/// The exact prefixes the tool lists, one per table plus the two dated roots.
/// The IAM `s3:prefix` condition allows these and no others.
pub fn list_prefixes() -> Vec<String> {
    let mut out: Vec<String> = MARKET_DATA_TABLES
        .iter()
        .map(|t| format!("{PARTITIONS_ROOT}{t}/"))
        .collect();
    out.push(RAW_FRAMES_ROOT.to_string());
    out.push(SEAL_SPILL_ROOT.to_string());
    out
}

/// What the tool does with one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// In scope and dated before the cutoff: every version is deleted.
    Delete,
    /// In scope but dated on or after the cutoff: kept.
    KeepNotOld,
    /// Not one of the three shapes, or dated outside the granted year: kept
    /// and reported.
    KeepUnparsed,
}

/// Reads a `YYYY-MM-DD` date at the start of `s`. Returns the date only when
/// it is a real calendar date.
fn leading_date(s: &str) -> Option<NaiveDate> {
    let head = s.get(..10)?;
    let bytes = head.as_bytes();
    let shape_ok = bytes.iter().enumerate().all(|(i, b)| match i {
        4 | 7 => *b == b'-',
        _ => b.is_ascii_digit(),
    });
    if !shape_ok {
        return None;
    }
    NaiveDate::parse_from_str(head, "%Y-%m-%d").ok()
}

fn cutoff() -> Option<NaiveDate> {
    NaiveDate::parse_from_str(CUTOFF_DATE, "%Y-%m-%d").ok()
}

fn verdict_for(date: NaiveDate) -> Verdict {
    if date.year() != GRANTED_YEAR {
        return Verdict::KeepUnparsed;
    }
    match cutoff() {
        Some(c) if date < c => Verdict::Delete,
        Some(_) => Verdict::KeepNotOld,
        None => Verdict::KeepUnparsed,
    }
}

/// Classifies one object key. O(key length).
pub fn classify_key(key: &str) -> Verdict {
    if let Some(rest) = key.strip_prefix(PARTITIONS_ROOT) {
        let Some((table, name)) = rest.split_once('/') else {
            return Verdict::KeepUnparsed;
        };
        if !MARKET_DATA_TABLES.contains(&table) || name.contains('/') {
            return Verdict::KeepUnparsed;
        }
        let Some(date) = leading_date(name) else {
            return Verdict::KeepUnparsed;
        };
        // A partition name continues with `.csv.gz` (day) or `T<hour>` (hour).
        return match name.as_bytes().get(10) {
            Some(b'.') | Some(b'T') => verdict_for(date),
            _ => Verdict::KeepUnparsed,
        };
    }
    for root in [RAW_FRAMES_ROOT, SEAL_SPILL_ROOT] {
        if let Some(rest) = key.strip_prefix(root) {
            let Some((dir, file)) = rest.split_once('/') else {
                return Verdict::KeepUnparsed;
            };
            if dir.len() != 10 || file.is_empty() {
                return Verdict::KeepUnparsed;
            }
            return match leading_date(dir) {
                Some(date) => verdict_for(date),
                None => Verdict::KeepUnparsed,
            };
        }
    }
    Verdict::KeepUnparsed
}

/// One listed version or delete marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionEntry {
    pub key: String,
    /// `"null"` for a version written before versioning was enabled.
    pub version_id: String,
    pub bytes: i64,
    pub is_delete_marker: bool,
}

/// Counts for one top-level group in the report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupTotals {
    pub versions: usize,
    pub delete_markers: usize,
    pub bytes: i64,
}

/// What a run would delete and what it keeps.
#[derive(Debug, Clone, Default)]
pub struct DeletePlan {
    pub delete: Vec<VersionEntry>,
    /// Ordered by group name: `questdb-partitions/<table>`, `raw-frames`,
    /// `seal-spill`.
    pub groups: std::collections::BTreeMap<String, GroupTotals>,
    pub kept_not_old: usize,
    pub kept_unparsed: Vec<String>,
}

fn group_of(key: &str) -> String {
    if let Some(rest) = key.strip_prefix(PARTITIONS_ROOT) {
        let table = rest.split('/').next().unwrap_or_default();
        return format!("{PARTITIONS_ROOT}{table}");
    }
    key.split('/').next().unwrap_or_default().to_string()
}

impl DeletePlan {
    /// Sorts every listed entry into delete / keep. O(entries).
    pub fn from_entries(entries: Vec<VersionEntry>) -> Self {
        let mut plan = Self::default();
        for e in entries {
            match classify_key(&e.key) {
                Verdict::Delete => {
                    let g = plan.groups.entry(group_of(&e.key)).or_default();
                    if e.is_delete_marker {
                        g.delete_markers += 1;
                    } else {
                        g.versions += 1;
                        g.bytes = g.bytes.saturating_add(e.bytes.max(0));
                    }
                    plan.delete.push(e);
                }
                Verdict::KeepNotOld => plan.kept_not_old += 1,
                Verdict::KeepUnparsed => plan.kept_unparsed.push(e.key),
            }
        }
        plan.kept_unparsed.sort();
        plan.kept_unparsed.dedup();
        plan
    }

    /// Total bytes of the versions (not markers) to delete.
    pub fn total_bytes(&self) -> i64 {
        self.groups.values().map(|g| g.bytes).sum()
    }
}

/// Why a run must not start, or `None` when it may. `ist_now` is the wall
/// clock in IST. A dry run is read-only and may run at any hour until the
/// last day; an apply also needs the confirm word and must be outside
/// 09:00–15:45 IST.
pub fn run_refusal(apply: bool, confirm: &str, ist_now: NaiveDateTime) -> Option<String> {
    const UNEVALUABLE: &str = "internal: the run window cannot be evaluated";
    let Ok(last) = NaiveDate::parse_from_str(LAST_RUN_IST_DATE, "%Y-%m-%d") else {
        return Some(UNEVALUABLE.to_string());
    };
    if ist_now.date() > last {
        return Some(format!(
            "Quote 29e is one-time; this tool expired after {LAST_RUN_IST_DATE} IST"
        ));
    }
    if !apply {
        return None;
    }
    if confirm != APPLY_CONFIRM {
        return Some(format!("apply needs --confirm {APPLY_CONFIRM}"));
    }
    let (Some(from), Some(until)) = (
        NaiveTime::from_hms_opt(APPLY_BLOCKED_FROM_HHMM.0, APPLY_BLOCKED_FROM_HHMM.1, 0),
        NaiveTime::from_hms_opt(APPLY_BLOCKED_UNTIL_HHMM.0, APPLY_BLOCKED_UNTIL_HHMM.1, 0),
    ) else {
        return Some(UNEVALUABLE.to_string());
    };
    let t = ist_now.time();
    if t >= from && t < until {
        return Some("apply is refused 09:00-15:45 IST".to_string());
    }
    None
}

/// Plain-text report for the job log and summary. At most 50 kept keys are
/// listed by name; the count is always exact.
pub fn render_report(title: &str, plan: &DeletePlan) -> String {
    const MAX_LISTED: usize = 50;
    let mut out = String::new();
    out.push_str(&format!("{title}\n"));
    out.push_str(&format!(
        "bucket {BUCKET}, keys dated before {CUTOFF_DATE}\n\n"
    ));
    out.push_str("group | versions | delete markers | GB\n");
    for (name, g) in &plan.groups {
        out.push_str(&format!(
            "{name} | {} | {} | {:.2}\n",
            g.versions,
            g.delete_markers,
            g.bytes as f64 / 1e9
        ));
    }
    out.push_str(&format!(
        "\nto delete: {} entries, {:.2} GB\n",
        plan.delete.len(),
        plan.total_bytes() as f64 / 1e9
    ));
    out.push_str(&format!(
        "kept, dated {CUTOFF_DATE} or later: {}\n",
        plan.kept_not_old
    ));
    out.push_str(&format!(
        "kept, not a recognised key: {}\n",
        plan.kept_unparsed.len()
    ));
    for k in plan.kept_unparsed.iter().take(MAX_LISTED) {
        out.push_str(&format!("  {k}\n"));
    }
    if plan.kept_unparsed.len() > MAX_LISTED {
        out.push_str(&format!(
            "  … and {} more\n",
            plan.kept_unparsed.len() - MAX_LISTED
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ist(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn entry(key: &str, bytes: i64, marker: bool) -> VersionEntry {
        VersionEntry {
            key: key.to_string(),
            version_id: "v".to_string(),
            bytes,
            is_delete_marker: marker,
        }
    }

    #[test]
    fn test_market_data_tables_are_sorted_unique_and_exclude_audit() {
        let mut sorted = MARKET_DATA_TABLES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, MARKET_DATA_TABLES.to_vec());
        for t in MARKET_DATA_TABLES {
            assert!(!t.contains("audit") && !t.contains("lifecycle") && !t.contains("sebi"));
        }
    }

    #[test]
    fn test_market_data_tables_match_the_iam_grant() {
        let tf = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/aws/terraform/s3-old-data-delete-2026-10-09.tf"
        ))
        .unwrap();
        let start = tf.find("s3_old_delete_tables = [").unwrap();
        let block = &tf[start..start + tf[start..].find(']').unwrap()];
        let in_tf: Vec<&str> = block.split('"').skip(1).step_by(2).collect();
        assert_eq!(in_tf, MARKET_DATA_TABLES.to_vec());
    }

    #[test]
    fn test_list_prefixes_are_exact_and_end_with_a_slash() {
        let p = list_prefixes();
        assert_eq!(p.len(), 19);
        assert!(p.iter().all(|x| x.ends_with('/')));
        assert!(p.contains(&"questdb-partitions/ticks/".to_string()));
        assert!(p.contains(&"raw-frames/".to_string()));
        assert!(p.contains(&"seal-spill/".to_string()));
        assert!(
            !p.iter()
                .any(|x| x.starts_with("deploys") || x.starts_with("sebi"))
        );
    }

    #[test]
    fn test_classify_partition_keys() {
        use Verdict::*;
        let cases = [
            ("questdb-partitions/ticks/2026-10-08.csv.gz", Delete),
            ("questdb-partitions/ticks/2026-10-08T15.csv.gz", Delete),
            (
                "questdb-partitions/market_depth/2026-10-01T09.sha-ab12.csv.gz",
                Delete,
            ),
            ("questdb-partitions/candles_1s/2026-09-25.csv.gz", Delete),
            ("questdb-partitions/ticks/2026-10-09.csv.gz", KeepNotOld),
            ("questdb-partitions/ticks/2026-10-09T03.csv.gz", KeepNotOld),
            ("questdb-partitions/ticks/2026-10-12.csv.gz", KeepNotOld),
            (
                "questdb-partitions/order_audit/2026-10-01.csv.gz",
                KeepUnparsed,
            ),
            (
                "questdb-partitions/instrument_lifecycle/2026-10-01.csv.gz",
                KeepUnparsed,
            ),
            ("questdb-partitions/ticks_x/2026-10-01.csv.gz", KeepUnparsed),
            (
                "questdb-partitions/ticks/sub/2026-10-01.csv.gz",
                KeepUnparsed,
            ),
            ("questdb-partitions/ticks/2026-10-01", KeepUnparsed),
            ("questdb-partitions/ticks/2026-13-01.csv.gz", KeepUnparsed),
            ("questdb-partitions/ticks/2026-02-30.csv.gz", KeepUnparsed),
            ("questdb-partitions/ticks/2025-12-31.csv.gz", KeepUnparsed),
            ("questdb-partitions/ticks/x2026-10-01.csv.gz", KeepUnparsed),
            ("questdb-partitions/ticks", KeepUnparsed),
            ("questdb-partitions/", KeepUnparsed),
        ];
        for (k, want) in cases {
            assert_eq!(classify_key(k), want, "{k}");
        }
    }

    #[test]
    fn test_classify_dated_folder_keys() {
        use Verdict::*;
        let cases = [
            ("raw-frames/2026-10-08/ws-frames-1.wal.gz", Delete),
            ("raw-frames/2026-09-30/a/b.gz", Delete),
            ("seal-spill/2026-10-01/seals_v4-x.bin", Delete),
            ("raw-frames/2026-10-09/ws-frames-1.wal.gz", KeepNotOld),
            ("seal-spill/2026-10-09/x", KeepNotOld),
            ("raw-frames/2026-10-08/", KeepUnparsed),
            ("raw-frames/2026-10-08", KeepUnparsed),
            ("raw-frames/2026-10-8/x", KeepUnparsed),
            ("raw-frames/2026-10-08x/x", KeepUnparsed),
            ("raw-frames/latest/x", KeepUnparsed),
            ("seal-spill/x.bin", KeepUnparsed),
            ("seal-spill/2024-10-01/x", KeepUnparsed),
            ("deploys/2026-10-01/tickvault", KeepUnparsed),
            ("sebi-preserve/2026-10-01/x", KeepUnparsed),
            ("", KeepUnparsed),
        ];
        for (k, want) in cases {
            assert_eq!(classify_key(k), want, "{k}");
        }
    }

    #[test]
    fn test_plan_counts_versions_markers_and_bytes() {
        let plan = DeletePlan::from_entries(vec![
            entry("questdb-partitions/ticks/2026-10-08.csv.gz", 100, false),
            entry("questdb-partitions/ticks/2026-10-08.csv.gz", 0, true),
            entry("questdb-partitions/ticks/2026-10-09.csv.gz", 50, false),
            entry("raw-frames/2026-10-01/a.wal.gz", 7, false),
            entry("raw-frames/2026-10-01/a.wal.gz", -3, false),
            entry("questdb-partitions/order_audit/2026-10-01.csv.gz", 9, false),
            entry("questdb-partitions/order_audit/2026-10-01.csv.gz", 9, false),
        ]);
        assert_eq!(plan.delete.len(), 4);
        assert_eq!(plan.kept_not_old, 1);
        assert_eq!(
            plan.kept_unparsed,
            vec!["questdb-partitions/order_audit/2026-10-01.csv.gz".to_string()]
        );
        let ticks = &plan.groups["questdb-partitions/ticks"];
        assert_eq!(
            (ticks.versions, ticks.delete_markers, ticks.bytes),
            (1, 1, 100)
        );
        let raw = &plan.groups["raw-frames"];
        assert_eq!((raw.versions, raw.delete_markers, raw.bytes), (2, 0, 7));
        assert_eq!(plan.total_bytes(), 107);
    }

    #[test]
    fn test_empty_plan_deletes_nothing() {
        let plan = DeletePlan::from_entries(Vec::new());
        assert!(plan.delete.is_empty());
        assert_eq!(plan.total_bytes(), 0);
        assert!(render_report("t", &plan).contains("to delete: 0 entries"));
    }

    #[test]
    fn test_run_refusal_rules() {
        // Dry runs any hour until the last day.
        assert_eq!(run_refusal(false, "", ist("2026-10-09 10:00")), None);
        assert_eq!(run_refusal(false, "", ist("2026-10-16 23:59")), None);
        assert!(run_refusal(false, "", ist("2026-10-17 00:00")).is_some());
        // Apply needs the word.
        assert!(run_refusal(true, "", ist("2026-10-09 22:30")).is_some());
        assert!(run_refusal(true, "delete", ist("2026-10-09 22:30")).is_some());
        assert_eq!(
            run_refusal(true, APPLY_CONFIRM, ist("2026-10-09 22:30")),
            None
        );
        // Apply never inside 09:00-15:45 IST.
        assert!(run_refusal(true, APPLY_CONFIRM, ist("2026-10-12 09:00")).is_some());
        assert!(run_refusal(true, APPLY_CONFIRM, ist("2026-10-12 15:44")).is_some());
        assert_eq!(
            run_refusal(true, APPLY_CONFIRM, ist("2026-10-12 08:59")),
            None
        );
        assert_eq!(
            run_refusal(true, APPLY_CONFIRM, ist("2026-10-12 15:45")),
            None
        );
        // Apply never after the last day.
        assert!(run_refusal(true, APPLY_CONFIRM, ist("2026-10-17 20:00")).is_some());
    }

    #[test]
    fn test_render_report_lists_groups_and_caps_kept_keys() {
        let mut entries = vec![entry("seal-spill/2026-10-01/a", 2_000_000_000, false)];
        for i in 0..60 {
            entries.push(entry(&format!("raw-frames/odd-{i}"), 1, false));
        }
        let plan = DeletePlan::from_entries(entries);
        let r = render_report("DRY RUN", &plan);
        assert!(r.starts_with("DRY RUN\n"));
        assert!(r.contains("seal-spill | 1 | 0 | 2.00"));
        assert!(r.contains("kept, not a recognised key: 60"));
        assert!(r.contains("and 10 more"));
    }
}
