//! Manifest verification: every file in a published folder is listed with its
//! sha256 and size, nothing is missing, nothing extra. O(total bytes) once per
//! load, cold.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::{Refusal, hex_lower};

/// The manifest file name; it lists every other file and is not listed itself.
pub const MANIFEST_NAME: &str = "manifest.json";
/// The schema string a manifest must carry.
pub const MANIFEST_SCHEMA: &str = "tickvault-strategy-manifest/1";

#[derive(Debug, Clone, serde::Deserialize)]
struct ManifestFile {
    schema: String,
    files: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ManifestEntry {
    name: String,
    sha256: String,
    bytes: u64,
}

/// A folder whose every file matched its manifest, with the file bytes.
#[derive(Debug, Clone)]
pub struct VerifiedFolder {
    files: BTreeMap<String, Vec<u8>>,
}

impl VerifiedFolder {
    /// The bytes of a verified file, or `MissingFile` if the manifest did not list it.
    pub fn file(&self, name: &str) -> Result<&[u8], Refusal> {
        self.files
            .get(name)
            .map(Vec::as_slice)
            .ok_or_else(|| Refusal::MissingFile {
                name: name.to_owned(),
            })
    }

    /// Whether the folder holds a verified file of that name.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.files.contains_key(name)
    }

    /// The verified file names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.files.keys().map(String::as_str).collect()
    }
}

/// Verifies the manifest of one folder (files directly in it; subfolders such
/// as `params/` are separate snapshots and are skipped).
pub fn verify_folder(dir: &Path) -> Result<VerifiedFolder, Refusal> {
    let manifest_bytes = read(dir, MANIFEST_NAME)?;
    let manifest: ManifestFile =
        serde_json::from_slice(&manifest_bytes).map_err(|e| Refusal::BadManifest(e.to_string()))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(Refusal::BadManifest(format!(
            "schema {:?}",
            manifest.schema
        )));
    }
    let mut listed: BTreeMap<String, ManifestEntry> = BTreeMap::new();
    for entry in manifest.files {
        if entry.name == MANIFEST_NAME
            || entry.name.contains('/')
            || entry.name.contains('\\')
            || entry.name.starts_with('.')
        {
            return Err(Refusal::BadManifest(format!(
                "bad file name {:?}",
                entry.name
            )));
        }
        if entry.sha256.len() != 64 || !entry.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Refusal::BadManifest(format!(
                "bad sha256 for {}",
                entry.name
            )));
        }
        let name = entry.name.clone();
        if listed.insert(name.clone(), entry).is_some() {
            return Err(Refusal::BadManifest(format!("{name} listed twice")));
        }
    }
    if listed.is_empty() {
        return Err(Refusal::BadManifest("lists no files".to_owned()));
    }
    // Unlisted extra files refuse the folder (a file brutex did not vouch for).
    let entries = std::fs::read_dir(dir).map_err(|e| Refusal::Unreadable {
        name: dir.display().to_string(),
        detail: e.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| Refusal::Unreadable {
            name: dir.display().to_string(),
            detail: e.to_string(),
        })?;
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != MANIFEST_NAME && !listed.contains_key(&name) {
            return Err(Refusal::UnlistedFile { name });
        }
    }
    let mut files = BTreeMap::new();
    for (name, entry) in listed {
        let path = dir.join(&name);
        if !path.is_file() {
            return Err(Refusal::MissingFile { name });
        }
        let bytes = read(dir, &name)?;
        let got_len = bytes.len() as u64;
        if got_len != entry.bytes {
            return Err(Refusal::SizeMismatch {
                name,
                expected: entry.bytes,
                got: got_len,
            });
        }
        let got = hex_lower(&Sha256::digest(&bytes));
        if !got.eq_ignore_ascii_case(&entry.sha256) {
            return Err(Refusal::HashMismatch {
                name,
                expected: entry.sha256.to_ascii_lowercase(),
                got,
            });
        }
        files.insert(name, bytes);
    }
    Ok(VerifiedFolder { files })
}

fn read(dir: &Path, name: &str) -> Result<Vec<u8>, Refusal> {
    std::fs::read(dir.join(name)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Refusal::MissingFile {
                name: name.to_owned(),
            }
        } else {
            Refusal::Unreadable {
                name: name.to_owned(),
                detail: e.to_string(),
            }
        }
    })
}

/// Builds manifest JSON for the given files (test fixtures and the dry run's
/// self-check). Never used to vouch for a downloaded folder.
#[must_use]
pub fn manifest_json_for(files: &[(&str, &[u8])]) -> String {
    let entries: Vec<serde_json::Value> = files
        .iter()
        .map(|(n, b)| serde_json::json!({ "name": n, "sha256": hex_lower(&Sha256::digest(b)), "bytes": b.len() }))
        .collect();
    serde_json::json!({ "schema": MANIFEST_SCHEMA, "files": entries }).to_string()
}
// O(1) EXEMPT: end
