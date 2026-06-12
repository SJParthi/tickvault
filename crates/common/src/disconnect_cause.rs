//! WebSocket disconnect-cause classifier — best-guess **source** attribution
//! (Dhan-side / network / token / unknown) from the raw transport-error string
//! plus an optional Dhan disconnect code.
//!
//! Operator request 2026-06-12: the disconnect/reconnect Telegram showed the raw
//! transport error ("Connection reset without closing handshake", "Handshake not
//! finished") but never told the operator the likely *source* — our box, the
//! network, or Dhan. This classifier adds that, in plain English.
//!
//! # Honest envelope (no hallucination)
//! A single TCP reset **cannot** be perfectly attributed to AWS-vs-network-vs-Dhan
//! from one event. So the resets map to a best-guess bucket with **explicit
//! uncertainty wording + a confirm-hint**, and the caller ALWAYS shows the raw
//! error alongside — a wrong bucket can never hide the truth.
//!
//! # Performance
//! Pure function, no allocation, cold path (disconnect events are rare). The
//! number of checks is a fixed constant; cost is bounded by the (short) reason
//! string length — effectively O(1) per call.

/// Best-guess source of a WebSocket disconnect / connect failure.
///
/// The variants are deliberately coarse + honest: where a single event cannot
/// distinguish "our AWS" from "the internet path" from "Dhan's server", they are
/// grouped (e.g. [`DisconnectCause::DhanOrNetworkReset`]) rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectCause {
    /// Dhan disconnect code 805 — too many connections on the account.
    /// Almost always a second device/app logged into the same Dhan login.
    DhanTooManyConnections,
    /// Dhan disconnect code 807 — the daily access token expired.
    DhanTokenExpired,
    /// Other Dhan-side rejections (auth failed / data-plan / invalid request:
    /// codes 806/808/809/810/811/812/813/814).
    DhanAuthOrSubscription,
    /// An established connection was reset mid-stream (no Dhan code). Could be
    /// Dhan's server, their load balancer, or the network path — NOT attributable
    /// from a single event. The app auto-reconnects.
    DhanOrNetworkReset,
    /// The connection could not be completed (handshake / TLS / DNS / refused /
    /// timeout). Usually the network path or this server's egress; sometimes
    /// Dhan being unreachable.
    NetworkOrTls,
    /// Could not classify — the raw error is shown so the operator still sees it.
    Unknown,
}

impl DisconnectCause {
    /// Short plain-English source label (no jargon, no library names — safe for
    /// the Telegram "Likely source:" line).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DhanTooManyConnections => "Dhan (another login)",
            Self::DhanTokenExpired => "Dhan (login token expired)",
            Self::DhanAuthOrSubscription => "Dhan (login / data plan)",
            Self::DhanOrNetworkReset => "Dhan or network",
            Self::NetworkOrTls => "Network / connection",
            Self::Unknown => "Unknown",
        }
    }

    /// One-line plain-English explanation of what likely happened.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::DhanTooManyConnections => {
                "another device or app is logged into your Dhan account, so Dhan dropped this one"
            }
            Self::DhanTokenExpired => {
                "your daily Dhan login expired; the app refreshes it automatically"
            }
            Self::DhanAuthOrSubscription => {
                "Dhan refused the connection — likely a login or data-plan issue"
            }
            Self::DhanOrNetworkReset => {
                "Dhan's server or the line in between dropped the connection; the app reconnects on its own"
            }
            Self::NetworkOrTls => {
                "the connection could not be completed (network, DNS or handshake)"
            }
            Self::Unknown => "the cause could not be identified — see the exact error above",
        }
    }

    /// How the operator can confirm the source (the honest "we can't be 100%
    /// sure from one event, here's how to check" hint).
    #[must_use]
    pub const fn confirm_hint(self) -> &'static str {
        match self {
            Self::DhanTooManyConnections => {
                "close other Dhan logins (the Dhan website's live chart, the Dhan mobile app, any other script)"
            }
            Self::DhanTokenExpired => {
                "auto-refresh handles it; if it keeps repeating, re-check the token / TOTP"
            }
            Self::DhanAuthOrSubscription => "check your Dhan data plan + login is active",
            Self::DhanOrNetworkReset => {
                "if other Dhan calls failed at the same time it is the network; if only the feed dropped it is Dhan-side"
            }
            Self::NetworkOrTls => {
                "check this server's internet + DNS to Dhan; if other sites work it is likely Dhan-side"
            }
            Self::Unknown => "investigate the exact error shown above",
        }
    }
}

