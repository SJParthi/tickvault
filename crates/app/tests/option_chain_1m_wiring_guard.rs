//! Source-scan ratchet — per-minute option-chain REST pipeline wired into
//! the shared post-market spawn seam (operator grant 2026-07-12, PR-3).
//!
//! Pins (house pattern: `spot_1m_rest_wiring_guard.rs`):
//! 1. main.rs carries exactly ONE `spawn_supervised_option_chain_1m(` call
//!    site + exactly ONE `run_option_chain_1m_probe(` spawn, BOTH inside
//!    `spawn_post_market_tasks` (the shared post-market seam; the rest_canary itself was retired 2026-07-14 — invoked from BOTH
//!    boot paths, once-guarded), gated on `config.option_chain_1m.enabled`
//!    (pipeline) / `probe_and_report` (probe-only arm) — the DEFAULT-OFF
//!    entitlement contract: the pipeline NEVER auto-runs while disabled.
//! 2. The spot→chain sequencing watch channel is plumbed: the spot params
//!    carry `minute_done_tx` and the chain params carry
//!    `spot_minute_done`, from ONE `tokio::sync::watch::channel` created
//!    only when both halves are enabled.
//! 3. Stub-guard: `option_chain_1m_boot.rs` really does the work — the
//!    expirylist warmup + chain fetch via the shared path constants +
//!    join_api_url, the extra `client-id` header, per-fire token re-load,
//!    the fallback-bounded spot-sequencing wait, persistence through the
//!    storage writer, and the edge-triggered escalation events (no
//!    skeleton PR — audit-findings Rule 14).
//!
//! Runbook: `.claude/rules/project/rest-1m-pipeline-error-codes.md`.

use std::fs;
use std::path::PathBuf;

fn read_app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `(` suffix distinguishes CALL sites from doc-comment mentions; the
/// fn DEFINITIONS live in option_chain_1m_boot.rs, not main.rs, so every
/// main.rs hit is a call site.
const SPAWN_CALL: &str = "spawn_supervised_option_chain_1m(";
const PROBE_CALL: &str = "run_option_chain_1m_probe(";
const PIPELINE_GATE: &str = "config.option_chain_1m.enabled";
const PROBE_GATE: &str = "config.option_chain_1m.probe_and_report";
const POST_MARKET_FN: &str = "fn spawn_post_market_tasks(";

/// Byte span of `spawn_post_market_tasks` in main.rs (start..next top-level
/// fn) — the seam both arms must live inside.
fn post_market_span(main_src: &str) -> (usize, usize) {
    let seam_pos = main_src
        .find(POST_MARKET_FN)
        .expect("spawn_post_market_tasks must exist in main.rs");
    let seam_end = main_src[seam_pos + POST_MARKET_FN.len()..]
        .find("\nfn ")
        .map_or(main_src.len(), |off| seam_pos + POST_MARKET_FN.len() + off);
    (seam_pos, seam_end)
}

