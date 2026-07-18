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
//! Environment variables (set by Terraform — unchanged from the Python):
//!   TELEGRAM_BOT_TOKEN_SSM_PARAM  — SSM path holding the bot token
//!   TELEGRAM_CHAT_ID_SSM_PARAM    — SSM path holding the chat ID
//!   LOG_LEVEL                     — INFO (default) / DEBUG / WARNING
//!
//! Parity ledger (every deliberate deviation from the Python original):
//! - python `print()` forensics → `tracing::info!` (same CloudWatch Logs
//!   destination; the crate denies print_stdout).
//! - python edge-dated timestamps (year 0001/9999) raised OverflowError on
//!   the tz conversion and fell back to the invocation time; chrono
//!   converts those dates without overflow, so the rendered clock is the
//!   real converted time — both satisfy the `H:MM AM|PM` format contract.
//! - python `str(dict)` renders `{'k': 'v'}`; Rust renders the serde_json
//!   representation `{"k":"v"}` for non-string SNS Message bodies (only
//!   reachable on malformed publishes; no raw alarm JSON either way).
//! - python `_fold_records(cache=None)` optionality collapsed: every call
//!   site (lambda handler + all tests) passes a cache, so the Rust fn
//!   takes `&mut HashMap` unconditionally.
//! - python truthy-non-dict records / Sns values raised AttributeError →
//!   generic safe line; Rust maps every non-object, non-null shape to the
//!   same generic safe line (exotic FALSY non-dicts like `""`/`[]`/`0`
//!   would have rendered "🔔 " in python — no test pins that; documented).
//! - python wrapped the render loop + the whole fold in `except Exception`
//!   fail-open arms; the Rust render chain has no panicking path, so those
//!   arms are structurally unreachable and not reproduced.
//! - reqwest errors are stripped of their URL (`Error::without_url`)
//!   before entering the failures list — the URL embeds the bot token
//!   (hardening beyond python; urllib error strings never carried it).
//! - env vars are read per invocation instead of at module import — same
//!   effective behavior inside a Lambda container.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, FixedOffset, NaiveDateTime, Offset, Timelike, Utc};
use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::{error, info, warn};

pub const TELEGRAM_API_BASE: &str = "https://api.telegram.org";
pub const TELEGRAM_TIMEOUT_SECONDS: u64 = 8;

/// Warm-container duplicate-OK guard (judge contract, robustness graft):
/// suppress a repeat SAME-state OK for the same alarm within this window.
/// ALARM-state records are NEVER consulted against this cache — a dropped
/// 🆘 is unacceptable; a duplicate ✅ after a cold start is accepted.
pub const OK_REPEAT_SUPPRESS_SECS: f64 = 300.0;

pub const GENERIC_SAFE_LINE: &str = "🔔 Alert received — details are in the server log";