/// Classify the best-guess source of a disconnect from the raw reason string and
/// an optional Dhan disconnect code.
///
/// A Dhan code (when present) takes precedence — it is authoritative. Otherwise
/// the raw transport-error string is matched against known signatures. Matching
/// is case-insensitive and the order encodes priority (a Dhan code embedded in
/// the string is honoured before the generic reset/handshake buckets).
///
/// # Performance
/// O(1) — a fixed number of substring checks over the (short) reason string. No
/// allocation. See module docs.
#[must_use]
pub fn classify_disconnect_cause(reason: &str, dhan_code: Option<u16>) -> DisconnectCause {
    // 1) Authoritative Dhan disconnect code, if we have it. This is the ONLY
    //    way a Dhan code (805/807/...) drives classification. We deliberately do
    //    NOT scrape digit substrings out of the reason text: a bare
    //    `contains("805")` would false-positive on prices, security_ids, byte
    //    counts ("18805", "8050ms", "os error 805"), mis-attributing a network
    //    reset to a Dhan bucket (security/hostile review 2026-06-12, MEDIUM).
    if let Some(code) = dhan_code {
        return classify_dhan_code(code);
    }

    let lower = reason.to_ascii_lowercase();

    // 2) Established-connection reset mid-stream — checked BEFORE the
    //    connect-phase signatures because "reset without closing handshake"
    //    contains the substring "handshake" and must NOT be mis-bucketed as a
    //    connect failure. Cannot attribute reset precisely (Dhan or network).
    if lower.contains("reset") || lower.contains("broken pipe") {
        return DisconnectCause::DhanOrNetworkReset;
    }

    // 4) Connect-phase failures (handshake / TLS / DNS / refused / timeout).
    //    Note: bare "handshake" is intentionally NOT a signature — only the
    //    specific "handshake not finished" — so "closing handshake" (a reset,
    //    handled above) can never reach here.
    const NETWORK_SIGNATURES: [&str; 7] = [
        "handshake not finished",
        "tls",
        "dns",
        "resolve",
        "refused",
        "timed out",
        "timeout",
    ];
    if NETWORK_SIGNATURES.iter().any(|s| lower.contains(s)) {
        return DisconnectCause::NetworkOrTls;
    }

    DisconnectCause::Unknown
}

