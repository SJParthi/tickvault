//! In-process JSON-RPC coverage for the tickvault-logs MCP server.
//!
//! Replaces the retired subprocess-based parity harness
//! (`tests/parity.rs`, deleted — it re-materialized and EXECUTED the
//! banned legacy runtime from pinned git history as a child process,
//! which the rust-only-forever lock now forbids). This suite drives the
//! SAME public entry points a real MCP client hits —
//! [`tickvault_logs_mcp::rpc::process_line`] over real JSON-RPC 2.0 request
//! lines — but entirely IN-PROCESS: no interpreter is spawned, no child
//! MCP server process exists, nothing is executed from git history.
//!
//! What is still exercised for real, because these are the tool's OWN
//! documented behaviors (not the retired transport mechanism):
//!   - `run_doctor` / `git_recent_log` / `docker_status` spawn real local
//!     `bash` / `git` / `docker` binaries (exactly like production) —
//!     this file never calls `Command::new` itself; it only drives the
//!     library, which does the spawning internally.
//!   - `questdb_sql` / `tickvault_api` talk to a LOCAL loopback mock HTTP
//!     server this file starts on `127.0.0.1` (never a real network host).
//!   - `cloudwatch_logs` is asked to run through its full SigV4 / portal /
//!     `aws` CLI fallback chain. The deterministic validation branches
//!     (invalid region, plaintext portal refusal) never touch the network.
//!     The one branch that does reach a real `aws` CLI invocation is
//!     EXPECTED to fail in this sandbox (no working credential chain) —
//!     per the task brief, that failure is asserted gracefully and never
//!     required to succeed.
//!
//! The banned legacy runtime's name is never spelled here (a workspace
//! guard scans for it, case-insensitively, across `crates/`).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

use tickvault_logs_mcp::config::{Ctx, EndpointsConfig};
use tickvault_logs_mcp::rpc;

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Repo root for tests that deliberately point at the REAL checkout
/// (read-only `git log`, the real `deploy/docker/docker-compose.yml`).
fn real_repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn write_fixture(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir fixture parent");
    }
    std::fs::write(path, content).expect("write fixture file");
}

/// A fresh, uniquely-named temp directory for one test's fixture tree.
fn fixture_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "tv-logs-mcp-inprocess-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir fixture root");
    root
}

fn ctx_at(root: &Path) -> Ctx {
    Ctx {
        repo_root: root.to_path_buf(),
        cfg: EndpointsConfig::default(),
    }
}

fn empty_ctx() -> Ctx {
    ctx_at(&fixture_root("empty"))
}

/// Build one `tools/call` JSON-RPC request line and drive it through
/// [`rpc::process_line`] exactly as a real client's stdio frame would.
fn call_tool_via_line(ctx: &Ctx, id: i64, tool: &str, args: Value) -> Value {
    let line = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    })
    .to_string();
    rpc::process_line(ctx, &line)
        .unwrap_or_else(|| panic!("tools/call for `{tool}` produced no response line"))
}

/// Parse a successful `tools/call` response's inner `content[0].text` JSON
/// payload (every tool wraps its result this way).
fn inner_result(resp: &Value) -> Value {
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no content[0].text in response: {resp}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("content text not JSON ({e}): {text}"))
}

