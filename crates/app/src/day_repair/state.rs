//! The repair's progress file: `data/state/day-repair/<day>.progress`.
//!
//! One line per finished step or object, appended, `sync_data`'d, the folder
//! synced when the file is new:
//!
//! ```text
//! v1<TAB><step><TAB><object><TAB><detail><TAB><crc32 of everything before the last tab, 8 hex>
//! ```
//!
//! The file is a NOTE of what was done, never proof of it: on a resume every
//! step re-checks its result (S3, QuestDB, the disk) and appends again when
//! it finds the work done (plan ITEM UTC-REPAIR, Failure Modes). A torn LAST
//! line (no newline: the process died mid-write) is ignored and counted; a
//! bad line anywhere else refuses the whole file, so a damaged note can never
//! read as a step that did not happen.
//!
//! O(lines) to load, O(1) to append, cold (a few hundred lines at most).

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// One step of the repair, in run order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Partitions and files listed, guards read.
    Enumerate,
    /// One partition or file copied to the quarantine prefix and verified.
    Copy,
    /// One partition dropped in QuestDB.
    Drop,
    /// One seal, spill or DLQ file moved into `data/held/<day>/`.
    Move,
    /// The day refolded from its raw frames (detail carries the counts).
    Rebuild,
    /// The verify gate passed.
    Verify,
    /// `.done` written, the app unit released.
    Done,
}

impl Step {
    /// The on-disk name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enumerate => "enumerate",
            Self::Copy => "copy",
            Self::Drop => "drop",
            Self::Move => "move",
            Self::Rebuild => "rebuild",
            Self::Verify => "verify",
            Self::Done => "done",
        }
    }

    /// Parses an on-disk name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "enumerate" => Self::Enumerate,
            "copy" => Self::Copy,
            "drop" => Self::Drop,
            "move" => Self::Move,
            "rebuild" => Self::Rebuild,
            "verify" => Self::Verify,
            "done" => Self::Done,
            _ => return None,
        })
    }
}

/// One progress record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressLine {
    /// The step it records.
    pub step: Step,
    /// What it is about (`table/partition`, a file path, or `day`).
    pub object: String,
    /// `key=value;...` details (copy key, SHA-256, counts).
    pub detail: String,
}

/// Why a progress file was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressError {
    /// The file could not be read or written.
    Io(String),
    /// A line other than a torn last one failed its format or CRC.
    Damaged { line: usize },
    /// A field holds a tab or newline, which the format cannot carry.
    BadField(String),
}

impl std::fmt::Display for ProgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "progress file I/O: {e}"),
            Self::Damaged { line } => write!(f, "progress file damaged at line {line}"),
            Self::BadField(v) => write!(f, "progress field holds a tab or newline: {v:?}"),
        }
    }
}

/// CRC-32 (IEEE, reflected, the zlib polynomial), bitwise. O(bytes); the
/// lines are short and written a few hundred times per run.
#[must_use]
pub fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Encodes one line, newline included.
///
/// # Errors
/// A field holds a tab or a newline.
// TEST-EXEMPT: covered by test_progress_line_crc_round_trip_and_torn_line_refused
pub fn encode_line(line: &ProgressLine) -> Result<String, ProgressError> {
    for v in [line.object.as_str(), line.detail.as_str()] {
        if v.contains(['\t', '\n', '\r']) {
            return Err(ProgressError::BadField(v.to_owned()));
        }
    }
    let body = format!(
        "v1\t{}\t{}\t{}",
        line.step.as_str(),
        line.object,
        line.detail
    );
    let crc = crc32_ieee(body.as_bytes());
    Ok(format!("{body}\t{crc:08x}\n"))
}

/// Decodes one line (without its newline). `None` when the format or the CRC
/// is wrong.
#[must_use]
// TEST-EXEMPT: covered by test_progress_line_crc_round_trip_and_torn_line_refused
pub fn decode_line(raw: &str) -> Option<ProgressLine> {
    let (body, crc_hex) = raw.rsplit_once('\t')?;
    let crc = u32::from_str_radix(crc_hex, 16).ok()?;
    if crc_hex.len() != 8 || crc32_ieee(body.as_bytes()) != crc {
        return None;
    }
    let mut parts = body.splitn(4, '\t');
    if parts.next()? != "v1" {
        return None;
    }
    let step = Step::parse(parts.next()?)?;
    let object = parts.next()?.to_owned();
    let detail = parts.next()?.to_owned();
    Some(ProgressLine {
        step,
        object,
        detail,
    })
}

