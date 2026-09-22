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
//! * A seal re-ingested by the BOOT drain (spill/DLQ) can reach the writer
//!   before the day's table is published. That bar is written with a NULL
//!   `contract` — the same omission, never a guess.

use std::collections::HashMap;
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
pub type CandleContractLabels = HashMap<CandleContractKey, Arc<str>>;

static LABELS: LazyLock<ArcSwap<CandleContractLabels>> =
    LazyLock::new(|| ArcSwap::from_pointee(HashMap::new()));

/// Replaces the whole table atomically and returns how many names it holds.
///
/// A replace, never a merge: yesterday's names for expired ids must not
/// survive into today, and a merge would keep them forever.
pub fn publish_candle_contract_labels(labels: CandleContractLabels) -> usize {
    let n = labels.len();
    LABELS.store(Arc::new(labels));
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
    let incoming = Arc::new(labels);
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
        metrics::gauge!("tv_candle_contract_labels_published").set(incoming.len() as f64);
    }
    stored
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

    #[test]
    fn test_publish_candle_contract_labels_finds_a_label_under_its_own_segment_only() {
        let _g = lock();
        let mut m = HashMap::new();
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
        publish_candle_contract_labels(HashMap::new());
        let mut boot = HashMap::new();
        boot.insert((13, "IDX_I"), Arc::<str>::from("NIFTY"));
        assert!(publish_candle_contract_labels_if_empty(boot));
        assert_eq!(
            candle_contract_labels().get(&(13, "IDX_I")).map(|s| &**s),
            Some("NIFTY")
        );

        // The attach replaces the whole table with spots + options.
        let mut attach = HashMap::new();
        attach.insert((13, "IDX_I"), Arc::<str>::from("NIFTY"));
        attach.insert((9_100_201, "NSE_FNO"), Arc::<str>::from("NIFTY-CE"));
        publish_candle_contract_labels(attach);

        // A late boot publish must NOT wipe the option name.
        let mut late = HashMap::new();
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
        let mut first = HashMap::new();
        first.insert((9_100_101, "NSE_FNO"), Arc::<str>::from("OLD-EXPIRED-CE"));
        publish_candle_contract_labels(first);
        let mut second = HashMap::new();
        second.insert((9_100_102, "NSE_FNO"), Arc::<str>::from("NEW-CE"));
        assert_eq!(publish_candle_contract_labels(second), 1);
        let t = candle_contract_labels();
        assert!(
            t.get(&(9_100_101, "NSE_FNO")).is_none(),
            "an expired name must not survive a republish"
        );
    }
}