// ---------------------------------------------------------------------------
// Process-env serialization — TICKVAULT_*/AWS_* are process-global; only
// the tests that explicitly override them via `EnvGuard` ever write them,
// and they always do so through this single shared lock so two such tests
// never interleave their writes.
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    fn set(pairs: &[(&str, &str)]) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut saved = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            saved.push(((*k).to_string(), std::env::var(k).ok()));
            // SAFETY: every writer of these env vars in this test binary
            // goes through `EnvGuard`, which holds `ENV_LOCK` for its whole
            // lifetime — writes are serialized process-wide.
            unsafe { std::env::set_var(k, v) };
        }
        Self { _lock: lock, saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            match v {
                // SAFETY: see `EnvGuard::set`.
                Some(v) => unsafe { std::env::set_var(&k, v) },
                None => unsafe { std::env::remove_var(&k) },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Local loopback mock HTTP server (questdb_sql / tickvault_api targets)
// ---------------------------------------------------------------------------

fn spawn_mock_http() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
    let port = listener.local_addr().expect("mock listener addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut req: Vec<u8> = Vec::new();
                loop {
                    let Ok(n) = s.read(&mut buf) else { return };
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&req);
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status_line, ctype, body): (&str, &str, String) = if path
                    .starts_with("/exec?query=SELECT")
                {
                    (
                        "200 OK",
                        "application/json",
                        r#"{"query":"SELECT 1","columns":[{"name":"1","type":"INT"}],"dataset":[[1]],"count":1}"#
                            .to_string(),
                    )
                } else if path.starts_with("/exec?query=BADJSON") {
                    ("200 OK", "application/json", "not-a-json-body".to_string())
                } else if path.starts_with("/exec") {
                    (
                        "400 Bad Request",
                        "application/json",
                        r#"{"error":"bad query"}"#.to_string(),
                    )
                } else if path == "/health" {
                    (
                        "200 OK",
                        "application/json",
                        r#"{"status":"ok","service":"tickvault"}"#.to_string(),
                    )
                } else if path == "/text" {
                    ("200 OK", "text/plain", "hello mcp\nline two".to_string())
                } else {
                    ("404 Not Found", "text/plain", "nope".to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    port
}

/// A port nothing is listening on (bind-then-drop) — used to exercise the
/// HTTP transport-error branch deterministically and fast (connection
/// refused on loopback, no network egress).
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
    let port = listener.local_addr().expect("probe addr").port();
    drop(listener);
    port
}

// ---------------------------------------------------------------------------
// 1. JSON-RPC framing: initialize / tools/list / unknown tool / unknown
//    method / notification / parse error / blank line.
// ---------------------------------------------------------------------------

#[test]
fn initialize_tools_list_and_rpc_framing() {
    let ctx = empty_ctx();

    let resp = rpc::process_line(
        &ctx,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}).to_string(),
    )
    .expect("initialize responds");
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(resp["result"]["capabilities"]["tools"], json!({}));
    assert_eq!(resp["result"]["serverInfo"]["name"], "tickvault-logs");

    // A notification produces no response line at all.
    assert!(
        rpc::process_line(
            &ctx,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string()
        )
        .is_none()
    );

    let resp = rpc::process_line(
        &ctx,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string(),
    )
    .expect("tools/list responds");
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 14);
    let expected_names = [
        "tail_errors",
        "list_novel_signatures",
        "summary_snapshot",
        "triage_log_tail",
        "signature_history",
        "find_runbook_for_code",
        "questdb_sql",
        "grep_codebase",
        "run_doctor",
        "git_recent_log",
        "tickvault_api",
        "docker_status",
        "app_log_tail",
        "cloudwatch_logs",
    ];
    for (i, name) in expected_names.iter().enumerate() {
        assert_eq!(tools[i]["name"], *name, "tool #{i} name mismatch");
        assert_eq!(tools[i]["inputSchema"]["type"], "object");
        let desc = tools[i]["description"]
            .as_str()
            .expect("description is a string");
        assert!(desc.len() > 10, "tool `{name}` description looks empty");
    }

    // unknown tool -> -32601, name echoed via the legacy `f"{value}"` shape.
    let resp = rpc::process_line(
        &ctx,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "nope"}})
            .to_string(),
    )
    .expect("tools/call responds");
    assert_eq!(resp["error"]["code"], -32601);
    assert_eq!(resp["error"]["message"], "unknown tool: nope");

    // unknown method -> -32601.
    let resp = rpc::process_line(
        &ctx,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "bogus/x"}).to_string(),
    )
    .expect("bogus method responds");
    assert_eq!(resp["error"]["code"], -32601);
    assert_eq!(resp["error"]["message"], "method not found: bogus/x");

    // Missing `method` entirely -> "" per the legacy `req.get("method", "")`.
    let resp =
        rpc::process_line(&ctx, &json!({"jsonrpc": "2.0", "id": 5}).to_string()).expect("responds");
    assert_eq!(resp["error"]["message"], "method not found: ");

    // Parse error: id is always null, code -32700.
    let resp = rpc::process_line(&ctx, "{not json").expect("parse error responds");
    assert_eq!(resp["id"], Value::Null);
    assert_eq!(resp["error"]["code"], -32700);
    assert_eq!(resp["error"]["message"], "parse error");

    // Blank / whitespace-only lines produce no response at all.
    assert!(rpc::process_line(&ctx, "").is_none());
    assert!(rpc::process_line(&ctx, "   \t").is_none());

    // The server keeps serving after all of the above (no panics, no
    // process-ending side effects from any prior call).
    let resp = rpc::process_line(
        &ctx,
        &json!({"jsonrpc": "2.0", "id": 6, "method": "initialize"}).to_string(),
    )
    .expect("still serving after the whole transcript");
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
}

