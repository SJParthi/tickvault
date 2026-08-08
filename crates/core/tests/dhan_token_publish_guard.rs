//! Ratchet: the Dhan access token stays PUBLISHED, and the publish stays
//! fail-soft and out of the read-only secret manager.
//!
//! # Why these pins exist
//!
//! Dhan permits exactly ONE active access token per account
//! (`docs/dhan-ref/02-authentication.md:216`). tickvault is the sole Dhan
//! minter; if it stops publishing the token, any peer consumer of the same
//! account must mint its own — which invalidates tickvault's token and starts
//! a flapping re-mint war. Operator directive 2026-08-08, contract in
//! `.claude/rules/project/groww-shared-token-minter-2026-07-02.md` §9.
//!
//! These are source-scan meta-guards: they fail the BUILD if a future refactor
//! silently drops the publish, awaits it on the mint path, moves the SSM write
//! into the read-only secret manager, or logs the token value.

use std::fs;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

/// Strips `//` line comments so a pin can never be satisfied by prose alone.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns only the PRODUCTION portion of a module — everything before the
/// `#[cfg(test)]` block.
///
/// Needed because the publisher's own unit tests legitimately reference the
/// Groww constants in order to assert the two token paths never collide. That
/// reference must not trip the production-scoped pin below.
fn production_only(src: &str) -> String {
    let code = strip_line_comments(src);
    match code.find("#[cfg(test)]") {
        Some(i) => code[..i].to_string(),
        None => code,
    }
}

/// The publisher module must exist and perform an SSM `put_parameter`.
#[test]
fn publisher_module_exists_and_writes_to_ssm() {
    let src = read("src/auth/dhan_token_publisher.rs");
    let code = strip_line_comments(&src);

    assert!(
        code.contains("put_parameter"),
        "dhan_token_publisher.rs must call SSM put_parameter — without the \
         publish, a peer consumer must mint its own Dhan token and invalidate \
         ours (one-active-token per account)."
    );
    assert!(
        code.contains("SecureString"),
        "the Dhan token must be written as a SecureString (KMS-encrypted at \
         rest), never a plain String parameter."
    );
    assert!(
        code.contains("overwrite"),
        "the publish must set overwrite(true) — the parameter is rewritten on \
         every mint/renewal."
    );
}

/// BOTH post-mint sites must publish. A token refreshed but not republished
/// leaves peers holding the just-invalidated value.
#[test]
fn both_mint_sites_publish() {
    let src = read("src/auth/token_manager.rs");
    let code = strip_line_comments(&src);

    let publishes = code.matches("self.publish_current_token_to_ssm()").count();
    assert!(
        publishes >= 2,
        "expected the publish at BOTH post-mint sites (initial \
         generateAccessToken + renewal), found {publishes}. A renewal that \
         does not republish leaves peers on the invalidated token."
    );

    // The publish must sit alongside the cache save at every mint site, so the
    // two can never drift apart.
    let caches = code.matches("self.save_current_token_to_cache()").count();
    assert_eq!(
        publishes, caches,
        "every crash-cache save must be paired with a publish ({caches} cache \
         saves vs {publishes} publishes) — a mint that caches locally but does \
         not publish re-opens the token war."
    );
}

/// The publish must be SPAWNED, never awaited on the mint path — a hanging SSM
/// call must never delay a token mint or stall the Step-6 boot deadline.
#[test]
fn publish_is_spawned_not_awaited() {
    let src = read("src/auth/dhan_token_publisher.rs");
    let code = strip_line_comments(&src);

    assert!(
        code.contains("tokio::spawn"),
        "the publish must be spawned so it can never block the mint path."
    );

    let manager = strip_line_comments(&read("src/auth/token_manager.rs"));
    assert!(
        !manager.contains("publish_current_token_to_ssm().await"),
        "the publish must NOT be awaited on the mint path — a slow SSM call \
         would delay the token mint and can stall boot."
    );
}

/// Fail-soft: the publisher must never panic. tickvault already holds the
/// token in memory, so a publish failure must never degrade its own auth.
#[test]
fn publisher_is_fail_soft() {
    // Production code only — a unit test may legitimately unwrap.
    let code = production_only(&read("src/auth/dhan_token_publisher.rs"));

    for banned in ["unwrap()", "expect(", "panic!"] {
        assert!(
            !code.contains(banned),
            "dhan_token_publisher.rs must be fail-soft — found `{banned}`. A \
             publish failure must be logged and counted, never fatal."
        );
    }
    assert!(
        code.contains("tv_dhan_token_publish_total"),
        "publish outcomes must be counted so a silently-failing publish is \
         visible (a silent failure re-opens the token war invisibly)."
    );
}

/// The token VALUE must never reach a log line.
#[test]
fn publisher_never_logs_token_value() {
    // Production code only — the count below is a leak-surface budget.
    let code = production_only(&read("src/auth/dhan_token_publisher.rs"));

    // `expose_secret()` is legitimate exactly once — to build the request body.
    let exposures = code.matches("expose_secret()").count();
    assert!(
        exposures <= 1,
        "expected at most ONE expose_secret() (building the SSM request), \
         found {exposures} — every additional exposure is a credential-leak \
         surface."
    );

    for line in code.lines() {
        let is_log = ["error!", "warn!", "info!", "debug!", "trace!"]
            .iter()
            .any(|m| line.contains(m));
        if is_log {
            assert!(
                !line.contains("expose_secret"),
                "a log line must never expose the token value: {}",
                line.trim()
            );
        }
    }
}

/// `secret_manager.rs` must STAY read-only. This mirrors the pre-existing pin
/// in `groww_no_mint_guard.rs:129` and is restated here so a future refactor
/// that "tidies" the publish back into the secret manager fails in BOTH suites.
#[test]
fn secret_manager_stays_read_only() {
    let code = strip_line_comments(&read("src/auth/secret_manager.rs"));
    assert!(
        !code.to_ascii_lowercase().contains("put_parameter"),
        "secret_manager.rs must remain read-only — the SSM write belongs in \
         dhan_token_publisher.rs (groww_no_mint_guard.rs pins this too)."
    );
}

/// The Dhan publish must never target the Groww token parameter. The secret
/// NAME is identical (`access-token`) — only the service segment differs — so
/// a wrong constant would overwrite the Groww token and 401 the Groww REST legs.
#[test]
fn publish_targets_the_dhan_service_path() {
    // Production code only — the module's own unit tests reference the Groww
    // constants on purpose, to assert the two paths never collide.
    let code = production_only(&read("src/auth/dhan_token_publisher.rs"));
    assert!(
        code.contains("SSM_DHAN_SERVICE") && code.contains("DHAN_ACCESS_TOKEN_SECRET"),
        "the publish path must be built from the DHAN service + DHAN secret \
         constants."
    );
    assert!(
        !code.contains("SSM_GROWW_SERVICE"),
        "the Dhan publisher must never reference the Groww service segment — \
         writing a Dhan token over the Groww parameter would break the Groww \
         REST legs."
    );
}

/// The rule file must carry the contract, so the 'why' survives this code.
#[test]
fn rule_file_pins_the_shared_token_contract() {
    let rule = read("../../.claude/rules/project/groww-shared-token-minter-2026-07-02.md");
    for phrase in [
        "§9",
        "dhan_token_publisher",
        "One active token",
        "/tickvault/prod/dhan/access-token",
    ] {
        assert!(
            rule.contains(phrase),
            "the minter lock must document the Dhan shared-token contract — \
             missing: {phrase}"
        );
    }
}
