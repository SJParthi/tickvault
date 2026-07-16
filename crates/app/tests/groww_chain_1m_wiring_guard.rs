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
//! 2. 2026-07-14 auto-ladder (operator approval): the spot→chain
//!    sequencing channel is RETIRED — the chain leg fires on its OWN
//!    minute-boundary timer; the helper creates ONE shared
//!    `GrowwRestBurstState` and threads it into every Groww REST leg.
//!    The chain→contract minute-done publish is KEPT unchanged.
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
// The REAL branch (hostile-round-1 NIT-6): a dead `let chain_enabled = ...`
// binding anywhere before an UNCONDITIONAL spawn must not satisfy the gate
// pin — the needle is the `if` itself, plus the binding assertion below.
const CHAIN_GATE: &str = "if chain_enabled {";
const CHAIN_GATE_BINDING: &str = "let chain_enabled = config.groww_option_chain_1m.enabled;";
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
    // ... and `chain_enabled` must really BE the config read (NIT-6: the
    // `if chain_enabled {` pin alone could gate on a repurposed flag).
    assert!(
        main_src
            .match_indices(CHAIN_GATE_BINDING)
            .any(|(pos, _)| pos > def_pos && pos < spawn_positions[0]),
        "the `{CHAIN_GATE_BINDING}` binding must sit inside the helper \
         before the chain spawn site"
    );
}

/// 2026-07-14 auto-ladder pin: the spot→chain sequencing channel is
/// RETIRED (each Groww REST leg fires on its OWN minute-boundary timer);
/// the helper creates ONE shared burst state and threads it into every
/// Groww REST leg; the chain→contract minute-done publish is KEPT.
#[test]
fn ratchet_groww_chain1m_auto_ladder_own_timer_and_burst_wiring() {
    let main_src = read_app_src("src/main.rs");
    let (def_pos, end) = helper_span(&main_src);
    let helper_body = &main_src[def_pos..end];

    // The retired spot→chain channel gate must NOT come back — a re-added
    // sequencing wait would silently re-impose the ~2.5s chain delay the
    // auto-ladder deleted.
    assert!(
        !helper_body.contains("config.groww_spot_1m.enabled && chain_enabled"),
        "the retired spot→chain watch-channel gate reappeared in \
         spawn_groww_spot_1m_leg — the 2026-07-14 auto-ladder fires each \
         leg on its OWN timer (no spot→chain sequencing channel)"
    );
    // ONE shared burst state, built from the config section, threaded into
    // the legs (spot + chain + contract + probe all take `burst:`).
    assert!(
        helper_body.contains("GrowwRestBurstState::new("),
        "the helper must build the shared GrowwRestBurstState from \
         [groww_rest_burst] config"
    );
    assert!(
        helper_body.contains("config.groww_rest_burst.tier")
            && helper_body.contains("config.groww_rest_burst.warm_up"),
        "the burst state must be built from the [groww_rest_burst] config \
         fields (tier + warm_up)"
    );
    assert!(
        helper_body
            .matches("burst: std::sync::Arc::clone(&burst)")
            .count()
            >= 3,
        "the shared burst handle must flow into the spot, chain and \
         contract params (plus the probe path)"
    );
    // The chain→contract sequencing channel is KEPT (the contract leg's
    // selection depends on the chain's per-minute anchors).
    assert!(
        helper_body.contains("chain_minute_done_tx"),
        "the chain→contract minute-done channel must still be created in \
         the helper (contract sequencing is unchanged)"
    );

    // The chain boot module still publishes the chain→contract signal
    // unconditionally AFTER every fire.
    let chain_src = read_app_src("src/groww_option_chain_1m_boot.rs");
    let publish_pos = chain_src
        .find("tx.send_replace(Some(fire))")
        .expect("the chain→contract publish site must exist");
    let fire_call_pos = chain_src
        .find("fire_one_groww_chain_minute(")
        .expect("fire call exists");
    assert!(
        publish_pos > fire_call_pos,
        "the chain→contract publish must come AFTER the chain fire"
    );

    // The spot leg no longer publishes any minute-done signal (its
    // downstream consumer — the chain wait — is retired).
    let spot_src = read_app_src("src/groww_spot_1m_boot.rs");
    assert!(
        !spot_src.contains("send_replace"),
        "groww_spot_1m_boot.rs must NOT publish a minute-done signal — the \
         spot→chain sequencing is retired (2026-07-14 auto-ladder)"
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
        // 2026-07-14 auto-ladder: the chain fires on its OWN minute
        // boundary + a concurrent underlying wave; a 429 demotes the
        // shared session tier (edge-latched).
        "GROWW_CHAIN_1M_FIRE_DELAY_MS",
        "intra_wave_stagger_ms",
        "tv_groww_rest_burst_tier_total",
        "note_rate_limited",
        "burst_demoted",
        // Bounded per-underlying budget (no unbounded retry).
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