#[test]
fn bad_argument_types_are_a_typed_error_not_a_panic() {
    let ctx = empty_ctx();

    let resp = call_tool_via_line(&ctx, 1, "tickvault_api", json!({"path": 42}));
    assert_eq!(resp["error"]["code"], -32000);
    assert!(
        resp["error"]["message"]
            .as_str()
            .expect("error message is a string")
            .contains("must be a string")
    );

    let resp = call_tool_via_line(&ctx, 2, "tail_errors", json!({"limit": "not-a-number"}));
    assert_eq!(resp["error"]["code"], -32000);

    let resp = call_tool_via_line(&ctx, 3, "grep_codebase", json!({"pattern": true}));
    assert_eq!(resp["error"]["code"], -32000);

    // Missing required args map to the legacy KeyError-shaped -32000 text.
    let resp = call_tool_via_line(&ctx, 4, "signature_history", json!({}));
    assert_eq!(resp["error"]["code"], -32000);
    assert_eq!(
        resp["error"]["message"],
        "tool signature_history failed: 'signature'"
    );

    let resp = call_tool_via_line(&ctx, 5, "find_runbook_for_code", json!({}));
    assert_eq!(resp["error"]["code"], -32000);
    assert_eq!(
        resp["error"]["message"],
        "tool find_runbook_for_code failed: 'code'"
    );

    let resp = call_tool_via_line(&ctx, 6, "questdb_sql", json!({}));
    assert_eq!(resp["error"]["code"], -32000);

    let resp = call_tool_via_line(&ctx, 7, "tickvault_api", json!({}));
    assert_eq!(resp["error"]["code"], -32000);
}

// ---------------------------------------------------------------------------
// 2. File-backed tools: tail_errors, list_novel_signatures,
//    summary_snapshot, triage_log_tail, signature_history,
//    find_runbook_for_code, app_log_tail.
// ---------------------------------------------------------------------------

fn build_error_fixtures(root: &Path) -> PathBuf {
    let logs = root.join("data").join("logs");
    let machine = logs.join("machine");
    // Timestamps are pinned to 2020 (safely in the past relative to any
    // real wall clock this suite ever runs under) so the since_minutes=0
    // "nothing is novel yet" assertion below can never flake against the
    // real clock.
    write_fixture(
        &machine.join("errors.jsonl.2020-01-01-05"),
        concat!(
            r#"{"timestamp":"2020-01-01T05:10:00.123456Z","level":"ERROR","code":"DH-904","target":"tickvault_core::oms","message":"rate limited","severity":"high"}"#,
            "\n",
            "this line is not json\n",
            r#"{"timestamp":"2020-01-01T05:11:30Z","level":"ERROR","target":"tickvault_core::websocket","message":"socket closed"}"#,
            "\n",
            "\n",
            r#"{"timestamp":"2020-01-01T05:12:00.000001Z","level":"ERROR","code":"WS-GAP-05","target":"tickvault_core::websocket::connection_pool","message":"pool slot respawned"}"#,
            "\n",
        ),
    );
    write_fixture(
        &machine.join("errors.jsonl.2020-01-01-06"),
        concat!(
            r#"{"timestamp":"2020-01-01T06:01:00Z","level":"ERROR","code":"DH-904","target":"tickvault_core::oms","message":"rate limited"}"#,
            "\n",
            r#"{"timestamp":"2020-01-01T06:02:00Z","level":"ERROR","code":"BOOT-01","target":"tickvault_app::infra","message":"questdb readiness deadline approaching","severity":"high"}"#,
            "\n",
        ),
    );
    write_fixture(
        &machine.join(tickvault_logs_mcp::tools::SUMMARY_FILENAME),
        "# Error summary\n\n| sig | count |\n|---|---|\n| 500cf7253d9e7649 | 2 |\n",
    );
    write_fixture(
        &logs.join(tickvault_logs_mcp::tools::AUTO_FIX_LOG),
        concat!(
            "2020-01-01 05:00:01 triage: rule clear-spill matched\n",
            "2020-01-01 05:00:02 triage: executed ok\n",
            "2020-01-01 05:10:00 triage: rule dh-904 matched\n",
            "2020-01-01 05:10:01 triage: escalated to operator\n",
        ),
    );
    write_fixture(
        &root.join("docs").join("runbooks").join("zzz-fixture.md"),
        concat!(
            "# ZZZ-TEST-01 fixture runbook\n\n",
            "context before\n",
            "ZZZ-TEST-01 fixture line for lookup\n",
            "context after\n",
            "more trailing context\n",
        ),
    );
    logs
}

