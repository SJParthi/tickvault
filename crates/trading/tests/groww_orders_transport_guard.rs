//! ORD-PR-3 source-scan ratchets + public-API budget saturation for the Groww
//! order transport/executor lane. Feature-gated so a no-feature build compiles
//! this file to nothing (the subtree does not exist without `groww_orders`).

#![cfg(feature = "groww_orders")]

use std::fs;
use std::path::PathBuf;

fn src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/oms/groww")
        .join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Strip Rust comments (line `//…` and block `/* … */`) so a forbidden token
/// that appears only in a DOC COMMENT (explaining what NOT to do) never trips a
/// code-scan. A `//` that is part of `://` (a URL scheme) is treated as code,
/// not a comment — the house comment-stripper contract.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let mut in_block = false;
    while i < bytes.len() {
        if in_block {
            if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            in_block = true;
            i += 2;
            continue;
        }
        if bytes[i] == b'/'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'/'
            && (i == 0 || bytes[i - 1] != b':')
        {
            // Line comment — skip to end of line.
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Scan the production region only — everything before the trailing
/// `mod tests` block (test code lives in a separate file / inline test module
/// and is exempt). Splitting at `mod tests` (not the first `#[cfg(test)]`)
/// keeps the `#[cfg(test)] fn new_live_for_test` production-adjacent gate in
/// view while excluding the actual test bodies.
fn production_region(source: &str) -> &str {
    match source.rfind("mod tests") {
        Some(idx) => &source[..idx],
        None => source,
    }
}

// ---------------------------------------------------------------------------
// Gate 5 / paper-zero-HTTP: HTTP + endpoint PATHs live ONLY in api_client.rs.
// executor/rate_budget/events contain NO reqwest, NO `https`, NO base-url.
// ---------------------------------------------------------------------------

#[test]
fn test_gate6_paper_lane_zero_http_import_scan() {
    for module in ["executor.rs", "rate_budget.rs", "events.rs"] {
        let code = strip_comments(&src(module));
        let prod = production_region(&code).to_ascii_lowercase();
        assert!(
            !prod.contains("reqwest"),
            "{module}: paper lane must not reference reqwest"
        );
        assert!(
            !prod.contains("https://"),
            "{module}: paper lane must not embed an https base URL"
        );
        assert!(
            !prod.contains("api.groww.in"),
            "{module}: paper lane must not embed the Groww host"
        );
        assert!(
            !prod.contains("/v1/order/"),
            "{module}: endpoint PATH strings are confined to api_client.rs (Gate 5)"
        );
    }
}

#[test]
fn test_endpoint_paths_and_http_confined_to_api_client() {
    // api_client.rs is the ONLY module allowed to contain reqwest + the paths.
    let api = src("api_client.rs");
    assert!(
        api.contains("reqwest"),
        "api_client owns the reqwest transport"
    );
    assert!(
        api.contains("/v1/order/create"),
        "api_client owns the create path"
    );
    assert!(
        api.contains("api.groww.in"),
        "api_client owns the base-url const"
    );
}

// ---------------------------------------------------------------------------
// Carry-note (b): fsynced ledger calls run under spawn_blocking; NO block_in_place.
// ---------------------------------------------------------------------------

#[test]
fn test_ratchet_ledger_fsync_wrapped_in_spawn_blocking() {
    let code = strip_comments(&src("executor.rs"));
    assert!(
        code.contains("spawn_blocking"),
        "executor must run fsync ledger calls under spawn_blocking (carry-note b)"
    );
    assert!(
        !code.contains("block_in_place"),
        "executor must NOT use block_in_place (panics on the current-thread test runtime)"
    );
    // The mutating ledger call exists (routed through the with_ledger closure).
    assert!(
        code.contains("record_intent(new"),
        "the write-ahead record_intent must be present"
    );
}

// ---------------------------------------------------------------------------
// Carry-note (c): reference-id salt from a CSPRNG (uuid v4); no predictable src.
// ---------------------------------------------------------------------------

#[test]
fn test_ratchet_reference_id_salt_from_csprng() {
    let exec = src("executor.rs");
    let prod = production_region(&exec);
    assert!(
        prod.contains("uuid::Uuid::new_v4()"),
        "reference-id salt must draw from the uuid-v4 OS-CSPRNG"
    );
    // A predictable salt source must never feed the reference id.
    let salt_fn_start = prod
        .find("pub fn reference_id_salt()")
        .expect("reference_id_salt fn present");
    let salt_fn = &prod[salt_fn_start..salt_fn_start + 300.min(prod.len() - salt_fn_start)];
    for banned in ["SystemTime", "Instant", "elapsed", "timestamp", "now_ms"] {
        assert!(
            !salt_fn.contains(banned),
            "reference_id_salt must not derive from a predictable source ({banned})"
        );
    }
}

// ---------------------------------------------------------------------------
// Gate 2/3: ExecutionMode::Live is only constructed outside cfg(test) via the
// const-gated new_live; the unconditional Live mutator is #[cfg(test)].
// ---------------------------------------------------------------------------

#[test]
fn test_execution_mode_live_only_under_cfg_test() {
    let exec = src("executor.rs");
    let prod = production_region(&exec);
    // The only production construction of Live is inside new_live, which is
    // guarded by live_send_permitted; the unconditional constructor is test-only.
    assert!(
        prod.contains("live_send_permitted(cfg.live_fire_requested)"),
        "new_live must gate Live construction on live_send_permitted"
    );
    // The bypassing constructor must be cfg(test).
    let idx = exec
        .find("fn new_live_for_test")
        .expect("new_live_for_test present");
    let preceding = &exec[idx.saturating_sub(120)..idx];
    assert!(
        preceding.contains("#[cfg(test)]"),
        "the Live-bypass constructor must be #[cfg(test)]"
    );
}

// ---------------------------------------------------------------------------
// Seam: a hint NEVER transitions state — events.rs never touches the FSM.
// ---------------------------------------------------------------------------

#[test]
fn test_hint_never_transitions_state() {
    let events = strip_comments(&src("events.rs"));
    let prod = production_region(&events);
    for fsm in [
        "evaluate_transition",
        "TrackedOrderState",
        "TransitionDecision",
        "super::state",
    ] {
        assert!(
            !prod.contains(fsm),
            "events.rs (hint sink) must NOT reference the FSM ({fsm}) — a hint only enqueues a poll"
        );
    }
    assert!(
        prod.contains("OrderPollHint"),
        "the hint sink produces a targeted poll hint"
    );
}

// ---------------------------------------------------------------------------
// Rate budget: self-caps ≤ doc ceilings; saturation ≤ reads cap (public API).
// ---------------------------------------------------------------------------

#[test]
fn test_rate_budget_saturation_within_reads_cap() {
    use tickvault_trading::oms::groww::rate_budget::describe_budget;
    let d = describe_budget();
    assert!(d.orders_per_second <= 10);
    assert!(d.orders_per_minute <= 250);
    assert!(d.reads_per_second <= 20);
    assert!(d.reads_per_minute <= 500);
    assert!(
        d.worst_case_reads_per_minute <= u64::from(d.reads_per_minute),
        "worst-case scheduler reads ({}) exceed the reads self-cap ({})",
        d.worst_case_reads_per_minute,
        d.reads_per_minute
    );
}