#[test]
fn ratchet_chain1m_spawn_and_probe_are_config_gated_inside_post_market_seam() {
    let main_src = read_app_src("src/main.rs");
    let (seam_pos, seam_end) = post_market_span(&main_src);

    // Exactly ONE pipeline spawn site, inside the seam.
    let spawn_positions: Vec<usize> = main_src
        .match_indices(SPAWN_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        spawn_positions.len(),
        1,
        "expected exactly 1 `{SPAWN_CALL}` call site in main.rs (the shared \
         spawn_post_market_tasks seam covers BOTH boot paths + the \
         once-guard); found {} — a second site would double-spawn the \
         per-minute chain fetcher",
        spawn_positions.len()
    );
    let spawn_pos = spawn_positions[0];
    assert!(
        spawn_pos > seam_pos && spawn_pos < seam_end,
        "the chain spawn at byte {spawn_pos} must live INSIDE \
         spawn_post_market_tasks (fn spans bytes {seam_pos}..{seam_end})"
    );

    // Exactly ONE probe-only spawn site, inside the seam.
    let probe_positions: Vec<usize> = main_src
        .match_indices(PROBE_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        probe_positions.len(),
        1,
        "expected exactly 1 `{PROBE_CALL}` spawn in main.rs (the probe-only \
         arm of the DEFAULT-OFF contract); found {}",
        probe_positions.len()
    );
    let probe_pos = probe_positions[0];
    assert!(
        probe_pos > seam_pos && probe_pos < seam_end,
        "the probe spawn at byte {probe_pos} must live INSIDE \
         spawn_post_market_tasks (fn spans bytes {seam_pos}..{seam_end})"
    );

    // The pipeline gate precedes the pipeline spawn; the probe gate
    // precedes the probe spawn; and the pipeline gate comes FIRST (the
    // probe arm is the `else if` of the SAME decision — the pipeline can
    // never run alongside the probe).
    let gate_pos = main_src
        .find(PIPELINE_GATE)
        .expect("main.rs must gate the chain spawn on the enabled flag");
    // Find the gate INSIDE the seam nearest before the spawn.
    let gate_positions: Vec<usize> = main_src
        .match_indices(PIPELINE_GATE)
        .map(|(pos, _)| pos)
        .collect();
    assert!(
        gate_positions
            .iter()
            .any(|g| *g > seam_pos && *g < spawn_pos && spawn_pos - g < 2_048),
        "the `{PIPELINE_GATE}` gate must immediately precede the chain \
         spawn inside spawn_post_market_tasks (gates at {gate_positions:?}, \
         spawn at {spawn_pos}, first gate {gate_pos})"
    );
    let probe_gate_positions: Vec<usize> = main_src
        .match_indices(PROBE_GATE)
        .map(|(pos, _)| pos)
        .collect();
    assert!(
        probe_gate_positions
            .iter()
            .any(|g| *g > seam_pos && *g < probe_pos && probe_pos - g < 2_048),
        "the `{PROBE_GATE}` gate must immediately precede the probe spawn \
         (gates at {probe_gate_positions:?}, probe at {probe_pos})"
    );
    assert!(
        spawn_pos < probe_pos,
        "the enabled-pipeline arm must come BEFORE the probe-only arm \
         (an if/else-if over the same config — never both)"
    );
}

/// The spot→chain sequencing watch channel is plumbed end-to-end in
/// main.rs: one channel, created only when BOTH halves are enabled, its
/// sender into the spot params and its receiver into the chain params.
#[test]
fn ratchet_chain1m_watch_channel_is_plumbed_between_spot_and_chain() {
    let main_src = read_app_src("src/main.rs");
    let (seam_pos, seam_end) = post_market_span(&main_src);
    let seam = &main_src[seam_pos..seam_end];

    for needle in [
        // One watch channel, dual-gated on BOTH config flags.
        "tokio::sync::watch::channel::<Option<u32>>(None)",
        "config.spot_1m_rest.enabled && config.option_chain_1m.enabled",
        // Sender → the spot leg's params.
        "minute_done_tx: spot_minute_done_tx",
        // Receiver → the chain leg's params.
        "spot_minute_done: spot_minute_done_rx",
    ] {
        assert!(
            seam.contains(needle),
            "spawn_post_market_tasks lost its `{needle}` wiring — the \
             spot→chain sequencing signal must be plumbed exactly once, \
             gated on both halves being enabled"
        );
    }

    // And the spot module actually PUBLISHES the signal at fire end.
    let spot_src = read_app_src("src/spot_1m_rest_boot.rs");
    assert!(
        spot_src.contains("tx.send_replace(Some(fire))"),
        "spot_1m_rest_boot.rs lost the minute-done publish — the chain \
         leg's early-wake sequencing depends on it"
    );
}

