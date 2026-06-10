//! Dhan REST URL join normalization (DHAN-REST-400 item 1b, 2026-06-10).
//!
//! Every Dhan REST URL used to be built with `format!("{base}{path}")`. A
//! base-URL override carrying a trailing slash (env var / per-env TOML) then
//! produced `https://api.dhan.co/v2//ip/getIP` — a malformed path that 400s
//! EVERY REST endpoint at once while the WebSocket feed (separate URL
//! construction) stays healthy. [`join_api_url`] guarantees exactly one `/`
//! between base and path regardless of how either side is configured.

/// Joins an API base URL and a path with exactly one `/` between them.
///
/// - `("https://api.dhan.co/v2",  "/profile")` → `https://api.dhan.co/v2/profile`
/// - `("https://api.dhan.co/v2/", "/profile")` → `https://api.dhan.co/v2/profile`
/// - `("https://api.dhan.co/v2/", "profile")`  → `https://api.dhan.co/v2/profile`
/// - `("https://api.dhan.co/v2",  "profile")`  → `https://api.dhan.co/v2/profile`
///
/// The scheme's `//` is untouched (only the base's TRAILING slashes are
/// trimmed). Pure, cold-path, never panics.
#[must_use]
pub fn join_api_url(base: &str, path: &str) -> String {
    let trimmed_base = base.trim_end_matches('/');
    let trimmed_path = path.trim_start_matches('/');
    if trimmed_path.is_empty() {
        return trimmed_base.to_string();
    }
    format!("{trimmed_base}/{trimmed_path}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{
        DHAN_GENERATE_TOKEN_PATH, DHAN_GET_IP_PATH, DHAN_RENEW_TOKEN_PATH, DHAN_USER_PROFILE_PATH,
    };

    /// `//` anywhere after the scheme means a malformed join.
    fn has_double_slash_after_scheme(url: &str) -> bool {
        let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
        after_scheme.contains("//")
    }

    /// RATCHET (task DHAN-REST-400 item 1b): a trailing-slash base can never
    /// produce `//` in the joined URL.
    #[test]
    fn test_join_api_url_trailing_slash_base_no_double_slash() {
        let url = join_api_url("https://api.dhan.co/v2/", "/ip/getIP");
        assert_eq!(url, "https://api.dhan.co/v2/ip/getIP");
        assert!(!has_double_slash_after_scheme(&url));
    }

    #[test]
    fn test_join_api_url_all_four_slash_combinations() {
        for base in ["https://api.dhan.co/v2", "https://api.dhan.co/v2/"] {
            for path in ["/profile", "profile"] {
                let url = join_api_url(base, path);
                assert_eq!(
                    url, "https://api.dhan.co/v2/profile",
                    "base={base} path={path}"
                );
                assert!(!has_double_slash_after_scheme(&url));
            }
        }
    }

    #[test]
    fn test_join_api_url_preserves_scheme_double_slash() {
        let url = join_api_url("https://auth.dhan.co/", "/app/generateAccessToken");
        assert!(url.starts_with("https://"));
        assert_eq!(url, "https://auth.dhan.co/app/generateAccessToken");
    }

    /// Every real Dhan path constant joined against a hostile trailing-slash
    /// base must produce a clean URL — this is the exact failure shape that
    /// would 400 ALL REST endpoints at once (2026-06-10 incident hypothesis).
    #[test]
    fn test_join_api_url_real_dhan_path_constants_never_double_slash() {
        for path in [
            DHAN_USER_PROFILE_PATH,
            DHAN_GET_IP_PATH,
            DHAN_RENEW_TOKEN_PATH,
            DHAN_GENERATE_TOKEN_PATH,
            "/charts/intraday",
        ] {
            let url = join_api_url("https://api.dhan.co/v2/", path);
            assert!(
                !has_double_slash_after_scheme(&url),
                "double slash for path {path}: {url}"
            );
            assert!(url.ends_with(path.trim_start_matches('/')));
        }
    }

    #[test]
    fn test_join_api_url_multiple_trailing_slashes_collapsed() {
        let url = join_api_url("https://api.dhan.co/v2///", "///profile");
        assert_eq!(url, "https://api.dhan.co/v2/profile");
    }

    #[test]
    fn test_join_api_url_empty_path_returns_trimmed_base() {
        assert_eq!(
            join_api_url("https://api.dhan.co/v2/", ""),
            "https://api.dhan.co/v2"
        );
        assert_eq!(
            join_api_url("https://api.dhan.co/v2", "/"),
            "https://api.dhan.co/v2"
        );
    }
}
