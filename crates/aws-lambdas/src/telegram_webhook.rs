//! Telegram webhook — Z+ L1 DETECT layer (Rust port of
//! `deploy/aws/lambda/telegram-webhook/handler.py`, phase 2b-2 wave 1).
//!
//! Receives SNS messages from `tv-alerts` (CloudWatch alarms OR direct
//! `aws sns publish` from the deploy-aws workflow) and forwards them to
//! the operator's Telegram chat via the bot API.
//!
//! House style (2026-07-07 Telegram UX overhaul, judge final contract):
//! every CloudWatch alarm renders as `{emoji} {plain-English line}` +
//! an IST 12-hour timestamp. The raw CloudWatch `NewStateReason`
//! ("Threshold Crossed: 1 datapoint ...") NEVER reaches Telegram — it is
//! logged to CloudWatch Logs only (tracing), for forensics. ALARM+OK
//! pairs inside one SNS batch fold to a single recovered line; lone OK
//! flips fold into ONE recovered line; ALARM records are NEVER folded,
//! digested, or suppressed. Messages are sent as plain text (no
//! markup-parsing payload field) so an alarm name containing '*' or '_'
//! can never trigger a silent Markdown-parse 400 drop.
//!
//! Environment variables (set by Terraform — unchanged from the legacy runtime):
//!   TELEGRAM_BOT_TOKEN_SSM_PARAM  — SSM path holding the bot token
//!   TELEGRAM_CHAT_ID_SSM_PARAM    — SSM path holding the chat ID
//!   LOG_LEVEL                     — INFO (default) / DEBUG / WARNING
//!
//! Parity ledger (every deliberate deviation from the legacy original):
//! - legacy `print()` forensics → `tracing::info!` (same CloudWatch Logs
//!   destination; the crate denies print_stdout).
//! - legacy edge-dated timestamps (year 0001/9999) raised OverflowError on
//!   the tz conversion and fell back to the invocation time; chrono
//!   converts those dates without overflow, so the rendered clock is the
//!   real converted time — both satisfy the `H:MM AM|PM` format contract.
//! - legacy `str(dict)` renders `{'k': 'v'}`; Rust renders the serde_json
//!   representation `{"k":"v"}` for non-string SNS Message bodies (only
//!   reachable on crafted/malformed publishes).
//! - serde_json enforces a 128-level recursion limit that the legacy runtime's
//!   `json.loads` did not share at moderate depth: a VALID alarm JSON
//!   carrying a >128-deep irrelevant nested field parsed in legacy
//!   (depth ~200 → house line; extreme ~1000+ depth raised
//!   RecursionError → the fail-open generic safe line — oracle-verified
//!   2026-07-18 against the recovered handler.py) but fails
//!   `parse_alarm` here and takes the plain-SNS fallback instead. That
//!   fallback is therefore REDACTED (`redact_new_state_reason` blanks
//!   every `"NewStateReason"` value, escape-aware) and length-capped
//!   (`PLAIN_FALLBACK_BODY_MAX_CHARS`), so raw forensic text still
//!   NEVER reaches the Telegram body — a fail-safe divergence,
//!   unreachable from real CloudWatch publishes (real alarm JSON is
//!   shallow; reaching this arm requires a crafted `sns:Publish`).
//! - legacy `_fold_records(cache=None)` optionality collapsed: every call
//!   site (lambda handler + all tests) passes a cache, so the Rust fn
//!   takes `&mut HashMap` unconditionally.
//! - legacy truthy-non-dict records / Sns values raised AttributeError →
//!   generic safe line; Rust maps every non-object, non-null shape to the
//!   same generic safe line (exotic FALSY non-dicts like `""`/`[]`/`0`
//!   would have rendered "🔔 " in the legacy runtime — no test pins that; documented).
//! - the legacy runtime wrapped the render loop + the whole fold in `except Exception`
//!   fail-open arms; the Rust render chain has no panicking path, so those
//!   arms are structurally unreachable and not reproduced.
//! - reqwest errors are stripped of their URL (`Error::without_url`)
//!   before entering the failures list — the URL embeds the bot token
//!   (hardening beyond legacy; urllib error strings never carried it).
//! - env vars are read per invocation instead of at module import — same
//!   effective behavior inside a Lambda container.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, FixedOffset, NaiveDateTime, Offset, Timelike, Utc};
use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::{error, info, warn};

pub const TELEGRAM_API_BASE: &str = "https://api.telegram.org"; // APPROVED: infrastructure constant — the Telegram Bot API base (Lambda-side, legacy webhook parity)
pub const TELEGRAM_TIMEOUT_SECONDS: u64 = 8;

/// Warm-container duplicate-OK guard (judge contract, robustness graft):
/// suppress a repeat SAME-state OK for the same alarm within this window.
/// ALARM-state records are NEVER consulted against this cache — a dropped
/// 🆘 is unacceptable; a duplicate ✅ after a cold start is accepted.
pub const OK_REPEAT_SUPPRESS_SECS: f64 = 300.0;

/// Window within which a REPEAT ALARM for the SAME alarm, in the SAME state,
/// is rendered as a one-line "still down" page instead of the full two-line
/// house line.
///
/// # This is COALESCING, never suppression
///
/// The never-drop law above is correct and is unchanged: an ALARM record is
/// ALWAYS delivered. A recorded incident produced 25 pages from one flapping
/// alarm in a single session, and the fix for that is NOT to start dropping
/// 🆘 — a dropped page is unrecoverable, a verbose page is merely annoying.
/// What this does is make the 2nd and later pages of one ongoing episode
/// SHORT and self-labelling ("3rd page in 10 min"), so a burst reads as one
/// event rather than as three unrelated emergencies.
///
/// # The key is the exact alarm name AND state — never anything coarser
///
/// Coalescing is keyed on the alarm NAME (the cache key) plus its cached
/// STATE. It must never be widened to a category, a prefix, a severity or a
/// time bucket. A second, DIFFERENT alarm firing while a noisy first one is
/// active has to arrive as its own full page: several alarms in this account
/// are the only signal for their condition anywhere — the unrecovered-frames
/// alarm says so in its own description — so swallowing one inside another's
/// streak would lose the only notification that condition will ever produce.
/// `alarm_coalescing_is_never_widened_across_different_alarms` pins this.
///
/// 600 s = ten minutes, chosen to match the longest evaluation cadence in the
/// alarm set (5-minute periods x 2 evaluation periods), so one ongoing
/// condition re-notifying on its natural schedule reads as one episode.
pub const ALARM_REPEAT_COALESCE_SECS: f64 = 600.0;

/// Per-alarm memory carried across a warm Lambda container: the last state
/// delivered for that alarm name, the epoch seconds it was delivered at, and
/// how many consecutive ALARM pages the current episode has produced.
///
/// # Honest limit: this is WARM-CONTAINER ONLY
///
/// AWS keeps a Lambda container warm for roughly fifteen minutes of idle time
/// and may recycle it at any moment, and concurrent invocations get separate
/// containers with separate copies of this map. So the streak count is a
/// best-effort convenience, not a guarantee: after a cold start, or on a
/// second concurrent container, an ongoing episode's next page renders as a
/// FULL first page again. That direction is deliberate — losing the "3rd page"
/// label costs nothing, whereas persisting this state anywhere would put a
/// storage dependency between an emergency and the operator's phone.
pub type AlertCache = std::collections::HashMap<String, (String, f64, u32)>;

pub const GENERIC_SAFE_LINE: &str = "🔔 Alert received — details are in the server log";

