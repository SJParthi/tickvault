//! Source-scan ratchet — per-minute spot 1m REST pipeline wired into the
//! shared post-market spawn seam (operator grant 2026-07-12, PR-2).
//!
//! Pins (house pattern: `ws_rate_limit_cooldown_wiring_guard.rs`):
//! 1. main.rs carries exactly ONE `spawn_supervised_spot_1m_rest(` call
//!    site, INSIDE `spawn_post_market_tasks` (the shared post-market seam; the rest_canary itself was retired 2026-07-14 —
//!    invoked from BOTH boot paths, once-guarded), and that call is gated
//!    on `config.spot_1m_rest.enabled` (fail-safe: absent section = off).
//! 2. Stub-guard: `spot_1m_rest_boot.rs` really does the work — posts to
//!    the intraday path, re-loads the token handle per fire, drives the
//!    bounded re-poll ladder from the constant schedule, and persists via
//!    the storage writer (no skeleton PR — audit-findings Rule 14).
//!
//! Runbook: `.claude/rules/project/rest-1m-pipeline-error-codes.md`.

use std::fs;
use std::path::PathBuf;

fn read_app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `(` suffix distinguishes CALL sites from doc-comment mentions; the
/// fn DEFINITION lives in spot_1m_rest_boot.rs, not main.rs, so every
/// main.rs hit is a call site.
const SPAWN_CALL: &str = "spawn_supervised_spot_1m_rest(";
const CONFIG_GATE: &str = "config.spot_1m_rest.enabled";
const POST_MARKET_FN: &str = "fn spawn_post_market_tasks(";

#[test]
fn ratchet_spot1m_spawn_is_config_gated_inside_post_market_seam() {
    let main_src = read_app_src("src/main.rs");

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
         per-minute fetcher",
        spawn_positions.len()
    );
    let spawn_pos = spawn_positions[0];

    let seam_pos = main_src
        .find(POST_MARKET_FN)
        .expect("spawn_post_market_tasks must exist in main.rs");
    // Pin the seam's END too (2026-07-12 review L1 — a one-sided check
    // would still pass if the spawn migrated into ANY later fn): the next
    // top-level `fn ` after the seam bounds it.
    let seam_end = main_src[seam_pos + POST_MARKET_FN.len()..]
        .find("\nfn ")
        .map_or(main_src.len(), |off| seam_pos + POST_MARKET_FN.len() + off);
    assert!(
        spawn_pos > seam_pos && spawn_pos < seam_end,
        "the spot_1m spawn at byte {spawn_pos} must live INSIDE \
         spawn_post_market_tasks (fn spans bytes {seam_pos}..{seam_end}) — \
         that seam is called from BOTH boot paths and carries the \
         process-global once-guard"
    );

    let gate_positions: Vec<usize> = main_src
        .match_indices(CONFIG_GATE)
        .map(|(pos, _)| pos)
        .collect();
    assert!(
        !gate_positions.is_empty(),
        "main.rs must gate the spot_1m spawn on `{CONFIG_GATE}` \
         (fail-safe: an absent [spot_1m_rest] section disables it)"
    );
    assert!(
        gate_positions
            .iter()
            .any(|gate| { *gate < spawn_pos && spawn_pos - gate < 2_048 && *gate > seam_pos }),
        "the `{CONFIG_GATE}` gate must immediately precede the spawn call \
         inside spawn_post_market_tasks (gates at {gate_positions:?}, \
         spawn at {spawn_pos})"
    );
}

/// Stub-guard (Rule 14 — no skeleton): the boot module must actually fetch
/// + parse + persist, not just define pub fns.
#[test]
fn ratchet_spot1m_boot_module_is_not_a_stub() {
    let module_src = read_app_src("src/spot_1m_rest_boot.rs");
    for needle in [
        // The Dhan endpoint is reached via the shared path constant +
        // join_api_url (the DHAN-REST-400 trailing-slash lesson).
        "DHAN_CHARTS_INTRADAY_PATH",
        "join_api_url(",
        // The JWT is re-loaded from the live handle EVERY fire (24h
        // rotation) — never copied once at spawn.
        "params.token_handle.load()",
        // The bounded in-minute re-poll ladder is driven from the constant
        // schedule (never an unbounded retry).
        "SPOT_1M_REST_RETRY_OFFSETS_MS",
        // Successes persist through the storage writer + flush.
        "Spot1mRestWriter",
        "writer.flush()",
        // The scheduler sleeps to computed minute boundaries (never a
        // drifting interval(60s)).
        "next_minute_close_fire(",
        // Edge-triggered escalation + recovery events are wired.
        "Spot1mFetchDegraded",
        "Spot1mFetchRecovered",
        // 2026-07-12 hostile-review fixes stay wired: the hard per-SID
        // ladder budget (H2), the missed-boundary loud accounting (H2),
        // the same-second re-fire horizon (H1), and the streamed body cap
        // (security M).
        "SPOT_1M_REST_SID_BUDGET_SECS",
        "tv_spot1m_boundary_skipped_total",
        "next_fire_after(",
        "SPOT_1M_REST_MAX_BODY_BYTES",
    ] {
        assert!(
            module_src.contains(needle),
            "spot_1m_rest_boot.rs lost its `{needle}` wiring — the module \
             must fetch, parse, persist and page for real (Rule 14: no \
             skeleton PRs)"
        );
    }
}

