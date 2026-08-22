//! Every Telegram event variant this system declares should be one production
//! code can actually send.
//!
//! Measured 2026-08-22: **31 of 79 `NotificationEvent` variants have no
//! production constructor at all** -- they exist in the enum, they render, they
//! carry severity logic and tests, and no code path outside `#[cfg(test)]` ever
//! builds one. Among them are the names an operator would assume cover the
//! worst failures: `QuestDbDisconnected`, `WebSocketPoolHalt`,
//! `MarketOpenStreamingFailed`, `SelfTestCritical`, `RealtimeGuaranteeCritical`,
//! `BootDeadlineMissed`, `WebSocketReconnectionExhausted`.
//!
//! # Why this class keeps recurring
//!
//! It has now been hit at least twice with a written record each time.
//! `CadenceExpiryDisagreement` was retired on 2026-08-21 with the reasoning
//! stated verbatim in `dhan-rest-only-noise-lock-2026-07-14.md`: leaving it
//! "would have meant a declared Telegram family that nothing can send, which is
//! exactly the permanently quiet surface this table's deleted rows exist to
//! prevent." And `CrossVerify1mSummary` / `CrossVerify1mAborted` are the same
//! shape found today -- their emitter (`cross_verify_1m_boot.rs`) was deleted
//! on 2026-07-13, the visibility suite was tombstoned the next day, and when
//! the comparator came back under the 2026-08-09 revival, only the COMPARATOR
//! came back. The variants still carry `test_cross_verify_1m_summary_compared_
//! zero_is_high` and `test_cross_verify_1m_summary_blind_message_is_loud`: a
//! card designed to shout when the day proved nothing, that the live comparator
//! producing exactly that condition cannot reach.
//!
//! # What this guard does and does not claim
//!
//! It does NOT say all 31 are defects. Several are deliberately retired and say
//! so at their own sites (the post-market pool-halt family, for one), and
//! keeping a retired variant as historical audit is the house convention the
//! Groww error codes followed. What the guard fixes is that the set was
//! INVISIBLE and free to grow. It is now shrink-only: wiring a variant or
//! deleting it makes the list shorter, and a NEW variant that ships without a
//! dispatcher fails the build.
//!
//! Detection is deliberately generous, so a false positive cannot happen: a
//! variant counts as dispatched if `NotificationEvent::<Name>` or
//! `Self::<Name>` appears anywhere in any crate's `src/` outside this enum's
//! own file, with inline test modules cut. A helper that builds the variant
//! indirectly therefore still counts.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Variants with no production constructor, measured 2026-08-22.
///
/// SHRINK-ONLY. Adding an entry means shipping a Telegram card that cannot be
/// sent, and needs to be argued in review.
const NO_PRODUCTION_DISPATCHER: &[&str] = &[
    "BarMismatchCorrectedFromHistorical",
    "BarMismatchCrossCheckFailed",
    "BarMismatchCrossCheckInconclusive",
    "BootDeadlineMissed",
    "CrossVerify1mAborted",
    "CrossVerify1mSummary",
    "CustomStatusUrgent",
    "EndOfDayDigest",
    "InstrumentBuildFailed",
    "IpVerificationFailed",
    "IpVerificationSuccess",
    "MarketOpenReadinessConfirmation",
    "MarketOpenStreamingConfirmation",
    "MarketOpenStreamingFailed",
    "QuestDbDisconnected",
    "QuestDbReconnected",
    "RealtimeGuaranteeCritical",
    "RealtimeGuaranteeDegraded",
    "RealtimeGuaranteeHealthy",
    "SelfTestCritical",
    "SelfTestDegraded",
    "SelfTestPassed",
    "SpotCrossverifyAborted",
    "StaticIpBootCheckFailed",
    "StaticIpBootCheckPassed",
    "StaticIpBootCheckRetrying",
    "WebSocketPoolDeferredOffHours",
    "WebSocketPoolDegraded",
    "WebSocketPoolHalt",
    "WebSocketPoolRecovered",
    "WebSocketReconnectionExhausted",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/core -> crates -> repo root")
        .to_path_buf()
}

