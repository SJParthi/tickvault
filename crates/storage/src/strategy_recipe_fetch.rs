//! Read-only fetch of one published brutex strategy from
//! `s3://<cold bucket>/strategy-recipes/<name>/` into a local staging folder
//! (rule `brutex-strategy-host-2026-10-29.md`). ListObjectsV2 and GetObject
//! ONLY: this module never puts, copies or deletes in the bucket (a source scan
//! below fails the build if it ever does).
//!
//! What it downloads: the newest `v<N>` folder's own files, its two newest
//! parameter snapshots dated on or before the trading day (the second is the
//! "previous snapshot" for the sanity gate), and the newest universe snapshot.
//! The verification itself (manifest, gates, replay) happens on the local copy in
//! `tickvault_trading::brutex_host::loader`. Cold path: once before the open.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tickvault_trading::brutex_host::select::{pick_version, usable_snapshot_dates};

use crate::s3_cold::S3Cold;

/// The bucket prefix every strategy lives under.
pub const RECIPE_ROOT_PREFIX: &str = "strategy-recipes/";
/// A single published file larger than this is refused (disk safety).
pub const MAX_RECIPE_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// The whole download is refused past this (disk safety on a 150 GB root).
pub const MAX_RECIPE_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Which keys to download, and where each lands relative to the staging root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadPlan {
    pub version: Option<u32>,
    pub files: Vec<(String, PathBuf, u64)>,
    pub total_bytes: u64,
}

fn segments(rest: &str) -> Vec<&str> {
    rest.split('/').collect()
}

fn safe_relative(rest: &str) -> Option<PathBuf> {
    let parts = segments(rest);
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('\\'))
    {
        return None;
    }
    Some(parts.iter().collect())
}

/// Chooses the keys to fetch from a listing of `strategy-recipes/<name>/`.
/// Pure. Keys outside the strategy's prefix, with `..` or empty segments, or
/// over the size limits are refused rather than skipped silently.
pub fn plan_download(
    listing: &[(String, u64)],
    strategy: &str,
    trading_day: &str,
) -> Result<DownloadPlan> {
    if strategy.is_empty() || strategy.contains('/') || strategy.contains("..") {
        bail!("bad strategy name {strategy:?}");
    }
    let prefix = format!("{RECIPE_ROOT_PREFIX}{strategy}/");
    let mut rel: Vec<(&str, &str, u64)> = Vec::new(); // (key, rest, size)
    for (key, size) in listing {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        if rest.is_empty() || rest.ends_with('/') {
            continue; // folder marker
        }
        if safe_relative(rest).is_none() {
            bail!("refusing key with an unsafe path: {key:?}");
        }
        if *size > MAX_RECIPE_FILE_BYTES {
            bail!("refusing {key}: {size} bytes is over the {MAX_RECIPE_FILE_BYTES}-byte limit");
        }
        rel.push((key.as_str(), rest, *size));
    }
    let versions: Vec<&str> = rel
        .iter()
        .filter_map(|(_, r, _)| segments(r).first().copied())
        .collect();
    let pick = pick_version(versions.iter().copied());
    let Some(version) = pick.newest else {
        return Ok(DownloadPlan {
            version: None,
            files: Vec::new(),
            total_bytes: 0,
        });
    };
    let vname = format!("v{version}");
    let param_dates: Vec<&str> = rel
        .iter()
        .filter_map(|(_, r, _)| {
            let s = segments(r);
            (s.len() == 4 && s[0] == vname && s[1] == "params").then(|| s[2])
        })
        .collect();
    let (params_ok, _) = usable_snapshot_dates(param_dates, trading_day);
    let keep_params: Vec<String> = params_ok.into_iter().take(2).collect();
    let uni_dates: Vec<&str> = rel
        .iter()
        .filter_map(|(_, r, _)| {
            let s = segments(r);
            (s.len() == 3 && s[0] == "universe").then(|| s[1])
        })
        .collect();
    let (uni_ok, _) = usable_snapshot_dates(uni_dates, trading_day);
    let keep_uni = uni_ok.first().cloned();

    let mut files = Vec::new();
    let mut total: u64 = 0;
    for (key, rest, size) in rel {
        let s = segments(rest);
        let wanted = (s.len() == 2 && s[0] == vname)
            || (s.len() == 4
                && s[0] == vname
                && s[1] == "params"
                && keep_params.iter().any(|d| d == s[2]))
            || (s.len() == 3 && s[0] == "universe" && keep_uni.as_deref() == Some(s[1]));
        if !wanted {
            continue;
        }
        total = total.saturating_add(size);
        if total > MAX_RECIPE_TOTAL_BYTES {
            bail!("refusing the download: over {MAX_RECIPE_TOTAL_BYTES} bytes");
        }
        let local = safe_relative(rest).with_context(|| format!("unsafe key {key}"))?;
        files.push((key.to_owned(), local, size));
    }
    files.sort();
    Ok(DownloadPlan {
        version: Some(version),
        files,
        total_bytes: total,
    })
}