/// Map a Dhan disconnect code to its source bucket. Codes per
/// `dhan-ref/08-annexure-enums.md` rule 12 (Data-API errors 805/807/...).
#[must_use]
const fn classify_dhan_code(code: u16) -> DisconnectCause {
    match code {
        805 => DisconnectCause::DhanTooManyConnections,
        807 => DisconnectCause::DhanTokenExpired,
        806 | 808 | 809 | 810 | 811 | 812 | 813 | 814 => DisconnectCause::DhanAuthOrSubscription,
        _ => DisconnectCause::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dhan_code_805_is_too_many_connections() {
        assert_eq!(
            classify_disconnect_cause("anything", Some(805)),
            DisconnectCause::DhanTooManyConnections
        );
    }

    #[test]
    fn test_dhan_code_807_is_token_expired() {
        assert_eq!(
            classify_disconnect_cause("anything", Some(807)),
            DisconnectCause::DhanTokenExpired
        );
    }

    #[test]
    fn test_dhan_code_auth_subscription_bucket() {
        for code in [806u16, 808, 809, 810, 811, 812, 813, 814] {
            assert_eq!(
                classify_disconnect_cause("x", Some(code)),
                DisconnectCause::DhanAuthOrSubscription,
                "code {code} should map to DhanAuthOrSubscription"
            );
        }
    }

    #[test]
    fn test_dhan_code_takes_precedence_over_string() {
        // Even if the string looks like a reset, an explicit code wins.
        assert_eq!(
            classify_disconnect_cause("connection reset without closing handshake", Some(805)),
            DisconnectCause::DhanTooManyConnections
        );
    }

    #[test]
    fn test_reset_without_closing_handshake_is_dhan_or_network() {
        assert_eq!(
            classify_disconnect_cause(
                "WebSocket protocol error: Connection reset without closing handshake",
                None
            ),
            DisconnectCause::DhanOrNetworkReset
        );
    }

    #[test]
    fn test_handshake_not_finished_is_network() {
        assert_eq!(
            classify_disconnect_cause(
                "connect failed: WebSocket protocol error: Handshake not finished",
                None
            ),
            DisconnectCause::NetworkOrTls
        );
    }

    #[test]
    fn test_dns_and_refused_and_timeout_are_network() {
        for r in [
            "dns resolution failed",
            "failed to resolve host",
            "connection refused",
            "operation timed out",
            "connect timeout",
            "tls handshake error",
        ] {
            assert_eq!(
                classify_disconnect_cause(r, None),
                DisconnectCause::NetworkOrTls,
                "{r:?} should classify as NetworkOrTls"
            );
        }
    }

    #[test]
    fn test_digits_in_reason_string_never_false_attribute_to_dhan() {
        // Regression (security/hostile review 2026-06-12, MEDIUM): a bare
        // `contains("805")`/`"807")` would mis-bucket prices, ids and byte
        // counts as Dhan codes. Dhan attribution comes ONLY from the
        // authoritative `dhan_code` param, never from scraped digit substrings.
        for r in [
            "timeout after 8050ms",          // contains "805" -> must stay network
            "id 18805 connection reset",     // contains "805" -> reset wins
            "write failed: os error 807",    // contains "807" -> not token-expired
            "price 2807.50 unrelated noise", // contains "807" -> unknown
        ] {
            let cause = classify_disconnect_cause(r, None);
            assert_ne!(
                cause,
                DisconnectCause::DhanTooManyConnections,
                "{r:?} must not be falsely attributed to Dhan-too-many"
            );
            assert_ne!(
                cause,
                DisconnectCause::DhanTokenExpired,
                "{r:?} must not be falsely attributed to Dhan-token-expired"
            );
        }
        // Sanity: the same codes ARE honoured when passed authoritatively.
        assert_eq!(
            classify_disconnect_cause("timeout after 8050ms", Some(805)),
            DisconnectCause::DhanTooManyConnections
        );
    }

    #[test]
    fn test_broken_pipe_is_dhan_or_network() {
        assert_eq!(
            classify_disconnect_cause("write failed: Broken pipe (os error 32)", None),
            DisconnectCause::DhanOrNetworkReset
        );
    }

    #[test]
    fn test_unrecognised_is_unknown() {
        assert_eq!(
            classify_disconnect_cause("some totally novel error text", None),
            DisconnectCause::Unknown
        );
        assert_eq!(
            classify_disconnect_cause("", None),
            DisconnectCause::Unknown
        );
    }

    #[test]
    fn test_unknown_dhan_code_is_unknown() {
        assert_eq!(
            classify_disconnect_cause("x", Some(999)),
            DisconnectCause::Unknown
        );
    }

    #[test]
    fn test_case_insensitive_matching() {
        assert_eq!(
            classify_disconnect_cause("CONNECTION RESET WITHOUT CLOSING HANDSHAKE", None),
            DisconnectCause::DhanOrNetworkReset
        );
    }

    #[test]
    fn test_every_variant_has_nonempty_label_explanation_hint() {
        for c in [
            DisconnectCause::DhanTooManyConnections,
            DisconnectCause::DhanTokenExpired,
            DisconnectCause::DhanAuthOrSubscription,
            DisconnectCause::DhanOrNetworkReset,
            DisconnectCause::NetworkOrTls,
            DisconnectCause::Unknown,
        ] {
            assert!(!c.label().is_empty(), "{c:?} label empty");
            assert!(!c.explanation().is_empty(), "{c:?} explanation empty");
            assert!(!c.confirm_hint().is_empty(), "{c:?} hint empty");
        }
    }
}