/// Known alarm → auto-driver plain English (charter §D: no library names,
/// no file paths, no jargon). Keys are alarm names AFTER the tv-<env>-
/// prefix strip. Unknown names fall back to a humanized form — never a
/// lookup panic, never raw JSON.
///
/// # Why this table must be COMPLETE, and is now guarded
///
/// It carried 29 entries, of which 8 named alarms that no longer exist and 21
/// matched a live one — so 83 of the 102 live alarms rendered as a humanized
/// slug ("Errcode ws gap 03 xverify vacuous"), which is the alarm name with
/// the hyphens taken out, not plain English. The errcode family is the entire
/// coded-error route to the operator's phone and had NO entries at all.
///
/// The alternative considered and REJECTED was delivering each alarm's
/// terraform `AlarmDescription` into the message. It is forensic free text a
/// future engineer could paste anything into, and the house contract — stated
/// at the `NewStateReason` redactor below — is that free-text forensic fields
/// never reach Telegram. Writing the phrases by hand keeps every word that
/// reaches the phone reviewed in this file.
///
/// Staleness is what made this table useless, so it is now MECHANICALLY
/// pinned in both directions by
/// `crates/aws-lambdas/tests/alarm_phrase_coverage_guard.rs`: every live alarm
/// must have an entry, and every entry must name a live alarm.
///
/// O(n) scan per lookup — cold path (a handful of alarm renders per SNS
/// batch), deliberately not a hash map so the table stays a reviewable literal.
pub const ALARM_PHRASES: [(&str, &str); 111] = [
    // ---- capacity + candle building ----
    (
        "aggregator-refusal-rate-high",
        "🔷 DHAN: more than a quarter of prices arrive with a bad time stamp — the prices ARE saved, only the per-minute summary skips them. Normal is under 10%",
    ),
    (
        "aggregator-slots-exhausted",
        "🔷 DHAN: ran out of candle slots — some instruments are no longer getting candles",
    ),
    (
        "seal-writer-dropped",
        "Finished candles were thrown away instead of being saved",
    ),
    (
        "seal-writer-rescued",
        "Finished candles had to be written to a rescue file instead of the database",
    ),
    // ---- app + host health ----
    (
        "api-auth-failed",
        "Someone is repeatedly failing the password check on the operator web pages",
    ),
    (
        "app-log-ingestion-silent",
        "🖥️ HOST: the app has stopped writing any log lines",
    ),
    (
        "binary-sha-stale",
        "The running app is more than a day behind the latest approved code",
    ),
    (
        "boot-heartbeat-missing",
        "The app did not start on time this morning",
    ),
    ("clock-skew-high", "The server clock has drifted too far"),
    (
        "cpu-high-5min",
        "Server CPU has been very high for 5 minutes",
    ),
    ("mem-used-high", "Server memory is almost full"),
    (
        "instance-status-failed",
        "The cloud server is failing its health checks",
    ),
    (
        "system-status-failed",
        "The cloud hardware is failing its health checks",
    ),
    (
        "market-hours-liveness-missing",
        "🖥️ HOST: the app has gone silent during market hours",
    ),
    (
        "network-out-runaway",
        "Outbound network traffic is abnormally high",
    ),
    (
        "logs-ingestion-runaway",
        "Log volume is growing abnormally fast",
    ),
    (
        "eventbridge-dlq-depth",
        "Scheduled cloud tasks are failing and piling up",
    ),
    // ---- disk ----
    ("disk-used-high", "Server disk is almost full"),
    (
        "disk-fill-rate-high",
        "Disk space is being used up unusually fast",
    ),
    (
        "spill-dir-free-low",
        "Disk space is nearly gone — rescued prices will stop fitting soon",
    ),
    ("disk-watcher-respawn", "The disk monitor keeps restarting"),
    (
        "ebs-write-latency-high",
        "Disk writes have become very slow",
    ),
    (
        "partition-archive-failed",
        "Old data could not be moved to cold storage",
    ),
    // ---- database ----
    (
        "questdb-disconnected",
        "The database has been unreachable for too long",
    ),
    (
        "questdb-wal-suspended",
        "A database table has stopped storing rows while still accepting them",
    ),
    (
        "questdb-wal-apply-lag",
        "The database is falling behind on writing what it has already accepted",
    ),
    (
        "questdb-wal-probe-failed",
        "The check that watches for stuck database tables is itself failing",
    ),
    (
        "wal-suspension-probe-blind",
        "The check that watches for stuck database tables cannot see anything",
    ),
    (
        "wal-catchup-budget-exhausted",
        "The database ran out of time catching up on rows it had accepted",
    ),
    (
        "questdb-console-front-errors",
        "The database console page is failing",
    ),
    (
        "questdb-console-proxy-errors",
        "The database console link is failing",
    ),
    // ---- market data capture + loss ----
    (
        "market-data-persistence-loss",
        "Market prices were LOST — dropped without being rescued to disk",
    ),
    (
        "durable-floor-breach",
        "Incoming prices were lost before they could be safely stored",
    ),
    (
        "ticks-dropped",
        "Live prices were dropped before they could be saved",
    ),
    (
        "ticks-spilling",
        "Prices are being saved to disk instead of the database because the database is behind — nothing is lost, they are replayed",
    ),
    ("ticks-lost-spill", "Rescued prices were lost for good"),
    (
        "tick-spill-replay-failing",
        "Rescued prices could not be put back into the database",
    ),
    (
        "tick-spill-quarantined",
        "A rescue file of prices was set aside as unreadable",
    ),
    (
        "wal-frames-not-recovered",
        "Saved market data could not be recovered at start-up — that data is gone",
    ),
    (
        "tick-rescue-abandoned",
        "Prices marked as rescued may never have reached the disk file — treat this session's price count as unproven",
    ),
    (
        "depth-rescue-abandoned",
        "Order-book levels marked as rescued may never have reached the disk file — treat this session's depth count as unproven",
    ),
    (
        "wal-spill-shutdown-incomplete",
        "The app was cut off while still writing captured market data — those frames were never saved and cannot be recovered",
    ),
    (
        "wal-replay-restore-failed",
        "A saved market data file could not be put back for the next start-up — it is still on disk but no start-up will find it",
    ),
    (
        "wal-replay-unknown-magic",
        "A saved market data file is in a format this app cannot read — that data is gone",
    ),
    (
        "offload-writer-shutdown-incomplete",
        "The app shut down before it finished saving everything it was holding",
    ),
    // ---- Dhan live feed ----
    (
        "dhan-live-lane-down",
        "🔷 DHAN: the live market data feed is not running",
    ),
    (
        "dhan-no-ticks-flowing",
        "🔷 DHAN: connected, but no live prices are arriving",
    ),
    (
        "dhan-socket-parked",
        "🔷 DHAN: a market data connection has given up and will not try again",
    ),
    (
        "dhan-worst-socket-deaf",
        "🔷 DHAN: one market data connection looks alive but has stopped delivering prices",
    ),
    (
        "dhan-wal-dropped",
        "🔷 DHAN: live prices arrived but were never safely stored — that data is gone",
    ),
    (
        "dhan-contract-universe-failed",
        "🔷 DHAN: could not work out today's contract list — most instruments are unsubscribed",
    ),
    (
        "live-universe-fallback",
        "🔷 DHAN: fell back to a tiny instrument list — almost nothing is being watched",
    ),
    (
        "depth-steering-stalled",
        "🔷 DHAN: the order-book tracker has stopped following the market",
    ),
    (
        "preopen-ready-late",
        "🔷 DHAN: the feed was not fully connected and subscribed by 9:12 AM",
    ),
    (
        "ws-no-alive-connections",
        "🔷 DHAN: no live market data connections are up",
    ),
    (
        "ws-ring-full",
        "🔷 DHAN: the incoming price buffer is full — new prices are being refused",
    ),
    (
        "ws-ring-bytes-full",
        "🔷 DHAN: the incoming price buffer has hit its size limit — new prices are being refused",
    ),
    (
        "token-remaining-low",
        "🔷 DHAN: access token expires soon — spot-1m + option-chain pulls will stop",
    ),
    // ---- orders + risk ----
    ("orders-rejected", "Orders are being rejected"),
    (
        "orders-placed-storm",
        "Far more orders are being placed than expected",
    ),
    (
        "order-fill-lag-high",
        "Orders are taking too long to be filled",
    ),
    ("order-audit-chain-loss", "Order history rows were lost"),
    (
        "daily-loss-breach",
        "Today's trading loss has crossed the limit you set",
    ),
    // ---- helper Lambdas: is it erroring? ----
    (
        "budget-killswitch-errors",
        "The cost kill-switch helper is failing",
    ),
    (
        "hard-stop-guard-errors",
        "The cost stop-switch helper is failing",
    ),
    (
        "daily-budget-digest-errors",
        "The daily cost summary helper is failing",
    ),
    (
        "start-watchdog-errors",
        "The morning start helper is failing",
    ),
    (
        "deploy-watchdog-errors",
        "The stale-code checker is failing",
    ),
    (
        "market-open-readiness-errors",
        "The pre-open readiness check is failing",
    ),
    (
        "market-hours-gate-errors",
        "The market-hours alarm switch is failing",
    ),
    (
        "boot-heartbeat-gate-errors",
        "The morning start-up alarm switch is failing",
    ),
    (
        "dhan-token-minter-errors",
        "🔷 DHAN: the daily access-key helper is failing",
    ),
    (
        "operator-control-errors",
        "The operator control page is failing",
    ),
    (
        "telegram-webhook-errors",
        "The Telegram alert relay itself is failing",
    ),
    (
        "telegram-drops",
        "Some alerts could not be delivered to this chat",
    ),
    // ---- helper Lambdas: did it run at all? ----
    (
        "hard-stop-guard-not-invoked",
        "The cost stop-switch did not run — its schedule was dropped",
    ),
    (
        "daily-budget-digest-not-invoked",
        "The daily cost summary did not run — its schedule was dropped",
    ),
    (
        "start-watchdog-not-invoked",
        "The morning start helper did not run — its schedule was dropped",
    ),
    (
        "deploy-watchdog-not-invoked",
        "The stale-code checker did not run — its schedule was dropped",
    ),
    (
        "market-open-readiness-not-invoked",
        "The pre-open readiness check did not run — its schedule was dropped",
    ),
    (
        "market-hours-gate-not-invoked",
        "The market-hours alarm switch did not run — its schedule was dropped",
    ),
    (
        "boot-heartbeat-gate-not-invoked",
        "The morning start-up alarm switch did not run — its schedule was dropped",
    ),
    (
        "dhan-token-minter-not-invoked",
        "🔷 DHAN: the daily access-key helper did not run — its schedule was dropped",
    ),
    // ---- coded errors (the error!-to-phone route) ----
    (
        "errcode-dh-901",
        "🔷 DHAN: the broker sign-in check is failing — usually THEIR server, not our key. Read the status first: 401/403 means the key, 5xx means Dhan is down. Do not replace the key on a 5xx",
    ),
    (
        "errcode-dh-906",
        "🔷 DHAN: the broker refused an order — do not retry it, fix the order",
    ),
    (
        "errcode-auth-gap-04",
        "🔷 DHAN: the login code no longer matches — sign-in stays dead until it is fixed by hand",
    ),
    (
        "errcode-auth-gap-05-remint-failed",
        "🔷 DHAN: could not get a fresh access key after repeated tries",
    ),
    (
        "errcode-boot-02",
        "The app could not reach its database at start-up, so it did not start",
    ),
    (
        "errcode-boot-03",
        "The server clock has drifted too far for the app to start",
    ),
    (
        "errcode-chain-01",
        "🔷 DHAN: option-chain access is being refused — the chain pull is down",
    ),
    (
        "errcode-chain-02-escalation",
        "🔷 DHAN: the option-chain pull has been failing for several minutes",
    ),
    (
        "errcode-chain-04-warmup",
        "🔷 DHAN: the option chain could not start this morning and is down for the day",
    ),
    (
        "errcode-spot1m-01-escalation",
        "🔷 DHAN: the index price pull has been failing for several minutes",
    ),
    (
        "errcode-aggregator-drop-01",
        "Finished candles were dropped, or prices were refused by the candle builder",
    ),
    (
        "errcode-hot-path-02",
        "The database is behind, so prices are being held on disk and replayed — nothing is lost",
    ),
    (
        "errcode-oms-gap-06",
        "The paper order runtime stopped and had to be restarted",
    ),
    (
        "errcode-proc-01",
        "The app was killed by the server for using too much memory",
    ),
    (
        "errcode-resource-02",
        "The trading app's memory is close to its ceiling — it will be slowed, not killed, but check what is growing",
    ),
    (
        "errcode-risk-gap-03",
        "🔷 DHAN: some instruments have gone quiet or never sent a price today",
    ),
    (
        "errcode-stream-silent",
        "The error feed the alarms read has stopped arriving — the app is still reporting errors, so the other alarms are now blind until this is fixed",
    ),
    (
        "errcode-storage-gap-05",
        "Disk is above its high-water mark and nothing is left that may be archived away",
    ),
    (
        "errcode-wal-suspend-01",
        "The database is behind on applying writes — rows are accepted but not yet visible; if it does not catch up they stop being stored",
    ),
    (
        "errcode-ws-spill-01",
        "The safety capture for incoming market data hit a limit — a writer restarted, or replay stood down to protect memory. The server log line names which; on its own this does NOT mean anything was lost",
    ),
    (
        "errcode-ws-spill-02",
        "Incoming market data hit a capture limit — EITHER a frame was genuinely dropped, OR replay deferred segments to the next start, which loses nothing. The server log line names which — check it before assuming loss",
    ),
    (
        "errcode-ws-gap-02-swap-emptied-socket",
        "🔷 DHAN: an order-book connection was left carrying nothing after a contract swap",
    ),
    (
        "deploy-provenance-blind",
        "the check that tells you the box is running the latest code cannot run — it is not saying the code is fine, it is saying it does not know",
    ),
    (
        "errcode-scoreboard-01",
        "yesterday's per-connection summary could not be saved — the raw records are safe and it can be rebuilt, but the summary view is missing",
    ),
    (
        "errcode-ws-gap-03-universe-collapse",
        "🔷 DHAN: fell back to just four indices — almost the whole instrument list is missing",
    ),
    (
        "errcode-ws-gap-03-xverify-diverged",
        "🔷 DHAN: our recorded prices disagree badly with the broker's own record",
    ),
    (
        "errcode-ws-gap-03-xverify-failed",
        "🔷 DHAN: the end-of-day price cross-check could not run",
    ),
    (
        "errcode-ws-gap-03-xverify-vacuous",
        "🔷 DHAN: the end-of-day price cross-check compared nothing at all",
    ),
];

