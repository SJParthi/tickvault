//! The fsynced write-ahead INTENT LEDGER — the safety centerpiece
//! (design §4.5). ORD-PR-2: the ledger is the ONLY I/O in the pure core.
//!
//! # Contract
//! - Durable append-only NDJSON journal
//!   `data/orders/groww-intents-YYYYMMDD.ndjson` (IST date; the directory is
//!   config `ledger_dir`). **fsync per append; a mutation whose intent
//!   cannot be durably recorded is REFUSED** (`LedgerUnavailable` —
//!   write-ahead discipline; persist-class failures log at `error!`).
//! - Event-sourced phases per intent:
//!   `recorded → sent → acked | rejected | ambiguous → resolved_landed |
//!   resolved_not_landed | replayed | unresolved`.
//! - [`IntentReceipt`] is the TYPE-STATE token: produced ONLY by a
//!   successful fsynced `recorded` append; the ORD-PR-3 mutating transport
//!   fns take `&IntentReceipt`, so a mutating send without a durable intent
//!   record is a COMPILE error. Not `Clone`/`Copy`; constructor private.
//! - Replay on open (boot / same-day crash restart / day rollover):
//!   [`IntentLedger::open`] directory-scans EVERY `groww-intents-*.ndjson`
//!   file and replays them oldest-first (HIGH-4) — open intents,
//!   `MutationInFlight` / `DuplicateIntent` indexes, and the max reference
//!   sequence are rebuilt ACROSS files, so a prior-day open intent still
//!   blocks a same-order mutation after the close boundary (the cross-close
//!   classification in [`super::reconcile`] consumes the result).
//! - Torn-tail policy (HIGH-3): ANY unterminated final segment — parseable
//!   or not — is a torn tail, TOLERATED + TRUNCATED on open (its fsync
//!   never ACKed a receipt, so truncation is safe and prevents append
//!   concatenation onto a partial line). A newline-TERMINATED line that
//!   fails to parse — interior OR final — is committed corruption ⇒
//!   FAIL-LOUD (`InteriorCorruption`): the caller enters reconcile-only
//!   mode and refuses mutations (hostile F-9).
//! - An intent whose LAST durable phase is `recorded` at replay provably
//!   never left the box ⇒ classified [`BootIntentClass::NeverSent`].
//! - Appends always target TODAY's file — the caller passes its CURRENT IST
//!   date per append; on a mid-session date change subsequent appends
//!   switch to the new day's file and the sequence continues monotonically
//!   (HIGH-4). A date OLDER than the active file's is REFUSED fail-loud
//!   (`BackwardDate`, R2-MEDIUM-3a — clock skew / caller bug; later-phase
//!   records must never land in an older-dated file).
//! - Retention: ledger files are order-lifecycle records — retained
//!   (SEBI-5y-class), NO sweeper (design §4.5; PR-0 operator question).
//!
//! ⚠ SYNCHRONOUS fsync I/O BY DESIGN: every append blocks on `sync_data`.
//! The executor MUST call the ledger via `tokio::task::spawn_blocking`
//! (ORD-PR-3 wires + ratchets this) — never from an async hot task.
//!
//! Filename prefixes reserved: `groww-intents-*` = this session;
//! `groww-gtt-intents-*` = Session 2 (collision registry).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use super::reference_id::{self, IstDate};
use super::types::GrowwOmsError;

/// The ledger filename prefix (collision-registry reserved).
pub const LEDGER_FILE_PREFIX: &str = "groww-intents-";

/// What kind of mutation an intent journals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    /// `POST /v1/order/create`.
    Place,
    /// `POST /v1/order/modify`.
    Modify,
    /// `POST /v1/order/cancel`.
    Cancel,
}

impl IntentKind {
    /// Stable audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Place => "place",
            Self::Modify => "modify",
            Self::Cancel => "cancel",
        }
    }
}

/// Event-sourced intent phase (design §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentPhase {
    /// Durably journaled, nothing sent yet.
    Recorded,
    /// The HTTP send is about to leave / left the box.
    Sent,
    /// Broker accepted (2xx SUCCESS + usable payload).
    Acked,
    /// Definitive reject (400-class + well-shaped FAILURE envelope).
    Rejected,
    /// Outcome unknown — the §4.7 resolution ladder owns it.
    Ambiguous,
    /// Ladder verdict: the mutation landed at the broker.
    ResolvedLanded,
    /// Ladder verdict: the mutation provably never landed.
    ResolvedNotLanded,
    /// A bounded replay of the same intent (same reference id) was issued.
    Replayed,
    /// Ladder exhausted — operator action required (Critical class).
    Unresolved,
}

impl IntentPhase {
    /// Stable audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Sent => "sent",
            Self::Acked => "acked",
            Self::Rejected => "rejected",
            Self::Ambiguous => "ambiguous",
            Self::ResolvedLanded => "resolved_landed",
            Self::ResolvedNotLanded => "resolved_not_landed",
            Self::Replayed => "replayed",
            Self::Unresolved => "unresolved",
        }
    }

    /// Whether this phase SETTLES the intent. Non-terminal intents block a
    /// new place on the same reference id (`DuplicateIntent`) and a new
    /// mutation on the same order (`MutationInFlight`). `Unresolved` is
    /// deliberately NOT terminal — an exhausted ladder still blocks new
    /// mutations until the operator (or the token-recovery re-arm) settles
    /// it (hostile F-1: the refusal is pinned on the OPEN intent).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Acked | Self::Rejected | Self::ResolvedLanded | Self::ResolvedNotLanded
        )
    }
}

/// One NDJSON ledger line — an intent-phase event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentRecord {
    /// Unique per-intent id (never recycled; a cancel RESEND is a NEW
    /// intent linked via [`IntentRecord::linked_intent_id`] — hostile F-15).
    pub intent_id: String,
    /// The wire `order_reference_id` this intent rides (for a place: its
    /// own generated id; for modify/cancel: the id identifying the intent).
    pub reference_id: String,
    /// place / modify / cancel.
    pub kind: IntentKind,
    /// The phase this line records.
    pub phase: IntentPhase,
    /// The broker order id, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groww_order_id: Option<String>,
    /// `"paper"` / `"live"` — mode-tagged forensics.
    pub mode: String,
    /// Event time, epoch milliseconds (caller-stamped).
    pub ts_ms: i64,
    /// Free-form classification detail (GA code, ladder step, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The prior intent this one supersedes/links (cancel resend chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_intent_id: Option<String>,
}

/// Ledger-level failure.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// Filesystem failure (open/write/fsync/read).
    #[error("ledger I/O failure: {0}")]
    Io(#[from] std::io::Error),
    /// A record failed to serialize (should be unreachable; fail-loud).
    #[error("ledger serialize failure: {0}")]
    Serialize(String),
    /// A newline-TERMINATED ledger line failed to parse — the write
    /// completed, so this is COMMITTED corruption (interior or final line
    /// alike; HIGH-3): fail-closed — mutations refused, reconcile-only mode
    /// (hostile F-9).
    #[error("ledger interior corruption at line {line_no}")]
    InteriorCorruption {
        /// 1-based line number of the corrupt terminated line.
        line_no: usize,
    },
    /// An append targeted a date OLDER than the active file's date
    /// (R2-MEDIUM-3a): clock skew or a caller bug — later-phase records
    /// must never land in an older-dated file (they would replay BEFORE
    /// the phases they supersede on the next oldest-first boot scan). The
    /// append is REFUSED fail-loud; the mutation is refused upstream.
    #[error(
        "ledger append refused: date {attempted_yymmdd} is older than the \
         active file date {active_yymmdd} (clock skew or caller bug)"
    )]
    BackwardDate {
        /// The refused append's 6-digit `yymmdd` date block.
        attempted_yymmdd: u32,
        /// The active file's 6-digit `yymmdd` date block.
        active_yymmdd: u32,
    },
}

/// Type-state proof that an intent was durably journaled (fsynced
/// `recorded` line). NOT `Clone`/`Copy`; only [`IntentLedger::record_intent`]
/// constructs it. ORD-PR-3's mutating transport fns take `&IntentReceipt`.
#[derive(Debug)]
pub struct IntentReceipt {
    intent_id: String,
}

impl IntentReceipt {
    /// The journaled intent id this receipt proves.
    #[must_use]
    pub fn intent_id(&self) -> &str {
        &self.intent_id
    }
}

