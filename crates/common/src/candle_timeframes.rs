//! Shared active candle contract. Configuration, aggregation and metrics use
//! this same ordered set; wire identities are independent of dense RAM slots.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CandleTimeframeSpec {
    pub label: &'static str,
    pub table_name: &'static str,
    pub seconds: u32,
    /// Stable seal-spill/DLQ identity. Retired IDs must never be reused.
    pub storage_ordinal: u8,
}

/// Exactly the ten frames requested on 2026-09-14, in duration order.
/// The new ten-minute frame appends durable ID 24; dense RAM slots are 0..10.
pub const ACTIVE_CANDLE_TIMEFRAMES: [CandleTimeframeSpec; 10] = [
    CandleTimeframeSpec {
        label: "1s",
        table_name: "candles_1s",
        seconds: 1,
        storage_ordinal: 5,
    },
    CandleTimeframeSpec {
        label: "3s",
        table_name: "candles_3s",
        seconds: 3,
        storage_ordinal: 7,
    },
    CandleTimeframeSpec {
        label: "5s",
        table_name: "candles_5s",
        seconds: 5,
        storage_ordinal: 9,
    },
    CandleTimeframeSpec {
        label: "1m",
        table_name: "candles_1m",
        seconds: 60,
        storage_ordinal: 0,
    },
    CandleTimeframeSpec {
        label: "3m",
        table_name: "candles_3m",
        seconds: 180,
        storage_ordinal: 1,
    },
    CandleTimeframeSpec {
        label: "5m",
        table_name: "candles_5m",
        seconds: 300,
        storage_ordinal: 2,
    },
    CandleTimeframeSpec {
        label: "10m",
        table_name: "candles_10m",
        seconds: 600,
        storage_ordinal: 24,
    },
    CandleTimeframeSpec {
        label: "15m",
        table_name: "candles_15m",
        seconds: 900,
        storage_ordinal: 3,
    },
    CandleTimeframeSpec {
        label: "30m",
        table_name: "candles_30m",
        seconds: 1_800,
        storage_ordinal: 22,
    },
    CandleTimeframeSpec {
        label: "60m",
        table_name: "candles_60m",
        seconds: 3_600,
        storage_ordinal: 23,
    },
];

/// Top Volume currently ranks only these four short candle frames.
/// The remaining active candle frames are still aggregated and persisted.
pub const TOP_VOLUME_CANDLE_TIMEFRAMES: [CandleTimeframeSpec; 4] = [
    ACTIVE_CANDLE_TIMEFRAMES[0],
    ACTIVE_CANDLE_TIMEFRAMES[1],
    ACTIVE_CANDLE_TIMEFRAMES[2],
    ACTIVE_CANDLE_TIMEFRAMES[3],
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_frames_have_unique_labels_tables_and_durable_ids() {
        assert_eq!(ACTIVE_CANDLE_TIMEFRAMES.len(), 10);
        for (index, spec) in ACTIVE_CANDLE_TIMEFRAMES.iter().enumerate() {
            assert_eq!(spec.table_name, format!("candles_{}", spec.label));
            if index > 0 {
                assert!(ACTIVE_CANDLE_TIMEFRAMES[index - 1].seconds < spec.seconds);
            }
            for previous in &ACTIVE_CANDLE_TIMEFRAMES[..index] {
                assert_ne!(spec.label, previous.label);
                assert_ne!(spec.table_name, previous.table_name);
                assert_ne!(spec.storage_ordinal, previous.storage_ordinal);
            }
        }
        assert_eq!(ACTIVE_CANDLE_TIMEFRAMES[6].storage_ordinal, 24);
        assert_eq!(ACTIVE_CANDLE_TIMEFRAMES[6].seconds, 600);
    }

    #[test]
    fn top_volume_is_exactly_the_four_requested_short_candle_frames() {
        assert_eq!(
            TOP_VOLUME_CANDLE_TIMEFRAMES.map(|spec| spec.label),
            ["1s", "3s", "5s", "1m"]
        );
        assert_eq!(
            TOP_VOLUME_CANDLE_TIMEFRAMES.map(|spec| spec.seconds),
            [1, 3, 5, 60]
        );
        for spec in TOP_VOLUME_CANDLE_TIMEFRAMES {
            assert!(ACTIVE_CANDLE_TIMEFRAMES.contains(&spec));
        }
    }
}
