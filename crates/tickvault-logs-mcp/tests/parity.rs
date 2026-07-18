//! PARITY HARNESS (PR 2c evidence): drives BOTH the live Python
//! `scripts/mcp-servers/tickvault-logs/server.py` and the Rust binary
//! over an IDENTICAL scripted JSON-RPC transcript and diffs every
//! response after a small, explicitly-documented normalization.
//!
//! NORMALIZATION (the complete list — nothing else is touched):
//!  1. KEY ORDER: every response envelope AND every `content[0].text`
//!     payload is parsed with serde_json (BTreeMap-backed ⇒ keys sort)
//!     and re-serialized compact. Array ORDER is preserved and
//!     load-bearing everywhere except rule 4.
//!  2. `cutoff_utc` (list_novel_signatures): wall-clock volatile — both
//!     sides are asserted to be non-empty ISO-shaped strings, then
//!     masked to `<VOLATILE:cutoff_utc>`.
//!  3. Invalid-regex `error` (grep_codebase): both sides are asserted
//!     to start with `invalid regex: `, then the library-specific
//!     detail is masked (`re.error` text != `regex::Error` text).
//!  4. `matches` arrays (find_runbook_for_code, grep_codebase): sorted
//!     by canonical JSON — filesystem traversal order is not part of
//!     the parity contract.
//!  5. pathlib subpath ValueError SUFFIX (grep_codebase outside-root
//!     -32000 errors): CPython 3.12 (gh-84538) dropped the trailing
//!     " OR one path is relative and the other is absolute." from
//!     `PurePath.relative_to`'s message; the Rust port mirrors the
//!     3.11-era long form. The stable prefix
//!     `'<path>' is not in the subpath of '<root>'` stays load-bearing;
//!     the version-variant suffix is stripped from BOTH sides.
//!
//! Sessions (env fixed at child spawn):
//!  A: fixture logs + PATH shims (bash/docker/aws) + local HTTP mock;
//!     no AWS creds, no portal, no bearer.
//!  B: bearer token + AWS creds + an INVALID AWS region (deterministic
//!     validation errors, zero network).
//!  C: portal http:// URL + token (deterministic https-refusal).
//!
//! ENV-UNPROVEN (stated honestly, not papered over): the LIVE SigV4
//! CloudWatch network call and the LIVE portal HTTPS call are not
//! exercised end-to-end here (no AWS/portal in this environment); their
//! request BUILDERS are pinned bit-exact by Python-derived golden
//! vectors in `src/sigv4.rs` unit tests instead.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Child process wrapper
// ---------------------------------------------------------------------------

struct McpChild {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    label: String,
}

impl McpChild {
    fn spawn(label: &str, program: &Path, args: &[String], envs: &[(String, String)]) -> Self {
        let mut cmd = Command::new(program);
        cmd.args(args);
        // Scrub every TICKVAULT_* / AWS_* / proxy var so the session env
        // is exactly what the harness sets.
        for (key, _) in std::env::vars() {
            let upper = key.to_ascii_uppercase();
            if upper.starts_with("TICKVAULT_")
                || upper.starts_with("AWS_")
                || upper == "HTTP_PROXY"
                || upper == "HTTPS_PROXY"
                || upper == "ALL_PROXY"
                || upper == "NO_PROXY"
            {
                cmd.env_remove(&key);
            }
        }
        cmd.env("NO_PROXY", "*");
        cmd.env("no_proxy", "*");
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {label} ({}): {e}", program.display()));
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            label: label.to_string(),
        }
    }

    fn send_raw(&mut self, line: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            writeln!(stdin, "{line}").unwrap_or_else(|e| panic!("{}: write: {e}", self.label));
            stdin
                .flush()
                .unwrap_or_else(|e| panic!("{}: flush: {e}", self.label));
        }
    }

    fn read_response(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("{}: read: {e}", self.label));
        assert!(n > 0, "{}: child closed stdout unexpectedly", self.label);
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("{}: bad response JSON ({e}): {line}", self.label))
    }

    fn request(&mut self, req: &Value) -> Value {
        self.send_raw(&req.to_string());
        self.read_response()
    }

    fn notify(&mut self, req: &Value) {
        self.send_raw(&req.to_string());
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        // Close stdin → EOF → loop exit; then reap.
        self.stdin.take();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    // crates/tickvault-logs-mcp -> crates -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, content).expect("write fixture");
}

