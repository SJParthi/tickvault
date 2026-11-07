//! Production-only line coverage, per crate (2026-11-07 audit, tests-coverage).
//!
//! `scripts/coverage-gate.sh` gives every file under `crates/<name>/` to that
//! crate and counts every coverable line, so the test code that lives inside
//! `src` (inline `#[cfg(test)] mod` blocks and the `*_tests.rs` files they
//! load) counts as covered production code. Test code runs almost fully under
//! the tests, so the crate percentages the floors compare against are higher
//! than the share of PRODUCTION lines the tests reach.
//!
//! This tool reads the same `cargo llvm-cov --json` export the gate reads and
//! prints, per crate, the line coverage of production code only. It is a
//! REPORT: it gates nothing and the floors in
//! `quality/crate-coverage-thresholds.toml` are unchanged. It is printed next
//! to them so the gap is visible.
//!
//! Usage: `coverage_prod_report <coverage.json> [crate-coverage-thresholds.toml]`
//!
//! It uses std only, so CI compiles it with one `rustc` call instead of
//! building common's dependency tree a second time in a job that has run out
//! of runner disk before (see the note in `ci.yml`).
//!
//! What counts as TEST code:
//! - a whole file named `tests.rs` or `*_tests.rs`, or under `crates/<x>/tests/`;
//! - a whole file loaded by a top-level `#[cfg(test)] mod name;` (with or
//!   without `#[path]`), and every file such a test file loads in turn;
//! - in any other file, every line from a top-level `#[cfg(test)]` (or
//!   `#[cfg(all(test, ..))]`) attribute on a `mod name { .. }` to that
//!   module's closing brace, found by brace matching over a lexer that skips
//!   strings, raw strings, char literals and comments.
//!
//! Per-line counts come from each file's llvm-cov `segments`, using llvm-cov's
//! own line rule (`LineCoverageStats`), so each line reads as `llvm-cov show`
//! shows it (pinned by a test on a real export). llvm-cov's `summary.lines`,
//! which the gate sums, adds each FUNCTION's lines instead, so a line shared by
//! a function and a closure inside it counts twice there. The table therefore
//! prints the gate's figure and the per-line figure side by side, and the
//! production share is compared with the per-line one.
//!
//! HONEST LIMITS: a `#[cfg(test)]` module nested inside another module, and a
//! `#[cfg(test)]` fn, impl or use at the top level, count as production. A
//! source file that cannot be read counts as production and is listed.

#![cfg_attr(not(test), deny(unsafe_code))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// A small JSON reader for the llvm-cov export. It decodes only the fields
// this report needs and skips every other value without building it.
// ---------------------------------------------------------------------------

/// One llvm-cov segment: `[line, col, count, has_count, is_region_entry, is_gap_region]`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Segment {
    line: u64,
    count: u64,
    has_count: bool,
    is_region_entry: bool,
    is_gap_region: bool,
}

/// One file of the export.
#[derive(Debug, Clone, PartialEq, Default)]
struct FileRecord {
    filename: String,
    segments: Option<Vec<Segment>>,
    summary_count: u64,
    summary_covered: u64,
}

