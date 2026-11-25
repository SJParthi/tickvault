//! Retain dated MTF receipts across a same-session restart. Validity ends at
//! the integer instant of the superseding observation, including an unknown one.
use super::DailyMtfTerms;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;
use tickvault_common::mtf_receipt_json::{MtfHistory, MtfLeverage};
use tickvault_common::operator_movers_universe::{
    MAX_DAILY_MAPPING_BYTES, MAX_MTF_HISTORY_INTERVALS, MAX_UNIVERSE_LIST_ROWS, ist_date_of_epoch,
};

#[derive(Debug, Serialize)]
pub(super) struct Interval {
    #[serde(flatten)]
    terms: DailyMtfTerms,
    mtf_valid_before_epoch_secs: i64,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct ReceiptState {
    #[serde(flatten)]
    pub(super) current: Option<DailyMtfTerms>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) mtf_history: Vec<Interval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mtf_last_refresh_epoch_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mtf_latest_source_modified_at_epoch_secs: Option<i64>,
}

#[derive(Deserialize)]
struct DatedReceipts {
    trading_date_ist: String,
    mtf_leverage_by_security_id: Option<MtfLeverage>,
    mtf_observed_at_epoch_secs: Option<i64>,
    mtf_source_modified_at_epoch_secs: Option<i64>,
    #[serde(default)]
    mtf_history: MtfHistory,
    #[serde(
        default,
        rename = "mtf_valid_before_epoch_secs",
        deserialize_with = "unexpected_current_end"
    )]
    has_current_end: bool,
    mtf_last_refresh_epoch_secs: Option<i64>,
    mtf_latest_source_modified_at_epoch_secs: Option<i64>,
}

fn unexpected_current_end<'de, D: serde::Deserializer<'de>>(de: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(de).map(|_| true)
}

impl DatedReceipts {
    fn into_state(self, date: &str) -> Option<ReceiptState> {
        let serde_json::Value::Array(history) = self.mtf_history.0 else {
            return None;
        };
        if self.trading_date_ist != date
            || self.has_current_end
            || history.len() > MAX_MTF_HISTORY_INTERVALS
        {
            return None;
        }
        let current = match (
            self.mtf_leverage_by_security_id,
            self.mtf_observed_at_epoch_secs,
            self.mtf_source_modified_at_epoch_secs,
        ) {
            (None, None, None) => None,
            (Some(MtfLeverage(map)), Some(observed), Some(modified)) => Some(DailyMtfTerms {
                mtf_leverage_by_security_id: parse_leverage(map)?,
                mtf_observed_at_epoch_secs: observed,
                mtf_source_modified_at_epoch_secs: modified,
            }),
            _ => return None,
        };
        let mut prior = Vec::with_capacity(history.len());
        for mut entry in history {
            let end = entry.get("mtf_valid_before_epoch_secs")?.as_i64()?;
            let terms = DailyMtfTerms {
                mtf_observed_at_epoch_secs: entry.get("mtf_observed_at_epoch_secs")?.as_i64()?,
                mtf_source_modified_at_epoch_secs: entry
                    .get("mtf_source_modified_at_epoch_secs")?
                    .as_i64()?,
                mtf_leverage_by_security_id: parse_leverage(
                    entry
                        .as_object_mut()?
                        .remove("mtf_leverage_by_security_id")?,
                )?,
            };
            prior.push(Interval {
                terms,
                mtf_valid_before_epoch_secs: end,
            });
        }
        let state = ReceiptState {
            current,
            mtf_history: prior,
            mtf_last_refresh_epoch_secs: self.mtf_last_refresh_epoch_secs,
            mtf_latest_source_modified_at_epoch_secs: self.mtf_latest_source_modified_at_epoch_secs,
        };
        state.valid_on(date).then_some(state)
    }
}

