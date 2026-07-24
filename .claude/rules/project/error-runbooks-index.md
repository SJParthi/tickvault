# Error-Code Runbooks — Index (moved out of the auto-load path 2026-07-20)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this pointer.
> **Where the runbooks live:** `docs/error-runbooks/` — every per-error-code
> runbook file (`*-error-codes.md`, plus `dual-instance-lock-2026-07-04.md`)
> was moved there VERBATIM in the Phase-2 rules-tree diet (context-size
> incident 2026-07-20; the auto-loaded `.claude/rules/` tree was killing
> sessions via the autocompact breaker). Nothing was deleted or reworded.
>
> - `ErrorCode::runbook_path()` (`crates/common/src/error_code.rs`) points at
>   the new `docs/error-runbooks/<name>.md` paths; the crossref test scans
>   BOTH `.claude/rules/` and `docs/error-runbooks/`.
> - Triaging an error code? Use `code.runbook_path()` (authoritative), then
>   Read the file from `docs/error-runbooks/`. KNOWN LIMIT (flagged
>   follow-up): `mcp__tickvault-logs__find_runbook_for_code` content-searches
>   only `docs/runbooks/` + `.claude/rules/` — its behavior is frozen by the
>   MCP parity pin (`crates/tickvault-logs-mcp/tests/parity.rs`
>   `SERVER_PY_PINNED_COMMIT`); widening it to `docs/error-runbooks/` needs a
>   deliberate parity-pin bump PR.
> - The ONLY runbook file still in the auto-load tree is
>   `cadence-error-codes.md` (live sessions were editing it at move time).
> - Adding a NEW error code: put its runbook in `docs/error-runbooks/`,
>   point `runbook_path()` there, and mention the code string in that file —
>   the cross-ref ratchets enforce both directions.