// ---------------------------------------------------------------------------
// GAP-11 (2026-07-14): Dhan REST forensics rows + close_to_persist_ms
// stamping. The scans below run against the PRODUCTION region only (split
// at the first `#[cfg(test)]`) so a guard can never match its own
// assertion literals or a test fixture (the shadow-writer vacuous-scan
// lesson).
// ---------------------------------------------------------------------------

/// Everything before the trailing tests module — the production region.
/// Split at the `#[cfg(test)] mod tests` MODULE marker (a bare
/// `#[cfg(test)]` also gates mid-file test helpers in these modules).
fn production_region(src: &str) -> &str {
    src.split("#[cfg(test)]\nmod tests").next().unwrap_or(src)
}

/// The body of one fn: from its marker to the next top-level fn item.
/// Anchored on fn names, never line numbers (robust to reformatting).
fn fn_region<'a>(src: &'a str, marker: &str, rel: &str) -> &'a str {
    let start = src.find(marker).unwrap_or_else(|| {
        panic!("{rel}: fn marker `{marker}` not found in the production region")
    });
    let tail = &src[start + marker.len()..];
    let end = ["\nasync fn ", "\npub async fn ", "\npub fn ", "\nfn "]
        .iter()
        .filter_map(|m| tail.find(m))
        .min()
        .unwrap_or(tail.len());
    &tail[..end]
}

/// GAP-11a: the Dhan spot AND chain legs really emit `rest_fetch_audit`
/// forensics rows (the rest-1m-pipeline rule file's "fast FOLLOW-UP"
/// closed) — never a skeleton.
#[test]
fn ratchet_dhan_spot_and_chain_legs_emit_rest_fetch_audit() {
    let spot_src = read_app_src("src/spot_1m_rest_boot.rs");
    let spot = production_region(&spot_src);
    for needle in [
        "RestFetchAuditWriter",
        "ensure_rest_fetch_audit_table(",
        "build_dhan_fetch_audit_row(",
        "audit_append_best_effort(",
        "RestFetchOutcome::NamedGap",
        "\"flush_failed\"",
        "\"persist_failed\"",
        "\"named_gap\"",
        "\"no_token\"",
    ] {
        assert!(
            spot.contains(needle),
            "spot_1m_rest_boot.rs production region lost its `{needle}` \
             forensics wiring — every Dhan spot verdict/gap must land a \
             rest_fetch_audit row (GAP-11)"
        );
    }
    let chain_src = read_app_src("src/option_chain_1m_boot.rs");
    let chain = production_region(&chain_src);
    for needle in [
        "RestFetchAuditWriter",
        "ensure_rest_fetch_audit_table(",
        "build_dhan_chain_audit_row(",
        "chain_audit_append_best_effort(",
        "REST_FETCH_LEG_CHAIN_1M",
        "RestFetchOutcome::NoToken",
        "RestFetchOutcome::Skipped",
        "\"empty_chain\"",
        "\"persist_failed\"",
        "\"flush_failed\"",
    ] {
        assert!(
            chain.contains(needle),
            "option_chain_1m_boot.rs production region lost its `{needle}` \
             forensics wiring — every Dhan chain (minute, underlying) must \
             land a rest_fetch_audit row with leg='chain_1m' (GAP-11)"
        );
    }
}

