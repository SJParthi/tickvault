//! Boot-only, conflict-preserving recovery of candle rows.
//!
//! `bucket_revision` starts again when an instrument slot is constructed; it
//! is not a cross-process order. Recovery therefore inserts missing keys,
//! accepts identical rows (including the explicitly unknown legacy extension),
//! and refuses every differing original payload.
//! The application must exclude all managed candle producers until this pass
//! ends. Independent writers to these tables are unsupported: the SQL reads
//! and ILP write are not an atomic compare-and-insert transaction.

use anyhow::{Context, Result};
use serde_json::Value;
use tickvault_trading::candles::BufferedSeal;

use crate::shadow_candle_writer::ShadowCandleWriter;
use crate::shadow_seal_columns::ShadowSealRow;

// Preserve the typed refusal when a guard fails; anyhow's ensure! macro
// returns its concrete Error directly rather than converting our error type.
macro_rules! ensure {
    ($condition:expr, $($message:tt)*) => {
        if !$condition {
            return Err(anyhow::anyhow!($($message)*).into());
        }
    };
}

/// A hard memory/query bound independent of the spill file or live drain cap.
pub const MAX_RECOVERY_BATCH: usize = 64;
const QUERY_TIMEOUT_SECS: u64 = 5;
/// One entire WAL+SELECT phase, across all selected tables. The separate ILP
/// transport retains its existing bounded request/reconnect policy.
const APPLIED_PHASE_TIMEOUT_SECS: u64 = 10;
const WAL_ATTEMPTS: usize = 8;
const WAL_RECHECK_MILLIS: u64 = 100;
const MAX_QUERY_BYTES: usize = 24 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// A confirmed batch includes newly inserted and unchanged existing rows.
/// Existing rows include the explicit 18-column legacy duplicate exception;
/// those retain NULL provenance and are never promoted to the current metric.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveredBatch {
    pub inserted: usize,
    pub identical: usize,
}

#[derive(Debug)]
pub enum RecoveryRefusal {
    /// A matching composite key exists with any different persisted value.
    Conflict,
    /// Managed-writer exclusion was not established for this invocation.
    OrderingUnverified,
    /// Query, WAL, write, schema or post-write verification did not complete.
    Unconfirmed(anyhow::Error),
}

impl From<anyhow::Error> for RecoveryRefusal {
    fn from(error: anyhow::Error) -> Self {
        Self::Unconfirmed(error)
    }
}

const COLUMNS: [&str; 25] = [
    "ts_micros",
    "security_id",
    "segment",
    "feed",
    "open",
    "high",
    "low",
    "close",
    "volume",
    "oi",
    "tick_count",
    "close_pct_from_prev_day",
    "open_pct",
    "change_pct",
    "open_gap_pct",
    "net_volume",
    "volume_basis",
    "total_buy_qty",
    "total_sell_qty",
    "lot_size",
    "instrument_definition_version",
    "underlying_id",
    "ranking_family",
    "volume_quality",
    "bucket_revision",
];

#[derive(Clone, Copy, Debug)]
enum Cell {
    Integer(Option<i64>),
    Float(f64),
    Text(Option<&'static str>),
}

impl Cell {
    fn matches(self, actual: &Value) -> bool {
        match self {
            Self::Integer(Some(value)) => actual.as_i64() == Some(value),
            Self::Integer(None) | Self::Text(None) => actual.is_null(),
            Self::Text(Some(value)) => actual.as_str() == Some(value),
            Self::Float(value) if value.is_finite() => actual.as_f64() == Some(value),
            // QuestDB's DOUBLE null is represented by NaN; the ILP writer
            // and JSON reader normalize nonfinite values to SQL NULL.
            Self::Float(_) => actual.is_null(),
        }
    }

