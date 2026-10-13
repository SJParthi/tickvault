//! The top_volume spill replay is started on EVERY boot (top_volume spill
//! plan item 5, PR-C review 2026-10-13).
//!
//! `[top_volume] spill_enabled = false` is the rollback switch for the
//! WRITER: board batches that do not land are then discarded and counted, as
//! before the spill tier existed. It must never stop the REPLAY. Files
//! already on disk would otherwise never be replayed or aged, and the daily
//! archive would hold their `top_volume_<tf>` hours forever. The writer half
//! of the rollback is pinned by `test_spill_disabled_restores_discard_behaviour`
//! in `top_volume_rank_persistence.rs`.

fn main_production_region() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&path).expect("read main.rs"); // APPROVED: test
    src.split_once("\n#[cfg(test)]")
        .map_or(src.as_str(), |(before, _)| before)
        .to_string()
}

#[test]
fn test_top_volume_spill_replay_is_started_whatever_the_switch_says() {
    let src = main_production_region();
    let call = src
        .find("spawn_supervised_top_volume_spill_replay(")
        .expect("main.rs must start the top_volume spill replay");
    assert_eq!(
        src.matches("spawn_supervised_top_volume_spill_replay(")
            .count(),
        1,
        "exactly one top_volume replay supervisor"
    );
    // The statement that holds the call must not be gated on the switch.
    let stmt_start = src[..call].rfind(';').unwrap_or(0);
    // Code only: the comment above the call explains the switch by name.
    let stmt: String = src[stmt_start..call]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !stmt.contains("spill_enabled"),
        "the replay must not be gated on `[top_volume] spill_enabled`: {stmt}"
    );
    assert!(
        !stmt.contains(".then("),
        "the replay must not be optional: {stmt}"
    );
    // The switch still reaches the writer.
    assert!(src.contains("top_volume_spill_enabled: config.top_volume.spill_enabled"));
}