/// Known alarm → auto-driver plain English (charter §D: no library names,
/// no file paths, no jargon). Keys are alarm names AFTER the tv-<env>-
/// prefix strip. Unknown names fall back to a humanized form — never a
/// lookup panic, never raw JSON. Python parity: `ALARM_PHRASES` dict,
/// byte-identical phrases (incl. the 🔷 DHAN / 🖥️ HOST broker tags per the
/// operator directive 2026-07-14).
///
/// O(n) scan over 29 entries per lookup — cold path (a handful of alarm
/// renders per SNS batch), deliberately not a hash map so the table stays
/// a reviewable literal.
pub const ALARM_PHRASES: [(&str, &str); 29] = [
    // 2026-07-18 (stage-4 dead-producer sweep): the dlq-ticks /
    // late-tick-after-boundary / spill-dropped / ticks-dropped alarm slugs
    // were retired with their deleted tick-chain alarms; mappings removed.
    (
        "aggregator-no-seals",
        "Candle building has stopped during market hours",
    ),
    (
        "binary-sha-stale",
        "The running app is more than a day behind the latest approved code",
    ),
    (
        "boot-heartbeat-missing",
        "The app did not start on time this morning",
    ),
    (
        "budget-killswitch-errors",
        "The cost kill-switch helper is failing",
    ),
    ("clock-skew-high", "The server clock has drifted too far"),
    (
        "cpu-high-5min",
        "Server CPU has been very high for 5 minutes",
    ),
    ("disk-used-high", "Server disk is almost full"),
    ("disk-watcher-respawn", "The disk monitor keeps restarting"),
    (
        "ebs-write-latency-high",
        "Disk writes have become very slow",
    ),
    (
        "eventbridge-dlq-depth",
        "Scheduled cloud tasks are failing and piling up",
    ),
    (
        "instance-status-failed",
        "The cloud server is failing its health checks",
    ),
    (
        "logs-ingestion-runaway",
        "Log volume is growing abnormally fast",
    ),
    (
        "market-hours-liveness-missing",
        "🖥️ HOST: the app has gone silent during market hours",
    ),
    ("mem-used-high", "Server memory is almost full"),
    (
        "network-out-runaway",
        "Outbound network traffic is abnormally high",
    ),
    (
        "operator-control-errors",
        "The operator control page is failing",
    ),
    (
        "order-update-ws-inactive",
        "Order confirmations feed has gone quiet",
    ),
    ("orders-rejected", "Orders are being rejected"),
    (
        "questdb-console-front-errors",
        "The database console page is failing",
    ),
    (
        "questdb-disconnected",
        "The database has been unreachable for too long",
    ),
    (
        "realtime-guarantee-critical",
        "Overall system health has dropped to critical",
    ),
    (
        "system-status-failed",
        "The cloud hardware is failing its health checks",
    ),
    (
        "telegram-webhook-errors",
        "The Telegram alert relay itself is failing",
    ),
    (
        "tick-gap-instruments-silent",
        "🔷 DHAN: some instruments have stopped sending prices",
    ),
    (
        "token-remaining-low",
        "🔷 DHAN: access token expires soon — spot-1m + option-chain pulls will stop",
    ),
    (
        "ws-failed-connections",
        "🔷 DHAN: the live market data connection keeps failing",
    ),
    (
        "ws-frame-dropped-no-wal",
        "Live market data arrived but could not be saved",
    ),
    (
        "ws-pool-all-dead",
        "🔷 DHAN: ALL live market data connections are down",
    ),
    (
        "ws-reconnect-gap-high",
        "🔷 DHAN: the live market data feed is taking too long to reconnect",
    ),
];

/// Cached SSM reads — Lambda containers stay warm for ~15 min. Re-fetch
/// only when the container is cold. Python parity: `_CACHED_TOKEN` /
/// `_CACHED_CHAT_ID` module globals.
static CACHED_TOKEN: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
static CACHED_CHAT_ID: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

/// Warm-container OK-suppression cache — Python parity: `_LAST_SENT`.
static LAST_SENT: Mutex<Option<HashMap<String, (String, f64)>>> = Mutex::new(None);

/// The fixed IST offset (+05:30). `east_opt` is statically in range; the
/// fallback arm is unreachable (kept to avoid `unwrap` per crate lints).
fn ist() -> FixedOffset {
    FixedOffset::east_opt(crate::time::IST_OFFSET_SECS).unwrap_or_else(|| Utc.fix())
}

/// Python `str(value or default)` over a JSON field: falsy (missing /
/// null / "" / false) → default; other non-strings stringify (ledger:
/// serde_json repr, not python repr — unreachable on real alarm JSON).
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
/// 5+10. Python parity: `_severity_emoji` (the python `"deploy ok" in`
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
/// seconds, tz-naive treated as UTC). Python parity: `fromisoformat`
/// with the `Z`→`+00:00` replace + the strptime fallbacks (python also
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
    // tz-naive input — python `parsed.replace(tzinfo=timezone.utc)`.
    if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc().fixed_offset());
    }
    None
}

/// Render a CloudWatch StateChangeTime as an IST 12-hour clock string.
///
/// Python parity: `_ist_12h`. Malformed / missing input falls back to
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
/// environment-agnostic. Python parity: `_ENV_PREFIX_RE = ^tv-[a-z0-9]+-`
/// (leftmost-anchored substitution, implemented without a regex dep).
fn strip_env_prefix(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("tv-") {
        let run = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .count();
        if run > 0 {
            if let Some(tail) = rest[run..].strip_prefix('-') {
                return tail;
            }
        }
    }
    name
}