/// What a load found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    /// Every intact line, in file order.
    pub lines: Vec<ProgressLine>,
    /// Whether the last line was torn (no newline) and ignored.
    pub torn_tail: bool,
}

/// Parses a whole file's text. O(lines).
///
/// # Errors
/// A bad line that is not a torn tail.
// TEST-EXEMPT: covered by test_progress_line_crc_round_trip_and_torn_line_refused
pub fn parse_text(text: &str) -> Result<Loaded, ProgressError> {
    let mut out = Loaded::default();
    let ends_clean = text.is_empty() || text.ends_with('\n');
    let pieces: Vec<&str> = text.split('\n').collect();
    // `split` yields a final "" after a trailing newline.
    let real = if ends_clean {
        pieces.len().saturating_sub(1)
    } else {
        pieces.len()
    };
    for (i, raw) in pieces.iter().take(real).enumerate() {
        let is_last = i + 1 == real;
        match decode_line(raw) {
            Some(l) => out.lines.push(l),
            None if is_last && !ends_clean => out.torn_tail = true,
            None => return Err(ProgressError::Damaged { line: i + 1 }),
        }
    }
    Ok(out)
}

/// The open progress file.
#[derive(Debug)]
pub struct ProgressLog {
    path: PathBuf,
    lines: Vec<ProgressLine>,
    torn_tail: bool,
}

impl ProgressLog {
    /// The file for `day` under `dir`.
    #[must_use]
    // TEST-EXEMPT: covered by test_append_reload_and_torn_tail_repair
    pub fn path_for(dir: &Path, day: &str) -> PathBuf {
        dir.join(format!("{day}.progress"))
    }