#[test]
fn file_backed_tools_success_and_edge_cases() {
    let root = fixture_root("files");
    let logs_dir = build_error_fixtures(&root);
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    write_fixture(
        &logs_dir.join(format!("app.{today}.log")),
        "boot step 1 config ok\nboot step 2 observability ok\nboot step 3 logging ok\n",
    );
    write_fixture(
        &logs_dir.join("app.2020-01-01.log"),
        "fixed-date line one\nfixed-date line two\n",
    );
    let ctx = ctx_at(&root);

    // tail_errors: defaults collect every parseable event across both
    // rotations, newest rotation first (5 valid JSON lines total: 3 in the
    // -05 file, 2 in the -06 file — the two non-JSON / blank lines skip).
    let resp = call_tool_via_line(&ctx, 1, "tail_errors", json!({}));
    let out = inner_result(&resp);
    assert_eq!(out["count"], 5);
    assert_eq!(
        out["files_scanned"]
            .as_array()
            .expect("files_scanned array")
            .len(),
        2
    );

    // tail_errors: limit + code filter narrows to exactly one event.
    let resp = call_tool_via_line(
        &ctx,
        2,
        "tail_errors",
        json!({"limit": 1, "code": "DH-904"}),
    );
    let out = inner_result(&resp);
    assert_eq!(out["count"], 1);
    assert_eq!(out["events"][0]["code"], "DH-904");

    // list_novel_signatures: an empty lookback window can never include
    // anything from 2020.
    let resp = call_tool_via_line(
        &ctx,
        3,
        "list_novel_signatures",
        json!({"since_minutes": 0}),
    );
    let out = inner_result(&resp);
    assert_eq!(out["novel_count"], 0);

    // list_novel_signatures: a huge lookback includes every DISTINCT
    // signature (the two DH-904 rows share one signature — dedup by
    // code|target|message, severity is not part of the hash — so 5 raw
    // events collapse to 4 distinct signatures).
    let resp = call_tool_via_line(
        &ctx,
        4,
        "list_novel_signatures",
        json!({"since_minutes": 99_999_999_i64}),
    );
    let out = inner_result(&resp);
    assert_eq!(out["novel_count"], 4);

    // list_novel_signatures: the 32-bit C-int overflow band must degrade to
    // a typed -32000 error, never a process abort, and the exact wire
    // message is checked only by its safe (non-runtime-naming) suffix.
    let resp = call_tool_via_line(
        &ctx,
        5,
        "list_novel_signatures",
        json!({"since_minutes": i64::MAX}),
    );
    assert_eq!(resp["error"]["code"], -32000);
    let msg = resp["error"]["message"]
        .as_str()
        .expect("error message is a string");
    assert!(
        msg.ends_with("too large to convert to C int"),
        "unexpected overflow message: {msg}"
    );

    // summary_snapshot: existing file.
    let resp = call_tool_via_line(&ctx, 6, "summary_snapshot", json!({}));
    let out = inner_result(&resp);
    assert_eq!(out["exists"], true);
    assert!(
        out["markdown"]
            .as_str()
            .expect("markdown is a string")
            .contains("500cf7253d9e7649")
    );

    // summary_snapshot: missing file -> exists:false, empty markdown.
    let no_summary_ctx = empty_ctx();
    let resp = call_tool_via_line(&no_summary_ctx, 7, "summary_snapshot", json!({}));
    let out = inner_result(&resp);
    assert_eq!(out["exists"], false);
    assert_eq!(out["markdown"], "");

    // triage_log_tail: limit + default.
    let resp = call_tool_via_line(&ctx, 8, "triage_log_tail", json!({"limit": 2}));
    let out = inner_result(&resp);
    assert_eq!(out["returned"], 2);
    assert_eq!(out["total_lines"], 4);

    let resp = call_tool_via_line(&ctx, 9, "triage_log_tail", json!({}));
    let out = inner_result(&resp);
    assert_eq!(out["exists"], true);
    assert_eq!(out["returned"], 4);

    // signature_history: the well-known golden vector for
    // (DH-904, tickvault_core::oms, "rate limited") — both fixture rows
    // hash identically regardless of the `severity` field.
    let known_sig = tickvault_logs_mcp::signature::signature_hash(
        Some("DH-904"),
        "tickvault_core::oms",
        "rate limited",
    );
    assert_eq!(known_sig, "500cf7253d9e7649");
    let resp = call_tool_via_line(
        &ctx,
        10,
        "signature_history",
        json!({"signature": known_sig, "limit": 10}),
    );
    let out = inner_result(&resp);
    assert_eq!(out["count"], 2);

    // signature_history: unknown signature -> zero matches, no error.
    let resp = call_tool_via_line(
        &ctx,
        11,
        "signature_history",
        json!({"signature": "0000000000000000"}),
    );
    let out = inner_result(&resp);
    assert_eq!(out["count"], 0);

    // find_runbook_for_code: match, with a preview window around the hit.
    let resp = call_tool_via_line(
        &ctx,
        12,
        "find_runbook_for_code",
        json!({"code": "ZZZ-TEST-01"}),
    );
    let out = inner_result(&resp);
    assert_eq!(out["match_count"], 1);
    let file = out["matches"][0]["file"]
        .as_str()
        .expect("file is a string");
    assert!(
        file.contains("zzz-fixture.md"),
        "unexpected match file: {file}"
    );
    assert!(
        out["matches"][0]["preview"]
            .as_str()
            .expect("preview is a string")
            .contains("ZZZ-TEST-01")
    );

    // find_runbook_for_code: no match anywhere.
    let resp = call_tool_via_line(
        &ctx,
        13,
        "find_runbook_for_code",
        json!({"code": "ZZZ-NOPE-99"}),
    );
    let out = inner_result(&resp);
    assert_eq!(out["match_count"], 0);
    assert_eq!(out["matches"].as_array().expect("matches array").len(), 0);

    // app_log_tail: default date resolves to "today" (UTC).
    let resp = call_tool_via_line(&ctx, 14, "app_log_tail", json!({"limit": 2}));
    let out = inner_result(&resp);
    assert_eq!(out["ok"], true);
    assert_eq!(out["returned"], 2);

    // app_log_tail: explicit date.
    let resp = call_tool_via_line(&ctx, 15, "app_log_tail", json!({"date": "2020-01-01"}));
    let out = inner_result(&resp);
    assert_eq!(out["ok"], true);
    assert_eq!(out["total_lines"], 2);
    assert_eq!(out["lines"][0], "fixed-date line one");

    // app_log_tail: no log for that date -> a graceful, typed error.
    let resp = call_tool_via_line(&ctx, 16, "app_log_tail", json!({"date": "1999-01-01"}));
    let out = inner_result(&resp);
    assert_eq!(out["ok"], false);
    assert!(
        out["error"]
            .as_str()
            .expect("error is a string")
            .contains("log file not found")
    );
}

