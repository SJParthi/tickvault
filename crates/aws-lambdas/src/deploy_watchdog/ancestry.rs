//! Exact-SHA deployment ancestry decisions. No provider calls or side effects.
//!
//! GitHub compares BASE...HEAD. Here BASE is the recorded deployed binary and
//! HEAD is the observed target: an `ahead` response means the BINARY is behind.
//! A mismatch alone never authorizes a deploy or an old-build notification.

use serde_json::Value;

const FULL_SHA_BYTES: usize = 40;
const MAX_REPOSITORY_COMPONENT_BYTES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BuildRelation {
    Current,
    BinaryBehind,
    BinaryAhead,
    Diverged,
    Unknown,
}

impl BuildRelation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::BinaryBehind => "binary_behind",
            Self::BinaryAhead => "binary_ahead",
            Self::Diverged => "diverged",
            Self::Unknown => "unknown",
        }
    }

    pub(super) const fn permits_dispatch(self) -> bool {
        matches!(self, Self::BinaryBehind)
    }

    /// None means that a Boolean stale/healthy verdict would hide uncertainty.
    pub(super) const fn stale_value(self) -> Option<bool> {
        match self {
            Self::BinaryBehind => Some(true),
            Self::Current | Self::BinaryAhead => Some(false),
            Self::Diverged | Self::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BuildComparison {
    pub(super) relation: BuildRelation,
    pub(super) reason: &'static str,
    pub(super) http_status: Option<u16>,
}

impl BuildComparison {
    pub(super) const fn unknown(reason: &'static str, status: Option<u16>) -> Self {
        Self {
            relation: BuildRelation::Unknown,
            reason,
            http_status: status,
        }
    }

    pub(super) const fn current() -> Self {
        Self {
            relation: BuildRelation::Current,
            reason: "matching_exact_sha",
            http_status: None,
        }
    }
}

pub(super) fn validated_full_sha(value: Option<&str>) -> Option<&str> {
    let value = value?.trim();
    (value.len() == FULL_SHA_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(value)
}

/// Bounded, literal commit IDs; no branch names, paths or query injection.
pub(super) fn comparison_path(repo: &str, binary: &str, desired: &str) -> Option<String> {
    let (owner, name) = repo.split_once('/')?;
    if ![owner, name].into_iter().all(|part| {
        !part.is_empty()
            && part.len() <= MAX_REPOSITORY_COMPONENT_BYTES
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    }) {
        return None;
    }
    let binary = validated_full_sha(Some(binary))?;
    let desired = validated_full_sha(Some(desired))?;
    // Page 2 omits GitHub's first-page changed-file/patch list. Only the
    // comparison metadata is needed; no commit/file list is trusted here.
    Some(format!(
        "/repos/{repo}/compare/{binary}...{desired}?per_page=1&page=2"
    ))
}

pub(super) fn classify_comparison(
    binary: &str,
    desired: &str,
    status: u16,
    body: &Value,
) -> BuildComparison {
    let (Some(binary), Some(desired)) = (
        validated_full_sha(Some(binary)),
        validated_full_sha(Some(desired)),
    ) else {
        return BuildComparison::unknown("invalid_sha", None);
    };
    if binary == desired {
        return BuildComparison::current();
    }
    if status != 200 {
        return BuildComparison::unknown(
            if status == 0 {
                "github_unavailable"
            } else {
                "github_http_error"
            },
            Some(status),
        );
    }
    let unknown = BuildComparison::unknown("invalid_comparison", Some(status));
    if body.pointer("/base_commit/sha").and_then(Value::as_str) != Some(binary) {
        return unknown;
    }
    let Some(merge_base) = validated_full_sha(
        body.pointer("/merge_base_commit/sha")
            .and_then(Value::as_str),
    ) else {
        return unknown;
    };
    let (Some(ahead), Some(behind)) = (
        body.get("ahead_by").and_then(Value::as_u64),
        body.get("behind_by").and_then(Value::as_u64),
    ) else {
        return unknown;
    };
    let relation = match body.get("status").and_then(Value::as_str) {
        Some("ahead") if ahead > 0 && behind == 0 && merge_base == binary => {
            BuildRelation::BinaryBehind
        }
        Some("behind") if ahead == 0 && behind > 0 && merge_base == desired => {
            BuildRelation::BinaryAhead
        }
        Some("diverged")
            if ahead > 0 && behind > 0 && merge_base != binary && merge_base != desired =>
        {
            BuildRelation::Diverged
        }
        _ => return unknown,
    };
    BuildComparison {
        relation,
        reason: "verified_ancestry",
        http_status: Some(status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BINARY: &str = "37328206793e099b0781c4b449a0859e17387a73";
    const MIRROR: &str = "49dd25239bde21411008b11bd76c51d5e0f8284f";
    const COMMON: &str = "713e7a9dd7a6c19f1be18cb006643d7953a1e34a";

    fn comparison(base: &str, merge_base: &str, status: &str, ahead: u64, behind: u64) -> Value {
        json!({
            "base_commit": {"sha": base},
            "merge_base_commit": {"sha": merge_base},
            "status": status,
            "ahead_by": ahead,
            "behind_by": behind,
        })
    }

    #[test]
    fn newer_binary_than_deploy_relevant_mirror_never_dispatches() {
        let result = classify_comparison(
            BINARY,
            MIRROR,
            200,
            &comparison(BINARY, MIRROR, "behind", 0, 2),
        );
        assert_eq!(result.relation, BuildRelation::BinaryAhead);
        assert_eq!(result.relation.stale_value(), Some(false));
        assert!(!result.relation.permits_dispatch());
    }

    #[test]
    fn actual_binary_ancestor_of_target_permits_dispatch() {
        let result = classify_comparison(
            MIRROR,
            BINARY,
            200,
            &comparison(MIRROR, MIRROR, "ahead", 2, 0),
        );
        assert_eq!(result.relation, BuildRelation::BinaryBehind);
        assert_eq!(result.relation.stale_value(), Some(true));
        assert!(result.relation.permits_dispatch());
    }

    #[test]
    fn divergent_history_is_discrepancy_without_automatic_downgrade() {
        let result = classify_comparison(
            BINARY,
            MIRROR,
            200,
            &comparison(BINARY, COMMON, "diverged", 2, 3),
        );
        assert_eq!(result.relation, BuildRelation::Diverged);
        assert_eq!(result.relation.stale_value(), None);
        assert!(!result.relation.permits_dispatch());
    }

    #[test]
    fn unavailable_comparison_never_promotes_mirror_mismatch_to_old_build() {
        for status in [0, 301, 401, 403, 404, 429, 500, 503] {
            let result = classify_comparison(BINARY, MIRROR, status, &json!({}));
            assert_eq!(result.relation, BuildRelation::Unknown, "status={status}");
            assert_eq!(result.http_status, Some(status));
            assert_eq!(result.relation.stale_value(), None);
            assert!(!result.relation.permits_dispatch());
        }
    }

    #[test]
    fn exact_identity_needs_no_comparison_response() {
        assert_eq!(
            classify_comparison(BINARY, BINARY, 0, &Value::Null),
            BuildComparison::current()
        );
    }

    #[test]
    fn shared_seven_character_prefix_is_not_exact_identity() {
        let first = "abcdef0123456789012345678901234567890123";
        let second = "abcdef0123456789012345678901234567890124";
        let result =
            classify_comparison(first, second, 200, &comparison(first, first, "ahead", 1, 0));
        assert_eq!(result.relation, BuildRelation::BinaryBehind);
    }

    #[test]
    fn inconsistent_or_missing_comparison_metadata_fails_closed() {
        let valid = comparison(MIRROR, MIRROR, "ahead", 2, 0);
        let mut cases = vec![Value::Null, json!({}), json!([])];
        for (key, value) in [
            ("status", json!("behind")),
            ("status", json!("identical")),
            ("status", json!("future_status")),
            ("ahead_by", json!(0)),
            ("ahead_by", json!(-1)),
            ("ahead_by", json!(2.0)),
            ("ahead_by", json!(null)),
            ("behind_by", json!(1)),
            ("behind_by", json!("0")),
            ("base_commit", json!({"sha": BINARY})),
            ("merge_base_commit", json!({"sha": COMMON})),
            ("merge_base_commit", json!({"sha": "unknown"})),
        ] {
            let mut changed = valid.clone();
            changed[key] = value;
            cases.push(changed);
        }
        for body in cases {
            let result = classify_comparison(MIRROR, BINARY, 200, &body);
            assert_eq!(result.relation, BuildRelation::Unknown, "body={body}");
            assert!(!result.relation.permits_dispatch());
        }
    }

    #[test]
    fn malformed_or_abbreviated_commit_ids_never_compare() {
        for invalid in ["", "unknown", "37328206", "main", "../main", "3732820?x=1"] {
            assert!(validated_full_sha(Some(invalid)).is_none());
            assert!(comparison_path("SJParthi/tickvault", invalid, MIRROR).is_none());
            assert_eq!(
                classify_comparison(invalid, MIRROR, 200, &json!({})).relation,
                BuildRelation::Unknown
            );
        }
        assert!(validated_full_sha(Some(&BINARY.to_uppercase())).is_none());
        assert_eq!(
            validated_full_sha(Some(&format!(" {BINARY}\n"))),
            Some(BINARY)
        );
    }

    #[test]
    fn comparison_path_binds_full_shas_and_excludes_file_patch_page() {
        assert_eq!(
            comparison_path("SJParthi/tickvault", BINARY, MIRROR),
            Some(format!(
                "/repos/SJParthi/tickvault/compare/{BINARY}...{MIRROR}?per_page=1&page=2"
            ))
        );
        for repo in [
            "",
            "../tickvault",
            "SJParthi/..",
            "a/b/c",
            "a/b?x=1",
            "a/b#x",
            "a/%2f",
        ] {
            assert!(
                comparison_path(repo, BINARY, MIRROR).is_none(),
                "repo={repo}"
            );
        }
    }

    #[test]
    fn only_verified_behind_can_dispatch_or_receive_deploy_alert() {
        for relation in [
            BuildRelation::Current,
            BuildRelation::BinaryBehind,
            BuildRelation::BinaryAhead,
            BuildRelation::Diverged,
            BuildRelation::Unknown,
        ] {
            assert_eq!(
                relation.permits_dispatch(),
                relation == BuildRelation::BinaryBehind
            );
        }
    }
}
