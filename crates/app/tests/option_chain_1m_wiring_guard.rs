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

/// Strip the `//`-comment tail of a line (the `http_client_fallback_guard`
/// house stripper): a `//` NOT immediately preceded by `:` opens a comment;
/// a URL scheme separator (`://` inside a string literal) never does — a
/// naive `split("//")` would truncate the scan mid-line (false negative).
fn code_portion(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let preceded_by_colon = i > 0 && bytes[i - 1] == b':';
            if !preceded_by_colon {
                return &line[..i];
            }
            // `://` — a URL scheme separator inside a string literal, not
            // a comment. Skip past it and keep scanning.
            i += 2;
        } else {
            i += 1;
        }
    }
    line
}

/// The PRODUCTION region of a module — everything BEFORE the first
/// `#[cfg(test)]` — with `//` line comments stripped via [`code_portion`].
/// The 2026-07-14 hardening needles scan THIS, so a test-module mention or
/// a doc-comment quote can never vacuously satisfy a ratchet (the
/// 2026-07-06 comment-scan false-OK class).
fn production_code(module_src: &str) -> String {
    let prod = module_src
        .split("#[cfg(test)]")
        .next()
        .expect("split always yields at least one piece");
    let stripped = prod
        .lines()
        .map(code_portion)
        .collect::<Vec<_>>()
        .join("\n");
    // Block-comment tripwire (refuter round 1, MEDIUM): [`code_portion`]
    // strips ONLY `//` line comments — a `/* join_set.spawn( */` block
    // comment would survive the strip and vacuously satisfy every
    // production-region needle. Both target modules carry zero `/*`
    // today; keep it that way (loud false-BLOCK on a string-literal
    // `/*` is acceptable — never a false pass).
    assert!(
        !stripped.contains("/*"),
        "block comments in the production region would defeat the \
         comment-stripped needles — use // comments or extend the stripper"
    );
    stripped
}