struct JsonReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn error(&self, what: &str) -> String {
        format!("coverage JSON: {what} at byte {}", self.pos)
    }

    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.bytes.get(self.pos) {
            if matches!(byte, b' ' | b'\n' | b'\r' | b'\t') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_whitespace();
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, wanted: u8) -> Result<(), String> {
        if self.peek() == Some(wanted) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected '{}'", wanted as char)))
        }
    }

    /// After a value inside a container: true when another element follows,
    /// false at the closing byte (which is consumed).
    fn next_element(&mut self, close: u8) -> Result<bool, String> {
        match self.peek() {
            Some(b',') => {
                self.pos += 1;
                Ok(true)
            }
            Some(byte) if byte == close => {
                self.pos += 1;
                Ok(false)
            }
            _ => Err(self.error("expected ',' or a closing bracket")),
        }
    }

    /// Opens a container; returns false (and consumes the close) when empty.
    fn open(&mut self, open: u8, close: u8) -> Result<bool, String> {
        self.expect(open)?;
        if self.peek() == Some(close) {
            self.pos += 1;
            return Ok(false);
        }
        Ok(true)
    }

    /// Raw bytes of a string, escapes left in place (used for object keys).
    fn raw_string(&mut self) -> Result<&'a [u8], String> {
        self.expect(b'"')?;
        let start = self.pos;
        loop {
            match self.bytes.get(self.pos) {
                None => return Err(self.error("unterminated string")),
                Some(b'\\') => self.pos += 2,
                Some(b'"') => {
                    let raw = self.bytes.get(start..self.pos).unwrap_or_default();
                    self.pos += 1;
                    return Ok(raw);
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// A decoded string value.
    fn string(&mut self) -> Result<String, String> {
        let raw = self.raw_string()?;
        let mut out = Vec::with_capacity(raw.len());
        let mut index = 0;
        while let Some(&byte) = raw.get(index) {
            if byte != b'\\' {
                out.push(byte);
                index += 1;
                continue;
            }
            let escaped = raw.get(index + 1).copied().unwrap_or(b'\\');
            index += 2;
            match escaped {
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'r' => out.push(b'\r'),
                b'b' => out.push(0x08),
                b'f' => out.push(0x0c),
                b'u' => {
                    let hex = raw
                        .get(index..index + 4)
                        .and_then(|digits| std::str::from_utf8(digits).ok())
                        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                        .ok_or_else(|| self.error("bad \\u escape"))?;
                    index += 4;
                    let decoded = char::from_u32(hex).unwrap_or(char::REPLACEMENT_CHARACTER);
                    let mut buffer = [0_u8; 4];
                    out.extend_from_slice(decoded.encode_utf8(&mut buffer).as_bytes());
                }
                other => out.push(other),
            }
        }
        String::from_utf8(out).map_err(|_| self.error("string is not UTF-8"))
    }

    fn unsigned(&mut self) -> Result<u64, String> {
        self.skip_whitespace();
        let start = self.pos;
        while let Some(byte) = self.bytes.get(self.pos) {
            if byte.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        std::str::from_utf8(self.bytes.get(start..self.pos).unwrap_or_default())
            .ok()
            .and_then(|digits| digits.parse::<u64>().ok())
            .ok_or_else(|| self.error("expected an unsigned integer"))
    }

    fn boolean(&mut self) -> Result<bool, String> {
        self.skip_whitespace();
        let rest = self.bytes.get(self.pos..).unwrap_or_default();
        if rest.starts_with(b"true") {
            self.pos += 4;
            Ok(true)
        } else if rest.starts_with(b"false") {
            self.pos += 5;
            Ok(false)
        } else {
            Err(self.error("expected a boolean"))
        }
    }

    /// Skips any value without building it.
    fn skip_value(&mut self) -> Result<(), String> {
        match self.peek() {
            Some(b'"') => self.raw_string().map(|_| ()),
            Some(b'{') | Some(b'[') => {
                let mut depth = 0_usize;
                loop {
                    match self.bytes.get(self.pos) {
                        None => return Err(self.error("unterminated container")),
                        Some(b'"') => {
                            self.raw_string()?;
                            continue;
                        }
                        Some(b'{') | Some(b'[') => depth += 1,
                        Some(b'}') | Some(b']') => {
                            depth -= 1;
                            if depth == 0 {
                                self.pos += 1;
                                return Ok(());
                            }
                        }
                        Some(_) => {}
                    }
                    self.pos += 1;
                }
            }
            Some(_) => {
                while let Some(byte) = self.bytes.get(self.pos) {
                    if matches!(byte, b',' | b'}' | b']') || byte.is_ascii_whitespace() {
                        break;
                    }
                    self.pos += 1;
                }
                Ok(())
            }
            None => Err(self.error("expected a value")),
        }
    }

    /// Walks an object, calling `field` with each key; `field` must consume
    /// the value (or return false to have it skipped).
    fn object(
        &mut self,
        mut field: impl FnMut(&mut Self, &[u8]) -> Result<bool, String>,
    ) -> Result<(), String> {
        if !self.open(b'{', b'}')? {
            return Ok(());
        }
        loop {
            let key = self.raw_string()?;
            self.expect(b':')?;
            if !field(self, key)? {
                self.skip_value()?;
            }
            if !self.next_element(b'}')? {
                return Ok(());
            }
        }
    }

    fn array(
        &mut self,
        mut item: impl FnMut(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.open(b'[', b']')? {
            return Ok(());
        }
        loop {
            item(self)?;
            if !self.next_element(b']')? {
                return Ok(());
            }
        }
    }

    fn segment(&mut self) -> Result<Segment, String> {
        self.expect(b'[')?;
        let line = self.unsigned()?;
        self.expect(b',')?;
        self.unsigned()?; // column
        self.expect(b',')?;
        let count = self.unsigned()?;
        self.expect(b',')?;
        let has_count = self.boolean()?;
        self.expect(b',')?;
        let is_region_entry = self.boolean()?;
        // Older exports end here; newer ones add is_gap_region.
        let mut is_gap_region = false;
        if self.peek() == Some(b',') {
            self.pos += 1;
            is_gap_region = self.boolean()?;
            while self.peek() == Some(b',') {
                self.pos += 1;
                self.skip_value()?;
            }
        }
        self.expect(b']')?;
        Ok(Segment {
            line,
            count,
            has_count,
            is_region_entry,
            is_gap_region,
        })
    }

    fn file(&mut self) -> Result<FileRecord, String> {
        let mut record = FileRecord::default();
        self.object(|reader, key| match key {
            b"filename" => {
                record.filename = reader.string()?;
                Ok(true)
            }
            b"segments" => {
                let mut segments = Vec::new();
                reader.array(|reader| {
                    segments.push(reader.segment()?);
                    Ok(())
                })?;
                record.segments = Some(segments);
                Ok(true)
            }
            b"summary" => {
                reader.object(|reader, key| {
                    if key != b"lines" {
                        return Ok(false);
                    }
                    reader.object(|reader, key| match key {
                        b"count" => {
                            record.summary_count = reader.unsigned()?;
                            Ok(true)
                        }
                        b"covered" => {
                            record.summary_covered = reader.unsigned()?;
                            Ok(true)
                        }
                        _ => Ok(false),
                    })?;
                    Ok(true)
                })?;
                Ok(true)
            }
            _ => Ok(false),
        })?;
        Ok(record)
    }
}

/// Every file of every `data` entry of an llvm-cov export.
fn parse_export(bytes: &[u8]) -> Result<Vec<FileRecord>, String> {
    let mut reader = JsonReader::new(bytes);
    let mut files = Vec::new();
    reader.object(|reader, key| {
        if key != b"data" {
            return Ok(false);
        }
        reader.array(|reader| {
            reader.object(|reader, key| {
                if key != b"files" {
                    return Ok(false);
                }
                reader.array(|reader| {
                    files.push(reader.file()?);
                    Ok(())
                })?;
                Ok(true)
            })
        })?;
        Ok(true)
    })?;
    Ok(files)
}

// ---------------------------------------------------------------------------
// Per-line coverage from segments, the rule llvm-cov uses for its summary
// (`LineCoverageStats` / `LineCoverageIterator` in CoverageMapping.cpp).
// ---------------------------------------------------------------------------

/// One coverable ("mapped") line and whether it ran.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LineStat {
    line: u64,
    covered: bool,
}

fn is_start_of_region(segment: &Segment) -> bool {
    !segment.is_gap_region && segment.has_count && segment.is_region_entry
}

/// The mapped lines of a file, from its segments (sorted by position, as
/// llvm-cov exports them).
fn line_stats(segments: &[Segment]) -> Vec<LineStat> {
    let mut stats = Vec::new();
    let Some(first) = segments.first() else {
        return stats;
    };
    let mut line = first.line;
    let mut next = 0_usize;
    let mut wrapped: Option<Segment> = None;
    let mut on_line: Vec<Segment> = Vec::new();
    while next < segments.len() {
        if let Some(last) = on_line.last() {
            wrapped = Some(*last);
        }
        on_line.clear();
        while let Some(segment) = segments.get(next) {
            if segment.line != line {
                break;
            }
            on_line.push(*segment);
            next += 1;
        }
        let region_starts = on_line.iter().filter(|s| is_start_of_region(s)).count();
        let starts_skipped = on_line
            .first()
            .is_some_and(|s| !s.has_count && s.is_region_entry);
        let mut mapped =
            !starts_skipped && (wrapped.is_some_and(|w| w.has_count) || region_starts > 0);
        mapped |= on_line.iter().any(|s| s.is_region_entry && s.has_count);
        if mapped {
            let mut count = wrapped.map_or(0, |w| w.count);
            if region_starts > 0 {
                for segment in on_line.iter().filter(|s| is_start_of_region(s)) {
                    count = count.max(segment.count);
                }
            }
            stats.push(LineStat {
                line,
                covered: count > 0,
            });
        }
        line += 1;
    }
    stats
}

// ---------------------------------------------------------------------------
// Test-code detection in Rust source.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Punct(u8),
    Str(String),
    Other,
}

/// Tokens with their 1-based line. Comments, char literals and lifetimes are
/// dropped; strings keep their raw text.
fn tokenize(source: &str) -> Vec<(Token, u64)> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut pos = 0_usize;
    let mut line = 1_u64;
    let count_lines = |slice: &[u8]| slice.iter().filter(|b| **b == b'\n').count() as u64;
    while let Some(&byte) = bytes.get(pos) {
        let rest = bytes.get(pos..).unwrap_or_default();
        if byte == b'\n' {
            line += 1;
            pos += 1;
        } else if byte.is_ascii_whitespace() {
            pos += 1;
        } else if rest.starts_with(b"//") {
            while bytes.get(pos).is_some_and(|b| *b != b'\n') {
                pos += 1;
            }
        } else if rest.starts_with(b"/*") {
            let mut depth = 0_usize;
            loop {
                let here = bytes.get(pos..).unwrap_or_default();
                if here.is_empty() {
                    break;
                }
                if here.starts_with(b"/*") {
                    depth += 1;
                    pos += 2;
                } else if here.starts_with(b"*/") {
                    depth -= 1;
                    pos += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if here.first() == Some(&b'\n') {
                        line += 1;
                    }
                    pos += 1;
                }
            }
        } else if byte == b'"' {
            let start_line = line;
            let start = pos + 1;
            pos += 1;
            while let Some(&inner) = bytes.get(pos) {
                if inner == b'\\' {
                    pos += 2;
                } else if inner == b'"' {
                    break;
                } else {
                    pos += 1;
                }
            }
            let body = bytes.get(start..pos.min(bytes.len())).unwrap_or_default();
            line += count_lines(body);
            tokens.push((
                Token::Str(String::from_utf8_lossy(body).into_owned()),
                start_line,
            ));
            pos += 1;
        } else if byte == b'\'' {
            let after = bytes.get(pos + 1).copied();
            if after == Some(b'\\') {
                // Escaped char literal: skip to its closing quote.
                pos += 3;
                while bytes.get(pos).is_some_and(|b| *b != b'\'') {
                    pos += 1;
                }
                pos += 1;
            } else {
                let width = match after {
                    Some(lead) if lead < 0x80 => 1,
                    Some(lead) if lead >= 0xf0 => 4,
                    Some(lead) if lead >= 0xe0 => 3,
                    Some(_) => 2,
                    None => 1,
                };
                if bytes.get(pos + 1 + width) == Some(&b'\'') {
                    if after == Some(b'\n') {
                        line += 1;
                    }
                    pos += 2 + width;
                } else {
                    // A lifetime or a label: the name follows as an identifier.
                    pos += 1;
                }
            }
            tokens.push((Token::Other, line));
        } else if byte.is_ascii_digit() {
            while bytes
                .get(pos)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                pos += 1;
            }
            tokens.push((Token::Other, line));
        } else if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 {
            let start = pos;
            while bytes
                .get(pos)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b >= 0x80)
            {
                pos += 1;
            }
            let word =
                String::from_utf8_lossy(bytes.get(start..pos).unwrap_or_default()).into_owned();
            let next = bytes.get(pos).copied();
            let is_raw_prefix = matches!(word.as_str(), "r" | "br" | "cr");
            if is_raw_prefix && matches!(next, Some(b'"') | Some(b'#')) {
                let mut hashes = 0_usize;
                while bytes.get(pos + hashes) == Some(&b'#') {
                    hashes += 1;
                }
                if bytes.get(pos + hashes) == Some(&b'"') {
                    let start_line = line;
                    let body_start = pos + hashes + 1;
                    let mut closing = Vec::with_capacity(hashes + 1);
                    closing.push(b'"');
                    closing.extend(std::iter::repeat_n(b'#', hashes));
                    let mut end = body_start;
                    while end < bytes.len()
                        && !bytes.get(end..).unwrap_or_default().starts_with(&closing)
                    {
                        end += 1;
                    }
                    let body = bytes.get(body_start..end).unwrap_or_default();
                    line += count_lines(body);
                    tokens.push((
                        Token::Str(String::from_utf8_lossy(body).into_owned()),
                        start_line,
                    ));
                    pos = end + closing.len();
                    continue;
                }
                // `r#ident`: a raw identifier; the `#` becomes punctuation.
            } else if matches!(word.as_str(), "b" | "c") && next == Some(b'"') {
                // b"..." / c"...": the string token follows on the next loop.
                continue;
            }
            tokens.push((Token::Ident(word), line));
        } else {
            tokens.push((Token::Punct(byte), line));
            pos += 1;
        }
    }
    tokens
}