// ---------------------------------------------------------------------------
// 3. grep_codebase — success, invalid regex, and the "outside the repo
//    root" fail-open / fail-closed pair.
// ---------------------------------------------------------------------------

#[test]
fn grep_codebase_success_and_error_paths() {
    let root = fixture_root("grep");
    write_fixture(
        &root.join("crates").join("demo").join("src").join("lib.rs"),
        "fn tv_inprocess_grep_marker() {}\n// second line\n",
    );
    write_fixture(
        &root
            .join("crates")
            .join("demo")
            .join("src")
            .join("other.txt"),
        "tv_inprocess_grep_marker appears here too, but this is not a .rs file\n",
    );
    let ctx = ctx_at(&root);

    // Scoped path + glob: only the .rs file matches.
    let resp = call_tool_via_line(
        &ctx,
        1,
        "grep_codebase",
        json!({"pattern": "tv_inprocess_grep_marker", "path": "crates/demo/src", "file_glob": "*.rs"}),
    );
    let out = inner_result(&resp);
    assert_eq!(out["ok"], true);
    assert_eq!(out["match_count"], 1);
    assert_eq!(out["matches"][0]["file"], "crates/demo/src/lib.rs");
    assert_eq!(out["matches"][0]["line"], 1);

    // Invalid regex -> a graceful ok:false with a masked-but-prefixed error.
    let resp = call_tool_via_line(&ctx, 2, "grep_codebase", json!({"pattern": "((("}));
    let out = inner_result(&resp);
    assert_eq!(out["ok"], false);
    assert!(
        out["error"]
            .as_str()
            .expect("error is a string")
            .starts_with("invalid regex: ")
    );

    // Absolute path OUTSIDE the repo root, WITH a match, must error via the
    // -32000 dispatcher wrap (fail-closed: never silently succeeds with a
    // path that escapes the intended search root).
    let outside = fixture_root("grep-outside");
    write_fixture(
        &outside.join("needle.txt"),
        "TV_INPROCESS_OUTSIDE_NEEDLE one line\n",
    );
    let resp = call_tool_via_line(
        &ctx,
        3,
        "grep_codebase",
        json!({"pattern": "TV_INPROCESS_OUTSIDE_NEEDLE", "path": outside.to_string_lossy()}),
    );
    assert_eq!(resp["error"]["code"], -32000);
    assert!(
        resp["error"]["message"]
            .as_str()
            .expect("error message is a string")
            .contains("is not in the subpath of")
    );

    // Same outside directory, but with NO match: the outside-root check is
    // only reached on a hit, so this stays a normal empty success.
    let resp = call_tool_via_line(
        &ctx,
        4,
        "grep_codebase",
        json!({"pattern": "NO_SUCH_NEEDLE_ANYWHERE", "path": outside.to_string_lossy()}),
    );
    let out = inner_result(&resp);
    assert_eq!(out["ok"], true);
    assert_eq!(out["match_count"], 0);
}

