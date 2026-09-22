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
//! * Only OPTION contracts carry a label (the app builds it from the `OPTIDX`
//!   / `OPTSTK` rows). A spot, index or future candle writes NO `contract`, so
//!   the column reads NULL there. A name is never fabricated.
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
