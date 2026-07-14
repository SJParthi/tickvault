//! Source-scan ratchet — Groww per-minute PER-CONTRACT candle leg wired
//! in main.rs (operator grant 2026-07-13, PR-4 of the Groww per-minute
//! REST plan — the fill-model leg).
//!
//! Pins (house pattern: `groww_chain_1m_wiring_guard.rs`, whose dual-arm
//! pins this guard BUILDS ON — the contract leg is spawned INSIDE the
//! same `spawn_groww_spot_1m_leg` helper the spot guard already pins to
//! BOTH boot arms, so one helper carries all three legs and the arms can
//! never drift):
//! 1. main.rs has exactly ONE `spawn_supervised_groww_contract_1m(` call
//!    site, inside the `spawn_groww_spot_1m_leg` helper body, gated on
//!    `contract_enabled` = `config.groww_contract_1m.enabled &&
//!    chain_enabled` (the leg DEPENDS on the chain anchors — enabled
//!    without the chain leg is refused with a loud warn, never a silent
//!    anchor-less loop).
//! 2. The chain→contract sequencing watch channel + the anchor store are
//!    created ONLY when the contract leg is enabled (contract-off keeps
//!    the chain leg byte-identical to PR-3), and the chain boot module
//!    PUBLISHES the minute-done signal unconditionally after every fire +
//!    updates the anchor store on every Found underlying.
//! 3. Stub-guard: `groww_contract_1m_boot.rs` really does the work —
//!    resolves the contract book from the instruments master, selects the
//!    capped ATM window from the chain anchors, hits the Groww candles
//!    endpoint (`segment=FNO`), reads the shared-minter token READ-ONLY,
//!    persists via the new `option_contract_1m_rest` writer, and emits
//!    the per-fetch forensics rows (no skeleton PR — Rule 14).
//! 4. No `unwrap(`/`expect(`/`println!` in the module's PRODUCTION region.
//! 5. The strike PARSE-ONLY invariant holds for the contract module too:
//!    persisted strikes come ONLY from the master `groww_symbol` parse —
//!    never computed arithmetically (ATM-DISTANCE math is allowed and
//!    lives on `anchor`/`dist` locals, never on a `strike` binding).
//!
//! Runbook: `.claude/rules/project/rest-1m-pipeline-error-codes.md`.

use std::fs;
use std::path::PathBuf;

fn read_app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const HELPER_DEF: &str = "fn spawn_groww_spot_1m_leg(";
const CONTRACT_SPAWN_CALL: &str = "spawn_supervised_groww_contract_1m(";
// The REAL gate: the `if` itself plus the binding assertion below (the
// chain guard's NIT-6 lesson — a dead binding must not satisfy the pin).
const CONTRACT_GATE: &str = "if contract_enabled {";
const CONTRACT_GATE_BINDING: &str =
    "let contract_enabled = config.groww_contract_1m.enabled && chain_enabled;";

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
fn ratchet_groww_contract1m_spawn_is_config_gated_inside_the_dual_arm_helper() {
    let main_src = read_app_src("src/main.rs");
    let (def_pos, end) = helper_span(&main_src);

    // Exactly ONE contract spawn call site, inside the dual-arm helper
    // (the spot guard already pins the helper to BOTH boot arms — so the
    // contract leg inherits the crash-recovery coverage for free).
    let spawn_positions: Vec<usize> = main_src
        .match_indices(CONTRACT_SPAWN_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        spawn_positions.len(),
        1,
        "expected exactly 1 `{CONTRACT_SPAWN_CALL}` call site in main.rs \
         (inside spawn_groww_spot_1m_leg); found {spawn_positions:?}"
    );
    assert!(
        spawn_positions[0] > def_pos && spawn_positions[0] < end,
        "the contract spawn at byte {} must live inside the \
         spawn_groww_spot_1m_leg helper (bytes {def_pos}..{end}) so both \
         boot arms carry it",
        spawn_positions[0]
    );

    // The gate lives inside the helper, before the spawn site — and the
    // binding really IS the config-AND-chain read (the leg depends on the
    // chain anchors by construction).
    assert!(
        main_src
            .match_indices(CONTRACT_GATE)
            .any(|(pos, _)| pos > def_pos && pos < spawn_positions[0]),
        "the `{CONTRACT_GATE}` gate must sit inside the helper before the \
         contract spawn site"
    );
    assert!(
        main_src
            .match_indices(CONTRACT_GATE_BINDING)
            .any(|(pos, _)| pos > def_pos && pos < spawn_positions[0]),
        "the `{CONTRACT_GATE_BINDING}` binding must sit inside the helper \
         before the contract spawn site (the contract leg REQUIRES the \
         chain leg — its anchors are the ATM selection input)"
    );
    // Enabled-without-chain is refused LOUDLY (never a silent anchor-less
    // loop): the helper carries the refusal warn.
    let helper_body = &main_src[def_pos..end];
    assert!(
        helper_body.contains("config.groww_contract_1m.enabled && !chain_enabled"),
        "the helper must refuse contract-enabled-without-chain with a loud warn"
    );
}

