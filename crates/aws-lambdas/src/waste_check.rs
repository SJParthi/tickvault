//! Daily AWS waste check — a read-only Lambda that names what is billing
//! money while doing nothing (noise lock §2.18, owner card 2026-11-11
//! "Yes, daily waste check (Recommended)").
//!
//! Why it exists (Verified 2026-11-11): two EBS volumes attached to nothing
//! (a 600 GB old root and a 30 GB May root) plus two old snapshots billed
//! about $21 from 1 to 11 November and no message ever named them. The
//! hourly guard and the native budget see only totals.
//!
//! What one run does, once a day at 18:07 IST:
//!
//! 1. Reads, read-only: `ec2:DescribeVolumes` (status `available`),
//!    `ec2:DescribeSnapshots` (owner `self`), `ec2:DescribeAddresses`, and the
//!    previous run's state, one S3 object `waste-check/state.json` in the
//!    cold bucket (no size cap, so the full id set is always kept).
//! 2. Cost Explorer, context only: at most ONE `GetCostAndUsage` per IST day
//!    (month-to-date, grouped by service; the next page is never fetched;
//!    SDK retries are off, so one call is one billed request). The day is
//!    claimed first with a create-only S3 object
//!    `waste-check/claims/<date>-cost-query`, so a re-run, a test invoke or a
//!    duplicate delivery the same day sends no second query. If the claim or
//!    the query fails, the run carries on without the top-5 line.
//! 3. Findings: a volume seen `available` in runs at least 2 IST days apart
//!    (the first-seen date of each unattached volume is kept in the state, so
//!    a volume detached minutes ago is not reported the same evening); an
//!    owned snapshot older than 30 days; an Elastic IP with no association.
//!    The top 5 services by month-to-date cost are context, never a finding.
//! 4. Edge trigger ([`decide`]): ONE plain-English message through the alerts
//!    topic only when the set GAINS an item the previous run did not have. An
//!    unchanged or shrinking set sends nothing; a second message on the same
//!    IST day is refused (the new items wait for tomorrow).
//! 5. Before sending, the day is claimed with a create-only S3 object
//!    `waste-check/claims/<date>-message` (two runs delivered at once cannot
//!    both send), and the state is written as "sent today" FIRST (if that
//!    write fails, nothing is sent). Only then the message goes out, in ONE
//!    attempt (the SNS and claim clients have SDK retries off, so a publish
//!    SNS accepted is never sent a second time and a claim is never retried
//!    into its own 412). If SNS answers the send with a 4xx refusal (or the
//!    request was never sent), the previous state is put back so tomorrow
//!    sends the items. If the outcome is unknown (timeout, network error, a
//!    5xx or a lost reply: SNS may have delivered it), the "sent" state is
//!    kept. A failure therefore costs at most a missed message, never a
//!    repeat.
//! 6. Writes one audit file `s3://tv-<env>-cold/cost-audit/<YYYY-MM-DD>.json`
//!    (IST date).
//!
//! It can only look: the role has no delete, release, modify or stop action
//! (pinned by `crates/aws-lambdas/tests/waste_check_wiring_guard.rs`).
//!
//! **Honest limits.**
//! - The first run after deploy (or after an unreadable state) reports no
//!   unattached volume: each one is first seen that day and is reported two
//!   IST days later. A volume reattached and detached between two runs is
//!   not seen as moved and keeps its first-seen date.
//! - A snapshot with no start time is reported (age unknown): errs toward
//!   reporting.
//! - Prices are ap-south-1 list prices written as constants below (Assumed,
//!   not read from the pricing API), so every figure is an estimate. A
//!   snapshot is billed on its changed blocks, so its volume size gives an
//!   upper bound.
//! - Cost Explorer data lags by up to a day (Assumed, AWS docs); if the
//!   reply has a next page, the top-5 list is marked partial rather than
//!   paying for a second call. A test invoke earlier the same IST day uses
//!   up the day's query, so the 18:07 run then has no top-5 line.
//! - Names (`Name` tags, service names) are cut to plain ASCII letters,
//!   digits, spaces and a few punctuation marks before they reach the
//!   message, so a tag cannot reorder the line; a non-ASCII name loses its
//!   non-ASCII letters.
//! - A message lists at most 15 items and stays under the relay's 3,500
//!   character cap: the item lines stop early when they would not fit, and
//!   the count of the rest, the totals and the closing line are always kept.
//! - Any EC2 or state read failure ends the run with an error before
//!   anything is written: the previous state is never wiped by a partial
//!   read. Such a failure is visible in the Lambda log and as a missing
//!   audit file; §2.18 allows no alarm beyond the did-it-run check.
//!
//! Cold path: once a day, O(volumes + snapshots + addresses + services) with
//! a sort, bounded by `MAX_EC2_PAGES` pages per describe. Not on any hot path.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, Offset, Utc};
use lambda_runtime::Error;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{error, info, warn};

/// SNS subject. Deliberately free of the letters "ok" and "fail": the
/// Telegram relay picks its leading emoji from those words, and this is a
/// neutral notice (🔔), not a recovery or an emergency.
pub const WASTE_SUBJECT: &str = "AWS waste: new items costing money";

/// A volume is reported once it was first seen `available` at least this
/// many IST days before today (two daily runs 48 h apart).
pub const UNATTACHED_MIN_DAYS_SEEN: i64 = 2;
/// A snapshot must be older than this (30 days).
pub const OLD_SNAPSHOT_MIN_AGE_SECS: i64 = 30 * 86_400;

/// ap-south-1 list price, gp3 storage, USD per GB-month (estimate).
pub const EBS_GP3_USD_PER_GB_MONTH: f64 = 0.0912;
/// ap-south-1 list price, gp2 storage, USD per GB-month (estimate).
pub const EBS_GP2_USD_PER_GB_MONTH: f64 = 0.114;
/// ap-south-1 list price, io1/io2 storage only (provisioned IOPS are extra
/// and not counted), USD per GB-month (estimate).
pub const EBS_IO_USD_PER_GB_MONTH: f64 = 0.149;
/// Any other volume type: the gp2 rate as a rough estimate.
pub const EBS_OTHER_USD_PER_GB_MONTH: f64 = EBS_GP2_USD_PER_GB_MONTH;
/// Standard-tier snapshot storage, USD per GB-month (estimate, upper bound).
pub const SNAPSHOT_STANDARD_USD_PER_GB_MONTH: f64 = 0.05;
/// Archive-tier snapshot storage, USD per GB-month (estimate, upper bound).
pub const SNAPSHOT_ARCHIVE_USD_PER_GB_MONTH: f64 = 0.0125;
/// A public IPv4 address, USD per hour (estimate).
pub const PUBLIC_IPV4_USD_PER_HOUR: f64 = 0.005;
/// Hours in the billing month the estimates use.
pub const HOURS_PER_MONTH: f64 = 730.0;

/// How many services the context line names.
pub const TOP_SERVICES: usize = 5;
/// How many new items one message lists at most; the rest are counted.
pub const MAX_LISTED_ITEMS: usize = 15;
/// Page bound for each EC2 describe (1,000 items a page for snapshots).
pub const MAX_EC2_PAGES: usize = 20;
/// The longest message body: the Telegram relay cuts a plain body at
/// `PLAIN_FALLBACK_BODY_MAX_CHARS` (3,500), so keep 100 characters spare.
pub const WASTE_MESSAGE_MAX_CHARS: usize =
    crate::telegram_webhook::PLAIN_FALLBACK_BODY_MAX_CHARS - 100;
const _: () =
    assert!(WASTE_MESSAGE_MAX_CHARS < crate::telegram_webhook::PLAIN_FALLBACK_BODY_MAX_CHARS);
/// Longest name (tag or service) shown in a message, in characters.
pub const MAX_NAME_CHARS: usize = 48;

/// State format version.
pub const STATE_VERSION: u32 = 1;
/// The state object, in the audit (cold) bucket.
pub const STATE_KEY: &str = "waste-check/state.json";
/// The largest state object read back (the worst case, 20 pages of 1,000
/// snapshots, is well under 1 MiB).
pub const STATE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Prefix of the create-only claim objects (one per IST day and kind).
pub const CLAIM_PREFIX: &str = "waste-check/claims/";

/// What a day claim is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// The day's one Cost Explorer query.
    CostQuery,
    /// The day's one message.
    Message,
}

/// The claim object key for an IST date and a kind.
pub fn claim_key(date_ist: &str, kind: ClaimKind) -> String {
    let what = match kind {
        ClaimKind::CostQuery => "cost-query",
        ClaimKind::Message => "message",
    };
    format!("{CLAIM_PREFIX}{date_ist}-{what}")
}

/// A create-only write was refused because the object already exists (412)
/// or another create of it is in flight (409): the day is already claimed.
pub fn claim_already_taken(http_status: Option<u16>) -> bool {
    matches!(http_status, Some(412) | Some(409))
}

/// Why `sns:Publish` failed, as far as this run can know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// SNS did not accept the message: the request was never sent, or SNS
    /// answered with a 4xx refusal. Safe to put the old state back.
    Refused(String),
    /// The outcome is not known: a timeout, a network error, a 5xx or a
    /// reply that could not be read. SNS may have delivered the message, so
    /// the "sent" state is kept (a possible miss, never a repeat).
    Unknown(String),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "refused: {e}"),
            Self::Unknown(e) => write!(f, "outcome unknown: {e}"),
        }
    }
}

/// `true` only when a failed publish certainly did not deliver: the request
/// never left (`never_sent`, an SDK construction failure) or SNS answered
/// with a 4xx status. A timeout, a network error (no status), a 5xx or a 2xx
/// reply the SDK could not read all count as "maybe delivered".
pub fn publish_refused(never_sent: bool, http_status: Option<u16>) -> bool {
    never_sent || matches!(http_status, Some(400..=499))
}

/// What kind of waste a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    UnattachedDisk,
    OldSnapshot,
    IdleAddress,
}

/// One item billing money while doing nothing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub id: String,
    pub size_gib: Option<i64>,
    /// Whole days since the item's timestamp; `None` when AWS gave none.
    pub age_days: Option<i64>,
    pub est_monthly_usd: f64,
    /// The `Name` tag, or the public IP for an address.
    pub label: Option<String>,
    /// Disks only: the IST date this check first saw it attached to nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unattached_since: Option<String>,
}

/// The facts read from `DescribeVolumes`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VolumeFact {
    pub id: String,
    pub state: String,
    pub size_gib: i64,
    pub volume_type: String,
    pub created_unix: Option<i64>,
    pub name: Option<String>,
}

/// The facts read from `DescribeSnapshots`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnapshotFact {
    pub id: String,
    pub size_gib: i64,
    pub started_unix: Option<i64>,
    pub archived: bool,
    pub name: Option<String>,
}

/// The facts read from `DescribeAddresses`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AddressFact {
    pub allocation_id: Option<String>,
    pub public_ip: Option<String>,
    pub association_id: Option<String>,
    pub instance_id: Option<String>,
    pub network_interface_id: Option<String>,
}

/// One service's month-to-date cost.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServiceCost {
    pub service: String,
    pub usd: f64,
}

/// The raw `(service, amount)` rows of the one Cost Explorer reply.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServiceCostRows {
    pub rows: Vec<(String, String)>,
    /// The reply carried a next-page token that was deliberately not fetched.
    pub more_pages: bool,
}

/// What became of the day's one Cost Explorer query.
#[derive(Debug, Clone, PartialEq)]
pub enum CostOutcome {
    /// The query ran; `partial` = a next page existed and was not fetched.
    Read { partial: bool },
    /// An earlier run today already made the day's query (its claim exists).
    AlreadyQueriedToday,
    /// The claim or the query failed; the run carried on without the top-5.
    Unavailable(String),
}

impl CostOutcome {
    /// A short label for the audit file.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Read { partial: false } => "read",
            Self::Read { partial: true } => "read_partial",
            Self::AlreadyQueriedToday => "already_queried_today",
            Self::Unavailable(_) => "unavailable",
        }
    }

    /// The top-5 list is incomplete because a next page was not fetched.
    pub fn partial(&self) -> bool {
        matches!(self, Self::Read { partial: true })
    }
}

/// USD per GB-month for a volume type (estimate).
pub fn volume_price_per_gb(volume_type: &str) -> f64 {
    match volume_type {
        "gp3" => EBS_GP3_USD_PER_GB_MONTH,
        "gp2" => EBS_GP2_USD_PER_GB_MONTH,
        "io1" | "io2" => EBS_IO_USD_PER_GB_MONTH,
        _ => EBS_OTHER_USD_PER_GB_MONTH,
    }
}

/// Monthly cost of one idle public IPv4 address (estimate).
pub fn idle_address_monthly_usd() -> f64 {
    PUBLIC_IPV4_USD_PER_HOUR * HOURS_PER_MONTH
}

fn age_days(now_unix: i64, then_unix: Option<i64>) -> Option<i64> {
    then_unix.map(|t| now_unix.saturating_sub(t).max(0) / 86_400)
}

/// Clamp a size AWS reported as negative (never seen) to zero.
fn gib(size: i64) -> i64 {
    size.max(0)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_graphic())
}

fn parse_ist_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// The IST date each volume now `available` was first seen attached to
/// nothing: carried from the previous run, or `today_ist` for a volume not
/// seen before (a stored date later than today is clamped to today). A
/// volume no longer `available` is dropped, so its clock restarts if it is
/// detached again.
pub fn unattached_since_map(
    vols: &[VolumeFact],
    previous: &BTreeMap<String, String>,
    today_ist: &str,
) -> BTreeMap<String, String> {
    vols.iter()
        .filter(|v| v.state == "available" && valid_id(&v.id))
        .map(|v| {
            let since = match previous.get(&v.id) {
                Some(d) if d.as_str() <= today_ist => d.clone(),
                _ => today_ist.to_string(),
            };
            (v.id.clone(), since)
        })
        .collect()
}

/// Volumes in state `available` first seen attached to nothing at least
/// [`UNATTACHED_MIN_DAYS_SEEN`] IST days before `today_ist` (per `since`,
/// from [`unattached_since_map`]). A volume missing from `since` is not
/// reported.
pub fn unattached_volume_findings(
    vols: &[VolumeFact],
    now_unix: i64,
    since: &BTreeMap<String, String>,
    today_ist: NaiveDate,
) -> Vec<Finding> {
    vols.iter()
        .filter(|v| v.state == "available" && valid_id(&v.id))
        .filter_map(|v| {
            let first = since.get(&v.id)?;
            let first_date = parse_ist_date(first)?;
            if (today_ist - first_date).num_days() < UNATTACHED_MIN_DAYS_SEEN {
                return None;
            }
            Some(Finding {
                kind: FindingKind::UnattachedDisk,
                id: v.id.clone(),
                size_gib: Some(gib(v.size_gib)),
                age_days: age_days(now_unix, v.created_unix),
                est_monthly_usd: gib(v.size_gib) as f64 * volume_price_per_gb(&v.volume_type),
                label: v.name.clone(),
                unattached_since: Some(first.clone()),
            })
        })
        .collect()
}

