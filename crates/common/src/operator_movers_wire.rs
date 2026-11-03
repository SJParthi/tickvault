//! Bounded read-only movers transport shared by the app helper and operator Lambda.
//! The Lambda includes this dependency-light source directly to avoid linking common.
//! These are transport definitions, not a second price/F&O calculation engine.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_WIRE_BYTES: usize = 20_000;
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_SOURCE_ROWS: usize = 1_000;
pub const SELECTION_LIMIT: usize = 20;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Stock {
    pub security_id: u64,
    pub segment: String,
    pub name: String,
    pub ltp: f64,
    pub prev_close: Option<f64>,
    pub change: Option<f64>,
    pub change_pct: Option<f64>,
    pub volume: Option<u64>,
    pub value_cr: Option<f64>,
    pub last_trade_ist: String,
}

impl Stock {
    pub fn valid(&self) -> bool {
        self.segment == "NSE_EQ"
            && !self.name.is_empty()
            && self.name.len() <= 128
            && !self.name.chars().any(char::is_control)
            && self.last_trade_ist.len() <= 16
            && self.ltp.is_finite()
            && self.ltp > 0.0
            && [self.prev_close, self.change, self.change_pct, self.value_cr]
                .into_iter()
                .flatten()
                .all(f64::is_finite)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    Live,
    Stale,
    Closed,
    NoFnoList,
    NoIndexList,
    NoData,
    Unreachable,
    QueryFailed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Snapshot {
    pub status: SourceStatus,
    pub market_open: bool,
    pub as_of_ist: Option<String>,
    pub data_age_secs: Option<i64>,
    pub fno_stocks_total: usize,
    pub stocks_priced: usize,
    pub window_secs: i64,
    pub value_basis: String,
    pub selection_limit: usize,
    pub gainers_total: usize,
    pub losers_total: usize,
    pub gainers: Vec<Stock>,
    pub losers: Vec<Stock>,
}

impl Snapshot {
    /// Validate the complete envelope; malformed/truncated selections are never shown.
    pub fn valid(&self) -> bool {
        let counts_valid = self.selection_limit == SELECTION_LIMIT
            && self.stocks_priced <= MAX_SOURCE_ROWS
            && self.stocks_priced <= self.fno_stocks_total
            && self.fno_stocks_total <= 10_000
            && self.gainers_total <= self.stocks_priced
            && self.losers_total <= self.stocks_priced.saturating_sub(self.gainers_total)
            && self.gainers.len() == self.gainers_total.min(SELECTION_LIMIT)
            && self.losers.len() == self.losers_total.min(SELECTION_LIMIT);
        let rows_valid = self.gainers.iter().chain(&self.losers).all(Stock::valid)
            && ordered(&self.gainers, true)
            && ordered(&self.losers, false);
        let unique = {
            let mut ids = std::collections::HashSet::new();
            self.gainers
                .iter()
                .chain(&self.losers)
                .all(|s| ids.insert(s.security_id))
        };
        let timestamps_valid = self.as_of_ist.as_ref().is_none_or(|s| s.len() <= 16)
            && self.data_age_secs.is_none_or(|n| n >= 0)
            && (self.stocks_priced == 0
                || (self.as_of_ist.is_some() && self.data_age_secs.is_some()));
        let status_valid = match self.status {
            SourceStatus::Live => {
                self.market_open
                    && self.stocks_priced > 0
                    && self.data_age_secs.is_some_and(|n| (0..=60).contains(&n))
            }
            SourceStatus::Stale => self.market_open && self.stocks_priced > 0,
            // After the close the snapshot may carry the ended session's
            // final stored prices (owner 2026-11-03), so priced is allowed.
            SourceStatus::Closed => !self.market_open,
            _ => self.stocks_priced == 0,
        };
        counts_valid
            && rows_valid
            && unique
            && timestamps_valid
            && status_valid
            && self.window_secs > 0
            && self.window_secs <= 86_400
            && self.value_basis.len() <= 256
    }
}

fn ordered(rows: &[Stock], positive: bool) -> bool {
    rows.iter().all(|s| {
        s.change_pct
            .is_some_and(|p| if positive { p > 0.0 } else { p < 0.0 })
    }) && rows
        .windows(2)
        .all(|pair| match (pair[0].change_pct, pair[1].change_pct) {
            (Some(a), Some(b)) => {
                if positive {
                    a >= b
                } else {
                    a <= b
                }
            }
            _ => false,
        })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeError {
    InstanceStopped,
    InstanceUnavailable,
    BridgeUnavailable,
    CredentialsUnavailable,
    AppUnauthorized,
    AppUnavailable,
    Timeout,
    InvalidResponse,
    ResponseTooLarge,
    Busy,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    Snapshot { snapshot: Snapshot },
    Error { code: BridgeError },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Reply {
    pub version: u32,
    pub captured_at_epoch_secs: i64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl Reply {
    pub fn error(code: BridgeError, now: i64) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            captured_at_epoch_secs: now,
            outcome: Outcome::Error { code },
        }
    }

    pub fn valid_at(&self, now: i64) -> bool {
        self.version == PROTOCOL_VERSION
            && self.captured_at_epoch_secs <= now.saturating_add(5)
            && self.captured_at_epoch_secs >= now.saturating_sub(60)
            && match &self.outcome {
                Outcome::Snapshot { snapshot } => snapshot.valid(),
                Outcome::Error { .. } => true,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_at_requires_known_protocol_and_bounded_capture_clock() {
        let now = 1_800_000_000;
        for (age, expected) in [(-6, false), (-5, true), (0, true), (60, true), (61, false)] {
            let reply = Reply::error(BridgeError::AppUnavailable, now - age);
            assert_eq!(reply.valid_at(now), expected, "capture age {age}");
        }
        for invalid in [i64::MIN, i64::MAX] {
            assert!(!Reply::error(BridgeError::Timeout, invalid).valid_at(now));
        }
        for version in [0, 2, u32::MAX] {
            let mut reply = Reply::error(BridgeError::Timeout, now);
            reply.version = version;
            assert!(!reply.valid_at(now));
        }
    }
}