/// An out-of-line module declaration at the top level of a file.
#[derive(Debug, Clone, PartialEq)]
struct ChildModule {
    name: String,
    path_attribute: Option<String>,
    is_test: bool,
}

/// What a file's top level says about test code.
#[derive(Debug, Clone, PartialEq, Default)]
struct SourceScan {
    /// Inclusive line spans of top-level `#[cfg(test)]` inline modules.
    test_spans: Vec<(u64, u64)>,
    children: Vec<ChildModule>,
}

fn token_text(token: &Token) -> String {
    match token {
        Token::Ident(word) => word.clone(),
        Token::Punct(byte) => (*byte as char).to_string(),
        Token::Str(text) => format!("\"{text}\""),
        Token::Other => "?".to_string(),
    }
}

fn is_test_cfg(attribute: &str) -> bool {
    attribute == "cfg(test)" || attribute.starts_with("cfg(all(test,")
}

fn scan_source(source: &str) -> SourceScan {
    let tokens = tokenize(source);
    let mut scan = SourceScan::default();
    let mut depth = 0_usize;
    let mut index = 0_usize;
    // Set while inside a top-level test module: (start line, depth to return to).
    let mut open_test_module: Option<u64> = None;
    while let Some((token, line)) = tokens.get(index) {
        match token {
            Token::Punct(b'{') => {
                depth += 1;
                index += 1;
            }
            Token::Punct(b'}') => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(start) = open_test_module.take()
                {
                    scan.test_spans.push((start, *line));
                }
                index += 1;
            }
            Token::Punct(b'#') if depth == 0 => {
                // One or more outer attributes, then maybe a module item.
                let first_line = *line;
                let start = index;
                let mut is_test = false;
                let mut path_attribute = None;
                while matches!(tokens.get(index), Some((Token::Punct(b'#'), _))) {
                    let inner = matches!(tokens.get(index + 1), Some((Token::Punct(b'!'), _)));
                    let open_at = if inner { index + 2 } else { index + 1 };
                    if !matches!(tokens.get(open_at), Some((Token::Punct(b'['), _))) {
                        break;
                    }
                    let mut brackets = 0_usize;
                    let mut text = String::new();
                    let mut cursor = open_at;
                    while let Some((inner_token, _)) = tokens.get(cursor) {
                        match inner_token {
                            Token::Punct(b'[') => brackets += 1,
                            Token::Punct(b']') => brackets -= 1,
                            _ => {}
                        }
                        cursor += 1;
                        if brackets == 0 {
                            break;
                        }
                        if !(brackets == 1 && matches!(inner_token, Token::Punct(b'['))) {
                            text.push_str(&token_text(inner_token));
                        }
                    }
                    index = cursor;
                    if inner {
                        continue;
                    }
                    if is_test_cfg(&text) {
                        is_test = true;
                    }
                    if let Some(path) = text
                        .strip_prefix("path=\"")
                        .and_then(|p| p.strip_suffix('"'))
                    {
                        path_attribute = Some(path.to_string());
                    }
                }
                if index == start {
                    // A `#` that opens no attribute (`r#ident`, a `#` inside a
                    // `(..)` or `[..]` macro call): step over it, or the walk
                    // would stand on it forever.
                    index += 1;
                    continue;
                }
                // Visibility: `pub`, `pub(crate)`, `pub(super)`, `pub(in ..)`.
                let mut cursor = index;
                if matches!(tokens.get(cursor), Some((Token::Ident(word), _)) if word == "pub") {
                    cursor += 1;
                    if matches!(tokens.get(cursor), Some((Token::Punct(b'('), _))) {
                        while tokens
                            .get(cursor)
                            .is_some_and(|(t, _)| *t != Token::Punct(b')'))
                        {
                            cursor += 1;
                        }
                        cursor += 1;
                    }
                }
                let is_mod =
                    matches!(tokens.get(cursor), Some((Token::Ident(word), _)) if word == "mod");
                if !is_mod {
                    continue;
                }
                let Some((Token::Ident(name), _)) = tokens.get(cursor + 1) else {
                    index = cursor + 1;
                    continue;
                };
                match tokens.get(cursor + 2) {
                    Some((Token::Punct(b';'), _)) => {
                        scan.children.push(ChildModule {
                            name: name.clone(),
                            path_attribute,
                            is_test,
                        });
                        index = cursor + 3;
                    }
                    Some((Token::Punct(b'{'), _)) => {
                        if is_test {
                            open_test_module = Some(first_line);
                        }
                        index = cursor + 2;
                    }
                    _ => index = cursor + 2,
                }
            }
            Token::Ident(word) if depth == 0 && word == "mod" => {
                // A module with no attributes.
                if let (Some((Token::Ident(name), _)), Some((Token::Punct(b';'), _))) =
                    (tokens.get(index + 1), tokens.get(index + 2))
                {
                    scan.children.push(ChildModule {
                        name: name.clone(),
                        path_attribute: None,
                        is_test: false,
                    });
                    index += 3;
                } else {
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    scan
}

// ---------------------------------------------------------------------------
// Files and crates.
// ---------------------------------------------------------------------------

/// The crate a path belongs to: the segment after the first `crates/`, as the
/// gate assigns it.
fn crate_of(path: &str) -> Option<String> {
    let after = path.split_once("crates/")?.1;
    let name = after.split('/').next()?;
    (!name.is_empty() && after.contains('/')).then(|| name.to_string())
}

/// Removes `.` and `..` without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// True for files that set the directory their child modules live in.
fn is_directory_owner(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let in_bin = path
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "bin");
    matches!(name, "mod.rs" | "lib.rs" | "main.rs" | "build.rs") || in_bin
}

/// The files a module declaration in `parent` may resolve to.
fn child_candidates(parent: &Path, child: &ChildModule) -> Vec<PathBuf> {
    let directory = parent.parent().unwrap_or_else(|| Path::new(""));
    if let Some(path) = &child.path_attribute {
        return vec![normalize(&directory.join(path))];
    }
    let base = if is_directory_owner(parent) {
        directory.to_path_buf()
    } else {
        directory.join(parent.file_stem().unwrap_or_default())
    };
    vec![
        normalize(&base.join(format!("{}.rs", child.name))),
        normalize(&base.join(&child.name).join("mod.rs")),
    ]
}

fn named_as_test(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or_default();
    name == "tests.rs"
        || name.ends_with("_tests.rs")
        || crate_of(path).is_some_and(|krate| path.contains(&format!("crates/{krate}/tests/")))
}

/// Which reported files are whole test files: named as tests, or loaded by a
/// `#[cfg(test)] mod name;`, or loaded by such a file in turn.
fn whole_test_files(scans: &BTreeMap<PathBuf, SourceScan>) -> BTreeSet<PathBuf> {
    let mut tests: BTreeSet<PathBuf> = scans
        .keys()
        .filter(|path| named_as_test(&path.to_string_lossy()))
        .cloned()
        .collect();
    loop {
        let mut added = false;
        for (path, scan) in scans {
            let parent_is_test = tests.contains(path);
            for child in &scan.children {
                if !(child.is_test || parent_is_test) {
                    continue;
                }
                for candidate in child_candidates(path, child) {
                    if scans.contains_key(&candidate) && tests.insert(candidate) {
                        added = true;
                    }
                }
            }
        }
        if !added {
            return tests;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Tally {
    lines: u64,
    covered: u64,
}

impl Tally {
    fn add(&mut self, covered: bool) {
        self.lines += 1;
        if covered {
            self.covered += 1;
        }
    }

    /// Percent with two decimals, rounded down, as text.
    fn percent(&self) -> String {
        if self.lines == 0 {
            return "n/a".to_string();
        }
        let hundredths = u128::from(self.covered) * 10_000 / u128::from(self.lines);
        format!("{}.{:02}", hundredths / 100, hundredths % 100)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
struct CrateReport {
    /// llvm-cov's `summary.lines`, exactly what `scripts/coverage-gate.sh`
    /// sums. It adds each function's lines, so a line shared by a function
    /// and a closure inside it counts twice.
    gate: Tally,
    /// Each mapped line once, as `llvm-cov show` displays it.
    all: Tally,
    production: Tally,
    test: Tally,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Report {
    crates: BTreeMap<String, CrateReport>,
    /// Files whose per-line totals differ from llvm-cov's per-function summary.
    summary_differs: usize,
    /// Files with no segments in the export (counted from the summary, as production).
    no_segments: Vec<String>,
    /// Files whose source could not be read (counted as production).
    unreadable: Vec<String>,
}

/// Builds the report. `read_source` returns a file's text, or None.
fn build_report(files: &[FileRecord], read_source: impl Fn(&Path) -> Option<String>) -> Report {
    let mut report = Report::default();
    let mut scans = BTreeMap::new();
    for file in files {
        if crate_of(&file.filename).is_none() {
            continue;
        }
        let path = normalize(Path::new(&file.filename));
        match read_source(&path) {
            Some(text) => {
                scans.insert(path, scan_source(&text));
            }
            None => {
                report.unreadable.push(file.filename.clone());
                scans.insert(path, SourceScan::default());
            }
        }
    }
    let test_files = whole_test_files(&scans);
    for file in files {
        let Some(krate) = crate_of(&file.filename) else {
            continue;
        };
        let path = normalize(Path::new(&file.filename));
        let whole_test = test_files.contains(&path);
        let spans = scans
            .get(&path)
            .map(|s| s.test_spans.as_slice())
            .unwrap_or_default();
        let entry = report.crates.entry(krate).or_default();
        entry.gate.lines += file.summary_count;
        entry.gate.covered += file.summary_covered;
        let Some(segments) = &file.segments else {
            report.no_segments.push(file.filename.clone());
            let bucket = if whole_test {
                &mut entry.test
            } else {
                &mut entry.production
            };
            for tally in [&mut entry.all, bucket] {
                tally.lines += file.summary_count;
                tally.covered += file.summary_covered;
            }
            continue;
        };
        let stats = line_stats(segments);
        let mut file_total = Tally::default();
        for stat in &stats {
            file_total.add(stat.covered);
            entry.all.add(stat.covered);
            let in_test = whole_test
                || spans
                    .iter()
                    .any(|(start, end)| (*start..=*end).contains(&stat.line));
            if in_test {
                entry.test.add(stat.covered);
            } else {
                entry.production.add(stat.covered);
            }
        }
        if file_total.lines != file.summary_count || file_total.covered != file.summary_covered {
            report.summary_differs += 1;
        }
    }
    report
}

/// `default` and the `[crates]` table of the thresholds file, as text.
fn parse_floors(toml: &str) -> (Option<String>, BTreeMap<String, String>) {
    let mut default = None;
    let mut floors = BTreeMap::new();
    let mut section = String::new();
    for raw in toml.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            section = line.to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim().trim_matches('"'), value.trim().to_string());
        if section.is_empty() && key == "default" {
            default = Some(value);
        } else if section == "[crates]" {
            floors.insert(key.to_string(), value);
        }
    }
    (default, floors)
}

fn render(report: &Report, floors: &(Option<String>, BTreeMap<String, String>)) -> String {
    let mut out = String::new();
    out.push_str("## Production-only line coverage (report only; floors unchanged)\n\n");
    out.push_str("| Crate | Gate % (summary) | All lines % (per line) | Production % (per line) | Production lines covered | Test lines in src | Floor (unchanged) |\n");
    out.push_str("|---|---|---|---|---|---|---|\n");
    let mut total = CrateReport::default();
    for (krate, entry) in &report.crates {
        let floor = floors
            .1
            .get(krate)
            .cloned()
            .or_else(|| floors.0.clone().map(|d| format!("{d} (default)")))
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "| {krate} | {} | {} | {} | {} of {} | {} | {floor} |\n",
            entry.gate.percent(),
            entry.all.percent(),
            entry.production.percent(),
            entry.production.covered,
            entry.production.lines,
            entry.test.lines,
        ));
        for (sum, part) in [
            (&mut total.gate, entry.gate),
            (&mut total.all, entry.all),
            (&mut total.production, entry.production),
            (&mut total.test, entry.test),
        ] {
            sum.lines += part.lines;
            sum.covered += part.covered;
        }
    }
    out.push_str(&format!(
        "| **workspace** | {} | {} | {} | {} of {} | {} | - |\n\n",
        total.gate.percent(),
        total.all.percent(),
        total.production.percent(),
        total.production.covered,
        total.production.lines,
        total.test.lines,
    ));
    out.push_str("Test code = `tests.rs` / `*_tests.rs` files, files loaded by a top-level `#[cfg(test)] mod x;`, and top-level `#[cfg(test)] mod x { .. }` blocks. Nested test modules count as production.\n\n");
    out.push_str(&format!(
        "Gate % adds each function's lines (llvm-cov's summary), so a line shared with a closure counts twice; the per-line columns count each line once, as `llvm-cov show` does. Compare production % with all lines % (per line). {} files differ between the two counts.\n\n",
        report.summary_differs
    ));
    for (label, list) in [
        (
            "files with no segments in the export (summary used, counted as production)",
            &report.no_segments,
        ),
        (
            "source files that could not be read (counted as production)",
            &report.unreadable,
        ),
    ] {
        out.push_str(&format!("- {} {label}\n", list.len()));
        for name in list.iter().take(10) {
            out.push_str(&format!("  - {name}\n"));
        }
    }
    out
}

/// Runs the report; returns the text and the exit code.
fn outcome(args: &[String]) -> (String, i32) {
    let Some(json_path) = args.get(1) else {
        return (
            "usage: coverage_prod_report <coverage.json> [crate-coverage-thresholds.toml]\n"
                .to_string(),
            2,
        );
    };
    let bytes = match std::fs::read(json_path) {
        Ok(bytes) => bytes,
        Err(err) => return (format!("cannot read {json_path}: {err}\n"), 1),
    };
    let files = match parse_export(&bytes) {
        Ok(files) => files,
        Err(err) => return (format!("{err}\n"), 1),
    };
    if files.is_empty() {
        return (format!("{json_path} lists no files\n"), 1);
    }
    let floors = args
        .get(2)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| parse_floors(&text))
        .unwrap_or_default();
    let report = build_report(&files, |path| std::fs::read_to_string(path).ok());
    (render(&report, &floors), 0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (text, code) = outcome(&args);
    let _ = std::io::stdout().write_all(text.as_bytes());
    if code == 0
        && let Ok(summary_path) = std::env::var("GITHUB_STEP_SUMMARY")
        && let Ok(mut summary) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(summary_path)
    {
        let _ = summary.write_all(text.as_bytes());
    }
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(line: u64, count: u64, has_count: bool, entry: bool, gap: bool) -> Segment {
        Segment {
            line,
            count,
            has_count,
            is_region_entry: entry,
            is_gap_region: gap,
        }
    }

    #[test]
    fn test_scan_finds_a_test_module_span_by_brace_matching() {
        let source = "fn prod() {}\n\
                      #[cfg(test)]\n\
                      mod tests {\n\
                      \x20   fn a() { let s = \"}}}\"; let c = '}'; }\n\
                      \x20   // }\n\
                      \x20   /* } /* } */ */\n\
                      }\n\
                      fn after() {}\n";
        let scan = scan_source(source);
        assert_eq!(scan.test_spans, vec![(2, 7)]);
    }

    #[test]
    fn test_scan_does_not_assume_the_module_runs_to_end_of_file() {
        let source = "#[cfg(test)]\nmod a {\n}\nfn prod() {\n    1;\n}\n";
        let scan = scan_source(source);
        assert_eq!(scan.test_spans, vec![(1, 3)]);
    }

    #[test]
    fn test_scan_handles_raw_strings_lifetimes_and_visibility() {
        let source = "fn f<'a>(x: &'a str) -> &'a str { x }\n\
                      #[cfg(test)]\n\
                      #[allow(dead_code)] // APPROVED: test\n\
                      pub(crate) mod t {\n\
                      \x20   const R: &str = r#\"{ \" }\"#;\n\
                      \x20   const B: u8 = b'{';\n\
                      \x20   fn g<'b>() {}\n\
                      }\n\
                      #[cfg(test)]\n\
                      fn not_a_module() {}\n\
                      mod prod_inline {\n\
                      \x20   #[cfg(test)]\n\
                      \x20   mod nested {}\n\
                      }\n";
        let scan = scan_source(source);
        assert_eq!(scan.test_spans, vec![(2, 8)]);
    }

    #[test]
    fn test_scan_accepts_cfg_all_test_and_rejects_cfg_not_test() {
        let source =
            "#[cfg(all(test, feature = \"x\"))]\nmod a {\n}\n#[cfg(not(test))]\nmod b {\n}\n";
        let scan = scan_source(source);
        assert_eq!(scan.test_spans, vec![(1, 3)]);
    }

    #[test]
    fn test_scan_records_out_of_line_modules_with_path() {
        let source = "pub mod prod;\n#[cfg(test)]\n#[path = \"x_tests.rs\"]\nmod tests;\n#[cfg(test)]\nmod support;\n";
        let scan = scan_source(source);
        assert_eq!(
            scan.children,
            vec![
                ChildModule {
                    name: "prod".into(),
                    path_attribute: None,
                    is_test: false
                },
                ChildModule {
                    name: "tests".into(),
                    path_attribute: Some("x_tests.rs".into()),
                    is_test: true
                },
                ChildModule {
                    name: "support".into(),
                    path_attribute: None,
                    is_test: true
                },
            ]
        );
        assert!(scan.test_spans.is_empty());
    }

    #[test]
    fn test_whole_test_files_follow_cfg_test_declarations_transitively() {
        let mut scans = BTreeMap::new();
        scans.insert(
            PathBuf::from("/w/crates/core/src/lib.rs"),
            scan_source("pub mod prod;\n#[cfg(test)]\nmod test_support;\n"),
        );
        scans.insert(
            PathBuf::from("/w/crates/core/src/prod.rs"),
            SourceScan::default(),
        );
        scans.insert(
            PathBuf::from("/w/crates/core/src/test_support.rs"),
            scan_source("pub mod ssm;\n"),
        );
        scans.insert(
            PathBuf::from("/w/crates/core/src/test_support/ssm.rs"),
            SourceScan::default(),
        );
        scans.insert(
            PathBuf::from("/w/crates/core/src/a/b_tests.rs"),
            SourceScan::default(),
        );
        let tests = whole_test_files(&scans);
        let mut names: Vec<_> = tests
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "/w/crates/core/src/a/b_tests.rs",
                "/w/crates/core/src/test_support.rs",
                "/w/crates/core/src/test_support/ssm.rs",
            ]
        );
    }

    #[test]
    fn test_child_candidates_respect_mod_rs_and_path() {
        let child = ChildModule {
            name: "x".into(),
            path_attribute: None,
            is_test: true,
        };
        assert_eq!(
            child_candidates(Path::new("/w/src/a.rs"), &child),
            vec![
                PathBuf::from("/w/src/a/x.rs"),
                PathBuf::from("/w/src/a/x/mod.rs")
            ]
        );
        assert_eq!(
            child_candidates(Path::new("/w/src/a/mod.rs"), &child)[0],
            PathBuf::from("/w/src/a/x.rs")
        );
        let with_path = ChildModule {
            path_attribute: Some("a/x_tests.rs".into()),
            ..child
        };
        assert_eq!(
            child_candidates(Path::new("/w/src/a.rs"), &with_path),
            vec![PathBuf::from("/w/src/a/x_tests.rs")]
        );
    }

    #[test]
    fn test_line_stats_follows_llvm_rules() {
        // 1: region start, count 5 -> covered.
        // 2: only the region's end; the wrapped count (5) carries -> covered.
        // 3: region start, count 0 -> mapped, not covered.
        // 4: a skipped region starts -> not mapped.
        // 5: a gap entry with a count -> mapped, wrapped count 0 -> not covered.
        // 6: end of the gap region -> mapped, not covered.
        let segments = [
            seg(1, 5, true, true, false),
            seg(2, 0, false, false, false),
            seg(3, 0, true, true, false),
            seg(3, 0, false, false, false),
            seg(4, 0, false, true, false),
            seg(5, 0, true, true, true),
            seg(6, 0, false, false, false),
        ];
        let lines: Vec<(u64, bool)> = line_stats(&segments)
            .iter()
            .map(|s| (s.line, s.covered))
            .collect();
        assert_eq!(
            lines,
            vec![(1, true), (2, true), (3, false), (5, false), (6, false)]
        );
    }

    #[test]
    fn test_line_stats_takes_max_of_region_starts_on_a_line() {
        let segments = [
            seg(1, 0, true, true, false),
            seg(1, 7, true, true, false),
            seg(2, 0, false, false, false),
        ];
        let lines: Vec<(u64, bool)> = line_stats(&segments)
            .iter()
            .map(|s| (s.line, s.covered))
            .collect();
        assert_eq!(lines, vec![(1, true), (2, true)]);
    }

    /// A real `llvm-cov export` (rustc 1.95.0, `-C instrument-coverage`) of
    /// `exp_lib.rs.txt` after its tests ran, filename rewritten to `/w/...`.
    const REAL_EXPORT: &str =
        include_str!("../../tests/fixtures/coverage_prod_report/exp_coverage.json");
    const REAL_SOURCE: &str =
        include_str!("../../tests/fixtures/coverage_prod_report/exp_lib.rs.txt");

    #[test]
    fn test_real_export_matches_llvm_cov_show_line_by_line() {
        let files = parse_export(REAL_EXPORT.as_bytes()).expect("real export parses");
        assert_eq!(files.len(), 1);
        let segments = files[0].segments.as_deref().expect("segments exported");
        let covered: Vec<u64> = line_stats(segments)
            .iter()
            .filter(|s| s.covered)
            .map(|s| s.line)
            .collect();
        let uncovered: Vec<u64> = line_stats(segments)
            .iter()
            .filter(|s| !s.covered)
            .map(|s| s.line)
            .collect();
        // Read off `llvm-cov show` for the same profile: a count > 0, or 0.
        let shown_covered: Vec<u64> = [1, 2, 3, 4, 7, 9]
            .into_iter()
            .chain(19..=27)
            .chain([29, 30, 32, 35, 36, 39, 41, 42, 43, 45, 47, 48, 52, 53])
            .chain(59..=67)
            .collect();
        let shown_uncovered: Vec<u64> = [5]
            .into_iter()
            .chain(11..=17)
            .chain([31, 33, 44, 49, 50, 71, 72, 73])
            .collect();
        assert_eq!(covered, shown_covered);
        assert_eq!(uncovered, shown_uncovered);
        // llvm-cov's own summary adds per-function lines (closures twice).
        assert_eq!((files[0].summary_count, files[0].summary_covered), (57, 41));
    }

    #[test]
    fn test_real_export_production_split() {
        let files = parse_export(REAL_EXPORT.as_bytes()).expect("real export parses");
        let report = build_report(&files, |path| {
            (path == Path::new("/w/crates/exp/src/lib.rs")).then(|| REAL_SOURCE.to_string())
        });
        let exp = report.crates.get("exp").expect("exp row");
        assert_eq!(
            exp.gate,
            Tally {
                lines: 57,
                covered: 41
            }
        );
        assert_eq!(
            exp.all,
            Tally {
                lines: 54,
                covered: 38
            }
        );
        // The test module (source lines 55-74) holds 12 mapped lines, 9 covered.
        assert_eq!(
            exp.test,
            Tally {
                lines: 12,
                covered: 9
            }
        );
        assert_eq!(
            exp.production,
            Tally {
                lines: 42,
                covered: 29
            }
        );
        assert_eq!(exp.production.percent(), "69.04");
        assert_eq!(report.summary_differs, 1);
        assert!(report.unreadable.is_empty());
    }

    const FIXTURE: &str = r#"{"type":"llvm.coverage.json.export","version":"2.0.1","data":[{"files":[
      {"filename":"/w/crates/app/src/lib.rs","branches":[],"expansions":[{"x":[1,2,{"y":"]"}]}],
       "segments":[[1,1,4,true,true,false],[2,1,0,true,true,false],[3,1,0,false,false,false],[5,1,9,true,true,false],[6,1,0,true,true,false],[7,1,0,false,false,false]],
       "summary":{"lines":{"count":6,"covered":4,"percent":66.6},"regions":{"count":4}}},
      {"filename":"/w/crates/app/src/x_tests.rs",
       "segments":[[1,1,3,true,true,false],[2,1,0,false,false,false]],
       "summary":{"lines":{"count":1,"covered":1}}},
      {"filename":"/w/crates/app/src/nosegs.rs","summary":{"lines":{"count":10,"covered":3}}},
      {"filename":"/registry/serde/src/lib.rs","segments":[],"summary":{"lines":{"count":1,"covered":1}}}
    ],"functions":[{"name":"\"}x","regions":[[1,2,3]]}],"totals":{}}]}"#;

    const FIXTURE_LIB: &str =
        "fn prod() {\n    zero();\n}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";

    #[test]
    fn test_parse_export_reads_files_and_skips_other_fields() {
        let files = parse_export(FIXTURE.as_bytes()).expect("fixture parses");
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].filename, "/w/crates/app/src/lib.rs");
        assert_eq!(files[0].segments.as_ref().map(Vec::len), Some(6));
        assert_eq!((files[0].summary_count, files[0].summary_covered), (6, 4));
        assert_eq!(files[2].segments, None);
        assert_eq!((files[2].summary_count, files[2].summary_covered), (10, 3));
    }

    #[test]
    fn test_parse_export_refuses_truncated_input() {
        let truncated = &FIXTURE.as_bytes()[..FIXTURE.len() / 2];
        assert!(parse_export(truncated).is_err());
    }

    #[test]
    fn test_report_arithmetic_on_fixture() {
        let files = parse_export(FIXTURE.as_bytes()).expect("fixture parses");
        let report = build_report(&files, |path| {
            (path == Path::new("/w/crates/app/src/lib.rs")).then(|| FIXTURE_LIB.to_string())
        });
        let app = report.crates.get("app").expect("app row");
        // lib.rs mapped lines 1C 2C 3U 5C 6C 7U; the test module spans 4-8,
        // so production is 1-3 (2 of 3) and test 5-7 (2 of 3).
        // x_tests.rs: lines 1C 2C, all test. nosegs.rs: summary 3 of 10, production.
        assert_eq!(
            app.production,
            Tally {
                lines: 13,
                covered: 5
            }
        );
        assert_eq!(
            app.test,
            Tally {
                lines: 5,
                covered: 4
            }
        );
        assert_eq!(
            app.all,
            Tally {
                lines: 18,
                covered: 9
            }
        );
        assert_eq!(
            app.gate,
            Tally {
                lines: 17,
                covered: 8
            }
        );
        assert_eq!(app.production.percent(), "38.46");
        // x_tests.rs: per-line 2/2 against a summary of 1/1.
        assert_eq!(report.summary_differs, 1);
        assert_eq!(
            report.no_segments,
            vec!["/w/crates/app/src/nosegs.rs".to_string()]
        );
        assert_eq!(report.unreadable.len(), 2);
        assert!(!report.crates.contains_key("serde"));
    }

    #[test]
    fn test_floors_and_render() {
        let floors = parse_floors(
            "default = 63.0\n[crates]\n# a = 1\napp = 72.6 # note\n[files]\n\"x\" = 1.0\n",
        );
        assert_eq!(floors.0.as_deref(), Some("63.0"));
        assert_eq!(floors.1.get("app").map(String::as_str), Some("72.6"));
        assert_eq!(floors.1.len(), 1);
        let files = parse_export(FIXTURE.as_bytes()).expect("fixture parses");
        let report = build_report(&files, |_| Some(FIXTURE_LIB.to_string()));
        let text = render(&report, &floors);
        assert!(text.contains("| app |"), "{text}");
        assert!(text.contains("72.6"), "{text}");
        assert!(text.contains("floors unchanged"), "{text}");
    }

    #[test]
    fn test_outcome_usage_and_missing_file() {
        assert_eq!(outcome(&["x".to_string()]).1, 2);
        assert_eq!(
            outcome(&["x".to_string(), "/nonexistent/cov.json".to_string()]).1,
            1
        );
    }

    #[test]
    fn test_crate_of_matches_the_gate() {
        assert_eq!(
            crate_of("/home/runner/w/crates/app/src/main.rs").as_deref(),
            Some("app")
        );
        assert_eq!(crate_of("/x/crates/"), None);
        assert_eq!(crate_of("/x/y.rs"), None);
    }

    /// A `#` at the top level that opens no attribute must be stepped over.
    /// Each of these used to spin forever (the walk stood on the `#`).
    #[test]
    fn test_scan_steps_over_a_top_level_hash_that_opens_no_attribute() {
        let sources = [
            "pub fn r#match() {}\n#[cfg(test)]\nmod t {\n}\n",
            "pub struct r#Type;\npub mod r#async;\n#[cfg(test)]\nmod t {\n}\n",
            "m!(# x);\nm![# y];\n#[cfg(test)]\nmod t {\n}\n",
            "#! x\n#[cfg(test)]\nmod t {\n}\n",
            "#[allow(dead_code)] # z\n#[cfg(test)]\nmod t {\n}\n",
        ];
        for source in sources {
            let (sender, receiver) = std::sync::mpsc::channel();
            let owned = source.to_string();
            std::thread::spawn(move || {
                let _ = sender.send(scan_source(&owned));
            });
            let scan = receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("scan_source did not finish on {source:?}"));
            let last = source.lines().count() as u64;
            assert_eq!(scan.test_spans, vec![(last - 2, last)], "{source:?}");
        }
    }

    #[test]
    fn test_string_decodes_every_escape() {
        let mut reader = JsonReader::new(br#""a\nb\tc\rd\be\ff\u00e9\"\\\/""#);
        assert_eq!(
            reader.string().expect("escaped string decodes"),
            "a\nb\tc\rd\u{8}e\u{c}f\u{e9}\"\\/"
        );
        let mut lone = JsonReader::new(br#""\ud800""#);
        assert_eq!(lone.string().expect("lone surrogate decodes"), "\u{fffd}");
        let mut bad = JsonReader::new(br#""\u12""#);
        assert!(bad.string().is_err());
        let mut not_utf8 = JsonReader::new(b"\"\xff\"");
        assert!(not_utf8.string().is_err());
        let mut unterminated = JsonReader::new(b"\"abc");
        assert!(unterminated.string().is_err());
    }

    #[test]
    fn test_reader_errors_on_wrong_types() {
        assert!(JsonReader::new(b"x").boolean().is_err());
        assert!(JsonReader::new(b"-1").unsigned().is_err());
        assert!(JsonReader::new(b"").skip_value().is_err());
        assert!(JsonReader::new(b"[1,").skip_value().is_err());
        let mut scalar = JsonReader::new(b" 12.5e3 ,");
        scalar.skip_value().expect("scalar skips");
        assert_eq!(scalar.peek(), Some(b','));
        assert!(parse_export(b"{\"data\":[{\"files\":[[1]]}]}").is_err());
        assert!(parse_export(b"{\"data\" 1}").is_err());
        // A segment with extra trailing fields still reads.
        let mut extra = JsonReader::new(b"[3,1,2,true,true,false,9,\"x\"]");
        assert_eq!(
            extra.segment().expect("segment with extra fields"),
            seg(3, 2, true, true, false)
        );
        // An older five-field segment reads with no gap flag.
        let mut short = JsonReader::new(b"[4,1,0,false,true]");
        assert_eq!(
            short.segment().expect("five-field segment"),
            seg(4, 0, false, true, false)
        );
    }

    #[test]
    fn test_tokenize_char_literals_of_every_width_and_byte_strings() {
        let source = "const A: [char; 4] = ['a', 'é', '€', '😀'];\n\
                      const N: char = '\n';\n\
                      const E: char = '\\'';\n\
                      const S: &[u8] = b\"{\";\n\
                      const C: &CStr = c\"}\";\n\
                      const R: &[u8] = br\"{\";\n\
                      #[cfg(test)]\n\
                      mod t {\n\
                      }\n";
        let scan = scan_source(source);
        assert_eq!(scan.test_spans, vec![(8, 10)]);
        let strings: Vec<String> = tokenize(source)
            .into_iter()
            .filter_map(|(token, _)| match token {
                Token::Str(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(strings, vec!["{", "}", "{"]);
        // A lone quote at the end of the input is one `Other` token.
        assert_eq!(tokenize("'"), vec![(Token::Other, 1)]);
    }

    #[test]
    fn test_normalize_and_child_candidates_resolve_dot_segments() {
        assert_eq!(
            normalize(Path::new("/w/src/./a/../b.rs")),
            PathBuf::from("/w/src/b.rs")
        );
        // `Path::components` keeps a `.` only at the start.
        assert_eq!(
            normalize(Path::new("./src/a/../b.rs")),
            PathBuf::from("src/b.rs")
        );
        let child = ChildModule {
            name: "x".into(),
            path_attribute: Some("../other/x_tests.rs".into()),
            is_test: true,
        };
        assert_eq!(
            child_candidates(Path::new("/w/src/a/lib.rs"), &child),
            vec![PathBuf::from("/w/src/other/x_tests.rs")]
        );
        assert!(is_directory_owner(Path::new("/w/src/bin/tool.rs")));
        assert!(!is_directory_owner(Path::new("/w/src/tool.rs")));
    }

    /// Writes `text` at `<temp>/coverage_prod_report_unit_<pid>/<relative>`.
    fn temp_file(relative: &str, text: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("coverage_prod_report_unit_{}", std::process::id()))
            .join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("temp dir"); // APPROVED: test
        }
        std::fs::write(&path, text).expect("temp file"); // APPROVED: test
        path
    }

    #[test]
    fn test_outcome_reports_parse_errors_empty_exports_and_success() {
        let malformed = temp_file("malformed.json", "{\"data\":[");
        let (text, code) = outcome(&["x".into(), malformed.to_string_lossy().into_owned()]);
        assert_eq!(code, 1);
        assert!(text.starts_with("coverage JSON:"), "{text}");

        let empty = temp_file("empty.json", "{\"data\":[{\"files\":[]}]}");
        let (text, code) = outcome(&["x".into(), empty.to_string_lossy().into_owned()]);
        assert_eq!(code, 1);
        assert!(text.contains("lists no files"), "{text}");

        let source = temp_file("crates/app/src/lib.rs", FIXTURE_LIB);
        let json = FIXTURE.replace("/w/crates/app/src/lib.rs", &source.to_string_lossy());
        let good = temp_file("good.json", &json);
        let floors = temp_file("floors.toml", "default = 63.0\n[crates]\nother = 9.0\n");
        let (text, code) = outcome(&[
            "x".into(),
            good.to_string_lossy().into_owned(),
            floors.to_string_lossy().into_owned(),
        ]);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("floors unchanged"), "{text}");
        // lib.rs is read from disk, so its test module is split out exactly as
        // in `test_report_arithmetic_on_fixture`: production 5 of 13.
        let app_row = |text: &str| {
            text.lines()
                .find(|line| line.starts_with("| app |"))
                .map(str::to_string)
                .unwrap_or_default()
        };
        let row = app_row(&text);
        assert!(row.contains("| 38.46 | 5 of 13 | 5 |"), "{text}");
        assert!(row.ends_with("| 63.0 (default) |"), "{text}");
        // No floors file: the app row's floor column reads "-".
        let (text, code) = outcome(&["x".into(), good.to_string_lossy().into_owned()]);
        assert_eq!(code, 0);
        assert!(app_row(&text).ends_with("| - |"), "{text}");
    }

    #[test]
    fn test_reader_empty_containers_and_bad_separators() {
        assert_eq!(parse_export(b"{}").expect("empty object"), Vec::new());
        assert_eq!(
            parse_export(b"{\"data\":[{}]}").expect("empty entry"),
            Vec::new()
        );
        assert!(parse_export(b"{\"data\":[] x}").is_err());
        assert!(line_stats(&[]).is_empty());
        assert_eq!(Tally::default().percent(), "n/a");
    }

    #[test]
    fn test_scan_skips_comments_escapes_inner_attributes_and_odd_mod_items() {
        // Line 1 inner attribute, 2-3 a block comment over two lines, 4 an
        // escaped string and a `\u{..}` char, 5-6 the module's two attributes.
        let source = "#![allow(dead_code)]\n\
                      /* a {\n b */\n\
                      const S: &str = \"a\\\"{\"; const C: char = '\\u{7b}';\n\
                      #[cfg_attr(x, y = 1)]\n\
                      #[cfg(test)]\n\
                      mod t {\n\
                      }\n";
        assert_eq!(scan_source(source).test_spans, vec![(5, 8)]);
        // `mod` with no name, and a named `mod` at the end of input.
        assert_eq!(scan_source("#[cfg(test)]\nmod").test_spans, Vec::new());
        assert_eq!(scan_source("#[cfg(test)]\nmod x").test_spans, Vec::new());
        // An unterminated block comment ends the input.
        assert!(tokenize("/* {").is_empty());
    }

    #[test]
    fn test_whole_test_file_without_segments_counts_as_test() {
        let json = br#"{"data":[{"files":[
          {"filename":"/w/crates/app/src/a_tests.rs","summary":{"lines":{"count":4,"covered":3}}}
        ]}]}"#;
        let files = parse_export(json).expect("parses");
        let report = build_report(&files, |_| Some(String::new()));
        let app = report.crates.get("app").expect("app row");
        assert_eq!(
            app.test,
            Tally {
                lines: 4,
                covered: 3
            }
        );
        assert_eq!(app.production, Tally::default());
    }
}