/// Owned snapshots older than 30 days (or with no start time at all).
pub fn old_snapshot_findings(snaps: &[SnapshotFact], now_unix: i64) -> Vec<Finding> {
    snaps
        .iter()
        .filter(|s| valid_id(&s.id))
        .filter(|s| match s.started_unix {
            Some(t) => now_unix.saturating_sub(t) > OLD_SNAPSHOT_MIN_AGE_SECS,
            None => true,
        })
        .map(|s| {
            let rate = if s.archived {
                SNAPSHOT_ARCHIVE_USD_PER_GB_MONTH
            } else {
                SNAPSHOT_STANDARD_USD_PER_GB_MONTH
            };
            Finding {
                kind: FindingKind::OldSnapshot,
                id: s.id.clone(),
                size_gib: Some(gib(s.size_gib)),
                age_days: age_days(now_unix, s.started_unix),
                est_monthly_usd: gib(s.size_gib) as f64 * rate,
                label: s.name.clone(),
                unattached_since: None,
            }
        })
        .collect()
}

/// Elastic IPs associated with nothing. An address with neither an
/// allocation id nor a public IP cannot be named or released and is skipped.
pub fn idle_address_findings(addrs: &[AddressFact]) -> Vec<Finding> {
    addrs
        .iter()
        .filter(|a| {
            a.association_id.is_none()
                && a.instance_id.is_none()
                && a.network_interface_id.is_none()
        })
        .filter_map(|a| {
            let id = a.allocation_id.clone().or_else(|| a.public_ip.clone())?;
            if !valid_id(&id) {
                return None;
            }
            Some(Finding {
                kind: FindingKind::IdleAddress,
                id,
                size_gib: None,
                age_days: None,
                est_monthly_usd: idle_address_monthly_usd(),
                label: a.public_ip.clone(),
                unattached_since: None,
            })
        })
        .collect()
}

/// Every finding, sorted by id, one per id. An id that is not a short
/// printable ASCII string (never seen from AWS) is skipped, so the stored
/// state always parses back.
pub fn collect_findings(
    vols: &[VolumeFact],
    snaps: &[SnapshotFact],
    addrs: &[AddressFact],
    now_unix: i64,
    unattached_since: &BTreeMap<String, String>,
    today_ist: NaiveDate,
) -> Vec<Finding> {
    let mut all = unattached_volume_findings(vols, now_unix, unattached_since, today_ist);
    all.extend(old_snapshot_findings(snaps, now_unix));
    all.extend(idle_address_findings(addrs));
    all.sort_by(|a, b| a.id.cmp(&b.id));
    all.dedup_by(|a, b| a.id == b.id);
    all
}

/// The top `TOP_SERVICES` services by month-to-date cost. Amounts that are
/// not finite numbers are skipped (counted in the second value); duplicate
/// service names are summed; zero or negative totals are left out.
pub fn top_services(rows: &[(String, String)]) -> (Vec<ServiceCost>, usize) {
    let mut totals: BTreeMap<&str, f64> = BTreeMap::new();
    let mut skipped = 0usize;
    for (service, amount) in rows {
        match amount.trim().parse::<f64>() {
            Ok(v) if v.is_finite() => *totals.entry(service.as_str()).or_insert(0.0) += v,
            _ => skipped += 1,
        }
    }
    let mut out: Vec<ServiceCost> = totals
        .into_iter()
        .filter(|(_, usd)| *usd > 0.0)
        .map(|(service, usd)| ServiceCost {
            service: service.to_string(),
            usd,
        })
        .collect();
    out.sort_by(|a, b| {
        b.usd
            .total_cmp(&a.usd)
            .then_with(|| a.service.cmp(&b.service))
    });
    out.truncate(TOP_SERVICES);
    (out, skipped)
}

/// The Cost Explorer period: first day of the UTC month to tomorrow
/// (exclusive end, so today's partial spend is included).
pub fn cost_period(today_utc: NaiveDate) -> (NaiveDate, NaiveDate) {
    let start = today_utc.with_day(1).unwrap_or(today_utc);
    (start, today_utc + Duration::days(1))
}

// ---------------------------------------------------------------------------
// State + the edge-trigger decision
// ---------------------------------------------------------------------------

/// The persisted state: every finding id of the last run (no cap, the state
/// is an S3 object), the first-seen IST date of each unattached volume, and
/// the IST date of the last message sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WasteState {
    pub v: u32,
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub unattached_since: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_ist: Option<String>,
}

/// What the previous run left behind.
#[derive(Debug, Clone, PartialEq)]
pub enum PreviousState {
    /// The state object does not exist yet.
    FirstRun,
    /// The state object could not be read as a state (the reason is kept).
    Corrupt(String),
    Ids {
        ids: BTreeSet<String>,
        unattached_since: BTreeMap<String, String>,
        last_message_ist: Option<String>,
    },
}

impl PreviousState {
    fn last_message_ist(&self) -> Option<&str> {
        match self {
            Self::Ids {
                last_message_ist, ..
            } => last_message_ist.as_deref(),
            Self::FirstRun | Self::Corrupt(_) => None,
        }
    }

    /// The previous run's finding ids (empty for a first run or an
    /// unreadable state).
    pub fn ids(&self) -> BTreeSet<String> {
        match self {
            Self::Ids { ids, .. } => ids.clone(),
            Self::FirstRun | Self::Corrupt(_) => BTreeSet::new(),
        }
    }

    /// The previous run's first-seen dates of unattached volumes.
    pub fn unattached_since(&self) -> BTreeMap<String, String> {
        match self {
            Self::Ids {
                unattached_since, ..
            } => unattached_since.clone(),
            Self::FirstRun | Self::Corrupt(_) => BTreeMap::new(),
        }
    }

    /// A short label for the audit file.
    pub fn label(&self) -> &'static str {
        match self {
            Self::FirstRun => "first_run",
            Self::Corrupt(_) => "unreadable",
            Self::Ids { .. } => "ids",
        }
    }
}

/// Parse the stored state. `None` (object not found) is a first run;
/// anything that is not a well-formed version-1 state is `Corrupt`, never an
/// error (the run then treats every current finding as new, once).
pub fn parse_state(raw: Option<&str>) -> PreviousState {
    let Some(raw) = raw else {
        return PreviousState::FirstRun;
    };
    let state: WasteState = match serde_json::from_str(raw) {
        Ok(s) => s,
        Err(e) => return PreviousState::Corrupt(format!("not state JSON: {e}")),
    };
    if state.v != STATE_VERSION {
        return PreviousState::Corrupt(format!("unknown state version {}", state.v));
    }
    if let Some(bad) = state.ids.iter().find(|id| !valid_id(id)) {
        return PreviousState::Corrupt(format!("malformed id {bad:?}"));
    }
    if let Some((id, date)) = state
        .unattached_since
        .iter()
        .find(|(id, date)| !valid_id(id) || parse_ist_date(date).is_none())
    {
        return PreviousState::Corrupt(format!("malformed first-seen entry {id:?} {date:?}"));
    }
    let last = state
        .last_message_ist
        .filter(|d| parse_ist_date(d).is_some());
    PreviousState::Ids {
        ids: state.ids.into_iter().collect(),
        unattached_since: state.unattached_since,
        last_message_ist: last,
    }
}

/// The state to store.
pub fn state_from(
    ids: &BTreeSet<String>,
    unattached_since: &BTreeMap<String, String>,
    last_message_ist: Option<String>,
) -> WasteState {
    WasteState {
        v: STATE_VERSION,
        ids: ids.iter().cloned().collect(),
        unattached_since: unattached_since.clone(),
        last_message_ist,
    }
}

/// The outcome of one run's comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Ids that read as new this run (sorted).
    pub new_ids: Vec<String>,
    /// Send one message now.
    pub send: bool,
    pub reason: &'static str,
    /// The state to write after this run (when `send`, it records the
    /// message as sent today and is written BEFORE the message goes out).
    pub next_state: WasteState,
    /// The state to put back if the message cannot be sent: the previous
    /// items still present and the previous message date, so the new items
    /// read as new again tomorrow.
    pub revert_state: WasteState,
}

/// The edge trigger. `today_ist` is `YYYY-MM-DD`; `unattached_since` is
/// this run's first-seen map, carried into every state written.
///
/// - No findings, or no new ones: nothing is sent; the state becomes the
///   current set (so a resolved item that comes back reads as new again).
/// - New ones on a day a message was already sent: nothing is sent; the
///   state keeps only the previous items still present, so the new ones read
///   as new again tomorrow.
/// - Otherwise: one message; the state becomes the current set and today.
pub fn decide(
    prev: &PreviousState,
    current: &BTreeSet<String>,
    unattached_since: &BTreeMap<String, String>,
    today_ist: &str,
) -> Decision {
    let last = prev.last_message_ist().map(str::to_string);
    let kept: BTreeSet<String> = prev.ids().intersection(current).cloned().collect();
    let revert_state = state_from(&kept, unattached_since, last.clone());
    let (new_ids, base_reason): (Vec<String>, &'static str) = match prev {
        PreviousState::FirstRun => (current.iter().cloned().collect(), "first_run"),
        PreviousState::Corrupt(_) => (current.iter().cloned().collect(), "state_unreadable"),
        PreviousState::Ids { ids, .. } => {
            (current.difference(ids).cloned().collect(), "new_findings")
        }
    };

    let (send, reason, next_state) = if current.is_empty() {
        (
            false,
            "no_findings",
            state_from(current, unattached_since, last),
        )
    } else if new_ids.is_empty() {
        (
            false,
            "no_new_findings",
            state_from(current, unattached_since, last),
        )
    } else if last.as_deref() == Some(today_ist) {
        // FirstRun / Corrupt never carry a date, so `kept` is the safe
        // answer for them too: send nothing, remember nothing new.
        (false, "already_sent_today", revert_state.clone())
    } else {
        (
            true,
            base_reason,
            state_from(current, unattached_since, Some(today_ist.to_string())),
        )
    };
    Decision {
        new_ids: if current.is_empty() {
            Vec::new()
        } else {
            new_ids
        },
        send,
        reason,
        next_state,
        revert_state,
    }
}

// ---------------------------------------------------------------------------
// Message + audit
// ---------------------------------------------------------------------------

/// Punctuation a name may keep in the message. Everything else that is not
/// an ASCII letter or digit is dropped: control characters, Markdown marks
/// (`_ * ` [ ]`) and every bidirectional or zero-width format character
/// (for example U+202E, U+2066..U+2069, U+200D), so a `Name` tag cannot
/// reorder or hide the size and cost written beside it.
///
/// `:` `/` `.` `@` and `#` are dropped too (2026-11-11 review): Telegram
/// turns a URL, a bare domain (`evil.example`), an email address, a
/// `@mention` or a `#hashtag` in plain text into a link, so a `Name` tag
/// could otherwise put a clickable link inside a trusted alert. Without them
/// a tag such as `https://evil.example/login` reads `httpsevilexamplelogin`.
const NAME_PUNCTUATION: &[char] = &[' ', '-', ',', '+', '=', '(', ')', '&', '%', '\''];

/// Keep a name AWS returned safe and short for a plain-text message.
fn clean(s: &str) -> String {
    let kept: Vec<char> = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || NAME_PUNCTUATION.contains(c))
        .collect();
    let mut out: String = kept.iter().take(MAX_NAME_CHARS).collect();
    if kept.len() > MAX_NAME_CHARS {
        out.push('…');
    }
    out
}

/// The fixed IST offset (+05:30); the fallback arm is unreachable (19,800 s
/// is in range) and exists only to avoid `unwrap`.
fn ist() -> FixedOffset {
    FixedOffset::east_opt(crate::time::IST_OFFSET_SECS).unwrap_or_else(|| Utc.fix())
}

/// "6:07 PM IST, 11 Nov 2026".
pub fn ist_stamp(now_utc: DateTime<Utc>) -> String {
    now_utc
        .with_timezone(&ist())
        .format("%-I:%M %p IST, %-d %b %Y")
        .to_string()
}

/// "2026-11-09" -> "9 Nov 2026" (the raw text if it is not a date).
fn day_phrase(date_ist: &str) -> String {
    parse_ist_date(date_ist)
        .map(|d| d.format("%-d %b %Y").to_string())
        .unwrap_or_else(|| date_ist.to_string())
}

fn age_phrase(age: Option<i64>, what: &str) -> String {
    match age {
        Some(0) => format!("{what} today"),
        Some(1) => format!("{what} 1 day ago"),
        Some(d) => format!("{what} {d} days ago"),
        None => format!("{what} at an unknown time"),
    }
}

/// The label text shown beside an item. An address's label is the public IP
/// AWS assigned (never a tag), shown as a parsed IP so its dots and colons
/// survive; anything else, or a label that does not parse as an IP, goes
/// through [`clean`].
fn label_text(kind: FindingKind, label: &str) -> String {
    if kind == FindingKind::IdleAddress
        && let Ok(ip) = label.parse::<std::net::IpAddr>()
    {
        return ip.to_string();
    }
    clean(label)
}

fn item_lines(n: usize, f: &Finding) -> [String; 2] {
    let name = f
        .label
        .as_deref()
        .map(|l| format!(" ({})", label_text(f.kind, l)))
        .unwrap_or_default();
    let size = f.size_gib.unwrap_or(0);
    match f.kind {
        FindingKind::UnattachedDisk => [
            format!(
                "{n}. Disk {}{name}: {size} GB, attached to nothing since at least {}, {}. About ${:.2} a month.",
                f.id,
                day_phrase(f.unattached_since.as_deref().unwrap_or("")),
                age_phrase(f.age_days, "made"),
                f.est_monthly_usd
            ),
            format!(
                "   To delete: EC2 console, Volumes, select {}, Actions, Delete volume.",
                f.id
            ),
        ],
        FindingKind::OldSnapshot => [
            format!(
                "{n}. Snapshot {}{name}: {size} GB, {}. Up to ${:.2} a month.",
                f.id,
                age_phrase(f.age_days, "taken"),
                f.est_monthly_usd
            ),
            format!(
                "   To delete: EC2 console, Snapshots, select {}, Actions, Delete snapshot.",
                f.id
            ),
        ],
        FindingKind::IdleAddress => [
            format!(
                "{n}. Fixed IP address {}{name}: reserved but used by nothing. About ${:.2} a month.",
                f.id, f.est_monthly_usd
            ),
            format!(
                "   To release: EC2 console, Elastic IPs, select {}, Actions, Release Elastic IP addresses.",
                f.id
            ),
        ],
    }
}

fn more_line(rest: usize) -> String {
    format!("...and {rest} more, listed in today's cost audit file.")
}

/// Characters of `lines` joined by newlines (one newline per line counted).
fn chars_with_newlines(lines: &[String]) -> usize {
    lines.iter().map(|l| l.chars().count() + 1).sum()
}