fn events_path() -> PathBuf {
    repo_root().join("crates/core/src/notification/events.rs")
}

/// Every variant declared in `pub enum NotificationEvent`.
fn declared_variants() -> Vec<String> {
    let src = std::fs::read_to_string(events_path()).expect("events.rs must be readable");
    let mut out = Vec::new();
    let mut inside = false;
    for line in src.lines() {
        if line.starts_with("pub enum NotificationEvent") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if line.starts_with('}') {
            break;
        }
        let Some(rest) = line.strip_prefix("    ") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if name.is_empty() {
            continue;
        }
        // Trim before matching: variants are written `Name {`, with a space,
        // and requiring the brace to be adjacent found only 8 of 79.
        let tail = rest[name.len()..].trim_start();
        if tail.starts_with('{') || tail.starts_with(',') || tail.starts_with('(') {
            out.push(name);
        }
    }
    out
}

/// All production `src/` text across the workspace, minus this enum's own file
/// and minus every trailing `#[cfg(test)] mod` block.
fn production_sources() -> String {
    let root = repo_root().join("crates");
    let mut buf = String::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs")
                && p.to_string_lossy().contains("/src/")
                && !p.ends_with("notification/events.rs")
                && let Ok(text) = std::fs::read_to_string(&p)
            {
                buf.push_str(&cut_test_module(&text));
                buf.push('\n');
            }
        }
    }
    buf
}

/// Drop the trailing `#[cfg(test)] mod ...` block. The cut is on a
/// column-zero `#[cfg(test)]` FOLLOWED BY `mod`: a bare column-zero
/// `#[cfg(test)]` also guards test-only consts partway up several files, and
/// cutting at the first one silently discards production code below it.
fn cut_test_module(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate() {
        if *line == "#[cfg(test)]"
            && lines[i + 1..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| l.starts_with("mod "))
        {
            end = i;
            break;
        }
    }
    lines[..end].join("\n")
}

fn undispatched() -> BTreeSet<String> {
    let prod = production_sources();
    declared_variants()
        .into_iter()
        .filter(|v| {
            !prod.contains(&format!("NotificationEvent::{v}"))
                && !prod.contains(&format!("Self::{v}"))
        })
        .collect()
}

#[test]
fn the_set_of_unsendable_telegram_variants_only_shrinks() {
    let actual = undispatched();
    let allow: BTreeSet<String> = NO_PRODUCTION_DISPATCHER
        .iter()
        .map(|s| (*s).to_string())
        .collect();

    let added: Vec<&String> = actual.difference(&allow).collect();
    assert!(
        added.is_empty(),
        "these NotificationEvent variants have NO production constructor -- they render, \n\
         carry severity logic and tests, and can never be sent:\n  {added:?}\n\
         Wire a dispatch site, or argue the entry onto NO_PRODUCTION_DISPATCHER in review."
    );

    let wired: Vec<&String> = allow.difference(&actual).collect();
    assert!(
        wired.is_empty(),
        "these are on NO_PRODUCTION_DISPATCHER but now DO have a dispatcher -- remove them \n\
         so the list keeps shrinking:\n  {wired:?}"
    );
}

#[test]
fn the_scan_finds_a_realistic_number_of_variants() {
    let declared = declared_variants();
    assert!(
        declared.len() >= 70,
        "only {} variants parsed out of the enum -- the scanner is broken, not the enum",
        declared.len()
    );
    assert!(
        declared.iter().any(|v| v == "CrossVerify1mSummary"),
        "the parser must see a known variant"
    );
}

#[test]
fn guard_self_test() {
    // A non-module cfg(test) must not truncate production source.
    let src = "fn a() {}\n#[cfg(test)]\nconst ONLY_IN_TESTS: u8 = 1;\nfn b() {}\n#[cfg(test)]\nmod t {}\n";
    let kept = cut_test_module(src);
    assert!(
        kept.contains("fn b()"),
        "a non-module cfg(test) truncated production source"
    );
    assert!(!kept.contains("mod t"), "the test module was not cut");
}