/// Cached SSM reads — Lambda containers stay warm for ~15 min. Re-fetch
/// only when the container is cold. Legacy parity: `_CACHED_TOKEN` /
/// `_CACHED_CHAT_ID` module globals.
static CACHED_TOKEN: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
static CACHED_CHAT_ID: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

/// Warm-container OK-suppression cache — legacy parity: `_LAST_SENT`.
static LAST_SENT: Mutex<Option<AlertCache>> = Mutex::new(None);

/// The fixed IST offset (+05:30). `east_opt` is statically in range; the
/// fallback arm is unreachable (kept to avoid `unwrap` per crate lints).
fn ist() -> FixedOffset {
    FixedOffset::east_opt(crate::time::IST_OFFSET_SECS).unwrap_or_else(|| Utc.fix())
}

/// Legacy `str(value or default)` over a JSON field: falsy (missing /
/// null / "" / false) → default; other non-strings stringify (ledger:
/// serde_json repr, not legacy repr — unreachable on real alarm JSON).
fn value_str_or(value: Option<&Value>, default: &str) -> String {
    match value {
        None | Some(Value::Null) => default.to_string(),
        Some(Value::String(s)) if s.is_empty() => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(false)) => default.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Map alarm severity / subject to a leading emoji per charter §D rule
/// 5+10. Legacy parity: `_severity_emoji` (the legacy `"deploy ok" in`
/// clause is a subset of the `"ok" in` clause — one contains-check here).
pub fn severity_emoji(subject: &str, alarm_state: Option<&str>) -> &'static str {
    let subject_lower = subject.to_lowercase();
    let state = alarm_state.unwrap_or("").to_uppercase();
    if subject_lower.contains("fail") || subject_lower.contains("critical") || state == "ALARM" {
        return "🆘";
    }
    if state == "INSUFFICIENT_DATA" {
        return "⚠️";
    }
    if state == "OK" {
        return "✅";
    }
    if subject_lower.contains("ok") {
        return "✅";
    }
    "🔔"
}

/// Parse the CloudWatch StateChangeTime shapes
/// ("2026-07-07T04:31:12.345+0000", "...Z", with/without fractional
/// seconds, tz-naive treated as UTC). Legacy parity: `fromisoformat`
/// with the `Z`→`+00:00` replace + the strptime fallbacks (the legacy runtime also
/// accepted date-only / space-separated ISO shapes CloudWatch never
/// emits; not reproduced — malformed input degrades identically).
fn parse_state_change_time(raw: &str) -> Option<DateTime<FixedOffset>> {
    if raw.is_empty() {
        return None;
    }
    let normalized = raw.replace('Z', "+00:00");
    // %.f matches an optional fractional-seconds group; %z matches both
    // "+0000" and "+00:00" — one format covers every CloudWatch shape.
    if let Ok(dt) = DateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f%z") {
        return Some(dt);
    }
    // tz-naive input — legacy `parsed.replace(tzinfo=timezone.utc)`.
    if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc().fixed_offset());
    }
    None
}

/// Render a CloudWatch StateChangeTime as an IST 12-hour clock string.
///
/// Legacy parity: `_ist_12h`. Malformed / missing input falls back to
/// the invocation time — the timestamp line degrades, never crashes
/// (fail-open: malformed / edge-dated inputs degrade to the invocation
/// time; chrono has no OverflowError class — see the module ledger).
pub fn ist_12h(state_change_time: &str) -> String {
    let raw = state_change_time.trim();
    let parsed = parse_state_change_time(raw).unwrap_or_else(|| Utc::now().with_timezone(&ist()));
    let ist_time = parsed.with_timezone(&ist());
    let hour24 = ist_time.hour();
    let hour = match hour24 % 12 {
        0 => 12,
        h => h,
    };
    let ampm = if hour24 < 12 { "AM" } else { "PM" };
    format!("{hour}:{:02} {ampm}", ist_time.minute())
}

/// Strip the `tv-<env>-` prefix off an alarm name so the phrase table is
/// environment-agnostic. Legacy parity: `_ENV_PREFIX_RE = ^tv-[a-z0-9]+-`
/// (leftmost-anchored substitution, implemented without a regex dep).
fn strip_env_prefix(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("tv-") {
        let run = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .count();
        if run > 0
            && let Some(tail) = rest[run..].strip_prefix('-')
        {
            return tail;
        }
    }
    name
}

/// Map an alarm name to plain English; humanize unknown names (fail-open).
/// Legacy parity: `_alarm_phrase`.
pub fn alarm_phrase(alarm_name: &str) -> String {
    let key = strip_env_prefix(alarm_name.trim());
    if let Some((_, phrase)) = ALARM_PHRASES.iter().find(|(k, _)| *k == key) {
        return (*phrase).to_string();
    }
    let spaced = key.replace(['-', '_'], " ");
    let words = spaced.trim();
    if words.is_empty() {
        return "A cloud alarm changed state".to_string();
    }
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => "A cloud alarm changed state".to_string(),
    }
}

/// Format one CloudWatch alarm object into the house-style Telegram text.
///
/// `{emoji} {plain-English line}` + newline + `{IST 12-hour} IST`.
/// The raw NewStateReason NEVER enters this string. Legacy parity:
/// `_house_line`.
pub fn house_line(alarm: &Value) -> String {
    let name = value_str_or(alarm.get("AlarmName"), "unknown-alarm");
    let state = value_str_or(alarm.get("NewStateValue"), "ALARM").to_uppercase();
    let phrase = alarm_phrase(&name);
    let when = value_str_or(alarm.get("StateChangeTime"), "");
    let ist_time = ist_12h(&when);
    if state == "OK" {
        return format!("✅ Recovered: {phrase} — {ist_time} IST");
    }
    let emoji = if state.is_empty() {
        "🔔"
    } else {
        severity_emoji("", Some(&state))
    };
    format!("{emoji} {phrase}\n{ist_time} IST")
}

/// ONE green line covering one or more recovered alarms.
/// Legacy parity: `_recovered_line`.
pub fn recovered_line(phrases: &[String], ist_time: &str) -> String {
    format!("✅ Recovered: {} — {ist_time} IST", phrases.join("; "))
}

/// Return the CloudWatch alarm object if `message` is an alarm JSON
/// string, else None. Legacy parity: `_parse_alarm`.
pub fn parse_alarm(message: &Value) -> Option<Value> {
    let Value::String(s) = message else {
        return None;
    };
    let parsed: Value = serde_json::from_str(s).ok()?;
    if parsed.is_object() && parsed.get("AlarmName").is_some() {
        return Some(parsed);
    }
    None
}

/// Character cap on the plain-SNS fallback body (Telegram's hard message
/// limit is 4096 chars; cap + marker stays well under it). The legacy runtime had no
/// cap — the fallback arm is already a divergence-documented fail-safe
/// (module ledger), and ordinary deploy/operator publishes are far
/// shorter, so byte-parity for them is unaffected.
pub const PLAIN_FALLBACK_BODY_MAX_CHARS: usize = 3500;

/// Blank the VALUE of every unescaped `"NewStateReason"` JSON field in
/// `text` (MED-1 fix): the plain-SNS fallback can receive a VALID alarm
/// JSON that serde_json refused on its 128-level recursion limit, and the
/// house contract is that `NewStateReason` forensic text NEVER reaches
/// the Telegram body — on ANY input, parse-failure fallbacks included.
///
/// Hand scanner, escape-aware: a string value is scanned with `\`
/// consuming the next byte, so `\"` inside the value never terminates
/// the redaction early. A non-string or unterminated value (never
/// emitted by real CloudWatch — crafted input only) is redacted to the
/// end of the text, fail-safe. Ordinary non-JSON messages contain no
/// `"NewStateReason"` key, so this is a byte-exact no-op for them
/// (legacy plain-fallback parity preserved; oracle-verified).
pub fn redact_new_state_reason(text: &str) -> String {
    const KEY: &str = "\"NewStateReason\"";
    if !text.contains(KEY) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(KEY) {
        let after_key = pos + KEY.len();
        out.push_str(&rest[..after_key]);
        rest = &rest[after_key..];
        let b = rest.as_bytes();
        let mut j = 0usize;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == b':' {
            j += 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            out.push_str(&rest[..j]);
            if j < b.len() && b[j] == b'"' {
                // JSON string value — blank its content, honoring escapes.
                let mut k = j + 1;
                let mut closed = false;
                while k < b.len() {
                    match b[k] {
                        b'\\' => k += 2, // escape consumes the next byte
                        b'"' => {
                            closed = true;
                            break;
                        }
                        _ => k += 1,
                    }
                }
                out.push_str("\"[redacted]");
                if closed {
                    out.push('"');
                    rest = &rest[k + 1..];
                } else {
                    // Unterminated string — redact to the end, fail-safe.
                    rest = "";
                }
            } else {
                // Non-string value (crafted input only): redact to the
                // end rather than attempt balanced-JSON skipping.
                out.push_str("[redacted]");
                rest = "";
            }
        }
        // No colon after the key: a bare occurrence (e.g. the literal
        // key name as a string VALUE) — nothing to redact; the loop
        // continues scanning the remainder.
    }
    out.push_str(rest);
    out
}

/// Cap the fallback body at [`PLAIN_FALLBACK_BODY_MAX_CHARS`] chars
/// (char-boundary safe). Ordinary publishes never reach the cap.
fn cap_fallback_body(body: String) -> String {
    match body.char_indices().nth(PLAIN_FALLBACK_BODY_MAX_CHARS) {
        None => body,
        Some((cut, _)) => format!("{}…[truncated]", &body[..cut]),
    }
}

