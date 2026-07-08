//! Deterministic RANGE-BASED shard cutter for the Groww multi-connection
//! auto-scale ladder (§34, operator authorization 2026-07-03 — see
//! `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §34 and
//! `.claude/rules/project/groww-scale-error-codes.md`).
//!
//! The cutter turns the daily Groww watch-set into N DISJOINT, COVERING
//! shards of at most `instruments_per_conn` instruments each — one shard per
//! sidecar connection. RANGE-BASED (not hash): entries are sorted by
//! `(exchange, segment, security_id)` and cut into contiguous ranges, so the
//! assignment is deterministic AND human-inspectable ("conn 3 owns SIDs
//! X..Y"), and appending new instruments that sort last churns only the tail
//! shard instead of reshuffling everything (which `hash(token) % N` would).
//!
//! COLD PATH ONLY: the shard decision runs at boot / ladder-step time
//! (O(N log N) sort — flagged honestly, never per-tick); the per-tick path
//! never consults the cutter.
//!
//! Fail-closed invariants (GROWW-SCALE-03 class — a violation is a cutter
//! bug, never silently subscribed twice):
//! - shards are DISJOINT (no `(exchange, segment, security_id)` in two shards)
//! - the shard union EXACTLY covers the input watch-set
//! - duplicate identities in the input are rejected
//! - `instruments_per_conn` outside `[1, GROWW_MAX_SUBSCRIPTIONS]` is rejected

use super::instruments::{GROWW_MAX_SUBSCRIPTIONS, WatchEntry};

/// One connection's shard: the contiguous slice of the sorted watch-set that
/// sidecar `conn_id` subscribes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrowwShard {
    /// Zero-based connection id (`GROWW_CONN_ID` env of the child process).
    pub conn_id: usize,
    /// The instruments this connection owns (≤ `instruments_per_conn`,
    /// sorted by `(exchange, segment, security_id)`).
    pub entries: Vec<WatchEntry>,
}

/// Why a shard cut is refused (fail-closed — the ladder step HALTs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShardCutError {
    /// `instruments_per_conn` outside `[1, GROWW_MAX_SUBSCRIPTIONS]`.
    InvalidPerConnCap {
        /// The rejected cap value.
        got: usize,
        /// The Groww per-session hard cap.
        max: usize,
    },
    /// The input watch-set contains the same `(exchange, segment,
    /// security_id)` twice — sharding it would either double-subscribe or
    /// silently drop one copy. The upstream watch-set builder must dedupe.
    DuplicateIdentity {
        /// Exchange of the duplicated identity.
        exchange: String,
        /// Segment of the duplicated identity.
        segment: String,
        /// The duplicated security id.
        security_id: i64,
    },
}

impl core::fmt::Display for ShardCutError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidPerConnCap { got, max } => {
                write!(f, "instruments_per_conn must be in [1, {max}], got {got}")
            }
            Self::DuplicateIdentity {
                exchange,
                segment,
                security_id,
            } => write!(
                f,
                "duplicate watch identity ({exchange}, {segment}, {security_id}) — upstream watch-set must be deduped"
            ),
        }
    }
}

impl core::error::Error for ShardCutError {}

/// Ceil-division connection count for a universe: how many connections the
/// ladder ultimately needs to cover `universe_size` instruments at
/// `instruments_per_conn` per connection. `0` for an empty universe.
#[must_use]
pub const fn required_connections(universe_size: usize, instruments_per_conn: usize) -> usize {
    if instruments_per_conn == 0 {
        // Defensive: the caller validates the cap first (`cut_shards` and
        // `GrowwScaleConfig::validate` both reject 0); 0 here means "no
        // valid shard layout exists", never a divide-by-zero panic.
        return 0;
    }
    universe_size.div_ceil(instruments_per_conn)
}

