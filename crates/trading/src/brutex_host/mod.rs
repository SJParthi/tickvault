//! brutex strategy host: the consumer side of the owner's 29 Oct 2026 decisions
//! (`.claude/rules/project/brutex-strategy-host-2026-10-29.md`, contract in
//! `docs/architecture/brutex-strategy-contract.md`).
//!
//! tickvault defines no strategy. It loads a version (and, for a dynamic
//! strategy, a parameter and a universe snapshot) that brutex published, checks
//! the manifest hashes, the gates and the bit-for-bit replay of the check files
//! through the strategy crate, and decides whether it may go live. Every load,
//! refusal and swap is an audit event.
//!
//! Everything here is pure (no network, no clock): the folder reader takes a
//! local directory, the clock comes in as arguments, and the strategy crate sits
//! behind [`engine::StrategyEngine`]. The S3 download lives in the storage crate
//! (`strategy_recipe_fetch`, List + Get only). Nothing in this module is wired
//! into the live trading path yet: no strategy is approved and brutex's crate
//! does not exist.
//!
//! # What is cold and what is per call
//!
//! * Cold (once before the market opens, or in the dry-run CLI): `loader`,
//!   `manifest`, `recipe`, `check_files`, `gates`, `replay`, `select`, `swap`,
//!   `audit`, `fixture`, and `live_candles::LiveCandleBook::configure` (the
//!   only allocation the candle book makes).
//! * Per tick: `live_candles::LiveCandleBook::on_tick`, one composite-key hash
//!   probe plus O(frames), no allocation. It does not drop a duplicate
//!   packet: a repeat adds to the bar's tick count and no volume. Per clock
//!   reading: `LiveCandleBook::on_clock`, O(frames), plus one O(open bars of
//!   a frame) scan when that frame's due bound passes (see `live_candles`).
//! * Per order (a hot call, not per tick): `worst_fill::worst_fill`,
//!   O(fill seconds used + book levels); `order_price::marketable_limit`, O(1);
//!   `send_queue::SendQueue` push and pop, O(log N) heap sifts with the
//!   capacity N at most 64 (6 levels) on a fixed array. All of them allocate
//!   nothing per call.
//! * The strategy crate behind `engine::StrategyEngine` carries its own
//!   per-tick contract: `on_tick`/`on_bar` O(1) and allocation-free (contract
//!   R4–R6).

// O(1) EXEMPT: begin — module declarations, the key type and the cold `hex_lower` helper only (no per-tick code in this file). The per-tick and per-order functions live in `live_candles`, `worst_fill`, `order_price` and `send_queue`; their complexity is stated in the module docs above and in each module.
pub mod audit;
pub mod check_files;
pub mod engine;
pub mod fixture;
pub mod gates;
pub mod live_candles;
pub mod loader;
pub mod manifest;
pub mod order_price;
pub mod recipe;
pub mod replay;
pub mod select;
pub mod send_queue;
pub mod swap;
pub mod worst_fill;

/// The only unique instrument key (I-P1-11): id and segment together.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
pub struct InstrumentKey {
    pub security_id: u64,
    pub exchange_segment: u8,
}

/// Why a version, snapshot or swap was refused. Every variant names enough to
/// act on without opening a log.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Refusal {
    #[error("file {name} could not be read: {detail}")]
    Unreadable { name: String, detail: String },
    #[error("manifest is malformed: {0}")]
    BadManifest(String),
    #[error("file {name} is listed in the manifest but missing")]
    MissingFile { name: String },
    #[error("file {name} is not listed in the manifest")]
    UnlistedFile { name: String },
    #[error("file {name} has {got} bytes, manifest says {expected}")]
    SizeMismatch {
        name: String,
        expected: u64,
        got: u64,
    },
    #[error("file {name} sha256 is {got}, manifest says {expected}")]
    HashMismatch {
        name: String,
        expected: String,
        got: String,
    },
    #[error("recipe is malformed: {0}")]
    BadRecipe(String),
    #[error("recipe status is {0:?}, not \"approved\"")]
    NotApproved(String),
    #[error("recipe says version {in_file}, its folder is v{folder}")]
    VersionMismatch { folder: u32, in_file: u32 },
    #[error("recipe was made by brutex commit {recipe}, tickvault is built against {built}")]
    CommitMismatch { recipe: String, built: String },
    #[error("check file {name} is malformed at row {row}: {detail}")]
    BadCheckFile {
        name: String,
        row: usize,
        detail: String,
    },
    #[error("check candle file is empty")]
    EmptyCandles,
    #[error("gate refused: {0}")]
    Gate(String),
    #[error("the strategy engine refused its configuration: {0}")]
    Engine(String),
    #[error("replay differs: {0}")]
    Replay(Box<replay::Mismatch>),
    #[error("no approved version found")]
    NoVersion,
    #[error("swap refused: {0}")]
    Swap(String),
}

/// Lower-case hex of a byte slice (sha256 digests).
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    s
}

// O(1) EXEMPT: end
#[cfg(test)]
mod tests;