    fn same(self, other: Self) -> bool {
        match (self, other) {
            (Self::Integer(left), Self::Integer(right)) => left == right,
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => {
                left == right || (!left.is_finite() && !right.is_finite())
            }
            _ => false,
        }
    }
}

fn cells(row: &ShadowSealRow) -> [Cell; 25] {
    use Cell::{Float as F, Integer as I, Text as T};
    [
        I(Some(row.timestamp_ist_nanos / 1_000)),
        I(Some(row.security_id)),
        T(Some(row.segment)),
        T(Some(row.feed)),
        F(row.open),
        F(row.high),
        F(row.low),
        F(row.close),
        I(Some(row.volume)),
        I(Some(row.oi)),
        I(Some(row.tick_count)),
        F(row.close_pct_from_prev_day),
        F(row.open_pct),
        F(row.change_pct),
        F(row.open_gap_pct),
        I(row.net_volume),
        T(Some(row.volume_basis)),
        I(Some(row.total_buy_qty)),
        I(Some(row.total_sell_qty)),
        I(row.lot_size),
        I(row.instrument_definition_version),
        I(row.underlying_id),
        T(row.ranking_family),
        I(Some(row.volume_quality)),
        I(row.bucket_revision),
    ]
}

fn same_key(left: &ShadowSealRow, right: &ShadowSealRow) -> bool {
    left.table_name == right.table_name
        && left.timestamp_ist_nanos == right.timestamp_ist_nanos
        && left.security_id == right.security_id
        && left.segment == right.segment
        && left.feed == right.feed
}

fn same_row(left: &ShadowSealRow, right: &ShadowSealRow) -> bool {
    same_key(left, right)
        && cells(left)
            .into_iter()
            .zip(cells(right))
            .all(|(a, b)| a.same(b))
}

/// SQL identifiers come only from the fixed TfIndex table family; strings
/// come from row identity and are still escaped so this boundary is complete.
fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn rows_query(table: &str, rows: &[ShadowSealRow]) -> Result<String> {
    ensure!(
        tickvault_trading::candles::TfIndex::ALL
            .into_iter()
            .any(|tf| tf.table_name() == table),
        "unrecognized candle recovery table"
    );
    ensure!(
        !rows.is_empty() && rows.len() <= MAX_RECOVERY_BATCH,
        "invalid recovery query bound"
    );
    let selection = COLUMNS[1..].join(",");
    let predicates: Vec<_> = rows
        .iter()
        .filter(|row| row.table_name == table)
        .map(|row| {
            format!(
                "(ts=cast({} as timestamp) and security_id={} and segment={} and feed={})",
                row.timestamp_ist_nanos / 1_000,
                row.security_id,
                sql_string(row.segment),
                sql_string(row.feed),
            )
        })
        .collect();
    ensure!(!predicates.is_empty(), "empty recovery table selection");
    // The extra row detects a non-unique/misconfigured DEDUP result instead
    // of silently truncating it to the number we hoped to find.
    let query = format!(
        "select cast(ts as long) ts_micros,{selection} from {table} where {} limit {}",
        predicates.join(" or "),
        rows.len() + 1,
    );
    ensure!(
        query.len() <= MAX_QUERY_BYTES,
        "recovery query exceeds byte bound"
    );
    Ok(query)
}

/// An old 18-column row gains SQL NULLs in all seven additive columns. An
/// equally provenance-unknown legacy replay may be recognized without writing
/// those NULLs or relabeling it as the current metric, but only when every
/// original payload field agrees. Any known metadata/quality/revision/basis
/// on either side remains subject to normal full-row matching.
fn matches_legacy_null_extension(
    expected: &ShadowSealRow,
    record: &[Value],
    indices: &[usize; 25],
) -> bool {
    use tickvault_trading::candles::volume_update::{
        VOLUME_QUALITY_LEGACY_BASIS, VOLUME_QUALITY_UNCLASSIFIED_NET,
        VOLUME_QUALITY_UNKNOWN_METADATA,
    };
    const ADDITIVE: [usize; 7] = [16, 19, 20, 21, 22, 23, 24];
    let synthesized_quality = VOLUME_QUALITY_LEGACY_BASIS
        | VOLUME_QUALITY_UNKNOWN_METADATA
        | if expected.net_volume.is_none() {
            VOLUME_QUALITY_UNCLASSIFIED_NET
        } else {
            0
        };
    if expected.volume_basis != crate::shadow_seal_columns::LEGACY_VOLUME_BASIS
        || expected.volume_quality != i64::from(synthesized_quality)
        || expected.lot_size.is_some()
        || expected.instrument_definition_version.is_some()
        || expected.underlying_id.is_some()
        || expected.ranking_family.is_some()
        || expected.bucket_revision.is_some()
        || ADDITIVE
            .iter()
            .any(|index| !record[indices[*index]].is_null())
    {
        return false;
    }
    cells(expected)
        .into_iter()
        .enumerate()
        .all(|(index, cell)| ADDITIVE.contains(&index) || cell.matches(&record[indices[index]]))
}

/// Resolve by column name, rejecting omitted/duplicate headers and malformed
/// rows. Known provenance must match; the narrow 18-column legacy exception
/// below only recognizes unchanged unknown provenance and never writes it.
fn classify_rows(body: &Value, expected: &[ShadowSealRow]) -> Result<Vec<bool>, RecoveryRefusal> {
    let columns = body
        .get("columns")
        .and_then(Value::as_array)
        .context("recovery query has no columns")?;
    let dataset = body
        .get("dataset")
        .and_then(Value::as_array)
        .context("recovery query has no dataset")?;
    let mut indices = [0usize; 25];
    for (slot, name) in indices.iter_mut().zip(COLUMNS) {
        let matches: Vec<_> = columns
            .iter()
            .enumerate()
            .filter_map(|(index, column)| {
                (column.get("name").and_then(Value::as_str) == Some(name)).then_some(index)
            })
            .collect();
        ensure!(
            matches.len() == 1,
            "missing or duplicate recovery column {name}"
        );
        *slot = matches[0];
    }
    ensure!(
        dataset.len() <= expected.len(),
        "duplicate or unexpected recovery rows"
    );
    let mut present = vec![false; expected.len()];
    for record in dataset {
        let record = record.as_array().context("malformed recovery row")?;
        ensure!(record.len() == columns.len(), "incomplete recovery row");
        let found = expected.iter().enumerate().find(|(_, row)| {
            cells(row)[..4]
                .iter()
                .zip(&indices[..4])
                .all(|(cell, index)| cell.matches(&record[*index]))
        });
        let Some((index, row)) = found else {
            return Err(RecoveryRefusal::Unconfirmed(anyhow::anyhow!(
                "unexpected recovery composite key"
            )));
        };
        ensure!(!present[index], "duplicate recovery composite key");
        if !cells(row)
            .into_iter()
            .zip(indices)
            .all(|(cell, index)| cell.matches(&record[index]))
            && !matches_legacy_null_extension(row, record, &indices)
        {
            return Err(RecoveryRefusal::Conflict);
        }
        present[index] = true;
    }
    Ok(present)
}

/// A caught-up, nonsuspended table is mandatory before reading its data.
/// Missing optional fields in the general watcher are mandatory here.
fn wal_applied(body: &Value, tables: &[&str]) -> Result<bool> {
    let (rows, skipped) = crate::wal_suspension_watcher::parse_wal_tables_response(body)
        .map_err(|_| anyhow::anyhow!("invalid recovery WAL response"))?;
    ensure!(skipped == 0, "incomplete recovery WAL response");
    let mut caught_up = true;
    for table in tables {
        let matches: Vec<_> = rows.iter().filter(|row| row.name == *table).collect();
        ensure!(
            matches.len() == 1,
            "missing or duplicate recovery WAL table"
        );
        let row = matches[0];
        ensure!(!row.suspended, "recovery WAL table is suspended");
        let writer = row
            .writer_txn
            .context("missing recovery writer transaction")?;
        let sequencer = row
            .sequencer_txn
            .context("missing recovery sequencer transaction")?;
        ensure!(
            writer >= 0 && sequencer >= writer,
            "invalid recovery transaction order"
        );
        caught_up &= writer == sequencer;
    }
    Ok(caught_up)
}

trait RecoveryStore {
    async fn applied_rows(&mut self, rows: &[ShadowSealRow]) -> Result<Vec<bool>, RecoveryRefusal>;
    fn insert_missing(&mut self, rows: &[ShadowSealRow]) -> Result<(), RecoveryRefusal>;
}

/// Shared policy used by production and failure-injecting tests. In-batch
/// duplicates are reconciled before any I/O. No revision comparison exists.
async fn reconcile<S: RecoveryStore>(
    store: &mut S,
    input: &[ShadowSealRow],
) -> Result<RecoveredBatch, RecoveryRefusal> {
    ensure!(
        !input.is_empty() && input.len() <= MAX_RECOVERY_BATCH,
        "invalid recovery batch bound"
    );
    let mut rows: Vec<ShadowSealRow> = Vec::with_capacity(input.len());
    for row in input {
        if let Some(prior) = rows.iter().find(|prior| same_key(prior, row)) {
            if !same_row(prior, row) {
                return Err(RecoveryRefusal::Conflict);
            }
        } else {
            rows.push(*row);
        }
    }
    let present = store.applied_rows(&rows).await?;
    ensure!(
        present.len() == rows.len(),
        "incomplete recovery admission result"
    );
    let missing: Vec<_> = rows
        .iter()
        .zip(&present)
        .filter_map(|(row, present)| (!*present).then_some(*row))
        .collect();
    if !missing.is_empty() {
        store.insert_missing(&missing)?;
        let confirmed = store.applied_rows(&rows).await?;
        ensure!(
            confirmed.len() == rows.len() && confirmed.iter().all(|value| *value),
            "recovery rows not confirmed after flush"
        );
    }
    Ok(RecoveredBatch {
        inserted: missing.len(),
        identical: input.len() - missing.len(),
    })
}

struct QuestDbRecovery<'a> {
    writer: &'a mut ShadowCandleWriter,
    client: reqwest::Client,
    exec_url: &'a str,
}

