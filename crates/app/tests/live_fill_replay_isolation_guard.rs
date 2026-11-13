//! The offline live-fill replay (plan item LF-8) stays offline and paper only.
//!
//! The replay drives the live paper fill chain from recorded frames with a
//! scripted TEST schedule. That is safe only while four things hold, and this
//! guard fails the build when any stops holding:
//!
//! 1. No live module names the replay: outside `src/live_fill/replay/` and
//!    the binary `src/bin/tv_live_fill_replay.rs`, no file under
//!    `crates/*/src` names `live_fill::replay`, `ScriptedIntents` or
//!    `ReplayDriver`, reaches it through a grouped import
//!    (`live_fill::{.., replay}`), a `super::replay` path or a
//!    `super::{.., replay}` group, a rename (`live_fill as x`, `live_fill::{self as x}`, `replay as x`) or a glob
//!    (`live_fill::*`), and `src/live_fill/mod.rs` only declares the module.
//!    Paths are read with whitespace squeezed and raw identifiers stripped,
//!    so `live_fill :: replay`, `live_fill::r#replay` and a path broken across
//!    lines are the same path. Nor may it compile the replay's source in by
//!    file path: a `#[path = ".."]` attribute (also inside `cfg_attr`) or an
//!    `include!`, `include_str!` or `include_bytes!` whose string (escapes
//!    decoded, a `concat!` split joined) has a path component named
//!    `replay` is refused (round 6: `#[path = "live_fill/replay/scripted.rs"]
//!    mod s;` reached `parse_schedule` with no banned name).
//! 2. The live engine choice (`live_fill::select_engine`) never names the
//!    scripted schedule, the reference test double, or the replay.
//! 3. The replay and its bucket reader never delete or write to the bucket,
//!    never build or place an order, never write a database row and never
//!    page: none of the [`BANNED_IN_REPLAY`] tokens (the bucket's delete,
//!    put, copy and upload calls, the order types, the database writers, the
//!    paging types, `error!(` and `Level::ERROR`) appears in their code, and
//!    EVERY path they name under `crate::`, `super::` (or
//!    `self::super::`), a `tickvault_*` crate, `live_fill_report::`,
//!    `std::` or `tokio::` (groups expanded) is on an
//!    allowlist ([`ALLOWED_PATHS`], [`ALLOWED_FETCH_IMPORTS`]); renaming one of those
//!    roots (`use tickvault_storage as st;`) or an item under it is refused.
//!    A path spelled absolute (`::tickvault_trading::..`) is read like any
//!    other. Under `tickvault_storage` and `tickvault_common::config` only
//!    exact items are allowed, never a whole module (those modules also
//!    hold writers, table DDL, the WAL writer and `QuestDbConfig`); `questdb`
//!    is refused in any letter case; and the AWS SDK named directly
//!    ([`BANNED_SDK`], a client that could upload or reconfigure the bucket)
//!    is refused (in the bucket reader, outside its test module).
//!    A process spawn or a socket (`Command::new`, `TcpStream`, `net::`,
//!    `hyper`, `ureq`, `aws_smithy` and the like) is refused under any root.
//!    Under `tickvault_*` and `live_fill_report` the replay names exact items
//!    (`fixture::FIXTURE_COMMIT`, never `write_demo_fixture`; the engine's
//!    traits, never `ReferenceDouble`; the report's pure builders, never
//!    `read_day` or `read_seconds`). So a writer, an uploader, the order side
//!    or the live chain's start reached through a path under one of those
//!    roots is refused, and so is a spawn or a socket under any root. What is
//!    NOT read: a path under another external crate root (`chrono`,
//!    `figment`, `serde_json`, `flate2`, `sha2`, `proptest`) beyond the banned
//!    tokens; none of those writes to the bucket, the database or a broker.
//! 4. `src/main.rs` never names the replay, and never compiles its source
//!    in by file path (the same `#[path]` and `include!` check as rule 1).
//!
//! Code only: comments are stripped first by a small lexer that keeps a `//`
//! inside a string literal (a `s3://` URL), so the docs may say what the code
//! never does and code after a URL on the same line is still scanned. Each
//! rule has a bite test that plants a violation and checks the scanner
//! reports it.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `.rs` file under `dir`, recursively, sorted. O(files).
fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out.sort();
}

