//! Source-scan ratchet — Groww per-minute option-chain leg wired in
//! main.rs (operator grant 2026-07-13, PR-3 of the Groww per-minute REST
//! plan).
//!
//! Pins (house pattern: `groww_spot_1m_wiring_guard.rs`, whose dual-arm
//! pins this guard BUILDS ON — the chain leg is spawned INSIDE the same
//! `spawn_groww_spot_1m_leg` helper the spot guard already pins to BOTH
//! boot arms, so one helper carries both legs and the arms can never
//! drift):
//! 1. main.rs has exactly ONE `spawn_supervised_groww_chain_1m(` call
//!    site + exactly ONE `run_groww_chain_1m_probe(` spawn site, BOTH
//!    inside the `spawn_groww_spot_1m_leg` helper body, gated on
//!    `config.groww_option_chain_1m.enabled` / `.probe_and_report`
//!    (fail-safe: absent section = pipeline off + probe on).
//! 2. The spot→chain sequencing watch channel is created ONLY when BOTH
//!    legs are enabled (chain-off keeps the spot leg byte-identical to
//!    PR-2), and the Groww spot boot module PUBLISHES the minute-done
//!    signal unconditionally after every fire.
//! 3. Stub-guard: `groww_option_chain_1m_boot.rs` really does the work —
//!    hits the Groww chain endpoint, resolves expiries from the
//!    instruments master (never an expiry REST endpoint), reads the
//!    shared-minter token READ-ONLY, persists via the feed-parameterized
//!    writer WITH the rho + close_to_data_ms extension, and emits the
//!    per-fetch forensics rows (no skeleton PR — audit-findings Rule 14).
//! 4. No `unwrap(`/`expect(`/`println!` in the module's PRODUCTION region.
//! 5. The parse-only strike invariant (the option_chain_1m float DEDUP
//!    key's safety condition) holds for the Groww module too.
//!
//! Runbook: `.claude/rules/project/rest-1m-pipeline-error-codes.md`.

use std::fs;
use std::path::PathBuf;

fn read_app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const HELPER_DEF: &str = "fn spawn_groww_spot_1m_leg(";
const CHAIN_SPAWN_CALL: &str = "spawn_supervised_groww_chain_1m(";
const PROBE_CALL: &str = "run_groww_chain_1m_probe(";
const CHAIN_GATE: &str = "config.groww_option_chain_1m.enabled";
const PROBE_GATE: &str = "config.groww_option_chain_1m.probe_and_report";

/// The helper body span in main.rs: from the definition to the next
/// top-level `fn `.
fn helper_span(main_src: &str) -> (usize, usize) {
    let def_pos = main_src
        .find(HELPER_DEF)
        .expect("spawn_groww_spot_1m_leg must exist in main.rs");
    let end = main_src[def_pos + HELPER_DEF.len()..]
        .find("\nfn ")
        .map_or(main_src.len(), |off| def_pos + HELPER_DEF.len() + off);
    (def_pos, end)
}

#[test]
fn ratchet_groww_chain1m_spawn_and_probe_are_config_gated_inside_the_dual_arm_helper() {
    let main_src = read_app_src("src/main.rs");
    let (def_pos, end) = helper_span(&main_src);

    // Exactly ONE chain spawn call site, inside the dual-arm helper (the
    // spot guard already pins the helper to BOTH boot arms — so the chain
    // leg inherits the crash-recovery coverage for free and can never
    // drift from it).
    let spawn_positions: Vec<usize> = main_src
        .match_indices(CHAIN_SPAWN_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        spawn_positions.len(),
        1,
        "expected exactly 1 `{CHAIN_SPAWN_CALL}` call site in main.rs \
         (inside spawn_groww_spot_1m_leg); found {spawn_positions:?}"
    );
    assert!(
        spawn_positions[0] > def_pos && spawn_positions[0] < end,
        "the chain spawn at byte {} must live inside the \
         spawn_groww_spot_1m_leg helper (bytes {def_pos}..{end}) so both \
         boot arms carry it",
        spawn_positions[0]
    );

    // Exactly ONE probe spawn site, also inside the helper.
    let probe_positions: Vec<usize> = main_src
        .match_indices(PROBE_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        probe_positions.len(),
        1,
        "expected exactly 1 `{PROBE_CALL}` spawn site in main.rs; found {probe_positions:?}"
    );
    assert!(
        probe_positions[0] > def_pos && probe_positions[0] < end,
        "the probe spawn must live inside spawn_groww_spot_1m_leg"
    );

    // Both gates live inside the helper, before their spawn sites.
    for (gate, site) in [
        (CHAIN_GATE, spawn_positions[0]),
        (PROBE_GATE, probe_positions[0]),
    ] {
        assert!(
            main_src
                .match_indices(gate)
                .any(|(pos, _)| pos > def_pos && pos < site),
            "the `{gate}` gate must sit inside the helper before its spawn \
             site at byte {site}"
        );
    }
}