// ---------------------------------------------------------------------------
// 4. Subprocess-backed tools: run_doctor, git_recent_log, docker_status.
//    The library spawns real `bash`/`git`/`docker` — this file never spawns
//    anything itself.
// ---------------------------------------------------------------------------

#[test]
fn run_doctor_success_and_missing_script() {
    let root = fixture_root("doctor");
    write_fixture(
        &root.join("scripts").join("doctor.sh"),
        concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' '[PASS] questdb reachable (fixture)'\n",
            "printf '%s\\n' '[FAIL] disk usage 91%'\n",
            "printf '%s\\n' '[WARN] token headroom 3h'\n",
            "exit 0\n",
        ),
    );
    let ctx = ctx_at(&root);
    let resp = call_tool_via_line(&ctx, 1, "run_doctor", json!({}));
    let out = inner_result(&resp);
    assert_eq!(out["ok"], true);
    assert_eq!(out["pass_count"], 1);
    assert_eq!(out["fail_count"], 1);
    assert_eq!(out["rows"].as_array().expect("rows array").len(), 3);

    // No scripts/doctor.sh at all: bash itself is still available and
    // exits non-zero — a graceful ok:false, never a panic.
    let missing_ctx = empty_ctx();
    let resp = call_tool_via_line(&missing_ctx, 2, "run_doctor", json!({}));
    let out = inner_result(&resp);
    assert_eq!(out["ok"], false);
}

#[test]
fn git_recent_log_success_against_real_repo_and_error_on_non_repo() {
    // Read-only `git log` against the REAL checkout — safe, fast, and
    // exercises the real success-parsing path end to end.
    let real_ctx = ctx_at(&real_repo_root());
    let resp = call_tool_via_line(&real_ctx, 1, "git_recent_log", json!({"limit": 3}));
    let out = inner_result(&resp);
    assert_eq!(out["ok"], true);
    let commits = out["commits"].as_array().expect("commits array");
    assert!(!commits.is_empty() && commits.len() <= 3);
    for commit in commits {
        assert!(commit["sha"].as_str().expect("sha is a string").len() <= 12);
        assert!(commit["author"].is_string());
        assert!(commit["subject"].is_string());
    }

    // A fixture directory that is not a git repository at all -> `git log`
    // fails, and the failure is surfaced gracefully.
    let non_repo_ctx = empty_ctx();
    let resp = call_tool_via_line(&non_repo_ctx, 2, "git_recent_log", json!({"limit": 2}));
    let out = inner_result(&resp);
    assert_eq!(out["ok"], false);
    assert!(out["error"].is_string());
}

#[test]
fn docker_status_missing_compose_file_and_real_repo_subprocess_path() {
    // No deploy/docker/docker-compose.yml in the fixture tree -> the tool
    // never even spawns `docker` and returns a typed error immediately.
    let ctx = empty_ctx();
    let resp = call_tool_via_line(&ctx, 1, "docker_status", json!({}));
    let out = inner_result(&resp);
    assert_eq!(out["ok"], false);
    assert!(
        out["error"]
            .as_str()
            .expect("error is a string")
            .contains("compose file not found")
    );

    // The REAL repo's compose file exists, so this reaches the real
    // `docker compose ps` subprocess call. Whether the local docker
    // toolchain can actually enumerate the stack in this sandbox is
    // environment-dependent (no daemon / missing secrets are both
    // deterministic, fast failures observed in this sandbox) — assert a
    // well-formed, non-panicking response either way, per the task's
    // "AWS/network/subprocess-dependent tools may fail here" allowance.
    let real_ctx = ctx_at(&real_repo_root());
    let resp = call_tool_via_line(&real_ctx, 2, "docker_status", json!({}));
    let out = inner_result(&resp);
    assert!(out["ok"].is_boolean());
    if out["ok"] == json!(false) {
        assert!(
            out.get("error").is_some() || out.get("exit_code").is_some(),
            "docker_status ok:false must carry either `error` or `exit_code`: {out}"
        );
    } else {
        assert!(out.get("containers").is_some());
    }
}