/// Sequencing + anchor-handoff pin: the chain→contract watch channel and
/// the anchor store exist ONLY when the contract leg is on, the chain
/// boot module publishes the minute-done signal UNCONDITIONALLY after
/// every fire (success OR failure — the contract leg must never block on
/// a failing chain leg), and the chain fire updates the anchor store on
/// every Found underlying.
#[test]
fn ratchet_groww_contract1m_sequencing_channel_and_chain_publish() {
    let main_src = read_app_src("src/main.rs");
    let (def_pos, end) = helper_span(&main_src);
    let helper_body = &main_src[def_pos..end];
    assert!(
        helper_body.contains("if contract_enabled {")
            && helper_body.contains("chain_minute_done_tx")
            && helper_body.contains("chain_minute_done_rx")
            && helper_body.contains("contract_anchor_store"),
        "the chain→contract watch channel + anchor store must be created in \
         the helper, gated on contract_enabled"
    );
    assert!(
        helper_body.contains("GrowwChainAnchorStore"),
        "the anchor store must be the chain module's shared type"
    );

    // The chain boot module publishes unconditionally at the end of every
    // fire (the spot→chain precedent) + updates the anchors in the Found
    // arm BEFORE the publish.
    let chain_src = read_app_src("src/groww_option_chain_1m_boot.rs");
    assert!(
        chain_src.contains("tx.send_replace(Some(fire))"),
        "groww_option_chain_1m_boot.rs must publish the minute-done signal \
         after every fire via send_replace"
    );
    let publish_pos = chain_src
        .find("tx.send_replace(Some(fire))")
        .expect("publish site exists");
    let fire_call_pos = chain_src
        .find("fire_one_groww_chain_minute(")
        .expect("fire call exists");
    assert!(
        publish_pos > fire_call_pos,
        "the publish must come AFTER the chain fire (chain first, contract \
         right after)"
    );
    assert!(
        chain_src.contains("anchor_store") && chain_src.contains("underlying_ltp_missing"),
        "the chain fire must hand the contract leg its ATM anchors (only a \
         REAL vendor LTP updates the store)"
    );
}