impl QuestDbRecovery<'_> {
    async fn query(&self, query: &str) -> Result<Value> {
        let mut response = self
            .client
            .get(self.exec_url)
            .query(&[("query", query)])
            .send()
            .await
            .context("recovery SQL request failed")?
            .error_for_status()
            .context("recovery SQL status rejected")?;
        if let Some(length) = response.content_length() {
            ensure!(
                length <= MAX_RESPONSE_BYTES as u64,
                "recovery response too large"
            );
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("recovery response read failed")?
        {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
                "recovery response exceeds bound"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("invalid recovery SQL JSON")
    }

    async fn wait_applied(&self, tables: &[&str]) -> Result<()> {
        let query = format!(
            "select name,suspended,writerTxn,sequencerTxn from wal_tables() where name in ({})",
            tables
                .iter()
                .map(|table| sql_string(table))
                .collect::<Vec<_>>()
                .join(","),
        );
        for attempt in 0..WAL_ATTEMPTS {
            if wal_applied(&self.query(&query).await?, tables)? {
                return Ok(());
            }
            if attempt + 1 < WAL_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_millis(WAL_RECHECK_MILLIS)).await;
            }
        }
        anyhow::bail!("recovery WAL apply barrier did not complete within bounded attempts")
    }
}

impl RecoveryStore for QuestDbRecovery<'_> {
    async fn applied_rows(&mut self, rows: &[ShadowSealRow]) -> Result<Vec<bool>, RecoveryRefusal> {
        tokio::time::timeout(
            std::time::Duration::from_secs(APPLIED_PHASE_TIMEOUT_SECS),
            async {
                let mut tables: Vec<&str> = rows.iter().map(|row| row.table_name).collect();
                tables.sort_unstable();
                tables.dedup();
                self.wait_applied(&tables).await?;
                let mut present = vec![false; rows.len()];
                for table in tables {
                    let indices: Vec<_> = rows
                        .iter()
                        .enumerate()
                        .filter_map(|(index, row)| (row.table_name == table).then_some(index))
                        .collect();
                    let selected: Vec<_> = indices.iter().map(|index| rows[*index]).collect();
                    let body = self.query(&rows_query(table, &selected)?).await?;
                    for (index, exists) in indices.into_iter().zip(classify_rows(&body, &selected)?)
                    {
                        present[index] = exists;
                    }
                }
                Ok::<_, RecoveryRefusal>(present)
            },
        )
        .await
        .map_err(|_| {
            RecoveryRefusal::Unconfirmed(anyhow::anyhow!(
                "recovery WAL/query phase exceeded deadline"
            ))
        })?
    }

    fn insert_missing(&mut self, rows: &[ShadowSealRow]) -> Result<(), RecoveryRefusal> {
        ensure!(
            self.writer.pending_count() == 0,
            "recovery sink has foreign pending rows"
        );
        for row in rows {
            self.writer.append_row(row)?;
        }
        // append_row's session gate may return Ok without writing. That is a
        // refusal, never confirmation that a recovered candle was persisted.
        ensure!(
            self.writer.pending_count() == rows.len(),
            "recovered row refused by persistence gate"
        );
        self.writer.flush()?;
        Ok(())
    }
}

