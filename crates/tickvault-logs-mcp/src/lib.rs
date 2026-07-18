//! tickvault-logs MCP server — Rust port of
//! `scripts/mcp-servers/tickvault-logs/server.py` (rust-only purge
//! phase 2c, 2026-07-18).
//!
//! Behavior-parity port: same 14 tools, same tool names, same inputSchema
//! JSON, same output shapes, same JSON-RPC 2.0 newline-delimited stdio
//! framing (MCP 2024-11-05 subset), same FNV-1a signature hash (bit-exact),
//! same hand-rolled AWS SigV4 signing chain. The parity harness
//! (`tests/parity.rs`) drives BOTH this binary and the surviving Python
//! server over an identical scripted transcript and diffs the responses
//! after a defined normalization.
//!
//! Design goals mirror the Python original:
//!   - Read-only by default: never writes, never mutates repo state.
//!   - Structured JSON out — one tool per question.
//!   - Cold path, out-of-process: not bound by the hot-path zero-alloc
//!     rules (this process never touches the tick pipeline).
//!
//! Known deliberate deviations from server.py (each documented at the
//! deviation site and in the PR body):
//!   - Repo-root resolution: Python uses `__file__`; this binary resolves
//!     the repo root from `TICKVAULT_MCP_REPO_ROOT` (if set + resolved),
//!     else walks up from the current dir looking for `.mcp.json` /
//!     `config/claude-mcp-endpoints.toml`, else uses the current dir.
//!     The `.mcp.json` launcher runs from the repo root, so the resolved
//!     root is identical in practice.
//!   - Error TEXT for OS/library-level failures (spawn errors, HTTP
//!     transport errors, invalid-regex details) necessarily differs from
//!     CPython's exception strings; the surrounding JSON shape is
//!     identical. The parity harness's masking covers ONLY cutoff_utc,
//!     the grep invalid-regex error detail, and `matches`-array sort
//!     order; transport-error text is NOT masked — the parity transcript
//!     AVOIDS transport-error paths (the mock HTTP server is always up).
//!   - stdout write failures (broken pipe): this binary ignores stdout
//!     write/flush errors and exits 0 at stdin EOF; CPython dies exit 1
//!     with a BrokenPipeError traceback on stderr. Unreachable in the
//!     real MCP lifecycle (the client holds the pipe open until it
//!     closes our stdin); the Rust direction is strictly safer.
//!   - grep_codebase with an absolute `path` outside the repo root:
//!     PARITY-MATCHED since review r3 — the first match returns the
//!     CPython 3.11 `Path.relative_to` ValueError text through the
//!     -32000 `tool grep_codebase failed: ...` wrap (previously ok:true
//!     with lossy absolute paths — a success-vs-error class divergence).
//!     Since review r4 (2026-07-18) the POSIX `//`-root is also
//!     parity-matched: pathlib_lexical preserves an exactly-two-slash
//!     root (pathlib rule) and the rel computation compares pathlib
//!     PARTS, so `path="//<root>/sub"` errors like CPython ('//' and
//!     '/' are different root parts) instead of failing open, and a
//!     `//`-prefixed outside path quotes the `//` form byte-for-byte.
//!     Residuals (exact list):
//!       1. Quoting: the replicated text uses plain single quotes;
//!          CPython uses repr(), which would escape a quote/control
//!          character inside a path — unreachable for real repo/fixture
//!          paths.
//!       2. NUL byte in the `path` arg (review r4, 2026-07-18):
//!          adversarial-client-only input. CPython raises -32000
//!          "embedded null byte" at os.walk/open; Rust's read_dir Err is
//!          swallowed by the walk's `else return Ok(())` arm, answering
//!          ok:true with 0 matches. Divergence class: ok-empty-vs-error,
//!          rust in the FAIL-SAFE direction (empty result, no error text,
//!          no data exposure). Deliberately unchanged.
//!   - app_log_tail `date` echoes: PARITY-MATCHED since review r3 — the
//!     joined log path is pathlib-normalized (`.` components dropped;
//!     since review r4 the `//`-root rule matches pathlib too), so a
//!     dotted date like "x/./y" echoes `app.x/y.log` on both sides
//!     byte-for-byte. No residual.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]

pub mod config;
pub mod pycompat;
pub mod rpc;
pub mod selftest;
pub mod signature;
pub mod sigv4;
pub mod tools;
