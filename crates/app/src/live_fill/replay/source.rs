//! Where the replay's frames come from (plan item LF-8): the raw WAL segments
//! of one day, in a local folder (`*.wal` or `*.wal.gz`) or fetched from the
//! cold bucket's `raw-frames/` copy, read in name order (the order they were
//! written), and each frame walked packet by packet exactly as the live drain
//! walks it.

// O(1) EXEMPT: begin: cold offline CLI: O(files log files) to order the inputs, O(bytes) per segment, O(packets) per frame.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tickvault_common::constants::MAX_PACKETS_PER_FRAME;
use tickvault_common::tick_types::{MarketDepthLevel, ParsedTick};
use tickvault_core::parser::dispatcher::{dispatch_frame_uncounted, main_feed_packet_len};
use tickvault_core::parser::types::ParsedFrame;
use tickvault_storage::ws_frame_spill::{
    SegmentRead, WalEndpoint, read_segment_frames_matching, segment_name_nanos,
};

use crate::dhan_feed_stack::unknown_packet_skip;

/// The segment name a local input file carries (`ws-frames-<n>.wal`, with or
/// without `.gz`), and its name nanos. Pure.
#[must_use]
pub fn input_segment_name(path: &Path) -> Option<(String, u128)> {
    let file = path.file_name()?.to_str()?;
    let name = file.strip_suffix(".gz").unwrap_or(file);
    segment_name_nanos(name).map(|n| (name.to_owned(), n))
}

/// Why a local folder of segments was refused.
#[derive(Debug)]
pub enum InputsError {
    /// The folder could not be listed.
    Io(anyhow::Error),
    /// Two files hold the same segment (`ws-frames-N.wal` and
    /// `ws-frames-N.wal.gz`): replaying both would feed it twice.
    Duplicate(String),
    /// `*.wal.sha-<hex>.gz` files (or the same unzipped): a second,
    /// different copy of a segment, the same refusal the bucket listing
    /// makes.
    Sidecars(Vec<String>),
}

impl std::fmt::Display for InputsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e:#}"),
            Self::Duplicate(n) => write!(
                f,
                "{n} is in the folder twice (plain and gzipped): keep one copy"
            ),
            Self::Sidecars(v) => write!(
                f,
                "the folder holds a second, different copy of a segment ({}); refusing to pick one",
                v.join(", ")
            ),
        }
    }
}

/// What a local folder holds: its segment files, oldest name first (the write
/// order replay and prune use), and its `.sha-<hex>` sidecar copies (zipped
/// or not), each with the name nanos of the segment it copies. Nothing is
/// refused here: a
/// segment held twice or a sidecar matters only when the day reads that
/// segment ([`select_day_inputs`]), as the bucket listing judges it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalFolder {
    /// `(name nanos, path)`, sorted; a segment held plain and gzipped shows
    /// twice with the same nanos.
    pub segments: Vec<(u128, PathBuf)>,
    /// `(file name, nanos of the segment it is a copy of)`, sorted by name.
    pub sidecars: Vec<(String, u128)>,
}

/// Whether a local file is a sidecar copy of a segment: `<name>.sha-<hex>.gz`
/// as the bucket holds it, or the same file unzipped (`<name>.sha-<hex>`, an
/// operator who ran `gunzip *.gz` on a downloaded day), so a second copy is
/// refused in either form, never silently skipped. Pure.
#[must_use]
pub fn is_local_sidecar(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.strip_suffix(".gz")
        .unwrap_or(name)
        .contains(".wal.sha-")
}

/// The name nanos of the segment a sidecar file copies (`<name>.sha-<hex>.gz`
/// or the unzipped `<name>.sha-<hex>` gives `<name>`'s nanos). Pure.
#[must_use]
pub fn sidecar_segment_nanos(file: &str) -> Option<u128> {
    if !is_local_sidecar(file) {
        return None;
    }
    let name = file.rsplit('/').next()?;
    let name = name.strip_suffix(".gz").unwrap_or(name);
    let (seg, _hex) = name.split_once(".sha-")?;
    segment_name_nanos(seg)
}