// APPROVED: the daily master's wire map contains only the NSE/E/EQ/EQUITY segment.
fn parse_leverage(value: serde_json::Value) -> Option<std::collections::HashMap<u64, f64>> {
    let serde_json::Value::Object(entries) = value else {
        return None;
    };
    if entries.len() > MAX_UNIVERSE_LIST_ROWS {
        return None;
    }
    entries
        .into_iter()
        .map(|(name, value)| {
            let id: u64 = name.parse().ok()?;
            (id.to_string() == name).then_some((id, value.as_f64()?))
        })
        .collect()
}

impl DailyMtfTerms {
    fn valid_on(&self, date: &str) -> bool {
        self.mtf_source_modified_at_epoch_secs <= self.mtf_observed_at_epoch_secs
            && [
                self.mtf_source_modified_at_epoch_secs,
                self.mtf_observed_at_epoch_secs,
            ]
            .into_iter()
            .all(|t| ist_date_of_epoch(t).as_deref() == Some(date))
            && !self.mtf_leverage_by_security_id.is_empty()
            && self.mtf_leverage_by_security_id.len() <= MAX_UNIVERSE_LIST_ROWS
            && self
                .mtf_leverage_by_security_id
                .iter()
                .all(|(&id, &n)| id > 0 && (n == 0.0 || (n.is_finite() && n >= 1.0)))
    }

    fn same_publication(&self, other: &Self) -> bool {
        self.mtf_source_modified_at_epoch_secs == other.mtf_source_modified_at_epoch_secs
            && self.mtf_leverage_by_security_id == other.mtf_leverage_by_security_id
    }
}

impl ReceiptState {
    fn valid_on(&self, date: &str) -> bool {
        let mut last_end = 0;
        let mut last_source = 0;
        if self.mtf_history.len() > MAX_MTF_HISTORY_INTERVALS {
            return false;
        }
        for interval in &self.mtf_history {
            let start = interval.terms.mtf_observed_at_epoch_secs;
            let end = interval.mtf_valid_before_epoch_secs;
            if !interval.terms.valid_on(date)
                || start < last_end
                || interval.terms.mtf_source_modified_at_epoch_secs < last_source
                || start >= end
                || ist_date_of_epoch(end).as_deref() != Some(date)
            {
                return false;
            }
            last_end = end;
            last_source = interval.terms.mtf_source_modified_at_epoch_secs;
        }
        if let Some(terms) = &self.current {
            if !terms.valid_on(date)
                || terms.mtf_observed_at_epoch_secs < last_end
                || terms.mtf_source_modified_at_epoch_secs < last_source
            {
                return false;
            }
            last_end = terms.mtf_observed_at_epoch_secs;
        }
        if let Some(source) = self.mtf_latest_source_modified_at_epoch_secs
            && (ist_date_of_epoch(source).as_deref() != Some(date)
                || self
                    .mtf_last_refresh_epoch_secs
                    .is_none_or(|seen| source > seen)
                || self
                    .verifiable_source_time()
                    .is_some_and(|verified| source < verified))
        {
            return false;
        }
        self.mtf_last_refresh_epoch_secs
            .is_none_or(|seen| seen >= last_end && ist_date_of_epoch(seen).as_deref() == Some(date))
    }

    fn verifiable_source_time(&self) -> Option<i64> {
        self.current
            .as_ref()
            .or_else(|| self.mtf_history.last().map(|i| &i.terms))
            .map(|t| t.mtf_source_modified_at_epoch_secs)
    }

    fn most_recent(&self) -> Option<i64> {
        self.mtf_last_refresh_epoch_secs
            .or_else(|| self.current.as_ref().map(|t| t.mtf_observed_at_epoch_secs))
            .or_else(|| {
                self.mtf_history
                    .last()
                    .map(|i| i.mtf_valid_before_epoch_secs)
            })
    }

    fn read(path: &Path, date: &str) -> std::io::Result<Self> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(MAX_DAILY_MAPPING_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_DAILY_MAPPING_BYTES {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice::<DatedReceipts>(&bytes)
            .ok()
            .and_then(|prev| prev.into_state(date))
            .unwrap_or_default())
    }