/// Build the fixture tree: machine + legacy errors.jsonl rotations (the
/// grace-window merge is exercised: one legacy-only rotation + one
/// duplicate name whose MACHINE copy must win), summary, triage log,
/// app log.
fn build_fixtures(fixture_root: &Path) {
    let logs = fixture_root.join("data").join("logs");
    let machine = logs.join("machine");

    // Machine rotations (newest name sorts last → scanned first).
    write(
        &machine.join("errors.jsonl.2026-07-18-05"),
        concat!(
            r#"{"timestamp":"2026-07-18T05:10:00.123456Z","level":"ERROR","code":"DH-904","target":"tickvault_core::oms","message":"rate limited","severity":"high"}"#,
            "\n",
            "this line is not json\n",
            r#"{"timestamp":"2026-07-18T05:11:30Z","level":"ERROR","target":"tickvault_core::websocket","message":"socket closed"}"#,
            "\n",
            "\n",
            r#"{"timestamp":"2026-07-18T05:12:00.000001Z","level":"ERROR","code":"WS-GAP-05","target":"tickvault_core::websocket::connection_pool","message":"pool slot respawned"}"#,
            "\n",
        ),
    );
    write(
        &machine.join("errors.jsonl.2026-07-18-06"),
        concat!(
            r#"{"timestamp":"2026-07-18T06:01:00Z","level":"ERROR","code":"DH-904","target":"tickvault_core::oms","message":"rate limited"}"#,
            "\n",
            r#"{"timestamp":"2026-07-18T06:02:00Z","level":"ERROR","code":"BOOT-01","target":"tickvault_app::infra","message":"questdb readiness deadline approaching","severity":"high"}"#,
            "\n",
        ),
    );
    // Legacy-only rotation (must appear via the grace merge).
    write(
        &logs.join("errors.jsonl.2026-07-18-04"),
        concat!(
            r#"{"timestamp":"2026-07-18T04:59:00Z","level":"ERROR","code":"I-P1-11","target":"tickvault_common::instrument_registry","message":"cross segment collision"}"#,
            "\n",
        ),
    );
    // Duplicate rotation name at the legacy level: the MACHINE copy must
    // win (this decoy row must NOT appear in any output).
    write(
        &logs.join("errors.jsonl.2026-07-18-05"),
        concat!(
            r#"{"timestamp":"2026-07-18T05:00:00Z","level":"ERROR","code":"DECOY-00","target":"legacy","message":"must not appear"}"#,
            "\n",
        ),
    );
    write(
        &machine.join("errors.summary.md"),
        "# Error summary\n\n| sig | count |\n|---|---|\n| 500cf7253d9e7649 | 2 |\n",
    );
    write(
        &logs.join("auto-fix.log"),
        "2026-07-18 05:00:01 triage: rule clear-spill matched\n2026-07-18 05:00:02 triage: executed ok\n2026-07-18 05:05:00 triage: no novel signatures\n2026-07-18 05:10:00 triage: rule dh-904 matched\n2026-07-18 05:10:01 triage: escalated to operator\n",
    );
    // Absolute-outside-root grep fixture (review r3 parity step): exactly
    // ONE file with ONE matching line, no subdirs — the FIRST-match
    // ValueError is deterministic on both sides regardless of walk order.
    write(
        &fixture_root.join("outside-grep").join("needle.txt"),
        "TV_PARITY_OUTSIDE_NEEDLE one line\n",
    );
    // Review r8 ensure_ascii fixture: a runbook whose preview carries an
    // em-dash, é, CJK and an ASTRAL char, scanned by session E (which
    // roots BOTH servers at the fixture tree). Exactly ONE file matches
    // the code, so the matches order is moot and the RAW inner text is
    // byte-comparable.
    write(
        &fixture_root
            .join("docs")
            .join("runbooks")
            .join("r8-ensure-ascii.md"),
        "# R8 ensure_ascii parity fixture\n\ncontext before \u{2014} \u{e9}\nR8-ASCII-01 \u{2014} em-dash \u{e9} \u{65e5}\u{672c}\u{8a9e} \u{1d54a}\ncontext after\n",
    );
    write(
        &logs.join("app.2026-07-18.log"),
        "boot step 1 config ok\nboot step 2 observability ok\nboot step 3 logging ok\nboot step 4 notification ok\nboot step 5 auth ok\nboot step 6 questdb ddl ok\nboot step 7 universe locked\nboot step 8 api listening\n",
    );
}