/// Stub-guard (Rule 14 — no skeleton): the boot module must actually
/// resolve books, select, fetch, parse, persist, audit and page — not
/// just define pub fns.
#[test]
fn ratchet_groww_contract1m_boot_module_is_not_a_stub() {
    let module_src = read_app_src("src/groww_contract_1m_boot.rs");
    for needle in [
        // The documented Groww candles endpoint + headers + FNO segment.
        "GROWW_HISTORICAL_CANDLES_URL",
        "GROWW_API_VERSION_HEADER",
        "\"FNO\"",
        // Contract book from the instruments master — never guessed.
        "download_master_bounded",
        "select_current_option_expiry",
        "select_option_contract_rows",
        "parse_contract_symbol",
        // Selection: chain anchors → capped deterministic ATM window.
        "GrowwChainAnchorStore",
        "select_contracts_for_minute",
        "GROWW_CONTRACT_1M_MAX_PER_MINUTE",
        "selection_truncated",
        "selection_unresolved",
        // The shared-minter token is read READ-ONLY (contract-routed cache).
        "GrowwTokenCache::new_contract",
        // Sequencing: the reused wait core + the contract fallback const.
        "wait_for_signal_or_fallback",
        "GROWW_CONTRACT_1M_FALLBACK_DELAY_MS",
        // Defensive pacing + bounded budgets (no unbounded retry).
        "contract_min_gap_wait_ms",
        "GROWW_CONTRACT_1M_FIRE_BUDGET_SECS",
        // Persist through the new table's writer; the shared parser's oi.
        "OptionContract1mRestWriter",
        "parse_groww_1m_candle_rows",
        "writer.flush()",
        // The scheduler sleeps to computed minute boundaries with the
        // H1/H2 invariants.
        "next_fire_after(",
        "count_missed_boundaries(",
        "tv_groww_contract1m_boundary_skipped_total",
        // Per-fetch forensics rows (leg='contract_1m') — success AND failure.
        "RestFetchAuditWriter",
        "REST_FETCH_LEG_CONTRACT_1M",
        "build_contract_audit_row(",
        // Edge-triggered escalation + recovery + book-degrade events.
        "GrowwContract1mFetchDegraded",
        "GrowwContract1mFetchRecovered",
        "GrowwContract1mBookUnresolved",
        // The measured freshness signal (Quote 2) + the 429 shape capture.
        "tv_groww_contract1m_close_to_data_ms",
        "tv_groww_contract1m_rate_limited_total",
    ] {
        assert!(
            module_src.contains(needle),
            "groww_contract_1m_boot.rs lost its `{needle}` wiring — the \
             module must resolve books, select, fetch, parse, persist, \
             audit and page for real (Rule 14: no skeleton PRs)"
        );
    }
}

/// No `unwrap(` / `expect(` / `println!` in the module's PRODUCTION
/// region (the compile-time lints deny them too — this pins the split so
/// a future `#[allow]` can't sneak one in silently).
#[test]
fn ratchet_groww_contract1m_module_production_region_is_panic_free() {
    let module_src = read_app_src("src/groww_contract_1m_boot.rs");
    let production = module_src
        .split("#[cfg(test)]")
        .next()
        .unwrap_or(&module_src);
    for banned in [".unwrap(", ".expect(", "println!", "dbg!("] {
        assert!(
            !production.contains(banned),
            "groww_contract_1m_boot.rs production region contains \
             `{banned}` — every fault path must degrade typed + coded, \
             never panic/print"
        );
    }
}

/// The strike PARSE-ONLY invariant, extended to the contract module: a
/// persisted strike comes ONLY from the master `groww_symbol` parse —
/// never computed arithmetically. ATM-distance math is legitimate but
/// must live on `dist`/`anchor` locals; a derived STRIKE value would mint
/// bit-different f64s the cross-table joins (option_chain_1m ↔
/// option_contract_1m_rest) silently miss.
#[test]
fn ratchet_groww_contract1m_strike_is_parse_only_never_computed() {
    let module_src = read_app_src("src/groww_contract_1m_boot.rs");
    // The single strike-producing site: the symbol-segment parse.
    assert!(
        module_src.contains("let strike: f64 = rev.next()?.trim().parse().ok()?;"),
        "groww_contract_1m_boot.rs lost the parse-only strike site — \
         strikes must come ONLY from the master groww_symbol parse"
    );
    // No arithmetic PRODUCING a strike anywhere in the PRODUCTION region
    // (split at `#[cfg(test)]`; comment lines stripped). The ATM-distance
    // subtraction reads `slot.strike - anchor` into a `dist` local — the
    // needles below target assignments/derivations onto strike values.
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
        "strike.mul",
        "strike.add",
        "strike.sub",
        "strike.div",
        "strike.round",
        "strike.floor",
        "strike.ceil",
        "strike = ",
    ] {
        assert!(
            !code.contains(needle),
            "groww_contract_1m_boot.rs contains `{needle}` — \
             computing/deriving strikes breaks the parse-only invariant \
             (move the key to a paise LONG first)"
        );
    }
}