/// The text with comments (`//` to the end of the line, `/* .. */`)
/// removed. A small lexer: a `//` inside a string literal (a `s3://` URL),
/// a raw string or a char literal is kept, so code after it on the same line
/// is still scanned. String contents are kept (rule 4 reads one). O(len).
fn code_only(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        if ch == '/' && next == Some('/') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '/' && next == Some('*') {
            let mut depth = 0usize;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if c[i] == '\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
            }
            continue;
        }
        if ch == '\'' {
            // A char literal ('"', '\'', 'a'), or a lifetime ('a): copy it.
            let len = if next == Some('\\') {
                (i + 3..c.len().min(i + 12))
                    .find(|&j| c[j] == '\'')
                    .map_or(1, |j| j + 1 - i)
            } else if c.get(i + 2) == Some(&'\'') {
                3
            } else {
                1
            };
            out.extend(&c[i..i + len]);
            i += len;
            continue;
        }
        if ch == '"' {
            // A raw string: `r`, then `#`s, right before the quote.
            let mut k = i;
            let mut hashes = 0;
            while k > 0 && c[k - 1] == '#' {
                hashes += 1;
                k -= 1;
            }
            let raw = k > 0 && c[k - 1] == 'r' && (k < 2 || !ident(c[k - 2]) || c[k - 2] == 'b');
            out.push(ch);
            i += 1;
            while i < c.len() {
                if !raw && c[i] == '\\' {
                    out.extend(&c[i..(i + 2).min(c.len())]);
                    i += 2;
                    continue;
                }
                if c[i] == '"' && (!raw || (1..=hashes).all(|h| c.get(i + h) == Some(&'#'))) {
                    let end = i + 1 + if raw { hashes } else { 0 };
                    out.extend(&c[i..end.min(c.len())]);
                    i = end;
                    break;
                }
                out.push(c[i]);
                i += 1;
            }
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// The code with every raw identifier prefix removed (`r#replay` reads as
/// `replay`): an `r#` not after an identifier character and before an
/// identifier start. A raw string (`r#"..`, `br#"..`) is kept. O(len).
fn strip_raw_idents(code: &str) -> String {
    let c: Vec<char> = code.chars().collect();
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] == 'r'
            && c.get(i + 1) == Some(&'#')
            && c.get(i + 2).is_some_and(|n| n.is_alphabetic() || *n == '_')
            && !(i > 0 && ident(c[i - 1]))
        {
            i += 2;
            continue;
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// The code with raw identifiers stripped ([`strip_raw_idents`]), every
/// whitespace run squeezed to one space and the spaces around `::`, `{`, `}`
/// and `,` removed, so `live_fill :: replay`, `live_fill::r#replay`, a path
/// broken across lines and a spaced group all read as one path. O(len).
fn normalize(code: &str) -> String {
    let code = strip_raw_idents(code);
    let mut s = String::with_capacity(code.len());
    let mut gap = false;
    for ch in code.chars() {
        if ch.is_whitespace() {
            gap = true;
        } else {
            if gap && !s.is_empty() {
                s.push(' ');
            }
            gap = false;
            s.push(ch);
        }
    }
    for tok in ["::", "{", "}", ","] {
        s = s
            .replace(&format!(" {tok}"), tok)
            .replace(&format!("{tok} "), tok);
    }
    s
}

fn rel(p: &Path) -> String {
    p.strip_prefix(root())
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

const REPLAY_NAMES: [&str; 3] = ["live_fill::replay", "ScriptedIntents", "ReplayDriver"];

fn is_replay_home(rel_path: &str) -> bool {
    rel_path.starts_with("crates/app/src/live_fill/replay/")
        || rel_path == "crates/app/src/bin/tv_live_fill_replay.rs"
}

/// Whether `word` appears in `text` as a whole identifier. O(len).
fn has_word(text: &str, word: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(i) = text[from..].find(word) {
        let at = from + i;
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            return true;
        }
        from = at + word.len();
    }
    false
}

/// Whether the code renames a `replay` PATH item (`::replay as x`,
/// `{replay as x}`, `, replay as x`), never prose such as "replay assertion"
/// inside a string. O(len).
fn renames_replay(code: &str) -> bool {
    let mut from = 0;
    while let Some(i) = code[from..].find("replay as ") {
        let at = from + i;
        let before = code[..at].trim_end();
        if before.ends_with("::") || before.ends_with('{') || before.ends_with(',') {
            return true;
        }
        from = at + 1;
    }
    false
}

/// The text of every `live_fill::{ .. }` group (to its matching `}`). O(len).
fn live_fill_groups(code: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = code[from..].find("live_fill::{") {
        let open = from + i + "live_fill::".len();
        let mut depth = 0usize;
        let mut end = code.len();
        for (j, ch) in code[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + j + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push(&code[open..end]);
        from = end;
    }
    out
}

/// Rule 1 for one file: the replay names it holds outside the replay's home.
fn rule1_violations(rel_path: &str, text: &str) -> Vec<String> {
    if is_replay_home(rel_path) {
        return Vec::new();
    }
    let lines = code_only(text);
    let code = normalize(&lines);
    let mut v: Vec<String> = REPLAY_NAMES
        .iter()
        .filter(|n| code.contains(**n))
        .map(|n| format!("{rel_path} names `{n}`"))
        .collect();
    if live_fill_groups(&code)
        .iter()
        .any(|g| has_word(g, "replay"))
    {
        v.push(format!(
            "{rel_path} imports the replay in a `live_fill::{{..}}` group"
        ));
    }
    // `live_fill::{self as x}` renames live_fill as surely as `live_fill as x`,
    // and `x::replay` then reaches the replay.
    if live_fill_groups(&code)
        .iter()
        .any(|g| g.contains("self as "))
    {
        v.push(format!(
            "{rel_path} renames live_fill in a group (`live_fill::{{self as ..}}`)"
        ));
    }
    // `super::replay` / `self::replay` name THIS replay only inside
    // `live_fill/` (another crate's module of the same name is not it, e.g.
    // `brutex_host::replay`).
    let in_live_fill = rel_path.starts_with("crates/app/src/live_fill/");
    for shape in [
        "super::replay",
        "self::replay",
        "live_fill::*",
        "live_fill as ",
    ] {
        let scoped = shape.ends_with("::replay");
        if (!scoped || in_live_fill) && code.contains(shape) {
            v.push(format!("{rel_path} reaches the replay through `{shape}`"));
        }
    }
    // Grouped `super::{..}` / `self::{..}` imports, expanded: inside
    // `live_fill/`, `use super::{chain, replay};` or
    // `use super::{replay::drive};` reaches the replay with none of the
    // ungrouped shapes above. Leading `super::` / `self::` steps are skipped,
    // so a deeper module's `super::{super::replay}` is read too (it can only
    // over-report, never miss).
    if in_live_fill {
        for root in ["super", "self"] {
            for p in paths_under(&code, root) {
                let mut rest = p.as_str();
                while let Some(r) = rest
                    .strip_prefix("super::")
                    .or_else(|| rest.strip_prefix("self::"))
                {
                    rest = r;
                }
                if rest == "replay"
                    || rest.starts_with("replay::")
                    || rest.starts_with("replay as ")
                {
                    v.push(format!(
                        "{rel_path} reaches the replay through `{root}::{p}` (a group)"
                    ));
                }
            }
        }
    }
    if renames_replay(&code) {
        v.push(format!("{rel_path} renames the replay (`replay as ..`)"));
    }
    if rel_path == "crates/app/src/live_fill/mod.rs" {
        let decl = lines
            .lines()
            .filter(|l| l.trim() == "pub mod replay;")
            .count();
        if decl != 1 {
            v.push(format!(
                "{rel_path} must declare `pub mod replay;` exactly once"
            ));
        }
        if code.contains("replay::") || code.contains("use replay") {
            v.push(format!("{rel_path} uses the replay module"));
        }
    }
    for p in included_paths(&strip_raw_idents(&lines)) {
        if names_replay_source(&p) {
            v.push(format!(
                "{rel_path} compiles the replay's source in by file path (`{p}`)"
            ));
        }
    }
    v
}

/// The contents of every string literal in `s`, simple escapes decoded
/// (`\\`, `\"`, `\n`, `\t`, `\xNN`, `\u{..}`), so `re\x70lay` reads as
/// `replay`. A raw string's body is read up to its first `"`. O(len).
fn string_literals(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] != '"' {
            i += 1;
            continue;
        }
        let raw = i > 0 && (c[i - 1] == '#' || c[i - 1] == 'r');
        let mut lit = String::new();
        i += 1;
        while i < c.len() && c[i] != '"' {
            if c[i] == '\\' && !raw && i + 1 < c.len() {
                let e = c[i + 1];
                i += 2;
                match e {
                    'n' => lit.push('\n'),
                    't' => lit.push('\t'),
                    'x' => {
                        let hex: String = c[i..(i + 2).min(c.len())].iter().collect();
                        if let Some(ch) =
                            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            lit.push(ch);
                        }
                        i += 2;
                    }
                    'u' => {
                        let end = (i..c.len()).find(|&j| c[j] == '}').unwrap_or(c.len());
                        let hex: String = c[(i + 1).min(end)..end].iter().collect();
                        if let Some(ch) =
                            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            lit.push(ch);
                        }
                        i = end + 1;
                    }
                    '\n' => {
                        while i < c.len() && c[i].is_whitespace() {
                            i += 1;
                        }
                    }
                    other => lit.push(other),
                }
                continue;
            }
            lit.push(c[i]);
            i += 1;
        }
        i += 1;
        out.push(lit);
    }
    out
}

/// The text from the opener at `open` (`(`, `[` or `{`) to its matching
/// closer, string literals skipped so a bracket inside one does not count.
/// O(len).
fn bracketed(c: &[char], open: usize) -> String {
    let mut depth = 0usize;
    let mut i = open;
    let mut in_str = false;
    while i < c.len() {
        let ch = c[i];
        if in_str {
            if ch == '\\' {
                i += 2;
                continue;
            }
            if ch == '"' {
                in_str = false;
            }
        } else if ch == '"' {
            in_str = true;
        } else if matches!(ch, '(' | '[' | '{') {
            depth += 1;
        } else if matches!(ch, ')' | ']' | '}') {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return c[open..=i].iter().collect();
            }
        }
        i += 1;
    }
    c[open..].iter().collect()
}

