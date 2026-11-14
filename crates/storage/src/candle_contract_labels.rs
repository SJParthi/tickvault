//! The contract-name lookup the candle writer uses to fill `candles_<tf>.contract`.
//!
//! The 2026-09-19 candle DDL declares `contract SYMBOL` so a candle row reads
//! and joins the same way a `top_volume` row does. Until 2026-09-22 nothing
//! ever wrote it: the column existed on every candle table and was NULL on
//! every row. This module is the missing half.
//!
//! ## Why a registry here rather than a field on the seal
//!
//! The name is not a property of a tick or of a fold — it is a fact about the
//! day's contract list, which the app builds once from the master artifact.
//! Carrying it through the seal would widen `BufferedSeal` (and the 128-byte
//! spill record, which is byte-full) for a value the writer can resolve on its
//! own. So the app publishes the day's table ONCE, and the writer reads it
//! ONCE PER SEALED BAR — never per tick.
//!
//! ⚠ CORRECTED 2026-10-26 (plan item J1): "never per tick" is false. The
//! tick writer (since 2026-09-22) and the depth writer probe this table for
//! EVERY row they write: one `ArcSwap` load and one hash probe per tick or
//! depth row, O(1), no allocation. Names are made ILP-safe once, at publish
//! (`sanitize_labels`), so those per-row probes write the stored name as it
//! is and never re-scan it.
//!
//! `storage` cannot depend on `app`, which owns `ContractUnderlyingMap`, so the
//! table is re-published here in storage's own key shape at the same moment the
//! app publishes its own.
//!
//! ## Cost
//!
//! One `ArcSwap` load plus one hash probe per sealed bar: O(1), and ZERO
//! allocation — the label is handed to the ILP buffer as a borrowed `&str`
//! out of the published snapshot.
//!
//! ## Honest limits
//!
//! * Options carry their contract name; spots and indices carry the symbol the
//!   day's mapping artifact gives them (added 2026-09-22 — before that they
//!   were NULL). A future writes NO `contract` (futures are no longer
//!   subscribed). A name is never fabricated: an id absent from both tables
//!   stays NULL.
//! * A name longer than [`CONTRACT_LABEL_MAX_CHARS`] (64) characters is cut
//!   to 64 at publish and counted on `tv_candle_contract_labels_truncated_total`
//!   (owner decision 2026-11-06, plan item PF-4), so the longest row a writer
//!   can produce is a constant. Real names are about 30 characters.
//! * A seal re-ingested by the BOOT drain (spill/DLQ) can reach the writer
//!   before the day's table is published. That bar is written with a NULL
//!   `contract` — the same omission, never a guess.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use arc_swap::{ArcSwap, Guard};

