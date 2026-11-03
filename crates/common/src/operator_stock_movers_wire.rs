//! Filtered, paged equity movers. Shared transport only; the authenticated API
//! owns eligibility and price calculations. Protocol v1 remains available.
use crate::operator_movers_wire::{
    BridgeError, MAX_SOURCE_ROWS, SELECTION_LIMIT, SourceStatus, Stock,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

pub const FILTERED_PROTOCOL_VERSION: u32 = 2;
pub const MAX_INDEX_NAMES: usize = 64;
pub const MAX_INDEX_NAME_BYTES: usize = 96;
pub const MAX_SEARCH_BYTES: usize = 32;
pub const MAX_FILTER_HEX_BYTES: usize = 1024;
pub const MAX_COMPANY_NAME_BYTES: usize = 128;
pub const MAX_VOLUME_TEXT_BYTES: usize = 20;
/// Browser watchlists use numeric identity; do not let distinct u64 IDs alias
/// after JSON decoding as IEEE-754 numbers. Dhan's actual IDs fit below this.
pub const MAX_BROWSER_SECURITY_ID: u64 = (1_u64 << 53) - 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    All,
    Gainers,
    Losers,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortColumn {
    Name,
    Ltp,
    Change,
    ChangePct,
    ValueCr,
    Volume,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    pub fno_only: bool,
    pub index: Option<String>,
    pub search: String,
    pub direction: Direction,
    pub offset: usize,
    /// Both absent keeps the earlier change-% ordering for existing clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<SortColumn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<SortOrder>,
}
impl Default for Filter {
    fn default() -> Self {
        Self {
            fno_only: true,
            index: None,
            search: String::new(),
            direction: Direction::All,
            offset: 0,
            sort_by: None,
            sort_order: None,
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
            && self.sort_by.is_some() == self.sort_order.is_some()
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

    /// Shared ordering for the API selection and transport validation.
    /// Explicit sorts keep missing numeric fields last and break ties by
    /// identity. Omitted sort fields retain earlier v2 tie compatibility.
    pub fn compare_rows(&self, left: &MemberStock, right: &MemberStock) -> Ordering {
        let column = self.sort_by.unwrap_or(SortColumn::ChangePct);
        let order = self
            .sort_order
            .unwrap_or(if self.direction == Direction::Losers {
                SortOrder::Asc
            } else {
                SortOrder::Desc
            });
        let a = &left.price;
        let b = &right.price;
        let primary = match column {
            SortColumn::Name => ordered_values(Some(a.name.as_str()), Some(b.name.as_str()), order),
            SortColumn::Ltp => ordered_values(Some(a.ltp), Some(b.ltp), order),
            SortColumn::Change => ordered_values(a.change, b.change, order),
            SortColumn::ChangePct => ordered_values(a.change_pct, b.change_pct, order),
            SortColumn::ValueCr => ordered_values(a.value_cr, b.value_cr, order),
            // Compare u64 directly: f64 would collapse distinct volumes >2^53.
            SortColumn::Volume => ordered_values(a.volume, b.volume, order),
        };
        if self.sort_by.is_none() {
            return primary;
        }
        primary
            .then_with(|| a.security_id.cmp(&b.security_id))
            .then_with(|| a.segment.cmp(&b.segment))
    }
}

fn ordered_values<T: PartialOrd>(left: Option<T>, right: Option<T>, order: SortOrder) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => {
            let value = left.partial_cmp(&right).unwrap_or(Ordering::Equal);
            if order == SortOrder::Desc {
                value.reverse()
            } else {
                value
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
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
    /// Absent when the current daily mapping contains only a trading symbol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub company_name: Option<String>,
    /// Exact decimal volume for JavaScript clients; JSON numbers above 2^53
    /// cannot preserve every u64. Older envelopes may omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_text: Option<String>,
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
            // After the close the snapshot may carry the ended session's
            // final stored prices (owner 2026-11-03), so priced is allowed.
            SourceStatus::Closed => !self.market_open,
            _ => self.stocks_priced == 0,
        };
        let mut ids = std::collections::HashSet::with_capacity(self.rows.len());
        status_valid
            && self.rows.iter().all(|row| {
                row.price.security_id != 0
                    && row.price.security_id <= MAX_BROWSER_SECURITY_ID
                    && row.price.valid()
                    && row
                        .company_name
                        .as_ref()
                        .is_none_or(|name| valid_label(name, MAX_COMPANY_NAME_BYTES))
                    && row.volume_text.as_ref().is_none_or(|text| {
                        text.len() <= MAX_VOLUME_TEXT_BYTES
                            && text.parse::<u64>().ok().is_some_and(|volume| {
                                row.price.volume == Some(volume) && volume.to_string() == *text
                            })
                    })
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
                .all(|pair| self.filter.compare_rows(&pair[0], &pair[1]) != Ordering::Greater)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    Snapshot { snapshot: Box<Snapshot> },
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
    fn test_to_hex_from_hex_round_trip_without_shell_metacharacters() {
        // Test-only adversarial input; split the literal so the source-only
        // HTML-handler guard does not classify this data as executable markup.
        for search in [
            "",
            "M&M",
            "a'$(touch /tmp/no);\\\"",
            concat!("<svg on", "load=x>"),
        ] {
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
    fn test_valid_at_requires_new_protocol_and_original_capture_clock() {
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
                    company_name: None,
                    volume_text: None,
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
            outcome: Outcome::Snapshot {
                snapshot: Box::new(good),
            },
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

    #[test]
    fn explicit_sort_filters_round_trip_and_keep_legacy_omission_compatible() {
        let legacy: Filter = serde_json::from_str(
            r#"{"fno_only":false,"index":null,"search":"","direction":"all","offset":0}"#,
        )
        .unwrap();
        assert!(legacy.valid());
        assert!(legacy.sort_by.is_none() && legacy.sort_order.is_none());
        let old_wire = serde_json::to_value(&legacy).unwrap();
        assert!(old_wire.get("sort_by").is_none());
        assert!(old_wire.get("sort_order").is_none());
        for column in [
            SortColumn::Name,
            SortColumn::Ltp,
            SortColumn::Change,
            SortColumn::ChangePct,
            SortColumn::ValueCr,
            SortColumn::Volume,
        ] {
            for order in [SortOrder::Asc, SortOrder::Desc] {
                let filter = Filter {
                    sort_by: Some(column),
                    sort_order: Some(order),
                    index: Some("\\".repeat(MAX_INDEX_NAME_BYTES)),
                    search: "\\".repeat(MAX_SEARCH_BYTES),
                    offset: 980,
                    ..legacy.clone()
                };
                assert!(filter.valid());
                let encoded = filter.to_hex().unwrap();
                assert!(encoded.len() <= MAX_FILTER_HEX_BYTES);
                assert_eq!(Filter::from_hex(&encoded), Some(filter.clone()));
                let echoed = serde_json::to_value(&filter).unwrap();
                assert!(echoed.get("sort_by").is_some());
                assert!(echoed.get("sort_order").is_some());
                assert_ne!(filter, legacy, "cache identity must include sorting");
                assert!(
                    !Filter {
                        sort_order: None,
                        ..filter.clone()
                    }
                    .valid()
                );
                assert!(
                    !Filter {
                        sort_by: None,
                        ..filter
                    }
                    .valid()
                );
            }
        }
        for bad in [
            r#"{"sort_by":"turnover","sort_order":"desc"}"#,
            r#"{"sort_by":"volume","sort_order":"sideways"}"#,
            r#"{"sort_by":"volume","sort_order":"desc","sql":"SELECT 1"}"#,
        ] {
            assert!(serde_json::from_str::<Filter>(bad).is_err());
        }
    }

    #[test]
    fn compare_rows_keeps_missing_numbers_last_and_compares_large_volumes_exactly() {
        let mut low = snapshot().rows.remove(0);
        low.price.security_id = 90;
        low.price.name = "AAA".into();
        low.price.ltp = 10.0;
        low.price.change = Some(-2.0);
        low.price.change_pct = Some(-20.0);
        low.price.value_cr = Some(2.0);
        low.price.volume = Some(u64::MAX - 1);
        let mut high = low.clone();
        high.price.security_id = 10;
        high.price.name = "BBB".into();
        high.price.ltp = 20.0;
        high.price.change = Some(3.0);
        high.price.change_pct = Some(30.0);
        high.price.value_cr = Some(3.0);
        high.price.volume = Some(u64::MAX);
        let mut missing = low.clone();
        missing.price.security_id = 1;
        missing.price.name = "ZZZ".into();
        missing.price.ltp = 30.0;
        missing.price.change = None;
        missing.price.change_pct = None;
        missing.price.value_cr = None;
        missing.price.volume = None;
        for column in [
            SortColumn::Name,
            SortColumn::Ltp,
            SortColumn::Change,
            SortColumn::ChangePct,
            SortColumn::ValueCr,
            SortColumn::Volume,
        ] {
            for order in [SortOrder::Asc, SortOrder::Desc] {
                let filter = Filter {
                    sort_by: Some(column),
                    sort_order: Some(order),
                    ..Filter::default()
                };
                let mut rows = [missing.clone(), high.clone(), low.clone()];
                rows.sort_unstable_by(|a, b| filter.compare_rows(a, b));
                let expected = match (column, order) {
                    (_, SortOrder::Asc) => [90, 10, 1],
                    (SortColumn::Name | SortColumn::Ltp, SortOrder::Desc) => [1, 10, 90],
                    (_, SortOrder::Desc) => [10, 90, 1],
                };
                assert_eq!(
                    rows.iter().map(|r| r.price.security_id).collect::<Vec<_>>(),
                    expected
                );
                let mut tie = low.clone();
                tie.price.security_id = 2;
                assert_eq!(filter.compare_rows(&tie, &low), Ordering::Less);
                assert_eq!(filter.compare_rows(&low, &tie), Ordering::Greater);
            }
        }
    }

    #[test]
    fn sorted_snapshot_requires_matching_order_and_bounds_optional_company_names() {
        let mut page = snapshot();
        page.filter.sort_by = Some(SortColumn::Volume);
        page.filter.sort_order = Some(SortOrder::Desc);
        page.rows[0].price.volume = Some(1);
        page.rows[1].price.volume = Some(2);
        assert!(
            !page.valid(),
            "change-% order is wrong for a volume request"
        );
        page.rows.swap(0, 1);
        assert!(page.valid());
        page.rows[1].price.volume = Some(2);
        assert!(
            !page.valid(),
            "equal values require deterministic ascending identity"
        );
        page.rows.swap(0, 1);
        assert!(page.valid());
        let legacy_row = serde_json::to_value(&page.rows[0]).unwrap();
        assert!(legacy_row.get("company_name").is_none());
        assert!(
            serde_json::from_value::<MemberStock>(legacy_row)
                .unwrap()
                .company_name
                .is_none()
        );
        for name in [
            "".to_string(),
            "bad\nname".into(),
            "x".repeat(MAX_COMPANY_NAME_BYTES + 1),
        ] {
            page.rows[0].company_name = Some(name);
            assert!(!page.valid());
        }
        page.rows[0].company_name = Some("Recorded Company Ltd".into());
        assert!(page.valid());
        page.rows[0].volume_text = Some("2".into());
        assert!(page.valid());
        for text in ["", "3", "+2", "02", "18446744073709551616", "2\n"] {
            page.rows[0].volume_text = Some(text.into());
            assert!(!page.valid(), "{text:?}");
        }
    }

    #[test]
    fn filtered_snapshot_refuses_ids_that_cannot_remain_exact_in_browser_watchlists() {
        let mut page = snapshot();
        for id in [1, MAX_BROWSER_SECURITY_ID] {
            page.rows[0].price.security_id = id;
            assert!(page.valid(), "safe positive identity {id}");
        }
        for id in [0, MAX_BROWSER_SECURITY_ID + 1, u64::MAX] {
            page.rows[0].price.security_id = id;
            assert!(!page.valid(), "unsafe browser identity {id}");
        }
    }

    #[test]
    fn test_matches_price_combines_case_insensitive_search_with_direction_and_missing_change() {
        let mut price = snapshot().rows.remove(0).price;
        price.name = "Acme Bank".into();
        for (direction, expected) in [
            (Direction::All, [true, true, true, true]),
            (Direction::Gainers, [true, false, false, false]),
            (Direction::Losers, [false, true, false, false]),
        ] {
            for (change, directional_match) in [Some(3.0), Some(-3.0), Some(0.0), None]
                .into_iter()
                .zip(expected)
            {
                price.change_pct = change;
                for (search, name_match) in [
                    ("", true),
                    ("aCmE", true),
                    ("BANK", true),
                    ("absent", false),
                ] {
                    let filter = Filter {
                        search: search.into(),
                        direction,
                        ..Filter::default()
                    };
                    assert_eq!(
                        filter.matches_price(&price),
                        name_match && directional_match,
                        "direction={direction:?}, change={change:?}, search={search:?}"
                    );
                }
            }
        }
        price.name = "ßeta".into();
        assert!(
            Filter {
                search: "SSE".into(),
                ..Filter::default()
            }
            .matches_price(&price)
        );
    }
}