    /// Loads (or starts) the progress file at `path`.
    ///
    /// # Errors
    /// Unreadable, or damaged.
    pub fn open(path: &Path) -> Result<Self, ProgressError> {
        let loaded = match std::fs::read_to_string(path) {
            Ok(text) => parse_text(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::default(),
            Err(e) => return Err(ProgressError::Io(format!("read {}: {e}", path.display()))),
        };
        Ok(Self {
            path: path.to_path_buf(),
            lines: loaded.lines,
            torn_tail: loaded.torn_tail,
        })
    }

    /// Whether a torn last line was ignored at load.
    #[must_use]
    pub const fn had_torn_tail(&self) -> bool {
        self.torn_tail
    }

    /// Every line, in order.
    #[must_use]
    pub fn lines(&self) -> &[ProgressLine] {
        &self.lines
    }

    /// The LAST line recorded for `step` and `object`. O(lines).
    #[must_use]
    pub fn find(&self, step: Step, object: &str) -> Option<&ProgressLine> {
        self.lines
            .iter()
            .rev()
            .find(|l| l.step == step && l.object == object)
    }

    /// Whether `step` was recorded for `object`.
    #[must_use]
    pub fn has(&self, step: Step, object: &str) -> bool {
        self.find(step, object).is_some()
    }

    /// Appends one line: a torn tail left by a crash is first rewritten out;
    /// then write, `sync_data`, and a folder sync when the file is new. O(1)
    /// (O(lines) once after a crash).
    ///
    /// # Errors
    /// A bad field, or any write or sync failure (the line is then not in
    /// memory either).
    pub fn append(&mut self, line: ProgressLine) -> Result<(), ProgressError> {
        let text = encode_line(&line)?;
        let path = self.path.clone();
        let io = |e: std::io::Error| ProgressError::Io(format!("{}: {e}", path.display()));
        // A torn line a crash left is cut out first (intact lines only, temp
        // file, rename), so the new line never lands after half a line.
        if self.torn_tail {
            self.rewrite_without_torn().map_err(io)?;
            self.torn_tail = false;
        }
        let is_new = !path.exists();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        f.write_all(text.as_bytes()).map_err(io)?;
        f.sync_data().map_err(io)?;
        if is_new && let Some(dir) = path.parent() {
            std::fs::File::open(dir)
                .and_then(|d| d.sync_all())
                .map_err(io)?;
        }
        self.lines.push(line);
        Ok(())
    }

    /// Rewrites the file from its intact lines (temp file, `sync_all`,
    /// rename, folder sync), dropping a torn line a crash left.
    fn rewrite_without_torn(&self) -> std::io::Result<()> {
        let text = std::fs::read_to_string(&self.path)?;
        let mut kept = String::with_capacity(text.len());
        for raw in text.split('\n').filter(|r| !r.is_empty()) {
            if decode_line(raw).is_some() {
                kept.push_str(raw);
                kept.push('\n');
            }
        }
        let tmp = self.path.with_extension("progress.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(kept.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        if let Some(dir) = self.path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

/// The value of `key` in a `key=value;...` detail. O(detail).
#[must_use]
pub fn detail_value<'a>(detail: &'a str, key: &str) -> Option<&'a str> {
    detail.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// The `u64` value of `key` in a detail.
#[must_use]
// TEST-EXEMPT: covered by test_detail_values
pub fn detail_u64(detail: &str, key: &str) -> Option<u64> {
    detail_value(detail, key)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tv-day-repair-state-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    fn line(step: Step, object: &str, detail: &str) -> ProgressLine {
        ProgressLine {
            step,
            object: object.to_owned(),
            detail: detail.to_owned(),
        }
    }

    #[test]
    fn test_crc32_ieee_known_vector() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0);
    }

    #[test]
    fn test_progress_line_crc_round_trip_and_torn_line_refused() {
        let l = line(Step::Copy, "ticks/2026-11-20T09", "key=a;sha256=ff;rows=3");
        let text = encode_line(&l).expect("encodes");
        assert_eq!(decode_line(text.trim_end_matches('\n')), Some(l.clone()));
        // A flipped byte fails the CRC.
        let bad = text.replacen("rows=3", "rows=4", 1);
        assert_eq!(decode_line(bad.trim_end_matches('\n')), None);
        // A torn LAST line is ignored and counted.
        let torn = format!("{text}{}", &text[..text.len() / 2]);
        let loaded = parse_text(&torn).expect("torn tail is not damage");
        assert_eq!(loaded.lines, vec![l.clone()]);
        assert!(loaded.torn_tail);
        // A bad line in the middle refuses the file.
        let middle = format!("{bad}{text}");
        assert_eq!(parse_text(&middle), Err(ProgressError::Damaged { line: 1 }));
    }

    #[test]
    fn test_step_names_round_trip() {
        for s in [
            Step::Enumerate,
            Step::Copy,
            Step::Drop,
            Step::Move,
            Step::Rebuild,
            Step::Verify,
            Step::Done,
        ] {
            assert_eq!(Step::parse(s.as_str()), Some(s));
        }
        assert_eq!(Step::parse("wipe"), None);
    }

    #[test]
    fn test_encode_refuses_tabs_and_newlines() {
        assert!(encode_line(&line(Step::Move, "a\tb", "")).is_err());
        assert!(encode_line(&line(Step::Move, "a", "x\ny")).is_err());
    }

    #[test]
    fn test_append_reload_and_torn_tail_repair() {
        let dir = scratch("append");
        let path = ProgressLog::path_for(&dir, "2026-11-20");
        let mut log = ProgressLog::open(&path).expect("new file");
        log.append(line(Step::Enumerate, "day", "partitions=2"))
            .expect("append");
        log.append(line(Step::Copy, "ticks/2026-11-20T09", "rows=5"))
            .expect("append");
        // A crash leaves half a line.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        f.write_all(b"v1\tdrop\tticks/2026").expect("torn write");
        drop(f);
        let mut again = ProgressLog::open(&path).expect("torn tail tolerated");
        assert!(again.had_torn_tail());
        assert_eq!(again.lines().len(), 2);
        again
            .append(line(Step::Drop, "ticks/2026-11-20T09", ""))
            .expect("append after torn tail");
        let third = ProgressLog::open(&path).expect("clean after repair");
        assert!(!third.had_torn_tail());
        assert_eq!(third.lines().len(), 3);
        assert!(third.has(Step::Drop, "ticks/2026-11-20T09"));
        assert!(!third.has(Step::Drop, "ticks/2026-11-20T10"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_detail_values() {
        let d = "key=quarantine/x.csv.gz;rows=12;sha256=ab";
        assert_eq!(detail_value(d, "key"), Some("quarantine/x.csv.gz"));
        assert_eq!(detail_u64(d, "rows"), Some(12));
        assert_eq!(detail_u64(d, "bytes"), None);
    }
}
