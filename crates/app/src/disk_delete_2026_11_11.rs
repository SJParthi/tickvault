//! One-off delete of the four Quote 52 leftovers (owner Quote 56, 2026-11-11).
//!
//! Two volumes attached to nothing and two old snapshots kept costing money
//! after the October rebuild (Cost Explorer, 1–11 Nov 2026: about $18 + tax).
//! The owner asked for the delete to be done for them, and chose the card
//! "One-off, these 4 only (Recommended)": a one-time role whose key can delete
//! ONLY these four ids, run once, then removed with this module.
//!
//! What this module does, and nothing else:
//! - the ids are compile-time constants ([`VOLUME_IDS`], [`SNAPSHOT_IDS`]);
//!   nothing passed in at run time can add an id;
//! - a volume is deleted only when it is `available` and attached to nothing;
//! - `dry` asks AWS with `DryRun` (proves the permission, deletes nothing);
//! - `apply` deletes, then reads all four again and fails unless every one is
//!   gone (a volume still `deleting` counts as gone);
//! - after [`DEADLINE_UNIX_SECS`] (2026-11-14 00:00 UTC, the role's own
//!   expiry) it refuses to run.
//!
//! Cold path: a handful of AWS calls, once. Not on any tick path.

use std::future::Future;

/// The two volumes Quote 52 named (600 GB old root, 30 GB May root).
pub const VOLUME_IDS: [&str; 2] = ["vol-0c6ab6e593e39d8c8", "vol-073ccaa417a0f344b"];

/// The two snapshots Quote 52 named (rollbacks of the 30 GB May root).
pub const SNAPSHOT_IDS: [&str; 2] = ["snap-090ed9c4f3df0ca61", "snap-0573ab07252f67bf3"];

/// The live server's root disk. Never deletable here; pinned by a test so it
/// can never appear in the lists above.
pub const LIVE_ROOT_VOLUME_ID: &str = "vol-07a6586a56466ea83";

/// 2026-11-14 00:00:00 UTC, the same instant the role's policy expires.
pub const DEADLINE_UNIX_SECS: i64 = 1_794_614_400;

/// Reads after the delete before the verify gives up.
pub const VERIFY_ATTEMPTS: usize = 10;

/// Seconds between verify reads.
pub const VERIFY_PAUSE_SECS: u64 = 3;

/// Report-only or delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ask AWS with `DryRun`: proves the permission, deletes nothing.
    Dry,
    /// Delete, then verify.
    Apply,
}

/// Parses the workflow's `mode` input. Anything but `dry` or `apply` is refused.
pub fn parse_mode(raw: &str) -> Option<Mode> {
    match raw {
        "dry" => Some(Mode::Dry),
        "apply" => Some(Mode::Apply),
        _ => None,
    }
}

/// What a volume read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeLookup {
    /// The volume exists in this state, with this many attachments.
    Found {
        /// EC2 state, e.g. `available`, `in-use`, `deleting`.
        state: String,
        /// Number of attachments.
        attachments: usize,
    },
    /// AWS reports no such volume.
    Gone,
}

/// The result of one delete call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    /// Deleted.
    Done,
    /// `DryRun` answered that the call would succeed.
    WouldSucceed,
    /// Refused or failed, with AWS's code and message.
    Failed(String),
}

/// The AWS surface this tool needs; a trait so the whole run is tested
/// against a fake.
pub trait DiskAws {
    /// Reads one volume.
    fn volume(&self, id: &str) -> impl Future<Output = Result<VolumeLookup, String>>;
    /// True when this account still owns the snapshot.
    fn snapshot_exists(&self, id: &str) -> impl Future<Output = Result<bool, String>>;
    /// Deletes one volume (`dry_run` asks only).
    fn delete_volume(&self, id: &str, dry_run: bool) -> impl Future<Output = CallOutcome>;
    /// Deletes one snapshot (`dry_run` asks only).
    fn delete_snapshot(&self, id: &str, dry_run: bool) -> impl Future<Output = CallOutcome>;
    /// Waits between verify reads.
    fn pause(&self) -> impl Future<Output = ()>;
}

/// What a run did, line by line, and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// One plain line per step.
    pub lines: Vec<String>,
    /// True only when every step did what the mode asked.
    pub ok: bool,
}

impl Report {
    fn note(&mut self, line: String) {
        self.lines.push(line);
    }

    fn fail(&mut self, line: String) {
        self.ok = false;
        self.lines.push(line);
    }
}

