# Implementation Plan: Rust-only word purge — zero occurrences in our own source

**Status:** APPROVED
**Date:** 2026-08-01
**Approved by:** Parthiban (operator), verbatim this session (typos preserved):
> "see can you assuure mt ehe ntitre worspcae is usign one and onl yfull y depeply
> throroiughly comrpehsively each and every nook and corner even inch by inch onely
> it used only RUST O(1) i ened the gauarntee and asruance nowhere i shdpou le even
> see the pyhton word itsefl okay?"
>
> "see no python bridge pyhto. word python code ntohign shodu lbe fuckign bro okay?"
>
> "i just need only RUST O(1) can you change it entilrey ?"

Authority: `.claude/rules/project/rust-only-forever-lock-2026-07-19.md` §0 Quote 2
(2026-07-31) established the zero-tracked-file floor. This plan closes the gap that
audit left: the WORD, and two sites that still EXECUTE the banned runtime.

## Design

The 2026-07-31 purge reached zero tracked `.py` files. The 2026-08-01 audit (PR #1716)
then found eleven live package-manager invocations the guard's token set missed. This
plan closes the remaining surface in three classes, each with a different technique
because they are not the same problem:

1. **Prose (comments / doc-comments) — ~1,100 occurrences.** Reword. The provenance
   (`handler.py:NNN` pointers, quoted symbol names, dates) MUST survive verbatim; only
   the word changes. Convention: `legacy` for code we retired, `vendor` for a
   third-party SDK, `interpreter` where the runtime itself is the subject.

2. **Wire values — 6 occurrences.** These cross a network boundary and CANNOT be
   reworded without changing behavior:
   - `oms/groww/push/connect.rs` `CONNECT_LANG` — the NATS CONNECT `lang` field.
   - `oms/groww/push/socket_token.rs` `HEADER_CLIENT_PLATFORM` — `x-client-platform`.
   - `tickvault-logs-mcp/src/tools.rs` ×4 — a returned tool error, the `require_str`
     type error, and an advertised MCP `inputSchema` description.
   Technique: byte-assemble via `core::str::from_utf8` in a const context (the
   precedent already in `rust_only_guard.rs::banned_token`). Bytes on the wire stay
   byte-for-byte IDENTICAL; the literal leaves the source. Each is pinned by a test
   whose EXPECTED value is also spelled as bytes, never as a literal.

3. **Execution sites — 2.** Neither is a rewording problem:
   - `tickvault-logs-mcp/tests/parity.rs` resurrects the deleted implementation from
     pinned git history onto disk and executes it, and hard-fails (not skips) when the
     interpreter is absent. It runs in CI on every PR via `Test (logs-mcp)`.
   - `aws-lambdas/src/operator_control_action_commands.rs` `WIPE_QUESTDB_COMMANDS[6]`
     is a 17-line embedded program dispatched via SSM RunCommand to the PROD box, where
     it truncates QuestDB tables.

Finally, a ratchet so the word cannot return: extend `rust_only_guard.rs` to scan our
own source (`crates/`, `scripts/`, `.github/`, `deploy/`, root manifests) for both the
word and for interpreter-spawn shapes in Rust, with the enforcement files themselves
byte-assembling the token they ban.

## Edge Cases

- **Enforcement files must name the token to ban it.** `rust_only_guard.rs` already
  byte-assembles; `tickvault_logs_mcp_guard.rs` did not and now does.
- **`.py` filename pointers are NOT the word** — `handler.py`, `server.py` stay.
  `PyPI`, `PYWIPE`, `pycompat` likewise do not contain it.
- **Guard tests that scan another crate's source** must move in lockstep with that
  crate's renames or they fail against text that no longer exists.
- **A doc filename contained the word** (`docs/dhan-support/…-python-also-fails.md`).
  Renamed; every reference updated. Cost stated under Rollback.
- **Test-name renames** are only safe with zero external references — each was
  `git grep`-verified before renaming.
- **Sed over a whole file also rewrites data lines**, not just comments. Every batch
  was preceded by a guard asserting all matches were comment lines, and followed by a
  full diff review.

## Failure Modes

| Failure | Detection | Consequence if missed |
|---|---|---|
| A wire value silently changes | byte-pin tests (expected value spelled as bytes) | Groww rejects the NATS CONNECT or the socket-token mint → order-push channel dead |
| A reworded comment loses provenance | `handler.py` reference count compared before/after | audit trail for the Lambda port degrades |
| A sed touches a data line | pre-flight comment-only assertion + full diff review | arbitrary behavior change |
| A guard test drifts from the source it scans | that crate's test run | enforcement passes vacuously |
| The word returns later | the new ratchet | the guarantee silently decays again |

## Test Plan

- `cargo test` per touched crate: common, core, storage, app, api, trading
  (+`groww_orders`), aws-lambdas, logs-mcp.
- New byte-pin tests: `test_connect_lang_wire_bytes_are_unchanged`,
  `test_client_platform_header_wire_bytes_are_unchanged`,
  `tools::tests::runtime_name_wire_bytes_are_unchanged`.
- Bite-test the new ratchet: re-introduce the word into a source file and confirm the
  guard FAILS; remove it and confirm it passes.
- `cargo fmt --all --check`, `cargo clippy --workspace`.
- `bash -n` on every touched shell script; YAML parse on every touched workflow;
  `cargo metadata` for the root manifest.

## Rollback

Every change is textual and independently revertible; no schema, no config, no
migration. `git revert` restores the previous state exactly. The two wire-value
conversions are the only behavior-adjacent edits and are each guarded by a byte pin,
so a bad revert is caught by the same test.

One irreversible-ish cost, stated plainly: renaming
`docs/dhan-support/2026-04-21-ticket-5519522-python-also-fails.md` breaks the GitHub
blob URL if that file was ever shared with Dhan support. The content is preserved
verbatim under the new name; only the URL changes.

## Observability

No new runtime signal — this PR is prose, identifiers and two byte-identical wire
constants. Enforcement is build-time only (the extended `rust_only_guard.rs`). The
retirement of the parity harness REMOVES a test signal and that loss is recorded in
the rule file rather than hidden.

## Per-Item Guarantee Matrix

Canonical definition: `.claude/rules/project/per-wave-guarantee-matrix.md`. Filled in for
this work rather than cross-referenced blindly — several rows are honestly N/A because
this change is prose, identifiers, and two byte-identical wire constants.

| Demand | How this plan satisfies it |
|---|---|
| 100% code coverage | No new production branches; the 3 new byte-pin tests + the new ratchet all execute. Coverage floors unchanged (`quality/crate-coverage-thresholds.toml`). |
| 100% audit coverage | N/A — no new event, no new table. The provenance audit trail is what this plan PRESERVES (the `handler.py:NNN` pointer count is compared before/after). |
| 100% testing coverage | Touched-crate suites in full: trading (+`groww_orders`), aws-lambdas, logs-mcp, common, core, storage, app, api. |
| 100% code checks | `cargo fmt --all --check`, clippy, banned-pattern scan, secret scan, plan-verify, the pre-commit 9-gate battery. |
| 100% code performance | N/A for the hot path — nothing on it is touched. The one allocation introduced (`grep_pattern_description()`) is on the cold MCP `tools/list` handshake, once per client. |
| 100% monitoring | N/A — no new runtime signal (see Observability). |
| 100% logging | Unchanged; no `error!`/`warn!` site added or removed. |
| 100% alerting | N/A — no new failure mode. |
| 100% security | The wipe rewrite is the security-relevant edit: dry-run-verified to leave every SEBI never-delete table untouched. No secret, credential, or token path touched. |
| 100% security hardening | Attack-surface delta: ZERO net. Two interpreter execution paths REMOVED (one in CI, one on the prod box) — strictly a reduction. |
| 100% bugs fixing | The sweep itself surfaced two real defects: the guard's word-only token set (PR #1716) and the harness that resurrected + executed a deleted file. |
| 100% scenarios covering | Scenarios table below — incl. the SEBI-tables-survive case and the reintroduction case. |
| 100% functionalities covering | Every renamed identifier was `git grep`-verified to have zero external references before renaming. |
| 100% code review | Three parallel agents partitioned by directory; each reported refusals rather than guessing, and two escalated the execution sites instead of editing them. |
| 100% extreme check | The new ratchet fails the build on reintroduction, bite-proven in both directions. |

## Resilience Demand Matrix

| Demand | This plan's position |
|---|---|
| Zero ticks lost | N/A — no capture, ring, spill, or DLQ path is touched. No new tick-drop path introduced. |
| WS never disconnects | The two Groww wire constants are the ONLY connection-adjacent edits; both are byte-pinned so the CONNECT frame and mint header are provably unchanged. |
| Never slow/locked/hanged | No hot-path code touched; no new allocation on any per-tick path. |
| QuestDB never fails | The wipe rewrite talks to QuestDB; it is bounded (`--max-time 15`/`30`) and its independent WIPE-RESULT tail still proves the counts reached zero. |
| O(1) latency | Unchanged. This plan makes NO O(1) claim — see the honest non-claim below. |
| Uniqueness + dedup | N/A — no DEDUP key, schema, or identity path touched. |
| Real-time proof | N/A — build-time enforcement only. |

**Honest non-claim (mandatory wording).** This plan does NOT claim the workspace is
O(1); that is false and cannot be made true (reading N rows is O(N) by counting, and
comparison sorting is provably ≥ O(n log n)). What is claimed is bounded and ratcheted:
100% inside the tested envelope, with ratcheted regression coverage — our own source is
at literal zero occurrences, enforced by a bite-proven build-failing guard; the six wire
values are byte-identical, each pinned by a test whose expected value is spelled as
bytes. Beyond that envelope, the vendor reference docs and dated plan history keep the
word deliberately, and that is stated rather than counted as done.

## Plan Items

- [x] Prose purge — `crates/aws-lambdas/` (601 → 1)
  - Files: 21 under `crates/aws-lambdas/`
  - Tests: `cargo test -p tickvault-aws-lambdas` (461 passed)
- [x] Prose purge — `crates/tickvault-logs-mcp/` (249 → 30, remainder is parity.rs)
  - Files: 10 under `crates/tickvault-logs-mcp/src/` + its Cargo.toml
  - Tests: `cargo test -p tickvault-logs-mcp` (79 lib + 1 parity passed)
- [x] Prose purge — common / core / storage / app / api
  - Files: `moneyness.rs`, `constants.rs`, `types.rs`, `segment.rs`, `sanitize.rs`,
    `error_code.rs`, `ws_event_types.rs`, `tls.rs`, `instruments.rs`, guard tests
  - Tests: `cargo test -p tickvault-common`, `-p tickvault-core`
- [x] Prose purge — scripts / workflows / terraform / manifests / .gitignore
  - Files: 24
  - Tests: `bash -n`, YAML parse, `cargo metadata`
- [x] Wire values byte-assembled + pinned (Groww CONNECT lang, x-client-platform)
  - Files: `oms/groww/push/connect.rs`, `oms/groww/push/socket_token.rs`
  - Tests: `test_connect_lang_wire_bytes_are_unchanged`,
    `test_client_platform_header_wire_bytes_are_unchanged`
- [x] Wire values byte-assembled + pinned (MCP tool errors + inputSchema description)
  - Files: `crates/tickvault-logs-mcp/src/tools.rs`
  - Tests: `tools::tests::runtime_name_wire_bytes_are_unchanged`
- [x] Enforcement literals byte-assembled in the MCP guard
  - Files: `crates/common/tests/tickvault_logs_mcp_guard.rs`
  - Tests: `cargo test -p tickvault-common --test tickvault_logs_mcp_guard`
- [x] Doc filename renamed + all references updated
  - Files: `docs/dhan-support/2026-04-21-ticket-5519522-vendor-sdk-also-fails.md`,
    `crates/common/src/moneyness.rs`
  - Tests: `git grep` for the old name returns zero
- [x] Retire the interpreter-spawning parity harness + its lockstep guard
  - Files: `crates/tickvault-logs-mcp/tests/parity.rs` (delete),
    `crates/common/tests/tickvault_logs_mcp_guard.rs`,
    `.claude/rules/project/rust-only-forever-lock-2026-07-19.md`
  - Tests: `cargo test -p tickvault-logs-mcp`, `-p tickvault-common`
- [x] Replace the embedded QuestDB-wipe program with curl + shell
  - Files: `crates/aws-lambdas/src/operator_control_action_commands.rs`,
    `crates/aws-lambdas/src/operator_control.rs`
  - Tests: `cargo test -p tickvault-aws-lambdas`
- [x] Extend the ratchet so the word cannot return to our own source
  - Files: `crates/common/tests/rust_only_guard.rs`
  - Tests: bite-test (add the word → guard fails; remove → passes)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Groww order-push channel connects after the CONNECT_LANG change | byte-identical frame; server accepts exactly as before |
| 2 | Socket-token mint runs after the header change | byte-identical `x-client-platform`; mint succeeds |
| 3 | An MCP client calls `tools/list` | the advertised schema description is byte-identical |
| 4 | Someone reintroduces the word into a source file | the ratchet fails the build |
| 5 | Someone re-adds an interpreter spawn in Rust | the ratchet fails the build |
| 6 | The operator runs `wipe-questdb` after the rewrite | same tables truncated; `WIPE-COMPLETE` printed |