/// Format a non-CloudWatch SNS publish (e.g., from the deploy-aws
/// workflow). Legacy parity: `_format_plain_sns` — byte-identical for
/// ordinary messages; diverges ONLY when the text carries a
/// `"NewStateReason"` field (redacted) or exceeds the fallback cap
/// (truncated) — both fail-safe, module-ledger documented.
pub fn format_plain_sns(subject: Option<&Value>, message: &Value) -> String {
    let subject_s: Option<String> = match subject {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    };
    let emoji = severity_emoji(subject_s.as_deref().unwrap_or(""), None);
    let raw_body = match message {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let body = cap_fallback_body(redact_new_state_reason(&raw_body));
    match subject_s {
        Some(s) => {
            let s = redact_new_state_reason(&s);
            format!("{emoji} {s}\n{body}")
        }
        None => format!("{emoji} {body}"),
    }
}

/// True when an OK for `name` repeats a recent OK (warm-container dedupe).
///
/// ONLY ever called for OK-state records — ALARM records are never
/// routed through this cache (never-drop law). Legacy parity:
/// `_should_suppress_ok`.
pub fn should_suppress_ok(name: &str, now_epoch: f64, cache: &AlertCache) -> bool {
    let Some((last_state, last_epoch, _)) = cache.get(name) else {
        return false;
    };
    last_state == "OK" && (now_epoch - last_epoch) < OK_REPEAT_SUPPRESS_SECS
}

/// How many consecutive ALARM pages the current episode has produced for
/// `name`, counting the one about to be sent. `1` means "first page of a new
/// episode" — render it in full.
///
/// The match arm is the whole safety property of this feature, so read it
/// literally: the cache is keyed by the EXACT alarm name, and the arm fires
/// only when THAT key's cached state is also `ALARM` and the previous page for
/// THAT key landed inside the window. A different alarm has a different key and
/// therefore always returns 1 — it can never be folded into a neighbour's
/// streak, which matters because several alarms in this account are the ONLY
/// signal their condition will ever produce.
///
/// Never returns 0 and never suppresses: the caller sends on every value.
pub fn alarm_repeat_streak(name: &str, now_epoch: f64, cache: &AlertCache) -> u32 {
    match cache.get(name) {
        Some((last_state, last_epoch, streak))
            if last_state == "ALARM" && (now_epoch - last_epoch) < ALARM_REPEAT_COALESCE_SECS =>
        {
            streak.saturating_add(1)
        }
        _ => 1,
    }
}

/// English ordinal for a page number — "2nd", "3rd", "4th", "11th", "21st".
/// Plain English is a Telegram commandment; "page #4" reads like a machine.
fn ordinal(n: u32) -> String {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The SHORT render for a repeat ALARM inside the coalescing window.
///
/// One line, emoji first (commandment 5), the same plain-English phrase the
/// full page used, and an explicit count so the operator can see at a glance
/// that this is an ongoing episode rather than a new emergency. The alarm is
/// still DELIVERED — only its length changes.
pub fn repeat_alarm_line(alarm: &Value, streak: u32) -> String {
    let name = value_str_or(alarm.get("AlarmName"), "unknown-alarm");
    let state = value_str_or(alarm.get("NewStateValue"), "ALARM").to_uppercase();
    let phrase = alarm_phrase(&name);
    let ist_time = ist_12h(&value_str_or(alarm.get("StateChangeTime"), ""));
    let emoji = severity_emoji("", Some(&state));
    let window_mins = (ALARM_REPEAT_COALESCE_SECS / 60.0).round() as u64;
    format!(
        "{emoji} Still down: {phrase} ({} page in {window_mins} min) — {ist_time} IST",
        ordinal(streak)
    )
}

/// Fold one SNS batch into the final list of Telegram texts.
///
/// Rules (judge final contract, Module 5) — legacy parity: `_fold_records`:
/// - ALARM records stay INDIVIDUAL house lines — never digested,
///   never suppressed (a later ALARM for the same alarm supersedes an
///   earlier one in the same batch: still exactly one 🆘 delivered).
/// - An ALARM followed by OK for the same alarm inside this batch
///   folds to ONLY the recovered line. An OK followed by a re-ALARM
///   keeps the ALARM — the final state per alarm decides, so a 🆘
///   can never be dropped by an older ✅.
/// - All lone-OK records fold into ONE recovered line.
/// - Repeat OK within OK_REPEAT_SUPPRESS_SECS of a sent OK for the
///   same alarm is suppressed (warm cache); ALARM is never consulted.
/// - A malformed record folds to a safe generic line — never a crash,
///   never raw JSON.
pub fn fold_records(records: &[Value], now_epoch: f64, cache: &mut AlertCache) -> Vec<String> {
    let now = now_epoch;
    let mut plain_texts: Vec<String> = Vec::new();
    // Per alarm name (first-appearance order): the LAST record wins,
    // remembering whether an ALARM was seen anywhere in the batch.
    let mut name_order: Vec<String> = Vec::new();
    let mut last_by_name: HashMap<String, Value> = HashMap::new();
    let mut saw_alarm: HashMap<String, bool> = HashMap::new();

    let empty_message = Value::String(String::new());
    for record in records {
        // Legacy parity: a truthy non-dict record / Sns value raised
        // AttributeError → caught → generic safe line (fail-open).
        let sns_obj = match record {
            Value::Null => None,
            Value::Object(map) => match map.get("Sns") {
                None | Some(Value::Null) => None,
                Some(Value::Object(sns)) => Some(sns),
                Some(_) => {
                    warn!("Malformed SNS record — sending safe generic line");
                    plain_texts.push(GENERIC_SAFE_LINE.to_string());
                    continue;
                }
            },
            _ => {
                warn!("Malformed SNS record — sending safe generic line");
                plain_texts.push(GENERIC_SAFE_LINE.to_string());
                continue;
            }
        };
        let message = sns_obj
            .and_then(|s| s.get("Message"))
            .unwrap_or(&empty_message);
        if let Some(alarm) = parse_alarm(message) {
            // Forensics stay in CloudWatch Logs — NEVER in Telegram text.
            let forensics_reason = alarm.get("NewStateReason");
            info!(
                name = ?alarm.get("AlarmName"),
                state = ?alarm.get("NewStateValue"),
                reason = ?forensics_reason,
                "alarm-forensics"
            );
            let name = value_str_or(alarm.get("AlarmName"), "unknown-alarm");
            let state = value_str_or(alarm.get("NewStateValue"), "ALARM").to_uppercase();
            if !last_by_name.contains_key(&name) {
                name_order.push(name.clone());
            }
            let seen = saw_alarm.get(&name).copied().unwrap_or(false);
            saw_alarm.insert(name.clone(), seen || state == "ALARM");
            last_by_name.insert(name, alarm);
        } else {
            let subject = sns_obj.and_then(|s| s.get("Subject"));
            plain_texts.push(format_plain_sns(subject, message));
        }
    }

    let mut out: Vec<String> = Vec::new();
    let mut lone_ok_phrases: Vec<String> = Vec::new();
    let mut lone_ok_ist: Option<String> = None;

    // (the legacy runtime wrapped this loop in a per-record fail-open `except` —
    //  the Rust render chain has no panicking path; ledger.)
    for name in &name_order {
        let Some(alarm) = last_by_name.get(name) else {
            continue;
        };
        let final_state = value_str_or(alarm.get("NewStateValue"), "ALARM").to_uppercase();

        if final_state == "OK" && saw_alarm.get(name).copied().unwrap_or(false) {
            // ALARM→OK inside one batch: ONLY the recovered line.
            out.push(house_line(alarm));
            cache.insert(name.clone(), ("OK".to_string(), now, 0));
            continue;
        }

        if final_state == "OK" {
            // Lone OK flip — warm-cache dedupe, then fold into ONE line.
            if should_suppress_ok(name, now, cache) {
                info!("ok-repeat-suppressed name={name}");
                continue;
            }
            lone_ok_phrases.push(alarm_phrase(name));
            let when = value_str_or(alarm.get("StateChangeTime"), "");
            lone_ok_ist = Some(ist_12h(&when));
            cache.insert(name.clone(), ("OK".to_string(), now, 0));
            continue;
        }

        // ALARM / INSUFFICIENT_DATA final state — individual house line,
        // NEVER suppressed, NEVER folded away by an earlier OK.
        //
        // COALESCING (2026-09-01): a repeat ALARM for the SAME name in the
        // SAME state, inside ALARM_REPEAT_COALESCE_SECS, is still SENT but
        // rendered as one short "still down" line carrying its page number.
        // The streak is scoped to ALARM only — an INSUFFICIENT_DATA render is
        // a different state and always starts a fresh, full page, so a
        // flapping-to-unknown alarm can never quietly ride an ALARM streak.
        let streak = if final_state == "ALARM" {
            alarm_repeat_streak(name, now, cache)
        } else {
            1
        };
        if streak > 1 {
            info!("alarm-repeat-coalesced name={name} page={streak}");
            out.push(repeat_alarm_line(alarm, streak));
        } else {
            out.push(house_line(alarm));
        }
        cache.insert(name.clone(), (final_state, now, streak));
    }

    if !lone_ok_phrases.is_empty() {
        let ist_time = lone_ok_ist.unwrap_or_else(|| ist_12h(""));
        out.push(recovered_line(&lone_ok_phrases, &ist_time));
    }

    out.extend(plain_texts);
    out
}

/// The exact Telegram sendMessage form pairs. Plain-text mode is
/// load-bearing: there is NO markup-parsing field in this payload, so an
/// alarm name containing '*', '`' or '[' can never cause a silent
/// Markdown-parse 400 drop (the `test_post_payload_has_no_parse_mode`
/// ratchet asserts over these keys — the Rust analog of the legacy runtime's
/// `inspect.getsource` scan).
pub fn telegram_form_pairs(chat_id: &str, text: &str) -> [(&'static str, String); 3] {
    [
        ("chat_id", chat_id.to_string()),
        ("text", text.to_string()),
        ("disable_web_page_preview", "true".to_string()),
    ]
}

/// application/x-www-form-urlencoded encoding — legacy parity:
/// `urllib.parse.urlencode` (quote_plus: space → '+', unreserved
/// `A-Za-z0-9_.-~` pass through, everything else %XX per UTF-8 byte).
pub fn form_urlencode(pairs: &[(&str, String)]) -> String {
    fn encode_into(out: &mut String, s: &str) {
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                    out.push(byte as char);
                }
                b' ' => out.push('+'),
                _ => {
                    out.push('%');
                    out.push_str(&format!("{byte:02X}"));
                }
            }
        }
    }
    let mut out = String::new();
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        encode_into(&mut out, key);
        out.push('=');
        encode_into(&mut out, value);
    }
    out
}

/// POST to the Telegram bot API. Returns (status_code, body_text).
/// Legacy parity: `_post_to_telegram`. UNPROVEN until deploy — the live
/// HTTP leg runs only in a real Lambda; the payload construction it
/// sends is what the unit tests cover. The token/chat-id are NEVER
/// logged.
pub async fn post_to_telegram(
    client: &reqwest::Client,
    token: &str,
    chat_id: &str,
    text: &str,
) -> Result<(u16, String), reqwest::Error> {
    let url = format!("{TELEGRAM_API_BASE}/bot{token}/sendMessage");
    let body = form_urlencode(
        &telegram_form_pairs(chat_id, text)
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect::<Vec<_>>(),
    );
    let resp = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    let status = resp.status().as_u16();
    // legacy: resp.read().decode("utf-8", errors="replace") — reqwest
    // text() is already lossy on invalid UTF-8; a transport error while
    // reading propagates like the legacy exception path.
    let body_text = resp.text().await?;
    Ok((status, body_text))
}

