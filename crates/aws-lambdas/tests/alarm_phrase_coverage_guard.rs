//! Guard: the Telegram phrase table and the live CloudWatch alarm set must
//! agree, in BOTH directions.
//!
//! # What went wrong without it
//!
//! `ALARM_PHRASES` is the only thing standing between a CloudWatch alarm and a
//! message on the operator's phone that reads like English. When an alarm name
//! is missing from it, `alarm_phrase` falls back to humanizing the slug — so
//! `tv-prod-errcode-ws-gap-03-xverify-vacuous` arrives as
//! *"Errcode ws gap 03 xverify vacuous"*, which is the alarm name with its
//! hyphens removed, not an explanation. Measured before this guard existed: 29
//! entries, of which **8 named alarms that no longer exist** and 21 matched a
//! live one, against **102 live alarms** — so 83 alarms, including the entire
//! coded-error family that is the whole `error!` → phone route, had no phrase.
//!
//! Nothing caught that, because a missing entry has no symptom in any test: the
//! fallback never panics and never returns an empty string. It just quietly
//! degrades every future alarm, and the table rots by default because alarms are
//! added in terraform by people who have no reason to open this crate.
//!
//! # Why both directions are checked
//!
//! **Missing** entries make new alarms unreadable. **Stale** entries are worse
//! than useless in a subtler way: they are the record of a decision about an
//! alarm that no longer exists, they make the table look better-covered than it
//! is, and this crate's other tests can assert against them forever without any
//! of it being true of the running system — a false-OK in the shape of a
//! passing test.
//!
//! # Honest limit
//!
//! This proves the KEYS line up. It cannot judge whether a phrase is a *good*
//! explanation of its alarm — that stays a human review, which is exactly why
//! the phrases are hand-written literals in one reviewable file rather than
//! pulled from each alarm's terraform description at run time.

use std::collections::BTreeSet;
use std::path::PathBuf;

use tickvault_aws_lambdas::telegram_webhook::ALARM_PHRASES;

/// The terraform directory, resolved from this crate's manifest path so the
/// test does not depend on the working directory cargo happens to use.
///
/// Read at RUNTIME rather than via `include_str!`. That is deliberate: a fixed
/// list of files is exactly how this kind of guard goes quietly blind — someone
/// adds `new-alarms.tf`, nobody adds it to the list, and the guard keeps
/// passing while the alarms in it have no phrase. Enumerating the directory has
/// no such hole.
fn terraform_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/aws/terraform")
}

/// Every `*.tf` file that DECLARES at least one alarm, with its contents.
fn alarm_bearing_files() -> Vec<(String, String)> {
    let dir = terraform_dir();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.expect("readable directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        if !body.contains("resource \"aws_cloudwatch_metric_alarm\"") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unnamed>")
            .to_string();
        out.push((name, body));
    }
    out.sort();
    out
}

fn error_code_alarms_tf() -> String {
    let path = terraform_dir().join("error-code-alarms.tf");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}
