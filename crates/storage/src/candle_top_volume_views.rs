//! Historical Top Volume is a projection of the persisted candle quantity.
//!
//! These query-time views do not participate in the RAM decision path. They
//! neither re-difference source counters nor infer a second signed quantity.
//! Lot size and instrument definition are pinned on each candle; the current
//! lifecycle master supplies display labels only. Pre-migration rows remain
//! visible with unavailable scores instead of borrowing today's lot size.

use tickvault_trading::candles::TfIndex;

/// Versioned score: signed whole-bar quantity relative to one lot.
pub use tickvault_trading::candles::CANDLE_SIGNED_BAR_VS_ONE_LOT_METRIC as CANDLE_TOP_VOLUME_METRIC_VERSION;

/// One view name for a selected Top Volume candle timeframe, derived from its
/// canonical suffix rather than a second hand-maintained timeframe registry.
/// Candle-only durations return `None` and cannot generate a primary view.
#[must_use]
pub fn candle_top_volume_view_name(tf: TfIndex) -> Option<String> {
    if !tf.is_top_volume() {
        return None;
    }
    Some(format!(
        "top_volume_{}",
        tf.table_name().trim_start_matches("candles_")
    ))
}

/// Collapse the current display dimension before joining a candle. The master
/// can contain duplicates from historical degraded writes even after DEDUP is
/// enabled. Grouping the full identity guarantees one dimension row per key.
/// When the group is ambiguous, labels are NULL instead of choosing a winner
/// by insertion order; its count remains visible for diagnosis. The first()
/// value is exposed only for a singleton, so it is deterministic without a
/// timestamp tie-break. Cast legacy SYMBOL/STRING columns to VARCHAR, which
/// the aggregate accepts, while keeping identity columns in their own types.
///
/// Aggregate reference: <https://questdb.com/docs/query/functions/aggregation/>.
/// The complete grouped view still requires execution on the pinned engine.
pub(super) fn unique_candle_lifecycle_dim_subquery() -> &'static str {
    "(SELECT security_id, exchange_segment, feed, count() AS metadata_row_count, \
     CASE WHEN count() = 1 THEN first(cast(symbol_name AS VARCHAR)) \
          ELSE cast(null AS VARCHAR) END AS symbol_name, \
     CASE WHEN count() = 1 THEN first(cast(display_name AS VARCHAR)) \
          ELSE cast(null AS VARCHAR) END AS display_name, \
     CASE WHEN count() = 1 THEN first(cast(instrument_type AS VARCHAR)) \
          ELSE cast(null AS VARCHAR) END AS instrument_type \
     FROM instrument_lifecycle WHERE dry_run = false \
     GROUP BY security_id, exchange_segment, feed) il"
}

/// A row can be displayed without being suitable for ranking. In particular,
/// SQL NULL is not a classified unchanged-close bar, and a missing definition is not today's lot.
/// The quality mask establishes the local fold contract, not exchange-wide
/// tick completeness or observed aggressor-side volume.
fn eligibility_sql() -> &'static str {
    "c.volume_basis = 'signed_bar_volume_vs_one_lot_v3' \
     AND c.volume_quality = 0 AND c.net_volume IS NOT NULL \
     AND c.lot_size BETWEEN 1 AND 4294967295 \
     AND c.instrument_definition_version > 0 \
     AND c.underlying_id > 0 AND c.ranking_family IN ('stock', 'index') \
     AND c.bucket_revision > 0 AND c.volume >= 0 \
     AND (c.net_volume = c.volume OR c.net_volume = -c.volume OR c.net_volume = 0)"
}

/// Exact signed rational ordering using signed LONG operations only.
///
/// Signed division truncates toward zero. The integer quotient and three
/// signed remainder limbs therefore order both positive and negative ratios
/// without taking abs(net), which would overflow for LONG_MIN. A 31/31/2-bit
/// expansion distinguishes distinct positive-u32-denominator fractions:
/// their distance exceeds 2^-64. Each remainder product remains inside LONG.
/// Safe substitute operands also protect planners which evaluate a key before
/// its eligibility flag. NULL, missing metadata and uncertain rows sort after
/// eligible rows within the same bucket/family.
fn exact_signed_ratio_order_sql() -> String {
    let eligible = eligibility_sql();
    let numerator = format!("(CASE WHEN {eligible} THEN c.net_volume ELSE cast(0 AS LONG) END)");
    let denominator = format!("(CASE WHEN {eligible} THEN c.lot_size ELSE cast(1 AS LONG) END)");
    let remainder = format!("({numerator} % {denominator})");
    let high_product = format!("({remainder} * 2147483648)");
    let middle_product = format!("(({high_product} % {denominator}) * 2147483648)");
    let low_product = format!("(({middle_product} % {denominator}) * 4)");
    format!(
        "CASE WHEN {eligible} THEN 1 ELSE 0 END DESC, \
         ({numerator} / {denominator}) DESC, \
         ({high_product} / {denominator}) DESC, \
         ({middle_product} / {denominator}) DESC, \
         ({low_product} / {denominator}) DESC"
    )
}

