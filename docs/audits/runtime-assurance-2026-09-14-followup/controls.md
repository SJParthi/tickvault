# Phase 3: complete wipe verification

Owned files:

- `crates/aws-lambdas/src/operator_control_action_commands.rs`
- `crates/aws-lambdas/src/operator_control.rs`

The previous verifier checked `candles_1m` as a representative of all `candles_*` tables. A nonempty `candles_60m`, for example, could therefore survive while the action printed `WIPE-COMPLETE`. The catalog parser also stripped quotes and interpolated loosely accepted `candles_*` names into SQL; shell command substitution could discard NUL bytes before count validation.

## Implemented behavior

| Boundary | Behavior in the candidate |
|---|---|
| Initial discovery | A shared read-only preparation command validates the complete `/exp` catalog before stopping the app, deleting replay data, or issuing a truncate. Malformed/unavailable catalogs or missing required market-data tables exit 1 before mutations. |
| Target policy | Keeps the original seven exact market-data table names plus the `candles_*` family. Every catalog identifier must be quoted CSV containing an ASCII identifier; duplicate names, extra fields/rows, malformed names, and injected SQL/shell text fail closed. The count helper independently checks its identifier allowlist before constructing SQL. |
| Original manifest | The validated original list drives truncation and remains available after restart. A table disappearing from the current catalog still receives its original count query. |
| Current manifest | Verification discovers the current target set again and checks the union of original and current targets. New candle tables cannot escape through the old one-minute representative check. |
| Per-target counts | Every target must return the exact quoted `count` header and one canonical unsigned integer equal to `0`. Unknown is printed as `?`, marks the operation partial, and exits 1. |
| Transport integrity | Raw curl output is piped into the CSV parser with a final transport-success record. NUL bytes and trailing blank records are no longer discarded by shell string capture before validation. Extra/spoofed records and failed transport after a valid-looking body fail. |
| Truncation failures | A recorded HTTP/transport failure for any truncate prevents completion even if all later count responses say zero. |
| Operator output | Preserves `WIPE-TARGETS`, `TRUNCATED`, `TRUNCATE-FAILED`, `WIPE-RESULT`, `WIPE-COMPLETE`, and `WIPE-PARTIAL` markers. `WIPE-RESULT` now includes every verified target. |

## Actually executed

The exact production **read-only** preparation and final verification strings were extracted from the Rust source and executed in Bash with the exact source-defined fake `curl` and `sleep`. The destructive middle commands were excluded; an explicit source check rejected any service, deletion, Docker, or truncate text in the extracted fragments. No HTTP, real SQL, service action, file deletion, live wipe, or device access ran.

| Local fixture group | Executed | Passed |
|---|---:|---:|
| Count response validation | 34 | 34 |
| Every target checked independently | 97 | 97 |
| Initial and current catalog validation | 54 | 54 |
| Original/current/required manifest scenarios | 14 | 14 |
| **Total** | **199** | **199** |

The 97-case group comprises one exact all-zero manifest check plus three independently injected failures for each of 32 targets: all 24 current candle names, seven other market-data tables, and one future candle name. The failures are positive counts, malformed/error replies, and a transport failure after returning a valid zero body. Non-target audit tables remain excluded.

Catalog cases include incorrect/unquoted headers, missing bodies, duplicate names, extra columns, blank rows, embedded newlines, SQL/shell-looking text, Unicode and punctuation in identifiers, forged success records, errors, transport failure after a complete catalog, and NUL bytes. Valid LF, CRLF, and missing final newline remain accepted. Manifest cases include every required table missing before mutation, a disappeared original candle, a newly introduced candle, a preserved unrelated audit table, and known truncate failure with otherwise zero counts.

Across the 199 fixtures, eight valid scenarios completed and 191 rejected scenarios returned `WIPE-PARTIAL` with exit 1. `bash -n` and `git diff --check` passed for the modified material. The prior 27-case phase-2 wipe fixture group is superseded by these 199, not additive evidence for the new code.

Reproduction tooling and results:

- `/tmp/p3-wipe-check.py`
- `/workspace/scratch/81296451ef3b/tickvault-audit/docs/audits/runtime-assurance-2026-09-14-followup/wipe-results.json`

Rust selectors added/updated (**not executed here**):

```text
cargo test -p tickvault-aws-lambdas --lib test_wipe_verifier_requires_explicit_zero_counts
cargo test -p tickvault-aws-lambdas --lib test_wipe_targets_and_verification_name_the_same_tables
cargo test -p tickvault-aws-lambdas --lib test_wipe_catalog_rejects_malformed_names_and_transport_failures
cargo test -p tickvault-aws-lambdas --lib test_wipe_original_current_and_required_targets_cannot_escape_verification
cargo test -p tickvault-aws-lambdas --lib test_wipe_questdb_truncates_live_rest_tables_too
```

## Limits

- No Rust compiler, Rust test runner, or rustfmt execution occurred in this subtask. The root coordinator must validate the candidate in the authorized isolated AWS checkout.
- The code still dispatches shell for this existing operator action. It does not satisfy an all-Rust workspace claim.
- The target scan and query loop scale with the number of tables; this maintenance action is not O(1).
- The application still restarts before final verification. Counts are individual observations, not a transactional cross-table snapshot or a promise that future writers cannot add rows. Simulated zero replies do not establish live QuestDB parity, durability, or operator-action success.
- A valid but unfamiliar non-ASCII or punctuation-containing table name makes the whole catalog fail closed. This intentionally requires review before any destructive operation can use that catalog.
- Per-request curl deadlines remain 15 seconds for catalogs and 5 seconds for counts; total verification time increases with target count. No hard whole-operation wall-clock guarantee is claimed.
