//! Config keys that look like switches and control nothing.
//!
//! No config struct in this workspace uses `#[serde(deny_unknown_fields)]` —
//! **zero occurrences** — so any key that matches no field is silently
//! discarded at load. The key stays in the file, reads like a live control, and
//! does nothing. This branch already fixed exactly one instance of that
//! (`groww_enabled = false` in `production.toml`, a switch for a field that no
//! longer existed); the CLASS was never swept.
//!
//! Sweeping it found **10 more**, and three of them are in `production.toml`:
//!
//! ```toml
//! [network]
//! # Static IP verification mandatory for order APIs (SEBI April 2026)
//! ip_verification_enabled = true
//! ip_check_interval_secs  = 300
//!
//! [risk]
//! auto_halt_on_error = true
//! ```
//!
//! An operator auditing that file would reasonably conclude a regulatory IP
//! control is switched on and errors auto-halt trading. Neither key is read by
//! anything. The IP verifier itself was retired in July — it has no production
//! callers — so `= true` is not merely ignored, it names a mechanism that no
//! longer exists.
//!
//! **What this test does and does not do.** It does NOT delete the keys and it
//! does NOT add `deny_unknown_fields`. The second is the real fix and is
//! deliberately not taken here: it changes boot behaviour, and a stale key
//! would stop the app starting rather than be ignored — a fine trade, but one
//! that belongs in its own change with the operator's eyes on it, not smuggled
//! into an audit. What it does is stop the set GROWING silently, and put the
//! ten on the record where the next reader trips over them.
//!
//! The re-wire path is real for at least one of them: the boot IP gate is
//! documented as needing to be re-armed before live orders. That is an argument
//! for keeping the key and knowing it is inert — not for pretending it works.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common -> crates -> repo root")
        .to_path_buf()
}

/// Keys known to have no reader, each with why it is still in the file.
///
/// This list may SHRINK freely — wiring a key up or deleting it is progress.
/// Growing it means a new dead switch was written, and that needs saying out
/// loud in the same change.
const KNOWN_INERT: &[(&str, &str)] = &[
    (
        "ip_verification_enabled",
        "production.toml [network], comment cites a SEBI mandate. The IP \
         verifier was retired 2026-07-13 and has no production callers, so \
         `= true` names a mechanism that does not exist. Documented as needing \
         re-arming before live orders.",
    ),
    (
        "ip_check_interval_secs",
        "production.toml [network], same retired verifier.",
    ),
    (
        "auto_halt_on_error",
        "production.toml [risk]. Reads as 'halt trading automatically on \
         error'. Nothing reads it. The risk engine's real halt paths are its \
         own typed breaches, not this key.",
    ),
    (
        "morning_run_ist",
        "base.toml [cross_verify], schedule for a comparator that no longer runs.",
    ),
    (
        "intraday_run_ist",
        "base.toml [cross_verify], same comparator.",
    ),
    (
        "timeframes_intraday",
        "base.toml [cross_verify], same comparator.",
    ),
    (
        "tolerance_price",
        "base.toml [cross_verify], same comparator.",
    ),
    (
        "tolerance_volume",
        "base.toml [cross_verify], same comparator.",
    ),
    (
        "block_trading_on_mismatch",
        "base.toml [cross_verify]. Reads as a trading kill-switch driven by \
         comparator disagreement. Nothing reads it.",
    ),
    (
        "history_repull_enabled",
        "base.toml [cadence], re-pull arm removed with the cross-fill path.",
    ),
];

const CONFIG_FILES: &[&str] = &[
    "config/base.toml",
    "config/production.toml",
    "config/local.toml",
];

/// Every `key = value` name declared in the config files.
fn declared_keys() -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for rel in CONFIG_FILES {
        let Ok(body) = std::fs::read_to_string(repo_root().join(rel)) else {
            continue;
        };
        for line in body.lines() {
            let t = line.trim();
            if t.starts_with('#') || t.starts_with('[') {
                continue;
            }
            let Some((lhs, _)) = t.split_once('=') else {
                continue;
            };
            let name = lhs.trim();
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                keys.insert(name.to_string());
            }
        }
    }
    keys
}