/// Render the one message. `new` are the new findings (in id order),
/// `all` every finding of this run. The item lines stop early when the next
/// one would push the body past [`WASTE_MESSAGE_MAX_CHARS`]; the count of
/// the rest, the totals, the cost line and the closing line are always kept.
pub fn render_message(
    new: &[&Finding],
    all: &[Finding],
    top: &[ServiceCost],
    top_partial: bool,
    now_utc: DateTime<Utc>,
) -> String {
    let new_usd: f64 = new.iter().map(|f| f.est_monthly_usd).sum();
    let all_usd: f64 = all.iter().map(|f| f.est_monthly_usd).sum();
    let plural = if new.len() == 1 { "item" } else { "items" };
    let head = vec![
        format!(
            "{} new {plural} billing money while doing nothing. Together about ${new_usd:.2} a month (₹{:.0} with GST, estimate).",
            new.len(),
            crate::budget_digest::inr(new_usd)
        ),
        String::new(),
    ];
    let mut tail = vec![
        String::new(),
        format!(
            "All waste found today: {} {}, about ${all_usd:.2} a month.",
            all.len(),
            if all.len() == 1 { "item" } else { "items" }
        ),
    ];
    if top.is_empty() {
        tail.push("Top costs this month: not available today.".to_string());
    } else {
        let list: Vec<String> = top
            .iter()
            .map(|s| format!("{} ${:.2}", clean(&s.service), s.usd))
            .collect();
        let partial = if top_partial { " (partial list)" } else { "" };
        tail.push(format!(
            "Top costs this month so far{partial}: {}.",
            list.join(", ")
        ));
    }
    tail.push(format!(
        "This check only looks; it deleted nothing. Checked {}.",
        ist_stamp(now_utc)
    ));

    let mut used = chars_with_newlines(&head) + chars_with_newlines(&tail);
    let mut items: Vec<String> = Vec::new();
    let mut listed = 0usize;
    for f in new.iter().take(MAX_LISTED_ITEMS) {
        let lines = item_lines(listed + 1, f);
        let cost = chars_with_newlines(&lines);
        let rest_after = new.len() - listed - 1;
        let more = if rest_after > 0 {
            more_line(rest_after).chars().count() + 1
        } else {
            0
        };
        if used + cost + more > WASTE_MESSAGE_MAX_CHARS {
            break;
        }
        items.extend(lines);
        used += cost;
        listed += 1;
    }
    if listed < new.len() {
        items.push(more_line(new.len() - listed));
    }

    let mut lines = head;
    lines.extend(items);
    lines.extend(tail);
    lines.join("\n")
}

/// The audit object key for an IST date.
pub fn audit_key(date_ist: &str) -> String {
    format!("cost-audit/{date_ist}.json")
}

/// Everything one run saw and decided, for the audit file.
#[derive(Debug, Clone, Copy)]
pub struct AuditInput<'a> {
    pub now_utc: DateTime<Utc>,
    pub date_ist: &'a str,
    pub findings: &'a [Finding],
    pub previous: &'a PreviousState,
    pub decision: &'a Decision,
    pub message_sent: bool,
    /// Why the message was not sent although the decision said send.
    pub send_error: Option<&'a str>,
    pub top: &'a [ServiceCost],
    pub cost: &'a CostOutcome,
    pub cost_rows_skipped: usize,
}

/// The audit file body.
pub fn audit_document(input: &AuditInput<'_>) -> Value {
    let total: f64 = input.findings.iter().map(|f| f.est_monthly_usd).sum();
    let cost_error = match input.cost {
        CostOutcome::Unavailable(e) => Some(e.as_str()),
        CostOutcome::Read { .. } | CostOutcome::AlreadyQueriedToday => None,
    };
    json!({
        "v": 1,
        "date_ist": input.date_ist,
        "checked_at_utc": input.now_utc.to_rfc3339(),
        "findings": input.findings,
        "all_monthly_usd_estimate": total,
        "new_ids": input.decision.new_ids,
        "decision": input.decision.reason,
        "message_sent": input.message_sent,
        "send_error": input.send_error,
        "previous_state": input.previous.label(),
        "unattached_since": input.decision.next_state.unattached_since,
        "top_services_month_to_date": input.top,
        "top_services_partial": input.cost.partial(),
        "cost_query": input.cost.label(),
        "cost_query_error": cost_error,
        "cost_rows_skipped": input.cost_rows_skipped,
        "prices_usd_estimate": {
            "ebs_gp3_per_gb_month": EBS_GP3_USD_PER_GB_MONTH,
            "ebs_gp2_per_gb_month": EBS_GP2_USD_PER_GB_MONTH,
            "ebs_io_per_gb_month": EBS_IO_USD_PER_GB_MONTH,
            "snapshot_standard_per_gb_month": SNAPSHOT_STANDARD_USD_PER_GB_MONTH,
            "snapshot_archive_per_gb_month": SNAPSHOT_ARCHIVE_USD_PER_GB_MONTH,
            "public_ipv4_per_hour": PUBLIC_IPV4_USD_PER_HOUR,
        },
        "limits": [
            "an unattached disk is reported once seen unattached in runs 2 IST days apart",
            "snapshot cost uses volume size, an upper bound",
            "prices are constants, not read from the pricing API",
        ],
    })
}

// ---------------------------------------------------------------------------
// The run, generic over the AWS surface (tests inject a fake)
// ---------------------------------------------------------------------------

/// Every AWS call the check makes.
#[allow(async_fn_in_trait)] // APPROVED: the run is generic over the trait (no dyn); futures awaited inline.
pub trait WasteAws {
    async fn volumes(&self) -> Result<Vec<VolumeFact>, String>;
    async fn snapshots(&self) -> Result<Vec<SnapshotFact>, String>;
    async fn addresses(&self) -> Result<Vec<AddressFact>, String>;
    async fn service_costs(
        &self,
        start: NaiveDate,
        end_exclusive: NaiveDate,
    ) -> Result<ServiceCostRows, String>;
    /// `Ok(None)` when the state object does not exist.
    async fn read_state(&self, bucket: &str, key: &str) -> Result<Option<String>, String>;
    async fn write_state(&self, bucket: &str, key: &str, value: &str) -> Result<(), String>;
    /// Create `key` only if it does not exist. `Ok(true)`: this call created
    /// it; `Ok(false)`: it already existed (or another create is in flight).
    async fn claim(&self, bucket: &str, key: &str) -> Result<bool, String>;
    /// One attempt only; the error says whether SNS certainly refused it.
    async fn publish(
        &self,
        topic_arn: &str,
        subject: &str,
        message: &str,
    ) -> Result<(), PublishError>;
    async fn put_audit(&self, bucket: &str, key: &str, body: String) -> Result<(), String>;
}

/// The two settings the run needs.
#[derive(Debug, Clone, PartialEq)]
pub struct WasteEnv {
    pub alerts_topic_arn: String,
    /// The cold bucket: the audit files, the state and the day claims.
    pub audit_bucket: String,
}

impl WasteEnv {
    /// Read the settings through `get`; an empty or missing one is a coded
    /// `LAMBDA-CONFIG-01` error.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let need = |key: &str| -> Result<String, String> {
            match get(key).map(|v| v.trim().to_string()) {
                Some(v) if !v.is_empty() => Ok(v),
                _ => {
                    error!(
                        code = "LAMBDA-CONFIG-01",
                        var = key,
                        "waste check: required setting is missing"
                    );
                    Err(format!("{key} env var is missing or empty"))
                }
            }
        };
        Ok(Self {
            alerts_topic_arn: need("ALERTS_TOPIC_ARN")?,
            audit_bucket: need("AUDIT_BUCKET")?,
        })
    }
}

fn read_failed(what: &str, e: String) -> String {
    error!(code = "LAMBDA-AWS-01", call = what, error = %e, "waste check: a read failed; nothing written");
    format!("{what} failed: {e}")
}

/// The day's one Cost Explorer query, claimed first so no second one runs
/// the same IST day. Never fails the run: the top-5 line is context only.
async fn month_to_date_costs<A: WasteAws>(
    aws: &A,
    bucket: &str,
    today_ist: &str,
    today_utc: NaiveDate,
) -> (ServiceCostRows, CostOutcome) {
    match aws
        .claim(bucket, &claim_key(today_ist, ClaimKind::CostQuery))
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            info!("waste check: today's Cost Explorer query already ran; no second query");
            return (ServiceCostRows::default(), CostOutcome::AlreadyQueriedToday);
        }
        Err(e) => {
            warn!(code = "LAMBDA-AWS-02", error = %e, "waste check: could not claim today's Cost Explorer query; skipping it");
            return (
                ServiceCostRows::default(),
                CostOutcome::Unavailable(format!("claim failed: {e}")),
            );
        }
    }
    let (start, end) = cost_period(today_utc);
    match aws.service_costs(start, end).await {
        Ok(rows) => {
            let partial = rows.more_pages;
            (rows, CostOutcome::Read { partial })
        }
        Err(e) => {
            warn!(code = "LAMBDA-AWS-01", call = "GetCostAndUsage", error = %e, "waste check: the cost query failed; carrying on without the top-5 line");
            (ServiceCostRows::default(), CostOutcome::Unavailable(e))
        }
    }
}

/// One full run. Returns the summary JSON, or an error after logging it.
pub async fn run_waste_check<A: WasteAws>(
    aws: &A,
    env: &WasteEnv,
    now_utc: DateTime<Utc>,
) -> Result<Value, String> {
    let now_unix = now_utc.timestamp();
    let today_utc = now_utc.date_naive();
    let today_ist = crate::time::today_ist_string(now_utc);
    let today_ist_date = now_utc.with_timezone(&ist()).date_naive();
    let bucket = env.audit_bucket.as_str();

    // 1. The EC2 and state reads. Any failure ends the run before a write.
    let vols = aws
        .volumes()
        .await
        .map_err(|e| read_failed("DescribeVolumes", e))?;
    let snaps = aws
        .snapshots()
        .await
        .map_err(|e| read_failed("DescribeSnapshots", e))?;
    let addrs = aws
        .addresses()
        .await
        .map_err(|e| read_failed("DescribeAddresses", e))?;
    let raw_state = aws
        .read_state(bucket, STATE_KEY)
        .await
        .map_err(|e| read_failed("ReadState", e))?;

    // 2. The day's one cost query (context; its failure never stops the run).
    let (costs, cost) = month_to_date_costs(aws, bucket, &today_ist, today_utc).await;

    // 3. Pure decision.
    let previous = parse_state(raw_state.as_deref());
    if let PreviousState::Corrupt(why) = &previous {
        warn!(reason = %why, "waste check: previous state unreadable; every finding reads as new once");
    }
    let since = unattached_since_map(&vols, &previous.unattached_since(), &today_ist);
    let findings = collect_findings(&vols, &snaps, &addrs, now_unix, &since, today_ist_date);
    let current: BTreeSet<String> = findings.iter().map(|f| f.id.clone()).collect();
    let decision = decide(&previous, &current, &since, &today_ist);
    let (top, skipped) = top_services(&costs.rows);

    // 4. The one message: claim the day, record it as sent, then send.
    let mut failures: Vec<String> = Vec::new();
    let mut message_sent = false;
    let mut send_error: Option<String> = None;
    let mut state_written = false;
    if decision.send {
        match aws
            .claim(bucket, &claim_key(&today_ist, ClaimKind::Message))
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                info!(
                    "waste check: another run already claimed today's message; this run writes nothing"
                );
                return Ok(json!({
                    "ok": true,
                    "findings": findings.len(),
                    "new": decision.new_ids.len(),
                    "decision": "already_claimed_today",
                    "message_sent": false,
                }));
            }
            Err(e) => {
                error!(code = "LAMBDA-AWS-02", error = %e, "waste check: could not claim today's message; nothing sent, nothing written");
                return Err(format!("message claim failed: {e}"));
            }
        }
        let sent_state = serde_json::to_string(&decision.next_state)
            .map_err(|e| format!("state encode failed: {e}"))?;
        if let Err(e) = aws.write_state(bucket, STATE_KEY, &sent_state).await {
            error!(code = "LAMBDA-AWS-02", error = %e, "waste check: could not record the message as sent; nothing sent, tomorrow retries");
            return Err(format!("state write before the message failed: {e}"));
        }
        state_written = true;
        let new_set: BTreeSet<&str> = decision.new_ids.iter().map(String::as_str).collect();
        let new: Vec<&Finding> = findings
            .iter()
            .filter(|f| new_set.contains(f.id.as_str()))
            .collect();
        let message = render_message(&new, &findings, &top, cost.partial(), now_utc);
        match aws
            .publish(&env.alerts_topic_arn, WASTE_SUBJECT, &message)
            .await
        {
            Ok(()) => message_sent = true,
            Err(PublishError::Unknown(e)) => {
                // SNS may have delivered it: keep the "sent" state, so the
                // worst case is a missed message, never a repeat tomorrow.
                send_error = Some(format!("sns:Publish outcome unknown: {e}"));
                error!(code = "LAMBDA-AWS-02", error = %e, maybe_unsent = ?decision.new_ids, "waste check: sns:Publish outcome unknown (it may have been delivered); the sent state is kept, so these items are not sent again (they are in today's audit file)");
                failures.push(format!("sns:Publish outcome unknown: {e}; sent state kept"));
            }
            Err(PublishError::Refused(e)) => {
                send_error = Some(format!("sns:Publish failed: {e}"));
                let reverted = serde_json::to_string(&decision.revert_state)
                    .map_err(|e| format!("state encode failed: {e}"));
                let put_back = match reverted {
                    Ok(body) => aws.write_state(bucket, STATE_KEY, &body).await,
                    Err(e) => Err(e),
                };
                match put_back {
                    Ok(()) => {
                        error!(code = "LAMBDA-AWS-02", error = %e, "waste check: sns:Publish failed; state put back, tomorrow retries");
                        failures.push(format!("sns:Publish failed: {e}; state put back"));
                    }
                    Err(e2) => {
                        error!(code = "LAMBDA-AWS-02", error = %e, put_back_error = %e2, unsent = ?decision.new_ids, "waste check: sns:Publish failed and the state could not be put back; these items will not be sent again (they are in today's audit file)");
                        failures.push(format!(
                            "sns:Publish failed: {e}; state put back failed: {e2}"
                        ));
                    }
                }
            }
        }
    }

    // 5. The state (when not already written), then the audit file; both
    //    attempted, either failure errors.
    if !state_written {
        match serde_json::to_string(&decision.next_state) {
            Ok(body) => {
                if let Err(e) = aws.write_state(bucket, STATE_KEY, &body).await {
                    error!(code = "LAMBDA-AWS-02", error = %e, key = STATE_KEY, "waste check: the state write failed");
                    failures.push(format!("state write failed: {e}"));
                }
            }
            Err(e) => failures.push(format!("state encode failed: {e}")),
        }
    }
    let audit = audit_document(&AuditInput {
        now_utc,
        date_ist: &today_ist,
        findings: &findings,
        previous: &previous,
        decision: &decision,
        message_sent,
        send_error: send_error.as_deref(),
        top: &top,
        cost: &cost,
        cost_rows_skipped: skipped,
    });
    let key = audit_key(&today_ist);
    if let Err(e) = aws.put_audit(bucket, &key, audit.to_string()).await {
        error!(code = "LAMBDA-AWS-02", error = %e, key = %key, "waste check: s3:PutObject for the audit file failed");
        failures.push(format!("s3:PutObject failed: {e}"));
    }
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }

    info!(
        findings = findings.len(),
        new = decision.new_ids.len(),
        reason = decision.reason,
        message_sent,
        cost_query = cost.label(),
        "waste check finished"
    );
    Ok(json!({
        "ok": true,
        "findings": findings.len(),
        "new": decision.new_ids.len(),
        "decision": decision.reason,
        "message_sent": message_sent,
        "cost_query": cost.label(),
        "audit_key": key,
    }))
}

