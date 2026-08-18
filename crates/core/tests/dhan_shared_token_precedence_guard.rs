//! Guard: the shared-token ladder must stay in the order that ends the
//! re-mint war, and a broker-rejected token must never be replaced by a
//! staler cached one.
//!
//! # Why this exists
//!
//! Dhan permits **one active token per account** — minting a new one
//! invalidates the old (`docs/dhan-ref/02-authentication.md:216`). On
//! 2026-08-18 the prod box overwrote the scheduled Lambda's 06:05 IST token at
//! 08:11 IST (SSM `LastModifiedUser` = the instance role), killing it. The
//! repair is an ordering contract, and orderings are exactly what silently rot:
//!
//! 1. **Read SSM first.** The shared token the scheduled minter published is
//!    the account's active one. Adopt it rather than minting a rival.
//! 2. **Verify before adopting.** A token that parses and has not expired can
//!    still have been invalidated by someone else's mint, so adoption is gated
//!    on a live broker probe.
//! 3. **Only then fall back.** Cache, then mint.
//!
//! # The defect this pins
//!
//! When the broker REJECTED the SSM token, the code stored `None`, logged
//! *"Falling through to mint"* — and then fell through to the **local cache**,
//! which is strictly staler than the token Dhan had just refused (every mint
//! publishes to SSM, so SSM is a superset of the cache). The session then ran
//! on a guaranteed-dead token, 401-ing every REST leg, without ever minting.
//! The log line said one thing and the control flow did another.

const TOKEN_MANAGER: &str = include_str!("../src/auth/token_manager.rs");

/// Strip `//` line comments so a symbol named in prose cannot satisfy a scan
/// meant to find real code.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn index_of(code: &str, needle: &str) -> usize {
    code.find(needle)
        .unwrap_or_else(|| panic!("token_manager.rs no longer contains `{needle}`"))
}

#[test]
fn ssm_is_read_before_the_local_cache() {
    let code = code_only(TOKEN_MANAGER);
    let ssm = index_of(&code, "fetch_dhan_access_token(");
    let cache = index_of(&code, "load_token_cache(");
    assert!(
        ssm < cache,
        "the shared SSM token must be read BEFORE the local cache. Reading the cache first \
         re-creates the re-mint war: the box adopts its own older token, mints when that \
         fails, and invalidates the scheduled minter's token for every other consumer."
    );
}

#[test]
fn an_ssm_token_is_never_adopted_without_a_broker_probe() {
    let code = code_only(TOKEN_MANAGER);
    let probe = index_of(&code, "get_user_profile(");
    let adopt = index_of(&code, "NotificationEvent::AuthenticationSuccess");
    assert!(
        probe < adopt,
        "adoption of the SSM token must be gated on a live broker probe. `exp` in the JWT \
         only proves the token has not TIMED OUT — it cannot show that someone else's mint \
         invalidated it, which is the failure mode that actually happens."
    );
}

#[test]
fn a_broker_rejected_token_does_not_fall_through_to_the_staler_cache() {
    let code = code_only(TOKEN_MANAGER);
    assert!(
        code.contains("shared_token_rejected_by_broker = true"),
        "the broker-rejection arm must record that it happened; without the flag the arm's \
         own warning (\"falling through to mint\") is false"
    );
    assert!(
        code.contains("if !shared_token_rejected_by_broker"),
        "the local-cache fast path must be skipped once the broker has rejected the SSM \
         token. The cache is strictly staler than a token Dhan already refused, so taking \
         it hands the session a dead token and never reaches the mint."
    );
    let flag_set = index_of(&code, "shared_token_rejected_by_broker = true");
    let cache = index_of(&code, "load_token_cache(");
    assert!(
        flag_set < cache,
        "the rejection flag must be set BEFORE the cache is consulted, or the guard on the \
         cache reads a value that has not been written yet"
    );
}

#[test]
fn an_emergency_mint_still_publishes_so_peers_converge() {
    let code = code_only(TOKEN_MANAGER);
    let publishes = code.matches("publish_current_token_to_ssm(").count();
    assert!(
        publishes >= 2,
        "a mint or renewal performed by the box MUST publish to SSM (found {publishes} call \
         sites, expected at least 2 — after acquire_token and after try_renew_token). The box \
         stays an EMERGENCY minter for the case where the scheduled Lambda has failed, and an \
         emergency-minted token that is not published leaves every other consumer holding the \
         token this mint just invalidated."
    );
}

#[test]
fn initialize_never_writes_a_parameter_directly() {
    let code = code_only(TOKEN_MANAGER);
    assert!(
        !code.contains("put_parameter"),
        "token_manager.rs must not call put_parameter directly — SSM writes go through \
         dhan_token_publisher, which is the single audited write site"
    );
}

#[test]
fn guard_self_test_comment_stripping_is_not_vacuous() {
    // A guard that would pass on an empty file proves nothing.
    let stripped = code_only("// load_token_cache( in a comment\nlet x = 1;");
    assert!(
        !stripped.contains("load_token_cache("),
        "code_only must drop commented mentions, or every ordering assertion above can be \
         satisfied by prose"
    );
    assert!(
        stripped.contains("let x = 1;"),
        "code_only must keep real code"
    );
}