/// A new intent to journal (phase is forced to `recorded` by the ledger).
#[derive(Debug, Clone)]
pub struct NewIntent {
    /// Unique intent id (the executor generates it via
    /// [`super::reference_id::generate_reference_id`]).
    pub intent_id: String,
    /// The wire reference id (== `intent_id` for places).
    pub reference_id: String,
    /// place / modify / cancel.
    pub kind: IntentKind,
    /// Target order for modify/cancel (`None` for a place).
    pub groww_order_id: Option<String>,
    /// `"paper"` / `"live"`.
    pub mode: String,
    /// Event time, epoch milliseconds.
    pub ts_ms: i64,
    /// Optional link to a prior intent (cancel resend chain, F-15).
    pub linked_intent_id: Option<String>,
}

/// In-memory summary of one replayed/journaled intent (the twin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentSummary {
    /// The intent id.
    pub intent_id: String,
    /// The wire reference id.
    pub reference_id: String,
    /// place / modify / cancel.
    pub kind: IntentKind,
    /// LAST durable phase.
    pub last_phase: IntentPhase,
    /// Broker order id, once any line carried it.
    pub groww_order_id: Option<String>,
    /// ts of the last line.
    pub last_ts_ms: i64,
}

/// Boot classification of an intent after replay (design §4.5/§4.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BootIntentClass {
    /// Last durable phase `recorded` — provably never left the box ⇒
    /// settle as `resolved_not_landed(never_sent)`.
    NeverSent,
    /// `sent` / `ambiguous` / `replayed` — the resolution ladder must run
    /// BEFORE any new mutation is accepted.
    NeedsResolution,
    /// `unresolved` carried across a restart — operator-action class.
    NeedsOperator,
    /// A terminal phase — nothing to do.
    Settled,
}

/// Classify a replayed intent's last durable phase (pure).
#[must_use]
pub fn classify_replayed_phase(phase: IntentPhase) -> BootIntentClass {
    match phase {
        IntentPhase::Recorded => BootIntentClass::NeverSent,
        IntentPhase::Sent | IntentPhase::Ambiguous | IntentPhase::Replayed => {
            BootIntentClass::NeedsResolution
        }
        IntentPhase::Unresolved => BootIntentClass::NeedsOperator,
        IntentPhase::Acked
        | IntentPhase::Rejected
        | IntentPhase::ResolvedLanded
        | IntentPhase::ResolvedNotLanded => BootIntentClass::Settled,
    }
}

/// The ledger filename for an IST date: `groww-intents-YYYYMMDD.ndjson`.
#[must_use]
pub fn ledger_file_name(date: IstDate) -> String {
    format!(
        "{LEDGER_FILE_PREFIX}{:04}{:02}{:02}.ndjson",
        date.year, date.month, date.day
    )
}

/// The result of replaying one ledger file (pure output of the parse pass).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerReplay {
    /// Last-phase summary per intent id, in first-seen order key space.
    pub intents: HashMap<String, IntentSummary>,
    /// Whether an UNTERMINATED final segment (parseable or not — HIGH-3)
    /// was tolerated (counted; truncated on open).
    pub torn_tail: bool,
    /// Parsed line count (excludes the torn tail).
    pub lines: usize,
    /// Byte offset where a torn tail begins (== clean length of the file).
    pub clean_len: u64,
    /// Max reference-id sequence observed across ALL lines whose
    /// `reference_id` matches OUR generated shape — crash-safe sequence
    /// resumption (design §4.4).
    pub max_sequence: Option<u32>,
}

/// Replay/parse a ledger byte buffer (pure — the file half of replay is in
/// [`IntentLedger::open`]).
///
/// Final-segment policy (HIGH-3):
/// - ANY unterminated final segment (missing trailing `\n`) is a torn tail —
///   even when it HAPPENS to parse: its fsync never ACKed a receipt, so it
///   is never counted and the caller truncates it (this also prevents a
///   later append concatenating onto it: the `{rec}{rec}\n` poisoning).
/// - A newline-TERMINATED line that fails to parse is COMMITTED corruption —
///   fail-loud (`InteriorCorruption`), final line included.
pub fn replay_bytes(buf: &[u8]) -> Result<LedgerReplay, LedgerError> {
    let mut intents: HashMap<String, IntentSummary> = HashMap::new();
    let mut lines = 0_usize;
    let mut torn_tail = false;
    let mut clean_len = 0_u64;
    let mut max_sequence: Option<u32> = None;

    // Split into newline-terminated segments; ONLY the final segment can be
    // unterminated by construction.
    let mut segments: Vec<(&[u8], bool)> = Vec::new(); // (bytes, newline_terminated)
    let mut start = 0_usize;
    for (i, b) in buf.iter().enumerate() {
        if *b == b'\n' {
            segments.push((&buf[start..i], true));
            start = i + 1;
        }
    }
    if start < buf.len() {
        segments.push((&buf[start..], false));
    }

    for (idx, (seg, terminated)) in segments.into_iter().enumerate() {
        if !terminated {
            // Torn tail (HIGH-3): unterminated ⇒ never durable-ACKed —
            // tolerated + counted, NEVER parsed (a parseable-but-
            // unterminated record must not be adopted), truncated by the
            // caller. Only the final segment can land here.
            torn_tail = true;
            continue;
        }
        // Skip genuinely empty terminated lines (defensive; we never write them).
        if seg.is_empty() {
            clean_len += 1;
            continue;
        }
        let parsed: Result<IntentRecord, _> = serde_json::from_slice(seg);
        match parsed {
            Ok(rec) => {
                lines += 1;
                clean_len += seg.len() as u64 + 1;
                if let Some(seq) = reference_id::extract_sequence(&rec.reference_id) {
                    max_sequence = Some(max_sequence.map_or(seq, |m| m.max(seq)));
                }
                // LOW: borrow-first — no key clone on the hit path.
                match intents.get_mut(&rec.intent_id) {
                    Some(entry) => {
                        entry.last_phase = rec.phase;
                        entry.last_ts_ms = rec.ts_ms;
                        if rec.groww_order_id.is_some() {
                            entry.groww_order_id = rec.groww_order_id;
                        }
                    }
                    None => {
                        intents.insert(
                            rec.intent_id.clone(),
                            IntentSummary {
                                intent_id: rec.intent_id,
                                reference_id: rec.reference_id,
                                kind: rec.kind,
                                last_phase: rec.phase,
                                groww_order_id: rec.groww_order_id,
                                last_ts_ms: rec.ts_ms,
                            },
                        );
                    }
                }
            }
            Err(_) => {
                // A TERMINATED line failed to parse — committed corruption,
                // fail-closed regardless of position (HIGH-3 / F-9).
                // GROWW-ORD-06 emit lands in ORD-PR-3 once the variants exist.
                error!(
                    target: "groww_ord",
                    line_no = idx + 1,
                    "groww intent ledger: terminated line failed to parse — \
                     fail-closed (mutations must be refused; reconcile-only)"
                );
                return Err(LedgerError::InteriorCorruption { line_no: idx + 1 });
            }
        }
    }

    Ok(LedgerReplay {
        intents,
        torn_tail,
        lines,
        clean_len,
        max_sequence,
    })
}

/// Whether a directory entry name is one of OUR ledger files
/// (`groww-intents-YYYYMMDD.ndjson`; the reserved `groww-gtt-intents-*`
/// prefix of Session 2 does NOT match).
#[must_use]
fn is_ledger_file_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(LEDGER_FILE_PREFIX) else {
        return false;
    };
    let Some(date) = rest.strip_suffix(".ndjson") else {
        return false;
    };
    date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit())
}