/// PATH shims for `bash` (doctor), `docker`, `aws` — runtime-generated,
/// never committed (the task's no-new-committed-python/shim rule).
fn build_shims(shim_dir: &Path) {
    std::fs::create_dir_all(shim_dir).expect("mkdir shims");
    let cases: &[(&str, &str)] = &[
        (
            "bash",
            "#!/bin/sh\n# parity shim — canned doctor output\nprintf '%s\\n' '[PASS] questdb reachable (parity fixture)'\nprintf '%s\\n' '[PASS] api healthy'\nprintf '%s\\n' '[FAIL] disk usage 91%'\nprintf '%s\\n' '[WARN] token headroom 3h'\nprintf '%s\\n' '[SKIP] aws detached'\nprintf '%s\\n' 'summary: 2 pass 1 fail'\nexit 0\n",
        ),
        (
            "docker",
            "#!/bin/sh\n# parity shim — canned compose ps json lines\nprintf '%s\\n' '{\"Name\":\"tv-questdb\",\"Service\":\"questdb\",\"State\":\"running\",\"Health\":\"healthy\"}'\nprintf '%s\\n' '{\"Name\":\"tv-app\",\"Service\":\"app\",\"State\":\"exited\",\"Health\":\"\"}'\nprintf '%s\\n' 'not-json-line'\nexit 0\n",
        ),
        (
            "aws",
            "#!/bin/sh\n# parity shim — canned filter-log-events json\nprintf '%s' '{\"events\":[{\"timestamp\":1752800100000,\"message\":\"boot ok\",\"logStreamName\":\"s1\"},{\"timestamp\":1752800000000,\"message\":\"earlier line\",\"logStreamName\":\"s1\"}]}'\nexit 0\n",
        ),
    ];
    for (name, body) in cases {
        let path = shim_dir.join(name);
        std::fs::write(&path, body).expect("write shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod shim");
        }
    }
}

// ---------------------------------------------------------------------------
// Local HTTP mock (questdb /exec + tickvault api routes)
// ---------------------------------------------------------------------------

fn spawn_mock_http() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let port = listener.local_addr().expect("addr").port();
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

// ---------------------------------------------------------------------------
// Normalization + comparison
// ---------------------------------------------------------------------------

fn canon(v: &Value) -> String {
    // serde_json without preserve_order: objects are BTreeMap-backed, so
    // to_string is key-sorted + compact — the rule-1 normalization.
    v.to_string()
}

fn sort_matches_array(inner: &mut Value) {
    if let Some(arr) = inner.get_mut("matches").and_then(Value::as_array_mut) {
        arr.sort_by_key(canon);
    }
}

/// Apply the documented per-tool masks to the PARSED content payload.
fn mask_inner(tool: &str, inner: &mut Value) {
    match tool {
        "list_novel_signatures" => {
            let cutoff = inner
                .get("cutoff_utc")
                .and_then(Value::as_str)
                .unwrap_or("");
            // ISO-shaped: YYYY-MM-DDTHH:MM:SS…+00:00 (huge lookback
            // windows land in past centuries — only the SHAPE is pinned).
            assert!(
                cutoff.len() >= 20
                    && cutoff.as_bytes().get(4) == Some(&b'-')
                    && cutoff.contains('T')
                    && cutoff.ends_with("+00:00"),
                "cutoff_utc not ISO-shaped: {cutoff:?}"
            );
            inner["cutoff_utc"] = Value::String("<VOLATILE:cutoff_utc>".to_string());
        }
        "grep_codebase" => {
            if let Some(err) = inner.get("error").and_then(Value::as_str) {
                assert!(
                    err.starts_with("invalid regex: "),
                    "grep error must carry the invalid-regex prefix: {err:?}"
                );
                inner["error"] = Value::String("invalid regex: <MASKED>".to_string());
            }
            sort_matches_array(inner);
        }
        "find_runbook_for_code" => sort_matches_array(inner),
        _ => {}
    }
}