/// Sequencing pin: the watch channel exists ONLY when both legs are on,
/// and the Groww spot leg publishes the minute-done signal
/// UNCONDITIONALLY after every fire (success OR failure — the chain must
/// never block on a failing spot leg; the Dhan seam's exact semantics).
#[test]
fn ratchet_groww_chain1m_sequencing_channel_and_spot_publish() {
    let main_src = read_app_src("src/main.rs");
    let (def_pos, end) = helper_span(&main_src);
    let helper_body = &main_src[def_pos..end];
    assert!(
        helper_body.contains("config.groww_spot_1m.enabled && chain_enabled"),
        "the watch channel must be created ONLY when BOTH Groww REST legs \
         are enabled (chain-off keeps the spot leg byte-identical to PR-2)"
    );
    assert!(
        helper_body.contains("tokio::sync::watch::channel::<Option<u32>>(None)"),
        "the spot→chain minute-done watch channel must be created in the helper"
    );
    assert!(
        helper_body.contains("minute_done_tx") && helper_body.contains("spot_minute_done"),
        "the sender must flow into the spot params and the receiver into \
         the chain params"
    );

    // The spot boot module publishes unconditionally at the end of every
    // fire (the option_chain_1m_wiring_guard precedent on the Dhan side).
    let spot_src = read_app_src("src/groww_spot_1m_boot.rs");
    assert!(
        spot_src.contains("tx.send_replace(Some(fire))"),
        "groww_spot_1m_boot.rs must publish the minute-done signal after \
         every fire via send_replace"
    );
    let publish_pos = spot_src
        .find("tx.send_replace(Some(fire))")
        .expect("publish site exists");
    let fire_call_pos = spot_src
        .find("fire_one_minute(")
        .expect("fire_one_minute call exists");
    assert!(
        publish_pos > fire_call_pos,
        "the publish must come AFTER the fire (spot first, chain right after)"
    );
}