// ---------------------------------------------------------------------------
// The real AWS surface (exercised only in a live invoke)
// ---------------------------------------------------------------------------

fn name_tag(tags: &[aws_sdk_ec2::types::Tag]) -> Option<String> {
    tags.iter()
        .find(|t| t.key() == Some("Name"))
        .and_then(|t| t.value())
        .map(str::to_string)
}

/// Map one SDK volume to its facts.
pub fn volume_fact(v: &aws_sdk_ec2::types::Volume) -> VolumeFact {
    VolumeFact {
        id: v.volume_id().unwrap_or_default().to_string(),
        state: v
            .state()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default(),
        size_gib: i64::from(v.size().unwrap_or(0)),
        volume_type: v
            .volume_type()
            .map(|t| t.as_str().to_string())
            .unwrap_or_default(),
        created_unix: v.create_time().map(|t| t.secs()),
        name: name_tag(v.tags()),
    }
}

/// Map one SDK snapshot to its facts.
pub fn snapshot_fact(s: &aws_sdk_ec2::types::Snapshot) -> SnapshotFact {
    SnapshotFact {
        id: s.snapshot_id().unwrap_or_default().to_string(),
        size_gib: i64::from(s.volume_size().unwrap_or(0)),
        started_unix: s.start_time().map(|t| t.secs()),
        archived: s.storage_tier() == Some(&aws_sdk_ec2::types::StorageTier::Archive),
        name: name_tag(s.tags()),
    }
}

/// Map one SDK address to its facts.
pub fn address_fact(a: &aws_sdk_ec2::types::Address) -> AddressFact {
    AddressFact {
        allocation_id: a.allocation_id().map(str::to_string),
        public_ip: a.public_ip().map(str::to_string),
        association_id: a.association_id().map(str::to_string),
        instance_id: a.instance_id().map(str::to_string),
        network_interface_id: a.network_interface_id().map(str::to_string),
    }
}

/// Map the Cost Explorer reply to `(service, amount)` rows.
pub fn cost_rows(
    out: &aws_sdk_costexplorer::operation::get_cost_and_usage::GetCostAndUsageOutput,
) -> ServiceCostRows {
    let mut rows = Vec::new();
    for period in out.results_by_time() {
        for group in period.groups() {
            let service = group.keys().first().cloned().unwrap_or_default();
            let amount = group
                .metrics()
                .and_then(|m| m.get("UnblendedCost"))
                .and_then(|m| m.amount())
                .unwrap_or("")
                .to_string();
            rows.push((service, amount));
        }
    }
    ServiceCostRows {
        rows,
        more_pages: out.next_page_token().is_some_and(|t| !t.is_empty()),
    }
}
/// The live AWS clients.
pub struct SdkWasteAws {
    ec2: aws_sdk_ec2::Client,
    ce: aws_sdk_costexplorer::Client,
    /// One attempt (`clients::sns_single_attempt`): a publish is never sent
    /// twice by an SDK retry.
    sns: aws_sdk_sns::Client,
    /// State reads and writes and the audit file (retries are harmless:
    /// each is a whole-object read or overwrite).
    s3: aws_sdk_s3::Client,
    /// The create-only day claims, one attempt (`clients::s3_single_attempt`),
    /// so a lost reply is a failure, never a retry that reads its own claim
    /// back as "already taken".
    s3_claim: aws_sdk_s3::Client,
}

fn sdk_err<E: std::error::Error>(e: E) -> String {
    aws_sdk_ec2::error::DisplayErrorContext(e).to_string()
}

impl SdkWasteAws {
    /// Build the live clients from one SDK config. The SNS client and the
    /// claim client make ONE attempt each whatever the config's retry mode;
    /// `ce` comes from `clients::cost_explorer_us_east_1` (retries off too).
    pub fn new(config: &aws_config::SdkConfig, ce: aws_sdk_costexplorer::Client) -> Self {
        Self {
            ec2: crate::clients::ec2(config),
            ce,
            sns: crate::clients::sns_single_attempt(config),
            s3: crate::clients::s3(config),
            s3_claim: crate::clients::s3_single_attempt(config),
        }
    }

    async fn put_json(&self, bucket: &str, key: &str, body: Vec<u8>) -> Result<(), String> {
        self.s3
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("application/json")
            .body(aws_sdk_s3::primitives::ByteStream::from(body))
            .send()
            .await
            .map(|_| ())
            .map_err(sdk_err)
    }
}

impl WasteAws for SdkWasteAws {
    async fn volumes(&self) -> Result<Vec<VolumeFact>, String> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_EC2_PAGES {
            let resp = self
                .ec2
                .describe_volumes()
                .filters(
                    aws_sdk_ec2::types::Filter::builder()
                        .name("status")
                        .values("available")
                        .build(),
                )
                .set_next_token(token.take())
                .send()
                .await
                .map_err(sdk_err)?;
            out.extend(resp.volumes().iter().map(volume_fact));
            match resp.next_token() {
                Some(t) if !t.is_empty() => token = Some(t.to_string()),
                _ => return Ok(out),
            }
        }
        Err(format!("more than {MAX_EC2_PAGES} pages of volumes"))
    }

    async fn snapshots(&self) -> Result<Vec<SnapshotFact>, String> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_EC2_PAGES {
            let resp = self
                .ec2
                .describe_snapshots()
                .owner_ids("self")
                .max_results(1000)
                .set_next_token(token.take())
                .send()
                .await
                .map_err(sdk_err)?;
            out.extend(resp.snapshots().iter().map(snapshot_fact));
            match resp.next_token() {
                Some(t) if !t.is_empty() => token = Some(t.to_string()),
                _ => return Ok(out),
            }
        }
        Err(format!("more than {MAX_EC2_PAGES} pages of snapshots"))
    }

    async fn addresses(&self) -> Result<Vec<AddressFact>, String> {
        let resp = self
            .ec2
            .describe_addresses()
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(resp.addresses().iter().map(address_fact).collect())
    }

    async fn service_costs(
        &self,
        start: NaiveDate,
        end_exclusive: NaiveDate,
    ) -> Result<ServiceCostRows, String> {
        use aws_sdk_costexplorer::types::{
            DateInterval, Granularity, GroupDefinition, GroupDefinitionType,
        };
        let period = DateInterval::builder()
            .start(start.format("%Y-%m-%d").to_string())
            .end(end_exclusive.format("%Y-%m-%d").to_string())
            .build()
            .map_err(|e| e.to_string())?;
        // The client is built by `clients::cost_explorer_from` with SDK
        // retries off: this is exactly one billed request.
        let out = self
            .ce
            .get_cost_and_usage()
            .time_period(period)
            .granularity(Granularity::Monthly)
            .metrics("UnblendedCost")
            .group_by(
                GroupDefinition::builder()
                    .r#type(GroupDefinitionType::Dimension)
                    .key("SERVICE")
                    .build(),
            )
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(cost_rows(&out))
    }

    async fn read_state(&self, bucket: &str, key: &str) -> Result<Option<String>, String> {
        // List first: without a bucket-wide list grant, a GetObject of a
        // missing key answers 403, which cannot be told from a real denial.
        // The role may list only this one key (an `s3:prefix` condition).
        let listed = self
            .s3
            .list_objects_v2()
            .bucket(bucket)
            .prefix(key)
            .max_keys(2)
            .send()
            .await
            .map_err(sdk_err)?;
        if !listed.contents().iter().any(|o| o.key() == Some(key)) {
            return Ok(None);
        }
        let out = self
            .s3
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(sdk_err)?;
        if out
            .content_length()
            .is_some_and(|n| n < 0 || n as u64 > STATE_MAX_BYTES as u64)
        {
            return Err(format!("state object larger than {STATE_MAX_BYTES} bytes"));
        }
        let bytes = out
            .body
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .into_bytes();
        if bytes.len() > STATE_MAX_BYTES {
            return Err(format!("state object larger than {STATE_MAX_BYTES} bytes"));
        }
        String::from_utf8(bytes.to_vec())
            .map(Some)
            .map_err(|e| format!("state object is not UTF-8: {e}"))
    }

    async fn write_state(&self, bucket: &str, key: &str, value: &str) -> Result<(), String> {
        self.put_json(bucket, key, value.as_bytes().to_vec()).await
    }

    async fn claim(&self, bucket: &str, key: &str) -> Result<bool, String> {
        let body = json!({ "claimed_at_utc": Utc::now().to_rfc3339() }).to_string();
        match self
            .s3_claim
            .put_object()
            .bucket(bucket)
            .key(key)
            .if_none_match("*")
            .content_type("application/json")
            .body(aws_sdk_s3::primitives::ByteStream::from(body.into_bytes()))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                let status = e.raw_response().map(|r| r.status().as_u16());
                if claim_already_taken(status) {
                    Ok(false)
                } else {
                    Err(sdk_err(e))
                }
            }
        }
    }

    async fn publish(
        &self,
        topic_arn: &str,
        subject: &str,
        message: &str,
    ) -> Result<(), PublishError> {
        match self
            .sns
            .publish()
            .topic_arn(topic_arn)
            .subject(subject)
            .message(message)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let never_sent = matches!(e, aws_sdk_sns::error::SdkError::ConstructionFailure(_));
                let status = e.raw_response().map(|r| r.status().as_u16());
                let text = sdk_err(e);
                if publish_refused(never_sent, status) {
                    Err(PublishError::Refused(text))
                } else {
                    Err(PublishError::Unknown(text))
                }
            }
        }
    }

    async fn put_audit(&self, bucket: &str, key: &str, body: String) -> Result<(), String> {
        self.put_json(bucket, key, body.into_bytes()).await
    }
}