    /// A slow refresh can finish after a newer response was committed. A
    /// conflicting earlier receipt shortens only the interval it contradicts.
    /// It cannot replace newer evidence or establish terms inside a known gap.
    fn delayed_refresh(&mut self, at: i64, delayed: Option<&DailyMtfTerms>, last: i64) {
        let agrees = |terms: &DailyMtfTerms| delayed.is_some_and(|d| terms.same_publication(d));
        if let Some(active) = self.current.as_mut()
            && active.mtf_observed_at_epoch_secs <= at
        {
            if !agrees(active) {
                if active.mtf_observed_at_epoch_secs < at {
                    self.mtf_history.push(Interval {
                        terms: active.clone(),
                        mtf_valid_before_epoch_secs: at,
                    });
                }
                active.mtf_observed_at_epoch_secs = last;
            }
            return;
        }
        let pos = self
            .mtf_history
            .partition_point(|i| i.mtf_valid_before_epoch_secs <= at);
        if let Some(interval) = self.mtf_history.get_mut(pos)
            && interval.terms.mtf_observed_at_epoch_secs <= at
            && !agrees(&interval.terms)
        {
            if interval.terms.mtf_observed_at_epoch_secs == at {
                self.mtf_history.remove(pos);
            } else {
                interval.mtf_valid_before_epoch_secs = at;
            }
        }
    }

    pub(super) fn refresh(
        path: &Path,
        date: &str,
        at: i64,
        current: Option<DailyMtfTerms>,
    ) -> std::io::Result<Self> {
        let mut old = Self::read(path, date)?;
        let last = old.most_recent();
        if ist_date_of_epoch(at).as_deref() != Some(date) {
            return Ok(old);
        }
        let current = current.filter(|t| t.valid_on(date) && t.mtf_observed_at_epoch_secs == at);
        // The watermark survives eviction and carries no eligibility on its own.
        let last_publication = old
            .mtf_latest_source_modified_at_epoch_secs
            .or_else(|| old.verifiable_source_time());
        if let Some(seen) = last
            && at < seen
        {
            old.delayed_refresh(at, current.as_ref(), seen);
            if let Some(delayed) = current {
                let source = delayed.mtf_source_modified_at_epoch_secs;
                // A newer publication already existed at `at`. Subsequent
                // receipts of older publications are rollbacks, even when the
                // network delivered the newer evidence after those receipts.
                old.mtf_history.retain(|i| {
                    i.mtf_valid_before_epoch_secs <= at
                        || i.terms.mtf_source_modified_at_epoch_secs >= source
                });
                if old
                    .current
                    .as_ref()
                    .is_some_and(|t| t.mtf_source_modified_at_epoch_secs < source)
                {
                    old.current = None;
                }
                old.mtf_latest_source_modified_at_epoch_secs = last_publication.max(Some(source));
                old.mtf_last_refresh_epoch_secs = Some(seen);
            }
            return Ok(old);
        }
        let mut current = current.filter(|t| {
            last_publication.is_none_or(|last| t.mtf_source_modified_at_epoch_secs >= last)
        });
        old.mtf_latest_source_modified_at_epoch_secs = current
            .as_ref()
            .map(|t| t.mtf_source_modified_at_epoch_secs)
            .or(last_publication);
        if let Some(previous) = old.current.take() {
            if current
                .as_ref()
                .is_some_and(|new| previous.same_publication(new))
            {
                current = Some(previous);
            } else {
                // Revisions may share an integer observation time. A zero-length
                // receipt proves nothing at an earlier representable cutoff.
                if previous.mtf_observed_at_epoch_secs < at {
                    old.mtf_history.push(Interval {
                        terms: previous,
                        mtf_valid_before_epoch_secs: at,
                    });
                }
                if last == Some(at) {
                    current = None;
                }
            }
        } else if last == Some(at) {
            // No ordering proof between a recorded unknown and a contradictory
            // observation at the same representable instant.
            current = None;
        }
        old.current = current;
        old.mtf_last_refresh_epoch_secs = Some(at);
        Ok(old)
    }
}