/// Create/converge one cold historical view. Every score consumes exactly
/// `candles_<tf>.net_volume` and that row's pinned `lot_size`. The DOUBLE
/// expressions are display values only; ORDER BY uses exact integer limbs.
///
/// The timestamp remains the candle bucket opening under the existing IST
/// wall-clock convention. No synthesized bucket-end or historical live-state
/// claim is introduced by this view. `bucket_revision` identifies amendments.
/// `volume_basis` distinguishes current whole-bar caches from historical sums.
///
/// This is SQL generation, not proof that the deployed QuestDB accepted the
/// DDL. Signed-division and view execution on the pinned engine remain release
/// gates. Selecting history incurs join/sort/output work proportional to the
/// selected population; it is not an O(1) operation.
#[must_use]
pub fn candle_top_volume_view_ddl(tf: TfIndex) -> Option<String> {
    let view = candle_top_volume_view_name(tf)?;
    let table = tf.table_name();
    let timeframe = table.trim_start_matches("candles_");
    let eligible = eligibility_sql();
    let exact_order = exact_signed_ratio_order_sql();
    let dimension = unique_candle_lifecycle_dim_subquery();
    let denominator = format!("(CASE WHEN {eligible} THEN c.lot_size ELSE cast(1 AS LONG) END)");
    Some(format!(
        "CREATE OR REPLACE VIEW {view} AS \
         SELECT c.ts, il.symbol_name, il.display_name, il.instrument_type, \
         c.ranking_family AS family, \
         CASE WHEN c.volume_basis = '{CANDLE_TOP_VOLUME_METRIC_VERSION}' \
              THEN c.net_volume ELSE cast(null AS LONG) END AS volume, c.lot_size, \
         CASE WHEN {eligible} THEN cast(c.net_volume AS DOUBLE) / cast({denominator} AS DOUBLE) \
              ELSE cast(null AS DOUBLE) END AS volume_lots, \
         CASE WHEN {eligible} THEN \
              (cast(c.net_volume AS DOUBLE) / cast({denominator} AS DOUBLE) - 1.0) * 100.0 \
              ELSE cast(null AS DOUBLE) END AS volume_chg_pct, \
         CASE WHEN {eligible} THEN true ELSE false END AS rank_eligible, \
         c.volume_quality, c.bucket_revision, c.instrument_definition_version, \
         c.underlying_id, c.feed, c.segment, c.security_id, \
         '{timeframe}' AS tf, \
         '{CANDLE_TOP_VOLUME_METRIC_VERSION}' AS metric_version, \
         c.volume_basis, \
         'persisted_candle' AS lot_metadata_basis, \
         'unique_current_lifecycle_labels' AS display_metadata_basis, \
         il.metadata_row_count AS display_metadata_rows, \
         CASE WHEN il.metadata_row_count > 1 THEN true ELSE false END AS display_metadata_ambiguous, \
         '{table}' AS source_table \
         FROM {table} c \
         LEFT JOIN {dimension} \
         ON c.security_id = il.security_id \
         AND c.segment = il.exchange_segment \
         AND c.feed = il.feed \
         ORDER BY c.ts DESC, c.ranking_family ASC, {exact_order}, \
         c.security_id ASC, \
         CASE WHEN c.segment = 'IDX_I' THEN 0 \
              WHEN c.segment = 'NSE_EQ' THEN 1 \
              WHEN c.segment = 'NSE_FNO' THEN 2 \
              WHEN c.segment = 'NSE_CURRENCY' THEN 3 \
              WHEN c.segment = 'BSE_EQ' THEN 4 \
              WHEN c.segment = 'MCX_COMM' THEN 5 \
              WHEN c.segment = 'BSE_CURRENCY' THEN 6 \
              WHEN c.segment = 'BSE_FNO' THEN 7 \
              ELSE 255 END ASC, c.segment ASC, c.feed ASC;"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn candle_top_volume_view_name_and_source_cover_every_timeframe_once() {
        let mut seen = std::collections::BTreeSet::new();
        for tf in TfIndex::TOP_VOLUME_ALL {
            let name = candle_top_volume_view_name(tf).expect("selected primary view");
            assert!(seen.insert(name.clone()));
            assert_eq!(
                name.strip_prefix("top_volume_"),
                tf.table_name().strip_prefix("candles_")
            );
            let ddl = candle_top_volume_view_ddl(tf).expect("selected primary DDL");
            assert!(ddl.starts_with(&format!("CREATE OR REPLACE VIEW {name} AS ")));
            assert!(ddl.contains(&format!("FROM {} c ", tf.table_name())));
            assert!(ddl.contains(&format!("'{}' AS source_table", tf.table_name())));
            assert_eq!(ddl.matches(';').count(), 1);
            assert!(!ddl.contains("FROM top_volume "));
            assert!(!ddl.contains("DROP "));
        }
        assert_eq!(seen.len(), TfIndex::TOP_VOLUME_ALL.len());
        let expected: std::collections::BTreeSet<String> = ["1s", "3s", "5s", "1m"]
            .into_iter()
            .map(|suffix| format!("top_volume_{suffix}"))
            .collect();
        assert_eq!(
            seen, expected,
            "primary views must be exactly the requested four"
        );
    }

    #[test]
    fn candle_only_timeframes_cannot_generate_primary_top_volume_names_or_ddl() {
        for tf in [
            TfIndex::M3,
            TfIndex::M5,
            TfIndex::M10,
            TfIndex::M15,
            TfIndex::M30,
            TfIndex::M60,
        ] {
            assert!(
                candle_top_volume_view_name(tf).is_none(),
                "{tf:?} is candle-only"
            );
            assert!(
                candle_top_volume_view_ddl(tf).is_none(),
                "{tf:?} must not generate CREATE VIEW"
            );
        }
    }

    #[test]
    fn candle_top_volume_view_ddl_preserves_signed_quantity_and_missing_metadata() {
        for tf in TfIndex::TOP_VOLUME_ALL {
            let ddl = candle_top_volume_view_ddl(tf).expect("selected primary DDL");
            for required in [
                "END AS volume, c.lot_size",
                "AS volume_lots",
                "AS volume_chg_pct",
                "c.volume_basis = 'signed_bar_volume_vs_one_lot_v3'",
                "AS rank_eligible",
                "c.volume_quality = 0",
                "c.net_volume IS NOT NULL",
                "c.lot_size BETWEEN 1 AND 4294967295",
                "c.instrument_definition_version > 0",
                "c.ranking_family AS family",
                "c.bucket_revision",
                "c.volume_basis",
                "'persisted_candle' AS lot_metadata_basis",
                "ELSE cast(null AS DOUBLE)",
                "LEFT JOIN",
                "c.security_id = il.security_id",
                "c.segment = il.exchange_segment",
                "c.feed = il.feed",
                "WHERE dry_run = false GROUP BY security_id, exchange_segment, feed) il",
            ] {
                assert!(ddl.contains(required), "missing {required}: {ddl}");
            }
            assert!(ddl.contains(CANDLE_TOP_VOLUME_METRIC_VERSION));
            for retired in [
                "AS gross_volume",
                "AS gross_lots",
                "AS signed_net_lots",
                "AS net_volume_chg_pct",
                "AS net_volume_basis",
            ] {
                assert!(!ddl.contains(retired), "retired output: {retired}");
            }
            assert!(!ddl.contains("il.lot_size"));
            assert!(!ddl.contains("c.net_volume > 0"));
            assert!(!ddl.contains("coalesce("));
            assert!(!ddl.contains("SELECT *"));
            assert!(!ddl.contains("LIMIT "));
        }
    }

    #[test]
    fn candle_labels_are_grouped_before_join_and_ambiguity_never_selects_a_duplicate() {
        let dimension = unique_candle_lifecycle_dim_subquery();
        assert!(dimension.contains("count() AS metadata_row_count"));
        assert!(dimension.ends_with("GROUP BY security_id, exchange_segment, feed) il"));
        for label in ["symbol_name", "display_name", "instrument_type"] {
            assert!(dimension.contains(&format!(
                "CASE WHEN count() = 1 THEN first(cast({label} AS VARCHAR)) ELSE cast(null AS VARCHAR) END AS {label}"
            )));
        }
        for tf in TfIndex::TOP_VOLUME_ALL {
            let ddl = candle_top_volume_view_ddl(tf).expect("selected primary DDL");
            assert!(ddl.contains(&format!("LEFT JOIN {dimension} ON ")));
            assert!(ddl.contains("il.metadata_row_count AS display_metadata_rows"));
            assert!(ddl.contains("AS display_metadata_ambiguous"));
            let (_, ordering) = ddl.split_once(" ORDER BY ").unwrap();
            assert!(!ordering.contains("il."));
            assert!(!eligibility_sql().contains("il."));
        }
    }

    /// Explicit release fixture against an isolated QuestDB. It creates only
    /// uniquely named synthetic tables/views and cleans them up. Ignoring it
    /// in ordinary unit runs is not a pass: the deployed engine's grouped
    /// VARCHAR aggregation and complete view DDL must execute before release.
    #[tokio::test]
    #[ignore = "requires TICKVAULT_ISOLATED_QUESTDB_EXEC_URL and an isolated QuestDB"]
    async fn candle_top_volume_questdb_duplicate_labels_preserve_one_row_per_candle() {
        async fn sql(
            client: &reqwest::Client,
            endpoint: &str,
            query: &str,
        ) -> Result<serde_json::Value, String> {
            let response = client
                .get(endpoint)
                .query(&[("query", query)])
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let status = response.status();
            let body = response
                .json::<serde_json::Value>()
                .await
                .map_err(|error| error.to_string())?;
            if !status.is_success() || body.get("error").is_some() {
                return Err(format!("synthetic fixture SQL failed: {status}: {body}"));
            }
            Ok(body)
        }

        let endpoint = std::env::var("TICKVAULT_ISOLATED_QUESTDB_EXEC_URL")
            .expect("explicit isolated QuestDB /exec URL required");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let prefix = format!("tv_candle_label_fixture_{}_{unique}", std::process::id());
        let candle_table = format!("{prefix}_candles");
        let master_table = format!("{prefix}_master");
        let view = format!("{prefix}_view");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap();
        let result = async {
            sql(&client, &endpoint, &format!(
                "CREATE TABLE {master_table} (security_id LONG, exchange_segment SYMBOL, feed SYMBOL, symbol_name SYMBOL, display_name STRING, instrument_type SYMBOL, dry_run BOOLEAN);"
            )).await?;
            sql(&client, &endpoint, &format!(
                "CREATE TABLE {candle_table} (ts TIMESTAMP, volume LONG, net_volume LONG, lot_size LONG, instrument_definition_version LONG, underlying_id LONG, ranking_family SYMBOL, volume_quality LONG, bucket_revision LONG, feed SYMBOL, segment SYMBOL, security_id LONG, volume_basis SYMBOL);"
            )).await?;
            sql(&client, &endpoint, &format!(
                "INSERT INTO {master_table} VALUES \
                 (1,'NSE_FNO','dhan','ONE','One option','OPTSTK',false), \
                 (2,'NSE_FNO','dhan','TWO','Two option','OPTSTK',false), \
                 (2,'NSE_FNO','dhan','TWO','Two option','OPTSTK',false), \
                 (3,'NSE_FNO','dhan','OLD','Old option','OPTSTK',false), \
                 (3,'NSE_FNO','dhan','NEW','New option','OPTIDX',false), \
                 (4,'NSE_FNO','dhan','FOUR','Four option','OPTSTK',false), \
                 (4,'NSE_FNO','dhan','DRY','Dry-run option','OPTIDX',true), \
                 (6,'NSE_FNO','dhan','DHAN','Dhan option','OPTSTK',false), \
                 (6,'NSE_FNO','truedata','TD','TrueData option','OPTSTK',false), \
                 (7,'NSE_FNO','dhan','NSE','NSE option','OPTSTK',false), \
                 (7,'BSE_FNO','dhan','BSE','BSE option','OPTSTK',false);"
            )).await?;
            sql(&client, &endpoint, &format!(
                "INSERT INTO {candle_table} VALUES \
                 (cast(1000000 AS TIMESTAMP),200,200,100,1,10,'stock',0,1,'dhan','NSE_FNO',1,'signed_bar_volume_vs_one_lot_v3'), \
                 (cast(1000000 AS TIMESTAMP),100,100,100,1,10,'stock',0,1,'dhan','NSE_FNO',2,'signed_bar_volume_vs_one_lot_v3'), \
                 (cast(1000000 AS TIMESTAMP),600,0,100,1,10,'stock',0,1,'dhan','NSE_FNO',3,'signed_bar_volume_vs_one_lot_v3'), \
                 (cast(1000000 AS TIMESTAMP),50,-50,100,1,10,'stock',0,1,'dhan','NSE_FNO',4,'signed_bar_volume_vs_one_lot_v3'), \
                 (cast(1000000 AS TIMESTAMP),100,-100,100,1,10,'stock',0,1,'dhan','NSE_FNO',5,'signed_bar_volume_vs_one_lot_v3'), \
                 (cast(1000000 AS TIMESTAMP),100,100,100,1,10,'stock',0,1,'dhan','NSE_FNO',6,'signed_bar_volume_vs_one_lot_v3'), \
                 (cast(1000000 AS TIMESTAMP),100,100,100,1,10,'stock',0,1,'truedata','NSE_FNO',6,'signed_bar_volume_vs_one_lot_v3'), \
                 (cast(1000000 AS TIMESTAMP),100,100,100,1,10,'stock',0,1,'dhan','NSE_FNO',7,'signed_bar_volume_vs_one_lot_v3'), \
                 (cast(1000000 AS TIMESTAMP),100,100,100,1,10,'stock',0,1,'dhan','BSE_FNO',7,'signed_bar_volume_vs_one_lot_v3');"
            )).await?;
            let ddl = candle_top_volume_view_ddl(TfIndex::M1)
                .expect("1m primary DDL")
                .replace("top_volume_1m", &view)
                .replace("candles_1m", &candle_table)
                .replace("instrument_lifecycle", &master_table);
            sql(&client, &endpoint, &ddl).await?;
            let result = sql(&client, &endpoint, &format!(
                "SELECT security_id,segment,feed,rank_eligible,display_metadata_rows,display_metadata_ambiguous,symbol_name,display_name FROM {view};"
            )).await?;
            let rows = result.get("dataset").and_then(serde_json::Value::as_array)
                .ok_or_else(|| "fixture response has no dataset".to_owned())?;
            if rows.len() != 9 {
                return Err(format!("expected 9 candle rows, got {}: {rows:?}", rows.len()));
            }
            let expected = [
                (1, "NSE_FNO", "dhan", Some(1), false, Some("ONE")),
                (2, "NSE_FNO", "dhan", Some(2), true, None),
                (6, "NSE_FNO", "dhan", Some(1), false, Some("DHAN")),
                (6, "NSE_FNO", "truedata", Some(1), false, Some("TD")),
                (7, "NSE_FNO", "dhan", Some(1), false, Some("NSE")),
                (7, "BSE_FNO", "dhan", Some(1), false, Some("BSE")),
                (3, "NSE_FNO", "dhan", Some(2), true, None),
                (4, "NSE_FNO", "dhan", Some(1), false, Some("FOUR")),
                (5, "NSE_FNO", "dhan", None, false, None),
            ];
            for (actual, (id, segment, feed, count, ambiguous, label)) in rows.iter().zip(expected) {
                if actual[0].as_i64() != Some(id) || actual[1] != segment || actual[2] != feed
                    || actual[3].as_bool() != Some(true) || actual[4].as_i64() != count
                    || actual[5].as_bool() != Some(ambiguous) || actual[6].as_str() != label
                    || (ambiguous && !actual[7].is_null())
                {
                    return Err(format!("unexpected candle/label/ordering result: {actual}"));
                }
            }
            Ok::<(), String>(())
        }.await;
        // Cleanup is specific to the unique synthetic names above, never a
        // prefix sweep over existing user tables. Preserve the first failure.
        for query in [
            format!("DROP VIEW IF EXISTS {view};"),
            format!("DROP TABLE IF EXISTS {candle_table};"),
            format!("DROP TABLE IF EXISTS {master_table};"),
        ] {
            let cleanup = sql(&client, &endpoint, &query).await;
            assert!(
                cleanup.is_ok(),
                "synthetic fixture cleanup failed: {cleanup:?}; fixture result: {result:?}"
            );
        }
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn candle_top_volume_order_is_exact_signed_net_per_lot_not_display_or_price() {
        for tf in TfIndex::TOP_VOLUME_ALL {
            let ddl = candle_top_volume_view_ddl(tf).expect("selected primary DDL");
            let (_, order) = ddl.split_once(" ORDER BY ").unwrap();
            assert!(order.starts_with("c.ts DESC, c.ranking_family ASC, CASE WHEN "));
            assert!(order.contains(&exact_signed_ratio_order_sql()));
            assert!(order.contains("2147483648"));
            assert!(order.contains(" * 4)"));
            assert!(!order.contains("DOUBLE"));
            assert!(!order.contains("net_volume_chg_pct"));
            assert!(!order.contains("gain_pct"));
            assert!(!order.contains("abs("));
            assert!(order.ends_with("ELSE 255 END ASC, c.segment ASC, c.feed ASC;"));
        }
    }

    // Independent integer model of the SQL limbs. Cross-products use i128,
    // while the SQL model deliberately uses i64 to expose intermediate
    // overflow. This tests the arithmetic envelope, not QuestDB execution.
    fn signed_key(numerator: i64, denominator: u32) -> [i64; 4] {
        let denominator = i64::from(denominator);
        let quotient = numerator / denominator;
        let remainder = numerator % denominator;
        let high = remainder * (1_i64 << 31);
        let middle = (high % denominator) * (1_i64 << 31);
        let low = (middle % denominator) * 4;
        [
            quotient,
            high / denominator,
            middle / denominator,
            low / denominator,
        ]
    }

    fn exact_cmp(left: (i64, u32), right: (i64, u32)) -> Ordering {
        (i128::from(left.0) * i128::from(right.1)).cmp(&(i128::from(right.0) * i128::from(left.1)))
    }

    fn assert_key_order(mut rows: Vec<(i64, u32)>) {
        rows.sort_unstable_by_key(|&(n, d)| signed_key(n, d));
        for pair in rows.windows(2) {
            let expected = exact_cmp(pair[0], pair[1]);
            assert_ne!(expected, Ordering::Greater, "{pair:?}");
            let observed = signed_key(pair[0].0, pair[0].1).cmp(&signed_key(pair[1].0, pair[1].1));
            assert_eq!(observed, expected, "inexact tie: {pair:?}");
        }
    }

    #[test]
    fn signed_fraction_limbs_match_exact_cross_products_across_zero_and_true_ties() {
        let mut rows = Vec::new();
        for numerator in -100..=100 {
            for denominator in 1..=97 {
                rows.push((numerator, denominator));
            }
        }
        assert_key_order(rows);
        assert_eq!(signed_key(-1, 2), signed_key(-2, 4));
        assert_eq!(signed_key(1, 2), signed_key(2, 4));
        assert!(signed_key(-1, u32::MAX) < signed_key(0, 1));
        assert!(signed_key(0, 1) < signed_key(1, u32::MAX));
    }

    #[test]
    fn signed_fraction_limbs_distinguish_extreme_long_and_u32_ratios_without_overflow() {
        let mut rows = Vec::new();
        for numerator in [
            i64::MIN + 1,
            i64::MIN + 2,
            -i64::from(u32::MAX),
            -2,
            -1,
            0,
            1,
            2,
            i64::from(u32::MAX),
            i64::MAX - 1,
            i64::MAX,
        ] {
            for denominator in [1, 2, 3, 31, 999_983, u32::MAX - 1, u32::MAX] {
                rows.push((numerator, denominator));
            }
        }
        // These neighboring fractions round to the same f64 but must not tie.
        for sign in [-1_i64, 1] {
            rows.push((sign * i64::from(u32::MAX - 1), u32::MAX));
            rows.push((sign * i64::from(u32::MAX - 2), u32::MAX - 1));
        }
        let mut state = 0x8a51_c327_d913_b507_u64;
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let numerator = i64::from_ne_bytes(state.to_ne_bytes());
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let denominator = (state as u32).max(1);
            if numerator != i64::MIN {
                rows.push((numerator, denominator));
            }
        }
        assert_key_order(rows);
    }
}