/// Normalize a full JSON-RPC response for comparison. For tools/call
/// successes the `content[0].text` payload is parsed, masked, and
/// replaced by its canonical form.
fn normalize(tool: Option<&str>, resp: &Value) -> String {
    let mut resp = resp.clone();
    if let Some(text) = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        let mut inner: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("content text not JSON ({e}): {text}"));
        if let Some(tool) = tool {
            mask_inner(tool, &mut inner);
        }
        if let Some(slot) = resp.pointer_mut("/result/content/0/text") {
            *slot = Value::String(canon(&inner));
        }
    }
    // Rule 5: CPython-version-variant pathlib suffix (see module doc).
    if let Some(msg) = resp.pointer("/error/message").and_then(Value::as_str) {
        const SUBPATH: &str = " is not in the subpath of ";
        const PY311_SUFFIX: &str = " OR one path is relative and the other is absolute.";
        if msg.contains(SUBPATH) && msg.ends_with(PY311_SUFFIX) {
            let stripped = msg[..msg.len() - PY311_SUFFIX.len()].to_string();
            if let Some(slot) = resp.pointer_mut("/error/message") {
                *slot = Value::String(stripped);
            }
        }
    }
    canon(&resp)
}

// ---------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------

struct Row {
    session: &'static str,
    tool: String,
    case: &'static str,
    ok: bool,
    diff: Option<(String, String)>,
}

struct Session {
    py: McpChild,
    rs: McpChild,
    next_id: i64,
    session: &'static str,
    rows: Vec<Row>,
}

impl Session {
    fn step(&mut self, tool_label: &str, case: &'static str, req: Value, tool: Option<&str>) {
        let py_resp = self.py.request(&req);
        let rs_resp = self.rs.request(&req);
        let py_norm = normalize(tool, &py_resp);
        let rs_norm = normalize(tool, &rs_resp);
        let ok = py_norm == rs_norm;
        self.rows.push(Row {
            session: self.session,
            tool: tool_label.to_string(),
            case,
            ok,
            diff: if ok { None } else { Some((py_norm, rs_norm)) },
        });
    }

    fn call(&mut self, tool: &str, case: &'static str, args: Value) {
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        });
        self.step(tool, case, req, Some(tool));
    }

    fn rpc(&mut self, label: &str, case: &'static str, method: &str) {
        self.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": self.next_id, "method": method});
        self.step(label, case, req, None);
    }

    /// Review r8: like `call`, but compares the inner `content[0].text`
    /// payloads RAW — no inner canonicalization — so the ensure_ascii
    /// escaping path is byte-pinned, not absorbed. Only valid for steps
    /// whose Python insertion key order coincides with serde's sorted
    /// order (find_runbook_for_code: code/match_count/matches and
    /// file/first_line/preview are both coincidentally alphabetical)
    /// with at most ONE match (traversal order moot).
    fn call_raw_text(&mut self, tool: &str, case: &'static str, args: Value) {
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        });
        let py_resp = self.py.request(&req);
        let rs_resp = self.rs.request(&req);
        let extract = |resp: &Value, label: &str| -> String {
            resp.pointer("/result/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{label}: no content text: {resp}"))
                .to_string()
        };
        let py_text = extract(&py_resp, "python");
        let rs_text = extract(&rs_resp, "rust");
        // Non-vacuity: Python really escaped the fixture's non-ASCII —
        // the em-dash rides as the 6-char backslash-u2014 literal, the
        // astral char as a lowercase surrogate PAIR, and the payload is
        // pure ASCII (so a raw-UTF-8 regression can never pass).
        assert!(
            py_text.contains("\\u2014") && py_text.contains("\\ud835\\udd4a"),
            "python inner text lost its ensure_ascii escapes: {py_text}"
        );
        assert!(
            py_text.bytes().all(|b| b < 0x7f),
            "python inner text not pure ASCII: {py_text}"
        );
        let ok = py_text == rs_text;
        self.rows.push(Row {
            session: self.session,
            tool: format!("{tool} (raw bytes)"),
            case,
            ok,
            diff: if ok { None } else { Some((py_text, rs_text)) },
        });
    }
}