/// The compact base object contains all non-history fields. JSON adds this
/// key and one comma between receipts; retain the most recent suffix that fits.
pub(super) fn fitting_history(
    history: Vec<Interval>,
    base_len: usize,
    max_bytes: usize,
) -> serde_json::Result<Vec<Interval>> {
    let mut left = max_bytes.saturating_sub(base_len + b",\"mtf_history\":[]".len());
    let mut kept = Vec::new();
    for interval in history.into_iter().rev().take(MAX_MTF_HISTORY_INTERVALS) {
        let cost = serde_json::to_vec(&interval)?.len() + usize::from(!kept.is_empty());
        if cost > left {
            break;
        }
        left -= cost;
        kept.push(interval);
    }
    kept.reverse();
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(stamp: i64, n: f64) -> DailyMtfTerms {
        DailyMtfTerms {
            mtf_observed_at_epoch_secs: stamp,
            mtf_source_modified_at_epoch_secs: stamp - 1,
            mtf_leverage_by_security_id: [(1, n)].into(),
        }
    }
    fn interval(stamp: i64) -> Interval {
        Interval {
            terms: terms(stamp, 2.0),
            mtf_valid_before_epoch_secs: stamp + 1,
        }
    }
    #[test]
    fn raw_mtf_recovery_never_restores_conflicting_ids_or_duplicate_interval_boundaries() {
        let date = "2026-11-25";
        let stamp = 1_795_582_800;
        let path =
            std::env::temp_dir().join(format!("tv-mtf-raw-restore-{}.json", std::process::id()));
        let map = r#""mtf_leverage_by_security_id":{"1":0,"\u0031":2.5,"2":1,"3":0}"#;
        let current = format!(
            r#"{map},"mtf_observed_at_epoch_secs":{stamp},"mtf_source_modified_at_epoch_secs":{stamp}"#
        );
        let history = format!(
            r#"{{{current},"mtf_valid_before_epoch_secs":{}}}"#,
            stamp + 3
        );
        let write = |proof: &str| {
            let raw = format!(r#"{{"trading_date_ist":"{date}",{proof}}}"#);
            std::fs::write(&path, raw).unwrap();
        };
        write(&current);
        let after = ReceiptState::refresh(&path, date, stamp + 3, None).unwrap();
        assert!(after.current.is_none());
        assert_eq!(
            after.mtf_history[0].terms.mtf_leverage_by_security_id,
            std::collections::HashMap::from([(2u64, 1.0), (3, 0.0)])
        );
        let prior = format!(r#""mtf_history":[{history}]"#);
        write(&prior);
        let restored = ReceiptState::read(&path, date).unwrap();
        assert_eq!(
            restored.mtf_history[0].terms.mtf_leverage_by_security_id,
            std::collections::HashMap::from([(2u64, 1.0), (3, 0.0)])
        );
        let wrong_end = history.replace(
            r#""mtf_valid_before_epoch_secs":"#,
            r#""mtf_valid_before_epoch_secs":0,"mtf_valid_before_epoch_secs":"#,
        );
        for bad in [
            format!(r#""mtf_history":[{wrong_end}]"#),
            format!("{prior},{prior}"),
            format!(r#"{current},"mtf_valid_before_epoch_secs":null"#),
            format!(r#"{current},"mtf_valid_before_epoch_secs":{stamp}"#),
        ] {
            write(&bad);
            let unknown = ReceiptState::read(&path, date).unwrap();
            assert!(
                unknown.current.is_none() && unknown.mtf_history.is_empty(),
                "{bad}"
            );
        }
        write(&format!(
            r#"{prior},"unknown_extension":1,"unknown_extension":2"#
        ));
        assert_eq!(
            ReceiptState::read(&path, date).unwrap().mtf_history.len(),
            1
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_mtf_history_cannot_be_rewritten_as_verified_eligibility() {
        let date = "2026-11-06";
        let stamp = chrono::DateTime::parse_from_rfc3339("2026-11-06T04:00:00Z")
            .unwrap()
            .timestamp();
        let mut original = serde_json::to_value(terms(stamp + 3, 2.0)).unwrap();
        original["trading_date_ist"] = date.into();
        original["mtf_history"] = serde_json::json!([interval(stamp)]);
        let check = |v: serde_json::Value| {
            serde_json::from_value::<DatedReceipts>(v)
                .ok()
                .and_then(|r| r.into_state(date))
        };
        assert!(check(original.clone()).is_some());
        let mut invalid = original.clone();
        invalid["mtf_history"][0]["mtf_valid_before_epoch_secs"] = (stamp + 4).into();
        assert!(check(invalid).is_none());
        let mut invalid = original.clone();
        invalid["mtf_leverage_by_security_id"] = serde_json::json!({"01":12});
        assert!(check(invalid).is_none());
        let mut invalid = original.clone();
        invalid
            .as_object_mut()
            .unwrap()
            .remove("mtf_source_modified_at_epoch_secs");
        assert!(check(invalid).is_none());
        original.as_object_mut().unwrap().remove("mtf_history");
        let legacy = check(original).unwrap();
        assert!(legacy.current.is_some() && legacy.mtf_history.is_empty());
    }

    #[test]
    fn delayed_newer_publication_invalidates_later_rollbacks_and_preserves_other_evidence() {
        let path = std::env::temp_dir().join(format!(
            "tv-mtf-delayed-revision-{}.json",
            std::process::id()
        ));
        let date = "2026-11-25";
        let stamp = 1_795_582_800;
        let refresh = |state: &ReceiptState, offset, publication, leverage| {
            let mut disk = serde_json::to_value(state).unwrap();
            disk["trading_date_ist"] = date.into();
            std::fs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
            let mut observed = terms(stamp + offset, leverage);
            observed.mtf_source_modified_at_epoch_secs = stamp + publication;
            let state = ReceiptState::refresh(&path, date, stamp + offset, Some(observed)).unwrap();
            assert!(state.valid_on(date));
            state
        };
        let first = refresh(&ReceiptState::default(), 0, -5, 1.0);
        let old = refresh(&first, 4, -3, 1.5);
        let delayed = refresh(&old, 2, -1, 2.0);
        assert!(delayed.current.is_none());
        assert_eq!(delayed.mtf_history.len(), 1);
        assert_eq!(
            delayed.mtf_history[0].mtf_valid_before_epoch_secs,
            stamp + 2
        );
        assert_eq!(
            delayed.mtf_latest_source_modified_at_epoch_secs,
            Some(stamp - 1)
        );
        let still_stale = refresh(&delayed, 5, -3, 1.5);
        assert!(still_stale.current.is_none());
        let verified = refresh(&still_stale, 6, -1, 2.0);
        assert_eq!(
            verified.current.unwrap().mtf_observed_at_epoch_secs,
            stamp + 6
        );

        // If an even newer tail is already present, the affected middle
        // interval still must be removed while the tail remains provable.
        let newer_tail = refresh(&old, 8, 1, 3.0);
        let repaired = refresh(&newer_tail, 2, -1, 2.0);
        assert_eq!(repaired.mtf_history.len(), 1);
        assert_eq!(
            repaired.mtf_history[0].mtf_valid_before_epoch_secs,
            stamp + 2
        );
        assert_eq!(
            repaired.current.unwrap().mtf_observed_at_epoch_secs,
            stamp + 8
        );
        assert_eq!(
            repaired.mtf_latest_source_modified_at_epoch_secs,
            Some(stamp + 1)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_refresh_uses_evicted_source_watermark_and_rejects_a_same_second_conflict() {
        let path = std::env::temp_dir().join(format!("tv-mtf-receipt-{}.json", std::process::id()));
        let date = "2026-11-06";
        let stamp = chrono::DateTime::parse_from_rfc3339("2026-11-06T04:00:00Z")
            .unwrap()
            .timestamp();
        let save = |state: &ReceiptState| {
            let mut disk = serde_json::to_value(state).unwrap();
            disk["trading_date_ist"] = date.into();
            std::fs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
        };
        let initial = ReceiptState {
            mtf_last_refresh_epoch_secs: Some(stamp),
            mtf_latest_source_modified_at_epoch_secs: Some(stamp - 5),
            ..Default::default()
        };
        save(&initial);
        let mut old = terms(stamp + 1, 2.5);
        old.mtf_source_modified_at_epoch_secs = stamp - 10;
        let blocked = ReceiptState::refresh(&path, date, stamp + 1, Some(old)).unwrap();
        assert!(blocked.current.is_none() && blocked.mtf_history.is_empty());
        save(&blocked);
        let newer = terms(stamp + 2, 3.0);
        let accepted = ReceiptState::refresh(&path, date, stamp + 2, Some(newer)).unwrap();
        assert!(accepted.current.is_some());
        save(&accepted);
        let contested =
            ReceiptState::refresh(&path, date, stamp + 2, Some(terms(stamp + 2, 1.5))).unwrap();
        assert!(contested.current.is_none() && contested.mtf_history.is_empty());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delayed_mtf_conflict_preserves_confirmed_endpoints_and_only_creates_unknowns() {
        let mut state = ReceiptState {
            current: Some(terms(100, 2.0)),
            mtf_last_refresh_epoch_secs: Some(120),
            ..Default::default()
        };
        let mut same = terms(100, 2.0);
        same.mtf_observed_at_epoch_secs = 110;
        state.delayed_refresh(110, Some(&same), 120);
        assert!(state.mtf_history.is_empty());
        state.delayed_refresh(110, None, 120);
        assert_eq!(
            state.current.as_ref().unwrap().mtf_observed_at_epoch_secs,
            120
        );
        assert_eq!(state.mtf_history[0].mtf_valid_before_epoch_secs, 110);
        state.delayed_refresh(115, None, 120);
        assert_eq!(state.mtf_history.len(), 1);
        state.delayed_refresh(100, None, 120);
        assert!(state.mtf_history.is_empty());
        assert_eq!(
            state.current.as_ref().unwrap().mtf_observed_at_epoch_secs,
            120
        );
    }

    #[test]
    fn mtf_byte_budget_preserves_ordinary_mapping_and_omits_whole_optional_terms() {
        let mut artifact = super::super::MappingArtifact {
            trading_date_ist: "2026-11-06".into(),
            mtf: ReceiptState::default(),
            master_rows: 1,
            isin_index_len: 1,
            ambiguous_isins: 0,
            constituents_seen: 1,
            resolved: 1,
            unresolved: 0,
            unresolved_fraction: 0.0,
            distinct_instruments: 1,
            unresolved_detail: vec![],
            mappings: vec![super::super::MappingEntry {
                index_name: "Nifty 50".into(),
                symbol: "TEST".into(),
                isin: "INE000000001".into(),
                security_id: 1,
                exchange_segment: 1,
            }],
        };
        let ordinary = serde_json::to_vec(&artifact).unwrap();
        artifact.mtf.current = Some(terms(100, 3.75));
        artifact.mtf.mtf_history = vec![interval(98)];
        let bytes = artifact.encode(ordinary.len()).unwrap();
        assert_eq!(bytes, ordinary);
        let bytes = artifact.encode(ordinary.len() - 1).unwrap();
        assert_eq!(
            bytes, ordinary,
            "capture still receives all mandatory broad mapping fields"
        );
    }

    #[test]
    fn test_fitting_history_retains_only_newest_whole_mtf_intervals() {
        let three = || (100..103).map(interval).collect::<Vec<_>>();
        let base = br#"{"a":1}"#;
        let mut record = serde_json::json!({"a":1});
        record["mtf_history"] = serde_json::to_value(&three()[1..]).unwrap();
        let two_bytes = serde_json::to_vec(&record).unwrap().len();
        for (cap, expected) in [
            (two_bytes, vec![101, 102]),
            (two_bytes - 1, vec![102]),
            (1, vec![]),
        ] {
            let kept = fitting_history(three(), base.len(), cap).unwrap();
            assert_eq!(
                kept.iter()
                    .map(|v| v.terms.mtf_observed_at_epoch_secs)
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
}