/// Entry point for the thin bin. UNPROVEN until deploy: the AWS legs run only
/// in a real Lambda invoke; [`run_waste_check`] is what the tests cover.
// TEST-EXEMPT: thin live-AWS shell; every decision it feeds is covered by the fake-driven run_waste_check tests.
pub async fn handle(_event: Value) -> Result<Value, Error> {
    let env = WasteEnv::from_lookup(|k| std::env::var(k).ok()).map_err(Error::from)?;
    let config = crate::clients::sdk_config().await;
    let ce = crate::clients::cost_explorer_us_east_1().await;
    let aws = SdkWasteAws::new(&config, ce);
    run_waste_check(&aws, &env, Utc::now())
        .await
        .map_err(Error::from)
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::cell::{Cell, RefCell};

    /// 2026-11-11 12:37:00 UTC = 18:07 IST, the scheduled minute.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, 37, 0).unwrap()
    }

    const DAY: i64 = 86_400;
    const TODAY: &str = "2026-11-11";

    fn today_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 11).unwrap()
    }

    fn vol(id: &str, state: &str, gib: i64, ty: &str, age_secs: Option<i64>) -> VolumeFact {
        VolumeFact {
            id: id.to_string(),
            state: state.to_string(),
            size_gib: gib,
            volume_type: ty.to_string(),
            created_unix: age_secs.map(|a| now().timestamp() - a),
            name: None,
        }
    }

    fn snap(id: &str, gib: i64, age_secs: Option<i64>, archived: bool) -> SnapshotFact {
        SnapshotFact {
            id: id.to_string(),
            size_gib: gib,
            started_unix: age_secs.map(|a| now().timestamp() - a),
            archived,
            name: None,
        }
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn since(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn no_since() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn ids_state(ids: &[&str], last: Option<&str>) -> PreviousState {
        PreviousState::Ids {
            ids: set(ids),
            unattached_since: no_since(),
            last_message_ist: last.map(str::to_string),
        }
    }

    // ---------------- prices + findings ----------------

    #[test]
    fn test_volume_price_per_gb_by_type() {
        assert_eq!(volume_price_per_gb("gp3"), EBS_GP3_USD_PER_GB_MONTH);
        assert_eq!(volume_price_per_gb("gp2"), EBS_GP2_USD_PER_GB_MONTH);
        assert_eq!(volume_price_per_gb("io1"), EBS_IO_USD_PER_GB_MONTH);
        assert_eq!(volume_price_per_gb("io2"), EBS_IO_USD_PER_GB_MONTH);
        assert_eq!(volume_price_per_gb("st1"), EBS_OTHER_USD_PER_GB_MONTH);
        assert_eq!(volume_price_per_gb(""), EBS_OTHER_USD_PER_GB_MONTH);
    }

    #[test]
    fn test_idle_address_monthly_usd_is_hourly_rate_times_730() {
        assert!((idle_address_monthly_usd() - 3.65).abs() < 1e-9);
    }

    #[test]
    fn test_claim_key_names_the_day_and_the_kind() {
        assert_eq!(
            claim_key(TODAY, ClaimKind::CostQuery),
            "waste-check/claims/2026-11-11-cost-query"
        );
        assert_eq!(
            claim_key(TODAY, ClaimKind::Message),
            "waste-check/claims/2026-11-11-message"
        );
        assert!(claim_key(TODAY, ClaimKind::Message).starts_with(CLAIM_PREFIX));
    }

    #[test]
    fn test_claim_already_taken_only_on_412_and_409() {
        assert!(claim_already_taken(Some(412)));
        assert!(claim_already_taken(Some(409)));
        assert!(!claim_already_taken(Some(403)));
        assert!(!claim_already_taken(Some(500)));
        assert!(!claim_already_taken(Some(200)));
        assert!(!claim_already_taken(None), "a network error is a failure");
    }

    #[test]
    fn test_cost_outcome_label_and_partial() {
        assert_eq!(CostOutcome::Read { partial: false }.label(), "read");
        assert_eq!(CostOutcome::Read { partial: true }.label(), "read_partial");
        assert_eq!(
            CostOutcome::AlreadyQueriedToday.label(),
            "already_queried_today"
        );
        assert_eq!(CostOutcome::Unavailable("x".into()).label(), "unavailable");
        assert!(CostOutcome::Read { partial: true }.partial());
        assert!(!CostOutcome::Read { partial: false }.partial());
        assert!(!CostOutcome::AlreadyQueriedToday.partial());
        assert!(!CostOutcome::Unavailable("x".into()).partial());
    }

    #[test]
    fn test_unattached_since_map_carries_dates_and_restarts_reattached_volumes() {
        let vols = vec![
            vol("vol-old", "available", 1, "gp3", Some(90 * DAY)),
            vol("vol-new", "available", 1, "gp3", Some(90 * DAY)),
            vol("vol-future", "available", 1, "gp3", None),
            vol("vol-inuse", "in-use", 1, "gp3", Some(90 * DAY)),
            vol("", "available", 1, "gp3", None),
            vol("bad id", "available", 1, "gp3", None),
        ];
        let prev = since(&[
            ("vol-old", "2026-11-01"),
            ("vol-future", "2026-12-25"),
            ("vol-inuse", "2026-11-01"),
            ("vol-gone", "2026-11-01"),
        ]);
        let m = unattached_since_map(&vols, &prev, TODAY);
        assert_eq!(
            m,
            since(&[
                ("vol-future", TODAY),
                ("vol-new", TODAY),
                ("vol-old", "2026-11-01"),
            ]),
            "carried, first seen today, a future date clamped; reattached and gone dropped"
        );
    }

    #[test]
    fn test_unattached_volume_findings_the_november_incident() {
        // The two volumes that billed about $21 from 1 to 11 Nov 2026, seen
        // unattached by an earlier run.
        let vols = vec![
            vol(
                "vol-0c6ab6e593e39d8c8",
                "available",
                600,
                "gp3",
                Some(14 * DAY),
            ),
            vol(
                "vol-073ccaa417a0f344b",
                "available",
                30,
                "gp3",
                Some(180 * DAY),
            ),
            vol("vol-inuse", "in-use", 150, "gp3", Some(30 * DAY)),
        ];
        let first_seen = since(&[
            ("vol-0c6ab6e593e39d8c8", "2026-11-09"),
            ("vol-073ccaa417a0f344b", "2026-11-01"),
            ("vol-inuse", "2026-11-01"),
        ]);
        let f = unattached_volume_findings(&vols, now().timestamp(), &first_seen, today_date());
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].id, "vol-0c6ab6e593e39d8c8");
        assert_eq!(f[0].kind, FindingKind::UnattachedDisk);
        assert_eq!(f[0].size_gib, Some(600));
        assert_eq!(f[0].age_days, Some(14));
        assert_eq!(f[0].unattached_since.as_deref(), Some("2026-11-09"));
        assert!((f[0].est_monthly_usd - 54.72).abs() < 1e-9);
        assert!((f[1].est_monthly_usd - 2.736).abs() < 1e-9);
    }

    #[test]
    fn test_regression_a_volume_detached_today_is_not_reported_until_two_days_later() {
        // Finding: CreateTime was used as the age, so an old volume detached
        // minutes before the run was paged the same evening. The 48 h grace
        // now counts from the first run that saw it unattached.
        let old_root = vec![vol(
            "vol-rollback",
            "available",
            600,
            "gp3",
            Some(120 * DAY),
        )];
        let t = now().timestamp();
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        let first = unattached_since_map(&old_root, &no_since(), "2026-11-11");
        assert!(unattached_volume_findings(&old_root, t, &first, day(11)).is_empty());
        let second = unattached_since_map(&old_root, &first, "2026-11-12");
        assert!(unattached_volume_findings(&old_root, t, &second, day(12)).is_empty());
        let third = unattached_since_map(&old_root, &second, "2026-11-13");
        let f = unattached_volume_findings(&old_root, t, &third, day(13));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].unattached_since.as_deref(), Some("2026-11-11"));
    }

    #[test]
    fn test_unattached_volume_findings_boundaries() {
        let t = now().timestamp();
        let vols = vec![
            vol("vol-1day", "available", 10, "gp3", Some(90 * DAY)),
            vol("vol-2days", "available", 10, "gp3", Some(90 * DAY)),
            vol("vol-unknown-age", "available", 10, "gp3", None),
            vol("vol-unseen", "available", 10, "gp3", Some(90 * DAY)),
            vol("vol-bad-date", "available", 10, "gp3", Some(90 * DAY)),
            vol("vol-neg", "available", -5, "gp3", Some(10 * DAY)),
            vol("", "available", 10, "gp3", Some(10 * DAY)),
        ];
        let first_seen = since(&[
            ("vol-1day", "2026-11-10"),
            ("vol-2days", "2026-11-09"),
            ("vol-unknown-age", "2026-11-01"),
            ("vol-bad-date", "yesterday"),
            ("vol-neg", "2026-11-01"),
            ("", "2026-11-01"),
        ]);
        let f = unattached_volume_findings(&vols, t, &first_seen, today_date());
        let ids: Vec<&str> = f.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["vol-2days", "vol-unknown-age", "vol-neg"]);
        assert_eq!(f[1].age_days, None, "no CreateTime reads as unknown age");
        assert_eq!(f[2].size_gib, Some(0));
        assert_eq!(f[2].est_monthly_usd, 0.0);
    }

    #[test]
    fn test_old_snapshot_findings_boundaries_and_tiers() {
        let t = now().timestamp();
        let snaps = vec![
            snap("snap-30d", 30, Some(30 * DAY), false),
            snap("snap-31d", 30, Some(31 * DAY), false),
            snap("snap-archived", 100, Some(90 * DAY), true),
            snap("snap-unknown", 8, None, false),
            snap("", 8, Some(90 * DAY), false),
        ];
        let f = old_snapshot_findings(&snaps, t);
        let ids: Vec<&str> = f.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["snap-31d", "snap-archived", "snap-unknown"]);
        assert!((f[0].est_monthly_usd - 1.5).abs() < 1e-9);
        assert!((f[1].est_monthly_usd - 1.25).abs() < 1e-9);
        assert_eq!(f[0].age_days, Some(31));
        assert_eq!(f[2].age_days, None);
        assert!(f.iter().all(|x| x.kind == FindingKind::OldSnapshot));
    }

    #[test]
    fn test_idle_address_findings_every_association_shape() {
        let free = AddressFact {
            allocation_id: Some("eipalloc-free".into()),
            public_ip: Some("13.0.0.1".into()),
            ..Default::default()
        };
        let associated = AddressFact {
            allocation_id: Some("eipalloc-a".into()),
            association_id: Some("eipassoc-1".into()),
            ..Default::default()
        };
        let on_instance = AddressFact {
            allocation_id: Some("eipalloc-b".into()),
            instance_id: Some("i-1".into()),
            ..Default::default()
        };
        let on_eni = AddressFact {
            allocation_id: Some("eipalloc-c".into()),
            network_interface_id: Some("eni-1".into()),
            ..Default::default()
        };
        let ip_only = AddressFact {
            public_ip: Some("13.0.0.2".into()),
            ..Default::default()
        };
        let nameless = AddressFact::default();
        let f = idle_address_findings(&[free, associated, on_instance, on_eni, ip_only, nameless]);
        let ids: Vec<&str> = f.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["eipalloc-free", "13.0.0.2"]);
        assert_eq!(f[0].label.as_deref(), Some("13.0.0.1"));
        assert!((f[0].est_monthly_usd - 3.65).abs() < 1e-9);
    }

    #[test]
    fn test_collect_findings_sorts_dedups_and_skips_unstorable_ids() {
        let vols = vec![
            vol("vol-b", "available", 1, "gp3", Some(10 * DAY)),
            vol("vol-b", "available", 1, "gp3", Some(10 * DAY)),
        ];
        let snaps = vec![
            snap("snap-a", 1, Some(40 * DAY), false),
            snap("snap with space", 1, Some(40 * DAY), false),
        ];
        let addrs = vec![AddressFact {
            allocation_id: Some("eipalloc-z".into()),
            ..Default::default()
        }];
        let first_seen = since(&[("vol-b", "2026-11-01")]);
        let f = collect_findings(
            &vols,
            &snaps,
            &addrs,
            now().timestamp(),
            &first_seen,
            today_date(),
        );
        let ids: Vec<&str> = f.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["eipalloc-z", "snap-a", "vol-b"]);
        assert!(
            collect_findings(&[], &[], &[], now().timestamp(), &no_since(), today_date())
                .is_empty()
        );
    }

    #[test]
    fn test_top_services_sorts_sums_skips_and_truncates() {
        let rows: Vec<(String, String)> = [
            ("EC2 - Other", "21.5"),
            ("Amazon Elastic Compute Cloud - Compute", "40.10"),
            ("Amazon Simple Storage Service", "3"),
            ("AWS Lambda", "0"),
            ("Tax", "-1"),
            ("Bad", "abc"),
            ("Nan", "NaN"),
            ("Inf", "inf"),
            ("Amazon CloudWatch", "2.5"),
            ("Amazon CloudWatch", "2.5"),
            ("AWS Key Management Service", "1"),
            ("Amazon Simple Notification Service", "1"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let (top, skipped) = top_services(&rows);
        assert_eq!(skipped, 3, "abc, NaN and inf are skipped");
        let names: Vec<&str> = top.iter().map(|s| s.service.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Amazon Elastic Compute Cloud - Compute",
                "EC2 - Other",
                "Amazon CloudWatch",
                "Amazon Simple Storage Service",
                // 1.0 tie: name order breaks it.
                "AWS Key Management Service",
            ]
        );
        assert!((top[2].usd - 5.0).abs() < 1e-9, "duplicates are summed");
        assert_eq!(top_services(&[]).0, Vec::new());
    }

    #[test]
    fn test_cost_period_month_to_date_with_exclusive_end() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(
            cost_period(d(2026, 11, 11)),
            (d(2026, 11, 1), d(2026, 11, 12))
        );
        assert_eq!(
            cost_period(d(2026, 11, 1)),
            (d(2026, 11, 1), d(2026, 11, 2))
        );
        assert_eq!(
            cost_period(d(2026, 11, 30)),
            (d(2026, 11, 1), d(2026, 12, 1))
        );
        assert_eq!(
            cost_period(d(2026, 12, 31)),
            (d(2026, 12, 1), d(2027, 1, 1))
        );
    }

    // ---------------- state ----------------

    #[test]
    fn test_parse_state_every_shape() {
        assert_eq!(parse_state(None), PreviousState::FirstRun);
        assert!(matches!(parse_state(Some("")), PreviousState::Corrupt(_)));
        assert!(matches!(
            parse_state(Some("not json")),
            PreviousState::Corrupt(_)
        ));
        assert!(matches!(
            parse_state(Some(r#"{"v":2,"ids":[]}"#)),
            PreviousState::Corrupt(_)
        ));
        assert!(matches!(
            parse_state(Some(r#"{"v":1,"ids":["has space"]}"#)),
            PreviousState::Corrupt(_)
        ));
        assert!(matches!(
            parse_state(Some(r#"{"v":1,"ids":[""]}"#)),
            PreviousState::Corrupt(_)
        ));
        assert!(matches!(
            parse_state(Some(
                r#"{"v":1,"ids":[],"unattached_since":{"vol-a":"last week"}}"#
            )),
            PreviousState::Corrupt(_)
        ));
        assert!(matches!(
            parse_state(Some(
                r#"{"v":1,"ids":[],"unattached_since":{"bad id":"2026-11-01"}}"#
            )),
            PreviousState::Corrupt(_)
        ));
        assert_eq!(
            parse_state(Some(
                r#"{"v":1,"ids":["vol-b","vol-a"],"unattached_since":{"vol-a":"2026-11-02"},"last_message_ist":"2026-11-10"}"#
            )),
            PreviousState::Ids {
                ids: set(&["vol-a", "vol-b"]),
                unattached_since: since(&[("vol-a", "2026-11-02")]),
                last_message_ist: Some("2026-11-10".into()),
            }
        );
        // A malformed date is dropped, never trusted as "sent today".
        assert_eq!(
            parse_state(Some(r#"{"v":1,"ids":[],"last_message_ist":"yesterday"}"#)),
            ids_state(&[], None)
        );
    }

    #[test]
    fn test_label_ids_and_unattached_since_of_each_previous_state() {
        assert_eq!(PreviousState::FirstRun.label(), "first_run");
        assert_eq!(PreviousState::Corrupt("x".into()).label(), "unreadable");
        assert_eq!(ids_state(&[], None).label(), "ids");
        assert!(PreviousState::FirstRun.ids().is_empty());
        assert!(
            PreviousState::Corrupt("x".into())
                .unattached_since()
                .is_empty()
        );
        let p = PreviousState::Ids {
            ids: set(&["vol-a"]),
            unattached_since: since(&[("vol-a", "2026-11-01")]),
            last_message_ist: None,
        };
        assert_eq!(p.ids(), set(&["vol-a"]));
        assert_eq!(p.unattached_since(), since(&[("vol-a", "2026-11-01")]));
    }

    #[test]
    fn test_state_from_round_trips_any_size_without_a_digest() {
        // Finding: past ~150 ids the old SSM state kept only a digest. The
        // state is now an S3 object and keeps every id, however many.
        let many: BTreeSet<String> = (0..5_000).map(|i| format!("snap-{i:017}")).collect();
        let first_seen = since(&[("vol-a", "2026-11-01")]);
        let s = state_from(&many, &first_seen, Some(TODAY.into()));
        let encoded = serde_json::to_string(&s).unwrap();
        assert!(encoded.len() > 4096, "bigger than the old SSM cap");
        assert!(!encoded.contains("digest"));
        assert_eq!(
            parse_state(Some(&encoded)),
            PreviousState::Ids {
                ids: many,
                unattached_since: first_seen,
                last_message_ist: Some(TODAY.into()),
            }
        );
    }

    // ---------------- the edge trigger, every permutation ----------------

    #[test]
    fn test_decide_none() {
        let d = decide(
            &ids_state(&["vol-a"], Some("2026-11-01")),
            &set(&[]),
            &no_since(),
            TODAY,
        );
        assert!(!d.send);
        assert_eq!(d.reason, "no_findings");
        assert!(d.next_state.ids.is_empty());
        assert_eq!(d.next_state.last_message_ist.as_deref(), Some("2026-11-01"));
        let d = decide(&PreviousState::FirstRun, &set(&[]), &no_since(), TODAY);
        assert!(!d.send);
    }

    #[test]
    fn test_decide_first_run_sends_everything_once() {
        let cur = set(&["vol-a", "vol-b"]);
        let d = decide(&PreviousState::FirstRun, &cur, &no_since(), TODAY);
        assert!(d.send);
        assert_eq!(d.reason, "first_run");
        assert_eq!(d.new_ids, vec!["vol-a", "vol-b"]);
        assert_eq!(d.next_state.last_message_ist.as_deref(), Some(TODAY));
        assert!(d.revert_state.ids.is_empty());
        assert_eq!(d.revert_state.last_message_ist, None);
        // The next day with the same set: silence.
        let next = parse_state(Some(&serde_json::to_string(&d.next_state).unwrap()));
        let d2 = decide(&next, &cur, &no_since(), "2026-11-12");
        assert!(!d2.send);
        assert_eq!(d2.reason, "no_new_findings");
    }

    #[test]
    fn test_decide_new_item_sends_only_the_new_one() {
        let first_seen = since(&[("vol-a", "2026-11-01")]);
        let d = decide(
            &ids_state(&["vol-a"], Some("2026-11-10")),
            &set(&["vol-a", "vol-b"]),
            &first_seen,
            TODAY,
        );
        assert!(d.send);
        assert_eq!(d.reason, "new_findings");
        assert_eq!(d.new_ids, vec!["vol-b"]);
        assert_eq!(d.next_state.ids, vec!["vol-a", "vol-b"]);
        assert_eq!(d.next_state.unattached_since, first_seen);
        assert_eq!(d.revert_state.ids, vec!["vol-a"]);
        assert_eq!(
            d.revert_state.last_message_ist.as_deref(),
            Some("2026-11-10")
        );
        assert_eq!(d.revert_state.unattached_since, first_seen);
    }

    #[test]
    fn test_decide_same_set_sends_nothing() {
        let d = decide(
            &ids_state(&["vol-a", "vol-b"], Some("2026-11-01")),
            &set(&["vol-a", "vol-b"]),
            &no_since(),
            TODAY,
        );
        assert!(!d.send);
        assert_eq!(d.reason, "no_new_findings");
        assert_eq!(d.next_state.last_message_ist.as_deref(), Some("2026-11-01"));
    }

    #[test]
    fn test_decide_resolved_sends_nothing_and_forgets_it() {
        let d = decide(
            &ids_state(&["vol-a", "vol-b"], Some("2026-11-01")),
            &set(&["vol-a"]),
            &no_since(),
            TODAY,
        );
        assert!(!d.send);
        assert_eq!(d.next_state.ids, vec!["vol-a"]);
        // If vol-b comes back later it is new again.
        let next = parse_state(Some(&serde_json::to_string(&d.next_state).unwrap()));
        let d2 = decide(&next, &set(&["vol-a", "vol-b"]), &no_since(), "2026-11-12");
        assert!(d2.send);
        assert_eq!(d2.new_ids, vec!["vol-b"]);
    }

    #[test]
    fn test_decide_mixed_new_and_resolved_sends_only_the_new() {
        let d = decide(
            &ids_state(&["vol-a", "vol-b"], None),
            &set(&["vol-b", "snap-c"]),
            &no_since(),
            TODAY,
        );
        assert!(d.send);
        assert_eq!(d.new_ids, vec!["snap-c"]);
        assert_eq!(d.next_state.ids, vec!["snap-c", "vol-b"]);
    }

    #[test]
    fn test_decide_corrupt_state_sends_everything_once() {
        let d = decide(
            &PreviousState::Corrupt("x".into()),
            &set(&["vol-a"]),
            &no_since(),
            TODAY,
        );
        assert!(d.send);
        assert_eq!(d.reason, "state_unreadable");
        assert_eq!(d.next_state.ids, vec!["vol-a"]);
    }

    #[test]
    fn test_decide_at_most_one_message_a_day() {
        // Already sent today: the new item waits and is NOT recorded, so it
        // is sent tomorrow; the resolved one is dropped.
        let d = decide(
            &ids_state(&["vol-a", "vol-gone"], Some(TODAY)),
            &set(&["vol-a", "vol-new"]),
            &no_since(),
            TODAY,
        );
        assert!(!d.send);
        assert_eq!(d.reason, "already_sent_today");
        assert_eq!(d.next_state.ids, vec!["vol-a"]);
        assert_eq!(d.next_state.last_message_ist.as_deref(), Some(TODAY));
        let next = parse_state(Some(&serde_json::to_string(&d.next_state).unwrap()));
        let d2 = decide(
            &next,
            &set(&["vol-a", "vol-new"]),
            &no_since(),
            "2026-11-12",
        );
        assert!(d2.send);
        assert_eq!(d2.new_ids, vec!["vol-new"]);
    }

    #[test]
    fn test_regression_a_big_set_sends_only_what_is_new() {
        // Finding: past ~150 ids a digest made one removal or one addition
        // re-send every item. 200 snapshots, one deleted: nothing is sent;
        // one volume added: only that volume is new.
        let many: BTreeSet<String> = (0..200).map(|i| format!("snap-{i:017}")).collect();
        let prev = PreviousState::Ids {
            ids: many.clone(),
            unattached_since: no_since(),
            last_message_ist: Some("2026-11-01".into()),
        };
        let mut shrunk = many.clone();
        shrunk.remove("snap-00000000000000007");
        let d = decide(&prev, &shrunk, &no_since(), TODAY);
        assert!(!d.send, "a deleted snapshot sends nothing");
        assert_eq!(d.reason, "no_new_findings");
        let mut grown = many.clone();
        grown.insert("vol-new".into());
        let d = decide(&prev, &grown, &no_since(), TODAY);
        assert!(d.send);
        assert_eq!(d.new_ids, vec!["vol-new"]);
    }

    // ---------------- message + audit ----------------

    #[test]
    fn test_waste_subject_reads_neutral_to_the_relay() {
        let lower = WASTE_SUBJECT.to_lowercase();
        assert!(!lower.contains("ok"), "{WASTE_SUBJECT}");
        assert!(!lower.contains("fail"), "{WASTE_SUBJECT}");
        assert!(!lower.contains("critical"), "{WASTE_SUBJECT}");
    }

    #[test]
    fn test_ist_stamp_is_twelve_hour_ist() {
        assert_eq!(ist_stamp(now()), "6:07 PM IST, 11 Nov 2026");
        let midnight = Utc.with_ymd_and_hms(2026, 11, 10, 18, 30, 0).unwrap();
        assert_eq!(ist_stamp(midnight), "12:00 AM IST, 11 Nov 2026");
    }

    #[test]
    fn test_regression_clean_drops_bidi_zero_width_and_non_ascii_marks() {
        // Finding: only Cc controls and five Markdown marks were stripped, so
        // a Name tag carrying U+202E could reorder the size and cost text.
        let tag =
            "\u{202E}old\u{2066}-root\u{2069}\u{200D}\u{200B}\u{FEFF}\u{061C}\u{2067}x\u{2068}";
        assert_eq!(clean(tag), "old-rootx");
        assert!(
            clean(tag).is_ascii(),
            "nothing but ASCII may reach the message"
        );
        assert_eq!(clean("A_b*c`[d]\n\t\u{7}"), "Abcd");
        assert_eq!(
            clean("tv prod (root) 30%, a+b=c & 'q'"),
            "tv prod (root) 30%, a+b=c & 'q'"
        );
        assert_eq!(clean("ñame"), "ame", "non-ASCII letters are dropped");
        assert_eq!(clean(&"n".repeat(80)), format!("{}…", "n".repeat(48)));
        assert_eq!(clean(&"n".repeat(48)), "n".repeat(48));
    }

    /// Review finding (2026-11-11): `:` `/` `.` `@` `#` were kept, so a
    /// `Name` tag could put a link Telegram makes clickable (a URL, a bare
    /// domain, an email, a mention or a hashtag) inside the waste message.
    #[test]
    fn test_regression_clean_cannot_form_a_link() {
        for tag in [
            "Verify at https://evil.example/aws-login",
            "see evil.example now",
            "mail owner@evil.example",
            "ping @some_bot",
            "#urgent",
            "ftp://x.y",
            "tg://resolve?domain=x",
        ] {
            let out = clean(tag);
            for c in [':', '/', '.', '@', '#'] {
                assert!(!out.contains(c), "{tag:?} -> {out:?} keeps {c:?}");
            }
        }
        assert_eq!(
            clean("Verify at https://evil.example/aws-login"),
            "Verify at httpsevilexampleaws-login"
        );
        // An address keeps the IP AWS assigned; a non-IP address label and
        // every disk or snapshot label still go through `clean`.
        assert_eq!(label_text(FindingKind::IdleAddress, "13.0.0.1"), "13.0.0.1");
        assert_eq!(
            label_text(FindingKind::IdleAddress, "2001:db8::1"),
            "2001:db8::1"
        );
        assert_eq!(
            label_text(FindingKind::IdleAddress, "http://evil.example"),
            "httpevilexample"
        );
        assert_eq!(label_text(FindingKind::UnattachedDisk, "13.0.0.1"), "13001");
        // The rendered message line carries no link either.
        let mut v = vol("vol-0abc", "available", 600, "gp3", Some(14 * DAY));
        v.name = Some("Verify at https://evil.example/aws-login".into());
        let all = collect_findings(
            &[v],
            &[],
            &[],
            now().timestamp(),
            &since(&[("vol-0abc", "2026-11-09")]),
            today_date(),
        );
        let new: Vec<&Finding> = all.iter().collect();
        let msg = render_message(&new, &all, &[], false, now());
        assert!(!msg.contains("https://"), "{msg}");
        assert!(!msg.contains("evil.example"), "{msg}");
        assert!(
            msg.contains("(Verify at httpsevilexampleaws-login)"),
            "{msg}"
        );
    }

    fn sample_findings() -> Vec<Finding> {
        let mut v = vol(
            "vol-0c6ab6e593e39d8c8",
            "available",
            600,
            "gp3",
            Some(14 * DAY),
        );
        v.name = Some("tv-rebuild-old-root".into());
        collect_findings(
            &[v],
            &[snap("snap-1", 30, Some(45 * DAY), false)],
            &[AddressFact {
                allocation_id: Some("eipalloc-1".into()),
                public_ip: Some("13.0.0.1".into()),
                ..Default::default()
            }],
            now().timestamp(),
            &since(&[("vol-0c6ab6e593e39d8c8", "2026-11-09")]),
            today_date(),
        )
    }

    #[test]
    fn test_render_message_names_each_item_size_cost_and_delete_step() {
        let all = sample_findings();
        let new: Vec<&Finding> = all.iter().collect();
        let top = vec![ServiceCost {
            service: "EC2 - Other".into(),
            usd: 21.5,
        }];
        let msg = render_message(&new, &all, &top, false, now());
        assert!(msg.starts_with("3 new items billing money"), "{msg}");
        assert!(msg.contains("Disk vol-0c6ab6e593e39d8c8 (tv-rebuild-old-root): 600 GB, attached to nothing since at least 9 Nov 2026, made 14 days ago. About $54.72 a month."), "{msg}");
        assert!(
            msg.contains("select vol-0c6ab6e593e39d8c8, Actions, Delete volume."),
            "{msg}"
        );
        assert!(
            msg.contains("Snapshot snap-1: 30 GB, taken 45 days ago. Up to $1.50 a month."),
            "{msg}"
        );
        assert!(msg.contains("Delete snapshot."), "{msg}");
        assert!(
            msg.contains("Fixed IP address eipalloc-1 (13.0.0.1)"),
            "{msg}"
        );
        assert!(msg.contains("Release Elastic IP addresses."), "{msg}");
        assert!(
            msg.contains("Top costs this month so far: EC2 - Other $21.50."),
            "{msg}"
        );
        assert!(
            msg.contains("it deleted nothing. Checked 6:07 PM IST, 11 Nov 2026."),
            "{msg}"
        );
        assert!(msg.contains("All waste found today: 3 items"), "{msg}");
        assert!(msg.chars().count() <= WASTE_MESSAGE_MAX_CHARS);
    }

    #[test]
    fn test_render_message_caps_the_list_and_marks_missing_or_partial_costs() {
        let vols: Vec<VolumeFact> = (0..20)
            .map(|i| {
                vol(
                    &format!("vol-{i:02}"),
                    "available",
                    1,
                    "gp3",
                    Some(10 * DAY),
                )
            })
            .collect();
        let first_seen: BTreeMap<String, String> = vols
            .iter()
            .map(|v| (v.id.clone(), "2026-11-01".to_string()))
            .collect();
        let all = collect_findings(
            &vols,
            &[],
            &[],
            now().timestamp(),
            &first_seen,
            today_date(),
        );
        let new: Vec<&Finding> = all.iter().collect();
        let msg = render_message(&new, &all, &[], false, now());
        assert!(msg.contains("...and 5 more"), "{msg}");
        assert!(!msg.contains("vol-15"), "{msg}");
        assert!(
            msg.contains("Top costs this month: not available today."),
            "{msg}"
        );
        assert!(msg.chars().count() <= WASTE_MESSAGE_MAX_CHARS);

        let one = &all[..1];
        let new1: Vec<&Finding> = one.iter().collect();
        let top = vec![ServiceCost {
            service: "A_b*c[d]".into(),
            usd: 1.0,
        }];
        let msg = render_message(&new1, one, &top, true, now());
        assert!(msg.starts_with("1 new item billing"), "{msg}");
        assert!(msg.contains("All waste found today: 1 item,"), "{msg}");
        assert!(msg.contains("so far (partial list): Abcd $1.00."), "{msg}");
    }

    #[test]
    fn test_regression_the_worst_case_message_fits_the_relay_and_keeps_its_tail() {
        // Finding: 15 real-length items with 48-character names ran past the
        // relay's 3,500-character cap, which cut off the "and N more" count,
        // the totals, the cost line and "it deleted nothing".
        let vols: Vec<VolumeFact> = (0..20)
            .map(|i| {
                let mut v = vol(
                    &format!("vol-0c6ab6e593e39d{i:03}"),
                    "available",
                    16_384,
                    "io2",
                    Some(1_000 * DAY),
                );
                v.name = Some(format!("{i:02}{}", "W".repeat(70)));
                v
            })
            .collect();
        let first_seen: BTreeMap<String, String> = vols
            .iter()
            .map(|v| (v.id.clone(), "2026-01-01".to_string()))
            .collect();
        let all = collect_findings(
            &vols,
            &[],
            &[],
            now().timestamp(),
            &first_seen,
            today_date(),
        );
        assert_eq!(all.len(), 20);
        let new: Vec<&Finding> = all.iter().collect();
        let top: Vec<ServiceCost> = (0..5)
            .map(|i| ServiceCost {
                service: format!(
                    "Amazon Elastic Compute Cloud Service Number {i} {}",
                    "Z".repeat(40)
                ),
                usd: 123_456.78,
            })
            .collect();
        let msg = render_message(&new, &all, &top, true, now());
        let chars = msg.chars().count();
        assert!(
            chars <= WASTE_MESSAGE_MAX_CHARS,
            "{chars} characters:\n{msg}"
        );
        let listed = msg.matches("To delete:").count();
        assert!((1..MAX_LISTED_ITEMS).contains(&listed), "{listed}");
        assert!(
            msg.contains(&format!(
                "...and {} more, listed in today's cost audit file.",
                20 - listed
            )),
            "{msg}"
        );
        assert!(msg.contains("All waste found today: 20 items"), "{msg}");
        assert!(
            msg.contains("Top costs this month so far (partial list): "),
            "{msg}"
        );
        assert!(
            msg.ends_with("it deleted nothing. Checked 6:07 PM IST, 11 Nov 2026."),
            "{msg}"
        );
    }

    #[test]
    fn test_render_message_unknown_age_and_long_label() {
        let mut v = vol("vol-x", "available", 5, "gp2", None);
        v.name = Some("n".repeat(80));
        let all = collect_findings(
            &[v],
            &[],
            &[],
            now().timestamp(),
            &since(&[("vol-x", "2026-11-01")]),
            today_date(),
        );
        let new: Vec<&Finding> = all.iter().collect();
        let msg = render_message(&new, &all, &[], false, now());
        assert!(msg.contains("made at an unknown time"), "{msg}");
        assert!(msg.contains(&format!("({}…)", "n".repeat(48))), "{msg}");
    }

    #[test]
    fn test_audit_key_uses_the_ist_date() {
        assert_eq!(audit_key("2026-11-11"), "cost-audit/2026-11-11.json");
    }

    #[test]
    fn test_audit_document_records_what_was_seen_and_decided() {
        let all = sample_findings();
        let prev = PreviousState::FirstRun;
        let cur: BTreeSet<String> = all.iter().map(|f| f.id.clone()).collect();
        let first_seen = since(&[("vol-0c6ab6e593e39d8c8", "2026-11-09")]);
        let d = decide(&prev, &cur, &first_seen, TODAY);
        let cost = CostOutcome::Read { partial: true };
        let doc = audit_document(&AuditInput {
            now_utc: now(),
            date_ist: TODAY,
            findings: &all,
            previous: &prev,
            decision: &d,
            message_sent: true,
            send_error: None,
            top: &[],
            cost: &cost,
            cost_rows_skipped: 2,
        });
        assert_eq!(doc["date_ist"], json!(TODAY));
        assert_eq!(doc["findings"].as_array().map(Vec::len), Some(3));
        assert_eq!(doc["findings"][2]["kind"], json!("unattached_disk"));
        assert_eq!(doc["findings"][2]["unattached_since"], json!("2026-11-09"));
        assert_eq!(doc["message_sent"], json!(true));
        assert_eq!(doc["send_error"], Value::Null);
        assert_eq!(doc["decision"], json!("first_run"));
        assert_eq!(doc["previous_state"], json!("first_run"));
        assert_eq!(doc["top_services_partial"], json!(true));
        assert_eq!(doc["cost_query"], json!("read_partial"));
        assert_eq!(doc["cost_query_error"], Value::Null);
        assert_eq!(doc["cost_rows_skipped"], json!(2));
        assert_eq!(doc["new_ids"].as_array().map(Vec::len), Some(3));
        assert_eq!(
            doc["unattached_since"]["vol-0c6ab6e593e39d8c8"],
            json!("2026-11-09")
        );
        assert!(doc["all_monthly_usd_estimate"].as_f64().unwrap() > 59.0);

        let down = CostOutcome::Unavailable("throttled".into());
        let doc = audit_document(&AuditInput {
            now_utc: now(),
            date_ist: TODAY,
            findings: &all,
            previous: &prev,
            decision: &d,
            message_sent: false,
            send_error: Some("sns:Publish failed: x"),
            top: &[],
            cost: &down,
            cost_rows_skipped: 0,
        });
        assert_eq!(doc["cost_query"], json!("unavailable"));
        assert_eq!(doc["cost_query_error"], json!("throttled"));
        assert_eq!(doc["send_error"], json!("sns:Publish failed: x"));
    }

    // ---------------- settings ----------------

    #[test]
    fn test_from_lookup_requires_both_settings() {
        let full = |k: &str| match k {
            "ALERTS_TOPIC_ARN" => Some("arn:topic".to_string()),
            "AUDIT_BUCKET" => Some(" tv-prod-cold ".to_string()),
            _ => None,
        };
        let env = WasteEnv::from_lookup(full).unwrap();
        assert_eq!(env.audit_bucket, "tv-prod-cold");
        assert!(
            WasteEnv::from_lookup(|k| if k == "AUDIT_BUCKET" { None } else { full(k) }).is_err()
        );
        assert!(
            WasteEnv::from_lookup(|k| if k == "ALERTS_TOPIC_ARN" {
                Some("  ".into())
            } else {
                full(k)
            })
            .is_err()
        );
    }

    // ---------------- SDK mapping ----------------

    #[test]
    fn test_volume_fact_snapshot_fact_and_address_fact_map_the_sdk_types() {
        use aws_sdk_ec2::primitives::DateTime as AwsTime;
        use aws_sdk_ec2::types::{
            Address, Snapshot, StorageTier, Tag, Volume, VolumeState, VolumeType,
        };
        let v = Volume::builder()
            .volume_id("vol-1")
            .state(VolumeState::Available)
            .size(600)
            .volume_type(VolumeType::Gp3)
            .create_time(AwsTime::from_secs(1_000))
            .tags(Tag::builder().key("Env").value("prod").build())
            .tags(Tag::builder().key("Name").value("old-root").build())
            .build();
        assert_eq!(
            volume_fact(&v),
            VolumeFact {
                id: "vol-1".into(),
                state: "available".into(),
                size_gib: 600,
                volume_type: "gp3".into(),
                created_unix: Some(1_000),
                name: Some("old-root".into()),
            }
        );
        assert_eq!(
            volume_fact(&Volume::builder().build()),
            VolumeFact::default()
        );

        let s = Snapshot::builder()
            .snapshot_id("snap-1")
            .volume_size(30)
            .start_time(AwsTime::from_secs(5))
            .storage_tier(StorageTier::Archive)
            .build();
        let f = snapshot_fact(&s);
        assert_eq!(f.id, "snap-1");
        assert_eq!(f.size_gib, 30);
        assert_eq!(f.started_unix, Some(5));
        assert!(f.archived);
        assert!(
            !snapshot_fact(
                &Snapshot::builder()
                    .storage_tier(StorageTier::Standard)
                    .build()
            )
            .archived
        );

        let a = Address::builder()
            .allocation_id("eipalloc-1")
            .public_ip("13.0.0.1")
            .association_id("eipassoc-1")
            .instance_id("i-1")
            .network_interface_id("eni-1")
            .build();
        let f = address_fact(&a);
        assert_eq!(f.allocation_id.as_deref(), Some("eipalloc-1"));
        assert_eq!(f.association_id.as_deref(), Some("eipassoc-1"));
        assert_eq!(f.instance_id.as_deref(), Some("i-1"));
        assert_eq!(f.network_interface_id.as_deref(), Some("eni-1"));
        assert_eq!(
            address_fact(&Address::builder().build()),
            AddressFact::default()
        );
    }

    #[test]
    fn test_cost_rows_reads_groups_and_the_next_page_marker() {
        use aws_sdk_costexplorer::operation::get_cost_and_usage::GetCostAndUsageOutput;
        use aws_sdk_costexplorer::types::{Group, MetricValue, ResultByTime};
        let group = |name: &str, amount: &str| {
            Group::builder()
                .keys(name)
                .metrics(
                    "UnblendedCost",
                    MetricValue::builder().amount(amount).unit("USD").build(),
                )
                .build()
        };
        let out = GetCostAndUsageOutput::builder()
            .results_by_time(
                ResultByTime::builder()
                    .groups(group("EC2 - Other", "21.5"))
                    .groups(Group::builder().build())
                    .build(),
            )
            .results_by_time(
                ResultByTime::builder()
                    .groups(group("EC2 - Other", "1"))
                    .build(),
            )
            .next_page_token("more")
            .build();
        let rows = cost_rows(&out);
        assert!(rows.more_pages);
        assert_eq!(
            rows.rows,
            vec![
                ("EC2 - Other".to_string(), "21.5".to_string()),
                (String::new(), String::new()),
                ("EC2 - Other".to_string(), "1".to_string()),
            ]
        );
        let (top, skipped) = top_services(&rows.rows);
        assert_eq!(skipped, 1);
        assert!((top[0].usd - 22.5).abs() < 1e-9);
        let empty = cost_rows(&GetCostAndUsageOutput::builder().next_page_token("").build());
        assert!(!empty.more_pages);
        assert!(empty.rows.is_empty());
    }

    // ---------------- the run, against a fake AWS ----------------

    /// A fake AWS. Every create-only claim lands in `claims` (a set), so two
    /// runs sharing one fake share the day claims, the state and the topic.
    #[derive(Default)]
    struct FakeAws {
        vols: Vec<VolumeFact>,
        snaps: Vec<SnapshotFact>,
        addrs: Vec<AddressFact>,
        costs: ServiceCostRows,
        state: RefCell<Option<String>>,
        fail: Vec<&'static str>,
        /// Fail only the Nth state write (1-based) of this fake's life.
        fail_state_write_number: Option<usize>,
        cost_calls: Cell<usize>,
        cost_period: RefCell<Option<(NaiveDate, NaiveDate)>>,
        published: RefCell<Vec<(String, String, String)>>,
        state_writes: Cell<usize>,
        /// Order of the side effects, to prove the state is written first.
        events: RefCell<Vec<String>>,
        claims: RefCell<BTreeSet<String>>,
        audits: RefCell<Vec<(String, String, String)>>,
    }

    impl FakeAws {
        fn check(&self, call: &str) -> Result<(), String> {
            if self.fail.contains(&call) {
                Err(format!("{call} refused"))
            } else {
                Ok(())
            }
        }
    }

    impl WasteAws for FakeAws {
        async fn volumes(&self) -> Result<Vec<VolumeFact>, String> {
            self.check("volumes")?;
            Ok(self.vols.clone())
        }
        async fn snapshots(&self) -> Result<Vec<SnapshotFact>, String> {
            self.check("snapshots")?;
            Ok(self.snaps.clone())
        }
        async fn addresses(&self) -> Result<Vec<AddressFact>, String> {
            self.check("addresses")?;
            Ok(self.addrs.clone())
        }
        async fn service_costs(
            &self,
            start: NaiveDate,
            end_exclusive: NaiveDate,
        ) -> Result<ServiceCostRows, String> {
            self.cost_calls.set(self.cost_calls.get() + 1);
            *self.cost_period.borrow_mut() = Some((start, end_exclusive));
            self.check("costs")?;
            Ok(self.costs.clone())
        }
        async fn read_state(&self, bucket: &str, key: &str) -> Result<Option<String>, String> {
            assert_eq!((bucket, key), ("tv-prod-cold", STATE_KEY));
            self.check("read_state")?;
            Ok(self.state.borrow().clone())
        }
        async fn write_state(&self, bucket: &str, key: &str, value: &str) -> Result<(), String> {
            assert_eq!((bucket, key), ("tv-prod-cold", STATE_KEY));
            let n = self.state_writes.get() + 1;
            self.state_writes.set(n);
            self.check("write_state")?;
            if self.fail_state_write_number == Some(n) {
                return Err(format!("state write {n} refused"));
            }
            self.events.borrow_mut().push("write_state".into());
            *self.state.borrow_mut() = Some(value.to_string());
            Ok(())
        }
        async fn claim(&self, bucket: &str, key: &str) -> Result<bool, String> {
            assert_eq!(bucket, "tv-prod-cold");
            assert!(key.starts_with(CLAIM_PREFIX), "{key}");
            if key.ends_with("-cost-query") {
                self.check("claim_cost")?;
            } else {
                self.check("claim_message")?;
            }
            self.events.borrow_mut().push(format!("claim {key}"));
            Ok(self.claims.borrow_mut().insert(key.to_string()))
        }
        async fn publish(
            &self,
            topic: &str,
            subject: &str,
            message: &str,
        ) -> Result<(), PublishError> {
            // "publish": SNS refused it (4xx), nothing delivered.
            self.check("publish").map_err(PublishError::Refused)?;
            self.events.borrow_mut().push("publish".into());
            self.published
                .borrow_mut()
                .push((topic.into(), subject.into(), message.into()));
            // "publish_lost_reply": SNS accepted and delivered it, but the
            // reply never arrived (timeout / connection reset).
            self.check("publish_lost_reply")
                .map_err(PublishError::Unknown)?;
            Ok(())
        }
        async fn put_audit(&self, bucket: &str, key: &str, body: String) -> Result<(), String> {
            self.check("put_audit")?;
            self.audits
                .borrow_mut()
                .push((bucket.into(), key.into(), body));
            Ok(())
        }
    }

    fn env() -> WasteEnv {
        WasteEnv {
            alerts_topic_arn: "arn:aws:sns:ap-south-1:1:tv-prod-alerts".into(),
            audit_bucket: "tv-prod-cold".into(),
        }
    }

    /// The November incident, with an earlier run having seen both volumes
    /// unattached (but never having sent a message).
    fn incident_aws() -> FakeAws {
        FakeAws {
            vols: vec![
                vol(
                    "vol-0c6ab6e593e39d8c8",
                    "available",
                    600,
                    "gp3",
                    Some(14 * DAY),
                ),
                vol(
                    "vol-073ccaa417a0f344b",
                    "available",
                    30,
                    "gp3",
                    Some(180 * DAY),
                ),
            ],
            snaps: vec![snap("snap-old", 30, Some(60 * DAY), false)],
            costs: ServiceCostRows {
                rows: vec![("EC2 - Other".into(), "21.5".into())],
                more_pages: false,
            },
            state: RefCell::new(Some(
                r#"{"v":1,"ids":[],"unattached_since":{"vol-0c6ab6e593e39d8c8":"2026-11-09","vol-073ccaa417a0f344b":"2026-11-01"}}"#
                    .to_string(),
            )),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_run_waste_check_sends_then_is_silent_next_day() {
        let aws = incident_aws();
        let out = run_waste_check(&aws, &env(), now()).await.unwrap();
        assert_eq!(out["message_sent"], json!(true));
        assert_eq!(out["decision"], json!("new_findings"));
        assert_eq!(out["cost_query"], json!("read"));
        assert_eq!(out["audit_key"], json!("cost-audit/2026-11-11.json"));
        assert_eq!(
            aws.cost_calls.get(),
            1,
            "exactly one Cost Explorer call per run"
        );
        assert_eq!(
            *aws.cost_period.borrow(),
            Some((
                NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 12).unwrap()
            ))
        );
        {
            let published = aws.published.borrow();
            assert_eq!(published.len(), 1);
            assert_eq!(published[0].1, WASTE_SUBJECT);
            assert!(published[0].2.contains("vol-0c6ab6e593e39d8c8"));
            assert!(published[0].2.contains("vol-073ccaa417a0f344b"));
            assert!(published[0].2.contains("snap-old"));
            assert!(published[0].2.contains("EC2 - Other $21.50"));
        }
        {
            let audits = aws.audits.borrow();
            assert_eq!(audits.len(), 1);
            assert_eq!(audits[0].0, "tv-prod-cold");
            let doc: Value = serde_json::from_str(&audits[0].2).unwrap();
            assert_eq!(doc["message_sent"], json!(true));
        }

        // Same facts the next day: nothing sent, state and audit still written.
        let next_day = now() + Duration::days(1);
        let out = run_waste_check(&aws, &env(), next_day).await.unwrap();
        assert_eq!(out["message_sent"], json!(false));
        assert_eq!(out["decision"], json!("no_new_findings"));
        assert_eq!(aws.published.borrow().len(), 1);
        assert_eq!(aws.audits.borrow().len(), 2);
        assert_eq!(aws.audits.borrow()[1].1, "cost-audit/2026-11-12.json");
        assert_eq!(aws.state_writes.get(), 2);
        assert_eq!(aws.cost_calls.get(), 2, "one query on each day");
    }

    #[tokio::test]
    async fn test_run_waste_check_first_run_reports_no_disk_until_two_days_later() {
        let mut aws = incident_aws();
        *aws.state.borrow_mut() = None;
        aws.snaps.clear();
        let out = run_waste_check(&aws, &env(), now()).await.unwrap();
        assert_eq!(out["message_sent"], json!(false));
        assert_eq!(out["decision"], json!("no_findings"));
        let out = run_waste_check(&aws, &env(), now() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(false));
        let out = run_waste_check(&aws, &env(), now() + Duration::days(2))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(true));
        assert!(
            aws.published.borrow()[0]
                .2
                .contains("since at least 11 Nov 2026")
        );
    }

    #[tokio::test]
    async fn test_run_waste_check_a_rerun_the_same_day_sends_nothing() {
        let aws = incident_aws();
        run_waste_check(&aws, &env(), now()).await.unwrap();
        let mut more = incident_aws();
        more.vols
            .push(vol("vol-new", "available", 8, "gp3", Some(3 * DAY)));
        *more.state.borrow_mut() = aws.state.borrow().clone();
        let out = run_waste_check(&more, &env(), now() + Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(
            out["decision"],
            json!("no_new_findings"),
            "vol-new is first seen now"
        );
        // Two days later the new volume is sent.
        let out = run_waste_check(&more, &env(), now() + Duration::days(2))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(true));
        let published = more.published.borrow();
        assert!(
            published[0].2.starts_with("1 new item"),
            "{}",
            published[0].2
        );
        assert!(published[0].2.contains("vol-new"));
    }

    #[tokio::test]
    async fn test_run_waste_check_already_sent_today_holds_new_items_for_tomorrow() {
        let aws = incident_aws();
        run_waste_check(&aws, &env(), now()).await.unwrap();
        let mut more = incident_aws();
        more.snaps.push(snap("snap-new", 8, Some(40 * DAY), false));
        *more.state.borrow_mut() = aws.state.borrow().clone();
        let out = run_waste_check(&more, &env(), now() + Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(out["decision"], json!("already_sent_today"));
        assert!(more.published.borrow().is_empty());
        let out = run_waste_check(&more, &env(), now() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(true));
        assert!(more.published.borrow()[0].2.contains("snap-new"));
    }

    #[tokio::test]
    async fn test_run_waste_check_no_findings_sends_nothing() {
        let aws = FakeAws::default();
        let out = run_waste_check(&aws, &env(), now()).await.unwrap();
        assert_eq!(out["decision"], json!("no_findings"));
        assert!(aws.published.borrow().is_empty());
        assert_eq!(aws.audits.borrow().len(), 1);
    }

    #[tokio::test]
    async fn test_run_waste_check_any_ec2_or_state_read_error_fails_loud_and_writes_nothing() {
        for call in ["volumes", "snapshots", "addresses", "read_state"] {
            let mut aws = incident_aws();
            *aws.state.borrow_mut() = Some(r#"{"v":1,"ids":["vol-keep"]}"#.into());
            aws.fail = vec![call];
            let err = run_waste_check(&aws, &env(), now()).await.unwrap_err();
            assert!(err.contains("failed"), "{call}: {err}");
            assert!(aws.published.borrow().is_empty(), "{call}");
            assert!(aws.audits.borrow().is_empty(), "{call}");
            assert!(aws.claims.borrow().is_empty(), "{call}: no claim taken");
            assert_eq!(aws.cost_calls.get(), 0, "{call}: no paid query");
            assert_eq!(aws.state_writes.get(), 0, "{call}");
            assert_eq!(
                aws.state.borrow().as_deref(),
                Some(r#"{"v":1,"ids":["vol-keep"]}"#),
                "{call}: the state is never wiped"
            );
        }
    }

    #[tokio::test]
    async fn test_regression_a_cost_explorer_failure_still_reports_the_waste() {
        // Finding: a Cost Explorer error (context only) aborted the run, so
        // a new 600 GB disk was never reported.
        for fail in ["costs", "claim_cost"] {
            let mut aws = incident_aws();
            aws.fail = vec![fail];
            let out = run_waste_check(&aws, &env(), now()).await.unwrap();
            assert_eq!(out["message_sent"], json!(true), "{fail}");
            assert_eq!(out["cost_query"], json!("unavailable"), "{fail}");
            let published = aws.published.borrow();
            assert!(published[0].2.contains("vol-0c6ab6e593e39d8c8"), "{fail}");
            assert!(
                published[0]
                    .2
                    .contains("Top costs this month: not available today."),
                "{fail}"
            );
            let doc: Value = serde_json::from_str(&aws.audits.borrow()[0].2).unwrap();
            assert_eq!(doc["cost_query"], json!("unavailable"), "{fail}");
            assert!(doc["cost_query_error"].as_str().is_some(), "{fail}");
        }
    }

    #[tokio::test]
    async fn test_regression_one_cost_query_per_ist_day_across_reruns() {
        // Finding: every manual or test invoke made another billed query.
        let aws = incident_aws();
        run_waste_check(&aws, &env(), now()).await.unwrap();
        for hours in [1, 2, 5] {
            let out = run_waste_check(&aws, &env(), now() + Duration::hours(hours))
                .await
                .unwrap();
            assert_eq!(out["cost_query"], json!("already_queried_today"));
        }
        assert_eq!(aws.cost_calls.get(), 1, "one query for the whole IST day");
        let out = run_waste_check(&aws, &env(), now() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(out["cost_query"], json!("read"));
        assert_eq!(aws.cost_calls.get(), 2);
    }

    #[tokio::test]
    async fn test_regression_two_deliveries_at_once_send_one_message() {
        // Finding: two invocations of one event both read the same state and
        // both published. They now share the day's create-only claim.
        let first = incident_aws();
        let second = incident_aws();
        // Both read the same old state; the second sees the first's claims.
        let out1 = run_waste_check(&first, &env(), now()).await.unwrap();
        *second.claims.borrow_mut() = first.claims.borrow().clone();
        let out2 = run_waste_check(&second, &env(), now() + Duration::seconds(3))
            .await
            .unwrap();
        assert_eq!(out1["message_sent"], json!(true));
        assert_eq!(out2["decision"], json!("already_claimed_today"));
        assert_eq!(out2["message_sent"], json!(false));
        assert!(second.published.borrow().is_empty());
        assert_eq!(second.state_writes.get(), 0, "the loser writes nothing");
        assert!(
            second.audits.borrow().is_empty(),
            "the loser writes nothing"
        );
        assert_eq!(second.cost_calls.get(), 0, "the day's query was taken too");
    }

    #[tokio::test]
    async fn test_regression_the_state_is_recorded_as_sent_before_the_message() {
        // Finding: the message went out before the state was written, so a
        // failed state write repeated it the next day.
        let aws = incident_aws();
        run_waste_check(&aws, &env(), now()).await.unwrap();
        let events = aws.events.borrow();
        let write = events.iter().position(|e| e == "write_state").unwrap();
        let publish = events.iter().position(|e| e == "publish").unwrap();
        assert!(write < publish, "{events:?}");
    }

    #[tokio::test]
    async fn test_regression_a_failed_state_write_sends_nothing_and_never_repeats() {
        let mut aws = incident_aws();
        aws.fail = vec!["write_state"];
        let err = run_waste_check(&aws, &env(), now()).await.unwrap_err();
        assert!(err.contains("state write before the message"), "{err}");
        assert!(aws.published.borrow().is_empty(), "nothing sent");
        // The next day (writes work again) sends it once.
        aws.fail.clear();
        let out = run_waste_check(&aws, &env(), now() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(true));
        let out = run_waste_check(&aws, &env(), now() + Duration::days(2))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(false));
        assert_eq!(aws.published.borrow().len(), 1, "never a repeat");
    }

    #[tokio::test]
    async fn test_run_waste_check_publish_failure_puts_the_old_state_back() {
        let mut aws = incident_aws();
        let before = aws.state.borrow().clone();
        aws.fail = vec!["publish"];
        let err = run_waste_check(&aws, &env(), now()).await.unwrap_err();
        assert!(
            err.contains("sns:Publish") && err.contains("state put back"),
            "{err}"
        );
        assert_eq!(aws.state_writes.get(), 2, "sent-state, then put back");
        let state = parse_state(aws.state.borrow().as_deref());
        let original = parse_state(before.as_deref());
        assert_eq!(state.ids(), original.ids());
        assert_eq!(state.last_message_ist(), None);
        let doc: Value = serde_json::from_str(&aws.audits.borrow()[0].2).unwrap();
        assert_eq!(doc["message_sent"], json!(false));
        assert!(doc["send_error"].as_str().is_some());
        // Tomorrow the same items are sent.
        aws.fail.clear();
        let out = run_waste_check(&aws, &env(), now() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(true));
    }

    #[tokio::test]
    async fn test_run_waste_check_publish_and_put_back_failure_costs_a_miss_not_a_repeat() {
        let mut aws = incident_aws();
        aws.fail = vec!["publish"];
        aws.fail_state_write_number = Some(2);
        let err = run_waste_check(&aws, &env(), now()).await.unwrap_err();
        assert!(err.contains("state put back failed"), "{err}");
        aws.fail.clear();
        let out = run_waste_check(&aws, &env(), now() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(false), "missed, never repeated");
    }

    /// Review finding (2026-11-11): SNS accepted the message, the reply was
    /// lost, and the run put the old state back, so the next day sent the
    /// same items again. An unknown outcome now keeps the "sent" state.
    #[tokio::test]
    async fn test_regression_a_lost_publish_reply_keeps_the_sent_state_and_never_repeats() {
        let mut aws = incident_aws();
        aws.fail = vec!["publish_lost_reply"];
        let err = run_waste_check(&aws, &env(), now()).await.unwrap_err();
        assert!(
            err.contains("outcome unknown") && err.contains("sent state kept"),
            "{err}"
        );
        assert_eq!(aws.published.borrow().len(), 1, "SNS delivered it once");
        assert_eq!(aws.state_writes.get(), 1, "the sent state is not put back");
        let state = parse_state(aws.state.borrow().as_deref());
        assert_eq!(state.last_message_ist(), Some("2026-11-11"));
        assert!(state.ids().contains("vol-0c6ab6e593e39d8c8"));
        let doc: Value = serde_json::from_str(&aws.audits.borrow()[0].2).unwrap();
        assert_eq!(doc["message_sent"], json!(false));
        assert!(
            doc["send_error"]
                .as_str()
                .is_some_and(|s| s.contains("outcome unknown")),
            "{doc}"
        );
        // The next day sees the same set: nothing is sent again.
        aws.fail.clear();
        let out = run_waste_check(&aws, &env(), now() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(false), "never a repeat");
        assert_eq!(aws.published.borrow().len(), 1, "still exactly one send");
    }

    #[test]
    fn test_publish_refused_only_when_certainly_not_delivered() {
        assert!(publish_refused(true, None), "never sent");
        assert!(publish_refused(false, Some(400)));
        assert!(publish_refused(false, Some(403)));
        assert!(publish_refused(false, Some(429)), "throttled: refused");
        assert!(publish_refused(false, Some(499)));
        assert!(!publish_refused(false, None), "timeout or network error");
        assert!(!publish_refused(false, Some(500)), "a 5xx may have landed");
        assert!(!publish_refused(false, Some(503)));
        assert!(
            !publish_refused(false, Some(200)),
            "an unreadable 2xx landed"
        );
        assert!(!publish_refused(false, Some(399)));
    }

    #[test]
    fn test_publish_error_display_names_the_kind() {
        assert_eq!(
            PublishError::Refused("AuthorizationError".into()).to_string(),
            "refused: AuthorizationError"
        );
        assert_eq!(
            PublishError::Unknown("timeout".into()).to_string(),
            "outcome unknown: timeout"
        );
    }

    /// Review finding (2026-11-11): the SNS client kept the SDK's standard
    /// retry (3 attempts), so a publish SNS accepted could be sent twice in
    /// one run, and a claim could be retried into its own 412.
    #[test]
    fn test_regression_sdk_waste_aws_new_sends_and_claims_in_one_attempt() {
        let base = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("ap-south-1"))
            .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(3))
            .build();
        let ce = crate::clients::cost_explorer_from(&base);
        let aws = SdkWasteAws::new(&base, ce);
        let attempts = |c: Option<&aws_config::retry::RetryConfig>| c.map(|r| r.max_attempts());
        assert_eq!(
            attempts(aws.sns.config().retry_config()),
            Some(1),
            "publish"
        );
        assert_eq!(
            attempts(aws.s3_claim.config().retry_config()),
            Some(1),
            "day claims"
        );
        assert_eq!(
            attempts(aws.ce.config().retry_config()),
            Some(1),
            "cost query"
        );
        // State and audit writes overwrite a whole object, so they keep
        // the config's retries.
        assert_eq!(attempts(aws.s3.config().retry_config()), Some(3));
    }

    #[tokio::test]
    async fn test_run_waste_check_message_claim_failure_sends_and_writes_nothing() {
        let mut aws = incident_aws();
        aws.fail = vec!["claim_message"];
        let err = run_waste_check(&aws, &env(), now()).await.unwrap_err();
        assert!(err.contains("message claim failed"), "{err}");
        assert!(aws.published.borrow().is_empty());
        assert_eq!(aws.state_writes.get(), 0);
        assert!(aws.audits.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_run_waste_check_audit_failure_still_writes_the_state() {
        let mut aws = incident_aws();
        aws.fail = vec!["put_audit"];
        let err = run_waste_check(&aws, &env(), now()).await.unwrap_err();
        assert!(err.contains("s3:PutObject"), "{err}");
        assert_eq!(aws.state_writes.get(), 1, "the state is still written");
        assert_eq!(aws.published.borrow().len(), 1);

        // A state write failure on a run that sends nothing still writes
        // the audit file.
        let aws = FakeAws {
            fail: vec!["write_state"],
            ..Default::default()
        };
        let err = run_waste_check(&aws, &env(), now()).await.unwrap_err();
        assert!(err.contains("state write failed"), "{err}");
        assert_eq!(aws.audits.borrow().len(), 1);
    }

    #[tokio::test]
    async fn test_run_waste_check_corrupt_state_sends_once_and_repairs_it() {
        let mut aws = incident_aws();
        aws.vols.clear();
        *aws.state.borrow_mut() = Some("garbage".into());
        let out = run_waste_check(&aws, &env(), now()).await.unwrap();
        assert_eq!(out["decision"], json!("state_unreadable"));
        assert_eq!(aws.published.borrow().len(), 1);
        let repaired = aws.state.borrow().clone();
        assert!(matches!(
            parse_state(repaired.as_deref()),
            PreviousState::Ids { .. }
        ));
    }

    #[tokio::test]
    async fn test_run_waste_check_resolved_item_sends_nothing() {
        let aws = incident_aws();
        run_waste_check(&aws, &env(), now()).await.unwrap();
        let mut fewer = incident_aws();
        fewer.vols.remove(0);
        *fewer.state.borrow_mut() = aws.state.borrow().clone();
        let out = run_waste_check(&fewer, &env(), now() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(out["message_sent"], json!(false));
        assert!(fewer.published.borrow().is_empty());
        let state = parse_state(fewer.state.borrow().as_deref());
        assert!(
            !state
                .unattached_since()
                .contains_key("vol-0c6ab6e593e39d8c8"),
            "a volume no longer unattached leaves the first-seen map"
        );
    }
}