/// Send every folded text into the Telegram POST — the single delivery
/// choke point (no filtering between the fold and the POST). Legacy
/// parity: the `for text in texts:` loop of `lambda_handler`. `post` is
/// the injected transport so tests exercise the full never-drop boundary
/// without HTTP (the Rust analog of the legacy runtime's `mock.patch`).
pub async fn send_texts<F, Fut>(texts: &[String], records_len: usize, mut post: F) -> Value
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<(u16, String), String>>,
{
    let mut sent: u64 = 0;
    let mut failures: Vec<String> = Vec::new();
    for text in texts {
        match post(text.clone()).await {
            Ok((status, body)) => {
                if status >= 400 {
                    let head: String = body.chars().take(200).collect();
                    failures.push(format!("http {status}: {head}"));
                    error!(code = "LAMBDA-NOTIFY-01", status, body = %head, "Telegram POST returned an error status");
                } else {
                    sent += 1;
                }
            }
            Err(err) => {
                failures.push(err.clone());
                error!(code = "LAMBDA-NOTIFY-01", error = %err, "Failed to relay one message to Telegram");
            }
        }
        // Cheap rate-limit cushion. Telegram allows ~30 msg/sec per bot;
        // if 5+ alarms fire in the same SNS batch we don't want to flirt
        // with their throttle. Legacy parity: `time.sleep(0.05)`.
        tokio::time::sleep(Duration::from_millis(50)).await; // APPROVED: legacy-parity throttle cushion (time.sleep(0.05)) — cold Lambda path, not the tick hot path
    }
    json!({"sent": sent, "failures": failures, "records": records_len})
}

/// Fold + send composed — the testable end-to-end delivery seam the
/// LambdaHandlerDelivery tests drive (the legacy runtime patched `lambda_handler`'s
/// collaborators; the Rust seam injects the cache + transport instead).
pub async fn deliver<F, Fut>(
    records: &[Value],
    now_epoch: f64,
    cache: &mut AlertCache,
    post: F,
) -> Value
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<(u16, String), String>>,
{
    let texts = fold_records(records, now_epoch, cache);
    send_texts(&texts, records.len(), post).await
}

async fn fetch_ssm_secret(parameter_name: &str) -> Result<String, Error> {
    let config = crate::clients::sdk_config().await;
    let ssm = crate::clients::ssm(&config);
    let resp = ssm
        .get_parameter()
        .name(parameter_name)
        .with_decryption(true)
        .send()
        .await?;
    resp.parameter()
        .and_then(|p| p.value())
        .map(str::to_string)
        .ok_or_else(|| Error::from("SSM parameter has no value"))
}

/// Return (bot_token, chat_id), caching across warm invocations.
/// Legacy parity: `_get_credentials`. Secret VALUES are never logged.
async fn get_credentials() -> Result<(String, String), Error> {
    let token_param = std::env::var("TELEGRAM_BOT_TOKEN_SSM_PARAM")
        .unwrap_or_else(|_| "/tickvault/prod/telegram/bot-token".to_string());
    let chat_param = std::env::var("TELEGRAM_CHAT_ID_SSM_PARAM")
        .unwrap_or_else(|_| "/tickvault/prod/telegram/chat-id".to_string());
    let token = CACHED_TOKEN
        .get_or_try_init(|| fetch_ssm_secret(&token_param))
        .await?
        .clone();
    let chat_id = CACHED_CHAT_ID
        .get_or_try_init(|| fetch_ssm_secret(&chat_param))
        .await?
        .clone();
    Ok((token, chat_id))
}