/// True when a volume may be deleted: available and attached to nothing.
pub fn volume_is_deletable(state: &str, attachments: usize) -> bool {
    state == "available" && attachments == 0
}

/// True when a volume counts as gone for the verify: no longer listed, or
/// already `deleting` or `deleted` (AWS finishes it by itself).
pub fn volume_counts_as_gone(lookup: &VolumeLookup) -> bool {
    match lookup {
        VolumeLookup::Gone => true,
        VolumeLookup::Found { state, .. } => state == "deleting" || state == "deleted",
    }
}

fn record_call(report: &mut Report, id: &str, mode: Mode, outcome: CallOutcome) {
    match (mode, outcome) {
        (Mode::Apply, CallOutcome::Done) => report.note(format!("{id}: deleted")),
        (Mode::Dry, CallOutcome::WouldSucceed) => {
            report.note(format!("{id}: would be deleted (permission confirmed)"))
        }
        (_, CallOutcome::Failed(why)) => report.fail(format!("{id}: delete refused: {why}")),
        (Mode::Dry, CallOutcome::Done) => {
            report.fail(format!("{id}: AWS deleted it during a dry run"))
        }
        (Mode::Apply, CallOutcome::WouldSucceed) => {
            report.fail(format!("{id}: AWS answered a dry run during apply"))
        }
    }
}

/// Runs the delete (or its dry run) over the four constants.
pub async fn run<A: DiskAws>(aws: &A, mode: Mode, now_unix_secs: i64) -> Report {
    let mut report = Report {
        lines: Vec::new(),
        ok: true,
    };
    if now_unix_secs >= DEADLINE_UNIX_SECS {
        report.fail(
            "refused: past 2026-11-14 00:00 UTC; Quote 56 authorizes this run only before then"
                .to_string(),
        );
        return report;
    }
    // Sanity check first: the live root must be visible and attached. If it is
    // not, this run is looking at the wrong account or region, where every
    // leftover would read "already gone" and the run would report OK falsely.
    match aws.volume(LIVE_ROOT_VOLUME_ID).await {
        Ok(VolumeLookup::Found { attachments, .. }) if attachments > 0 => {
            report.note(format!(
                "{LIVE_ROOT_VOLUME_ID}: live server disk visible and attached (right account and region; never touched)"
            ));
        }
        other => {
            report.fail(format!(
                "refused: the live server disk {LIVE_ROOT_VOLUME_ID} is not visible as attached ({other:?}); wrong account or region, nothing deleted"
            ));
            return report;
        }
    }
    let dry_run = mode == Mode::Dry;
    let mut to_verify_volumes: Vec<&str> = Vec::new();
    let mut to_verify_snapshots: Vec<&str> = Vec::new();

    for id in VOLUME_IDS {
        match aws.volume(id).await {
            Err(why) => report.fail(format!("{id}: could not read: {why}")),
            Ok(VolumeLookup::Gone) => report.note(format!("{id}: already gone")),
            Ok(lookup @ VolumeLookup::Found { .. }) if volume_counts_as_gone(&lookup) => {
                report.note(format!("{id}: already gone (being deleted by AWS)"))
            }
            Ok(VolumeLookup::Found { state, attachments }) => {
                if !volume_is_deletable(&state, attachments) {
                    report.fail(format!(
                        "{id}: NOT deleted: state {state}, {attachments} attachment(s)"
                    ));
                    continue;
                }
                let outcome = aws.delete_volume(id, dry_run).await;
                let deleted = outcome == CallOutcome::Done;
                record_call(&mut report, id, mode, outcome);
                if deleted && mode == Mode::Apply {
                    to_verify_volumes.push(id);
                }
            }
        }
    }

    for id in SNAPSHOT_IDS {
        match aws.snapshot_exists(id).await {
            Err(why) => report.fail(format!("{id}: could not read: {why}")),
            Ok(false) => report.note(format!("{id}: already gone")),
            Ok(true) => {
                let outcome = aws.delete_snapshot(id, dry_run).await;
                let deleted = outcome == CallOutcome::Done;
                record_call(&mut report, id, mode, outcome);
                if deleted && mode == Mode::Apply {
                    to_verify_snapshots.push(id);
                }
            }
        }
    }

    if mode == Mode::Apply && (!to_verify_volumes.is_empty() || !to_verify_snapshots.is_empty()) {
        verify(aws, &to_verify_volumes, &to_verify_snapshots, &mut report).await;
    }
    report
}

