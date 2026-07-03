//! Compile-time build provenance — B9 deploy provenance.
//!
//! The provenance chain (see `.claude/rules/project/deploy-provenance.md`):
//!
//! ```text
//! GITHUB_SHA (CI) / git rev-parse HEAD (dev)
//!     └─ crates/common/build.rs ─▶ env!("TICKVAULT_GIT_SHA")
//!            ├─ GET /health  "git_sha" field   (crates/api handlers/health.rs)
//!            └─ boot Telegram "Build: <short>" (crates/core notification/events.rs)
//! ```
//!
//! Honest envelope: the SHA is EXACT for CI-built artifacts (`GITHUB_SHA`
//! is authoritative in GitHub Actions). Local dev builds fall back to
//! `git rev-parse HEAD` at build-script run time and may lag the working
//! tree's HEAD until the build script re-runs; builds without git embed
//! the literal `"unknown"`. The value is validated at build time as 40
//! lowercase-hex characters — it is never garbage.

/// Full 40-hex git commit SHA the binary was built from, or `"unknown"`
/// when no SHA could be resolved at build time. Stamped by
/// `crates/common/build.rs`.
pub const BUILD_GIT_SHA: &str = env!("TICKVAULT_GIT_SHA");

/// First 7 characters of [`BUILD_GIT_SHA`] — the conventional short SHA
/// used in operator-facing surfaces (boot Telegram, portal footer).
///
/// No panic path: if the embedded value is shorter than 7 bytes the full
/// value is returned as-is.
#[must_use]
pub fn build_git_sha_short() -> &'static str {
    BUILD_GIT_SHA.get(..7).unwrap_or(BUILD_GIT_SHA)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_lower_hex_40(s: &str) -> bool {
        s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    #[test]
    fn test_build_git_sha_is_40_hex_or_unknown() {
        assert!(
            !BUILD_GIT_SHA.is_empty(),
            "BUILD_GIT_SHA must never be empty"
        );
        assert!(
            is_lower_hex_40(BUILD_GIT_SHA) || BUILD_GIT_SHA == "unknown",
            "BUILD_GIT_SHA must be a full lowercase-hex sha or the literal \
             'unknown', got: {BUILD_GIT_SHA}"
        );
    }

    #[test]
    fn test_build_git_sha_short_is_prefix_and_max_7() {
        let short = build_git_sha_short();
        assert!(
            short.len() <= 7,
            "short sha must be at most 7 chars: {short}"
        );
        assert!(
            BUILD_GIT_SHA.starts_with(short),
            "short sha must be a prefix of the full value"
        );
        assert!(!short.is_empty(), "short sha must never be empty");
    }
}
