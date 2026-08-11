//! Smoke tests — does the MCP server come up and expose a usable surface?
//!
//! ## Why this file exists
//!
//! Until 2026-08-10 this crate was **invisible** to the 22-type test-coverage
//! guard. `scripts/test-coverage-guard.sh` enumerated a hardcoded list of six
//! crates while the workspace has eight, so the guard printed "full workspace"
//! and "All required test types present" without ever looking at this crate or
//! at `aws-lambdas`. They were not failing — they were unexamined, which is
//! worse, because the output actively asserted completeness.
//!
//! When the enumeration was made dynamic, this crate immediately failed on the
//! smoke category. That failure was correct: there was no test here answering
//! the most basic question — *does it start, and does it expose its tools?*
//!
//! ## What these assert
//!
//! Real behaviour through the same public entry point a real MCP client hits
//! (`rpc::process_line` over JSON-RPC 2.0), not name-shaped placeholders. The
//! guard's smoke check is a grep on test NAMES, so a function called
//! `smoke_nothing() {}` would satisfy it forever. These deliberately assert on
//! output instead — a test that satisfies a grep while proving nothing is the
//! exact theatre the coverage audit flagged.
//!
//! Nothing here spawns a process, touches the network, or reads outside the
//! repo: `initialize` and `tools/list` are pure request/response.

use tickvault_logs_mcp::config::{Ctx, EndpointsConfig};
use tickvault_logs_mcp::{rpc, signature};

/// A context rooted at a directory with no logs.
///
/// Deliberately NOT `Ctx::from_process_env()`: that reads the ambient
/// environment, so the same test would behave differently on a developer's
/// machine and in CI. `initialize`, `tools/list` and the error paths asserted
/// here are all pure request/response and never touch the root.
fn smoke_ctx() -> Ctx {
    Ctx {
        repo_root: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        cfg: EndpointsConfig::default(),
    }
}

/// Build the request line for a JSON-RPC method with no params.
fn request(id: i64, method: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}"}}"#)
}

#[test]
fn smoke_server_initializes_and_reports_protocol() {
    let ctx = smoke_ctx();
    let response = rpc::process_line(&ctx, &request(1, "initialize")).expect(
        "initialize must produce a response — a request/response method cannot be a notification",
    );

    assert_eq!(response["jsonrpc"], "2.0", "must speak JSON-RPC 2.0");
    assert_eq!(response["id"], 1, "response must echo the request id");
    assert!(
        response.get("error").is_none(),
        "initialize returned an error: {}",
        response["error"]
    );
    assert!(
        response["result"].is_object(),
        "initialize must return a result object, got: {}",
        response["result"]
    );
}

#[test]
fn smoke_tools_list_is_non_empty_and_well_formed() {
    let ctx = smoke_ctx();
    let response = rpc::process_line(&ctx, &request(2, "tools/list"))
        .expect("tools/list must produce a response");

    let listed = response["result"]["tools"].as_array().unwrap_or_else(|| {
        panic!("tools/list must return result.tools as an array, got: {response}")
    });

    // The whole point of this server is the tools it exposes. An empty list is
    // a server that started and does nothing — precisely the kind of
    // "successfully broken" state a smoke test exists to catch.
    assert!(
        !listed.is_empty(),
        "tools/list returned ZERO tools — the server came up exposing nothing"
    );

    for tool in listed {
        let name = tool["name"].as_str().unwrap_or("");
        assert!(
            !name.is_empty(),
            "a listed tool has no name, which no client can call: {tool}"
        );
        assert!(
            tool["inputSchema"].is_object(),
            "tool `{name}` has no inputSchema object — clients cannot construct a valid call"
        );
    }
}

#[test]
fn smoke_unknown_method_is_refused_not_panicked() {
    // Hostile input must be refused politely. A panic here takes down the whole
    // server for every other tool.
    let ctx = smoke_ctx();
    let response = rpc::process_line(&ctx, &request(3, "method/that/does/not/exist"))
        .expect("an unknown method with an id must still get a response");

    assert!(
        response.get("error").is_some(),
        "an unknown method must return a JSON-RPC error, not a success: {response}"
    );
}

#[test]
fn smoke_malformed_input_does_not_panic() {
    let ctx = smoke_ctx();
    // Each of these has broken a naive line-oriented parser at some point.
    for junk in [
        "",
        "   ",
        "not json at all",
        "{",
        "[]",
        "null",
        r#"{"jsonrpc":"2.0"}"#,
        r#"{"jsonrpc":"2.0","id":1}"#,
    ] {
        // The contract is only that it must not panic; returning None (treat as
        // a notification / ignore) is a legitimate answer for several of these.
        let _ = rpc::process_line(&ctx, junk);
    }
}

#[test]
fn smoke_signature_hash_is_deterministic_and_non_empty() {
    // Signature hashing is how triage groups repeated errors. If it were
    // non-deterministic, every occurrence would look novel and the dedup that
    // the whole triage loop depends on would silently stop working.
    let a = signature::signature_hash(Some("SPOT1M-01"), "tickvault::app", "fetch failed");
    let b = signature::signature_hash(Some("SPOT1M-01"), "tickvault::app", "fetch failed");
    assert_eq!(a, b, "same input must hash identically across calls");
    assert!(!a.is_empty(), "signature hash must not be empty");

    let different = signature::signature_hash(Some("CHAIN-02"), "tickvault::app", "fetch failed");
    assert_ne!(
        a, different,
        "a different error code must produce a different signature, or triage \
         would collapse unrelated errors into one"
    );
}