async fn verify<A: DiskAws>(aws: &A, volumes: &[&str], snapshots: &[&str], report: &mut Report) {
    let mut left: Vec<String> = Vec::new();
    for attempt in 0..VERIFY_ATTEMPTS {
        if attempt > 0 {
            aws.pause().await;
        }
        left.clear();
        for id in volumes {
            match aws.volume(id).await {
                Ok(lookup) if volume_counts_as_gone(&lookup) => {}
                Ok(_) => left.push(format!("{id} still present")),
                Err(why) => left.push(format!("{id} unreadable: {why}")),
            }
        }
        for id in snapshots {
            match aws.snapshot_exists(id).await {
                Ok(false) => {}
                Ok(true) => left.push(format!("{id} still present")),
                Err(why) => left.push(format!("{id} unreadable: {why}")),
            }
        }
        if left.is_empty() {
            report.note("verify: every deleted id is gone".to_string());
            return;
        }
    }
    report.fail(format!("verify failed: {}", left.join("; ")));
}

/// The real AWS surface, over the EC2 client.
pub struct SdkDiskAws {
    client: aws_sdk_ec2::Client,
}

impl SdkDiskAws {
    /// Wraps an EC2 client.
    pub fn new(client: aws_sdk_ec2::Client) -> Self {
        Self { client }
    }
}

/// AWS's code and message, or the full error chain when there is no code
/// (a timeout, a DNS or TLS failure, a credentials error).
fn error_text<E>(e: &E) -> String
where
    E: aws_sdk_ec2::error::ProvideErrorMetadata + std::error::Error,
{
    match e.code() {
        Some(code) => format!("{code}: {}", e.message().unwrap_or("no message")),
        None => format!("{}", aws_sdk_ec2::error::DisplayErrorContext(e)),
    }
}

impl DiskAws for SdkDiskAws {
    async fn volume(&self, id: &str) -> Result<VolumeLookup, String> {
        use aws_sdk_ec2::error::ProvideErrorMetadata as _;
        match self.client.describe_volumes().volume_ids(id).send().await {
            Ok(out) => Ok(match out.volumes().first() {
                Some(v) => VolumeLookup::Found {
                    state: v
                        .state()
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_default(),
                    attachments: v.attachments().len(),
                },
                None => VolumeLookup::Gone,
            }),
            Err(e) if e.code() == Some("InvalidVolume.NotFound") => Ok(VolumeLookup::Gone),
            Err(e) => Err(error_text(&e)),
        }
    }

    async fn snapshot_exists(&self, id: &str) -> Result<bool, String> {
        use aws_sdk_ec2::error::ProvideErrorMetadata as _;
        let call = self
            .client
            .describe_snapshots()
            .owner_ids("self")
            .snapshot_ids(id)
            .send()
            .await;
        match call {
            Ok(out) => Ok(!out.snapshots().is_empty()),
            Err(e) if e.code() == Some("InvalidSnapshot.NotFound") => Ok(false),
            Err(e) => Err(error_text(&e)),
        }
    }

    async fn delete_volume(&self, id: &str, dry_run: bool) -> CallOutcome {
        use aws_sdk_ec2::error::ProvideErrorMetadata as _;
        let call = self
            .client
            .delete_volume()
            .volume_id(id)
            .dry_run(dry_run)
            .send()
            .await;
        match call {
            Ok(_) => CallOutcome::Done,
            Err(e) if e.code() == Some("DryRunOperation") => CallOutcome::WouldSucceed,
            Err(e) => CallOutcome::Failed(error_text(&e)),
        }
    }

    async fn delete_snapshot(&self, id: &str, dry_run: bool) -> CallOutcome {
        use aws_sdk_ec2::error::ProvideErrorMetadata as _;
        let call = self
            .client
            .delete_snapshot()
            .snapshot_id(id)
            .dry_run(dry_run)
            .send()
            .await;
        match call {
            Ok(_) => CallOutcome::Done,
            Err(e) if e.code() == Some("DryRunOperation") => CallOutcome::WouldSucceed,
            Err(e) => CallOutcome::Failed(error_text(&e)),
        }
    }

