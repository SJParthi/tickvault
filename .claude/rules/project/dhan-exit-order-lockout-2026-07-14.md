# 🔷 DHAN Exit-Order Layer Lockout — Error Codes (EXIT-ORDER-01 / EXIT-VERIFY-01) — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/dhan-exit-order-lockout-2026-07-14.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file before touching the exit-order layer (super orders, forever/OCO, slicing, MPP verify-after-place), `[exit_orders]` config, `ExitOrdersConfig`, the OMS `dry_run` hardcode, or the `EXIT-ORDER-01` / `EXIT-VERIFY-01` runbooks.** It holds both runbooks (§3), the full enable-time protocol (§4), the honest scope notes (§6) and the live-probe ledger (§7). Where this summary and the full file differ, the full file wins. The guard `crates/trading/tests/dhan_exit_order_lockout_guard.rs` now pins phrases in the FULL file. `runbook_path()` for `ExitOrder01ExecutionDegraded` / `ExitVerify01Degraded` still points at THIS path.

**Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F > `dhan-rest-only-noise-lock-2026-07-14.md` > `websocket-connection-scope-lock.md` "2026-07-13 Amendment" > this file > defaults. **Scope:** PERMANENT.

**Contract:** the exit-order layer lands code-complete but **DEFAULT-OFF behind four independent locks**; enabling the config flag activates **dry-run paper behavior only**; going LIVE requires the §4 enable-time protocol with a fresh dated operator quote recorded in the full file first.

**The four locks (§1):** (1) config default-off — `ExitOrdersConfig { #[serde(default)] enabled: bool }`, `config/base.toml` carries `enabled = false`; (2) no reachable live path — the dispatcher's `!cfg.enabled` early-return drops every `ExitCommand`; (3) hardcoded dry-run — `dry_run: true` in `new()`, the ONLY setter is `#[cfg(test)] enable_live_mode`, every exit method's `if self.dry_run` branch sits BEFORE any `get_access_token` call with the `// LIVE-EXIT-ARM` sentinel between them; (4) the build-failing ratchet + this rule, `All Green` required. Going live is **≥3 PR-visible diffs**.

**Both codes are LOG-SINK-ONLY:** ZERO new Telegram emit sites, NO `error_code_alerts` entry, NO paging-list edit, NO terraform change.

**Enable-time protocol order (§4):** dated quote FIRST → noise-lock §2 "order execution alerts" row → config flip (still paper) → the live code diff deleting the hardcode + guard rows + rule edit in the same PR → pre-live prerequisites.

**REJECT (§5, verbatim headlines):**
- Flips `[exit_orders] enabled` to `true` in ANY tracked config file, env var, deploy script, or workflow without the §4 protocol.
- Changes the `ExitOrdersConfig.enabled` serde/`Default` default away from `false`.
- Deletes the hardcoded `dry_run: true`, adds a live-mode setter outside `#[cfg(test)]`, or moves a token fetch ahead of the `if self.dry_run` branch in any exit method.
- Removes or weakens any guard test, the `// LIVE-EXIT-ARM` markers, or this rule file without a fresh dated operator quote first.
- Wires a Telegram sink without the noise-lock §2 family row landing FIRST.
- Exercises an ENTRY_LEG cancel after the entry is PART_TRADED/TRADED (bypassing `entry_leg_cancel_allowed`).
- Auto-slices a super order, or calls `/orders/slicing` for an under-freeze quantity.
- Hardcodes a numeric NSE freeze quantity in Rust (operator config only).
- Adds an `error_code_alerts` entry / terraform alarm for `EXIT-ORDER-01` or `EXIT-VERIFY-01` without the noise-lock rule edit.

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote, only then can the PR land."