fn code_only(src: &str) -> String {
    src.lines()
        .map(|line| match line.find('#') {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `alarm_name = "tv-${var.environment}-<slug>"` in the terraform,
/// reduced to `<slug>` — the same key space `strip_env_prefix` produces at run
/// time.
///
/// The one templated name, `tv-${var.environment}-errcode-${each.key}`, is
/// expanded from the `local.error_code_alerts` map keys it iterates, so the
/// coded-error family is covered per code rather than as one wildcard.
fn live_alarm_slugs() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let files = alarm_bearing_files();
    let keys = error_code_keys();
    for (_, src) in &files {
        for line in code_only(src).lines() {
            let Some(at) = line.find("alarm_name") else {
                continue;
            };
            let rest = &line[at..];
            let Some(eq) = rest.find('=') else { continue };
            let after = &rest[eq + 1..];
            let Some(open) = after.find('"') else {
                continue;
            };
            let tail = &after[open + 1..];
            let Some(close) = tail.find('"') else {
                continue;
            };
            let raw = &tail[..close];

            let Some(stripped) = raw.strip_prefix("tv-${var.environment}-") else {
                continue;
            };
            if stripped.contains("${each.key}") {
                for key in &keys {
                    out.insert(stripped.replace("${each.key}", key));
                }
                continue;
            }
            // Any OTHER interpolation is something this parser does not
            // understand; fail loudly rather than silently skipping an alarm.
            assert!(
                !stripped.contains("${"),
                "unrecognised interpolation in alarm_name {raw:?}. This guard expands \
                 only the errcode for_each; a new templated alarm name needs the parser \
                 taught about it, because silently skipping it is how the table rotted \
                 in the first place."
            );
            out.insert(stripped.to_string());
        }
    }
    out
}

/// The keys of `local.error_code_alerts` — the for_each the errcode alarm
/// iterates.
fn error_code_keys() -> Vec<String> {
    let mut out = Vec::new();
    let src = error_code_alarms_tf();
    for line in src.lines() {
        // Map entries are indented exactly four spaces: `    "dh-901" = {`.
        if !line.starts_with("    \"") || !line.trim_end().ends_with("= {") {
            continue;
        }
        let body = &line[5..];
        let Some(close) = body.find('"') else {
            continue;
        };
        let key = &body[..close];
        if !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            out.push(key.to_string());
        }
    }
    out
}

fn phrase_keys() -> BTreeSet<String> {
    ALARM_PHRASES
        .iter()
        .map(|(k, _)| (*k).to_string())
        .collect()
}

#[test]
fn every_live_alarm_has_a_plain_english_phrase() {
    let live = live_alarm_slugs();
    let phrases = phrase_keys();
    let missing: Vec<_> = live.difference(&phrases).cloned().collect();
    assert!(
        missing.is_empty(),
        "{} live CloudWatch alarm(s) have no ALARM_PHRASES entry. Without one, \
         `alarm_phrase` falls back to humanizing the slug, so the operator receives the \
         alarm NAME with its hyphens removed instead of an explanation. Add a plain \
         English line (no jargon, no file paths) to ALARM_PHRASES in \
         telegram_webhook.rs for each of:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn every_phrase_names_an_alarm_that_still_exists() {
    let live = live_alarm_slugs();
    let phrases = phrase_keys();
    let stale: Vec<_> = phrases.difference(&live).cloned().collect();
    assert!(
        stale.is_empty(),
        "{} ALARM_PHRASES entr(y/ies) name an alarm that no longer exists in terraform. \
         A stale entry is not harmless: it makes the table look better covered than it \
         is, and other tests can keep asserting against it forever while none of it is \
         true of the running system. Delete:\n  {}",
        stale.len(),
        stale.join("\n  ")
    );
}

#[test]
fn the_alarm_file_discovery_actually_finds_files() {
    // The scan enumerates deploy/aws/terraform at runtime, so a NEW *.tf full
    // of alarms is picked up with nobody having to remember this crate exists.
    // What that cannot catch is the discovery itself breaking — a moved
    // directory, a changed extension convention, a read failure swallowed
    // somewhere — which would make both directions of this guard pass while
    // comparing an empty set against an empty set. So the discovery is pinned
    // to a floor and to two files that must always be in it.
    let files = alarm_bearing_files();
    assert!(
        files.len() >= 15,
        "only {} terraform file(s) were found to declare an alarm — the discovery is \
         probably broken, and a broken discovery makes this whole guard pass vacuously",
        files.len()
    );
    let names: BTreeSet<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    for required in ["error-code-alarms.tf", "live-lane-alarms.tf"] {
        assert!(
            names.contains(required),
            "{required} declares alarms but was not discovered: {names:?}"
        );
    }
    for (name, body) in &files {
        assert!(
            body.contains("resource \"aws_cloudwatch_metric_alarm\""),
            "{name} was discovered without declaring an alarm — the filter is wrong"
        );
    }
}

#[test]
fn the_scan_is_not_vacuous() {
    let live = live_alarm_slugs();
    assert!(
        live.len() >= 90,
        "expected ~102 live alarm slugs, parsed {}. A parser that silently returns few \
         or none makes both directions of this guard pass while proving nothing — which \
         is the exact false-OK shape it exists to stop.",
        live.len()
    );
    assert!(
        live.contains("errcode-dh-901"),
        "the errcode for_each expansion is broken — the coded-error family is the whole \
         error-to-phone route and must be covered per code, not as one wildcard"
    );
    assert!(
        live.contains("questdb-disconnected"),
        "a plainly-named alarm went missing from the parse"
    );
}

#[test]
fn guard_self_test_parser_bites() {
    // A parser that accepts anything proves nothing, so exercise its edges.
    let keys = error_code_keys();
    assert!(
        keys.len() >= 20,
        "error_code_alerts key extraction found only {} keys",
        keys.len()
    );
    assert!(keys.iter().all(|k| !k.contains(' ')), "keys must be slugs");

    let stripped = code_only("# alarm_name = \"tv-${var.environment}-ghost\"\nreal = 1");
    assert!(
        !stripped.contains("alarm_name"),
        "comment stripping must drop commented alarms, or a slug named only in prose \
         would register as live and demand a phrase for an alarm that does not exist"
    );
}