/// `(security_id, segment)` — the candle row's own identity, in exactly the
/// types [`crate::shadow_seal_columns::ShadowSealRow`] carries, so the writer
/// probes with the row's fields and nothing is converted per bar. The segment
/// is the `&'static str` produced by
/// [`tickvault_common::segment::segment_code_to_str`], which is what keeps two
/// instruments sharing a numeric id on different segments apart (I-P1-11).
pub type CandleContractKey = (i64, &'static str);

/// The published name table.
/// `ahash` rather than the default SipHash (deep audit DA-12, HOT-10): the
/// key is an instrument id the vendor assigns, not attacker-chosen input, and
/// the table is probed once per written tick, depth and candle row.
pub type CandleContractLabels = HashMap<CandleContractKey, Arc<str>, ahash::RandomState>;

static LABELS: LazyLock<ArcSwap<CandleContractLabels>> =
    LazyLock::new(|| ArcSwap::from_pointee(CandleContractLabels::default()));

/// Publishes of the name table so far (both publish paths, only when they
/// stored a table). A reader that remembers rows judged against one table
/// compares this number to learn that a name may have changed since (operator
/// Quote 61: `feed_aux_repeat`). Only ever grows.
static LABELS_GENERATION: AtomicU64 = AtomicU64::new(0);

/// How many times the name table has been published in this process. Moves
/// on every [`publish_candle_contract_labels`] and on every
/// [`publish_candle_contract_labels_if_empty`] that stored its table. One
/// acquire load, O(1); read per wrong-day packet by the aux repeat filter.
#[must_use]
pub fn candle_contract_labels_generation() -> u64 {
    LABELS_GENERATION.load(Ordering::Acquire)
}

/// Replaces the whole table atomically and returns how many names it holds.
///
/// A replace, never a merge: yesterday's names for expired ids must not
/// survive into today, and a merge would keep them forever.
pub fn publish_candle_contract_labels(labels: CandleContractLabels) -> usize {
    let labels = sanitize_labels(labels);
    let n = labels.len();
    LABELS.store(Arc::new(labels));
    // After the store, Release: a reader that sees the new generation then
    // loads the new table, never the old one under the new number.
    LABELS_GENERATION.fetch_add(1, Ordering::Release);
    metrics::gauge!("tv_candle_contract_labels_published").set(n as f64);
    n
}

/// Publishes `labels` ONLY when the table is still empty, and returns whether
/// it did.
///
/// The boot path publishes spot and index names so 09:00 pre-open index ticks
/// carry a name before the contract attach runs. The attach later REPLACES the
/// whole table with spots + options. If the boot publish could also replace,
/// a slow boot racing a fast attach would wipe every option name for the rest
/// of the session, so the boot publish is a compare-and-swap against empty.
pub fn publish_candle_contract_labels_if_empty(labels: CandleContractLabels) -> bool {
    let incoming = Arc::new(sanitize_labels(labels));
    let mut stored = false;
    LABELS.rcu(|current| {
        if current.is_empty() {
            stored = true;
            Arc::clone(&incoming)
        } else {
            stored = false;
            Arc::clone(current)
        }
    });
    if stored {
        LABELS_GENERATION.fetch_add(1, Ordering::Release);
        metrics::gauge!("tv_candle_contract_labels_published").set(incoming.len() as f64);
    }
    stored
}

/// Makes every name ILP-safe ONCE, at publish (plan item J1, 2026-10-26).
///
/// The tick and depth writers probe this table for EVERY row they write, and
/// until this date each probe re-ran `sanitize_ilp_symbol` on the name: a
/// `chars()` scan per row, and an allocation for a name that needed
/// cleaning. Cleaning here moves that to the publish (a few times a day), so
/// a writer hands the stored `Arc<str>` to the ILP buffer as it is. A name
/// that is already clean keeps its allocation; only a dirty one is replaced.
/// O(names), cold.
fn sanitize_labels(mut labels: CandleContractLabels) -> CandleContractLabels {
    let mut truncated = 0_u64;
    // O(1) EXEMPT: begin — publish only (boot and contract attach), never per row
    for name in labels.values_mut() {
        if let std::borrow::Cow::Owned(clean) =
            tickvault_common::sanitize::sanitize_ilp_symbol(name)
        {
            *name = Arc::from(clean);
        }
        if let Some(cut) = truncate_label(name).map(Arc::<str>::from) {
            *name = cut;
            truncated += 1;
        }
    }
    // O(1) EXEMPT: end
    // Registered at 0 by every publish, so the series exists from boot.
    metrics::counter!(CONTRACT_LABELS_TRUNCATED_COUNTER).increment(truncated);
    labels
}

/// Characters a published contract name keeps (owner decision 2026-11-06,
/// plan item PF-4, "Cap at 64 characters"). Real names are about 30
/// characters (`BANKNIFTY-27Nov2026-51200-PE` is 28); the cap makes the
/// longest ILP line a tick row can write a constant
/// (`tick_stage::MAX_ILP_LINE_BYTES`), so the writer's row cut bounds its text.
pub const CONTRACT_LABEL_MAX_CHARS: usize = 64;

/// Names cut to [`CONTRACT_LABEL_MAX_CHARS`] at publish, over the process's
/// life. Expected to stay at zero.
pub const CONTRACT_LABELS_TRUNCATED_COUNTER: &str = "tv_candle_contract_labels_truncated_total";

/// `name` cut to its first [`CONTRACT_LABEL_MAX_CHARS`] characters, or `None`
/// when it is already that short. Cuts on a character boundary, never inside
/// one. O(len), publish only.
fn truncate_label(name: &str) -> Option<&str> {
    let (end, _) = name.char_indices().nth(CONTRACT_LABEL_MAX_CHARS)?;
    name.get(..end)
}

/// True when `name` is already what `sanitize_ilp_symbol` would write, i.e.
/// every published name. The writers `debug_assert!` it per row; release
/// builds skip it.
#[must_use]
pub fn label_is_ilp_clean(name: &str) -> bool {
    matches!(
        tickvault_common::sanitize::sanitize_ilp_symbol(name),
        std::borrow::Cow::Borrowed(_)
    )
}

/// A snapshot of the current table. Hold it for the length of one row write.
#[must_use]
pub fn candle_contract_labels() -> Guard<Arc<CandleContractLabels>> {
    LABELS.load()
}

/// Serialises every test that publishes: a publish REPLACES the global table,
/// so two tests publishing in parallel would erase each other's names.
#[cfg(test)]
pub(crate) static TEST_PUBLISH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Quote 61 review (2026-11-14): the generation moves on every publish
    /// that stores a table and not on a refused boot publish, so the aux
    /// repeat filter writes each key's next row after a name change.
    #[test]
    fn test_candle_contract_labels_generation_moves_on_every_stored_publish() {
        let _g = lock();
        publish_candle_contract_labels(CandleContractLabels::default());
        let empty = candle_contract_labels_generation();
        let mut boot = CandleContractLabels::default();
        boot.insert((13, "IDX_I"), Arc::<str>::from("NIFTY"));
        assert!(publish_candle_contract_labels_if_empty(boot.clone()));
        let after_boot = candle_contract_labels_generation();
        assert!(after_boot > empty, "a stored boot publish moves it");
        assert!(!publish_candle_contract_labels_if_empty(boot.clone()));
        assert_eq!(
            candle_contract_labels_generation(),
            after_boot,
            "a refused boot publish does not"
        );
        publish_candle_contract_labels(boot);
        assert!(
            candle_contract_labels_generation() > after_boot,
            "a replace moves it, even with the same names"
        );
    }

    #[test]
    fn test_publish_candle_contract_labels_finds_a_label_under_its_own_segment_only() {
        let _g = lock();
        let mut m = CandleContractLabels::default();
        m.insert(
            (9_100_001, "NSE_FNO"),
            Arc::<str>::from("NIFTY-25Sep2026-24500-CE"),
        );
        publish_candle_contract_labels(m);
        let t = candle_contract_labels();
        assert_eq!(
            t.get(&(9_100_001, "NSE_FNO")).map(|s| &**s),
            Some("NIFTY-25Sep2026-24500-CE")
        );
        // Same numeric id, different segment: a different instrument, no name.
        assert!(t.get(&(9_100_001, "NSE_EQ")).is_none());
    }

    #[test]
    fn test_publish_candle_contract_labels_if_empty_never_overwrites_a_published_table() {
        let _g = lock();
        publish_candle_contract_labels(CandleContractLabels::default());
        let mut boot = CandleContractLabels::default();
        boot.insert((13, "IDX_I"), Arc::<str>::from("NIFTY"));
        assert!(publish_candle_contract_labels_if_empty(boot));
        assert_eq!(
            candle_contract_labels().get(&(13, "IDX_I")).map(|s| &**s),
            Some("NIFTY")
        );

        // The attach replaces the whole table with spots + options.
        let mut attach = CandleContractLabels::default();
        attach.insert((13, "IDX_I"), Arc::<str>::from("NIFTY"));
        attach.insert((9_100_201, "NSE_FNO"), Arc::<str>::from("NIFTY-CE"));
        publish_candle_contract_labels(attach);

        // A late boot publish must NOT wipe the option name.
        let mut late = CandleContractLabels::default();
        late.insert((25, "IDX_I"), Arc::<str>::from("BANKNIFTY"));
        assert!(!publish_candle_contract_labels_if_empty(late));
        let t = candle_contract_labels();
        assert_eq!(
            t.get(&(9_100_201, "NSE_FNO")).map(|s| &**s),
            Some("NIFTY-CE")
        );
        assert!(t.get(&(25, "IDX_I")).is_none());
    }

    #[test]
    fn test_candle_contract_labels_republish_replaces_rather_than_merges() {
        let _g = lock();
        let mut first = CandleContractLabels::default();
        first.insert((9_100_101, "NSE_FNO"), Arc::<str>::from("OLD-EXPIRED-CE"));
        publish_candle_contract_labels(first);
        let mut second = CandleContractLabels::default();
        second.insert((9_100_102, "NSE_FNO"), Arc::<str>::from("NEW-CE"));
        assert_eq!(publish_candle_contract_labels(second), 1);
        let t = candle_contract_labels();
        assert!(
            t.get(&(9_100_101, "NSE_FNO")).is_none(),
            "an expired name must not survive a republish"
        );
    }

    /// J1 (2026-10-26): names are made ILP-safe at publish, by both publish
    /// paths, so the per-row writers can write them as they are.
    #[test]
    fn test_publish_sanitizes_every_name_once() {
        let _g = lock();
        let mut m = CandleContractLabels::default();
        m.insert((9_100_301, "NSE_FNO"), Arc::<str>::from("BAD,NAME=X\nCE"));
        m.insert((9_100_302, "NSE_FNO"), Arc::<str>::from("CLEAN-CE"));
        publish_candle_contract_labels(m);
        let t = candle_contract_labels();
        assert_eq!(
            t.get(&(9_100_301, "NSE_FNO")).map(|s| &**s),
            Some("BADNAMEXCE")
        );
        assert_eq!(
            t.get(&(9_100_302, "NSE_FNO")).map(|s| &**s),
            Some("CLEAN-CE")
        );
        assert!(t.values().all(|n| label_is_ilp_clean(n)));
        drop(t);

        publish_candle_contract_labels(CandleContractLabels::default());
        let mut boot = CandleContractLabels::default();
        boot.insert((13, "IDX_I"), Arc::<str>::from("NIF,TY"));
        assert!(publish_candle_contract_labels_if_empty(boot));
        assert_eq!(
            candle_contract_labels().get(&(13, "IDX_I")).map(|s| &**s),
            Some("NIFTY")
        );
        publish_candle_contract_labels(CandleContractLabels::default());
    }

    #[test]
    fn test_label_is_ilp_clean_matches_the_sanitizer() {
        assert!(label_is_ilp_clean("NIFTY-25Sep2026-24500-CE"));
        assert!(!label_is_ilp_clean("A,B"));
        assert!(!label_is_ilp_clean("A=B"));
        assert!(!label_is_ilp_clean("A\nB"));
    }

    /// PF-4 owner decision (2026-11-06): a name longer than 64 characters is
    /// cut to 64 at publish, by both publish paths, on a character boundary;
    /// a name at or under the cap is untouched.
    #[test]
    fn test_publish_cuts_a_name_over_64_characters_to_64() {
        let _g = lock();
        let long: String = "N".repeat(70);
        let exact: String = "E".repeat(CONTRACT_LABEL_MAX_CHARS);
        let wide: String = "\u{1F600}".repeat(65);
        let mut m = CandleContractLabels::default();
        m.insert((9_100_401, "NSE_FNO"), Arc::<str>::from(long.as_str()));
        m.insert((9_100_402, "NSE_FNO"), Arc::<str>::from(exact.as_str()));
        m.insert((9_100_403, "NSE_FNO"), Arc::<str>::from(wide.as_str()));
        publish_candle_contract_labels(m);
        let t = candle_contract_labels();
        assert_eq!(
            t.get(&(9_100_401, "NSE_FNO")).map(|s| s.to_string()),
            Some("N".repeat(64))
        );
        assert_eq!(
            t.get(&(9_100_402, "NSE_FNO")).map(|s| s.to_string()),
            Some(exact.clone()),
            "exactly 64 is kept whole"
        );
        let cut = t.get(&(9_100_403, "NSE_FNO")).map(|s| s.to_string());
        assert_eq!(cut.as_deref().map(|s| s.chars().count()), Some(64));
        assert!(
            t.values()
                .all(|n| n.chars().count() <= CONTRACT_LABEL_MAX_CHARS)
        );
        drop(t);

        publish_candle_contract_labels(CandleContractLabels::default());
        let mut boot = CandleContractLabels::default();
        boot.insert((13, "IDX_I"), Arc::<str>::from(long.as_str()));
        assert!(publish_candle_contract_labels_if_empty(boot));
        assert_eq!(
            candle_contract_labels()
                .get(&(13, "IDX_I"))
                .map(|s| s.chars().count()),
            Some(64)
        );
        publish_candle_contract_labels(CandleContractLabels::default());
    }

    #[test]
    fn test_truncate_label_boundaries() {
        assert_eq!(truncate_label(""), None);
        assert_eq!(truncate_label(&"a".repeat(64)), None);
        assert_eq!(
            truncate_label(&"a".repeat(65)),
            Some("a".repeat(64).as_str())
        );
        let wide = "\u{e9}".repeat(65);
        assert_eq!(truncate_label(&wide).map(|s| s.chars().count()), Some(64));
    }

    /// A clean name keeps its allocation; only a dirty one is replaced.
    #[test]
    fn test_sanitize_labels_keeps_a_clean_names_allocation() {
        let clean = Arc::<str>::from("CLEAN-CE");
        let mut m = CandleContractLabels::default();
        m.insert((1, "NSE_FNO"), Arc::clone(&clean));
        let out = sanitize_labels(m);
        let kept = out.get(&(1, "NSE_FNO")).expect("present");
        assert!(Arc::ptr_eq(kept, &clean));
    }
}
