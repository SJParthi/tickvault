//! Read optional broker proofs before JSON objects can collapse duplicate keys.
//! Unknown extension fields in an interval do not participate in its proof.
use crate::operator_movers_universe::{MAX_MTF_HISTORY_INTERVALS, MAX_UNIVERSE_LIST_ROWS};
use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet, btree_map::Entry};
use std::fmt;

/// Canonical security IDs with unambiguous terms. A malformed or oversized
/// container carries JSON null, so a caller can still read ordinary Stocks.
pub struct MtfLeverage(pub Value);

/// Dated intervals containing only fields that can establish or end eligibility.
pub struct MtfHistory(pub Value);

impl Default for MtfHistory {
    fn default() -> Self {
        Self(Value::Array(Vec::new()))
    }
}

impl<'de> Deserialize<'de> for MtfLeverage {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(Shape::Leverage).map(Self)
    }
}
impl<'de> Deserialize<'de> for MtfHistory {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(Shape::History).map(Self)
    }
}

/// Scalar proof field. Unexpected collections are consumed without retaining their contents.
pub struct MtfScalar(pub Value);
impl<'de> Deserialize<'de> for MtfScalar {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(Shape::Scalar).map(Self)
    }
}

struct Interval(Value);

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(Shape::Interval).map(Self)
    }
}

enum Shape {
    Scalar,
    Leverage,
    History,
    Interval,
}

impl<'de> Visitor<'de> for Shape {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an MTF proof")
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(if matches!(self, Self::Scalar) {
            Value::from(value)
        } else {
            Value::Null
        })
    }
    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(if matches!(self, Self::Scalar) {
            Value::from(value)
        } else {
            Value::Null
        })
    }
    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(if matches!(self, Self::Scalar) {
            Value::from(value)
        } else {
            Value::Null
        })
    }
    fn visit_str<E>(self, _: &str) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        match self {
            Self::Leverage => Ok(
                match read_unique_ids(map, |map| map.next_value::<MtfScalar>().map(Some))? {
                    Some(ids) => Value::Object(
                        ids.into_iter()
                            .map(|(id, n)| (id.to_string(), n.0))
                            .collect(),
                    ),
                    None => Value::Null,
                },
            ),
            Self::Interval => read_interval(map),
            Self::History | Self::Scalar => {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Value::Null)
            }
        }
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        if matches!(self, Self::History) {
            let mut history = Vec::new();
            while history.len() < MAX_MTF_HISTORY_INTERVALS {
                let Some(Interval(interval)) = seq.next_element()? else {
                    return Ok(Value::Array(history));
                };
                if interval.is_null() {
                    while seq.next_element::<IgnoredAny>()?.is_some() {}
                    return Ok(Value::Null);
                }
                history.push(interval);
            }
            if seq.next_element::<IgnoredAny>()?.is_none() {
                return Ok(Value::Array(history));
            }
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            return Ok(Value::Null);
        }
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Value::Null)
    }
}

/// Valid daily and published maps use one canonical positive decimal ID per security.
fn read_unique_ids<'de, A: MapAccess<'de>, T>(
    mut map: A,
    mut next: impl FnMut(&mut A) -> Result<Option<T>, A::Error>,
) -> Result<Option<BTreeMap<u64, T>>, A::Error> {
    let mut ids = BTreeMap::<u64, T>::new();
    let mut disputed = HashSet::new();
    let mut entries = 0usize;
    while let Some(key) = map.next_key::<String>()? {
        entries += 1;
        if entries > MAX_UNIVERSE_LIST_ROWS {
            map.next_value::<IgnoredAny>()?;
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            return Ok(None);
        }
        let Ok(id) = key.trim().parse::<u64>() else {
            map.next_value::<IgnoredAny>()?;
            continue;
        };
        // A parsed integer has its canonical spelling exactly when it begins
        // with 1–9 and ends in a digit (no leading zero, sign or whitespace).
        let canonical = matches!(key.as_bytes().first(), Some(b'1'..=b'9'))
            && key.as_bytes().last().is_some_and(u8::is_ascii_digit);
        match ids.entry(id) {
            Entry::Vacant(e) if canonical && !disputed.contains(&id) => {
                if let Some(value) = next(&mut map)? {
                    e.insert(value);
                } else {
                    disputed.insert(id);
                }
            }
            other => {
                // A spelling or value encountered twice remains unknown on all
                // subsequent occurrences, irrespective of their ordering.
                if let Entry::Occupied(existing) = other {
                    existing.remove();
                }
                disputed.insert(id);
                map.next_value::<IgnoredAny>()?;
            }
        }
    }
    Ok(Some(ids))
}

pub(crate) fn deserialize_mtf_leverage_map<'de, D: Deserializer<'de>>(
    de: D,
) -> Result<BTreeMap<u64, f64>, D::Error> {
    struct WireMap;
    impl<'de> Visitor<'de> for WireMap {
        type Value = BTreeMap<u64, f64>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an NSE equity leverage map")
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            Ok(read_unique_ids(map, |map| {
                map.next_value::<MtfScalar>().map(|v| v.0.as_f64())
            })?
            .unwrap_or_default())
        }
    }
    de.deserialize_map(WireMap)
}

fn read_interval<'de, A: MapAccess<'de>>(mut map: A) -> Result<Value, A::Error> {
    let mut fields = Map::new();
    while let Some(key) = map.next_key::<String>()? {
        if fields.contains_key(&key) {
            map.next_value::<IgnoredAny>()?;
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            return Ok(Value::Null);
        }
        let value = match key.as_str() {
            "mtf_observed_at_epoch_secs"
            | "mtf_source_modified_at_epoch_secs"
            | "mtf_valid_before_epoch_secs" => map.next_value::<MtfScalar>()?.0,
            "mtf_leverage_by_security_id" => map.next_value::<MtfLeverage>()?.0,
            _ => {
                map.next_value::<IgnoredAny>()?;
                continue;
            }
        };
        fields.insert(key, value);
    }
    Ok(Value::Object(fields))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mtf_budgets_count_raw_entries_and_preserve_the_last_valid_size() {
        let rows = (1..=MAX_UNIVERSE_LIST_ROWS)
            .map(|id| format!(r#""{id}":1"#))
            .collect::<Vec<_>>()
            .join(",");
        let last = format!("{{{rows}}}");
        assert_eq!(
            serde_json::from_str::<MtfLeverage>(&last)
                .unwrap()
                .0
                .as_object()
                .unwrap()
                .len(),
            MAX_UNIVERSE_LIST_ROWS
        );
        let extra = format!("{{{rows},\"1\":1}}");
        assert!(
            serde_json::from_str::<MtfLeverage>(&extra)
                .unwrap()
                .0
                .is_null()
        );
        let disk = format!(r#"{{"observed_at_epoch_secs":1,"leverage_by_security_id":{extra}}}"#);
        assert!(
            serde_json::from_str::<crate::operator_movers_universe::MtfTerms>(&disk)
                .unwrap()
                .leverage_by_security_id
                .is_empty()
        );
        let receipts = vec!["{}"; MAX_MTF_HISTORY_INTERVALS].join(",");
        assert_eq!(
            serde_json::from_str::<MtfHistory>(&format!("[{receipts}]"))
                .unwrap()
                .0
                .as_array()
                .unwrap()
                .len(),
            MAX_MTF_HISTORY_INTERVALS
        );
        assert!(
            serde_json::from_str::<MtfHistory>(&format!("[{receipts},{{}}]"))
                .unwrap()
                .0
                .is_null()
        );
    }
}
