//! Build-failing ratchet on the boot Telegram's Dhan per-minute capture claim.
//!
//! ADDED 2026-09-17, after finding the claim live and false.
//!
//! The boot Telegram's capture line tells the operator whether today's
//! per-minute Dhan spot-1m and option-chain pulls will fire. Until this guard
//! it computed that from config —
//! `spot_1m_rest.enabled || (cadence.enabled && cadence.dhan_lane)` — which was
//! correct while those legs existed.
//!
//! They were REMOVED on 2026-09-16 under the operator's sockets-only directive.
//! The config was not: `[cadence] enabled` and `dhan_lane` both still read
//! `true`, so BOTH operands evaluated true and the boot Telegram sent, on every
//! trading morning:
//!
//! > ✅ Dhan per-minute price capture — armed (fires 9:16 AM to 3:30 PM IST on
//! > trading days): Dhan spot candles for N indices + Dhan option chain for M
//! > indices
//!
//! for legs with no module, no executor and no scheduler.
//!
//! **This is the worst-placed false-OK in the removal**, and the reason is
//! placement rather than severity: every other stale claim fixed in this sweep
//! sits in a file somebody has to open. This one is DELIVERED — a green
//! checkmark, on the operator's phone, asserting a capability that does not
//! exist, once per trading day.
//!
//! What this guard pins, and why each half matters:
//!
//! 1. `main.rs` passes the const, not config. A flag cannot arm a module that
//!    is not compiled, so reading config here can only ever produce a wrong
//!    answer.
//! 2. The const's value is DERIVED from whether a producing module actually
//!    exists — it is not a stamped `false`. So restoring a leg without flipping
//!    the const fails HERE, and flipping the const without a leg fails HERE.
//!    That is the only form that cannot go stale, and it is the same shape as
//!    `rest_candle_fold::LIVE_INLET_HAS_PRODUCER`.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_source(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

/// Production region = everything above the FIRST column-0 `#[cfg(test)]`
/// (the house source-scan convention — a literal inside a test module must
/// never satisfy a production needle).
fn production_region(src: &str) -> String {
    match src.find("\n#[cfg(test)]") {
        Some(at) => src[..at].to_string(),
        None => src.to_string(),
    }
}

/// The modules that WOULD produce per-minute Dhan REST bars if any existed.
///
/// Named individually rather than globbed: a glob would quietly start matching
/// some future unrelated file and flip the derived expectation with it.
const PRODUCER_MODULES: &[&str] = &[
    "crates/app/src/spot_1m_rest_boot.rs",
    "crates/app/src/option_chain_1m_boot.rs",
    "crates/app/src/dhan_cadence_executor.rs",
    "crates/app/src/cadence_boot.rs",
];

fn a_producer_module_exists() -> Option<String> {
    PRODUCER_MODULES
        .iter()
        .find(|rel| workspace_root().join(rel).is_file())
        .map(|rel| (*rel).to_string())
}