/// Stub-guard (Rule 14 — no skeleton): the boot module must actually
/// resolve expiries, fetch, parse, persist, audit and page — not just
/// define pub fns.
#[test]
fn ratchet_groww_chain1m_boot_module_is_not_a_stub() {
    let module_src = read_app_src("src/groww_option_chain_1m_boot.rs");
    for needle in [
        // The documented Groww chain endpoint + headers.
        "GROWW_OPTION_CHAIN_URL_PREFIX",
        "GROWW_API_VERSION_HEADER",
        "expiry_date",
        // Expiry from the instruments master — never an expiry REST
        // endpoint, never guessed.
        "download_groww_master_rows",
        "select_current_option_expiry",
        // The shared-minter token is read READ-ONLY (chain-routed cache).
        "GrowwTokenCache::new_chain",
        // Sequencing: the reused Dhan wait core + the Groww fallback const.
        "wait_for_signal_or_fallback",
        "GROWW_CHAIN_1M_FALLBACK_DELAY_MS",
        // Defensive pacing + bounded budget (no unbounded retry).
        "groww_min_gap_wait_ms",
        "GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS",
        // Persist through the feed-parameterized writer WITH the
        // rho/close_to_data_ms extension; the live-lane id is reused.
        "OptionChain1mWriter::new_with_feed",
        "OPTION_CHAIN_1M_FEED_GROWW",
        "append_row_ext",
        "stable_index_security_id",
        "writer.flush()",
        // The scheduler sleeps to computed minute boundaries with the
        // H1/H2 invariants.
        "next_fire_after(",
        "count_missed_boundaries(",
        "tv_groww_chain1m_boundary_skipped_total",
        // Per-fetch forensics rows (leg='chain_1m') — success AND failure.
        "RestFetchAuditWriter",
        "REST_FETCH_LEG_CHAIN_1M",
        "build_chain_audit_row(",
        // Edge-triggered escalation + recovery events are wired.
        "GrowwChain1mFetchDegraded",
        "GrowwChain1mFetchRecovered",
        "GrowwChain1mExpiryUnresolved",
        "GrowwChain1mProbeVerdict",
        // The streamed body cap (security §18 pattern).
        "GROWW_CHAIN_1M_MAX_BODY_BYTES",
        // The measured freshness signal (NO response timestamp exists) +
        // the live-probe (d) chain-size numbers.
        "tv_groww_chain1m_close_to_data_ms",
        "tv_groww_chain1m_payload_bytes",
        "tv_groww_chain1m_strikes_per_chain",
        // The live-probe (e) 429 shape capture.
        "tv_groww_chain1m_rate_limited_total",
    ] {
        assert!(
            module_src.contains(needle),
            "groww_option_chain_1m_boot.rs lost its `{needle}` wiring — the \
             module must resolve expiries, fetch, parse, persist, audit and \
             page for real (Rule 14: no skeleton PRs)"
        );
    }
}

/// No `unwrap(` / `expect(` / `println!` in the module's PRODUCTION
/// region (the compile-time lints deny them too — this pins the split so
/// a future `#[allow]` can't sneak one in silently).
#[test]
fn ratchet_groww_chain1m_module_production_region_is_panic_free() {
    let module_src = read_app_src("src/groww_option_chain_1m_boot.rs");
    let production = module_src
        .split("#[cfg(test)]")
        .next()
        .unwrap_or(&module_src);
    for banned in [".unwrap(", ".expect(", "println!", "dbg!("] {
        assert!(
            !production.contains(banned),
            "groww_option_chain_1m_boot.rs production region contains \
             `{banned}` — every fault path must degrade typed + coded, \
             never panic/print"
        );
    }
}

/// The option_chain_1m float DEDUP-key safety invariant, extended to the
/// Groww writer (the Dhan-side `ratchet_chain1m_strike_is_parse_only_
/// never_computed` precedent): strikes come ONLY from the response map's
/// string keys — never computed arithmetically. A derived strike would
/// mint bit-different f64 keys and silently duplicate rows.
#[test]
fn ratchet_groww_chain1m_strike_is_parse_only_never_computed() {
    let module_src = read_app_src("src/groww_option_chain_1m_boot.rs");
    // The single strike-producing site: the string-key parse.
    assert!(
        module_src.contains("strike_key.trim().parse::<f64>()"),
        "groww_option_chain_1m_boot.rs lost the parse-only strike site — \
         strikes must come ONLY from the response's string keys"
    );
    // No arithmetic on strikes anywhere in the PRODUCTION region (split at
    // `#[cfg(test)]` — the house pattern: the module's own unit-test
    // float-compare assertions like `(ce.strike - X).abs()` must never
    // false-positive); comment lines are stripped so doc-comment prose
    // never matches.
    let production_region = module_src
        .split("#[cfg(test)]")
        .next()
        .unwrap_or(module_src.as_str());
    let code: String = production_region
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "strike *",
        "strike +",
        "strike /",
        "strike -",
        "* strike",
        "+ strike",
        "/ strike",
        "strike.mul",
        "strike.add",
        "strike.sub",
        "strike.div",
        "strike.round",
        "strike.floor",
        "strike.ceil",
    ] {
        assert!(
            !code.contains(needle),
            "groww_option_chain_1m_boot.rs contains `{needle}` — \
             computing/deriving strikes breaks the parse-only invariant the \
             float DEDUP key depends on (move the key to a paise LONG first)"
        );
    }
}
