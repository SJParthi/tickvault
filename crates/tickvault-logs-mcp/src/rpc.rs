//! Hand-rolled JSON-RPC 2.0 over newline-delimited stdio — the exact
//! MCP 2024-11-05 subset server.py implements: `initialize`,
//! `tools/list`, `tools/call`, `notifications/initialized` (silent),
//! unknown method → -32601, parse error → -32700 with `id: null`.
//!
//! Documented bounded deviations from CPython (transcript never hits
//! them): a syntactically-valid NON-OBJECT request line (e.g. `5`) gets a
//! "method not found" error here where Python would crash with an
//! AttributeError; a truthy non-object `params` / `arguments` is treated
//! as empty here where Python would raise mid-handler.

use std::io::{BufRead, Write};

use serde_json::{Map, Value, json};

use crate::config::Ctx;
use crate::tools;

/// Python `f"{value}"` rendering of a JSON value pulled from the request
/// (`unknown tool: {name}` / `method not found: {method}`): `None`,
/// `True`/`False`, bare strings, number repr.
fn py_display(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn envelope_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn envelope_error(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

/// Handle one parsed request. `None` = notification (no response line).
pub fn handle_request(ctx: &Ctx, req: &Value) -> Option<Value> {
    // Python: req_id = req.get("id") — missing id serializes as null.
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    // Python: method = req.get("method", "") — string compare below, so a
    // non-string method never matches and falls through to -32601.
    let method_val = req
        .get("method")
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    let method = method_val.as_str().unwrap_or("");
    // Python: params = req.get("params") or {} (falsy → {}).
    let params: Map<String, Value> = match req.get("params") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };

    match method {
        "initialize" => Some(envelope_result(
            &id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "tickvault-logs",
                    "version": "0.1.0",
                },
            }),
        )),
        "tools/list" => Some(envelope_result(
            &id,
            json!({"tools": tools::tools_list_json()}),
        )),
        "tools/call" => {
            let name_val = params.get("name").cloned().unwrap_or(Value::Null);
            let name = name_val.as_str().unwrap_or("");
            // Python: arguments = params.get("arguments") or {}.
            let arguments: Map<String, Value> = match params.get("arguments") {
                Some(Value::Object(m)) => m.clone(),
                _ => Map::new(),
            };
            match tools::call_tool(ctx, name, &arguments) {
                Some(Ok(result)) => {
                    // Python: json.dumps(result, indent=2) — 2-space
                    // indent, ": " separators; serde_json pretty matches
                    // the shape (key ORDER differs: sorted vs insertion —
                    // the harness's documented normalization).
                    let text = serde_json::to_string_pretty(&result).unwrap_or_default();
                    Some(envelope_result(
                        &id,
                        json!({"content": [{"type": "text", "text": text}]}),
                    ))
                }
                Some(Err(msg)) => Some(envelope_error(
                    &id,
                    -32000,
                    &format!("tool {} failed: {msg}", py_display(&name_val)),
                )),
                None => Some(envelope_error(
                    &id,
                    -32601,
                    &format!("unknown tool: {}", py_display(&name_val)),
                )),
            }
        }
        "notifications/initialized" => None,
        _ => Some(envelope_error(
            &id,
            -32601,
            &format!("method not found: {}", py_display(&method_val)),
        )),
    }
}

/// One stdin line → optional response value. Mirrors `_run_stdio_loop`'s
/// per-line body: strip, skip empty, parse error → `id: null` -32700.
pub fn process_line(ctx: &Ctx, raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(req) => handle_request(ctx, &req),
        Err(_) => Some(envelope_error(&Value::Null, -32700, "parse error")),
    }
}