/// Every file path the code pulls source in by: the string literals of each
/// attribute that names `path` (`#[path = ".."]`, also inside a
/// `#[cfg_attr(.., path = "..")]`), and of each `include!`, `include_str!`
/// and `include_bytes!` call, each literal alone and all of one call's
/// literals joined (a path split across a `concat!`). Whitespace between the
/// tokens is allowed. O(len).
fn included_paths(code: &str) -> Vec<String> {
    let c: Vec<char> = code.chars().collect();
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    let skip_ws = |mut j: usize| {
        while j < c.len() && c[j].is_whitespace() {
            j += 1;
        }
        j
    };
    let mut spans: Vec<String> = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '#' {
            let mut j = skip_ws(i + 1);
            if c.get(j) == Some(&'!') {
                j = skip_ws(j + 1);
            }
            if c.get(j) == Some(&'[') {
                let span = bracketed(&c, j);
                if has_word(&span, "path") {
                    spans.push(span);
                }
            }
        } else if ident(c[i]) && !(i > 0 && ident(c[i - 1])) {
            let end = (i..c.len()).find(|&j| !ident(c[j])).unwrap_or(c.len());
            let word: String = c[i..end].iter().collect();
            if matches!(word.as_str(), "include" | "include_str" | "include_bytes") {
                let j = skip_ws(end);
                if c.get(j) == Some(&'!') {
                    let k = skip_ws(j + 1);
                    if matches!(c.get(k), Some('(' | '[' | '{')) {
                        spans.push(bracketed(&c, k));
                    }
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    let mut out = Vec::new();
    for span in spans {
        let lits = string_literals(&span);
        if lits.len() > 1 {
            out.push(lits.concat());
        }
        out.extend(lits);
    }
    out
}

/// Whether an included path can reach the replay's source: any path
/// component named `replay` (the folder `src/live_fill/replay/`, however it
/// is reached: `live_fill/replay/x.rs`, `replay/x.rs` from inside
/// `live_fill/`, `../live_fill/./replay/`) or `replay.rs`. It can only
/// over-report, never miss. O(len).
fn names_replay_source(p: &str) -> bool {
    p.replace('\\', "/")
        .split('/')
        .any(|part| part == "replay" || part == "replay.rs")
}

/// The body of `fn name(` (from its first `{` to the matching `}`), or `None`.
fn fn_body<'a>(code: &'a str, name: &str) -> Option<&'a str> {
    let start = code.find(&format!("fn {name}("))?;
    let open = start + code[start..].find('{')?;
    let mut depth = 0usize;
    for (i, ch) in code[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&code[open..=open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Rule 2: what `select_engine`'s body names that it must not.
fn rule2_violations(mod_rs: &str) -> Vec<String> {
    let code = code_only(mod_rs);
    let Some(body) = fn_body(&code, "select_engine") else {
        return vec!["live_fill::select_engine not found".to_owned()];
    };
    ["ScriptedIntents", "ReferenceDouble", "replay"]
        .iter()
        .filter(|n| body.contains(**n))
        .map(|n| format!("select_engine names `{n}`"))
        .collect()
}

/// Matched in any letter case (`QuestDbConfig`, `QUESTDB_URL`).
const BANNED_ANY_CASE: [&str; 1] = ["questdb"];

/// The AWS SDK, named directly: a client built here could write to or
/// reconfigure the bucket (a multipart upload, a lifecycle or bucket call)
/// with none of the [`BANNED_IN_REPLAY`] names. The replay reaches the bucket
/// only through `s3_cold::S3Cold`, whose writes the compiler keeps
/// crate-private. In the bucket reader only its code before `mod tests` is
/// read (its tests build an offline SDK config).
const BANNED_SDK: [&str; 7] = [
    "aws_sdk_",
    "aws_config",
    "multipart",
    "upload_part",
    "put_bucket",
    "delete_bucket",
    "create_bucket",
];

const BANNED_IN_REPLAY: [&str; 38] = [
    // A process spawn or a raw socket reaches the bucket, the database or a
    // broker with none of the names below (the operator's AWS CLI, an ILP
    // line on port 9009). `std` and `tokio` are also allowlisted roots in
    // [`ALLOWED_PATHS`]; these catch the same items under any other crate.
    "Command::new",
    "process::Command",
    "CommandExt",
    "TcpStream",
    "TcpListener",
    "UdpSocket",
    "UnixStream",
    "UnixDatagram",
    "net::",
    "socket2",
    "hyper",
    "ureq",
    "aws_smithy",
    ".delete(",
    "::delete(",
    "delete_object",
    "copy_object",
    "put_if_absent",
    "put_object",
    "upload_segment",
    "upload_file",
    "upload_day",
    "ColdObjectStore",
    "raw_frame_upload::upload",
    "live_fill_report::upload",
    "PlaceOrderRequest",
    "OrderManagementSystem",
    "spawn_order_runtime",
    "LiveOrderEventsWriter",
    "ilp",
    "error!(",
    "Level::ERROR",
    "Notification",
    "aws_sdk_sns",
    "send_message",
    "reqwest",
    "tickvault_storage::{",
    "live_fill_report::{",
];

/// Every path root rule 3 reads, with the only paths the replay (in the app
/// crate) may name under it. A named path passes when it equals an entry or
/// lies under one (`entry::..`). Anything else under one of THESE roots is
/// refused, so a writer, an uploader, the order side or the live chain's
/// start reached through a path under them is refused, not only the tokens
/// in [`BANNED_IN_REPLAY`]; another external crate root is read only for
/// those tokens. `super` is the replay module itself (refused in its
/// `mod.rs`, where it is `live_fill`). Under `tickvault_storage`,
/// `tickvault_common::config`, `brutex_host`, `candles` and
/// `live_fill_report` only the exact items the replay uses are listed, never
/// a whole module: those modules also hold writers, table DDL
/// (`ensure_live_order_events_table`), the WAL writer and the boot replay
/// that renames segments, `QuestDbConfig`, a recipe-file writer
/// (`write_demo_fixture`), the price-reading reference double and the
/// report's database reads.
const ALLOWED_PATHS: [(&str, &[&str]); 10] = [
    (
        "crate",
        &[
            "dhan_feed_stack::fold_verdict",
            "dhan_feed_stack::live_fill_tap::TapTick",
            "dhan_feed_stack::live_fold",
            "dhan_feed_stack::unknown_packet_skip",
            "main_feed_backup::ReplayBackup",
            "main_feed_backup::read_backup_history",
            "dhan_contract_universe::ContractRow",
            "live_fill::chain",
            "live_fill::paper_rows",
            "live_fill::timing",
            "live_fill::taint",
            "live_fill::replay",
            "live_fill::BootOutcome",
            // Exact items only: `rows::read_day` and `recheck::read_seconds`
            // query the database.
            "live_fill_report::build::DailyReport",
            "live_fill_report::build::ReportContext",
            "live_fill_report::build::build_report",
            "live_fill_report::build::priced_signal_ids",
            "live_fill_report::build::rows_day_start",
            "live_fill_report::recheck::OrderInputs",
            "live_fill_report::recheck::Recheck",
            "live_fill_report::recheck::RecheckKind",
            "live_fill_report::recheck::recheck_order",
            "live_fill_report::render::csv_field",
            "live_fill_report::render::render_files",
            "live_fill_report::rows::EventRow",
        ],
    ),
    ("super", &["drive", "out", "scripted", "source", "*"]),
    (
        "tickvault_storage",
        &[
            "raw_frame_fetch::fetch_raw_frame_segment",
            "raw_frame_fetch::file_sha256_hex",
            "raw_frame_fetch::list_raw_frame_day",
            "raw_frame_fetch::is_sidecar_key",
            "raw_frame_fetch::gunzip_to",
            "raw_frame_fetch::plan_day_listing",
            "raw_frame_fetch::DayListing",
            "ws_frame_spill::SegmentRead",
            "ws_frame_spill::ReplayedFrame",
            "ws_frame_spill::WalEndpoint",
            "ws_frame_spill::WsType",
            "ws_frame_spill::WAL_RECEIPT_UNKNOWN_NANOS",
            "ws_frame_spill::frame_feeds_the_candle_fold",
            "ws_frame_spill::read_segment_frames_matching",
            "ws_frame_spill::segment_name_nanos",
            "live_order_events_persistence::LiveOrderEvent",
            "live_order_events_persistence::LiveOrderEventKind",
            "live_order_events_persistence::worse_than_model_wire",
            "live_order_events_persistence::write_csv_header",
            "live_order_events_persistence::write_csv_record",
            "s3_cold::S3Cold::load",
        ],
    ),
    (
        "tickvault_trading",
        &[
            // Exact items only: `fixture` also writes approved-status recipe
            // files (`write_demo_fixture`), and `engine` holds the
            // price-reading reference double (`ReferenceDouble`), which must
            // never be the replay's intent source.
            "brutex_host::InstrumentKey",
            "brutex_host::Refusal",
            "brutex_host::engine::StrategyEngine",
            "brutex_host::engine::BarInput",
            "brutex_host::engine::TickInput",
            "brutex_host::engine::IntentSink",
            "brutex_host::engine::OrderIntent",
            "brutex_host::engine::IntentKind",
            "brutex_host::engine::Side",
            "brutex_host::live_candles::IST_OFFSET_NS",
            "brutex_host::worst_fill::FillSchedule",
            "brutex_host::worst_fill::SecondBar",
            "brutex_host::worst_fill::rupees_to_paise",
            "brutex_host::recipe::Recipe",
            "brutex_host::recipe::RECIPE_SCHEMA",
            "brutex_host::recipe::SanityLimits",
            "brutex_host::recipe::ParamSnapshot",
            "brutex_host::fixture::FIXTURE_COMMIT",
            "candles::MultiTfAggregator",
            "candles::TfIndex",
            "candles::AGGREGATOR_MAX_SLOTS",
            "candles::LiveCandleState",
        ],
    ),
    ("tickvault_core", &["parser::types", "parser::dispatcher"]),
    (
        "tickvault_common",
        &[
            "types",
            "feed",
            "config::ApplicationConfig",
            "config::LiveFillConfig",
            "trading_calendar",
            "tick_types",
            "constants",
        ],
    ),
    ("tickvault_app", &["live_fill::replay"]),
    (
        "live_fill_report",
        &[
            "build::DailyReport",
            "build::ReportContext",
            "build::build_report",
            "build::priced_signal_ids",
            "build::rows_day_start",
            "recheck::OrderInputs",
            "recheck::Recheck",
            "recheck::RecheckKind",
            "recheck::recheck_order",
            "render::csv_field",
            "render::render_files",
            "rows::EventRow",
        ],
    ),
    // The standard library: local files, the process's own id and exit,
    // clocks and collections. Not `process::Command`, not `net`, not `os`
    // (a spawn or a socket).
    (
        "std",
        &[
            "borrow",
            "cmp",
            "collections",
            "convert",
            "env::args",
            "env::temp_dir",
            "env::var",
            "fmt",
            "fs",
            "hash",
            "io",
            "iter",
            "mem",
            "num",
            "ops",
            "path",
            "process::exit",
            "process::id",
            "result",
            "str",
            "string",
            "time",
            "vec",
        ],
    ),
    // tokio: the runtime that drives the bucket reads, nothing else (not its
    // `net`, `process` or `fs`).
    ("tokio", &["runtime"]),
];

/// The only `crate::` paths the bucket reader (`raw_frame_fetch.rs`) may
/// name: the uploader's key prefix constant, the bucket client and the WAL
/// name parser. It may name no `super::` path.
const ALLOWED_FETCH_IMPORTS: [&str; 3] = [
    "raw_frame_upload::RAW_FRAME_S3_PREFIX",
    "s3_cold::S3Cold",
    "ws_frame_spill::segment_name_nanos",
];

/// Reads one path tree at the start of `s` (normalized code), relative to
/// `prefix`, expanding `{..}` groups: `a::{b,c::{d,self}}` gives `a::b`,
/// `a::c::d` and `a::c`; a glob gives `a::*`; an item renamed with `as`
/// (other than `as _`) is returned as `<path> as <name>`. Returns the bytes
/// read. O(len).
fn read_tree(s: &str, prefix: &str, out: &mut Vec<String>) -> usize {
    let b = s.as_bytes();
    let join = |p: &str, seg: &str| {
        if p.is_empty() {
            seg.to_owned()
        } else if seg == "self" {
            p.to_owned()
        } else {
            format!("{p}::{seg}")
        }
    };
    let mut path = prefix.to_owned();
    let mut i = 0;
    loop {
        match b.get(i) {
            Some(b'{') => {
                i += 1;
                loop {
                    if b.get(i) == Some(&b'}') {
                        return i + 1;
                    }
                    let n = read_tree(&s[i..], &path, out);
                    i += n;
                    match b.get(i) {
                        Some(b',') => i += 1,
                        Some(b'}') => return i + 1,
                        _ => return i.max(1),
                    }
                }
            }
            Some(b'*') => {
                out.push(join(&path, "*"));
                return i + 1;
            }
            _ => {}
        }
        let start = i;
        while b
            .get(i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            i += 1;
        }
        path = join(&path, &s[start..i]);
        if s[i..].starts_with("::") {
            i += 2;
            continue;
        }
        if let Some(rest) = s[i..].strip_prefix(" as ") {
            let alias: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            i += 4 + alias.len();
            if alias != "_" {
                path = format!("{path} as {alias}");
            }
        }
        out.push(path);
        return i;
    }
}

/// Keywords a path can follow (`use ::x`, `return ::x::f()`, `x as ::T`).
const PATH_KEYWORDS: [&str; 19] = [
    "use", "return", "in", "as", "mut", "ref", "dyn", "impl", "where", "match", "if", "while",
    "else", "break", "let", "const", "static", "type", "pub",
];

/// Every path named under `root::` in `code` (normalized), groups expanded,
/// relative to the root. A `root::` right after an identifier character or a
/// `:` is part of a longer path and is not a root, EXCEPT a leading `::`
/// (`::tickvault_trading::..`, the absolute spelling): a `::` that does not
/// itself follow an identifier character or a `:` starts a path, so the
/// root after it is read. O(len).
fn paths_under(code: &str, root: &str) -> Vec<String> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let pat = format!("{root}::");
    let mut from = 0;
    while let Some(i) = code[from..].find(&pat) {
        let at = from + i;
        let head = &code[..at];
        let before = head.chars().next_back();
        from = at + pat.len();
        // `normalize` squeezes `use ::x` to `use::x`, so a keyword right
        // before the `::` also starts an absolute path.
        let absolute = head.strip_suffix("::").is_some_and(|h| {
            let word: String = h
                .chars()
                .rev()
                .take_while(|c| ident(*c))
                .collect::<Vec<char>>()
                .into_iter()
                .rev()
                .collect();
            PATH_KEYWORDS.contains(&word.as_str())
                || !h
                    .chars()
                    .next_back()
                    .is_some_and(|c| ident(c) || c == ':' || c == '>')
        });
        if before.is_some_and(|c| ident(c) || c == ':') && !absolute {
            continue;
        }
        let n = read_tree(&code[from..], "", &mut out);
        from += n;
    }
    out
}

/// Whether `path` equals an allowed entry or lies under one. O(entries).
fn path_allowed(path: &str, allowed: &[&str]) -> bool {
    !path.contains(" as ")
        && allowed
            .iter()
            .any(|a| path == *a || path.starts_with(&format!("{a}::")))
}

/// Rule 3 for one file of the replay or its bucket reader.
fn rule3_violations(rel_path: &str, text: &str) -> Vec<String> {
    let code = normalize(&code_only(text));
    let mut v: Vec<String> = BANNED_IN_REPLAY
        .iter()
        .filter(|n| code.contains(**n))
        .map(|n| format!("{rel_path} contains `{n}`"))
        .collect();
    let lower = code.to_lowercase();
    v.extend(
        BANNED_ANY_CASE
            .iter()
            .filter(|n| lower.contains(**n))
            .map(|n| format!("{rel_path} contains `{n}` (any case)")),
    );
    let in_storage = rel_path.starts_with("crates/storage/");
    // The bucket reader's own test module builds an offline SDK config.
    let sdk_scope = if in_storage {
        &code[..code.find("mod tests{").unwrap_or(code.len())]
    } else {
        code.as_str()
    };
    v.extend(
        BANNED_SDK
            .iter()
            .filter(|n| sdk_scope.contains(**n))
            .map(|n| format!("{rel_path} names the AWS SDK (`{n}`)")),
    );
    let in_replay_mod = rel_path.ends_with("live_fill/replay/mod.rs");
    // The replay's tests write a temporary WAL segment with the live writer.
    let in_replay_tests = rel_path.ends_with("live_fill/replay/replay_tests.rs");
    for (root, allowed) in ALLOWED_PATHS {
        // A renamed root (`use tickvault_storage as st;`) would hide every
        // path under it from the allowlist.
        if has_word(&code, &format!("{root} as")) {
            v.push(format!("{rel_path} renames `{root}`"));
        }
        let allowed: &[&str] = match root {
            "crate" if in_storage => &ALLOWED_FETCH_IMPORTS,
            // In the replay's `mod.rs`, `super` is `live_fill`.
            "super" if in_replay_mod || in_storage => &[],
            _ => allowed,
        };
        // The bucket reader's imports must match exactly: `S3Cold` names the
        // client, never a method under it.
        let exact = in_storage && root == "crate";
        let mut named = paths_under(&code, root);
        if in_storage && root == "super" {
            // In the bucket reader `super` is the crate root above its test
            // module and the reader itself inside it: only the test module's
            // `super::*` glob passes.
            let tests_at = code.find("mod tests{").unwrap_or(code.len());
            named = paths_under(&code[..tests_at], root);
            named.extend(
                paths_under(&code[tests_at..], root)
                    .into_iter()
                    .filter(|p| p != "*"),
            );
        }
        for p in named {
            let ok = if exact {
                allowed.contains(&p.as_str())
            } else {
                path_allowed(&p, allowed)
                    || (in_replay_tests
                        && root == "tickvault_storage"
                        && path_allowed(&p, &["ws_frame_spill::WsFrameSpill"]))
            };
            if !ok {
                v.push(format!("{rel_path} names `{root}::{p}`"));
            }
        }
    }
    // `self::super::..` names what `super::..` names, and `paths_under` skips
    // a `super::` after a `:`, so it read nothing: `self::super::super::super
    // ::live_fill_report::rows::read_day` passed. A `self::` path is read as a
    // root here, and one that steps up through `super` is judged by the
    // `super` allowlist (empty where `super` is: the replay's `mod.rs` and the
    // bucket reader). A `self::` path that stays inside the module (`self::x`)
    // names only the module's own children.
    let super_allowed: &[&str] = if in_replay_mod || in_storage {
        &[]
    } else {
        ALLOWED_PATHS
            .iter()
            .find(|(r, _)| *r == "super")
            .map_or(&[], |(_, a)| *a)
    };
    for p in paths_under(&code, "self") {
        let rest = if p == "super" || p.starts_with("super as ") {
            Some("")
        } else {
            p.strip_prefix("super::")
        };
        let Some(rest) = rest else {
            continue;
        };
        if rest.is_empty() || !path_allowed(rest, super_allowed) {
            v.push(format!("{rel_path} names `self::{p}`"));
        }
    }
    v
}

/// Rule 4: what `main.rs` names of the replay.
fn rule4_violations(main_rs: &str) -> Vec<String> {
    let code = strip_raw_idents(&code_only(main_rs));
    let mut v: Vec<String> = REPLAY_NAMES
        .iter()
        .chain(["tv-live-fill-replay", "tv_live_fill_replay"].iter())
        .filter(|n| code.contains(**n))
        .map(|n| format!("src/main.rs names `{n}`"))
        .collect();
    // A `#[path]` module or an `include!` of the replay's source names none
    // of the above: `#[path = "live_fill/replay/mod.rs"] mod r;` then
    // `r::run_cli(..)`.
    for p in included_paths(&code) {
        if names_replay_source(&p) {
            v.push(format!(
                "src/main.rs compiles the replay's source in by file path (`{p}`)"
            ));
        }
    }
    v
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn no_live_module_names_the_replay() {
    let mut files = Vec::new();
    for c in std::fs::read_dir(root().join("crates"))
        .expect("crates")
        .flatten()
    {
        rs_files(&c.path().join("src"), &mut files);
    }
    assert!(files.len() > 100, "the scan must see the workspace");
    let mut v = Vec::new();
    for f in &files {
        v.extend(rule1_violations(&rel(f), &read(f)));
    }
    assert!(
        v.is_empty(),
        "the replay must stay off the live path: {v:#?}"
    );
}

#[test]
fn select_engine_never_names_the_test_schedule() {
    let v = rule2_violations(&read(&root().join("crates/app/src/live_fill/mod.rs")));
    assert!(v.is_empty(), "{v:#?}");
}

#[test]
fn the_replay_never_writes_places_or_pages() {
    let mut files = Vec::new();
    rs_files(&root().join("crates/app/src/live_fill/replay"), &mut files);
    files.push(root().join("crates/app/src/bin/tv_live_fill_replay.rs"));
    files.push(root().join("crates/storage/src/raw_frame_fetch.rs"));
    assert!(files.len() >= 8, "{files:?}");
    let mut v = Vec::new();
    for f in &files {
        v.extend(rule3_violations(&rel(f), &read(f)));
    }
    assert!(v.is_empty(), "{v:#?}");
}

#[test]
fn main_never_names_the_replay() {
    let v = rule4_violations(&read(&root().join("crates/app/src/main.rs")));
    assert!(v.is_empty(), "{v:#?}");
}

// ---- bite tests: each rule reports a planted violation ----

#[test]
fn bite_rule1_a_live_module_naming_the_replay_is_caught() {
    assert!(
        !rule1_violations(
            "crates/app/src/dhan_feed_stack.rs",
            "use crate::live_fill::replay::drive;"
        )
        .is_empty()
    );
    assert!(
        !rule1_violations(
            "crates/app/src/order_runtime.rs",
            "let e = ScriptedIntents::default();"
        )
        .is_empty()
    );
    assert!(!rule1_violations("crates/core/src/x.rs", "ReplayDriver::new(").is_empty());
    assert!(
        rule1_violations("crates/app/src/x.rs", "// live_fill::replay in a comment").is_empty(),
        "comments do not count"
    );
    assert!(
        rule1_violations("crates/app/src/live_fill/replay/drive.rs", "ReplayDriver").is_empty(),
        "the replay's own home is exempt"
    );
    assert!(
        !rule1_violations("crates/app/src/live_fill/mod.rs", "pub mod chain;").is_empty(),
        "the declaration is required once"
    );
    assert!(
        !rule1_violations(
            "crates/app/src/live_fill/mod.rs",
            "pub mod replay;\nuse replay::drive;"
        )
        .is_empty()
    );
    assert!(rule1_violations("crates/app/src/live_fill/mod.rs", "pub mod replay;").is_empty());
    for planted in [
        "use crate::live_fill::{chain::PaperCmd, replay};\nlet s = replay::scripted::parse_schedule(&t);",
        "use crate::live_fill::{\n    chain,\n    replay::scripted,\n};",
        "use tickvault_app::live_fill::{replay as r};",
        "use crate::live_fill as lf;\nlf::replay::run_cli(&a);",
        "use crate::live_fill::*;\nreplay::run_cli(&a);",
        "use crate::live_fill::{self as lf};\nlf::replay::run_cli(&a);",
        "use crate::{live_fill::{self}};\nlive_fill :: replay::run_cli(&a);",
        "crate::live_fill::\n    replay::run_cli(&a);",
        "use crate::{live_fill as lf};\nlf::replay::run_cli(&a);",
    ] {
        assert!(
            !rule1_violations("crates/app/src/order_runtime_live_fill.rs", planted).is_empty(),
            "{planted}"
        );
    }
    assert!(
        rule1_violations(
            "crates/app/src/order_runtime_live_fill.rs",
            "use crate::live_fill::{chain::PaperCmd, replay_count};\nlet wal_replay = 1;"
        )
        .is_empty(),
        "other words that contain `replay` are not the module"
    );
    assert!(
        rule1_violations(
            "crates/core/src/x.rs",
            "let m = \"without a real re-dial the replay assertion below\";"
        )
        .is_empty(),
        "prose in a string is not a rename"
    );
    for path in [
        "crates/app/src/live_fill/chain.rs",
        "crates/app/src/live_fill/paper_rows.rs",
    ] {
        assert!(
            !rule1_violations(path, "let c = super::replay::run_cli(&args);").is_empty(),
            "{path}"
        );
    }
    assert!(
        rule1_violations(
            "crates/trading/src/brutex_host/loader.rs",
            "use super::replay::check;"
        )
        .is_empty(),
        "another module named `replay` elsewhere is not this one"
    );
}

#[test]
fn bite_rule2_select_engine_returning_the_schedule_is_caught() {
    let bad = "pub fn select_engine() -> Option<(X, Y)> {\n    Some((ScriptedIntents::default(), setup()))\n}";
    assert!(!rule2_violations(bad).is_empty());
    let double = "pub fn select_engine() -> Option<X> { Some(ReferenceDouble::new()) }";
    assert!(!rule2_violations(double).is_empty());
    let good = "pub fn select_engine() -> Option<X> {\n    None\n}\nfn other() { ScriptedIntents }";
    assert!(
        rule2_violations(good).is_empty(),
        "only the function's body is read"
    );
    assert!(
        !rule2_violations("fn nothing() {}").is_empty(),
        "a missing function fails loudly"
    );
}

#[test]
fn bite_rule3_a_bucket_delete_an_order_or_a_page_is_caught() {
    for planted in [
        "s3.delete(key).await?;",
        "s3.put_if_absent_bytes(k, b, h, &[]).await?;",
        "client.put_object().send().await?;",
        "let r = PlaceOrderRequest { .. };",
        "let o = OrderManagementSystem::new();",
        "spawn_order_runtime(cfg);",
        "let w = LiveOrderEventsWriter::new(&c);",
        "write_ilp_line(&mut b);",
        "let c = questdb::ingress::Sender::from_conf(x);",
        "error!(code = \"X\", \"boom\");",
        "let c = reqwest::Client::new();",
        "tickvault_storage::raw_frame_upload::upload_segment(&s3, &staging, &wal).await;",
        "crate::live_fill_report::upload::upload_day(&s3, dir, day).await;",
        "client.delete_object().bucket(b).key(k).send().await?;",
        "client.delete_objects().bucket(b).send().await?;",
        "client.copy_object().bucket(b).send().await?;",
        "let n = NotificationService::new();",
        "let c = aws_sdk_sns::Client::new(&conf);",
        "tracing::event!(Level::ERROR, \"boom\");",
        "use tickvault_storage::order_audit_persistence::OrderAuditWriter;",
        "use tickvault_storage::{raw_frame_upload, s3_cold};",
        "use crate::live_fill_report::{build, upload};",
        "fn f<S: ColdObjectStore>(s: &S) {}",
        // A renamed root hid every path under it from the allowlist.
        "use tickvault_storage as st;\nlet w = st::tick_persistence::TickWriter::new(&c);",
        "extern crate tickvault_trading as tt;",
        // A crate-private uploader in the same crate.
        "crate::instrument_snapshot_upload::spawn_snapshot_upload(cfg, s3);",
        // The real broker client.
        "tickvault_trading::oms::api_client::OrderApiClient::new(h, u, id);",
        // The live chain's start.
        "crate::live_fill::start_chain(a, b);",
        "use crate::{live_fill::{self as lf}};",
        "use crate::live_fill::{chain, paper_rows, start_chain};",
        "use super::super::order_runtime::spawn;",
        // Code after a URL on the same line is scanned.
        "let u = format!(\"s3://{b}/{k}\"); s3.wipe(&k).await?; crate::raw_frame_upload::run(x);",
        "let u = \"https://x\"; let c = tickvault_core::websocket::connect(u);",
        // The absolute spelling (a leading `::`) of a refused path.
        "let c = ::tickvault_trading::oms::api_client::OrderApiClient::new(h, u, id);",
        "let w = ::tickvault_storage::tick_persistence::TickWriter::new(&c);",
        "use ::tickvault_storage::tick_persistence::TickWriter;",
        "f(::tickvault_storage::tick_persistence::TickWriter::new(&c));",
        // The AWS SDK named directly: a write with none of the banned names.
        "let c = aws_sdk_s3::Client::new(&conf); c.create_multipart_upload().send().await?;",
        "c.upload_part().send().await?; c.complete_multipart_upload().send().await?;",
        "c.put_bucket_lifecycle_configuration().send().await?;",
        "c.delete_bucket().bucket(b).send().await?;",
        "let conf = aws_config::load_from_env().await;",
        // A whole module allowed used to pass its writers and DDL.
        "use tickvault_storage::live_order_events_persistence::ensure_live_order_events_table;",
        "ensure_live_order_events_table(&tickvault_common::config::QuestDbConfig::default()).await;",
        "let c = QuestDbConfig::default();",
        "let w = tickvault_storage::ws_frame_spill::WsFrameSpill::new(&dir);",
        "tickvault_storage::ws_frame_spill::replay_all_with_report(&dir);",
        "use tickvault_storage::live_order_events_persistence::LiveOrderEventsWriter as W;",
        "tickvault_storage::s3_cold::S3Cold::delete_key(&s, k);",
    ] {
        assert!(
            !rule3_violations("crates/app/src/live_fill/replay/x.rs", planted).is_empty(),
            "{planted}"
        );
    }
    assert!(
        rule3_violations("x.rs", "// never calls .delete( on the bucket").is_empty(),
        "docs may say what the code never does"
    );
    assert!(
        rule3_violations(
            "crates/app/src/live_fill/replay/x.rs",
            "use tickvault_storage::ws_frame_spill::SegmentRead;\n\
             use ::tickvault_storage::ws_frame_spill::ReplayedFrame;\n\
             let s = tickvault_storage::s3_cold::S3Cold::load(\"\").await;\n\
             let c: tickvault_common::config::LiveFillConfig = x;\n\
             crate::live_fill_report::build::rows_day_start(d);"
        )
        .is_empty(),
        "the allowlisted items pass, absolute spelling included"
    );
    assert!(
        rule3_violations(
            "crates/app/src/live_fill/replay/replay_tests.rs",
            "use tickvault_storage::ws_frame_spill::WsFrameSpill;\nlet s = WsFrameSpill::new(&tmp);"
        )
        .is_empty(),
        "the replay's tests may write a temporary WAL segment"
    );
    assert!(
        rule3_violations(
            "crates/app/src/live_fill/replay/x.rs",
            "let p = a::tickvault_storage::anything::X; let q = <T>::tickvault_storage::y;"
        )
        .is_empty(),
        "a root inside a longer path is not a root"
    );
    for planted in [
        "async fn f(c: &aws_sdk_s3::Client) { c.create_multipart_upload().send().await; }",
        "fn g() { let conf = aws_config::from_env(); }\nmod tests { }",
    ] {
        assert!(
            !rule3_violations("crates/storage/src/raw_frame_fetch.rs", planted).is_empty(),
            "{planted}"
        );
    }
    assert!(
        rule3_violations(
            "crates/storage/src/raw_frame_fetch.rs",
            "fn f() {}\nmod tests {\n    fn t() { let conf = aws_sdk_s3::Config::builder(); }\n}"
        )
        .is_empty(),
        "the bucket reader's tests may build an offline SDK config"
    );
    for planted in [
        "use crate::raw_frame_upload::upload_segment;",
        "use crate::raw_frame_upload::{RAW_FRAME_S3_PREFIX, upload_file};",
        "crate::s3_cold::S3Cold::delete_key(&s, k);",
        "use crate::tick_persistence::TickWriter;",
        "let u = format!(\"s3://{b}/{k}\"); s3.delete(&k).await?;",
        "let u = format!(\"s3://{b}/{k}\"); crate::s3_cold::S3Cold::wipe(&s3, &k);",
        "use super::s3_cold::S3Cold;",
        "use super::*;\ns3_cold::S3Cold::wipe(&s, k);",
        "mod tests {\n    use super::super::s3_cold::S3Cold;\n}",
    ] {
        assert!(
            !rule3_violations("crates/storage/src/raw_frame_fetch.rs", planted).is_empty(),
            "{planted}"
        );
    }
    assert!(
        rule3_violations(
            "crates/storage/src/raw_frame_fetch.rs",
            "use crate::raw_frame_upload::RAW_FRAME_S3_PREFIX;\nuse crate::s3_cold::S3Cold;\n\
             crate::ws_frame_spill::segment_name_nanos(n);"
        )
        .is_empty(),
        "the fetch's own imports pass"
    );
    assert!(
        rule3_violations(
            "crates/app/src/live_fill/replay/x.rs",
            "use crate::live_fill::{chain::PaperCmd, timing::{SESSION_OPEN_SOD, bar_in_session}};\n\
             use super::*;\nuse super::drive::{ReplayDriver, tail_ns};\nuse std::io::Write as _;\n\
             use tickvault_trading::brutex_host::{InstrumentKey, Refusal};\n\
             let c = '\"'; let u = r#\"s3://a//b\"#; let l: &'static str = \"x\";"
        )
        .is_empty(),
        "allowed groups, globs of the replay module, char literals and raw strings pass"
    );
    let kept = code_only("let u = \"s3://x\"; y(); // gone\n/* gone */ z();");
    assert!(kept.contains("y();") && kept.contains("z();") && !kept.contains("gone"));
}

#[test]
fn bite_rule4_main_naming_the_replay_is_caught() {
    assert!(!rule4_violations("tickvault_app::live_fill::replay::run_cli(&args);").is_empty());
    assert!(!rule4_violations("Command::new(\"tv-live-fill-replay\")").is_empty());
    assert!(rule4_violations("// the WAL replay runs at boot\nreplay_all(dir)?;").is_empty());
}

/// Round 4 (LF-8 review): rule 3 used to read only `crate::`, `super::`,
/// `tickvault_*` and `live_fill_report::` paths, so a process spawn or a raw
/// socket in replay code passed, and whole modules were allowed where the
/// replay needs single items (`write_demo_fixture`, `ReferenceDouble`,
/// `read_day` and `read_seconds` passed). Rule 1 and rule 4 missed a raw
/// identifier (`r#replay`).
#[test]
fn bite_round4_spawns_sockets_whole_modules_and_raw_identifiers_are_caught() {
    for planted in [
        // A process spawn: the operator's AWS CLI deleting bucket objects.
        "std::process::Command::new(\"aws\").args([\"s3\",\"rm\",\"s3://tv-prod-cold/raw-frames/2026-11-18/\",\"--recursive\"]).status();",
        // Written as rustfmt lays a long group out (the scanner squeezes the
        // whitespace); the one-line group spelling also trips
        // `rust_only_guard`'s workspace-wide ban on renaming `Command`.
        "use std::process::{\n    Command as C,\n};\nC::new(\"aws\");",
        "use std::process;\nprocess::Command::new(\"sh\");",
        "let c = tokio::process::Command::new(\"aws\");",
        "use std::os::unix::process::CommandExt;",
        // A raw socket writing a database row.
        "let mut s = std::net::TcpStream::connect(\"127.0.0.1:9009\")?;\ns.write_all(b\"ticks,feed=dhan x=1i\\n\")?;",
        "use std::net::{UdpSocket};",
        "let s = tokio::net::TcpStream::connect(a).await?;",
        "use std::os::unix::net::UnixStream;",
        "let c = hyper::Client::new();",
        "let r = ureq::get(u).call();",
        "let c = aws_smithy_runtime::client::http::Client::new();",
        // Whole modules: only the exact items the replay uses pass.
        "tickvault_trading::brutex_host::fixture::write_demo_fixture(&out_dir)?;",
        "let e = tickvault_trading::brutex_host::engine::ReferenceDouble::new();",
        "use tickvault_trading::brutex_host::engine::*;",
        "crate::live_fill_report::rows::read_day(\"http://127.0.0.1:9000/exec\", d).await;",
        "crate::live_fill_report::recheck::read_seconds(u, &i).await;",
        "use crate::live_fill_report::rows::{EventRow, read_day};",
        "live_fill_report::recheck::read_seconds(u, &i).await;",
    ] {
        assert!(
            !rule3_violations("crates/app/src/live_fill/replay/x.rs", planted).is_empty(),
            "{planted}"
        );
    }
    assert!(
        rule3_violations(
            "crates/app/src/live_fill/replay/x.rs",
            "use std::io::Write as _;\nstd::fs::write(p, b)?;\nstd::process::exit(c);\n\
             let rt = tokio::runtime::Builder::new_current_thread();\n\
             use std::collections::{BTreeMap, HashMap};\n\
             use tickvault_trading::brutex_host::fixture::FIXTURE_COMMIT;\n\
             use tickvault_trading::brutex_host::engine::{BarInput, IntentSink, Side};\n\
             use crate::live_fill_report::rows::EventRow;\n\
             let r = crate::live_fill_report::build::build_report(x);"
        )
        .is_empty(),
        "the items the replay uses pass"
    );
    for planted in [
        "crate::live_fill::r#replay::run_cli(&a);",
        "use crate::live_fill::{chain, r#replay};",
    ] {
        assert!(
            !rule1_violations("crates/app/src/main.rs", planted).is_empty(),
            "{planted}"
        );
    }
    assert!(!rule4_violations("tickvault_app::live_fill::r#replay::run_cli(&args);").is_empty());
    assert!(
        rule1_violations(
            "crates/app/src/x.rs",
            "let u = r#\"live_fill-replay\"#; let b = br#\"x\"#;"
        )
        .is_empty(),
        "a raw string is not a raw identifier"
    );
}

/// Round 5 (LF-8 review): rule 3 read no path that began `self::`, so
/// `self::super::..` (the same item as `super::..`) passed for a database
/// read or an order-side call; rule 1 caught `super::replay` but not a
/// grouped `use super::{chain, replay};` in a direct child of `live_fill`.
#[test]
fn bite_round5_self_super_paths_and_grouped_super_imports_are_caught() {
    for planted in [
        "use self::super::super::super::live_fill_report::rows::read_day;",
        "self::super::super::super::order_runtime_live_fill::retry_live_fill_exits(x);",
        "use self::{super::super::super::live_fill_report::rows::read_day};",
        "use self::super as up;",
    ] {
        assert!(
            !rule3_violations("crates/app/src/live_fill/replay/drive.rs", planted).is_empty(),
            "{planted}"
        );
    }
    assert!(
        !rule3_violations(
            "crates/app/src/live_fill/replay/mod.rs",
            "use self::super::chain::LiveFillChain;"
        )
        .is_empty(),
        "in the replay's mod.rs `self::super` is live_fill"
    );
    assert!(
        rule3_violations(
            "crates/app/src/live_fill/replay/out.rs",
            "use self::super::drive::ReplayOutput;\nuse self::helper::x;\nself::f();"
        )
        .is_empty(),
        "`self::super` to a sibling the replay may name, and `self::` children, pass"
    );
    for planted in [
        "use super::{chain, replay};\nfn f(){ replay::run_cli(&[]); }",
        "use super::{replay::drive};",
        "use super::{chain,\n    replay::scripted::parse_schedule};",
        "use self::super::{replay};",
        "use super::{replay as r};",
    ] {
        for path in [
            "crates/app/src/live_fill/taint.rs",
            "crates/app/src/live_fill/chain.rs",
        ] {
            assert!(
                !rule1_violations(path, planted).is_empty(),
                "{path}: {planted}"
            );
        }
    }
    assert!(
        rule1_violations(
            "crates/app/src/live_fill/taint.rs",
            "use super::{chain, timing::{SESSION_OPEN_SOD}, replay_count};"
        )
        .is_empty(),
        "a group naming other live_fill modules passes"
    );
    assert!(
        rule1_violations(
            "crates/trading/src/brutex_host/loader.rs",
            "use super::{replay, x};"
        )
        .is_empty(),
        "another module named `replay` elsewhere is not this one"
    );
}

/// Round 6 (LF-8 review): a live module could compile the replay's source in
/// by file path, `#[path = "live_fill/replay/scripted.rs"] mod s;` or
/// `include!("live_fill/replay/mod.rs")`, and call `parse_schedule` or
/// `run_cli` without writing any banned name; rule 1 and rule 4 read only
/// Rust paths, so every form below passed. They are read now, each with its
/// path in any spelling (`cfg_attr`, whitespace, a `concat!` split, an
/// escaped character, a Windows separator).
#[test]
fn bite_round6_path_attributes_and_includes_of_the_replay_are_caught() {
    for (path, planted) in [
        (
            "crates/app/src/order_runtime.rs",
            "#[path = \"live_fill/replay/scripted.rs\"] mod sched;\nfn f(t: &str) { let _ = sched::parse_schedule(t); }",
        ),
        (
            "crates/app/src/foo.rs",
            "mod sched { include!(\"live_fill/replay/mod.rs\"); }",
        ),
        (
            "crates/app/src/lib.rs",
            "#[path = \"live_fill/replay/mod.rs\"]\npub mod tools;",
        ),
        (
            "crates/app/src/live_fill/chain.rs",
            "#[path = \"replay/scripted.rs\"] mod s;",
        ),
        (
            "crates/app/src/live_fill/mod.rs",
            "pub mod replay;\n#[path = \"./replay/drive.rs\"] mod d;",
        ),
        (
            "crates/app/src/order_runtime.rs",
            "# [ cfg_attr ( test , path = \"live_fill/replay/scripted.rs\" ) ] mod s;",
        ),
        (
            "crates/app/src/order_runtime.rs",
            "include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/live_fill/\", \"replay\", \"/mod.rs\"));",
        ),
        (
            "crates/app/src/order_runtime.rs",
            "include ! (\"live_fill/re\\x70lay/mod.rs\");",
        ),
        (
            "crates/app/src/order_runtime.rs",
            "const S: &str = include_str!(\"live_fill\\\\replay\\\\scripted.rs\");",
        ),
        (
            "crates/core/src/x.rs",
            "const B: &[u8] = include_bytes!(r#\"../../app/src/live_fill/replay/mod.rs\"#);",
        ),
    ] {
        assert!(
            !rule1_violations(path, planted).is_empty(),
            "{path}: {planted}"
        );
    }
    assert!(
        !rule4_violations(
            "#[path = \"live_fill/replay/mod.rs\"] mod r;\nfn main() { r::run_cli(&[]); }"
        )
        .is_empty()
    );
    assert!(!rule4_violations("mod r { include!(\"live_fill/replay/mod.rs\"); }").is_empty());
    // What the tree already holds is not reported: a test reading its own
    // file, a sibling or a file of another crate by path, and the replay's
    // own `#[path]` test module inside its home.
    for (path, ok) in [
        (
            "crates/app/src/dhan_rest_stack.rs",
            "let src = include_str!(\"dhan_rest_stack.rs\");",
        ),
        (
            "crates/storage/src/wal_replay_frame_cap.rs",
            "let src = include_str!(\"ws_frame_spill.rs\");",
        ),
        (
            "crates/app/src/x.rs",
            "let tf = include_str!(\"../../../deploy/aws/terraform/error-code-alarms.tf\");",
        ),
        (
            "crates/app/src/live_fill/replay/mod.rs",
            "#[cfg(test)]\n#[path = \"replay_tests.rs\"]\nmod tests;",
        ),
        (
            "crates/app/src/x.rs",
            "// #[path = \"live_fill/replay/mod.rs\"] mod r; in a comment",
        ),
        (
            "crates/app/src/x.rs",
            "let path = \"live_fill/replay\"; let replay_dir = path;",
        ),
    ] {
        assert!(rule1_violations(path, ok).is_empty(), "{path}: {ok}");
    }
    assert!(rule4_violations("let src = include_str!(\"main.rs\");").is_empty());
}