fn python3() -> PathBuf {
    // Resolve once via the harness's own PATH (children may carry shims).
    let out = Command::new("sh")
        .args(["-c", "command -v python3"])
        .output()
        .expect("resolve python3");
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        !p.is_empty(),
        "python3 not found — parity harness requires it"
    );
    PathBuf::from(p)
}

fn spawn_session(
    session: &'static str,
    envs_common: &[(String, String)],
    extra: &[(&str, &str)],
) -> Session {
    let root = repo_root();
    let server_py = root
        .join("scripts")
        .join("mcp-servers")
        .join("tickvault-logs")
        .join("server.py");
    assert!(
        server_py.is_file(),
        "server.py missing: {}",
        server_py.display()
    );

    let mut envs: Vec<(String, String)> = envs_common.to_vec();
    for (k, v) in extra {
        envs.push(((*k).to_string(), (*v).to_string()));
    }
    let py = McpChild::spawn(
        &format!("python[{session}]"),
        &python3(),
        &[server_py.to_string_lossy().into_owned()],
        &envs,
    );
    // Rust child additionally pins the repo root (Python derives it from
    // server.py's file location).
    let mut rs_envs = envs.clone();
    // Python resolves its root via Path(__file__).resolve(); pin the Rust
    // child to the SAME resolved form so absolute-root echoes (the grep
    // outside-root -32000 ValueError text) compare byte-for-byte even
    // when the checkout path traverses a symlink.
    let root_resolved = root.canonicalize().unwrap_or_else(|_| root.clone());
    rs_envs.push((
        "TICKVAULT_MCP_REPO_ROOT".to_string(),
        root_resolved.to_string_lossy().into_owned(),
    ));
    let rs = McpChild::spawn(
        &format!("rust[{session}]"),
        Path::new(env!("CARGO_BIN_EXE_tickvault-logs-mcp")),
        &[],
        &rs_envs,
    );
    Session {
        py,
        rs,
        next_id: 0,
        session,
        rows: Vec::new(),
    }
}