/// GAP-11b: `ok` forensics rows are HELD until the DATA-table flush ACK
/// and stamped there (`stamp_held_ok_rows` AFTER `writer.flush()` in
/// source order, per fire/sweep region, all three Dhan/Groww legs) — a
/// pre-flush ok append would fabricate an ok row for a lost minute.
#[test]
fn ratchet_ok_audit_rows_stamp_after_data_flush_ack() {
    let cases: [(&str, &[&str]); 3] = [
        (
            "src/spot_1m_rest_boot.rs",
            &[
                "async fn fire_one_minute(",
                "async fn run_post_session_sweep(",
            ],
        ),
        ("src/groww_spot_1m_boot.rs", &["async fn fire_one_minute("]),
        (
            "src/option_chain_1m_boot.rs",
            &["async fn fire_one_chain_minute("],
        ),
    ];
    for (rel, markers) in cases {
        let src = read_app_src(rel);
        let prod = production_region(&src);
        for marker in markers {
            let region = fn_region(prod, marker, rel);
            let flush_pos = region.find("writer.flush()").unwrap_or_else(|| {
                panic!("{rel} `{marker}`: data `writer.flush()` call not found")
            });
            let stamp_pos = region.find("stamp_held_ok_rows(").unwrap_or_else(|| {
                panic!(
                    "{rel} `{marker}`: `stamp_held_ok_rows(` call not found — \
                     ok rows must be held until the data flush ACK (GAP-11)"
                )
            });
            assert!(
                stamp_pos > flush_pos,
                "{rel} `{marker}`: the held-ok stamp+append (byte {stamp_pos}) \
                 must follow the data flush (byte {flush_pos}) in source order \
                 — close_to_persist_ms is a post-flush-ACK measurement"
            );
            // Review LOW (2026-07-14): source ORDER alone would still pass
            // if a regression stamped with a literal `true` — the stamp
            // call must consume the REAL flush verdict binding.
            assert!(
                region.contains("let flush_result = writer.flush()"),
                "{rel} `{marker}`: the data flush must be BOUND as \
                 `let flush_result = writer.flush()` — the stamp gate reads \
                 that verdict, never a literal"
            );
            let stamp_window = &region[stamp_pos..region.len().min(stamp_pos + 400)];
            assert!(
                stamp_window.contains("flush_result.is_ok()"),
                "{rel} `{marker}`: the stamp_held_ok_rows call must gate on \
                 `flush_result.is_ok()` (the real flush verdict) — a literal \
                 `true` would fabricate ok rows for lost minutes"
            );
            assert!(
                region.contains("held_ok_rows.push("),
                "{rel} `{marker}`: lost the `held_ok_rows.push(` hold site — \
                 ok rows must not append before the flush verdict"
            );
            assert!(
                !region.contains("close_to_persist_ms: -1"),
                "{rel} `{marker}`: a hardcoded `close_to_persist_ms: -1` \
                 reappeared inside the fire/sweep region — ok rows must carry \
                 the REAL post-flush stamp via stamp_held_ok_rows"
            );
        }
    }
}

/// GAP-11 adversarial-review fixes (2026-07-14) stay wired:
/// - HIGH: the Dhan spot ladder threads REAL attempt/429 forensics and a
///   terminal-429 ladder classifies `rate_limited` (never a generic
///   `error` while the scoreboard digest sums 0 rate-limit hits).
/// - MEDIUM 1: Dhan spot missed boundaries land queryable
///   `outcome=skipped` forensics rows (the Groww spot / Dhan chain shape).
/// - MEDIUM 2: the Groww spot BACKFILL recovery holds an `ok` audit row
///   (cross-feed symmetry — a backfill-repaired Groww minute must not
///   read "failed, never recovered" in the digest forever).
#[test]
fn ratchet_gap11_review_fixes_stay_wired() {
    let spot_src = read_app_src("src/spot_1m_rest_boot.rs");
    let spot = production_region(&spot_src);
    for needle in [
        // HIGH — real ladder forensics + terminal-429 classification.
        "struct DhanLadderForensics",
        "dhan_failed_audit_class(",
        "RestFetchOutcome::RateLimited",
        // MEDIUM 1 — skipped-boundary forensics rows + midnight guard.
        "RestFetchOutcome::Skipped",
        "\"boundary_skipped\"",
    ] {
        assert!(
            spot.contains(needle),
            "spot_1m_rest_boot.rs production region lost its `{needle}` \
             wiring — the GAP-11 review HIGH/MEDIUM-1 fixes must stay \
             (real rate-limit facts + queryable skipped boundaries)"
        );
    }
    let groww_src = read_app_src("src/groww_spot_1m_boot.rs");
    let groww = production_region(&groww_src);
    let fire = fn_region(
        groww,
        "async fn fire_one_minute(",
        "src/groww_spot_1m_boot.rs",
    );
    let backfill_pos = fire
        .find("tv_groww_spot1m_backfilled_total")
        .expect("groww fire_one_minute lost its backfill-Ok arm");
    assert!(
        fire[backfill_pos..].contains("held_ok_rows.push(build_fetch_audit_row("),
        "groww_spot_1m_boot.rs fire_one_minute: the backfill-Ok arm must \
         HOLD an ok audit row for the repaired minute (GAP-11 review \
         MEDIUM 2) — without it a backfill-repaired Groww minute reads \
         'failed, never recovered' in the digest forever"
    );
}