/// Stub-guard (Rule 14 — no skeleton): the boot module must actually
/// warm up, fetch, parse, persist and page, not just define pub fns.
#[test]
fn ratchet_chain1m_boot_module_is_not_a_stub() {
    let module_src = read_app_src("src/option_chain_1m_boot.rs");
    for needle in [
        // Both Dhan endpoints are reached via the shared path constants +
        // join_api_url (the DHAN-REST-400 trailing-slash lesson).
        "DHAN_OPTION_CHAIN_PATH",
        "DHAN_OPTION_CHAIN_EXPIRYLIST_PATH",
        "join_api_url(",
        // The option-chain endpoints need the EXTRA client-id header
        // (option-chain.md rule 3).
        ".header(\"client-id\", client_id)",
        // The JWT is re-loaded from the live handle EVERY fire (24h
        // rotation) — never copied once at spawn.
        "params.token_handle.load()",
        // The day-start expirylist warmup resolves the CURRENT expiry —
        // never guessed (option-chain.md rule 9) — and fail-closes.
        "resolve_current_expiries(",
        "select_current_expiry(",
        "Chain04ExpirylistFailed",
        // The spot-sequencing wait is fallback-bounded (never blocked
        // forever on a dead spot leg).
        "CHAIN_1M_FALLBACK_DELAY_MS",
        "spot_signal_satisfies(",
        // Successes persist through the storage writer + flush.
        "OptionChain1mWriter",
        "writer.flush()",
        // The scheduler advances via the shared boundary discipline
        // (never a drifting interval(60s)); missed boundaries are LOUD.
        "next_fire_after(",
        "tv_chain1m_boundary_skipped_total",
        // Entitlement classification + the once-per-day stop.
        "is_entitlement_reject(",
        "Chain01EntitlementAbsent",
        // Edge-triggered escalation + recovery events are wired.
        "ChainFetchDegraded",
        "ChainFetchRecovered",
        // The streamed body cap (chains are BIG; hostile servers bounded).
        "CHAIN_1M_MAX_BODY_BYTES",
        // The per-underlying hard budget (overruns can't stack).
        "CHAIN_1M_UNDERLYING_BUDGET_SECS",
    ] {
        assert!(
            module_src.contains(needle),
            "option_chain_1m_boot.rs lost its `{needle}` wiring — the module \
             must warm up, fetch, parse, persist and page for real (Rule 14: \
             no skeleton PRs)"
        );
    }
}

/// DEDUP float-key safety ratchet (hostile-review L3): `strike` sits in
/// the `option_chain_1m` DEDUP UPSERT KEYS as a DOUBLE — safe ONLY
/// because every strike is PARSED from Dhan's decimal-string map key
/// (bit-identical f64 on re-fetch: "25650.0" and "25650.000000" parse to
/// the same bits) and is NEVER computed arithmetically. Any future
/// writer deriving strikes (e.g. ATM ± k×step) would silently mint
/// duplicate rows — that change must move the key to a paise LONG first
/// (see the note in `option_chain_1m_persistence.rs`).
#[test]
fn ratchet_chain1m_strike_is_parse_only_never_computed() {
    let boot_src = read_app_src("src/option_chain_1m_boot.rs");
    let persist_src = read_app_src("../storage/src/option_chain_1m_persistence.rs");

    // The single strike-producing site: the decimal-string key parse.
    assert!(
        boot_src.contains("strike_key.trim().parse::<f64>()"),
        "option_chain_1m_boot.rs lost the parse-only strike site — strikes \
         must come ONLY from Dhan's decimal-string keys"
    );

    // No arithmetic on strikes anywhere in either module (the invariant
    // the float DEDUP key depends on). Comment lines are stripped so
    // doc-comment prose (`/// strike, leg`) never false-positives.
    let strip_comments = |src: &str| -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let boot_code = strip_comments(&boot_src);
    let persist_code = strip_comments(&persist_src);
    for (name, src) in [
        ("option_chain_1m_boot.rs", boot_code.as_str()),
        ("option_chain_1m_persistence.rs", persist_code.as_str()),
    ] {
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
                !src.contains(needle),
                "{name} contains `{needle}` — computing/deriving strikes \
                 breaks the parse-only invariant the float DEDUP key depends \
                 on; move the key to a paise LONG before landing arithmetic"
            );
        }
    }
}