/// Lists, plans and downloads one strategy into `staging_root/<strategy>/`
/// (cleared first, so a file removed upstream cannot linger). Returns the local
/// strategy folder for the loader. Read-only on S3.
// TEST-EXEMPT: two read-only AWS SDK calls (list + get) around the pure plan_download, which is tested
pub async fn fetch_strategy(
    s3: &S3Cold,
    strategy: &str,
    trading_day: &str,
    staging_root: &Path,
) -> Result<(PathBuf, DownloadPlan)> {
    let listing = s3
        .list_keys(&format!("{RECIPE_ROOT_PREFIX}{strategy}/"))
        .await?;
    let plan = plan_download(&listing, strategy, trading_day)?;
    let dest = staging_root.join(strategy);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("clear {}", dest.display()))?;
    }
    std::fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
    for (key, local, _) in &plan.files {
        s3.get_to_path(key, &dest.join(local)).await?;
    }
    Ok((dest, plan))
}

// O(1) EXEMPT: end
#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str, n: u64) -> (String, u64) {
        (format!("strategy-recipes/demo/{s}"), n)
    }

    #[test]
    fn test_plan_download_picks_newest_version_two_newest_params_and_newest_universe() {
        let listing = vec![
            k("v1/recipe.json", 10),
            k("v3/recipe.json", 10),
            k("v3/manifest.json", 5),
            k("v3/params/2026-10-27/params.json", 3),
            k("v3/params/2026-10-28/params.json", 3),
            k("v3/params/2026-10-29/params.json", 3),
            k("v3/params/2026-10-31/params.json", 3),
            k("universe/2026-10-28/universe.json", 4),
            k("universe/2026-10-29/universe.json", 4),
            ("strategy-recipes/other/v9/recipe.json".to_owned(), 1),
        ];
        let p = plan_download(&listing, "demo", "2026-10-30").unwrap();
        assert_eq!(p.version, Some(3));
        let keys: Vec<&str> = p.files.iter().map(|(k, _, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "strategy-recipes/demo/universe/2026-10-29/universe.json",
                "strategy-recipes/demo/v3/manifest.json",
                "strategy-recipes/demo/v3/params/2026-10-28/params.json",
                "strategy-recipes/demo/v3/params/2026-10-29/params.json",
                "strategy-recipes/demo/v3/recipe.json",
            ]
        );
        assert_eq!(p.total_bytes, 4 + 5 + 3 + 3 + 10);
    }

    #[test]
    fn test_plan_download_refuses_unsafe_keys_and_oversize() {
        assert!(plan_download(&[k("v1/../../etc/passwd", 1)], "demo", "2026-10-30").is_err());
        assert!(plan_download(&[k("v1//x", 1)], "demo", "2026-10-30").is_err());
        assert!(
            plan_download(
                &[k("v1/big.csv", MAX_RECIPE_FILE_BYTES + 1)],
                "demo",
                "2026-10-30"
            )
            .is_err()
        );
        let many: Vec<(String, u64)> = (0..5)
            .map(|i| k(&format!("v1/f{i}.csv"), MAX_RECIPE_FILE_BYTES))
            .collect();
        assert!(plan_download(&many, "demo", "2026-10-30").is_err());
        assert!(plan_download(&[], "../x", "2026-10-30").is_err());
    }

    #[test]
    fn test_plan_download_with_no_version_downloads_nothing() {
        let p = plan_download(
            &[
                k("universe/2026-10-29/universe.json", 1),
                k("v0/recipe.json", 1),
            ],
            "demo",
            "2026-10-30",
        )
        .unwrap();
        assert_eq!(p.version, None);
        assert!(p.files.is_empty());
    }

    /// The bucket is read-only to tickvault: this file may call only List and Get.
    #[test]
    fn test_fetch_strategy_never_calls_a_write_api() {
        let src = include_str!("strategy_recipe_fetch.rs");
        let code = src.split("#[cfg(test)]").next().unwrap_or("");
        for banned in [
            "put_object",
            "put_if_absent",
            "delete_object",
            ".delete(",
            "copy_object",
            "upload_part",
            "create_multipart",
        ] {
            assert!(
                !code.contains(banned),
                "strategy_recipe_fetch must stay read-only; found {banned}"
            );
        }
    }
}
