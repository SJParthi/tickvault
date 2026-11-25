//! Regression: the Portal consumer reads the canonical producer's daily paths.
#[test]
fn index_and_fno_movers_read_the_existing_daily_artifacts() {
    use tickvault_api::handlers::{fno_stocks, stock_movers};
    let date = "2026-11-02";
    let directory = std::path::Path::new(fno_stocks::FNO_UNDERLYING_ARTIFACT_DIR);
    assert_eq!(
        directory.join(format!("{}{date}.json", stock_movers::INDEX_MAPPING_PREFIX)),
        tickvault_app::dhan_universe::mapping_artifact_path(date)
    );
    assert_eq!(
        fno_stocks::fno_underlying_artifact_path(directory, date),
        tickvault_app::dhan_universe::fno_underlying_artifact_path(date)
    );
}