// ---------------------------------------------------------------------------
// 5. Network-backed tools: questdb_sql, tickvault_api against a local
//    loopback mock — never a real network host.
// ---------------------------------------------------------------------------

#[test]
fn questdb_sql_and_tickvault_api_against_local_mock() {
    let port = spawn_mock_http();
    let base = format!("http://127.0.0.1:{port}");
    let ctx = empty_ctx();

    {
        let _env = EnvGuard::set(&[("TICKVAULT_QUESTDB_URL", base.as_str())]);
        let resp = call_tool_via_line(&ctx, 1, "questdb_sql", json!({"query": "SELECT 1"}));
        let out = inner_result(&resp);
        assert_eq!(out["ok"], true);
        assert_eq!(out["response"]["count"], 1);
    }
    {
        let _env = EnvGuard::set(&[("TICKVAULT_QUESTDB_URL", base.as_str())]);
        let resp = call_tool_via_line(&ctx, 2, "questdb_sql", json!({"query": "BAD"}));
        let out = inner_result(&resp);
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"], "HTTP Error 400: Bad Request");
    }
    {
        let _env = EnvGuard::set(&[("TICKVAULT_QUESTDB_URL", base.as_str())]);
        // 200 OK with a non-JSON body must degrade to a graceful decode
        // error, never a panic.
        let resp = call_tool_via_line(&ctx, 3, "questdb_sql", json!({"query": "BADJSON"}));
        let out = inner_result(&resp);
        assert_eq!(out["ok"], false);
        assert!(out["error"].is_string());
    }
    {
        let dead_url = format!("http://127.0.0.1:{}", closed_port());
        let _env = EnvGuard::set(&[("TICKVAULT_QUESTDB_URL", dead_url.as_str())]);
        let resp = call_tool_via_line(&ctx, 4, "questdb_sql", json!({"query": "SELECT 1"}));
        let out = inner_result(&resp);
        assert_eq!(out["ok"], false);
        assert!(out["error"].is_string());
    }

    {
        let _env = EnvGuard::set(&[("TICKVAULT_API_URL", base.as_str())]);
        let resp = call_tool_via_line(&ctx, 5, "tickvault_api", json!({"path": "/health"}));
        let out = inner_result(&resp);
        assert_eq!(out["ok"], true);
        assert_eq!(out["json"]["status"], "ok");
    }
    {
        let _env = EnvGuard::set(&[("TICKVAULT_API_URL", base.as_str())]);
        // No leading slash -> the tool normalizes it, and a non-JSON body
        // falls back to the `text` field.
        let resp = call_tool_via_line(&ctx, 6, "tickvault_api", json!({"path": "text"}));
        let out = inner_result(&resp);
        assert_eq!(out["ok"], true);
        assert!(
            out["text"]
                .as_str()
                .expect("text is a string")
                .starts_with("hello mcp")
        );
    }
    {
        let _env = EnvGuard::set(&[("TICKVAULT_API_URL", base.as_str())]);
        let resp = call_tool_via_line(&ctx, 7, "tickvault_api", json!({"path": "/nope"}));
        let out = inner_result(&resp);
        assert_eq!(out["ok"], false);
        assert!(
            out["error"]
                .as_str()
                .expect("error is a string")
                .starts_with("HTTP Error 404")
        );
    }
    {
        let _env = EnvGuard::set(&[
            ("TICKVAULT_API_URL", base.as_str()),
            ("TICKVAULT_API_BEARER_TOKEN", "dummy-inprocess-test-token"),
        ]);
        // A bearer token must never cross a plaintext connection to a
        // non-local host — refused before any network attempt.
        let resp = call_tool_via_line(
            &ctx,
            8,
            "tickvault_api",
            json!({"path": "/health", "base_url": "http://example.invalid:9"}),
        );
        let out = inner_result(&resp);
        assert_eq!(out["ok"], false);
        assert!(
            out["error"]
                .as_str()
                .expect("error is a string")
                .contains("refusing to send")
        );

        // The SAME bearer token IS allowed over plaintext to localhost.
        let resp = call_tool_via_line(&ctx, 9, "tickvault_api", json!({"path": "/health"}));
        let out = inner_result(&resp);
        assert_eq!(out["ok"], true);
    }
}