/// The stdio loop: newline-delimited requests in, newline-delimited
/// responses out, flush after every response (server.py flushes per
/// write).
pub fn run_stdio_loop(ctx: &Ctx) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(raw) = line else {
            break;
        };
        let Some(resp) = process_line(ctx, &raw) else {
            continue;
        };
        if let Ok(s) = serde_json::to_string(&resp) {
            let _ignored = writeln!(out, "{s}");
            let _ignored = out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EndpointsConfig;

    fn test_ctx() -> Ctx {
        // A fresh, empty repo root — the rpc-layer tests below only
        // dispatch tools that tolerate missing dirs.
        let root = std::env::temp_dir().join(format!(
            "tv-logs-mcp-rpc-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ignored = std::fs::create_dir_all(&root);
        Ctx {
            repo_root: root,
            cfg: EndpointsConfig::default(),
        }
    }

    #[test]
    fn initialize_shape_matches_server_py() {
        let ctx = test_ctx();
        let resp = handle_request(&ctx, &json!({"id": 1, "method": "initialize"})).unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(resp["result"]["serverInfo"]["name"], "tickvault-logs");
        assert_eq!(resp["result"]["serverInfo"]["version"], "0.1.0");
    }

    #[test]
    fn tools_list_has_all_14_tools() {
        let ctx = test_ctx();
        let resp = handle_request(&ctx, &json!({"id": 2, "method": "tools/list"})).unwrap();
        let arr = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(arr.len(), 14);
        for tool in arr {
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn unknown_tool_message_matches_python_fstring() {
        let ctx = test_ctx();
        let resp = handle_request(
            &ctx,
            &json!({"id": 3, "method": "tools/call", "params": {"name": "nope"}}),
        )
        .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
        assert_eq!(resp["error"]["message"], "unknown tool: nope");

        // Missing name → Python `None`.
        let resp = handle_request(&ctx, &json!({"id": 4, "method": "tools/call"})).unwrap();
        assert_eq!(resp["error"]["message"], "unknown tool: None");
    }

    #[test]
    fn tool_error_maps_to_32000_with_python_keyerror_shape() {
        let ctx = test_ctx();
        // signature_history without `signature` → Python KeyError repr `'signature'`.
        let resp = handle_request(
            &ctx,
            &json!({
                "id": 5,
                "method": "tools/call",
                "params": {"name": "signature_history", "arguments": {}},
            }),
        )
        .unwrap();
        assert_eq!(resp["error"]["code"], -32000);
        assert_eq!(
            resp["error"]["message"],
            "tool signature_history failed: 'signature'"
        );
    }

    #[test]
    fn unknown_method_and_notification() {
        let ctx = test_ctx();
        let resp = handle_request(&ctx, &json!({"id": 6, "method": "bogus/x"})).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
        assert_eq!(resp["error"]["message"], "method not found: bogus/x");

        // Missing method → "" per Python req.get("method", "").
        let resp = handle_request(&ctx, &json!({"id": 7})).unwrap();
        assert_eq!(resp["error"]["message"], "method not found: ");

        assert!(
            handle_request(&ctx, &json!({"method": "notifications/initialized"})).is_none(),
            "notifications must be silent"
        );
    }

    #[test]
    fn process_line_parse_error_and_blank_skip() {
        let ctx = test_ctx();
        assert!(process_line(&ctx, "").is_none());
        assert!(process_line(&ctx, "   \t").is_none());
        let resp = process_line(&ctx, "{not json").unwrap();
        assert_eq!(resp["id"], Value::Null);
        assert_eq!(resp["error"]["code"], -32700);
        assert_eq!(resp["error"]["message"], "parse error");
    }

    #[test]
    fn tools_call_success_wraps_pretty_text_content() {
        let ctx = test_ctx();
        // find_runbook_for_code on a temp repo root: no runbook dirs →
        // zero matches, no filesystem dependency beyond missing dirs.
        let resp = handle_request(
            &ctx,
            &json!({
                "id": 8,
                "method": "tools/call",
                "params": {"name": "find_runbook_for_code", "arguments": {"code": "ZZZ-00"}},
            }),
        )
        .unwrap();
        let content = resp["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        let text = content[0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["code"], "ZZZ-00");
        assert_eq!(parsed["match_count"], 0);
        // indent=2 shape.
        assert!(text.contains("\n  \""));
    }
}