/// Merge one file's replay summaries into the cross-file twin — files are
/// fed OLDEST-FIRST, so a later file's phase overrides an earlier one's
/// for the same intent (an intent recorded yesterday and resolved today).
///
/// Phase-regression guard (R2-MEDIUM-3b — the MEDIUM-8 append-time rule
/// mirrored at MERGE level): a replayed record can never move an intent's
/// `last_phase` from TERMINAL back to NON-terminal, regardless of file
/// order — a stale/adversarial later file must not resurrect a settled
/// intent (which would re-arm the `DuplicateIntent`/`MutationInFlight`
/// blocks against a provably-settled order). Terminality — not the
/// caller-stamped `ts_ms` — is the primary comparator: `ts_ms` clocks can
/// skew across restarts, while phase terminality is the same invariant
/// `append_phase` enforces at write time (a terminal→non-terminal line can
/// only exist adversarially). A newly-learned order id is still adopted
/// (monotone knowledge). Terminal → terminal (a ladder re-verdict) still
/// follows file order.
fn merge_summaries(
    into: &mut HashMap<String, IntentSummary>,
    from: HashMap<String, IntentSummary>,
) {
    for (id, s) in from {
        match into.get_mut(&id) {
            Some(existing) => {
                if existing.last_phase.is_terminal() && !s.last_phase.is_terminal() {
                    // Keep the settled phase + its ts; adopt only an order
                    // id the settled twin lacked.
                    warn!(
                        target: "groww_ord",
                        intent_id = %id,
                        settled_phase = existing.last_phase.as_str(),
                        stale_phase = s.last_phase.as_str(),
                        "groww intent ledger: cross-file replay tried to \
                         regress a settled intent to a non-terminal phase — \
                         keeping the terminal phase (R2-MEDIUM-3b)"
                    );
                    if existing.groww_order_id.is_none() && s.groww_order_id.is_some() {
                        existing.groww_order_id = s.groww_order_id;
                    }
                    continue;
                }
                existing.last_phase = s.last_phase;
                existing.last_ts_ms = s.last_ts_ms;
                if s.groww_order_id.is_some() {
                    existing.groww_order_id = s.groww_order_id;
                }
            }
            None => {
                into.insert(id, s);
            }
        }
    }
}

/// The write-ahead intent ledger: an open NDJSON file + the in-memory twin.
#[derive(Debug)]
pub struct IntentLedger {
    /// The ledger directory (appends open per-date files under it).
    ledger_dir: PathBuf,
    file: File,
    path: PathBuf,
    /// The IST date the active append file belongs to.
    active_date: IstDate,
    /// The in-memory twin: last phase per intent id.
    intents: HashMap<String, IntentSummary>,
    /// Open (non-terminal) place reference ids → intent id (DuplicateIntent
    /// refusal index, design §4.4).
    open_place_refs: HashMap<String, String>,
    /// Open (non-terminal) mutation intents per groww_order_id
    /// (MutationInFlight serialization invariant, hostile F-3).
    open_by_order: HashMap<String, String>,
    /// Max OUR-shape reference sequence seen (crash-safe resumption).
    max_sequence: Option<u32>,
    /// Whether opening tolerated (and truncated) a torn tail in ANY file.
    torn_tail_tolerated: bool,
}

impl IntentLedger {
    /// Open the ledger under `ledger_dir` with `today` as the append-target
    /// IST date, replaying EVERY `groww-intents-*.ndjson` file in the
    /// directory oldest-first (HIGH-4: day rollover — open intents, the
    /// `MutationInFlight`/`DuplicateIntent` indexes, and `max_sequence` are
    /// rebuilt ACROSS files; prior-day non-terminal intents feed
    /// [`super::reconcile::classify_cross_close_open_intents`]).
    ///
    /// Torn-tail policy per [`replay_bytes`] (HIGH-3): any file's
    /// unterminated final segment is tolerated, WARNED, counted, and
    /// TRUNCATED away (its fsync never ACKed a receipt — no
    /// `IntentReceipt` exists for it); a terminated-but-unparseable line in
    /// ANY file fails loud.
    pub fn open(ledger_dir: &Path, today: IstDate) -> Result<Self, LedgerError> {
        std::fs::create_dir_all(ledger_dir)?;

        // --- Directory scan: every ledger file, oldest first (the
        // zero-padded YYYYMMDD block makes lexicographic == chronological).
        let mut ledger_files: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(ledger_dir)? {
            let entry = entry?;
            if entry.file_name().to_str().is_some_and(is_ledger_file_name) {
                ledger_files.push(entry.path());
            }
        }
        ledger_files.sort();

        let mut intents: HashMap<String, IntentSummary> = HashMap::new();
        let mut max_sequence: Option<u32> = None;
        let mut torn_any = false;
        for file_path in &ledger_files {
            let mut f = OpenOptions::new().read(true).write(true).open(file_path)?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            let replay = match replay_bytes(&buf) {
                Ok(r) => r,
                Err(e) => {
                    error!(
                        target: "groww_ord",
                        path = %file_path.display(),
                        err = %e,
                        "groww intent ledger: replay failed for ledger file"
                    );
                    return Err(e);
                }
            };
            if replay.torn_tail {
                warn!(
                    target: "groww_ord",
                    path = %file_path.display(),
                    clean_len = replay.clean_len,
                    "groww intent ledger: torn final segment tolerated at open — \
                     truncating to last durable record (crash-mid-append shape)"
                );
                // Truncate the torn fragment so future replays stay clean and
                // future appends can never concatenate onto a partial line.
                // R2-LOW-4: fsync (sync_all — set_len is a LENGTH-metadata
                // change, so sync_data alone is not guaranteed to persist
                // it) so the truncation survives a power loss between this
                // open and the next append's own fsync; applies to the
                // active file AND prior-day files alike (this loop covers
                // every replayed file). Without it, a crash here could
                // resurrect the torn fragment and a later append would
                // concatenate onto it — the {rec}{rec}\n poisoning the
                // truncation exists to prevent.
                f.set_len(replay.clean_len)?;
                f.sync_all()?;
                torn_any = true;
            }
            merge_summaries(&mut intents, replay.intents);
            if let Some(seq) = replay.max_sequence {
                max_sequence = Some(max_sequence.map_or(seq, |m| m.max(seq)));
            }
        }

        // --- Today's append handle (created if absent; truncation above —
        // if today's file was torn — already happened, and append mode
        // always writes at the current end).
        let path = ledger_dir.join(ledger_file_name(today));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut ledger = Self {
            ledger_dir: ledger_dir.to_path_buf(),
            file,
            path,
            active_date: today,
            intents: HashMap::new(),
            open_place_refs: HashMap::new(),
            open_by_order: HashMap::new(),
            max_sequence,
            torn_tail_tolerated: torn_any,
        };
        for summary in intents.into_values() {
            ledger.index_summary(summary);
        }
        Ok(ledger)
    }

    /// Point the append handle at `date`'s file (HIGH-4 mid-session
    /// rollover) — a no-op while the date is unchanged. A date OLDER than
    /// the active file's is REFUSED fail-loud (R2-MEDIUM-3a): a backward
    /// clock (or caller bug) must never write later-phase records into an
    /// older-dated file, where the next oldest-first boot scan would replay
    /// them BEFORE the phases they supersede.
    fn ensure_active_file(&mut self, date: IstDate) -> Result<(), LedgerError> {
        if date == self.active_date {
            return Ok(());
        }
        if date.yymmdd_num() < self.active_date.yymmdd_num() {
            let err = LedgerError::BackwardDate {
                attempted_yymmdd: date.yymmdd_num(),
                active_yymmdd: self.active_date.yymmdd_num(),
            };
            // Fail-loud: a persist-class refusal logs at error!, and the
            // mutation is refused upstream (write-ahead discipline).
            // GROWW-ORD-06 emit lands in ORD-PR-3 once the variants exist.
            error!(
                target: "groww_ord",
                attempted = date.yymmdd_num(),
                active = self.active_date.yymmdd_num(),
                "groww intent ledger: backward-dated append refused — \
                 clock skew or caller bug; mutation refused"
            );
            return Err(err);
        }
        let path = self.ledger_dir.join(ledger_file_name(date));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.file = file;
        self.path = path;
        self.active_date = date;
        Ok(())
    }

    fn index_summary(&mut self, summary: IntentSummary) {
        if !summary.last_phase.is_terminal() {
            if summary.kind == IntentKind::Place {
                self.open_place_refs
                    .insert(summary.reference_id.clone(), summary.intent_id.clone());
            }
            if let Some(order_id) = &summary.groww_order_id {
                self.open_by_order
                    .insert(order_id.clone(), summary.intent_id.clone());
            }
        }
        self.intents.insert(summary.intent_id.clone(), summary);
    }