/// SNS-triggered entry point — legacy parity: `lambda_handler`.
///
/// UNPROVEN until deploy: the live SSM + Telegram HTTP legs run only in
/// a real Lambda. Credential errors are propagated (`?`) so SNS marks
/// the delivery failed and retries per its default policy — the legacy
/// re-raise semantics (without retry the alert is lost forever).
pub async fn handle(event: Value) -> Result<Value, Error> {
    let records: Vec<Value> = event
        .get("Records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if records.is_empty() {
        warn!("No SNS Records in event — skipping");
        return Ok(json!({"sent": 0, "skipped": 1}));
    }

    let (token, chat_id) = match get_credentials().await {
        Ok(pair) => pair,
        Err(err) => {
            error!(
                code = "LAMBDA-NOTIFY-01",
                "Failed to fetch Telegram credentials from SSM"
            );
            return Err(err);
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(TELEGRAM_TIMEOUT_SECONDS))
        .build()?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    // The warm-cache lock is scoped to the synchronous fold — never held
    // across an await (the send loop below is lock-free).
    let texts = {
        let mut guard = LAST_SENT.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.get_or_insert_with(HashMap::new);
        fold_records(&records, now, cache)
    };
    // (legacy backstopped a fold crash with one generic line per record;
    //  the Rust fold has no panicking path — ledger.)

    let result = send_texts(&texts, records.len(), |text| {
        let client = client.clone();
        let token = token.clone();
        let chat_id = chat_id.clone();
        async move {
            post_to_telegram(&client, &token, &chat_id, &text)
                .await
                // Token-redaction hardening: the reqwest error Display
                // embeds the request URL, which contains the bot token —
                // strip it before the string enters logs / the result.
                .map_err(|e| e.without_url().to_string())
        }
    })
    .await;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ist_12h_re() -> regex_lite::Re {
        regex_lite::Re
    }

    // Minimal stand-in matcher (no regex dep): `\b\d{1,2}:\d{2} (AM|PM) IST\b`.
    mod regex_lite {
        pub struct Re;
        impl Re {
            pub fn is_match(&self, s: &str) -> bool {
                // Find "H:MM AM IST" / "HH:MM PM IST" anywhere in s.
                for (idx, _) in s.match_indices(" IST") {
                    let head = &s[..idx];
                    if Self::ends_with_clock(head) {
                        return true;
                    }
                }
                false
            }
            pub fn matches_clock_only(s: &str) -> bool {
                // `^\d{1,2}:\d{2} (AM|PM)$`
                Self::ends_with_clock(s)
                    && s.chars().next().is_some_and(|c| c.is_ascii_digit())
                    && {
                        let colon = s.find(':').unwrap_or(0);
                        (1..=2).contains(&colon)
                    }
            }
            fn ends_with_clock(head: &str) -> bool {
                let Some(rest) = head
                    .strip_suffix(" AM")
                    .or_else(|| head.strip_suffix(" PM"))
                else {
                    return false;
                };
                // rest must end with \d{1,2}:\d{2}
                let bytes = rest.as_bytes();
                let n = bytes.len();
                if n < 4 {
                    return false;
                }
                let mm = &bytes[n - 2..];
                if !mm.iter().all(u8::is_ascii_digit) {
                    return false;
                }
                if bytes[n - 3] != b':' {
                    return false;
                }
                let mut i = n - 3;
                let mut digits = 0;
                while i > 0 && digits < 2 && bytes[i - 1].is_ascii_digit() {
                    i -= 1;
                    digits += 1;
                }
                digits >= 1
            }
        }
    }

    fn alarm_record(name: &str, state: &str, when: &str) -> Value {
        json!({
            "Sns": {
                "Subject": format!("{state}: {name}"),
                "Message": json!({
                    "AlarmName": name,
                    "NewStateValue": state,
                    "NewStateReason":
                        "Threshold Crossed: 1 datapoint [1.0] was greater than the threshold (0.0).",
                    "StateChangeTime": when,
                    "Region": "ap-south-1",
                })
                .to_string(),
            }
        })
    }

    fn alarm(name: &str, state: &str) -> Value {
        alarm_record(name, state, "2026-07-07T04:31:12.345+0000")
    }

    // ---- HouseLine (legacy: 6 tests) ----

    #[test]
    fn test_house_line_no_raw_threshold_json() {
        let a = json!({
            "AlarmName": "tv-prod-questdb-wal-suspended",
            "NewStateValue": "ALARM",
            "NewStateReason": "Threshold Crossed: 1 out of the last 1 datapoints \
                 [0.0 (07/07/26 04:26:00)] was less than or equal to the \
                 threshold (0.0) (minimum 1 datapoint for OK -> ALARM transition).",
            "StateChangeTime": "2026-07-07T04:31:12.345+0000",
        });
        let out = house_line(&a);
        assert!(!out.contains("Threshold Crossed"));
        assert!(!out.contains("Reason:"));
        assert!(!out.contains("datapoint"));
        assert!(!out.contains('{'));
        assert!(out.starts_with("🆘 "));
        assert!(out.contains("stopped storing rows while still accepting them"));
        assert!(ist_12h_re().is_match(&out));
    }

    #[test]
    fn test_alarm_state_two_lines_emoji_first() {
        let out = house_line(&json!({
            "AlarmName": "tv-prod-cpu-high-5min",
            "NewStateValue": "ALARM",
            "StateChangeTime": "2026-07-07T04:31:12.345+0000",
        }));
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "🆘 Server CPU has been very high for 5 minutes");
        assert_eq!(lines[1], "10:01 AM IST");
    }

    #[test]
    fn test_ok_state_single_recovered_line() {
        let out = house_line(&json!({
            "AlarmName": "tv-prod-cpu-high-5min",
            "NewStateValue": "OK",
            "StateChangeTime": "2026-07-07T04:31:12.345+0000",
        }));
        assert!(!out.contains('\n'));
        assert!(out.starts_with("✅ Recovered: "));
        assert!(ist_12h_re().is_match(&out));
    }

    #[test]
    fn test_unknown_alarm_name_fallback_still_plain_english() {
        let out = house_line(&json!({
            "AlarmName": "tv-prod-some-brand-new-alarm",
            "NewStateValue": "ALARM",
            "NewStateReason": "Threshold Crossed: blah",
            "StateChangeTime": "2026-07-07T04:31:12.345+0000",
        }));
        assert!(out.starts_with("🆘 Some brand new alarm"));
        assert!(!out.contains("Threshold Crossed"));
        assert!(!out.contains("tv-prod-"));
    }

    #[test]
    fn test_missing_fields_never_crash() {
        let out = house_line(&json!({}));
        assert!(out.starts_with("🆘 "));
        assert!(ist_12h_re().is_match(&out));
    }

    #[test]
    fn test_insufficient_data_is_warning_emoji() {
        let out = house_line(&json!({
            "AlarmName": "tv-prod-mem-used-high",
            "NewStateValue": "INSUFFICIENT_DATA",
            "StateChangeTime": "2026-07-07T04:31:12.345+0000",
        }));
        assert!(out.starts_with("⚠️ "));
    }

    // ---- Ist12Hour (legacy: 5 tests) ----

    #[test]
    fn test_ist_12_hour_timestamp() {
        // 04:31 UTC + 05:30 = 10:01 AM IST
        assert_eq!(ist_12h("2026-07-07T04:31:12.345+0000"), "10:01 AM");
    }

    #[test]
    fn test_pm_and_z_suffix() {
        // 10:00 UTC + 05:30 = 3:30 PM IST
        assert_eq!(ist_12h("2026-07-07T10:00:00Z"), "3:30 PM");
    }

    #[test]
    fn test_midnight_boundary_is_12_am() {
        // 18:30 UTC = 00:00 IST
        assert_eq!(ist_12h("2026-07-07T18:30:00+0000"), "12:00 AM");
    }

    #[test]
    fn test_malformed_input_falls_back_without_crash() {
        let out = ist_12h("not-a-timestamp");
        assert!(tests_clock_only(&out), "got {out:?}");
        let out2 = ist_12h("");
        assert!(tests_clock_only(&out2), "got {out2:?}");
    }

    fn tests_clock_only(s: &str) -> bool {
        regex_lite::Re::matches_clock_only(s)
    }

    #[test]
    fn test_edge_dated_timestamps_fall_back_without_crash() {
        // Legacy regression (2026-07-07 refute round 1): year-0001/9999
        // inputs OverflowError'd on the IST conversion. chrono converts
        // them without overflow (module ledger) — the contract is the
        // clock FORMAT, which must hold either way.
        for raw in ["9999-12-31T23:59:59+00:00", "0001-01-01T00:00:00+05:31"] {
            let out = ist_12h(raw);
            assert!(tests_clock_only(&out), "input {raw:?} gave {out:?}");
        }
    }

    // ---- AlarmPhrase (legacy: 3 tests) ----

    #[test]
    fn test_known_alarm_maps_to_plain_english() {
        assert_eq!(
            alarm_phrase("tv-prod-questdb-disconnected"),
            "The database has been unreachable for too long"
        );
    }

    #[test]
    fn test_env_prefix_is_stripped_for_any_environment() {
        assert_eq!(
            alarm_phrase("tv-staging-cpu-high-5min"),
            alarm_phrase("tv-prod-cpu-high-5min")
        );
    }

    #[test]
    fn test_empty_name_falls_back_to_generic() {
        assert_eq!(alarm_phrase(""), "A cloud alarm changed state");
    }

    // ---- FoldRecords (legacy: 9 tests) ----

    #[test]
    fn test_ok_flip_single_line_recovered() {
        let mut cache = HashMap::new();
        let texts = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "OK")],
            1_000_000.0,
            &mut cache,
        );
        assert_eq!(texts.len(), 1);
        assert!(!texts[0].contains('\n'));
        assert!(texts[0].starts_with("✅ Recovered: "));
        assert!(texts[0].contains("Server CPU has been very high for 5 minutes"));
        assert!(ist_12h_re().is_match(&texts[0]));
    }

    #[test]
    fn test_alarm_ok_pair_in_batch_folds_to_recovered_only() {
        let mut cache = HashMap::new();
        let texts = fold_records(
            &[
                alarm("tv-prod-cpu-high-5min", "ALARM"),
                alarm("tv-prod-cpu-high-5min", "OK"),
            ],
            1_000_000.0,
            &mut cache,
        );
        assert_eq!(texts.len(), 1);
        assert!(texts[0].starts_with('✅'));
        assert!(!texts[0].contains("🆘"));
    }

    #[test]
    fn test_ok_then_re_alarm_keeps_the_alarm() {
        // A 🆘 must NEVER be dropped by an older ✅ in the same batch.
        let mut cache = HashMap::new();
        let texts = fold_records(
            &[
                alarm("tv-prod-cpu-high-5min", "OK"),
                alarm("tv-prod-cpu-high-5min", "ALARM"),
            ],
            1_000_000.0,
            &mut cache,
        );
        assert_eq!(texts.len(), 1);
        assert!(texts[0].starts_with("🆘"));
    }

    #[test]
    fn test_multiple_lone_oks_fold_into_one_recovered_line() {
        let mut cache = HashMap::new();
        let texts = fold_records(
            &[
                alarm("tv-prod-cpu-high-5min", "OK"),
                alarm("tv-prod-mem-used-high", "OK"),
            ],
            1_000_000.0,
            &mut cache,
        );
        assert_eq!(texts.len(), 1);
        assert!(texts[0].starts_with("✅ Recovered: "));
        assert!(texts[0].contains("Server CPU has been very high for 5 minutes"));
        assert!(texts[0].contains("Server memory is almost full"));
    }

    #[test]
    fn test_alarms_stay_individual_never_digested() {
        let mut cache = HashMap::new();
        let texts = fold_records(
            &[
                alarm("tv-prod-cpu-high-5min", "ALARM"),
                alarm("tv-prod-questdb-disconnected", "ALARM"),
            ],
            1_000_000.0,
            &mut cache,
        );
        assert_eq!(texts.len(), 2);
        for text in &texts {
            assert!(text.starts_with("🆘"));
        }
    }

    #[test]
    fn test_alarm_never_suppressed_by_warm_cache() {
        let mut cache = HashMap::from([
            (
                "tv-prod-cpu-high-5min".to_string(),
                ("ALARM".to_string(), 999_999.0, 1),
            ),
            (
                "tv-prod-questdb-disconnected".to_string(),
                ("OK".to_string(), 999_999.0, 0),
            ),
        ]);
        let texts = fold_records(
            &[
                alarm("tv-prod-cpu-high-5min", "ALARM"),
                alarm("tv-prod-questdb-disconnected", "ALARM"),
            ],
            1_000_000.0,
            &mut cache,
        );
        assert_eq!(texts.len(), 2);
        for text in &texts {
            assert!(text.starts_with("🆘"));
        }
    }

    #[test]
    fn test_repeat_ok_within_window_is_suppressed() {
        let mut cache = HashMap::new();
        let first = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "OK")],
            1_000_000.0,
            &mut cache,
        );
        assert_eq!(first.len(), 1);
        let repeat = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "OK")],
            1_000_000.0 + 30.0,
            &mut cache,
        );
        assert!(repeat.is_empty());
        // Past the window the OK flows again.
        let later = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "OK")],
            1_000_000.0 + OK_REPEAT_SUPPRESS_SECS + 1.0,
            &mut cache,
        );
        assert_eq!(later.len(), 1);
    }

    #[test]
    fn test_malformed_sns_record_fails_open_to_generic_line() {
        let mut cache = HashMap::new();
        let texts = fold_records(
            &[
                Value::Null,
                json!({"Sns": null}),
                json!({"Sns": {"Message": {"weird": "shape"}}}),
            ],
            1_000_000.0,
            &mut cache,
        );
        assert!(!texts.is_empty());
        for text in &texts {
            assert!(!text.contains("Threshold Crossed"));
            assert!(!text.contains("NewStateReason"));
        }
    }

    #[test]
    fn test_no_raw_reason_json_in_any_folded_text() {
        let mut cache = HashMap::new();
        let texts = fold_records(
            &[
                alarm("tv-prod-cpu-high-5min", "ALARM"),
                alarm("tv-prod-mem-used-high", "OK"),
                json!({"Sns": {"Subject": "DLT deploy OK", "Message": "commit=abc ref=main"}}),
            ],
            1_000_000.0,
            &mut cache,
        );
        for text in &texts {
            assert!(!text.contains("Threshold Crossed"));
            assert!(!text.contains("Reason:"));
            assert!(!text.contains("NewStateReason"));
        }
    }

    // ---- FormatPlainSns (legacy: 3 tests) ----

    #[test]
    fn test_deploy_ok_subject_uses_check_emoji() {
        let out = format_plain_sns(Some(&json!("DLT deploy OK")), &json!("commit=abc ref=main"));
        assert!(out.starts_with("✅ DLT deploy OK"));
        assert!(out.contains("commit=abc"));
    }

    #[test]
    fn test_deploy_failed_subject_uses_emergency_emoji() {
        let out = format_plain_sns(
            Some(&json!("DLT deploy FAILED")),
            &json!("commit=abc run=999"),
        );
        assert!(out.starts_with("🆘 DLT deploy FAILED"));
    }

    #[test]
    fn test_no_subject_falls_back_to_bell() {
        let out = format_plain_sns(None, &json!("operator-test"));
        assert!(out.starts_with("🔔"));
    }

    // ---- DeepNestFallbackRedaction (MED-1 fix round, rust-only) ----
    //
    // A VALID alarm JSON with a >128-deep irrelevant nested field trips
    // serde_json's recursion limit → `parse_alarm` returns None → the
    // plain-SNS fallback fires. The contract: NewStateReason forensic
    // text NEVER reaches the Telegram body, on ANY input.

    fn deep_alarm_json(depth: usize) -> String {
        let deep = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        format!(
            "{{\"AlarmName\":\"tv-prod-feed-stall\",\
             \"NewStateValue\":\"ALARM\",\
             \"NewStateReason\":\"Threshold Crossed: 1 datapoint SECRETFORENSIC\",\
             \"StateChangeTime\":\"2026-07-18T10:00:00.000+0000\",\
             \"Extra\":{deep}}}"
        )
    }

    #[test]
    fn test_deep_nest_200_valid_alarm_never_leaks_new_state_reason() {
        // legacy oracle (recovered handler.py, 2026-07-18): depth-200
        // PARSES in the legacy runtime → house line '🆘 Feed stall\n3:30 PM IST'.
        // Rust diverges (ledger): redacted, capped plain fallback.
        let msg = deep_alarm_json(200);
        let mut cache = HashMap::new();
        let texts = fold_records(&[json!({"Sns": {"Message": msg}})], 0.0, &mut cache);
        assert_eq!(texts.len(), 1);
        assert!(!texts[0].contains("SECRETFORENSIC"));
        assert!(!texts[0].contains("Threshold Crossed"));
        assert!(texts[0].contains("[redacted]"));
    }

    #[test]
    fn test_deep_nest_2000_valid_alarm_never_leaks_and_never_panics() {
        // legacy oracle: depth-2000 raised RecursionError in json.loads →
        // fail-open generic safe line. Rust: fast serde_json refusal at
        // depth 128 → redacted + capped plain fallback. No panic, no
        // stack overflow, no forensic content either way.
        let msg = deep_alarm_json(2000);
        let mut cache = HashMap::new();
        let texts = fold_records(&[json!({"Sns": {"Message": msg}})], 0.0, &mut cache);
        assert_eq!(texts.len(), 1);
        assert!(!texts[0].contains("SECRETFORENSIC"));
        assert!(!texts[0].contains("Threshold Crossed"));
        assert!(texts[0].contains("[redacted]"));
        // The 4000+ bracket tail is capped under Telegram's 4096 limit.
        assert!(texts[0].chars().count() <= PLAIN_FALLBACK_BODY_MAX_CHARS + 64);
        assert!(texts[0].ends_with("…[truncated]"));
    }

    #[test]
    fn test_plain_fallback_ordinary_messages_byte_parity_with_legacy_oracle() {
        // Redaction + cap MUST be byte-exact no-ops on ordinary non-JSON
        // publishes. Oracle: the legacy runtime on the recovered handler.py
        // (git show 3a44ffd^:deploy/aws/lambda/telegram-webhook/handler.py),
        // run 2026-07-18:
        //   _format_plain_sns("DLT deploy OK", "commit=abc ref=main")
        //     == '✅ DLT deploy OK\ncommit=abc ref=main'
        //   _format_plain_sns(None, "operator-test") == '🔔 operator-test'
        //   _format_plain_sns("Deploy FAILED", "step=build exit=101")
        //     == '🆘 Deploy FAILED\nstep=build exit=101'
        assert_eq!(
            format_plain_sns(Some(&json!("DLT deploy OK")), &json!("commit=abc ref=main")),
            "✅ DLT deploy OK\ncommit=abc ref=main"
        );
        assert_eq!(
            format_plain_sns(None, &json!("operator-test")),
            "🔔 operator-test"
        );
        assert_eq!(
            format_plain_sns(Some(&json!("Deploy FAILED")), &json!("step=build exit=101")),
            "🆘 Deploy FAILED\nstep=build exit=101"
        );
    }

    #[test]
    fn test_redactor_blanks_value_with_escaped_quotes_and_keeps_siblings() {
        let s = r#"{"NewStateReason": "Threshold \"Crossed\": secret", "Other": "keep"}"#;
        let out = redact_new_state_reason(s);
        assert!(!out.contains("secret"));
        assert!(!out.contains("Crossed"));
        assert!(out.contains("\"[redacted]\""));
        assert!(out.contains(r#""Other": "keep""#));
    }

    #[test]
    fn test_redactor_is_noop_on_ordinary_text() {
        assert_eq!(
            redact_new_state_reason("commit=abc ref=main"),
            "commit=abc ref=main"
        );
        // The bare key name as a string VALUE (no colon) is left alone —
        // it carries no forensic content.
        let bare = r#"{"note": "NewStateReason"}"#;
        assert_eq!(redact_new_state_reason(bare), bare);
    }

    #[test]
    fn test_redactor_non_string_and_unterminated_values_redact_to_end() {
        let out = redact_new_state_reason(r#"{"NewStateReason": {"deep": "secret"}}"#);
        assert!(!out.contains("secret"));
        assert!(out.ends_with("[redacted]"));
        let out2 = redact_new_state_reason(r#"{"NewStateReason": "unterminated secret"#);
        assert!(!out2.contains("secret"));
        // Trailing backslash at end-of-input must not panic or overrun.
        let out3 = redact_new_state_reason("{\"NewStateReason\": \"x\\");
        assert!(!out3.contains('x') || out3.contains("[redacted]"));
        assert!(out3.contains("[redacted]"));
    }

    #[test]
    fn test_redactor_handles_multiple_occurrences() {
        let s = r#"{"NewStateReason":"one secret","Nested":{"NewStateReason":"two secret"}}"#;
        let out = redact_new_state_reason(s);
        assert!(!out.contains("secret"));
        assert_eq!(out.matches("[redacted]").count(), 2);
    }

    #[test]
    fn test_fallback_body_cap_truncates_long_bodies_only() {
        let long = "x".repeat(PLAIN_FALLBACK_BODY_MAX_CHARS + 500);
        let out = format_plain_sns(None, &json!(long));
        assert!(out.ends_with("…[truncated]"));
        assert!(out.chars().count() <= PLAIN_FALLBACK_BODY_MAX_CHARS + 64);
        let short = "y".repeat(100);
        let out2 = format_plain_sns(None, &json!(short.clone()));
        assert_eq!(out2, format!("🔔 {short}"));
    }

    // ---- LambdaHandlerDelivery (legacy: 3 tests) ----
    //
    // The legacy runtime patched lambda_handler's collaborators (`_get_credentials`,
    // `_post_to_telegram`, time.sleep); the Rust seam is `deliver` — the
    // same fold→send composition `handle` runs, with the cache + transport
    // injected. The empty-event test drives the real `handle` (it returns
    // before any credential/HTTP leg).

    async fn invoke(records: Vec<Value>) -> (Value, Vec<String>) {
        let posted = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let mut cache = HashMap::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let posted_in = posted.clone();
        let result = deliver(&records, now, &mut cache, move |text| {
            let posted_in = posted_in.clone();
            async move {
                posted_in
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(text);
                Ok((200, "{\"ok\":true}".to_string()))
            }
        })
        .await;
        let texts = posted.lock().unwrap_or_else(|e| e.into_inner()).clone();
        (result, texts)
    }

    #[tokio::test]
    async fn test_alarm_record_reaches_telegram_post_through_lambda_handler() {
        let (result, posted) =
            invoke(vec![alarm("tv-prod-ws-no-alive-connections", "ALARM")]).await;
        assert_eq!(result["sent"], json!(1));
        assert_eq!(result["failures"], json!([]));
        assert_eq!(posted.len(), 1);
        assert!(posted[0].starts_with("🆘"));
        assert!(posted[0].contains("no live market data connections are up"));
    }

    #[tokio::test]
    async fn test_poisoned_timestamp_record_never_drops_genuine_alarm_in_batch() {
        // Legacy regression (2026-07-07 refute round 1): a batch containing
        // one edge-dated StateChangeTime crashed the ENTIRE invocation
        // before any send — the genuine 🆘 in the same batch was dropped.
        let (result, posted) = invoke(vec![
            alarm("tv-prod-ws-no-alive-connections", "ALARM"),
            alarm_record(
                "tv-prod-cpu-high-5min",
                "ALARM",
                "9999-12-31T23:59:59+00:00",
            ),
        ])
        .await;
        assert_eq!(result["failures"], json!([]));
        assert_eq!(result["sent"], json!(2));
        let genuine: Vec<&String> = posted
            .iter()
            .filter(|t| t.contains("no live market data connections are up"))
            .collect();
        assert_eq!(genuine.len(), 1);
        assert!(genuine[0].starts_with("🆘"));
        // The edge-dated alarm still pages (degraded timestamp, not dropped).
        let poisoned: Vec<&String> = posted
            .iter()
            .filter(|t| t.contains("Server CPU has been very high for 5 minutes"))
            .collect();
        assert_eq!(poisoned.len(), 1);
        assert!(poisoned[0].starts_with("🆘"));
    }

    #[tokio::test]
    async fn test_empty_event_skips_cleanly_without_credentials() {
        let result = handle(json!({})).await.unwrap();
        assert_eq!(result, json!({"sent": 0, "skipped": 1}));
    }

    // ---- SeverityEmojiHeuristic (legacy: 3 tests) ----

    #[test]
    fn test_alarm_state_beats_subject() {
        // State=ALARM wins even if Subject says "ok"
        assert_eq!(severity_emoji("ok thing", Some("ALARM")), "🆘");
    }

    #[test]
    fn test_insufficient_data_is_warning() {
        assert_eq!(severity_emoji("anything", Some("INSUFFICIENT_DATA")), "⚠️");
    }

    #[test]
    fn test_unknown_falls_back_to_bell() {
        assert_eq!(severity_emoji("hello", None), "🔔");
    }

    // ---- PlainTextTransport (legacy: 5 tests) ----

    #[test]
    fn test_post_payload_has_no_parse_mode() {
        // Plain-text mode is load-bearing: an alarm name containing '*'
        // must never cause a silent Markdown-parse 400 drop. (the legacy runtime used
        // inspect.getsource; the Rust payload is a pure fn — assert over
        // its exact keys.)
        let pairs = telegram_form_pairs("chat", "text *with* markup");
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["chat_id", "text", "disable_web_page_preview"]);
        for (key, _) in &pairs {
            assert_ne!(*key, "parse_mode");
        }
    }

    #[test]
    fn test_alarm_phrases_pass_telegram_commandments() {
        let banned = [
            "rkyv", "papaya", "mpsc", ".rs", "data/", "QuestDB", "SSM", "WAL",
        ];
        for (_, phrase) in &ALARM_PHRASES {
            for word in banned {
                assert!(
                    !phrase.contains(word),
                    "banned token {word:?} in {phrase:?}"
                );
            }
        }
    }

    #[test]
    fn the_spill_phrases_never_assert_a_drop_they_cannot_know_happened() {
        // MEASURED 2026-09-03, 08:32 IST: the operator was paged
        // "An incoming market data frame was dropped before it could be safely
        // stored" and "The safety file for incoming market data could not be
        // written". Neither had happened. The real events were a replay
        // DEFERRAL (deferred_segments 91, consumed_segments 5, on the byte
        // budget) and the catch-up drain standing DOWN for memory at 9.1 GB
        // against a 15 GiB ceiling -- both safety mechanisms working. Every
        // row was accounted for: dropped == spilled exactly, 405,422 ticks
        // and 22,538,490 depth rows.
        //
        // These two codes are emitted by MANY sites with different meanings --
        // a genuinely dropped frame (channel full / writer dead), a writer
        // respawn, a replay deferral, a memory stand-down. The phrase is keyed
        // on the ALARM name, so it cannot see which one fired. It must
        // therefore state what is true of ALL of them and never assert loss.
        //
        // Alarming in the wrong direction is not the safe direction: an
        // operator taught that "dropped" sometimes means "deferred" stops
        // believing the word on the day it is real.
        for key in ["errcode-ws-spill-01", "errcode-ws-spill-02"] {
            let phrase = alarm_phrase(key);
            assert!(
                phrase.contains("log line names which"),
                "{key}: the phrase must point at the payload that says which \
                 emitter fired, since the alarm name alone cannot -- got {phrase:?}"
            );
        }
        assert!(
            alarm_phrase("errcode-ws-spill-01").contains("does NOT mean anything was lost"),
            "ws-spill-01 covers writer respawn and memory stand-down as well as \
             loss; it must not read as a loss report"
        );
        let two = alarm_phrase("errcode-ws-spill-02");
        assert!(
            two.contains("EITHER") && two.contains("loses nothing"),
            "ws-spill-02 must name BOTH outcomes -- a real drop and a deferral \
             -- so it cannot collapse back into an unconditional drop claim"
        );
    }

    #[test]
    fn test_broker_scoped_alarm_phrases_carry_dhan_tag() {
        // Operator directive 2026-07-14: broker-specific alarm phrases lead
        // with the broker tag; the OK flip reuses the phrase so recoveries
        // inherit the tag automatically. Ratchet — removing a tag fails here.
        // token-remaining-low wording is coordinator-ruled EXACT (2026-07-14).
        assert_eq!(
            alarm_phrase("token-remaining-low"),
            "🔷 DHAN: access token expires soon — spot-1m + option-chain pulls will stop"
        );
        // The four keys below are LIVE alarms as of 2026-09-01. The previous
        // four (ws-pool-all-dead, ws-failed-connections, ws-reconnect-gap-high,
        // tick-gap-instruments-silent) named alarms that had been deleted from
        // terraform, so this ratchet was asserting a tag on phrases no running
        // alarm could ever reach — a passing test proving nothing about the
        // system. `alarm_phrase_coverage_guard` now stops that recurring.
        for key in [
            "ws-no-alive-connections",
            "dhan-live-lane-down",
            "dhan-no-ticks-flowing",
            "errcode-risk-gap-03",
        ] {
            let phrase = alarm_phrase(key);
            assert!(
                phrase.starts_with("🔷 DHAN: "),
                "{key} must lead with the DHAN tag: {phrase:?}"
            );
        }
    }

    #[test]
    fn test_host_scoped_liveness_phrase_carries_host_tag() {
        // The app-silent alarm is the whole PROCESS (not one broker feed) —
        // it must read as host/system-level, never broker-ambiguous.
        let phrase = alarm_phrase("market-hours-liveness-missing");
        assert!(phrase.starts_with("🖥️ HOST: "), "got: {phrase:?}");
    }

    #[test]
    fn test_recovered_line_inherits_broker_tag_from_phrase() {
        // OK flip renders "✅ Recovered: {phrase} — {IST} IST" — the tag
        // rides inside the phrase, so the recovery names the same broker.
        let out = house_line(&json!({
            "AlarmName": "tv-prod-token-remaining-low",
            "NewStateValue": "OK",
            "StateChangeTime": "2026-07-14T04:31:12.345+0000",
        }));
        assert!(out.starts_with("✅ Recovered: "));
        assert!(out.contains("🔷 DHAN:"));
    }

    // ---- ALARM repeat coalescing (2026-09-01) ----

    #[test]
    fn test_repeat_alarm_is_coalesced_but_still_sent() {
        // The never-drop law is intact: every ALARM is delivered. Only the
        // 2nd and later pages of ONE ongoing episode get the short render.
        let mut cache = HashMap::new();
        let first = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "ALARM")],
            1_000_000.0,
            &mut cache,
        );
        assert_eq!(first.len(), 1);
        assert!(first[0].starts_with("🆘 Server CPU"), "got: {:?}", first[0]);
        assert!(!first[0].contains("Still down"));

        let second = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "ALARM")],
            1_000_000.0 + 60.0,
            &mut cache,
        );
        assert_eq!(second.len(), 1, "a repeat ALARM must still be SENT");
        assert!(
            second[0].starts_with("🆘 Still down: "),
            "got: {:?}",
            second[0]
        );
        assert!(second[0].contains("2nd page in 10 min"));
        assert!(!second[0].contains('\n'), "the repeat render is ONE line");

        let third = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "ALARM")],
            1_000_000.0 + 120.0,
            &mut cache,
        );
        assert!(
            third[0].contains("3rd page in 10 min"),
            "got: {:?}",
            third[0]
        );
    }

    #[test]
    fn alarm_coalescing_is_never_widened_across_different_alarms() {
        // THE safety property. Several alarms in this account are the ONLY
        // signal their condition will ever produce — wal-frames-not-recovered
        // says so in its own description — so a second, DIFFERENT alarm firing
        // while a noisy first one is active must arrive as its own FULL page.
        // If the coalescing key is ever widened past (alarm name, state), this
        // test is what fails.
        let mut cache = HashMap::new();
        for tick in 0..4 {
            let out = fold_records(
                &[alarm("tv-prod-cpu-high-5min", "ALARM")],
                1_000_000.0 + f64::from(tick) * 30.0,
                &mut cache,
            );
            assert_eq!(out.len(), 1);
        }

        // A different alarm, deep inside the first one's active streak.
        let other = fold_records(
            &[alarm("tv-prod-wal-frames-not-recovered", "ALARM")],
            1_000_000.0 + 121.0,
            &mut cache,
        );
        assert_eq!(other.len(), 1, "a different alarm must never be swallowed");
        assert!(
            !other[0].contains("Still down"),
            "a DIFFERENT alarm must render as a full FIRST page, never as a repeat of \
             its neighbour's episode: {:?}",
            other[0]
        );
        assert!(
            other[0].contains("could not be recovered at start-up"),
            "the second alarm must carry its OWN phrase: {:?}",
            other[0]
        );
        assert!(other[0].contains('\n'), "a full page is two lines");
    }

    #[test]
    fn test_alarm_outside_the_coalesce_window_starts_a_fresh_full_page() {
        let mut cache = HashMap::new();
        fold_records(
            &[alarm("tv-prod-cpu-high-5min", "ALARM")],
            1_000_000.0,
            &mut cache,
        );
        let later = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "ALARM")],
            1_000_000.0 + ALARM_REPEAT_COALESCE_SECS + 1.0,
            &mut cache,
        );
        assert!(
            !later[0].contains("Still down"),
            "past the window a new episode renders in full: {:?}",
            later[0]
        );
    }

    #[test]
    fn test_recovery_resets_the_streak() {
        // An OK ends the episode, so the NEXT ALARM is a first page again —
        // otherwise a flapping alarm would render "7th page" for what the
        // operator experiences as a new failure.
        let mut cache = HashMap::new();
        fold_records(
            &[alarm("tv-prod-cpu-high-5min", "ALARM")],
            1_000_000.0,
            &mut cache,
        );
        fold_records(
            &[alarm("tv-prod-cpu-high-5min", "ALARM")],
            1_000_000.0 + 30.0,
            &mut cache,
        );
        fold_records(
            &[alarm("tv-prod-cpu-high-5min", "OK")],
            1_000_000.0 + 60.0,
            &mut cache,
        );
        let re_alarm = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "ALARM")],
            1_000_000.0 + 90.0,
            &mut cache,
        );
        assert!(
            !re_alarm[0].contains("Still down"),
            "an ALARM after a recovery is a NEW episode: {:?}",
            re_alarm[0]
        );
    }

    #[test]
    fn test_insufficient_data_never_rides_an_alarm_streak() {
        // The streak is keyed on (name, state). A flip to INSUFFICIENT_DATA is
        // a different state and must render in full, or an alarm that goes
        // unknown mid-episode would be labelled as more of the same.
        let mut cache = HashMap::new();
        fold_records(
            &[alarm("tv-prod-cpu-high-5min", "ALARM")],
            1_000_000.0,
            &mut cache,
        );
        let unknown = fold_records(
            &[alarm("tv-prod-cpu-high-5min", "INSUFFICIENT_DATA")],
            1_000_000.0 + 30.0,
            &mut cache,
        );
        assert!(unknown[0].starts_with("⚠️"), "got: {:?}", unknown[0]);
        assert!(!unknown[0].contains("Still down"));
    }

    /// `alarm_repeat_streak` is the counter behind the "3rd page in 10 min"
    /// line, so its two boundaries ARE the contract: a repeat of the same
    /// ALARM inside the window increments, and everything else restarts at 1.
    ///
    /// Tested directly rather than only through `fold_records`, because a
    /// fold-level test passes just as happily when the streak is stuck at 1 —
    /// every page still goes out, so nothing looks broken, and the operator
    /// silently loses the "this is one ongoing episode" signal that the
    /// coalescing exists to give.
    #[test]
    fn test_alarm_repeat_streak_increments_only_inside_the_window() {
        let mut cache: AlertCache = HashMap::new();
        let t0 = 1_000_000.0;

        // Never seen before: this is the first page, not a repeat.
        assert_eq!(alarm_repeat_streak("a", t0, &cache), 1);

        cache.insert("a".to_string(), ("ALARM".to_string(), t0, 1));
        assert_eq!(
            alarm_repeat_streak("a", t0 + 1.0, &cache),
            2,
            "a repeat inside the window is the second page"
        );

        // The window is EXCLUSIVE at its edge, so a page landing exactly one
        // window later opens a fresh episode rather than extending the old
        // one. Pinned because an off-by-one here would let a slow flap
        // accumulate one unbroken streak all session.
        assert_eq!(
            alarm_repeat_streak("a", t0 + ALARM_REPEAT_COALESCE_SECS, &cache),
            1
        );

        // A cached OK is not a streak however recent it is — otherwise a
        // recovery followed by a genuine new failure would arrive as a terse
        // "still down" line instead of a full page.
        cache.insert("b".to_string(), ("OK".to_string(), t0, 7));
        assert_eq!(alarm_repeat_streak("b", t0 + 1.0, &cache), 1);
    }

    /// The short render must still be a real page: one line, plain English,
    /// emoji first, IST — the Telegram commandments do not relax because the
    /// message is a repeat.
    #[test]
    fn test_repeat_alarm_line_is_one_line_and_names_the_page_number() {
        let inner = json!({
            "AlarmName": "tv-prod-cpu-high-5min",
            "NewStateValue": "ALARM",
            "StateChangeTime": "2026-07-07T04:31:12.345+0000",
        });
        let line = repeat_alarm_line(&inner, 3);

        // One line. A "short" render that wraps to several lines is not
        // shorter than the full page and buys nothing.
        assert_eq!(line.lines().count(), 1, "got: {line:?}");

        // Plain English ordinal (commandment 1) — never "page #3".
        assert!(line.contains("3rd page"), "got: {line:?}");
        assert!(!line.contains('#'), "got: {line:?}");

        // Emoji first (commandment 5).
        assert!(line.starts_with('🆘'), "got: {line:?}");

        // The window is stated, so the operator can judge the RATE and not
        // just the count.
        assert!(line.contains("10 min"), "got: {line:?}");

        // IST (commandment 9), never a raw vendor UTC stamp — and it must be
        // the ALARM'S OWN time (04:31 UTC = 10:01 AM IST), never "now".
        // `ist_12h` falls back to the current time on an unparseable input,
        // so a weaker assertion here passes while every repeat page silently
        // claims to have just happened.
        assert!(line.contains("10:01 AM IST"), "got: {line:?}");
        assert!(!line.contains("+0000"), "got: {line:?}");

        // It carries the same plain-English phrase the full page used, so the
        // repeat is recognisable as the SAME incident.
        assert!(line.contains("CPU"), "got: {line:?}");
    }

    #[test]
    fn test_ordinal_suffixes_read_as_english() {
        for (n, want) in [
            (1u32, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (101, "101st"),
            (111, "111th"),
        ] {
            assert_eq!(ordinal(n), want, "ordinal({n})");
        }
    }
    // ---- Rust-side additions beyond the legacy suite ----

    #[test]
    fn test_form_urlencode_matches_legacy_quote_plus() {
        // legacy: urlencode({"chat_id": "-100", "text": "a b&c=✅"}) —
        // space → '+', '&'/'=' → %26/%3D, UTF-8 bytes %XX uppercase.
        let pairs = [
            ("chat_id", "-100".to_string()),
            ("text", "a b&c=✅".to_string()),
        ];
        assert_eq!(
            form_urlencode(&pairs),
            "chat_id=-100&text=a+b%26c%3D%E2%9C%85"
        );
    }
}