/// The segment files and sidecar copies in `dir` (see [`WalFolder`]). Other
/// files are ignored. O(files log files).
///
/// # Errors
/// The folder could not be listed ([`InputsError::Io`]).
pub fn list_wal_inputs(dir: &Path) -> std::result::Result<WalFolder, InputsError> {
    let mut out = WalFolder::default();
    let rd = std::fs::read_dir(dir)
        .with_context(|| format!("list {}", dir.display()))
        .map_err(InputsError::Io)?;
    for entry in rd {
        let path = entry
            .with_context(|| format!("list {}", dir.display()))
            .map_err(InputsError::Io)?
            .path();
        let file = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if is_local_sidecar(file) {
            if let Some(n) = sidecar_segment_nanos(file) {
                out.sidecars.push((file.to_owned(), n));
            }
            continue;
        }
        if let Some((_, n)) = input_segment_name(&path)
            && path.is_file()
        {
            out.segments.push((n, path));
        }
    }
    out.segments
        .sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    out.sidecars.sort();
    Ok(out)
}

/// The IST date a segment's name falls on (the date the raw uploader files it
/// under). Pure.
#[must_use]
pub fn name_ist_date(name_nanos: u128) -> Option<chrono::NaiveDate> {
    let utc_secs = i64::try_from(name_nanos / 1_000_000_000).ok()?;
    chrono::DateTime::from_timestamp(
        utc_secs.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64),
        0,
    )
    .map(|d| d.date_naive())
}

/// The inputs a replay of one day reads ([`select_day_inputs`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DayInputs {
    /// The chosen segment files, in name order.
    pub paths: Vec<PathBuf>,
    /// How many of them are named on the day itself (0 means the day has
    /// no segment, whatever the previous date holds).
    pub day_segments: usize,
    /// Segment files of other days, not read.
    pub left_out: usize,
}

/// The inputs a replay of `day` reads from a [`WalFolder`], by the same rule
/// as the bucket listing (`raw_frame_fetch::plan_day_listing`): every segment
/// named on `day` (IST), plus the newest one named on the PREVIOUS date (a
/// day's first frames can sit there; the bucket files a segment under the IST
/// date of its name and lists only `raw-frames/<D-1>/` for it, so a segment
/// named two or more days before is never read, after a weekend included).
/// Segments of other days are never read.
///
/// Refused only for what is actually read, as the bucket refuses it: a chosen
/// segment held twice (plain and gzipped), or a sidecar copy of a segment
/// named on the day or of the chosen previous one. A duplicate or sidecar of
/// a segment that is not read is ignored. O(segments + sidecars).
///
/// # Errors
/// [`InputsError::Duplicate`] or [`InputsError::Sidecars`].
pub fn select_day_inputs(
    folder: WalFolder,
    day: chrono::NaiveDate,
) -> std::result::Result<DayInputs, InputsError> {
    let total = folder.segments.len();
    let prev_date = day.pred_opt();
    let prev = folder
        .segments
        .iter()
        .rfind(|(n, _)| prev_date.is_some() && name_ist_date(*n) == prev_date)
        .map(|(n, _)| *n);
    let is_read = |n: u128| Some(n) == prev || name_ist_date(n) == Some(day);
    let sidecars: Vec<String> = folder
        .sidecars
        .iter()
        .filter(|(_, n)| is_read(*n))
        .map(|(f, _)| f.clone())
        .collect();
    if !sidecars.is_empty() {
        return Err(InputsError::Sidecars(sidecars));
    }
    let chosen: Vec<(u128, PathBuf)> = folder
        .segments
        .into_iter()
        .filter(|(n, _)| is_read(*n))
        .collect();
    if let Some(w) = chosen.windows(2).find(|w| w[0].0 == w[1].0) {
        let name = input_segment_name(&w[0].1).map_or_else(String::new, |(n, _)| n);
        return Err(InputsError::Duplicate(name));
    }
    let day_segments = chosen
        .iter()
        .filter(|(n, _)| name_ist_date(*n) == Some(day))
        .count();
    let left_out = total.saturating_sub(chosen.len());
    Ok(DayInputs {
        paths: chosen.into_iter().map(|(_, p)| p).collect(),
        day_segments,
        left_out,
    })
}