    async fn pause(&self) {
        tokio::time::sleep(std::time::Duration::from_secs(VERIFY_PAUSE_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const BEFORE: i64 = DEADLINE_UNIX_SECS - 86_400;

    /// Fake AWS: volumes and snapshots in maps; deletes remove them unless a
    /// failure is configured; records every delete call.
    #[derive(Default)]
    struct Fake {
        volumes: RefCell<HashMap<String, (String, usize)>>,
        snapshots: RefCell<Vec<String>>,
        read_error: Option<String>,
        delete_error: Option<String>,
        /// A deleted volume shows `deleting` for this many reads, then is gone.
        deleting_reads: RefCell<HashMap<String, usize>>,
        sticky_snapshot: bool,
        sticky_volume: bool,
        calls: RefCell<Vec<String>>,
        pauses: RefCell<usize>,
    }

    impl Fake {
        fn leftovers() -> Self {
            let f = Self::default();
            for id in VOLUME_IDS {
                f.volumes
                    .borrow_mut()
                    .insert(id.to_string(), ("available".to_string(), 0));
            }
            f.volumes
                .borrow_mut()
                .insert(LIVE_ROOT_VOLUME_ID.to_string(), ("in-use".to_string(), 1));
            for id in SNAPSHOT_IDS {
                f.snapshots.borrow_mut().push(id.to_string());
            }
            f
        }
    }

    impl Fake {
        fn only_live_root() -> Self {
            let f = Self::default();
            f.volumes
                .borrow_mut()
                .insert(LIVE_ROOT_VOLUME_ID.to_string(), ("in-use".to_string(), 1));
            f
        }
    }

    impl DiskAws for Fake {
        async fn volume(&self, id: &str) -> Result<VolumeLookup, String> {
            if let Some(e) = &self.read_error {
                return Err(e.clone());
            }
            let mut deleting = self.deleting_reads.borrow_mut();
            if let Some(n) = deleting.get_mut(id) {
                if *n > 0 {
                    *n -= 1;
                    return Ok(VolumeLookup::Found {
                        state: "deleting".to_string(),
                        attachments: 0,
                    });
                }
                return Ok(VolumeLookup::Gone);
            }
            Ok(match self.volumes.borrow().get(id) {
                Some((state, attachments)) => VolumeLookup::Found {
                    state: state.clone(),
                    attachments: *attachments,
                },
                None => VolumeLookup::Gone,
            })
        }

        async fn snapshot_exists(&self, id: &str) -> Result<bool, String> {
            if let Some(e) = &self.read_error {
                return Err(e.clone());
            }
            Ok(self.snapshots.borrow().iter().any(|s| s == id))
        }

        async fn delete_volume(&self, id: &str, dry_run: bool) -> CallOutcome {
            self.calls
                .borrow_mut()
                .push(format!("vol {id} dry={dry_run}"));
            if let Some(e) = &self.delete_error {
                return CallOutcome::Failed(e.clone());
            }
            if dry_run {
                return CallOutcome::WouldSucceed;
            }
            if !self.sticky_volume {
                self.volumes.borrow_mut().remove(id);
                self.deleting_reads.borrow_mut().insert(id.to_string(), 1);
            }
            CallOutcome::Done
        }

        async fn delete_snapshot(&self, id: &str, dry_run: bool) -> CallOutcome {
            self.calls
                .borrow_mut()
                .push(format!("snap {id} dry={dry_run}"));
            if let Some(e) = &self.delete_error {
                return CallOutcome::Failed(e.clone());
            }
            if dry_run {
                return CallOutcome::WouldSucceed;
            }
            if !self.sticky_snapshot {
                self.snapshots.borrow_mut().retain(|s| s != id);
            }
            CallOutcome::Done
        }

        async fn pause(&self) {
            *self.pauses.borrow_mut() += 1;
        }
    }

    fn block<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    #[test]
    fn test_ids_are_exactly_the_four_quote_52_leftovers_and_never_the_live_root() {
        assert_eq!(
            VOLUME_IDS,
            ["vol-0c6ab6e593e39d8c8", "vol-073ccaa417a0f344b"]
        );
        assert_eq!(
            SNAPSHOT_IDS,
            ["snap-090ed9c4f3df0ca61", "snap-0573ab07252f67bf3"]
        );
        assert!(!VOLUME_IDS.contains(&LIVE_ROOT_VOLUME_ID));
        assert!(!SNAPSHOT_IDS.contains(&LIVE_ROOT_VOLUME_ID));
    }

    #[test]
    fn test_deadline_is_2026_11_14_midnight_utc() {
        let dt = chrono::DateTime::from_timestamp(DEADLINE_UNIX_SECS, 0).expect("valid");
        assert_eq!(dt.to_rfc3339(), "2026-11-14T00:00:00+00:00");
    }

    #[test]
    fn test_parse_mode_accepts_only_dry_and_apply() {
        assert_eq!(parse_mode("dry"), Some(Mode::Dry));
        assert_eq!(parse_mode("apply"), Some(Mode::Apply));
        for bad in ["", "APPLY", "Dry", "apply ", "delete", "yes"] {
            assert_eq!(parse_mode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn test_volume_is_deletable_only_when_available_and_unattached() {
        assert!(volume_is_deletable("available", 0));
        assert!(!volume_is_deletable("available", 1));
        assert!(!volume_is_deletable("in-use", 0));
        assert!(!volume_is_deletable("in-use", 1));
        assert!(!volume_is_deletable("creating", 0));
        assert!(!volume_is_deletable("", 0));
    }

    #[test]
    fn test_volume_counts_as_gone_for_gone_or_deleting_only() {
        assert!(volume_counts_as_gone(&VolumeLookup::Gone));
        let found = |s: &str| VolumeLookup::Found {
            state: s.to_string(),
            attachments: 0,
        };
        assert!(volume_counts_as_gone(&found("deleting")));
        assert!(!volume_counts_as_gone(&found("available")));
        assert!(!volume_counts_as_gone(&found("in-use")));
    }

    #[test]
    fn test_dry_run_deletes_nothing_and_confirms_all_four() {
        let f = Fake::leftovers();
        let r = block(run(&f, Mode::Dry, BEFORE));
        assert!(r.ok, "{:?}", r.lines);
        assert_eq!(f.volumes.borrow().len(), 3);
        assert_eq!(f.snapshots.borrow().len(), 2);
        assert_eq!(f.calls.borrow().len(), 4);
        assert!(f.calls.borrow().iter().all(|c| c.ends_with("dry=true")));
        assert_eq!(*f.pauses.borrow(), 0);
    }

    #[test]
    fn test_apply_deletes_all_four_verifies_and_never_touches_the_live_root() {
        let f = Fake::leftovers();
        let r = block(run(&f, Mode::Apply, BEFORE));
        assert!(r.ok, "{:?}", r.lines);
        assert_eq!(
            f.volumes.borrow().keys().cloned().collect::<Vec<_>>(),
            vec![LIVE_ROOT_VOLUME_ID.to_string()]
        );
        assert!(f.snapshots.borrow().is_empty());
        assert!(
            !f.calls
                .borrow()
                .iter()
                .any(|c| c.contains(LIVE_ROOT_VOLUME_ID))
        );
        assert!(
            r.lines
                .iter()
                .any(|l| l == "verify: every deleted id is gone")
        );
    }

    #[test]
    fn test_apply_a_second_time_reports_already_gone_and_succeeds() {
        let f = Fake::only_live_root();
        let r = block(run(&f, Mode::Apply, BEFORE));
        assert!(r.ok, "{:?}", r.lines);
        assert_eq!(
            r.lines
                .iter()
                .filter(|l| l.contains("already gone"))
                .count(),
            4
        );
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn test_an_attached_or_in_use_volume_is_never_deleted() {
        for (state, attachments) in [("in-use", 1), ("available", 1), ("in-use", 0)] {
            let f = Fake::leftovers();
            f.volumes
                .borrow_mut()
                .insert(VOLUME_IDS[0].to_string(), (state.to_string(), attachments));
            let r = block(run(&f, Mode::Apply, BEFORE));
            assert!(!r.ok, "{state} {attachments}");
            assert!(!f.calls.borrow().iter().any(|c| c.contains(VOLUME_IDS[0])));
            assert!(f.volumes.borrow().contains_key(VOLUME_IDS[0]));
        }
    }

    #[test]
    fn test_refused_after_the_deadline_with_no_aws_call() {
        let f = Fake::leftovers();
        for now in [DEADLINE_UNIX_SECS, DEADLINE_UNIX_SECS + 1, i64::MAX] {
            let r = block(run(&f, Mode::Apply, now));
            assert!(!r.ok);
            assert!(r.lines[0].starts_with("refused"));
        }
        assert!(f.calls.borrow().is_empty());
        assert_eq!(f.volumes.borrow().len(), 3);
    }

    #[test]
    fn test_a_permission_refusal_fails_the_run() {
        let mut f = Fake::leftovers();
        f.delete_error = Some("UnauthorizedOperation: no".to_string());
        for mode in [Mode::Dry, Mode::Apply] {
            let r = block(run(&f, mode, BEFORE));
            assert!(!r.ok);
            assert!(r.lines.iter().any(|l| l.contains("UnauthorizedOperation")));
        }
        assert_eq!(f.volumes.borrow().len(), 3);
    }

    #[test]
    fn test_an_unreadable_volume_fails_and_deletes_nothing() {
        let mut f = Fake::leftovers();
        f.read_error = Some("Throttling: slow down".to_string());
        let r = block(run(&f, Mode::Apply, BEFORE));
        assert!(!r.ok);
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn test_verify_accepts_a_volume_still_deleting_without_waiting() {
        let f = Fake::leftovers();
        let r = block(run(&f, Mode::Apply, BEFORE));
        assert!(r.ok);
        // `deleting` counts as gone, so no pause is needed.
        assert_eq!(*f.pauses.borrow(), 0);
    }

    #[test]
    fn test_verify_fails_when_a_snapshot_never_disappears() {
        let mut f = Fake::leftovers();
        f.sticky_snapshot = true;
        let r = block(run(&f, Mode::Apply, BEFORE));
        assert!(!r.ok);
        assert_eq!(*f.pauses.borrow(), VERIFY_ATTEMPTS - 1);
        assert!(r.lines.last().expect("line").starts_with("verify failed"));
    }

    #[test]
    fn test_record_call_flags_a_mode_mismatch() {
        let mut r = Report {
            lines: Vec::new(),
            ok: true,
        };
        record_call(&mut r, "x", Mode::Dry, CallOutcome::Done);
        assert!(!r.ok);
        let mut r = Report {
            lines: Vec::new(),
            ok: true,
        };
        record_call(&mut r, "x", Mode::Apply, CallOutcome::WouldSucceed);
        assert!(!r.ok);
    }

    #[test]
    fn test_sdk_disk_aws_new_wraps_a_client() {
        let conf = aws_sdk_ec2::Config::builder()
            .behavior_version(aws_sdk_ec2::config::BehaviorVersion::latest())
            .region(aws_sdk_ec2::config::Region::new("ap-south-1"))
            .build();
        let aws = SdkDiskAws::new(aws_sdk_ec2::Client::from_conf(conf));
        assert_eq!(
            aws.client.config().region().map(|r| r.as_ref()),
            Some("ap-south-1")
        );
    }

    #[test]
    fn test_regression_wrong_account_or_region_is_refused_not_reported_gone() {
        // Every id reads "gone" when the run looks in the wrong place; the
        // live root check stops that from reporting OK.
        let f = Fake::default();
        for mode in [Mode::Dry, Mode::Apply] {
            let r = block(run(&f, mode, BEFORE));
            assert!(!r.ok, "{:?}", r.lines);
            assert!(r.lines[0].starts_with("refused: the live server disk"));
        }
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn test_live_root_not_attached_is_refused() {
        let f = Fake::leftovers();
        f.volumes.borrow_mut().insert(
            LIVE_ROOT_VOLUME_ID.to_string(),
            ("available".to_string(), 0),
        );
        let r = block(run(&f, Mode::Apply, BEFORE));
        assert!(!r.ok);
        assert!(f.calls.borrow().is_empty());
        assert_eq!(f.volumes.borrow().len(), 3);
    }

    #[test]
    fn test_a_rerun_that_finds_a_volume_deleting_succeeds_without_a_call() {
        for state in ["deleting", "deleted"] {
            let f = Fake::only_live_root();
            for id in VOLUME_IDS {
                f.volumes
                    .borrow_mut()
                    .insert(id.to_string(), (state.to_string(), 0));
            }
            let r = block(run(&f, Mode::Apply, BEFORE));
            assert!(r.ok, "{state}: {:?}", r.lines);
            assert!(f.calls.borrow().is_empty());
        }
    }

    #[test]
    fn test_verify_fails_when_a_volume_never_disappears() {
        let mut f = Fake::leftovers();
        f.sticky_volume = true;
        let r = block(run(&f, Mode::Apply, BEFORE));
        assert!(!r.ok);
        assert_eq!(*f.pauses.borrow(), VERIFY_ATTEMPTS - 1);
        let last = r.lines.last().expect("line");
        assert!(last.starts_with("verify failed") && last.contains(VOLUME_IDS[0]));
    }
}
