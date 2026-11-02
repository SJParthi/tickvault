//! Filtered, paged equity movers. Shared transport only; the authenticated API
//! owns eligibility and price calculations. Protocol v1 remains available.
use crate::operator_movers_wire::{
    BridgeError, MAX_SOURCE_ROWS, SELECTION_LIMIT, SourceStatus, Stock,
};
use serde::{Deserialize, Serialize};

pub const FILTERED_PROTOCOL_VERSION: u32 = 2;
pub const MAX_INDEX_NAMES: usize = 64;
pub const MAX_INDEX_NAME_BYTES: usize = 96;
pub const MAX_SEARCH_BYTES: usize = 32;
pub const MAX_FILTER_HEX_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    All,
    Gainers,
    Losers,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    pub fno_only: bool,
    pub index: Option<String>,
    pub search: String,
    pub direction: Direction,
    pub offset: usize,
}
impl Default for Filter {
    fn default() -> Self {
        Self {
            fno_only: true,
            index: None,
            search: String::new(),
            direction: Direction::All,
            offset: 0,
        }
    }
}
impl Filter {
    pub fn valid(&self) -> bool {
        self.index
            .as_ref()
            .is_none_or(|s| valid_label(s, MAX_INDEX_NAME_BYTES))
            && self.search.len() <= MAX_SEARCH_BYTES
            && !self.search.chars().any(char::is_control)
            && self.offset < MAX_SOURCE_ROWS
            && self.offset.is_multiple_of(SELECTION_LIMIT)
    }
    /// Only lowercase hex reaches the shell, never a raw index or search term.
    pub fn to_hex(&self) -> Option<String> {
        if !self.valid() {
            return None;
        }
        let bytes = serde_json::to_vec(self).ok()?;
        if bytes.len() > MAX_FILTER_HEX_BYTES / 2 {
            return None;
        }
        Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
    pub fn from_hex(value: &str) -> Option<Self> {
        if value.is_empty() || value.len() > MAX_FILTER_HEX_BYTES || !value.len().is_multiple_of(2)
        {
            return None;
        }
        let mut bytes = Vec::with_capacity(value.len() / 2);
        for pair in value.as_bytes().chunks_exact(2) {
            let digit = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                _ => None,
            };
            bytes.push(digit(pair[0])? * 16 + digit(pair[1])?);
        }
        let filter: Self = serde_json::from_slice(&bytes).ok()?;
        filter.valid().then_some(filter)
    }
    pub fn matches_price(&self, stock: &Stock) -> bool {
        stock
            .name
            .to_uppercase()
            .contains(&self.search.to_uppercase())
            && match self.direction {
                Direction::All => true,
                Direction::Gainers => stock.change_pct.is_some_and(|n| n > 0.0),
                Direction::Losers => stock.change_pct.is_some_and(|n| n < 0.0),
            }
    }
}
fn valid_label(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MembershipStatus {
    Available,
    Partial,
    Unavailable,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct MemberStock {
    #[serde(flatten)]
    pub price: Stock,
    /// Positions in index_names, never inferred from the trading symbol.
    pub indices: Vec<usize>,
    /// Unknown when today's canonical F&O artifact cannot be read.
    pub fno: Option<bool>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Snapshot {
    pub filter: Filter,
    pub status: SourceStatus,
    pub market_open: bool,
    pub as_of_ist: Option<String>,
    pub data_age_secs: Option<i64>,
    pub universe_total: usize,
    pub universe_available: bool,
    pub stocks_priced: usize,
    pub matching_total: usize,
    pub selection_limit: usize,
    pub window_secs: i64,
    pub value_basis: String,
    pub index_names: Vec<String>,
    pub membership_status: MembershipStatus,
    pub fno_list_available: bool,
    pub rows: Vec<MemberStock>,
}
impl Snapshot {
    pub fn valid(&self) -> bool {
        if !self.filter.valid()
            || self.selection_limit != SELECTION_LIMIT
            || self.universe_total > MAX_SOURCE_ROWS
            || (!self.universe_available && self.universe_total != 0)
            || self.stocks_priced > self.universe_total
            || self.matching_total > self.stocks_priced
            || self.rows.len()
                != self
                    .matching_total
                    .saturating_sub(self.filter.offset)
                    .min(SELECTION_LIMIT)
            || self.index_names.len() > MAX_INDEX_NAMES
            || !self
                .index_names
                .iter()
                .all(|s| valid_label(s, MAX_INDEX_NAME_BYTES))
            || !self.index_names.windows(2).all(|w| w[0] < w[1])
            || self.window_secs <= 0
            || self.window_secs > 86_400
            || self.value_basis.len() > 256
            || self.as_of_ist.as_ref().is_some_and(|s| !valid_label(s, 16))
            || self.data_age_secs.is_some_and(|s| s < 0)
            || (self.stocks_priced > 0
                && (self.as_of_ist.is_none() || self.data_age_secs.is_none()))
            || (self.membership_status == MembershipStatus::Unavailable
                && !self.index_names.is_empty())
        {
            return false;
        }
        let status_valid = match self.status {
            SourceStatus::Live => {
                self.market_open
                    && self.stocks_priced > 0
                    && self.data_age_secs.is_some_and(|n| (0..=60).contains(&n))
            }
            SourceStatus::Stale => self.market_open && self.stocks_priced > 0,
            SourceStatus::Closed => !self.market_open && self.stocks_priced == 0,
            _ => self.stocks_priced == 0,
        };
        let mut ids = std::collections::HashSet::with_capacity(self.rows.len());
        status_valid
            && self.rows.iter().all(|row| {
                row.price.security_id != 0
                    && row.price.valid()
                    && ids.insert(row.price.security_id)
                    && self.filter.matches_price(&row.price)
                    && (!self.filter.fno_only || row.fno == Some(true))
                    && (self.fno_list_available == row.fno.is_some())
                    && row.indices.len() <= self.index_names.len()
                    && row.indices.iter().all(|i| *i < self.index_names.len())
                    && row.indices.windows(2).all(|w| w[0] < w[1])
                    && self.filter.index.as_ref().is_none_or(|name| {
                        row.indices
                            .iter()
                            .any(|i| self.index_names.get(*i) == Some(name))
                    })
            })
            && self
                .rows
                .windows(2)
                .all(|w| match (w[0].price.change_pct, w[1].price.change_pct) {
                    (Some(a), Some(b)) => {
                        if self.filter.direction == Direction::Losers {
                            a <= b
                        } else {
                            a >= b
                        }
                    }
                    (Some(_), None) => true,
                    (None, None) => true,
                    (None, Some(_)) => false,
                })
    }
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
    pub valid_until_epoch_secs: i64,
    #[serde(flatten)]
    pub outcome: Outcome,
}
impl Reply {
    pub fn error(code: BridgeError, now: i64) -> Self {
        Self {
            version: FILTERED_PROTOCOL_VERSION,
            captured_at_epoch_secs: now,
            valid_until_epoch_secs: now.saturating_add(60),
            outcome: Outcome::Error { code },
        }
    }
    pub fn valid_at(&self, now: i64, filter: &Filter) -> bool {
        self.version == FILTERED_PROTOCOL_VERSION
            && filter.valid()
            && now < self.valid_until_epoch_secs
            && self.valid_until_epoch_secs > self.captured_at_epoch_secs
            && self.valid_until_epoch_secs <= self.captured_at_epoch_secs.saturating_add(60)
            && self.captured_at_epoch_secs <= now.saturating_add(5)
            && self.captured_at_epoch_secs >= now.saturating_sub(60)
            && match &self.outcome {
                Outcome::Snapshot { snapshot } => snapshot.filter == *filter && snapshot.valid(),
                Outcome::Error { .. } => true,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_round_trip_without_shell_metacharacters() {
        for search in ["", "M&M", "a'$(touch /tmp/no);\\\"", "<svg onload=x>"] {
            let f = Filter {
                search: search.into(),
                index: Some("Nifty 50".into()),
                ..Filter::default()
            };
            let hex = f.to_hex().unwrap();
            assert!(
                hex.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
            assert_eq!(Filter::from_hex(&hex), Some(f));
        }
    }
    #[test]
    fn invalid_filters_are_rejected_before_transport() {
        for value in ["", "0", "gg", "AA", "24;echo bad", "7b7d00"] {
            assert!(Filter::from_hex(value).is_none(), "{value}");
        }
        for offset in [1, 19, 1000, usize::MAX] {
            assert!(
                !Filter {
                    offset,
                    ..Filter::default()
                }
                .valid()
            );
        }
        assert!(
            !Filter {
                search: "x".repeat(MAX_SEARCH_BYTES + 1),
                ..Filter::default()
            }
            .valid()
        );
        assert!(
            !Filter {
                index: Some("\n".into()),
                ..Filter::default()
            }
            .valid()
        );
        assert!(serde_json::from_str::<Filter>(r#"{"command":"evil"}"#).is_err());
    }
    #[test]
    fn reply_requires_new_protocol_and_original_capture_clock() {
        let now = 1_800_000_000;
        let mut reply = Reply::error(BridgeError::Timeout, now);
        assert!(reply.valid_at(now + 59, &Filter::default()));
        assert!(!reply.valid_at(now + 60, &Filter::default()));
        assert!(!reply.valid_at(now + 61, &Filter::default()));
        reply.version = 1;
        assert!(!reply.valid_at(now, &Filter::default()));
    }
    fn snapshot() -> Snapshot {
        Snapshot {
            filter: Filter::default(),
            status: SourceStatus::Live,
            market_open: true,
            as_of_ist: Some("10:15:00".into()),
            data_age_secs: Some(2),
            universe_total: 2,
            universe_available: true,
            stocks_priced: 2,
            matching_total: 2,
            selection_limit: SELECTION_LIMIT,
            window_secs: 900,
            value_basis: "estimated turnover".into(),
            index_names: vec!["Nifty 50".into()],
            membership_status: MembershipStatus::Available,
            fno_list_available: true,
            rows: (1..=2)
                .map(|id| MemberStock {
                    price: Stock {
                        security_id: id,
                        segment: "NSE_EQ".into(),
                        name: format!("STOCK{id}"),
                        ltp: 100.0,
                        prev_close: Some(100.0),
                        change: Some(3.0 - id as f64),
                        change_pct: Some(3.0 - id as f64),
                        volume: Some(1),
                        value_cr: Some(0.1),
                        last_trade_ist: "10:15:00".into(),
                    },
                    indices: vec![0],
                    fno: Some(true),
                })
                .collect(),
        }
    }
    #[test]
    fn filtered_snapshot_refuses_false_counts_membership_eligibility_and_ordering() {
        let good = snapshot();
        assert!(good.valid());
        let mutations: [fn(&mut Snapshot); 12] = [
            |s| s.universe_total = 0,
            |s| s.universe_available = false,
            |s| s.matching_total = 3,
            |s| s.rows[0].indices = vec![1],
            |s| s.rows[0].indices = vec![0, 0],
            |s| s.rows[0].fno = None,
            |s| s.rows[0].fno = Some(false),
            |s| s.rows[0].price.security_id = 2,
            |s| s.rows.swap(0, 1),
            |s| s.filter.index = Some("Invented index".into()),
            |s| s.status = SourceStatus::Closed,
            |s| s.data_age_secs = Some(61),
        ];
        for change in mutations {
            let mut bad = good.clone();
            change(&mut bad);
            assert!(!bad.valid(), "{bad:?}");
        }
        let mut missing = good.clone();
        missing.rows.pop();
        assert!(!missing.valid());
        let now = 1_800_000_000;
        let reply = Reply {
            version: FILTERED_PROTOCOL_VERSION,
            captured_at_epoch_secs: now,
            valid_until_epoch_secs: now + 60,
            outcome: Outcome::Snapshot { snapshot: good },
        };
        assert!(reply.valid_at(now, &Filter::default()));
        assert!(!reply.valid_at(
            now,
            &Filter {
                fno_only: false,
                ..Filter::default()
            }
        ));
    }
}