/// Reads the main-feed frames of one segment (a `.gz` input is unzipped into
/// `staging` first and removed after). Returns the read and the segment's
/// unzipped size in bytes (what was held in memory). O(bytes); memory about
/// the segment plus its frames.
///
/// # Errors
/// The file could not be unzipped or read.
pub fn read_main_feed_segment(path: &Path, staging: Option<&Path>) -> Result<(SegmentRead, u64)> {
    let is_gz = path.extension().is_some_and(|e| e == "gz");
    if !is_gz {
        let bytes = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        let read = read_segment_frames_matching(path, &|_, ep| ep == WalEndpoint::MainFeed)?;
        return Ok((read, bytes));
    }
    let Some(staging) = staging else {
        anyhow::bail!("{} is gzipped: pass --staging <dir>", path.display());
    };
    let (name, _) = input_segment_name(path).context("not a segment name")?;
    let wal = staging.join(name);
    let bytes = tickvault_storage::raw_frame_fetch::gunzip_to(path, &wal)?;
    let read = read_segment_frames_matching(&wal, &|_, ep| ep == WalEndpoint::MainFeed);
    std::fs::remove_file(&wal).with_context(|| format!("remove {}", wal.display()))?;
    Ok((read?, bytes))
}

/// One tick packet of a frame.
#[derive(Debug, Clone, Copy)]
pub struct FrameTick {
    pub tick: ParsedTick,
    /// Best depth level of a Full packet.
    pub best: Option<MarketDepthLevel>,
    /// The packet's byte range in the frame (the backup dedup reads it).
    pub start: usize,
    pub end: usize,
}

/// What walking one frame found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameWalk {
    pub packets: u32,
    /// Bytes the walk gave up on (unknown code, partial packet), as the drain.
    pub abandoned_bytes: u64,
}

/// Walks one main-feed frame packet by packet with the live drain's rules
/// (`dhan_feed_stack::drain_main_feed_frame`): the same packet sizes, the
/// same skip of an unknown packet whose stamped length lands on a known one,
/// and the same give-up on anything else. Tick packets go to `out`.
/// O(packets), at most `MAX_PACKETS_PER_FRAME`.
pub fn walk_frame(bytes: &[u8], received_at_nanos: i64, out: &mut Vec<FrameTick>) -> FrameWalk {
    let mut w = FrameWalk::default();
    let mut offset = 0usize;
    let abandon = |offset: usize| bytes.len().saturating_sub(offset) as u64;
    while offset < bytes.len() {
        let Some(len) = main_feed_packet_len(&bytes[offset..]) else {
            if let Some(next) = unknown_packet_skip(bytes, offset) {
                offset = next;
                w.packets = w.packets.saturating_add(1);
                if w.packets >= MAX_PACKETS_PER_FRAME {
                    w.abandoned_bytes = abandon(offset);
                    return w;
                }
                continue;
            }
            w.abandoned_bytes = abandon(offset);
            return w;
        };
        let end = offset.saturating_add(len);
        if end > bytes.len() {
            w.abandoned_bytes = abandon(offset);
            return w;
        }
        match dispatch_frame_uncounted(&bytes[offset..end], received_at_nanos) {
            Ok(ParsedFrame::Tick(t)) => out.push(FrameTick {
                tick: t,
                best: None,
                start: offset,
                end,
            }),
            Ok(ParsedFrame::TickWithDepth(t, levels)) => out.push(FrameTick {
                tick: t,
                best: levels.first().copied(),
                start: offset,
                end,
            }),
            _ => {}
        }
        offset = end;
        w.packets = w.packets.saturating_add(1);
        if w.packets >= MAX_PACKETS_PER_FRAME && offset < bytes.len() {
            w.abandoned_bytes = abandon(offset);
            return w;
        }
    }
    w
}
// O(1) EXEMPT: end