/// Called only with the runner's boot exclusion held. The caller keeps the
/// synchronous ILP work off Tokio's asynchronous worker thread.
pub(crate) async fn reconcile_questdb_batch(
    writer: &mut ShadowCandleWriter,
    exec_url: &str,
    seals: &[BufferedSeal],
) -> Result<RecoveredBatch, RecoveryRefusal> {
    ensure!(
        !seals.is_empty() && seals.len() <= MAX_RECOVERY_BATCH,
        "invalid recovery batch bound"
    );
    ensure!(
        seals.iter().all(|seal| seal.security_id <= i64::MAX as u64),
        "recovery composite identity cannot be represented without saturation"
    );
    ensure!(
        seals
            .iter()
            .all(
                |seal| tickvault_common::segment::segment_code_to_str(seal.exchange_segment_code)
                    != "UNKNOWN"
            ),
        "unknown recovery exchange segment cannot establish a composite identity"
    );
    ensure!(
        writer.pending_count() == 0,
        "recovery started with a nonempty live buffer"
    );
    let client =
        crate::http_client::build_probe_client(QUERY_TIMEOUT_SECS).map_err(anyhow::Error::new)?;
    let rows: Vec<_> = seals
        .iter()
        .map(ShadowSealRow::from_buffered_seal)
        .collect();
    let mut store = QuestDbRecovery {
        writer,
        client,
        exec_url,
    };
    reconcile(&mut store, &rows).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::feed::Feed;
    use tickvault_trading::candles::{LiveCandleState, TfIndex};

    fn seal(id: u64, revision: u64) -> BufferedSeal {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_023_700;
        state.open = 99.0;
        state.high = 102.0;
        state.low = 98.0;
        state.close = 101.0;
        state.volume = 600;
        state.tick_count = 4;
        state.bucket_revision = revision;
        BufferedSeal {
            security_id: id,
            exchange_segment_code: 2,
            feed: Feed::Dhan,
            tf: TfIndex::S5,
            state,
        }
    }

    fn row(id: u64, revision: u64) -> ShadowSealRow {
        ShadowSealRow::from_buffered_seal(&seal(id, revision))
    }

    fn cell_json(cell: Cell) -> Value {
        match cell {
            Cell::Integer(value) => serde_json::json!(value),
            Cell::Text(value) => serde_json::json!(value),
            Cell::Float(value) => serde_json::json!(value),
        }
    }

    fn response(rows: &[ShadowSealRow]) -> Value {
        serde_json::json!({
            "columns": COLUMNS.iter().map(|name| serde_json::json!({"name":name})).collect::<Vec<_>>(),
            "dataset": rows.iter().map(|row| cells(row).into_iter().map(cell_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
        })
    }

    fn legacy_seal(source: &BufferedSeal, version: u8) -> BufferedSeal {
        let mut bytes = crate::seal_spill::SerializedSeal::from(source).to_bytes();
        bytes[7] = version;
        if version == 2 && !source.state.net_volume_classified {
            bytes[80..88].copy_from_slice(
                &crate::seal_spill::SEAL_SPILL_NET_VOLUME_UNCLASSIFIED.to_le_bytes(),
            );
        }
        crate::seal_spill::SerializedSeal::from_bytes(
            &bytes[..crate::seal_spill::SEAL_SPILL_LEGACY_RECORD_SIZE],
        )
        .expect("legacy fixture decoder")
        .try_into_buffered_seal()
        .expect("legacy active timeframe")
    }

    fn legacy_database_response(expected: &ShadowSealRow) -> Value {
        let mut body = response(&[*expected]);
        for index in [16, 19, 20, 21, 22, 23, 24] {
            body["dataset"][0][index] = Value::Null;
        }
        body
    }

    #[test]
    fn legacy_v1_v2_equal_eighteen_field_rows_can_skip_without_inventing_new_provenance() {
        for version in [1, 2] {
            for classified in [false, true] {
                let mut source = seal(39582, 9);
                source.state.net_volume_signed = -120;
                source.state.net_volume_classified = classified;
                let expected = ShadowSealRow::from_buffered_seal(&legacy_seal(&source, version));
                let body = legacy_database_response(&expected);
                assert_eq!(
                    classify_rows(&body, &[expected]).expect("same legacy payload"),
                    vec![true]
                );
                // Every one of the original 18 cells remains mandatory. No
                // clock, sign, price, counter or book difference is excused.
                for index in (0..25).filter(|index| ![16, 19, 20, 21, 22, 23, 24].contains(index)) {
                    let mut altered = body.clone();
                    altered["dataset"][0][index] = match &altered["dataset"][0][index] {
                        Value::String(_) => serde_json::json!("different"),
                        Value::Null => serde_json::json!(123),
                        _ => serde_json::json!(987654321),
                    };
                    assert!(
                        classify_rows(&altered, &[expected]).is_err(),
                        "legacy original column {} must match",
                        COLUMNS[index]
                    );
                }
                for index in [16, 19, 20, 21, 22, 23, 24] {
                    let mut populated = body.clone();
                    populated["dataset"][0][index] = serde_json::json!(1);
                    assert!(
                        matches!(
                            classify_rows(&populated, &[expected]),
                            Err(RecoveryRefusal::Conflict)
                        ),
                        "known extension {} must not be erased",
                        COLUMNS[index]
                    );
                }
            }
        }
    }

    #[test]
    fn legacy_equivalence_refuses_current_basis_known_metadata_and_non_synthetic_quality() {
        let mut source = seal(39582, 7);
        source.state.net_volume_classified = true;
        source.state.net_volume_signed = 120;
        let expected = ShadowSealRow::from_buffered_seal(&legacy_seal(&source, 2));
        let body = legacy_database_response(&expected);
        for changed in [
            ShadowSealRow {
                volume_basis: tickvault_trading::candles::CANDLE_SIGNED_BAR_VS_ONE_LOT_METRIC,
                ..expected
            },
            ShadowSealRow {
                lot_size: Some(600),
                ..expected
            },
            ShadowSealRow {
                instrument_definition_version: Some(4),
                ..expected
            },
            ShadowSealRow {
                underlying_id: Some(42),
                ..expected
            },
            ShadowSealRow {
                ranking_family: Some("stock"),
                ..expected
            },
            ShadowSealRow {
                bucket_revision: Some(1),
                ..expected
            },
            ShadowSealRow {
                volume_quality: expected.volume_quality
                    | i64::from(
                        tickvault_trading::candles::volume_update::VOLUME_QUALITY_COUNTER_AMBIGUOUS,
                    ),
                ..expected
            },
        ] {
            assert!(matches!(
                classify_rows(&body, &[changed]),
                Err(RecoveryRefusal::Conflict)
            ));
        }
    }

    fn wal(suspended: bool, writer: Value, sequencer: Value) -> Value {
        serde_json::json!({
            "columns": [
                {"name":"sequencerTxn"},{"name":"name"},
                {"name":"suspended"},{"name":"writerTxn"}
            ],
            "dataset": [[sequencer,"candles_5s",suspended,writer]],
        })
    }

    #[test]
    fn every_payload_field_is_compared_and_nullable_legacy_metadata_is_not_guessed() {
        let expected = row(39582, 2);
        let good = response(&[expected]);
        assert_eq!(
            classify_rows(&good, &[expected]).expect("exact match"),
            vec![true]
        );
        assert_eq!(
            classify_rows(&response(&[]), &[expected]).expect("absent"),
            vec![false]
        );
        for (index, name) in COLUMNS.iter().enumerate().skip(4) {
            let mut altered = good.clone();
            let cell = &mut altered["dataset"][0][index];
            *cell = match cell {
                Value::String(_) => Value::String("different".to_owned()),
                Value::Number(_) => serde_json::json!(987654321),
                Value::Null => serde_json::json!(1),
                _ => panic!("unexpected fixture value"),
            };
            assert!(
                matches!(
                    classify_rows(&altered, &[expected]),
                    Err(RecoveryRefusal::Conflict)
                ),
                "{name} must not be ignored"
            );
        }
        let mut missing = good.clone();
        missing["columns"].as_array_mut().expect("columns").pop();
        assert!(
            classify_rows(&missing, &[expected]).is_err(),
            "a legacy missing column is not a matching NULL cell"
        );
    }

    #[test]
    fn duplicated_database_keys_and_shuffled_or_missing_headers_are_not_silently_accepted() {
        let expected = row(39582, 2);
        assert!(classify_rows(&response(&[expected, expected]), &[expected]).is_err());
        let mut shuffled = response(&[expected]);
        shuffled["columns"]
            .as_array_mut()
            .expect("columns")
            .reverse();
        shuffled["dataset"][0]
            .as_array_mut()
            .expect("row")
            .reverse();
        assert_eq!(
            classify_rows(&shuffled, &[expected]).expect("column names resolve order"),
            vec![true]
        );
        shuffled["columns"][0] = shuffled["columns"][1].clone();
        assert!(classify_rows(&shuffled, &[expected]).is_err());
    }

    #[test]
    fn suspended_unapplied_missing_and_malformed_wal_cannot_certify_absence() {
        let tables = ["candles_5s"];
        assert!(
            wal_applied(
                &wal(false, serde_json::json!(5), serde_json::json!(5)),
                &tables
            )
            .expect("caught up")
        );
        assert!(
            !wal_applied(
                &wal(false, serde_json::json!(4), serde_json::json!(5)),
                &tables
            )
            .expect("unapplied")
        );
        assert!(
            wal_applied(
                &wal(true, serde_json::json!(5), serde_json::json!(5)),
                &tables
            )
            .is_err()
        );
        assert!(wal_applied(&wal(false, Value::Null, serde_json::json!(5)), &tables).is_err());
        assert!(
            wal_applied(
                &wal(false, serde_json::json!(6), serde_json::json!(5)),
                &tables
            )
            .is_err()
        );
        assert!(
            wal_applied(
                &wal(false, serde_json::json!(5), serde_json::json!(5)),
                &["candles_1m"]
            )
            .is_err()
        );
        assert!(wal_applied(&serde_json::json!({}), &tables).is_err());
    }

    #[test]
    fn queries_use_all_identity_fields_the_storage_clock_and_a_fixed_bound() {
        let expected = row(39582, 2);
        let query = rows_query("candles_5s", &[expected]).expect("bounded query");
        assert!(query.contains("cast(1716023700000000 as timestamp)"));
        assert!(query.contains("security_id=39582"));
        assert!(query.contains("segment='NSE_FNO'"));
        assert!(query.contains("feed='dhan'"));
        assert!(query.ends_with("limit 2"));
        assert_eq!(sql_string("a'b"), "'a''b'");
        assert!(rows_query("candles_2s", &[expected]).is_err());
        assert!(rows_query("candles_5s", &vec![expected; MAX_RECOVERY_BATCH + 1]).is_err());
    }

    #[derive(Default)]
    struct MemoryStore {
        persisted: Vec<ShadowSealRow>,
        writes: usize,
        reads: usize,
        partial_ack: Option<usize>,
        wal_unapplied: bool,
        wal_suspended: bool,
        hide_after_write: bool,
    }

    impl RecoveryStore for MemoryStore {
        async fn applied_rows(
            &mut self,
            rows: &[ShadowSealRow],
        ) -> Result<Vec<bool>, RecoveryRefusal> {
            self.reads += 1;
            ensure!(
                !self.wal_suspended && !self.wal_unapplied,
                "injected unconfirmed WAL"
            );
            if self.hide_after_write && self.writes > 0 {
                return Ok(vec![false; rows.len()]);
            }
            rows.iter()
                .map(
                    |row| match self.persisted.iter().find(|prior| same_key(prior, row)) {
                        Some(prior) if same_row(prior, row) => Ok(true),
                        Some(_) => Err(RecoveryRefusal::Conflict),
                        None => Ok(false),
                    },
                )
                .collect()
        }

        fn insert_missing(&mut self, rows: &[ShadowSealRow]) -> Result<(), RecoveryRefusal> {
            self.writes += 1;
            for row in rows.iter().take(self.partial_ack.unwrap_or(rows.len())) {
                assert!(
                    !self.persisted.iter().any(|prior| same_key(prior, row)),
                    "policy must never authorize a last-write-wins overwrite"
                );
                self.persisted.push(*row);
            }
            ensure!(
                self.partial_ack.take().is_none(),
                "injected lost/partial acknowledgment"
            );
            Ok(())
        }
    }

    #[tokio::test]
    async fn missing_rows_recover_and_identical_retry_sends_no_new_rows() {
        let rows = [row(39582, 2), row(39583, 3)];
        let mut store = MemoryStore::default();
        assert_eq!(
            reconcile(&mut store, &rows).await.expect("missing rows"),
            RecoveredBatch {
                inserted: 2,
                identical: 0
            }
        );
        assert_eq!(
            store.reads, 2,
            "confirmation requires a second applied-row read"
        );
        assert_eq!(
            reconcile(&mut store, &rows).await.expect("identical retry"),
            RecoveredBatch {
                inserted: 0,
                identical: 2
            }
        );
        assert_eq!(store.writes, 1);
        assert_eq!(store.persisted.len(), 2);
    }

    #[tokio::test]
    async fn partial_ack_retries_skip_confirmed_keys_and_recover_only_missing_rows() {
        let rows = [row(39582, 2), row(39583, 3), row(39584, 4)];
        for committed_prefix in 0..=rows.len() {
            let mut store = MemoryStore {
                partial_ack: Some(committed_prefix),
                ..Default::default()
            };
            assert!(reconcile(&mut store, &rows).await.is_err());
            assert_eq!(store.persisted.len(), committed_prefix);
            let retry = reconcile(&mut store, &rows)
                .await
                .expect("reconcile ambiguous first ACK");
            assert_eq!(retry.inserted, rows.len() - committed_prefix);
            assert_eq!(retry.identical, committed_prefix);
            assert_eq!(store.persisted.len(), rows.len());
        }
    }

    #[tokio::test]
    async fn differing_rows_never_overwrite_even_when_local_revision_is_larger_or_restarted() {
        let stored = row(39582, 2);
        for revision in [0, 1, 2, 3, 100_000] {
            let mut candidate = row(39582, revision);
            candidate.volume += 600;
            let mut store = MemoryStore {
                persisted: vec![stored],
                ..Default::default()
            };
            for _boot in 0..2 {
                assert!(matches!(
                    reconcile(&mut store, &[row(39583, 1), candidate]).await,
                    Err(RecoveryRefusal::Conflict)
                ));
                assert_eq!(
                    store.writes, 0,
                    "even harmless siblings wait when this batch conflicts"
                );
                assert!(same_row(&store.persisted[0], &stored));
            }
        }
    }

    #[tokio::test]
    async fn same_file_and_cross_file_different_revisions_remain_conflicts() {
        let old = row(39582, 1);
        let mut later = row(39582, 2);
        later.close += 1.0;
        let mut same_file = MemoryStore::default();
        assert!(matches!(
            reconcile(&mut same_file, &[old, later]).await,
            Err(RecoveryRefusal::Conflict)
        ));
        assert_eq!(same_file.reads, 0);
        assert_eq!(same_file.writes, 0);
        let mut cross_file = MemoryStore::default();
        reconcile(&mut cross_file, &[old, old])
            .await
            .expect("identical duplicates are harmless");
        assert_eq!(cross_file.persisted.len(), 1);
        assert!(matches!(
            reconcile(&mut cross_file, &[later]).await,
            Err(RecoveryRefusal::Conflict)
        ));
        assert!(same_row(&cross_file.persisted[0], &old));
    }

    #[tokio::test]
    async fn unapplied_wal_and_missing_post_ack_rows_never_confirm_recovery() {
        let rows = [row(39582, 1)];
        for suspended in [false, true] {
            let mut store = MemoryStore {
                wal_suspended: suspended,
                wal_unapplied: !suspended,
                ..Default::default()
            };
            assert!(reconcile(&mut store, &rows).await.is_err());
            assert_eq!(
                store.writes, 0,
                "unapplied rows cannot be mistaken for absent rows"
            );
        }
        let mut store = MemoryStore {
            hide_after_write: true,
            ..Default::default()
        };
        assert!(
            reconcile(&mut store, &rows).await.is_err(),
            "HTTP success alone is not recovery confirmation"
        );
        assert_eq!(store.writes, 1);
    }

    /// Release gate against an isolated loopback QuestDB (pinned by CI). It
    /// refuses an already-present candles_5s table and creates/removes only
    /// its own fresh fixture. A normal unit run skipping this is not a pass.
    /// Suspended/unapplied failure injection is covered by the pure guards;
    /// this fixture does not claim to suspend a real database worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TICKVAULT_ISOLATED_QUESTDB_EXEC_URL on a throwaway loopback QuestDB"]
    async fn isolated_questdb_recovery_inserts_skips_and_refuses_conflicts() {
        let endpoint = std::env::var("TICKVAULT_ISOLATED_QUESTDB_EXEC_URL")
            .expect("explicit isolated QuestDB URL required");
        let url = reqwest::Url::parse(&endpoint).expect("valid isolated URL");
        assert_eq!(url.scheme(), "http");
        assert!(
            matches!(
                url.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            ),
            "fixture refuses non-loopback endpoints"
        );
        assert_eq!(url.path(), "/exec");
        assert!(url.username().is_empty() && url.password().is_none());
        let config = tickvault_common::config::QuestDbConfig {
            host: url.host_str().expect("loopback host").to_owned(),
            http_port: url.port_or_known_default().expect("HTTP port"),
            pg_port: 8812,
            ilp_port: 9009,
        };
        let mut writer = ShadowCandleWriter::new(&config).expect("fixture writer");
        let client = crate::http_client::build_probe_client(QUERY_TIMEOUT_SECS)
            .expect("fixture HTTP client");
        let mut candidate = seal(39582, 7);
        candidate.state.open = 98.15;
        candidate.state.high = 103.65;
        candidate.state.low = 96.75;
        candidate.state.close = 101.35;
        candidate.state.close_pct_from_prev_day = 1.27;
        candidate.state.open_pct = -0.43;
        candidate.state.open_gap_pct = 0.19;
        let expected = ShadowSealRow::from_buffered_seal(&candidate);
        let mut store = QuestDbRecovery {
            writer: &mut writer,
            client,
            exec_url: &endpoint,
        };
        let exists = store
            .query("select table_name from tables() where table_name='candles_5s'")
            .await
            .expect("inspect fixture namespace");
        assert_eq!(
            exists
                .get("dataset")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0),
            "fixture refuses any preexisting candles_5s table"
        );
        let declaration = COLUMNS
            .into_iter()
            .zip(cells(&expected))
            .enumerate()
            .map(|(index, (name, cell))| {
                if index == 0 {
                    return "ts TIMESTAMP".to_owned();
                }
                let kind = match cell {
                    Cell::Integer(_) => "LONG",
                    Cell::Float(_) => "DOUBLE",
                    Cell::Text(_) => "SYMBOL",
                };
                format!("{name} {kind}")
            })
            .collect::<Vec<_>>()
            .join(",");
        store.query(&format!("create table candles_5s ({declaration}) timestamp(ts) partition by day WAL DEDUP UPSERT KEYS(ts,security_id,segment,feed)")).await.expect("create fresh fixture table");

        let checked: Result<()> = async {
            let inserted = reconcile(&mut store, &[expected]).await.map_err(|error| anyhow::anyhow!("missing insert failed: {error:?}"))?;
            anyhow::ensure!(inserted == RecoveredBatch { inserted: 1, identical: 0 }, "missing insert was not confirmed");
            let identical = reconcile(&mut store, &[expected]).await.map_err(|error| anyhow::anyhow!("exact retry failed: {error:?}"))?;
            anyhow::ensure!(identical == RecoveredBatch { inserted: 0, identical: 1 }, "exact retry was not skipped");
            let mut conflict = expected;
            conflict.volume += 600;
            anyhow::ensure!(matches!(reconcile(&mut store, &[conflict]).await, Err(RecoveryRefusal::Conflict)), "different gross quantity must remain a conflict");
            // A historical nullable basis is not guessed from today's metric.
            store.query("update candles_5s set volume_basis=null where security_id=39582").await?;
            store.wait_applied(&["candles_5s"]).await?;
            anyhow::ensure!(matches!(reconcile(&mut store, &[expected]).await, Err(RecoveryRefusal::Conflict)), "legacy NULL basis must refuse relabeling");

            // A migrated 18-column row can be confirmed as an unchanged
            // legacy duplicate only after all seven extensions remain NULL.
            // Merely NULL basis with the revision/quality above is insufficient.
            let legacy = ShadowSealRow::from_buffered_seal(&legacy_seal(&candidate, 2));
            anyhow::ensure!(matches!(reconcile(&mut store, &[legacy]).await, Err(RecoveryRefusal::Conflict)), "known stored revision/quality cannot be ignored");
            store.query("update candles_5s set volume_basis=null,lot_size=null,instrument_definition_version=null,underlying_id=null,ranking_family=null,volume_quality=null,bucket_revision=null where security_id=39582").await?;
            store.wait_applied(&["candles_5s"]).await?;
            let retained_legacy = reconcile(&mut store, &[legacy]).await.map_err(|error| anyhow::anyhow!("legacy duplicate not recognized: {error:?}"))?;
            anyhow::ensure!(retained_legacy == RecoveredBatch { inserted: 0, identical: 1 }, "legacy duplicate must skip without rewriting provenance");
            let legacy_body = store.query(&rows_query("candles_5s", &[legacy])?).await?;
            let legacy_record = &legacy_body["dataset"][0];
            anyhow::ensure!([16,19,20,21,22,23,24].into_iter().all(|index| legacy_record[index].is_null()), "legacy duplicate must leave all additive database fields NULL");

            // Exercise the real staged-file caller and production boot scope:
            // valid conflicting bytes remain staged across repeated attempts.
            let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
            let root = std::env::temp_dir().join(format!("tv-recovery-sql-{}-{unique}", std::process::id()));
            let spill = root.join("spill");
            let dlq = root.join("dlq");
            std::fs::create_dir_all(&spill)?;
            let name = "seals-isolated-conflict.cseal4";
            let bytes = crate::seal_spill::SerializedSeal::from(&candidate).to_bytes();
            std::fs::write(spill.join(name), bytes)?;
            store.writer.set_boot_recovery_exclusive(true);
            for _attempt in 0..2 {
                let outcome = tokio::task::block_in_place(|| crate::seal_writer_task::drain_recovered_seals(store.writer, &spill, &dlq, 64));
                anyhow::ensure!(outcome.files_deferred_conflict == 1 && outcome.files_left_pending == 1 && outcome.files_archived == 0 && outcome.seals_reingested == 0, "conflicting staged original was not retained: {outcome:?}");
                anyhow::ensure!(std::fs::read(spill.join(crate::seal_writer_task::SEAL_REPLAYING_SUBDIR).join(name))? == bytes, "staged original changed");
            }
            store.writer.set_boot_recovery_exclusive(false);
            // Only the synthetic local fixture is cleaned; live recovery
            // itself never deletes or replaces an unresolved original.
            std::fs::remove_dir_all(root)?;
            Ok(())
        }.await;
        let cleanup = store.query("drop table candles_5s").await;
        cleanup.expect("remove this test's fresh fixture table");
        checked.expect("isolated QuestDB recovery contract");
    }
}