/// Concatenated production source (never tests — a key mentioned only by a
/// test is not read by the running program).
fn production_source() -> String {
    let mut out = String::new();
    let mut stack = vec![repo_root().join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if !matches!(n, "target" | "tests" | "benches" | "fuzz") {
                    stack.push(p);
                }
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) == Some("rs")
                && let Ok(body) = std::fs::read_to_string(&p)
            {
                out.push_str(&body);
            }
        }
    }
    out
}

fn has_reader(src: &str, key: &str) -> bool {
    // Word-boundary match: `foo` must not be satisfied by `foo_bar`.
    src.match_indices(key).any(|(i, _)| {
        let before = src[..i].chars().next_back();
        let after = src[i + key.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        boundary(before) && boundary(after)
    })
}

#[test]
fn no_new_config_key_is_silently_ignored() {
    let src = production_source();
    assert!(
        src.len() > 100_000,
        "production source scan returned {} bytes — it is broken, and a broken \
         scan would report every key as unread",
        src.len()
    );

    let known: BTreeSet<&str> = KNOWN_INERT.iter().map(|(k, _)| *k).collect();
    let mut new_inert = Vec::new();
    for key in declared_keys() {
        if !has_reader(&src, &key) && !known.contains(key.as_str()) {
            new_inert.push(key);
        }
    }

    assert!(
        new_inert.is_empty(),
        "these config keys have NO reader in production source:\n  {}\n\n\
         No config struct sets serde(deny_unknown_fields), so a key that \
         matches no field is silently discarded at load — it stays in the file \
         reading like a live control and does nothing. If the key is meant to \
         work, wire it. If it is a leftover, delete it. If it is deliberately \
         inert pending a re-wire, add it to KNOWN_INERT with the reason.",
        new_inert.join("\n  ")
    );
}

#[test]
fn the_known_inert_list_only_shrinks() {
    let src = production_source();
    let revived: Vec<&str> = KNOWN_INERT
        .iter()
        .filter(|(k, _)| has_reader(&src, k))
        .map(|(k, _)| *k)
        .collect();
    assert!(
        revived.is_empty(),
        "these keys are listed as inert but now HAVE a reader — good news, and \
         the list must shrink in the same change so it does not keep claiming \
         a live control is dead:\n  {}",
        revived.join("\n  ")
    );
}

#[test]
fn the_retired_ip_gate_is_not_advertised_as_a_live_control() {
    // The narrowest, loudest case: production config asserting a regulatory
    // control is enabled, for a verifier with no production callers.
    let src = production_source();
    assert!(
        !has_reader(&src, "ip_verification_enabled"),
        "ip_verification_enabled now has a reader. If the boot IP gate was \
         re-armed, remove this test and its KNOWN_INERT entry together — do not \
         leave the workspace claiming the gate is still dead."
    );
    let prod = std::fs::read_to_string(repo_root().join("config/production.toml"))
        .expect("production.toml must exist");
    assert!(
        prod.contains("ip_verification_enabled"),
        "ip_verification_enabled left production.toml. That is a fine outcome, \
         but delete its KNOWN_INERT entry and this test in the same change."
    );
}

#[test]
fn guard_self_test() {
    let src = production_source();
    assert!(
        has_reader(&src, "security_id"),
        "word-boundary matcher cannot find a key that is demonstrably read"
    );
    assert!(
        !has_reader(&src, "definitely_not_a_config_key_anywhere"),
        "word-boundary matcher invents readers for keys that do not exist"
    );
    // A prefix must NOT satisfy a longer key, or every short key looks read.
    assert!(
        !has_reader("let questdb_host = 1;", "questdb"),
        "matcher treats `questdb` as read because `questdb_host` exists — it \
         would mark almost every key as having a reader"
    );
}