// ---------------------------------------------------------------------------
// 6. cloudwatch_logs — SigV4 / portal / aws-CLI fallback chain.
// ---------------------------------------------------------------------------

#[test]
fn cloudwatch_logs_sigv4_and_portal_validation_never_touch_the_network() {
    let ctx = empty_ctx();

    // Credentials present (dummy, never real) but an invalid region is
    // rejected before any HTTP request is built or sent.
    {
        let _env = EnvGuard::set(&[
            ("AWS_ACCESS_KEY_ID", "AKIDINPROCESSTESTONLY0"),
            ("AWS_SECRET_ACCESS_KEY", "inprocess-test-secret-never-real"),
            ("AWS_DEFAULT_REGION", "Bad_Region!"),
            ("AWS_SESSION_TOKEN", ""),
            ("TICKVAULT_PORTAL_URL", ""),
            ("TICKVAULT_PORTAL_TOKEN", ""),
        ]);
        let resp = call_tool_via_line(
            &ctx,
            1,
            "cloudwatch_logs",
            json!({"minutes": 5, "limit": 5}),
        );
        let out = inner_result(&resp);
        assert_eq!(out["ok"], false);
        assert!(
            out["error"]
                .as_str()
                .expect("error is a string")
                .starts_with("invalid AWS region")
        );
    }

    // No AWS credentials at all -> the portal path is selected; a
    // plaintext (non-https) portal URL is refused before any network call.
    {
        let _env = EnvGuard::set(&[
            ("AWS_ACCESS_KEY_ID", ""),
            ("AWS_SECRET_ACCESS_KEY", ""),
            ("TICKVAULT_PORTAL_URL", "http://portal.invalid"),
            ("TICKVAULT_PORTAL_TOKEN", "pt-inprocess-test-token"),
        ]);
        let resp = call_tool_via_line(&ctx, 2, "cloudwatch_logs", json!({"limit": 5}));
        let out = inner_result(&resp);
        assert_eq!(out["ok"], false);
        assert!(
            out["error"]
                .as_str()
                .expect("error is a string")
                .contains("https://")
        );
    }

    // Neither AWS credentials nor a portal configured -> falls through to
    // the read-only `aws` CLI fallback. This DOES reach the real `aws`
    // binary; with no credential chain configured it is expected to fail
    // fast (verified in this sandbox: an immediate "NoCredentials" class
    // error, no network egress). Per the task brief this failure is
    // graceful and acceptable — asserted, never required to succeed.
    {
        let _env = EnvGuard::set(&[
            ("AWS_ACCESS_KEY_ID", ""),
            ("AWS_SECRET_ACCESS_KEY", ""),
            ("TICKVAULT_PORTAL_URL", ""),
            ("TICKVAULT_PORTAL_TOKEN", ""),
        ]);
        let resp = call_tool_via_line(
            &ctx,
            3,
            "cloudwatch_logs",
            json!({"minutes": 1, "limit": 1}),
        );
        let out = inner_result(&resp);
        assert!(out["ok"].is_boolean());
        if out["ok"] == json!(false) {
            assert!(out.get("error").is_some());
        }
    }
}

// ---------------------------------------------------------------------------
// 7. The aggregate boot entry point main.rs actually calls.
// ---------------------------------------------------------------------------

#[test]
fn ctx_from_process_env_resolves_a_sane_repo_root() {
    // No env override is set up here (`TICKVAULT_MCP_REPO_ROOT` is never
    // touched by any test in this file), so this exercises the real
    // walk-up-from-cwd fallback `main.rs` relies on in production.
    let ctx = Ctx::from_process_env();
    assert!(ctx.repo_root.is_dir());
    assert!(
        ctx.repo_root.join(".mcp.json").is_file()
            || ctx
                .repo_root
                .join("config")
                .join("claude-mcp-endpoints.toml")
                .is_file(),
        "resolved repo root {:?} carries neither repo-root marker",
        ctx.repo_root
    );
    // Every derived path must be well-formed regardless of which profile
    // resolved.
    assert!(!ctx.logs_dir().as_os_str().is_empty());
    assert!(!ctx.machine_logs_dir().as_os_str().is_empty());
}