    /// Journal a NEW intent (phase `recorded`), returning the type-state
    /// [`IntentReceipt`] on durable success. `date` is the caller's CURRENT
    /// IST date — the append targets that date's file (HIGH-4 mid-session
    /// rollover). Refusals (all pre-I/O):
    /// - intent id already used (`InvalidOrderField` — ids never recycle);
    /// - an OPEN place already holds this reference id (`DuplicateIntent`);
    /// - an OPEN mutation already targets this order (`MutationInFlight`).
    ///
    /// An append/fsync failure REFUSES the mutation (`LedgerUnavailable`).
    pub fn record_intent(
        &mut self,
        new: NewIntent,
        date: IstDate,
    ) -> Result<IntentReceipt, GrowwOmsError> {
        if self.intents.contains_key(&new.intent_id) {
            return Err(GrowwOmsError::InvalidOrderField(format!(
                "intent id {} already journaled (ids never recycle)",
                new.intent_id
            )));
        }
        if new.kind == IntentKind::Place && self.open_place_refs.contains_key(&new.reference_id) {
            return Err(GrowwOmsError::DuplicateIntent {
                reference_id: new.reference_id,
            });
        }
        if let Some(order_id) = &new.groww_order_id
            && self.open_by_order.contains_key(order_id)
        {
            return Err(GrowwOmsError::MutationInFlight {
                groww_order_id: order_id.clone(),
            });
        }
        self.ensure_active_file(date)
            .map_err(|e| GrowwOmsError::LedgerUnavailable(e.to_string()))?;
        let record = IntentRecord {
            intent_id: new.intent_id.clone(),
            reference_id: new.reference_id.clone(),
            kind: new.kind,
            phase: IntentPhase::Recorded,
            groww_order_id: new.groww_order_id.clone(),
            mode: new.mode.clone(),
            ts_ms: new.ts_ms,
            detail: None,
            linked_intent_id: new.linked_intent_id.clone(),
        };
        self.append_fsync(&record)
            .map_err(|e| GrowwOmsError::LedgerUnavailable(e.to_string()))?;
        if let Some(seq) = reference_id::extract_sequence(&new.reference_id) {
            self.max_sequence = Some(self.max_sequence.map_or(seq, |m| m.max(seq)));
        }
        self.index_summary(IntentSummary {
            intent_id: new.intent_id.clone(),
            reference_id: new.reference_id,
            kind: new.kind,
            last_phase: IntentPhase::Recorded,
            groww_order_id: new.groww_order_id,
            last_ts_ms: new.ts_ms,
        });
        Ok(IntentReceipt {
            intent_id: new.intent_id,
        })
    }

    /// Journal a phase update for an existing intent (resolution-state
    /// update). `date` is the caller's CURRENT IST date — the append targets
    /// that date's file (HIGH-4). The `receipt`-less form exists because
    /// replayed intents (boot) have no live receipt; ORD-PR-3's SEND path
    /// uses [`IntentLedger::append_phase_for_receipt`].
    ///
    /// MEDIUM-8: a NON-terminal phase after a terminal one is REFUSED
    /// (typed, fail-loud) — a settled intent can never silently re-open the
    /// `DuplicateIntent`/`MutationInFlight` blocks.
    pub fn append_phase(
        &mut self,
        intent_id: &str,
        phase: IntentPhase,
        ts_ms: i64,
        date: IstDate,
        groww_order_id: Option<String>,
        detail: Option<String>,
    ) -> Result<(), GrowwOmsError> {
        let Some(existing) = self.intents.get(intent_id) else {
            return Err(GrowwOmsError::InvalidOrderField(format!(
                "unknown intent id {intent_id}"
            )));
        };
        if existing.last_phase.is_terminal() && !phase.is_terminal() {
            return Err(GrowwOmsError::IntentAlreadySettled {
                intent_id: intent_id.to_owned(),
                settled_phase: existing.last_phase.as_str(),
                attempted_phase: phase.as_str(),
            });
        }
        // Detach the record fields before the &mut file switch below.
        let (rec_intent_id, rec_reference_id, rec_kind) = (
            existing.intent_id.clone(),
            existing.reference_id.clone(),
            existing.kind,
        );
        self.ensure_active_file(date)
            .map_err(|e| GrowwOmsError::LedgerUnavailable(e.to_string()))?;
        let record = IntentRecord {
            intent_id: rec_intent_id,
            reference_id: rec_reference_id,
            kind: rec_kind,
            phase,
            groww_order_id: groww_order_id.clone(),
            mode: String::new(), // mode is stamped on the recorded line
            ts_ms,
            detail,
            linked_intent_id: None,
        };
        self.append_fsync(&record)
            .map_err(|e| GrowwOmsError::LedgerUnavailable(e.to_string()))?;

        // Update the twin + open indexes (existence checked above; the
        // fallback tuple is unreachable but keeps this panic-free).
        let mut updated: Option<(String, IntentKind, Option<String>)> = None;
        if let Some(s) = self.intents.get_mut(intent_id) {
            s.last_phase = phase;
            s.last_ts_ms = ts_ms;
            if groww_order_id.is_some() {
                s.groww_order_id = groww_order_id;
            }
            updated = Some((s.reference_id.clone(), s.kind, s.groww_order_id.clone()));
        }
        let Some((reference_id, kind, prior_order_id)) = updated else {
            return Ok(()); // unreachable by construction; fail-soft
        };
        if phase.is_terminal() {
            if kind == IntentKind::Place {
                self.open_place_refs.remove(&reference_id);
            }
            if let Some(order_id) = &prior_order_id {
                self.open_by_order.remove(order_id);
            }
        } else if let Some(order_id) = &prior_order_id {
            // A newly-learned order id joins the in-flight index.
            self.open_by_order
                .insert(order_id.clone(), intent_id.to_owned());
        }
        Ok(())
    }

    /// The receipt-anchored phase append (the type-state SEND path).
    pub fn append_phase_for_receipt(
        &mut self,
        receipt: &IntentReceipt,
        phase: IntentPhase,
        ts_ms: i64,
        date: IstDate,
        groww_order_id: Option<String>,
        detail: Option<String>,
    ) -> Result<(), GrowwOmsError> {
        self.append_phase(
            &receipt.intent_id,
            phase,
            ts_ms,
            date,
            groww_order_id,
            detail,
        )
    }

