# Groww Shared Token-Minter — Consumer-Only Lock (Operator 2026-07-02) — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/groww-shared-token-minter-2026-07-02.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file before touching any broker token mint / publish / read path (Dhan or Groww), `secret_manager.rs`, `dhan_token_publisher.rs`, the `dhan-token-minter` Lambda, its terraform/IAM/alarms, or `/tickvault/<env>/{dhan,groww}/*` SSM params.** It holds the §8 native-Groww-mint authorization (gated on the §8.4 probe), the §9 Dhan shared-token contract, the §10 Dhan minter Lambda contract, the §10.3 coexistence warning, and the §10.8 TOTP-retry rule. Where this summary and the full file differ, the full file wins. Guards `dhan_token_minter_wiring_guard.rs` and `dhan_token_publish_guard.rs` pin phrases in the FULL file.

**Banner (2026-08-21):** the ENTIRE Groww feed is ordered REMOVED (full authorization in `websocket-connection-scope-lock.md` §"2026-08-21 (THIRD quote of the day)"). Every Groww contract here is RETIRED as a live path and retained as historical audit. "**NOT retired: the SEBI never-delete obligation.**" `instrument_lifecycle`, `instrument_lifecycle_audit` and `index_constituency` rows tagged `feed='groww'` are NEVER deleted. Where sections conflict with this banner, the banner wins.

**Original lock (§1–§3, 2026-07-02):** ONE MINTER ONLY for Groww (the bruteX-owned Lambda); tickvault is READ-ONLY on the token, never reads `groww/api-key` / `groww/totp-secret`, never writes any `/tickvault/*/groww/*` param, never adds a second Groww minter.

**§8 (2026-08-08):** native Groww mint is authorized but GATED: "Ships ANY Groww mint code before the §8.4 probe verdict is recorded in this file" = REJECT; `secret_manager.rs` stays read-only (`groww_no_mint_guard.rs`).

**§9 (Dhan shared token):** Dhan is single-token PER ACCOUNT. REJECT: removing the publish; awaiting the publish on the mint path; moving the SSM write into `secret_manager.rs`; logging the token; pointing the publish at the Groww service segment; broadening the IAM grant; "Adds a SECOND Dhan minter anywhere".

**§10 (Dhan minter Lambda — supersedes §9's minter placement):** `tv-<env>-dhan-token-minter` (`crates/aws-lambdas/src/dhan_token_minter.rs`) is **the sole Dhan minter**; `cron(35 0 * * ? *)` = 06:05 IST **EVERY day**; reads enumerated `/tickvault/<env>/dhan/{client-id,client-secret,totp-secret}` ARNs; writes `/tickvault/<env>/dhan/access-token` ONLY; native Rust; NO EC2 permissions; fail-loud; JWT shape check before the write; `not-invoked` alarm with `treat_missing_data = "breaching"`. §10.3: two minters against a one-token account is a re-mint war — tickvault must READ the parameter instead of minting before the box runs alongside the Lambda.

**§10.4 REJECT (verbatim headlines):** a SECOND Dhan minter anywhere; widening write scope beyond `dhan/access-token` or read scope to a `/dhan/*` wildcard; logging the token, PIN or TOTP or rendering a `reqwest::Error` verbatim; publishing without the JWT shape check; any silent failure path; removing the not-invoked alarm or its `breaching` setting; a MON-FRI schedule; deleting §9's publisher without first switching tickvault to READ.

**§10.8 REJECT:** raising `MAX_TOTP_ATTEMPTS` above 2; retrying a non-TOTP rejection; retrying within the SAME TOTP step; lowering the Lambda timeout below the 82 s worst case; logging the code, PIN or token on the retry.

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote."