/// Map an alarm name to plain English; humanize unknown names (fail-open).
/// Python parity: `_alarm_phrase`.
pub fn alarm_phrase(alarm_name: &str) -> String {
    let key = strip_env_prefix(alarm_name.trim());
    if let Some((_, phrase)) = ALARM_PHRASES.iter().find(|(k, _)| *k == key) {
        return (*phrase).to_string();
    }
    let spaced = key.replace('-', " ").replace('_', " ");
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
/// The raw NewStateReason NEVER enters this string. Python parity:
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
/// Python parity: `_recovered_line`.
pub fn recovered_line(phrases: &[String], ist_time: &str) -> String {
    format!("✅ Recovered: {} — {ist_time} IST", phrases.join("; "))
}

/// Return the CloudWatch alarm object if `message` is an alarm JSON
/// string, else None. Python parity: `_parse_alarm`.
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

/// Format a non-CloudWatch SNS publish (e.g., from the deploy-aws
/// workflow). Python parity: `_format_plain_sns`.
pub fn format_plain_sns(subject: Option<&Value>, message: &Value) -> String {
    let subject_s: Option<String> = match subject {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    };
    let emoji = severity_emoji(subject_s.as_deref().unwrap_or(""), None);
    let body = match message {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match subject_s {
        Some(s) => format!("{emoji} {s}\n{body}"),
        None => format!("{emoji} {body}"),
    }
}

/// True when an OK for `name` repeats a recent OK (warm-container dedupe).
///
/// ONLY ever called for OK-state records — ALARM records are never
/// routed through this cache (never-drop law). Python parity:
/// `_should_suppress_ok`.
pub fn should_suppress_ok(
    name: &str,
    now_epoch: f64,
    cache: &HashMap<String, (String, f64)>,
) -> bool {
    let Some((last_state, last_epoch)) = cache.get(name) else {
        return false;
    };
    last_state == "OK" && (now_epoch - last_epoch) < OK_REPEAT_SUPPRESS_SECS
}

/// Fold one SNS batch into the final list of Telegram texts.
///
/// Rules (judge final contract, Module 5) — Python parity: `_fold_records`:
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
pub fn fold_records(
    records: &[Value],
    now_epoch: f64,
    cache: &mut HashMap<String, (String, f64)>,
) -> Vec<String> {
    let now = now_epoch;
    let mut plain_texts: Vec<String> = Vec::new();
    // Per alarm name (first-appearance order): the LAST record wins,
    // remembering whether an ALARM was seen anywhere in the batch.
    let mut name_order: Vec<String> = Vec::new();
    let mut last_by_name: HashMap<String, Value> = HashMap::new();
    let mut saw_alarm: HashMap<String, bool> = HashMap::new();

    let empty_message = Value::String(String::new());
    for record in records {
        // Python parity: a truthy non-dict record / Sns value raised
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

    // (python wrapped this loop in a per-record fail-open `except` —
    //  the Rust render chain has no panicking path; ledger.)
    for name in &name_order {
        let Some(alarm) = last_by_name.get(name) else {
            continue;
        };
        let final_state = value_str_or(alarm.get("NewStateValue"), "ALARM").to_uppercase();

        if final_state == "OK" && saw_alarm.get(name).copied().unwrap_or(false) {
            // ALARM→OK inside one batch: ONLY the recovered line.
            out.push(house_line(alarm));
            cache.insert(name.clone(), ("OK".to_string(), now));
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
            cache.insert(name.clone(), ("OK".to_string(), now));
            continue;
        }

        // ALARM / INSUFFICIENT_DATA final state — individual house line,
        // NEVER suppressed, NEVER folded away by an earlier OK.
        out.push(house_line(alarm));
        cache.insert(name.clone(), (final_state, now));
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
/// ratchet asserts over these keys — the Rust analog of python's
/// `inspect.getsource` scan).
pub fn telegram_form_pairs(chat_id: &str, text: &str) -> [(&'static str, String); 3] {
    [
        ("chat_id", chat_id.to_string()),
        ("text", text.to_string()),
        ("disable_web_page_preview", "true".to_string()),
    ]
}

/// application/x-www-form-urlencoded encoding — Python parity:
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
/// Python parity: `_post_to_telegram`. UNPROVEN until deploy — the live
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
    // python: resp.read().decode("utf-8", errors="replace") — reqwest
    // text() is already lossy on invalid UTF-8; a transport error while
    // reading propagates like the python exception path.
    let body_text = resp.text().await?;
    Ok((status, body_text))
}

/// Send every folded text into the Telegram POST — the single delivery
/// choke point (no filtering between the fold and the POST). Python
/// parity: the `for text in texts:` loop of `lambda_handler`. `post` is
/// the injected transport so tests exercise the full never-drop boundary
/// without HTTP (the Rust analog of python's `mock.patch`).
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
                    error!(status, body = %head, "Telegram POST returned an error status");
                } else {
                    sent += 1;
                }
            }
            Err(err) => {
                failures.push(err.clone());
                error!(error = %err, "Failed to relay one message to Telegram");
            }
        }
        // Cheap rate-limit cushion. Telegram allows ~30 msg/sec per bot;
        // if 5+ alarms fire in the same SNS batch we don't want to flirt
        // with their throttle. Python parity: `time.sleep(0.05)`.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    json!({"sent": sent, "failures": failures, "records": records_len})
}

/// Fold + send composed — the testable end-to-end delivery seam the
/// LambdaHandlerDelivery tests drive (python patched `lambda_handler`'s
/// collaborators; the Rust seam injects the cache + transport instead).
pub async fn deliver<F, Fut>(
    records: &[Value],
    now_epoch: f64,
    cache: &mut HashMap<String, (String, f64)>,
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
/// Python parity: `_get_credentials`. Secret VALUES are never logged.
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

/// SNS-triggered entry point — Python parity: `lambda_handler`.
///
/// UNPROVEN until deploy: the live SSM + Telegram HTTP legs run only in
/// a real Lambda. Credential errors are propagated (`?`) so SNS marks
/// the delivery failed and retries per its default policy — the python
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
            error!("Failed to fetch Telegram credentials from SSM");
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
    // (python backstopped a fold crash with one generic line per record;
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
                        colon >= 1 && colon <= 2
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

    // ---- HouseLine (Python: 6 tests) ----

    #[test]
    fn test_house_line_no_raw_threshold_json() {
        let a = json!({
            "AlarmName": "tv-prod-order-update-ws-inactive",
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
        assert!(out.contains("Order confirmations feed has gone quiet"));
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

    // ---- Ist12Hour (Python: 5 tests) ----

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
        // Python regression (2026-07-07 refute round 1): year-0001/9999
        // inputs OverflowError'd on the IST conversion. chrono converts
        // them without overflow (module ledger) — the contract is the
        // clock FORMAT, which must hold either way.
        for raw in ["9999-12-31T23:59:59+00:00", "0001-01-01T00:00:00+05:31"] {
            let out = ist_12h(raw);
            assert!(tests_clock_only(&out), "input {raw:?} gave {out:?}");
        }
    }

    // ---- AlarmPhrase (Python: 3 tests) ----

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

    // ---- FoldRecords (Python: 9 tests) ----

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
                ("ALARM".to_string(), 999_999.0),
            ),
            (
                "tv-prod-questdb-disconnected".to_string(),
                ("OK".to_string(), 999_999.0),
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

    // ---- FormatPlainSns (Python: 3 tests) ----

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

    // ---- LambdaHandlerDelivery (Python: 3 tests) ----
    //
    // Python patched lambda_handler's collaborators (`_get_credentials`,
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
        let (result, posted) = invoke(vec![alarm("tv-prod-ws-pool-all-dead", "ALARM")]).await;
        assert_eq!(result["sent"], json!(1));
        assert_eq!(result["failures"], json!([]));
        assert_eq!(posted.len(), 1);
        assert!(posted[0].starts_with("🆘"));
        assert!(posted[0].contains("ALL live market data connections are down"));
    }

    #[tokio::test]
    async fn test_poisoned_timestamp_record_never_drops_genuine_alarm_in_batch() {
        // Python regression (2026-07-07 refute round 1): a batch containing
        // one edge-dated StateChangeTime crashed the ENTIRE invocation
        // before any send — the genuine 🆘 in the same batch was dropped.
        let (result, posted) = invoke(vec![
            alarm("tv-prod-ws-pool-all-dead", "ALARM"),
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
            .filter(|t| t.contains("ALL live market data connections are down"))
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

    // ---- SeverityEmojiHeuristic (Python: 3 tests) ----

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

    // ---- PlainTextTransport (Python: 5 tests) ----

    #[test]
    fn test_post_payload_has_no_parse_mode() {
        // Plain-text mode is load-bearing: an alarm name containing '*'
        // must never cause a silent Markdown-parse 400 drop. (Python used
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
    fn test_broker_scoped_alarm_phrases_carry_dhan_tag() {
        // Operator directive 2026-07-14: broker-specific alarm phrases lead
        // with the broker tag; the OK flip reuses the phrase so recoveries
        // inherit the tag automatically. Ratchet — removing a tag fails here.
        // token-remaining-low wording is coordinator-ruled EXACT (2026-07-14).
        assert_eq!(
            alarm_phrase("token-remaining-low"),
            "🔷 DHAN: access token expires soon — spot-1m + option-chain pulls will stop"
        );
        for key in [
            "ws-pool-all-dead",
            "ws-failed-connections",
            "ws-reconnect-gap-high",
            "tick-gap-instruments-silent",
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

    // ---- Rust-side additions beyond the Python suite ----

    #[test]
    fn test_form_urlencode_matches_python_quote_plus() {
        // python: urlencode({"chat_id": "-100", "text": "a b&c=✅"}) —
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