#[test]
fn the_boot_report_capture_claim_matches_whether_a_leg_actually_exists() {
    let main_rs = production_region(&read_source("crates/app/src/main.rs"));

    // ---- what the code claims -------------------------------------------
    let declared_false = main_rs.contains("const DHAN_PER_MINUTE_LEGS_EXIST: bool = false;");
    let declared_true = main_rs.contains("const DHAN_PER_MINUTE_LEGS_EXIST: bool = true;");
    assert!(
        declared_false || declared_true,
        "crates/app/src/main.rs no longer declares \
         `const DHAN_PER_MINUTE_LEGS_EXIST: bool = …` in its production region. \
         That const is what keeps the boot Telegram's capture line honest — it is \
         read for BOTH `spot_1m_enabled` and `chain_1m_enabled`. If the per-minute \
         Dhan REST legs genuinely came back, restore the const as `true` and this \
         guard will agree. Do not delete the const to make this pass: config \
         cannot answer the question, because a flag cannot arm a module that is \
         not compiled."
    );

    // ---- what is actually true ------------------------------------------
    let producer = a_producer_module_exists();

    match (&producer, declared_true) {
        (Some(rel), false) => panic!(
            "a per-minute Dhan REST producer is BACK on disk ({rel}) but \
             DHAN_PER_MINUTE_LEGS_EXIST is still `false`, so the boot Telegram \
             reports capture switched off while a leg is running. Flip the const \
             to `true` in crates/app/src/main.rs — and re-read the (true, *) arms \
             in crates/core/src/notification/events.rs before you do: their text \
             still carries \"(Groww per-minute legs report separately)\", which has \
             been false since the 2026-08-21 Groww removal and would go back out \
             on the operator's phone the moment one of those arms renders again."
        ),
        (None, true) => panic!(
            "DHAN_PER_MINUTE_LEGS_EXIST is `true` but NONE of the producing \
             modules exists:\n  {}\n\n\
             That is exactly the state this guard was written for: the boot \
             Telegram would send \"✅ Dhan per-minute price capture — armed\" to \
             the operator's phone, every trading morning, for legs that were \
             deleted. Set it back to `false`, or restore a real producer.",
            PRODUCER_MODULES.join("\n  ")
        ),
        _ => {}
    }

    // ---- and the boot report must READ the const, not config -------------
    for field in ["spot_1m_enabled", "chain_1m_enabled"] {
        let needle = format!("{field}: DHAN_PER_MINUTE_LEGS_EXIST");
        assert!(
            main_rs.contains(&needle),
            "the boot report's `{field}` is no longer fed by \
             DHAN_PER_MINUTE_LEGS_EXIST. If it went back to reading config \
             (`config.spot_1m_rest.enabled`, `config.cadence.enabled && \
             config.cadence.dhan_lane`), the claim is wrong again the moment a \
             flag says `true` for a deleted leg — which is precisely how the \
             false page survived the 2026-09-16 removal. `[cadence] enabled` and \
             `dhan_lane` BOTH still read `true` in config/base.toml today."
        );
    }

    // ---- parser self-check ----------------------------------------------
    //
    // Without this, a `main.rs` that stopped mentioning the fields at all would
    // satisfy nothing and yet fail nothing useful — the vacuous-guard shape this
    // branch has already found three times.
    assert!(
        main_rs.contains("NotificationEvent::StartupComplete"),
        "parser self-check FAILED: crates/app/src/main.rs's production region no \
         longer mentions NotificationEvent::StartupComplete, so this guard is \
         asserting against a file that no longer sends the boot report. Re-anchor \
         it on wherever the boot Telegram moved to — do not delete it."
    );
}

#[test]
fn the_rendered_capture_line_does_not_claim_a_removed_feed_reports_separately() {
    let events = production_region(&read_source("crates/core/src/notification/events.rs"));

    // The ONLY arm that can render today is (false, false). Its text must not
    // point the operator at the Groww per-minute legs, removed 2026-08-21.
    let at = events.find("(false, false) =>").unwrap_or_else(|| {
        panic!(
            "crates/core/src/notification/events.rs no longer has a `(false, false)` \
             arm on the boot-report capture match. That is the only REACHABLE arm \
             (main.rs constant-folds both operands), so it is the text the operator \
             actually receives. Re-anchor this guard on its replacement."
        )
    });
    // Bound the window to the arm itself.
    let arm = &events[at..];
    let arm = &arm[..arm.find(".to_string(),").map_or(arm.len(), |e| e + 13)];

    assert!(
        !arm.contains("Groww per-minute"),
        "the REACHABLE boot-report capture arm tells the operator that \"Groww \
         per-minute legs report separately\". The entire Groww feed was REMOVED on \
         2026-08-21 (websocket-connection-scope-lock.md, the THIRD quote of that \
         day) — there are no Groww per-minute legs and nothing reports separately. \
         This line goes to the operator's phone on every boot, so a stale broker \
         name here is not a comment typo; it sends him looking for alerts that can \
         never arrive.\n\nThe arm as written:\n{arm}"
    );

    assert!(
        !arm.contains("switched off"),
        "the REACHABLE boot-report capture arm says the Dhan per-minute capture is \
         \"switched off\". It is not switched off — it is REMOVED: \
         spot_1m_rest_boot.rs, option_chain_1m_boot.rs, dhan_cadence_executor.rs \
         and cadence_boot.rs are all deleted, and so is crates/core/src/cadence/. \
         \"Switched off\" invites the operator to go and switch it on, and there is \
         no switch that can do anything.\n\nThe arm as written:\n{arm}"
    );
}