/// Cuts the watch-set into deterministic, disjoint, covering range shards.
///
/// Sorts a copy of `entries` by `(exchange, segment, security_id)` and cuts
/// contiguous chunks of at most `instruments_per_conn`. The same input
/// always produces the same shards (determinism ratchet-tested); an empty
/// input produces zero shards.
///
/// # Errors
/// - [`ShardCutError::InvalidPerConnCap`] when `instruments_per_conn` is 0
///   or exceeds [`GROWW_MAX_SUBSCRIPTIONS`].
/// - [`ShardCutError::DuplicateIdentity`] when two entries share the same
///   `(exchange, segment, security_id)` — fail-closed, the upstream
///   watch-set builder must dedupe (GROWW-SCALE-03 class).
pub fn cut_shards(
    entries: &[WatchEntry],
    instruments_per_conn: usize,
) -> Result<Vec<GrowwShard>, ShardCutError> {
    if instruments_per_conn == 0 || instruments_per_conn > GROWW_MAX_SUBSCRIPTIONS {
        return Err(ShardCutError::InvalidPerConnCap {
            got: instruments_per_conn,
            max: GROWW_MAX_SUBSCRIPTIONS,
        });
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    // Deterministic order: (exchange, segment, security_id). Cold path — the
    // owned sort copy costs nothing on the per-tick path.
    let mut sorted: Vec<WatchEntry> = entries.to_vec();
    sorted.sort_by(|a, b| {
        (a.exchange.as_str(), a.segment.as_str(), a.security_id).cmp(&(
            b.exchange.as_str(),
            b.segment.as_str(),
            b.security_id,
        ))
    });

    // Disjointness precondition: adjacent duplicates after the sort mean the
    // same identity appears twice → fail closed.
    for pair in sorted.windows(2) {
        if pair[0].exchange == pair[1].exchange
            && pair[0].segment == pair[1].segment
            && pair[0].security_id == pair[1].security_id
        {
            return Err(ShardCutError::DuplicateIdentity {
                exchange: pair[1].exchange.clone(),
                segment: pair[1].segment.clone(),
                security_id: pair[1].security_id,
            });
        }
    }

    // Contiguous range cut. By construction the chunks are disjoint and
    // their union is exactly `sorted` (== the input set) — the invariant
    // the GROWW-SCALE-03 runtime detector (PR-2) re-checks defensively.
    let shards: Vec<GrowwShard> = sorted
        .chunks(instruments_per_conn)
        .enumerate()
        .map(|(conn_id, chunk)| GrowwShard {
            conn_id,
            entries: chunk.to_vec(),
        })
        .collect();

    debug_assert_eq!(
        shards.iter().map(|s| s.entries.len()).sum::<usize>(),
        entries.len(),
        "shard union must cover the input watch-set exactly"
    );

    Ok(shards)
}

#[cfg(test)]
mod tests {
    use super::super::instruments::WatchKind;
    use super::*;

    fn entry(exchange: &str, segment: &str, security_id: i64) -> WatchEntry {
        WatchEntry {
            exchange: exchange.to_string(),
            segment: segment.to_string(),
            exchange_token: security_id.to_string(),
            kind: WatchKind::Ltp,
            security_id,
            isin: None,
            symbol_name: None,
            index_name: None,
            expiry_date: None,
        }
    }

    /// Build a universe of `n` NSE CASH entries with distinct ids, shuffled
    /// deterministically (reverse order) so tests exercise the sort.
    fn universe(n: i64) -> Vec<WatchEntry> {
        (0..n).rev().map(|i| entry("NSE", "CASH", i)).collect()
    }

    /// RATCHET (design §1 invariant): shards are DISJOINT and their union
    /// == the input watch-set — the fail-closed contract GROWW-SCALE-03
    /// pages on if it ever breaks at runtime.
    #[test]
    fn test_cut_shards_disjoint_and_covering() {
        let input = universe(25);
        let shards = cut_shards(&input, 10).expect("valid cut");
        assert_eq!(shards.len(), 3);

        // Union size == input size (coverage, no drops, no duplication).
        let total: usize = shards.iter().map(|s| s.entries.len()).sum();
        assert_eq!(total, input.len());

        // Disjoint: every identity appears exactly once across all shards.
        let mut seen = std::collections::HashSet::new();
        for shard in &shards {
            for e in &shard.entries {
                assert!(
                    seen.insert((e.exchange.clone(), e.segment.clone(), e.security_id)),
                    "identity {:?} appeared in two shards",
                    (e.security_id)
                );
            }
        }
        // Coverage: every input identity is present.
        for e in &input {
            assert!(seen.contains(&(e.exchange.clone(), e.segment.clone(), e.security_id)));
        }
    }

    /// RATCHET: same input (any order) → same shards. Determinism is what
    /// makes "conn 3 owns SIDs X..Y" human-inspectable and restart-stable.
    #[test]
    fn test_cut_shards_deterministic_order() {
        let mut a = universe(23);
        let b: Vec<WatchEntry> = a.iter().rev().cloned().collect();
        let cut_a = cut_shards(&a, 7).expect("valid");
        let cut_b = cut_shards(&b, 7).expect("valid");
        assert_eq!(cut_a, cut_b, "input order must not change the cut");
        // And a repeated cut of the identical input is identical.
        a.reverse();
        assert_eq!(cut_shards(&a, 7).expect("valid"), cut_a);
        // Entries inside each shard are sorted by (exchange, segment, id).
        for shard in &cut_a {
            for pair in shard.entries.windows(2) {
                assert!(pair[0].security_id < pair[1].security_id);
            }
        }
    }

    /// Every shard holds at most `instruments_per_conn` entries; all but the
    /// tail shard are exactly full; conn_ids are 0..N-1 in order.
    #[test]
    fn test_cut_shards_respects_per_conn_cap() {
        let input = universe(25);
        let shards = cut_shards(&input, 10).expect("valid");
        assert_eq!(shards[0].entries.len(), 10);
        assert_eq!(shards[1].entries.len(), 10);
        assert_eq!(shards[2].entries.len(), 5, "tail shard holds remainder");
        for (i, shard) in shards.iter().enumerate() {
            assert_eq!(shard.conn_id, i);
            assert!(shard.entries.len() <= 10);
        }
    }

    /// Empty watch-set → zero shards (no connections spawned), never an error.
    #[test]
    fn test_cut_shards_empty_input() {
        assert_eq!(cut_shards(&[], 1000).expect("valid"), Vec::new());
    }

    /// FINANCIAL/ENVELOPE BOUNDARY: cap 0 is rejected fail-closed.
    #[test]
    fn test_cut_shards_rejects_zero_cap() {
        assert_eq!(
            cut_shards(&universe(3), 0),
            Err(ShardCutError::InvalidPerConnCap {
                got: 0,
                max: GROWW_MAX_SUBSCRIPTIONS
            })
        );
    }

    /// FINANCIAL/ENVELOPE BOUNDARY: cap above the documented Groww
    /// per-session limit (1000) is rejected; exactly 1000 is accepted.
    #[test]
    fn test_cut_shards_rejects_over_groww_cap() {
        assert_eq!(
            cut_shards(&universe(3), GROWW_MAX_SUBSCRIPTIONS + 1),
            Err(ShardCutError::InvalidPerConnCap {
                got: GROWW_MAX_SUBSCRIPTIONS + 1,
                max: GROWW_MAX_SUBSCRIPTIONS
            })
        );
        assert!(cut_shards(&universe(3), GROWW_MAX_SUBSCRIPTIONS).is_ok());
    }

    /// A duplicated `(exchange, segment, security_id)` identity is rejected
    /// fail-closed (GROWW-SCALE-03 class) — never double-subscribed, never
    /// silently dropped. Same id on a DIFFERENT segment is two distinct
    /// instruments (I-P1-11 composite identity) and is allowed.
    #[test]
    fn test_cut_shards_rejects_duplicate_identity() {
        let mut input = universe(5);
        input.push(entry("NSE", "CASH", 3)); // duplicate identity
        assert_eq!(
            cut_shards(&input, 10),
            Err(ShardCutError::DuplicateIdentity {
                exchange: "NSE".to_string(),
                segment: "CASH".to_string(),
                security_id: 3,
            })
        );
        // I-P1-11: same id, different segment = distinct instrument → OK.
        let mut cross_segment = universe(5);
        cross_segment.push(entry("NSE", "FNO", 3));
        assert!(cut_shards(&cross_segment, 10).is_ok());
    }

    /// Ceil math for the ladder ceiling: 768 SIDs at 1000/conn = 1 conn;
    /// 10_000 at 1000 = 10; 10_001 at 1000 = 11; 0 SIDs = 0 conns; cap 0
    /// degrades to 0 (no valid layout), never a panic.
    #[test]
    fn test_required_connections_ceil_math() {
        assert_eq!(required_connections(768, 1000), 1);
        assert_eq!(required_connections(1000, 1000), 1);
        assert_eq!(required_connections(1001, 1000), 2);
        assert_eq!(required_connections(10_000, 1000), 10);
        assert_eq!(required_connections(10_001, 1000), 11);
        assert_eq!(required_connections(100_000, 1000), 100);
        assert_eq!(required_connections(0, 1000), 0);
        assert_eq!(required_connections(5, 0), 0);
    }

    /// A universe smaller than the per-conn cap yields exactly one shard —
    /// the ladder ceiling for today's ~768-SID universe is 1 connection.
    #[test]
    fn test_cut_shards_single_shard_small_universe() {
        let input = universe(768);
        let shards = cut_shards(&input, 1000).expect("valid");
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].conn_id, 0);
        assert_eq!(shards[0].entries.len(), 768);
    }

    /// RANGE-cut tail-churn property (why hash-mod-N was rejected):
    /// appending instruments that sort AFTER the existing set leaves every
    /// existing full shard byte-identical — only the tail changes.
    #[test]
    fn test_cut_shards_tail_churn_only_on_recut() {
        let input = universe(20);
        let before = cut_shards(&input, 10).expect("valid");
        assert_eq!(before.len(), 2);

        let mut grown = input.clone();
        grown.push(entry("NSE", "CASH", 20)); // sorts last
        grown.push(entry("NSE", "CASH", 21));
        let after = cut_shards(&grown, 10).expect("valid");
        assert_eq!(after.len(), 3);
        // Existing full shards are untouched by the growth.
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1], before[1]);
        // Only the new tail shard carries the new instruments.
        assert_eq!(after[2].entries.len(), 2);
    }
}