    fn append_fsync(&mut self, record: &IntentRecord) -> Result<(), LedgerError> {
        let mut line =
            serde_json::to_vec(record).map_err(|e| LedgerError::Serialize(e.to_string()))?;
        line.push(b'\n');
        let write = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_data());
        if let Err(e) = write {
            // Persist-class failure ⇒ error!, never warn! (Rule 5 of the
            // error-level meta-guard). The mutation is REFUSED upstream.
            // GROWW-ORD-06 emit lands in ORD-PR-3 once the variants exist.
            error!(
                target: "groww_ord",
                path = %self.path.display(),
                err = %e,
                "groww intent ledger: append/fsync failed — mutation refused"
            );
            return Err(LedgerError::Io(e));
        }
        Ok(())
    }

    /// Classify every OPEN (non-terminal) intent for the boot protocol:
    /// resolve ALL of these BEFORE accepting any new mutation (design §4.9).
    #[must_use]
    pub fn classify_open_intents(&self) -> Vec<(IntentSummary, BootIntentClass)> {
        let mut out: Vec<(IntentSummary, BootIntentClass)> = self
            .intents
            .values()
            .filter(|s| !s.last_phase.is_terminal())
            .map(|s| (s.clone(), classify_replayed_phase(s.last_phase)))
            .collect();
        out.sort_by(|a, b| a.0.intent_id.cmp(&b.0.intent_id));
        out
    }

    /// Max OUR-shape reference sequence observed (ledger replay + appends).
    /// The next place uses `max_sequence() + 1` (design §4.4).
    #[must_use]
    pub fn max_sequence(&self) -> Option<u32> {
        self.max_sequence
    }

    /// Whether open() tolerated (and truncated) a torn final segment in ANY
    /// replayed ledger file.
    #[must_use]
    pub fn torn_tail_tolerated(&self) -> bool {
        self.torn_tail_tolerated
    }

    /// Number of OPEN (non-terminal) intents.
    #[must_use]
    pub fn open_intent_count(&self) -> usize {
        self.intents
            .values()
            .filter(|s| !s.last_phase.is_terminal())
            .count()
    }

    /// Look up an intent summary.
    #[must_use]
    pub fn intent(&self, intent_id: &str) -> Option<&IntentSummary> {
        self.intents.get(intent_id)
    }

    /// The ledger file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const DATE: IstDate = IstDate {
        year: 2026,
        month: 7,
        day: 15,
    };

    /// Unique temp dir per test (house pattern: std::env::temp_dir()).
    fn temp_ledger_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-groww-ord-pr2-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn place_intent(seq: u32) -> NewIntent {
        let rid = reference_id::generate_reference_id(DATE, seq, 7);
        NewIntent {
            intent_id: rid.clone(),
            reference_id: rid,
            kind: IntentKind::Place,
            groww_order_id: None,
            mode: "paper".to_owned(),
            ts_ms: 1_760_000_000_000 + i64::from(seq),
            linked_intent_id: None,
        }
    }

    #[test]
    fn test_ledger_file_name_shape() {
        assert_eq!(ledger_file_name(DATE), "groww-intents-20260715.ndjson");
    }

    // --- roundtrip: record + phases survive close/reopen ---

    #[test]
    fn test_ledger_roundtrip_across_reopen() {
        let dir = temp_ledger_dir("roundtrip");
        {
            let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
            let receipt = ledger.record_intent(place_intent(1), DATE).unwrap();
            ledger
                .append_phase_for_receipt(
                    &receipt,
                    IntentPhase::Sent,
                    1_760_000_000_100,
                    DATE,
                    None,
                    None,
                )
                .unwrap();
            ledger
                .append_phase_for_receipt(
                    &receipt,
                    IntentPhase::Acked,
                    1_760_000_000_200,
                    DATE,
                    Some("GRW1".to_owned()),
                    None,
                )
                .unwrap();
            let receipt2 = ledger.record_intent(place_intent(2), DATE).unwrap();
            ledger
                .append_phase_for_receipt(
                    &receipt2,
                    IntentPhase::Sent,
                    1_760_000_000_300,
                    DATE,
                    None,
                    None,
                )
                .unwrap();
        }
        // Reopen — the twin must match: intent 1 settled (acked), intent 2
        // open at `sent`, sequence resumed at 2.
        let ledger = IntentLedger::open(&dir, DATE).unwrap();
        assert!(!ledger.torn_tail_tolerated());
        assert_eq!(ledger.max_sequence(), Some(2));
        assert_eq!(ledger.open_intent_count(), 1);
        let open = ledger.classify_open_intents();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].0.last_phase, IntentPhase::Sent);
        assert_eq!(open[0].1, BootIntentClass::NeedsResolution);
        cleanup(&dir);
    }

    // --- duplicate / in-flight refusals ---

    #[test]
    fn test_duplicate_place_reference_refused_before_io() {
        let dir = temp_ledger_dir("dup");
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        let intent = place_intent(1);
        let reference = intent.reference_id.clone();
        ledger.record_intent(intent, DATE).unwrap();
        // Second place on the SAME reference while the first is open.
        let mut second = place_intent(1);
        second.intent_id = reference_id::generate_reference_id(DATE, 99, 1);
        second.reference_id = reference.clone();
        match ledger.record_intent(second, DATE) {
            Err(GrowwOmsError::DuplicateIntent { reference_id }) => {
                assert_eq!(reference_id, reference);
            }
            other => panic!("expected DuplicateIntent, got {other:?}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn test_place_reference_reusable_only_after_terminal_phase() {
        let dir = temp_ledger_dir("dup-term");
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        let intent = place_intent(3);
        let reference = intent.reference_id.clone();
        let intent_id = intent.intent_id.clone();
        ledger.record_intent(intent, DATE).unwrap();
        ledger
            .append_phase(&intent_id, IntentPhase::Rejected, 2, DATE, None, None)
            .unwrap();
        // Same reference id after the first intent settled: a REPLAY of the
        // same logical order reuses it via a new linked intent.
        let mut replayed = place_intent(3);
        replayed.intent_id = reference_id::generate_reference_id(DATE, 4, 4);
        replayed.reference_id = reference;
        replayed.linked_intent_id = Some(intent_id);
        assert!(ledger.record_intent(replayed, DATE).is_ok());
        cleanup(&dir);
    }

    #[test]
    fn test_mutation_in_flight_refused_per_order() {
        let dir = temp_ledger_dir("inflight");
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        let modify_id = reference_id::generate_reference_id(DATE, 10, 1);
        ledger
            .record_intent(
                NewIntent {
                    intent_id: modify_id.clone(),
                    reference_id: modify_id.clone(),
                    kind: IntentKind::Modify,
                    groww_order_id: Some("GRW9".to_owned()),
                    mode: "paper".to_owned(),
                    ts_ms: 1,
                    linked_intent_id: None,
                },
                DATE,
            )
            .unwrap();
        // A cancel on the same order while the modify is open ⇒ refused
        // (serialization invariant F-3; cancel supersedes only after the
        // modify resolves).
        let cancel_id = reference_id::generate_reference_id(DATE, 11, 1);
        match ledger.record_intent(
            NewIntent {
                intent_id: cancel_id.clone(),
                reference_id: cancel_id.clone(),
                kind: IntentKind::Cancel,
                groww_order_id: Some("GRW9".to_owned()),
                mode: "paper".to_owned(),
                ts_ms: 2,
                linked_intent_id: None,
            },
            DATE,
        ) {
            Err(GrowwOmsError::MutationInFlight { groww_order_id }) => {
                assert_eq!(groww_order_id, "GRW9");
            }
            other => panic!("expected MutationInFlight, got {other:?}"),
        }
        // After the modify settles, the cancel is accepted.
        ledger
            .append_phase(&modify_id, IntentPhase::Acked, 3, DATE, None, None)
            .unwrap();
        assert!(
            ledger
                .record_intent(
                    NewIntent {
                        intent_id: cancel_id.clone(),
                        reference_id: cancel_id,
                        kind: IntentKind::Cancel,
                        groww_order_id: Some("GRW9".to_owned()),
                        mode: "paper".to_owned(),
                        ts_ms: 4,
                        linked_intent_id: None,
                    },
                    DATE
                )
                .is_ok()
        );
        cleanup(&dir);
    }

    // --- torn tail (kill-9 mid-append shape) ---

    #[test]
    fn test_torn_tail_tolerated_truncated_and_appendable() {
        let dir = temp_ledger_dir("torn");
        let path;
        {
            let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
            let receipt = ledger.record_intent(place_intent(1), DATE).unwrap();
            ledger
                .append_phase_for_receipt(&receipt, IntentPhase::Sent, 2, DATE, None, None)
                .unwrap();
            path = ledger.path().to_path_buf();
        }
        // Simulate kill -9 mid-append: a partial JSON fragment, no newline.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(br#"{"intent_id":"TV26071500"#).unwrap();
        }
        let clean_len_before = {
            let content = std::fs::read(&path).unwrap();
            replay_bytes(&content).unwrap().clean_len
        };
        // Reopen: torn tail tolerated + truncated; earlier records intact.
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        assert!(ledger.torn_tail_tolerated());
        assert_eq!(ledger.open_intent_count(), 1);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            clean_len_before,
            "torn fragment truncated away"
        );
        // New appends land cleanly after the truncation.
        let receipt = ledger.record_intent(place_intent(2), DATE).unwrap();
        assert_eq!(receipt.intent_id().len(), 16);
        drop(ledger);
        // A third replay parses everything (no concatenation corruption).
        let ledger = IntentLedger::open(&dir, DATE).unwrap();
        assert!(!ledger.torn_tail_tolerated());
        assert_eq!(ledger.open_intent_count(), 2);
        cleanup(&dir);
    }

    // --- interior corruption (fail-loud) ---

    #[test]
    fn test_interior_corruption_fails_closed() {
        let dir = temp_ledger_dir("interior");
        let path;
        {
            let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
            ledger.record_intent(place_intent(1), DATE).unwrap();
            path = ledger.path().to_path_buf();
        }
        // Corrupt an INTERIOR line: garbage line followed by a valid line.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"\x00\x01 not json at all\n").unwrap();
            let good = IntentRecord {
                intent_id: "TVGOODLINE000001".to_owned(),
                reference_id: "TVGOODLINE000001".to_owned(),
                kind: IntentKind::Place,
                phase: IntentPhase::Recorded,
                groww_order_id: None,
                mode: "paper".to_owned(),
                ts_ms: 9,
                detail: None,
                linked_intent_id: None,
            };
            let mut line = serde_json::to_vec(&good).unwrap();
            line.push(b'\n');
            f.write_all(&line).unwrap();
        }
        match IntentLedger::open(&dir, DATE) {
            Err(LedgerError::InteriorCorruption { line_no }) => assert_eq!(line_no, 2),
            other => panic!("expected InteriorCorruption, got {other:?}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn test_corrupt_terminated_final_line_fails_loud() {
        // HIGH-3(b): a newline-TERMINATED final line that fails to parse is
        // COMMITTED corruption — fail-loud, never a tolerated torn tail.
        let dir = temp_ledger_dir("term-final-corrupt");
        let path;
        {
            let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
            ledger.record_intent(place_intent(1), DATE).unwrap();
            path = ledger.path().to_path_buf();
        }
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"\x00garbage final line, but terminated\n")
                .unwrap();
        }
        // The pure pass fails loud…
        let content = std::fs::read(&path).unwrap();
        match replay_bytes(&content) {
            Err(LedgerError::InteriorCorruption { line_no }) => assert_eq!(line_no, 2),
            other => panic!("expected InteriorCorruption, got {other:?}"),
        }
        // …and so does open().
        assert!(matches!(
            IntentLedger::open(&dir, DATE),
            Err(LedgerError::InteriorCorruption { line_no: 2 })
        ));
        cleanup(&dir);
    }

    #[test]
    fn test_parseable_unterminated_final_line_is_torn_and_truncated() {
        // HIGH-3(a): even a PARSEABLE final record without a trailing \n is
        // a torn tail — its fsync never ACKed a receipt; truncation prevents
        // the {rec}{rec}\n concatenation poisoning.
        let dir = temp_ledger_dir("parseable-torn");
        let path;
        {
            let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
            ledger.record_intent(place_intent(1), DATE).unwrap();
            path = ledger.path().to_path_buf();
        }
        let clean_len = std::fs::metadata(&path).unwrap().len();
        // A COMPLETE, valid JSON record — but no trailing newline.
        {
            let torn = IntentRecord {
                intent_id: reference_id::generate_reference_id(DATE, 77, 7),
                reference_id: reference_id::generate_reference_id(DATE, 77, 7),
                kind: IntentKind::Place,
                phase: IntentPhase::Recorded,
                groww_order_id: None,
                mode: "paper".to_owned(),
                ts_ms: 5,
                detail: None,
                linked_intent_id: None,
            };
            let line = serde_json::to_vec(&torn).unwrap(); // NO newline push
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&line).unwrap();
        }
        // Pure pass: torn, NOT counted, prefix intact.
        let content = std::fs::read(&path).unwrap();
        let replay = replay_bytes(&content).unwrap();
        assert!(replay.torn_tail, "parseable-but-unterminated is STILL torn");
        assert_eq!(replay.lines, 1, "the torn record must never be adopted");
        assert_eq!(replay.clean_len, clean_len);
        assert_eq!(
            replay.max_sequence,
            Some(1),
            "the torn record's sequence must not leak"
        );
        // Open truncates it away and blocks nothing else.
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        assert!(ledger.torn_tail_tolerated());
        assert_eq!(ledger.open_intent_count(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), clean_len);
        // Post-truncate append, then a clean re-replay.
        ledger.record_intent(place_intent(2), DATE).unwrap();
        drop(ledger);
        let ledger = IntentLedger::open(&dir, DATE).unwrap();
        assert!(!ledger.torn_tail_tolerated());
        assert_eq!(ledger.open_intent_count(), 2);
        assert_eq!(ledger.max_sequence(), Some(2));
        cleanup(&dir);
    }

    // --- kill-9 replay classification (P30/P31/P32) ---

    #[test]
    fn test_boot_replay_classification_per_last_phase() {
        let dir = temp_ledger_dir("classify");
        {
            let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
            // Intent A: recorded only ⇒ NeverSent (P31).
            ledger.record_intent(place_intent(1), DATE).unwrap();
            // Intent B: recorded + sent ⇒ NeedsResolution (P32).
            let b = ledger.record_intent(place_intent(2), DATE).unwrap();
            ledger
                .append_phase_for_receipt(&b, IntentPhase::Sent, 2, DATE, None, None)
                .unwrap();
            // Intent C: through ambiguous ⇒ NeedsResolution.
            let c = ledger.record_intent(place_intent(3), DATE).unwrap();
            ledger
                .append_phase_for_receipt(&c, IntentPhase::Sent, 3, DATE, None, None)
                .unwrap();
            ledger
                .append_phase_for_receipt(&c, IntentPhase::Ambiguous, 4, DATE, None, None)
                .unwrap();
            // Intent D: unresolved ⇒ NeedsOperator.
            let d = ledger.record_intent(place_intent(4), DATE).unwrap();
            ledger
                .append_phase_for_receipt(&d, IntentPhase::Sent, 5, DATE, None, None)
                .unwrap();
            ledger
                .append_phase_for_receipt(&d, IntentPhase::Ambiguous, 6, DATE, None, None)
                .unwrap();
            ledger
                .append_phase_for_receipt(&d, IntentPhase::Unresolved, 7, DATE, None, None)
                .unwrap();
            // Intent E: settled ⇒ absent from the open set.
            let e = ledger.record_intent(place_intent(5), DATE).unwrap();
            ledger
                .append_phase_for_receipt(&e, IntentPhase::Sent, 8, DATE, None, None)
                .unwrap();
            ledger
                .append_phase_for_receipt(
                    &e,
                    IntentPhase::Acked,
                    9,
                    DATE,
                    Some("GRW5".to_owned()),
                    None,
                )
                .unwrap();
        }
        let ledger = IntentLedger::open(&dir, DATE).unwrap();
        let open = ledger.classify_open_intents();
        assert_eq!(open.len(), 4);
        let classes: Vec<BootIntentClass> = open.iter().map(|(_, c)| *c).collect();
        assert_eq!(
            classes
                .iter()
                .filter(|c| **c == BootIntentClass::NeverSent)
                .count(),
            1
        );
        assert_eq!(
            classes
                .iter()
                .filter(|c| **c == BootIntentClass::NeedsResolution)
                .count(),
            2
        );
        assert_eq!(
            classes
                .iter()
                .filter(|c| **c == BootIntentClass::NeedsOperator)
                .count(),
            1
        );
        assert_eq!(ledger.max_sequence(), Some(5));
        cleanup(&dir);
    }

    #[test]
    fn test_classify_replayed_phase_total() {
        use BootIntentClass as C;
        use IntentPhase as P;
        assert_eq!(classify_replayed_phase(P::Recorded), C::NeverSent);
        assert_eq!(classify_replayed_phase(P::Sent), C::NeedsResolution);
        assert_eq!(classify_replayed_phase(P::Ambiguous), C::NeedsResolution);
        assert_eq!(classify_replayed_phase(P::Replayed), C::NeedsResolution);
        assert_eq!(classify_replayed_phase(P::Unresolved), C::NeedsOperator);
        assert_eq!(classify_replayed_phase(P::Acked), C::Settled);
        assert_eq!(classify_replayed_phase(P::Rejected), C::Settled);
        assert_eq!(classify_replayed_phase(P::ResolvedLanded), C::Settled);
        assert_eq!(classify_replayed_phase(P::ResolvedNotLanded), C::Settled);
    }

    #[test]
    fn test_phase_terminality_pins() {
        use IntentPhase as P;
        for p in [
            P::Acked,
            P::Rejected,
            P::ResolvedLanded,
            P::ResolvedNotLanded,
        ] {
            assert!(p.is_terminal(), "{p:?}");
        }
        for p in [
            P::Recorded,
            P::Sent,
            P::Ambiguous,
            P::Replayed,
            P::Unresolved,
        ] {
            assert!(!p.is_terminal(), "{p:?} must stay blocking");
        }
    }

    #[test]
    fn test_append_phase_unknown_intent_refused() {
        let dir = temp_ledger_dir("unknown-intent");
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        assert!(matches!(
            ledger.append_phase("TVNOPE0000000001", IntentPhase::Sent, 1, DATE, None, None),
            Err(GrowwOmsError::InvalidOrderField(_))
        ));
        cleanup(&dir);
    }

    #[test]
    fn test_empty_file_replay_is_clean() {
        let replay = replay_bytes(b"").unwrap();
        assert!(!replay.torn_tail);
        assert_eq!(replay.lines, 0);
        assert!(replay.intents.is_empty());
        assert_eq!(replay.max_sequence, None);
    }

    // --- MEDIUM-8: a settled intent can never regress to a non-terminal
    //     phase ---

    #[test]
    fn test_non_terminal_phase_after_terminal_refused() {
        let dir = temp_ledger_dir("phase-regress");
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        let intent = place_intent(1);
        let intent_id = intent.intent_id.clone();
        ledger.record_intent(intent, DATE).unwrap();
        ledger
            .append_phase(&intent_id, IntentPhase::Sent, 2, DATE, None, None)
            .unwrap();
        ledger
            .append_phase(
                &intent_id,
                IntentPhase::Acked,
                3,
                DATE,
                Some("GRW1".to_owned()),
                None,
            )
            .unwrap();
        // Any NON-terminal phase after the terminal `acked` is refused,
        // typed and fail-loud.
        for regress in [
            IntentPhase::Recorded,
            IntentPhase::Sent,
            IntentPhase::Ambiguous,
            IntentPhase::Replayed,
            IntentPhase::Unresolved,
        ] {
            match ledger.append_phase(&intent_id, regress, 4, DATE, None, None) {
                Err(GrowwOmsError::IntentAlreadySettled {
                    intent_id: refused_id,
                    settled_phase,
                    attempted_phase,
                }) => {
                    assert_eq!(refused_id, intent_id);
                    assert_eq!(settled_phase, "acked");
                    assert_eq!(attempted_phase, regress.as_str());
                }
                other => panic!("expected IntentAlreadySettled for {regress:?}, got {other:?}"),
            }
        }
        // Terminal → terminal (ladder re-verdict) stays allowed.
        assert!(
            ledger
                .append_phase(&intent_id, IntentPhase::ResolvedLanded, 5, DATE, None, None)
                .is_ok()
        );
        cleanup(&dir);
    }

    // --- HIGH-4: day-rollover replay + mid-session file switch ---

    const DAY1: IstDate = IstDate {
        year: 2026,
        month: 7,
        day: 14,
    };

    #[test]
    fn test_day_rollover_replays_prior_files_and_blocks_mutations() {
        let dir = temp_ledger_dir("rollover");
        // Yesterday's session: an open place (sent) + an open modify with a
        // known order id.
        {
            let mut l1 = IntentLedger::open(&dir, DAY1).unwrap();
            let place_rid = reference_id::generate_reference_id(DAY1, 7, 1);
            let receipt = l1
                .record_intent(
                    NewIntent {
                        intent_id: place_rid.clone(),
                        reference_id: place_rid.clone(),
                        kind: IntentKind::Place,
                        groww_order_id: None,
                        mode: "paper".to_owned(),
                        ts_ms: 1,
                        linked_intent_id: None,
                    },
                    DAY1,
                )
                .unwrap();
            l1.append_phase_for_receipt(&receipt, IntentPhase::Sent, 2, DAY1, None, None)
                .unwrap();
            let modify_rid = reference_id::generate_reference_id(DAY1, 8, 1);
            let m = l1
                .record_intent(
                    NewIntent {
                        intent_id: modify_rid.clone(),
                        reference_id: modify_rid,
                        kind: IntentKind::Modify,
                        groww_order_id: Some("GRW1".to_owned()),
                        mode: "paper".to_owned(),
                        ts_ms: 3,
                        linked_intent_id: None,
                    },
                    DAY1,
                )
                .unwrap();
            l1.append_phase_for_receipt(&m, IntentPhase::Sent, 4, DAY1, None, None)
                .unwrap();
        }
        // Next-morning boot on DAY 2 (DATE): the scan replays BOTH days.
        let mut l2 = IntentLedger::open(&dir, DATE).unwrap();
        assert_eq!(l2.open_intent_count(), 2, "prior-day opens replayed");
        assert_eq!(l2.max_sequence(), Some(8), "max_sequence spans files");
        // Yesterday's open MODIFY still blocks a same-order mutation.
        let cancel_rid = reference_id::generate_reference_id(DATE, 9, 1);
        match l2.record_intent(
            NewIntent {
                intent_id: cancel_rid.clone(),
                reference_id: cancel_rid.clone(),
                kind: IntentKind::Cancel,
                groww_order_id: Some("GRW1".to_owned()),
                mode: "paper".to_owned(),
                ts_ms: 5,
                linked_intent_id: None,
            },
            DATE,
        ) {
            Err(GrowwOmsError::MutationInFlight { groww_order_id }) => {
                assert_eq!(groww_order_id, "GRW1");
            }
            other => panic!("expected MutationInFlight, got {other:?}"),
        }
        // Yesterday's open PLACE still blocks its reference id.
        let day1_place_rid = reference_id::generate_reference_id(DAY1, 7, 1);
        assert!(matches!(
            l2.record_intent(
                NewIntent {
                    intent_id: reference_id::generate_reference_id(DATE, 10, 1),
                    reference_id: day1_place_rid,
                    kind: IntentKind::Place,
                    groww_order_id: None,
                    mode: "paper".to_owned(),
                    ts_ms: 6,
                    linked_intent_id: None,
                },
                DATE,
            ),
            Err(GrowwOmsError::DuplicateIntent { .. })
        ));
        // A fresh place appends to TODAY's file; yesterday's file is
        // untouched.
        let day1_len = std::fs::metadata(dir.join(ledger_file_name(DAY1)))
            .unwrap()
            .len();
        l2.record_intent(place_intent(11), DATE).unwrap();
        assert_eq!(l2.path(), dir.join(ledger_file_name(DATE)).as_path());
        assert_eq!(
            std::fs::metadata(dir.join(ledger_file_name(DAY1)))
                .unwrap()
                .len(),
            day1_len,
            "appends never land in a prior day's file"
        );
        cleanup(&dir);
    }

    #[test]
    fn test_mid_session_rollover_appends_to_new_days_file() {
        let dir = temp_ledger_dir("midsession-roll");
        let day2 = IstDate {
            year: 2026,
            month: 7,
            day: 16,
        };
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        ledger.record_intent(place_intent(1), DATE).unwrap();
        let day1_path = dir.join(ledger_file_name(DATE));
        let day1_len = std::fs::metadata(&day1_path).unwrap().len();
        // The IST date rolls mid-session: the NEXT append switches files.
        let rid2 = reference_id::generate_reference_id(day2, 2, 2);
        let receipt = ledger
            .record_intent(
                NewIntent {
                    intent_id: rid2.clone(),
                    reference_id: rid2.clone(),
                    kind: IntentKind::Place,
                    groww_order_id: None,
                    mode: "paper".to_owned(),
                    ts_ms: 10,
                    linked_intent_id: None,
                },
                day2,
            )
            .unwrap();
        let day2_path = dir.join(ledger_file_name(day2));
        assert_eq!(ledger.path(), day2_path.as_path());
        assert!(day2_path.exists(), "the new day's file was created");
        assert_eq!(
            std::fs::metadata(&day1_path).unwrap().len(),
            day1_len,
            "day-1 file untouched after the rollover"
        );
        // Sequence continues monotonically across the switch.
        assert_eq!(ledger.max_sequence(), Some(2));
        // Phase appends follow the new date too.
        let day2_len = std::fs::metadata(&day2_path).unwrap().len();
        ledger
            .append_phase_for_receipt(&receipt, IntentPhase::Sent, 11, day2, None, None)
            .unwrap();
        assert!(std::fs::metadata(&day2_path).unwrap().len() > day2_len);
        assert_eq!(std::fs::metadata(&day1_path).unwrap().len(), day1_len);
        // A reopen on day2 replays BOTH files: 2 open intents.
        drop(ledger);
        let reopened = IntentLedger::open(&dir, day2).unwrap();
        assert_eq!(reopened.open_intent_count(), 2);
        assert_eq!(reopened.max_sequence(), Some(2));
        cleanup(&dir);
    }

    // --- R2-MEDIUM-3a: backward-dated appends are refused fail-loud ---

    #[test]
    fn test_backward_date_append_refused_typed() {
        let dir = temp_ledger_dir("backward-date");
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        // A record_intent targeting YESTERDAY (clock skew / caller bug)
        // must be refused with a typed error, before any file switch.
        let intent = place_intent(1);
        let intent_id = intent.intent_id.clone();
        match ledger.record_intent(intent.clone(), DAY1) {
            Err(GrowwOmsError::LedgerUnavailable(msg)) => {
                assert!(msg.contains("older than"), "typed refusal, got: {msg}");
            }
            other => panic!("expected LedgerUnavailable(backward date), got {other:?}"),
        }
        // Nothing was journaled or indexed — the same intent id is still
        // usable with the CORRECT date.
        assert!(ledger.intent(&intent_id).is_none());
        let receipt = ledger.record_intent(intent, DATE).unwrap();
        // Phase appends refuse a backward date too.
        match ledger.append_phase_for_receipt(&receipt, IntentPhase::Sent, 2, DAY1, None, None) {
            Err(GrowwOmsError::LedgerUnavailable(msg)) => {
                assert!(msg.contains("older than"));
            }
            other => panic!("expected LedgerUnavailable(backward date), got {other:?}"),
        }
        // Yesterday's file was never created by the refused appends.
        assert!(!dir.join(ledger_file_name(DAY1)).exists());
        // The correct date still appends normally.
        assert!(
            ledger
                .append_phase_for_receipt(&receipt, IntentPhase::Sent, 3, DATE, None, None)
                .is_ok()
        );
        // The pure LedgerError variant is typed + carries both dates.
        let err = LedgerError::BackwardDate {
            attempted_yymmdd: 260_714,
            active_yymmdd: 260_715,
        };
        assert!(err.to_string().contains("260714"));
        assert!(err.to_string().contains("260715"));
        cleanup(&dir);
    }

    // --- R2-MEDIUM-3b: a stale file can never resurrect a settled intent ---

    #[test]
    fn test_merge_never_regresses_terminal_phase_across_files() {
        let dir = temp_ledger_dir("merge-regress");
        let intent_id;
        let reference;
        // Day-1 file: an intent settled at ACKED (terminal) with an order id.
        {
            let mut l1 = IntentLedger::open(&dir, DAY1).unwrap();
            let rid = reference_id::generate_reference_id(DAY1, 1, 1);
            intent_id = rid.clone();
            reference = rid.clone();
            let receipt = l1
                .record_intent(
                    NewIntent {
                        intent_id: rid.clone(),
                        reference_id: rid,
                        kind: IntentKind::Place,
                        groww_order_id: None,
                        mode: "paper".to_owned(),
                        ts_ms: 1,
                        linked_intent_id: None,
                    },
                    DAY1,
                )
                .unwrap();
            l1.append_phase_for_receipt(&receipt, IntentPhase::Sent, 2, DAY1, None, None)
                .unwrap();
            l1.append_phase_for_receipt(
                &receipt,
                IntentPhase::Acked,
                3,
                DAY1,
                Some("GRW1".to_owned()),
                None,
            )
            .unwrap();
        }
        // Adversarial DAY-2 file: a stale NON-terminal line for the SAME
        // intent (append_phase would refuse writing this — MEDIUM-8 — so
        // it can only exist adversarially/corrupt). Oldest-first replay
        // feeds it AFTER the settled day-1 phases.
        {
            let stale = IntentRecord {
                intent_id: intent_id.clone(),
                reference_id: reference.clone(),
                kind: IntentKind::Place,
                phase: IntentPhase::Sent,
                groww_order_id: None,
                mode: String::new(),
                ts_ms: 99,
                detail: None,
                linked_intent_id: None,
            };
            let mut line = serde_json::to_vec(&stale).unwrap();
            line.push(b'\n');
            std::fs::write(dir.join(ledger_file_name(DATE)), line).unwrap();
        }
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        // The terminal phase MUST survive the stale later file.
        let summary = ledger.intent(&intent_id).unwrap();
        assert_eq!(
            summary.last_phase,
            IntentPhase::Acked,
            "a stale file must never resurrect a settled intent"
        );
        assert_eq!(summary.last_ts_ms, 3, "the settled ts is kept too");
        assert_eq!(summary.groww_order_id.as_deref(), Some("GRW1"));
        assert_eq!(ledger.open_intent_count(), 0);
        assert!(ledger.classify_open_intents().is_empty());
        // Neither refusal index was re-armed: the reference id is reusable
        // (a replay of the settled logical order)…
        let replay_id = reference_id::generate_reference_id(DATE, 2, 2);
        assert!(
            ledger
                .record_intent(
                    NewIntent {
                        intent_id: replay_id,
                        reference_id: reference,
                        kind: IntentKind::Place,
                        groww_order_id: None,
                        mode: "paper".to_owned(),
                        ts_ms: 100,
                        linked_intent_id: None,
                    },
                    DATE,
                )
                .is_ok(),
            "DuplicateIntent must not be re-armed by the stale line"
        );
        // …and a mutation on the settled order is not blocked either.
        let cancel_id = reference_id::generate_reference_id(DATE, 3, 3);
        assert!(
            ledger
                .record_intent(
                    NewIntent {
                        intent_id: cancel_id.clone(),
                        reference_id: cancel_id,
                        kind: IntentKind::Cancel,
                        groww_order_id: Some("GRW1".to_owned()),
                        mode: "paper".to_owned(),
                        ts_ms: 101,
                        linked_intent_id: None,
                    },
                    DATE,
                )
                .is_ok(),
            "MutationInFlight must not be re-armed by the stale line"
        );
        cleanup(&dir);
    }

    #[test]
    fn test_directory_scan_ignores_foreign_and_gtt_files() {
        let dir = temp_ledger_dir("scan-filter");
        // Session-2 reserved prefix + unrelated files must NOT be replayed.
        std::fs::write(dir.join("groww-gtt-intents-20260714.ndjson"), b"not json\n").unwrap();
        std::fs::write(dir.join("notes.txt"), b"hello\n").unwrap();
        std::fs::write(dir.join("groww-intents-2026071.ndjson"), b"junk\n").unwrap(); // 7 digits
        let ledger = IntentLedger::open(&dir, DATE).unwrap();
        assert_eq!(ledger.open_intent_count(), 0);
        assert!(is_ledger_file_name("groww-intents-20260715.ndjson"));
        assert!(!is_ledger_file_name("groww-gtt-intents-20260715.ndjson"));
        assert!(!is_ledger_file_name("groww-intents-20260715.ndjson.bak"));
        assert!(!is_ledger_file_name("groww-intents-2026X715.ndjson"));
        cleanup(&dir);
    }

    proptest! {
        /// Record serialization roundtrips through the NDJSON line format.
        #[test]
        fn prop_ledger_record_roundtrip(
            seq in 0_u32..1_679_616,
            phase_idx in 0_usize..9,
            kind_idx in 0_usize..3,
            ts in 0_i64..2_000_000_000_000,
            order in proptest::option::of("[A-Z0-9]{1,12}"),
            detail in proptest::option::of("\\PC{0,40}"),
        ) {
            let phases = [
                IntentPhase::Recorded, IntentPhase::Sent, IntentPhase::Acked,
                IntentPhase::Rejected, IntentPhase::Ambiguous,
                IntentPhase::ResolvedLanded, IntentPhase::ResolvedNotLanded,
                IntentPhase::Replayed, IntentPhase::Unresolved,
            ];
            let kinds = [IntentKind::Place, IntentKind::Modify, IntentKind::Cancel];
            let rid = reference_id::generate_reference_id(DATE, seq, 5);
            let rec = IntentRecord {
                intent_id: rid.clone(),
                reference_id: rid,
                kind: kinds[kind_idx],
                phase: phases[phase_idx],
                groww_order_id: order,
                mode: "paper".to_owned(),
                ts_ms: ts,
                detail,
                linked_intent_id: None,
            };
            let line = serde_json::to_vec(&rec).unwrap();
            let back: IntentRecord = serde_json::from_slice(&line).unwrap();
            prop_assert_eq!(back, rec.clone());
            // And a one-line buffer replays to exactly this intent.
            let mut buf = line;
            buf.push(b'\n');
            let replay = replay_bytes(&buf).unwrap();
            prop_assert_eq!(replay.lines, 1);
            prop_assert!(!replay.torn_tail);
            prop_assert_eq!(
                replay.intents.get(&rec.intent_id).map(|s| s.last_phase),
                Some(rec.phase)
            );
            prop_assert_eq!(replay.max_sequence, Some(seq));
        }

        /// A valid prefix + ANY torn (non-newline-terminated) garbage tail
        /// replays clean: torn tolerated, prefix intact, never a panic.
        #[test]
        fn prop_torn_tail_never_breaks_replay(tail in proptest::collection::vec(any::<u8>(), 1..64)) {
            // Keep the tail newline-free so it is a single TORN final line.
            let tail: Vec<u8> = tail.into_iter().filter(|b| *b != b'\n').collect();
            prop_assume!(!tail.is_empty());
            let rid = reference_id::generate_reference_id(DATE, 1, 1);
            let rec = IntentRecord {
                intent_id: rid.clone(),
                reference_id: rid,
                kind: IntentKind::Place,
                phase: IntentPhase::Recorded,
                groww_order_id: None,
                mode: "paper".to_owned(),
                ts_ms: 1,
                detail: None,
                linked_intent_id: None,
            };
            let mut buf = serde_json::to_vec(&rec).unwrap();
            buf.push(b'\n');
            let clean = buf.len() as u64;
            buf.extend_from_slice(&tail);
            match replay_bytes(&buf) {
                Ok(replay) => {
                    prop_assert_eq!(replay.lines, 1);
                    prop_assert_eq!(replay.clean_len, clean);
                    // HIGH-3: ANY unterminated final segment — even one that
                    // happens to parse — is a torn tail; the prefix always
                    // survives.
                    prop_assert!(replay.torn_tail);
                    prop_assert!(replay.intents.contains_key(&rec.intent_id));
                }
                Err(e) => return Err(TestCaseError::fail(format!("replay must not fail: {e}"))),
            }
        }
    }
}