/// Stripper self-test (the http_client_fallback_guard precedent): a URL
/// scheme separator inside a string literal must NOT truncate the scan.
#[test]
fn code_portion_self_test_url_scheme_is_not_a_comment() {
    let line = r#"let x = "http://host"; join_set.spawn(async move {"#;
    assert!(
        code_portion(line).contains("join_set.spawn("),
        "stripper must not treat '://' as a comment start: {:?}",
        code_portion(line)
    );
    let mixed = r#"let url = "https://a/b"; // join_set.spawn( explanation"#;
    let kept = code_portion(mixed);
    assert!(kept.contains(r#""https://a/b""#), "{kept:?}");
    assert!(
        !kept.contains("join_set.spawn("),
        "the real comment tail must be stripped: {kept:?}"
    );
    assert_eq!(code_portion("// pure comment line"), "");
}

/// The `(` suffix distinguishes CALL sites from doc-comment mentions; the
/// fn DEFINITIONS live in option_chain_1m_boot.rs, not main.rs, so every
/// main.rs hit is a call site.
const SPAWN_CALL: &str = "spawn_supervised_option_chain_1m(";
const PROBE_CALL: &str = "run_option_chain_1m_probe(";
const PIPELINE_GATE: &str = "config.option_chain_1m.enabled";
const PROBE_GATE: &str = "config.option_chain_1m.probe_and_report";
// PR-C2 re-home (2026-07-13, operator retirement directive —
// websocket-connection-scope-lock.md "2026-07-13 Amendment"): the Dhan
// chain legs moved OUT of the deleted main.rs `spawn_post_market_tasks`
// seam INTO `dhan_rest_stack.rs` (the sole surviving Dhan surface, which
// owns the token + the spot→chain sequencing). The seam for the pins below
// is therefore the dhan_rest_stack module itself.
fn read_stack_src() -> String {
    // PRODUCTION region only — cut at the module's own `#[cfg(test)]`
    // in-file tests, whose needle literals would otherwise satisfy the
    // pins vacuously (the shadow-writer vacuous-scan lesson).
    let full = read_app_src("src/dhan_rest_stack.rs");
    match full.find("#[cfg(test)]") {
        Some(cut) => full[..cut].to_string(),
        None => full,
    }
}

#[test]
fn ratchet_chain1m_spawn_and_probe_are_config_gated_inside_post_market_seam() {
    // PR-C2: the seam is the dhan_rest_stack module (see the re-home note
    // above); positions are module-relative.
    let main_src = read_stack_src();
    let (seam_pos, seam_end) = (0usize, main_src.len());

    // Exactly ONE pipeline spawn site, inside the seam.
    let spawn_positions: Vec<usize> = main_src
        .match_indices(SPAWN_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        spawn_positions.len(),
        1,
        "expected exactly 1 `{SPAWN_CALL}` call site in dhan_rest_stack.rs \
         (the sole surviving Dhan surface since PR-C2); found {} — a second \
         site would double-spawn the per-minute chain fetcher",
        spawn_positions.len()
    );
    let spawn_pos = spawn_positions[0];
    assert!(
        spawn_pos > seam_pos && spawn_pos < seam_end,
        "the chain spawn at byte {spawn_pos} must live inside the \
         dhan_rest_stack module (bytes {seam_pos}..{seam_end})"
    );

    // Exactly ONE probe-only spawn site, inside the seam.
    let probe_positions: Vec<usize> = main_src
        .match_indices(PROBE_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        probe_positions.len(),
        1,
        "expected exactly 1 `{PROBE_CALL}` spawn in dhan_rest_stack.rs (the \
         probe-only arm of the DEFAULT-OFF contract); found {}",
        probe_positions.len()
    );
    let probe_pos = probe_positions[0];
    assert!(
        probe_pos > seam_pos && probe_pos < seam_end,
        "the probe spawn at byte {probe_pos} must live inside the \
         dhan_rest_stack module (bytes {seam_pos}..{seam_end})"
    );

    // The pipeline gate precedes the pipeline spawn; the probe gate
    // precedes the probe spawn; and the pipeline gate comes FIRST (the
    // probe arm is the `else if` of the SAME decision — the pipeline can
    // never run alongside the probe).
    let gate_pos = main_src
        .find(PIPELINE_GATE)
        .expect("dhan_rest_stack.rs must gate the chain spawn on the enabled flag");
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
         spawn inside dhan_rest_stack.rs (gates at {gate_positions:?}, \
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
    // PR-C2: the plumbing lives in dhan_rest_stack.rs (see the re-home
    // note above).
    let stack_src = read_stack_src();
    let seam = &stack_src[..];

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
            "dhan_rest_stack.rs lost its `{needle}` wiring — the \
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

// ---------------------------------------------------------------------------
// 2026-07-14 chain-capture hardening ratchets (runbook:
// `.claude/rules/project/cross-source-chain-coverage-2026-07-14.md`)
// ---------------------------------------------------------------------------

/// Byte span of `fire_one_chain_minute` in option_chain_1m_boot.rs
/// (start..next top-level fn) — the region the concurrency + retry pins
/// scan.
fn chain_fire_span(module_src: &str) -> &str {
    let start = module_src
        .find("async fn fire_one_chain_minute(")
        .expect("fire_one_chain_minute must exist in option_chain_1m_boot.rs");
    let rest = &module_src[start..];
    let end = rest[1..].find("\nfn ").map_or(rest.len(), |off| off + 1);
    &rest[..end]
}

/// The fire keeps its Dhan-DOCUMENTED JoinSet concurrency (distinct
/// underlyings concurrently; the 3s bound is per unique
/// (underlying, expiry) key) — a future sequentialization must be a
/// deliberate, rule-edited change, and the doc quote must stay at the
/// spawn site as the in-code authority.
#[test]
fn ratchet_chain_fire_keeps_joinset_concurrency() {
    let module_src = read_app_src("src/option_chain_1m_boot.rs");
    // CODE-only scan (comment-stripped production region): the mandated
    // doc quote below itself contains "JoinSet", so a comment-visible
    // needle would be self-satisfied by the quote — the spawn CALL
    // literal in stripped code is the honest pin.
    let production = production_code(&module_src);
    let fire_code = chain_fire_span(&production);
    assert!(
        fire_code.contains("join_set.spawn("),
        "fire_one_chain_minute lost its `join_set.spawn(` call — the \
         Dhan-documented concurrent-distinct fire must not be silently \
         sequentialized (cross-source-chain-coverage-2026-07-14.md \
         rule-edit required)"
    );
    let fire = chain_fire_span(&module_src);
    // The quote is a wrapped comment block — normalize the wrapping
    // before matching the doc fragment.
    let normalized: String = fire
        .lines()
        .map(|l| l.trim_start().trim_start_matches("//").trim())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        normalized.contains("concurrently every 3 seconds"),
        "the Dhan option-chain rate-limit doc quote must stay at the \
         spawn site (the in-code authority for keeping concurrency)"
    );
}

/// The phase-2 retry pass is wired into the fire AND launch-gated by the
/// decision ceiling; refused retries are counted, never silent.
#[test]
fn ratchet_chain_retry_pass_is_ceiling_gated() {
    let module_src = read_app_src("src/option_chain_1m_boot.rs");
    // Comment-stripped PRODUCTION region only: the `#[cfg(test)]` module
    // calls `chain_retry_allowed(` and doc comments quote
    // `"skipped_ceiling"`, so a whole-file scan would survive deleting
    // the production wiring (coverage-review mutation #3).
    let production = production_code(&module_src);
    let fire_code = chain_fire_span(&production);
    assert!(
        fire_code.contains("chain_retry_pass("),
        "fire_one_chain_minute lost its phase-2 retry pass call"
    );
    for needle in [
        // The ceiling gate chain: pass → decision → allowed.
        "chain_retry_allowed(",
        "retry_launch_elapsed_ms(",
        "CHAIN_1M_DECISION_CEILING_SECS",
        // Refused retries are COUNTED (never silent) — the label AND the
        // exact counter EMISSION line (deleting the metrics::counter!
        // call must fail the build, not just renaming the label).
        "\"skipped_ceiling\"",
        r#"metrics::counter!("tv_chain1m_retry_total", "outcome" => "skipped_ceiling")"#,
        "tv_chain1m_retry_total",
        "retry_skipped_ceiling",
    ] {
        assert!(
            production.contains(needle),
            "option_chain_1m_boot.rs lost its `{needle}` wiring in the \
             PRODUCTION region — the bounded retry must stay ceiling-gated \
             with counted refusals"
        );
    }
}

/// The not-served counter's `underlying` label comes from the pinned
/// static symbols (`target.symbol` via the verdict tuple) — never a
/// runtime-built string (static-label discipline, 3 label values).
#[test]
fn ratchet_not_served_counter_labels_static_underlying_symbols() {
    let module_src = read_app_src("src/option_chain_1m_boot.rs");
    // Comment-stripped PRODUCTION region only — a doc-comment or
    // test-module mention can never satisfy these needles.
    let production = production_code(&module_src);
    // The single EMISSION site: the counter-name literal with the STATIC
    // `underlying` binding as its label (a &'static str from the pinned
    // symbols) — never a runtime-built string.
    let macro_literal = r#""tv_chain1m_underlying_not_served_total", "underlying" => underlying"#;
    let pos = production.find(macro_literal).unwrap_or_else(|| {
        panic!(
            "the not-served counter emission must label with the static \
             `underlying` binding — literal `{macro_literal}` not found \
             in the production region"
        )
    });
    // No runtime label construction anywhere near the emission.
    let window = &production[pos.saturating_sub(200)..(pos + 200).min(production.len())];
    assert!(
        !window.contains("format!"),
        "the not-served counter label must never be runtime-built: {window}"
    );
    // And the verdict tuples feed the pinned static symbols.
    assert!(
        production.contains("served_verdicts.push((") && production.contains("target.symbol"),
        "the verdict line must push the pinned static target.symbol \
         (production region)"
    );
}

/// All 3 mid-session trading-day flip exits (chain loop + spot
/// per-minute loop + spot batch loop) are coded + counted; the bare
/// info-only exit form is gone.
#[test]
fn ratchet_trading_day_flip_exits_are_coded() {
    // Comment-stripped PRODUCTION regions only.
    let chain_src = production_code(&read_app_src("src/option_chain_1m_boot.rs"));
    let spot_src = production_code(&read_app_src("src/spot_1m_rest_boot.rs"));
    assert_eq!(
        chain_src
            .matches("stage = \"trading_day_flip_exit\"")
            .count(),
        1,
        "the chain loop must carry exactly ONE coded trading-day flip exit"
    );
    assert!(
        chain_src.contains("tv_chain1m_trading_day_flip_exit_total"),
        "the chain flip exit lost its counter"
    );
    assert_eq!(
        spot_src
            .matches("stage = \"trading_day_flip_exit\"")
            .count(),
        2,
        "the spot leg must carry exactly TWO coded flip exits (per-minute \
         loop + batch loop)"
    );
    assert!(
        spot_src.contains("tv_spot1m_trading_day_flip_exit_total"),
        "the spot flip exits lost their counter"
    );
    for (name, src) in [
        ("option_chain_1m_boot.rs", chain_src.as_str()),
        ("spot_1m_rest_boot.rs", spot_src.as_str()),
    ] {
        assert!(
            !src.contains("no longer a trading day — exiting"),
            "{name} regressed to the bare info-only trading-day exit — \
             flip exits must stay coded + counted"
        );
    }
}

/// Behavioral pin on the flip-exit CONSEQUENCE (coverage-review mutation
/// #2): each coded `trading_day_flip_exit` emission must be followed by a
/// `return` — deleting the `return;`/`return false;` would keep the loop
/// firing on a non-trading day with every count/counter needle above
/// still green. Windowed source-scan (the flip sites live inside a
/// non-injectable supervised loop, so a real behavioral unit test would
/// need a full scheduler harness — the scan is the honest pin): the scan
/// stops at the flip if-block's own closing `}` (a trimmed line starting
/// with `}`), so ONLY the flip block's own `return` counts — an unrelated
/// `return;` in the next loop arm (the `next_fire_after` else-block's,
/// which sat at exactly non-empty line 10 at the chain site) can never
/// satisfy the pin (refuter round 1, HIGH). A hard cap of 8 non-empty
/// lines (the multi-line `error!` message body + `);` span ≤ 5 today)
/// backstops pathological formatting.
#[test]
fn ratchet_trading_day_flip_exits_return_immediately() {
    for (name, rel, expected_sites) in [
        ("option_chain_1m_boot.rs", "src/option_chain_1m_boot.rs", 1),
        ("spot_1m_rest_boot.rs", "src/spot_1m_rest_boot.rs", 2),
    ] {
        let production = production_code(&read_app_src(rel));
        let mut sites = 0_usize;
        for (pos, _) in production.match_indices("stage = \"trading_day_flip_exit\"") {
            sites += 1;
            let mut non_empty = 0_usize;
            let mut has_return = false;
            for line in production[pos..].lines().skip(1) {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.starts_with("return") {
                    has_return = true;
                    break;
                }
                // The flip if-block's closing brace: the block ended
                // WITHOUT a `return` — stop here so a later loop arm's
                // `return` can never satisfy the pin.
                if trimmed.starts_with('}') {
                    break;
                }
                non_empty += 1;
                if non_empty >= 8 {
                    break;
                }
            }
            assert!(
                has_return,
                "{name}: the trading_day_flip_exit emission at byte {pos} \
                 is no longer followed by a `return` inside its own \
                 if-block — the loop would keep firing on a non-trading \
                 day (the exit IS the behavior, not the log line)"
            );
        }
        assert_eq!(
            sites, expected_sites,
            "{name}: expected {expected_sites} flip-exit site(s) to pin"
        );
    }
}