#[test]
fn parity_transcript() {
    let fixture_root = std::env::temp_dir().join(format!("tv-mcp-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&fixture_root);
    build_fixtures(&fixture_root);
    let shim_dir = fixture_root.join("shims");
    build_shims(&shim_dir);
    let mock_port = spawn_mock_http();

    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let common: Vec<(String, String)> = vec![
        (
            "PATH".to_string(),
            format!("{}:{inherited_path}", shim_dir.display()),
        ),
        (
            "TICKVAULT_LOGS_DIR".to_string(),
            fixture_root
                .join("data")
                .join("logs")
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "TICKVAULT_QUESTDB_URL".to_string(),
            format!("http://127.0.0.1:{mock_port}"),
        ),
        (
            "TICKVAULT_API_URL".to_string(),
            format!("http://127.0.0.1:{mock_port}"),
        ),
    ];

    let mut rows: Vec<Row> = Vec::new();

    // ------------------------------------------------------------------
    // Session A — fixtures + shims + mock; no creds/portal/bearer.
    // ------------------------------------------------------------------
    {
        let mut s = spawn_session("A", &common, &[]);
        s.rpc("(rpc)", "initialize", "initialize");
        // Notification: no response; the NEXT request's reply proves the
        // notification produced no stray output on either side.
        s.py.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s.rs.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s.rpc("(rpc)", "tools/list after notification", "tools/list");

        s.call("tail_errors", "defaults", json!({}));
        s.call("tail_errors", "limit=2", json!({"limit": 2}));
        s.call(
            "tail_errors",
            "limit=3 code=DH-904",
            json!({"limit": 3, "code": "DH-904"}),
        );
        s.call(
            "list_novel_signatures",
            "since_minutes huge (all novel; cutoff masked)",
            json!({"since_minutes": 99999999}),
        );
        s.call(
            "list_novel_signatures",
            "since window empty (cutoff masked)",
            json!({"since_minutes": 0}),
        );
        // PR #1644 R6 CRITICAL: since_minutes overflow bands must be
        // byte-identical -32000 errors (never a Rust panic/abort).
        s.call(
            "list_novel_signatures",
            "since_minutes=i64::MAX → C-int band -32000",
            json!({"since_minutes": 9_223_372_036_854_775_807_i64}),
        );
        s.call(
            "list_novel_signatures",
            "since_minutes=1e12 → date-range band -32000",
            json!({"since_minutes": 1_000_000_000_000_i64}),
        );
        s.call(
            "list_novel_signatures",
            "since_minutes=1.44e12 → days-magnitude band -32000",
            json!({"since_minutes": 1_440_000_000_000_i64}),
        );
        s.call(
            "list_novel_signatures",
            "since_minutes=i64::MIN → C-int band -32000",
            json!({"since_minutes": -9_223_372_036_854_775_808_i64}),
        );
        // Keeps-serving proof: same session, next request answered on
        // BOTH sides after the overflow errors above.
        s.call(
            "tail_errors",
            "server keeps serving after since_minutes errors",
            json!({"limit": 1}),
        );
        s.call("summary_snapshot", "fixture summary", json!({}));
        s.call("triage_log_tail", "limit=3", json!({"limit": 3}));
        s.call("triage_log_tail", "default limit", json!({}));
        // 500cf7253d9e7649 = FNV of the DH-904 fixture event (golden).
        s.call(
            "signature_history",
            "known signature",
            json!({"signature": "500cf7253d9e7649", "limit": 10}),
        );
        s.call(
            "signature_history",
            "unknown signature",
            json!({"signature": "0000000000000000"}),
        );
        s.call("signature_history", "missing arg → KeyError", json!({}));
        s.call(
            "find_runbook_for_code",
            "WS-GAP-05 (matches sorted)",
            json!({"code": "WS-GAP-05"}),
        );
        s.call(
            "find_runbook_for_code",
            "no match",
            json!({"code": "ZZZ-NOPE-99"}),
        );
        s.call("questdb_sql", "mock 200", json!({"query": "SELECT 1"}));
        s.call(
            "questdb_sql",
            "mock 400 → HTTP Error text",
            json!({"query": "BAD"}),
        );
        s.call(
            "grep_codebase",
            "scoped path + glob (matches sorted)",
            json!({"pattern": "_signature_hash", "path": "scripts/mcp-servers", "file_glob": "*.py"}),
        );
        s.call(
            "grep_codebase",
            "invalid regex (detail masked)",
            json!({"pattern": "((("}),
        );
        // Review r3 LOW-a: an absolute `path` OUTSIDE the repo root whose
        // walk finds a match must yield the IDENTICAL -32000
        // `tool grep_codebase failed: '<file>' is not in the subpath of
        // '<root>' ...` error on both sides (python: pathlib ValueError;
        // rust: the mirrored strip_prefix arm). Deterministic: the fixture
        // dir holds exactly one file with one matching line.
        s.call(
            "grep_codebase",
            "absolute path outside root → -32000 ValueError parity",
            json!({
                "pattern": "TV_PARITY_OUTSIDE_NEEDLE",
                "path": fixture_root.join("outside-grep").to_string_lossy(),
            }),
        );
        // Same outside dir, no match: python never reaches relative_to —
        // both sides answer ok:true with zero matches.
        s.call(
            "grep_codebase",
            "absolute path outside root, no match → ok empty",
            json!({
                "pattern": "TV_PARITY_NO_SUCH_NEEDLE",
                "path": fixture_root.join("outside-grep").to_string_lossy(),
            }),
        );
        // Review r4 LOW-1: a `//`-prefixed absolute path — pathlib
        // PRESERVES the exactly-two-slash POSIX root, so both sides must
        // quote the '//tmp/...' form in the -32000 ValueError text
        // byte-for-byte (rust previously collapsed it to '/tmp/...').
        s.call(
            "grep_codebase",
            "double-slash-root outside path → `//`-quoted ValueError parity",
            json!({
                "pattern": "TV_PARITY_OUTSIDE_NEEDLE",
                "path": format!("/{}", fixture_root.join("outside-grep").display()),
            }),
        );
        s.call("run_doctor", "bash shim", json!({}));
        s.call("git_recent_log", "limit=3 (real git)", json!({"limit": 3}));
        s.call(
            "tickvault_api",
            "mock JSON route",
            json!({"path": "/health"}),
        );
        s.call("tickvault_api", "mock text route", json!({"path": "text"}));
        s.call("docker_status", "docker shim", json!({}));
        s.call(
            "app_log_tail",
            "fixture date limit=5",
            json!({"limit": 5, "date": "2026-07-18"}),
        );
        s.call(
            "app_log_tail",
            "missing date file",
            json!({"date": "1999-01-01"}),
        );
        s.call(
            "cloudwatch_logs",
            "aws CLI shim path",
            json!({"minutes": 60, "limit": 5}),
        );
        // RPC error shapes.
        s.next_id += 1;
        let id = s.next_id;
        s.step(
            "(rpc)",
            "unknown tool → -32601",
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "nope"}}),
            None,
        );
        s.rpc("(rpc)", "unknown method → -32601", "bogus/method");
        // Parse error: send a raw non-JSON line to both.
        s.py.send_raw("{this is not json");
        s.rs.send_raw("{this is not json");
        let py_resp = s.py.read_response();
        let rs_resp = s.rs.read_response();
        let ok = canon(&py_resp) == canon(&rs_resp);
        s.rows.push(Row {
            session: "A",
            tool: "(rpc)".to_string(),
            case: "parse error → -32700 id null",
            ok,
            diff: if ok {
                None
            } else {
                Some((canon(&py_resp), canon(&rs_resp)))
            },
        });
        rows.append(&mut s.rows);
    }

    // ------------------------------------------------------------------
    // Session B — bearer + AWS creds + INVALID region (deterministic
    // validation failures; zero network).
    // ------------------------------------------------------------------
    {
        let mut s = spawn_session(
            "B",
            &common,
            &[
                ("TICKVAULT_API_BEARER_TOKEN", "dummy-parity-token"),
                ("AWS_ACCESS_KEY_ID", "AKIDPARITYEXAMPLE"),
                ("AWS_SECRET_ACCESS_KEY", "parity-secret-never-logged"),
                ("AWS_DEFAULT_REGION", "Bad_Region!"),
            ],
        );
        s.rpc("(rpc)", "initialize", "initialize");
        s.call(
            "tickvault_api",
            "bearer plaintext non-localhost refusal",
            json!({"path": "/health", "base_url": "http://example.invalid:9"}),
        );
        s.call(
            "tickvault_api",
            "bearer + localhost http allowed (mock)",
            json!({"path": "/health"}),
        );
        s.call(
            "cloudwatch_logs",
            "SigV4 path, invalid region refused",
            json!({"minutes": 5, "limit": 5}),
        );
        rows.append(&mut s.rows);
    }

    // ------------------------------------------------------------------
    // Session C — portal http:// refusal (no creds).
    // ------------------------------------------------------------------
    {
        let mut s = spawn_session(
            "C",
            &common,
            &[
                ("TICKVAULT_PORTAL_URL", "http://portal.invalid"),
                ("TICKVAULT_PORTAL_TOKEN", "pt-parity-token"),
            ],
        );
        s.rpc("(rpc)", "initialize", "initialize");
        s.call(
            "cloudwatch_logs",
            "portal https-only refusal",
            json!({"limit": 5}),
        );
        rows.append(&mut s.rows);
    }

    // ------------------------------------------------------------------
    // Session D — config-file logs_dir branch (hostile-review r1 finding
    // 1): NO TICKVAULT_LOGS_DIR env override, so both children resolve
    // the logs dir from `logs_dir_local` in the endpoints config. The
    // value carries a `/./` component — Python's pathlib drops it at
    // construction; the Rust side must dot-normalize identically (the
    // string echoes into `dir`/`path` fields). The configured path is
    // ABSOLUTE (fixture root), so the two children's differing repo-root
    // derivations are inert and the session stays deterministic.
    // ------------------------------------------------------------------
    {
        let cfg_file = fixture_root.join("parity-endpoints.toml");
        std::fs::write(
            &cfg_file,
            format!(
                "active = \"parity\"\n\n[profiles.parity]\nlogs_dir_local = \"{}/./data/logs\"\nlogs_source = \"local\"\n",
                fixture_root.display()
            ),
        )
        .expect("write parity endpoints config");
        let cfg_file_str = cfg_file.to_string_lossy().into_owned();
        let common_no_logs_dir: Vec<(String, String)> = common
            .iter()
            .filter(|(k, _)| k != "TICKVAULT_LOGS_DIR")
            .cloned()
            .collect();
        let mut s = spawn_session(
            "D",
            &common_no_logs_dir,
            &[("TICKVAULT_MCP_ENDPOINTS_CONFIG", cfg_file_str.as_str())],
        );
        s.rpc("(rpc)", "initialize", "initialize");
        s.call("tail_errors", "config-file dotted logs dir", json!({}));
        s.call("summary_snapshot", "config-file dotted logs dir", json!({}));
        rows.append(&mut s.rows);
    }

    // ------------------------------------------------------------------
    // Session E — review r8 ensure_ascii RAW-byte pin. find_runbook must
    // scan a DETERMINISTIC runbook tree (the real repo's rules churn), so
    // the Python child runs a runtime COPY of server.py placed inside
    // the fixture tree — its `__file__`-derived repo root becomes the
    // fixture root (the shim precedent: runtime-generated, never
    // committed) — and the Rust child is pinned to the same root via
    // TICKVAULT_MCP_REPO_ROOT.
    // ------------------------------------------------------------------
    {
        let real_server = repo_root()
            .join("scripts")
            .join("mcp-servers")
            .join("tickvault-logs")
            .join("server.py");
        let server_copy = fixture_root
            .join("scripts")
            .join("mcp-servers")
            .join("tickvault-logs")
            .join("server.py");
        std::fs::create_dir_all(server_copy.parent().unwrap()).expect("mkdir server copy");
        std::fs::copy(&real_server, &server_copy).expect("copy server.py into fixture tree");
        let py = McpChild::spawn(
            "python[E]",
            &python3(),
            &[server_copy.to_string_lossy().into_owned()],
            &common,
        );
        // Match Python's Path(__file__).resolve()-derived root exactly
        // (symlinked temp dirs), like spawn_session does for the repo.
        let fixture_resolved = fixture_root
            .canonicalize()
            .unwrap_or_else(|_| fixture_root.clone());
        let mut rs_envs = common.clone();
        rs_envs.push((
            "TICKVAULT_MCP_REPO_ROOT".to_string(),
            fixture_resolved.to_string_lossy().into_owned(),
        ));
        let rs = McpChild::spawn(
            "rust[E]",
            Path::new(env!("CARGO_BIN_EXE_tickvault-logs-mcp")),
            &[],
            &rs_envs,
        );
        let mut s = Session {
            py,
            rs,
            next_id: 0,
            session: "E",
            rows: Vec::new(),
        };
        s.rpc("(rpc)", "initialize", "initialize");
        s.call_raw_text(
            "find_runbook_for_code",
            "non-ASCII runbook preview → ensure_ascii raw bytes",
            json!({"code": "R8-ASCII-01"}),
        );
        rows.append(&mut s.rows);
    }

    // ------------------------------------------------------------------
    // Report + verdict
    // ------------------------------------------------------------------
    eprintln!("\nPER-STEP PARITY TABLE (byte-identical after documented normalization):");
    eprintln!("| session | tool | case | identical |");
    eprintln!("|---|---|---|---|");
    for row in &rows {
        eprintln!(
            "| {} | {} | {} | {} |",
            row.session,
            row.tool,
            row.case,
            if row.ok { "Y" } else { "N" }
        );
    }
    let mut failed = 0usize;
    for row in &rows {
        if let Some((py, rs)) = &row.diff {
            failed += 1;
            eprintln!(
                "\nMISMATCH [{} / {} / {}]:",
                row.session, row.tool, row.case
            );
            eprintln!("  python: {py}");
            eprintln!("  rust  : {rs}");
        }
    }
    let _ = std::fs::remove_dir_all(&fixture_root);
    assert_eq!(
        failed,
        0,
        "{failed} of {} parity steps mismatched — see diffs above",
        rows.len()
    );
    // Non-vacuity floor: the transcript must actually cover the surface.
    assert!(
        rows.len() >= 30,
        "transcript unexpectedly short: {}",
        rows.len()
    );
}
