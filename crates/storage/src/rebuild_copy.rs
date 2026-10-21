//! The file-copy and restore half of the 2026-10-22 rebuild (plan item 66,
//! operator Quotes 32/33/34, 2026-10-21).
//!
//! The partition copies live in `partition_archive` (they reuse the archiver's
//! export, never-overwrite placement and verify). This module holds what the
//! archiver has no use for:
//!
//! - [`RebuildCold`]: content-addressed upload of one file from the box's
//!   `data/` tree, its verified download onto the new disk, and the small JSON
//!   manifest that ties a copy run to its restore.
//! - [`QuestDbHttp`]: the restore's QuestDB calls — `SHOW CREATE TABLE` on the
//!   old box, the same DDL run on the new one, and the chunked `/imp` CSV import
//!   back into it, each verified by a row count.
//!
//! Nothing here deletes or overwrites anything. Every upload is
//! `If-None-Match: *` with a SHA-256 checksum to a key that embeds that
//! SHA-256, so a re-run reuses and a different content is a different key.
//!
//! Cold path only: the box-side `tickvault rebuild-box` subcommand, with the
//! app stopped. Never reached from the app's own boot.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use sha2::Digest as _;

use crate::s3_cold::{ExistingObject, S3Cold, base64_encode, classify_existing, hex_encode};

/// Every object the rebuild writes lives under this prefix in the cold
/// bucket (apart from the partition copies, which go to the archiver's own
/// `questdb-partitions/` layout).
pub const REBUILD_S3_PREFIX: &str = "rebuild-2026-10-22";

/// Prefix of the scratch tables the copy phase's restore rehearsal creates on
/// the OLD box and drops again. The only tables this module ever drops.
pub const REBUILD_PROBE_TABLE_PREFIX: &str = "rebuild_probe_";

/// Largest CSV body sent in one `/imp` request.
pub const IMPORT_CHUNK_BYTES: usize = 64 * 1024 * 1024;

/// Read block when hashing or decompressing a file.
const READ_BLOCK_BYTES: usize = 8 * 1024 * 1024;

/// Timeout of one QuestDB statement (DDL, count, SHOW CREATE TABLE).
const QUESTDB_STATEMENT_TIMEOUT_SECS: u64 = 120;

/// Timeout of one `/imp` chunk (64 MiB of CSV).
const QUESTDB_IMPORT_TIMEOUT_SECS: u64 = 900;

/// Multipart boundary for `/imp`. A CSV body never contains it: it is checked
/// before every send.
const IMP_BOUNDARY: &str = "tv-rebuild-2026-10-22-0c7f3a9e5d1b";

/// One file from the box's `data/` tree, copied and verified.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RebuildCopiedFile {
    /// Path relative to the `data/` root it was copied from.
    pub rel_path: String,
    /// The key the bytes live at.
    pub s3_key: String,
    /// File length.
    pub bytes: u64,
    /// SHA-256 of the file, lowercase hex.
    pub sha256_hex: String,
    /// True when this call uploaded it.
    pub uploaded: bool,
}

/// True when `rel` is a plain relative path the rebuild may turn into an S3
/// key and later back into a path under the restore root: non-empty, no
/// leading `/`, no `.` or `..` component, no empty component, printable ASCII
/// only. Anything else is refused, never rewritten.
#[must_use]
pub fn is_safe_rel_path(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && rel.bytes().all(|b| (0x21..=0x7e).contains(&b))
        && rel
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// The content-addressed key of a copied file:
/// `rebuild-2026-10-22/files/<rel>.sha-<hex>`.
#[must_use]
pub fn rebuild_file_key(rel: &str, sha256_hex: &str) -> String {
    format!("{REBUILD_S3_PREFIX}/files/{rel}.sha-{sha256_hex}")
}

/// SHA-256 (raw) and length of a file, read in 8 MiB blocks.
///
/// # Errors
/// Any read failure.
pub fn file_sha256(path: &Path) -> Result<([u8; 32], u64)> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0_u8; READ_BLOCK_BYTES];
    let mut len: u64 = 0;
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len = len.saturating_add(n as u64);
    }
    Ok((hasher.finalize().into(), len))
}

/// A cold-bucket handle for the rebuild.
pub struct RebuildCold {
    cold: S3Cold,
}

impl RebuildCold {
    /// Connects with the default credential chain (the instance role on the
    /// box) to an EXPLICIT bucket — never one derived from the environment.
    // TEST-EXEMPT: loads the live AWS config chain
    pub async fn connect(bucket: &str) -> Self {
        Self {
            cold: S3Cold::connect(bucket.to_string()).await,
        }
    }

    /// The bucket.
    #[must_use]
    pub fn bucket(&self) -> &str {
        self.cold.bucket()
    }

    /// Copies `<root>/<rel>` to its content-addressed key and verifies the
    /// stored length and SHA-256.
    // TEST-EXEMPT: S3 round trips; is_safe_rel_path, rebuild_file_key and file_sha256 are tested below
    pub async fn copy_file(&self, root: &Path, rel: &str) -> Result<RebuildCopiedFile, String> {
        if !is_safe_rel_path(rel) {
            return Err(format!("unsafe relative path {rel:?}"));
        }
        let path = root.join(rel);
        let (digest, bytes) = file_sha256(&path).map_err(|e| format!("{e:#}"))?;
        let sha_b64 = base64_encode(&digest);
        let sha_hex = hex_encode(&digest);
        let key = rebuild_file_key(rel, &sha_hex);
        let head = self.cold.head(&key).await.map_err(|e| format!("{e:#}"))?;
        let mut uploaded = false;
        match classify_existing(head.as_ref(), bytes, &sha_b64) {
            ExistingObject::Identical => {}
            ExistingObject::Different => {
                return Err(format!("{key} holds different bytes; not overwritten"));
            }
            ExistingObject::Absent => {
                self.cold
                    .put_if_absent_path(&path, &key, &sha_b64)
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                uploaded = true;
            }
        }
        let after = self.cold.head(&key).await.map_err(|e| format!("{e:#}"))?;
        if classify_existing(after.as_ref(), bytes, &sha_b64) != ExistingObject::Identical {
            return Err(format!("{key} did not verify after the upload"));
        }
        Ok(RebuildCopiedFile {
            rel_path: rel.to_string(),
            s3_key: key,
            bytes,
            sha256_hex: sha_hex,
            uploaded,
        })
    }

    /// Downloads `key` to `dest` (through `dest.part`, renamed only after its
    /// length and SHA-256 match), creating parent folders. An existing `dest`
    /// with the right content is left as it is; one with other content is an
    /// error (the restore never overwrites a file).
    // TEST-EXEMPT: S3 round trip; the hash check is file_sha256, tested below
    pub async fn fetch_file(
        &self,
        key: &str,
        dest: &Path,
        want_len: u64,
        want_sha256_hex: &str,
    ) -> Result<(), String> {
        if dest.exists() {
            let (digest, len) = file_sha256(dest).map_err(|e| format!("{e:#}"))?;
            if len == want_len && hex_encode(&digest) == want_sha256_hex {
                return Ok(());
            }
            return Err(format!(
                "{} already exists with different content; not overwritten",
                dest.display()
            ));
        }
        let mut part = dest.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        self.cold
            .get_to_path(key, &part)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let (digest, len) = file_sha256(&part).map_err(|e| format!("{e:#}"))?;
        if len != want_len || hex_encode(&digest) != want_sha256_hex {
            return Err(format!(
                "{key} downloaded as {len} bytes / {} — expected {want_len} / {want_sha256_hex}",
                hex_encode(&digest)
            ));
        }
        std::fs::rename(&part, dest).map_err(|e| format!("rename into {}: {e}", dest.display()))
    }

    /// Writes a small JSON body to a NEW key (`If-None-Match: *`).
    // TEST-EXEMPT: one S3 call
    pub async fn put_new_json(&self, key: &str, body: Vec<u8>) -> Result<(), String> {
        let digest: [u8; 32] = sha2::Sha256::digest(&body).into();
        self.cold
            .put_if_absent_bytes(key, body, &base64_encode(&digest), &[])
            .await
            .map_err(|e| format!("{e:#}"))
    }

    /// Reads a small object (the manifest) through a temp file under `scratch`.
    // TEST-EXEMPT: one S3 call
    pub async fn get_small(&self, key: &str, scratch: &Path) -> Result<Vec<u8>, String> {
        let path = scratch.join("rebuild-manifest.download");
        let _ = std::fs::remove_file(&path);
        self.cold
            .get_to_path(key, &path)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let body = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let _ = std::fs::remove_file(&path);
        Ok(body)
    }

    /// Length of `key`, when it exists.
    // TEST-EXEMPT: one S3 call
    pub async fn head_len(&self, key: &str) -> Result<Option<u64>, String> {
        self.cold
            .head(key)
            .await
            .map(|m| m.map(|m| m.len))
            .map_err(|e| format!("{e:#}"))
    }

    /// Every key under `prefix` with its size.
    // TEST-EXEMPT: one paginated S3 call
    pub async fn list_keys(&self, prefix: &str) -> Result<Vec<(String, u64)>, String> {
        self.cold
            .list_keys(prefix)
            .await
            .map_err(|e| format!("{e:#}"))
    }
}

// ---------------------------------------------------------------------------
// QuestDB HTTP for the restore
// ---------------------------------------------------------------------------

/// Byte offset just past the LAST complete CSV record in `buf`, scanning from
/// a record boundary (not inside quotes). `None` when `buf` holds no complete
/// record. Quote-aware: a newline inside a quoted field is not a boundary
/// (the same rule as the archiver's record counter).
#[must_use]
pub fn last_record_end(buf: &[u8]) -> Option<usize> {
    let mut in_quotes = false;
    let mut last = None;
    for (i, &b) in buf.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => last = Some(i + 1),
            _ => {}
        }
    }
    last
}

/// Number of complete CSV records in `buf` (quote-aware), `buf` starting at a
/// record boundary. A final record with no trailing newline is counted.
#[must_use]
pub fn count_records(buf: &[u8]) -> u64 {
    let mut in_quotes = false;
    let mut records: u64 = 0;
    let mut open = false;
    for &b in buf {
        match b {
            b'"' => {
                in_quotes = !in_quotes;
                open = true;
            }
            b'\n' if !in_quotes => {
                records = records.saturating_add(1);
                open = false;
            }
            _ => open = true,
        }
    }
    if open {
        records.saturating_add(1)
    } else {
        records
    }
}

/// The multipart/form-data body `/imp` expects: one `data` part holding the
/// header line followed by `records`.
#[must_use]
pub fn imp_multipart_body(table: &str, header: &[u8], records: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(header.len() + records.len() + 256);
    body.extend_from_slice(b"--");
    body.extend_from_slice(IMP_BOUNDARY.as_bytes());
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"data\"; filename=\"");
    body.extend_from_slice(table.as_bytes());
    body.extend_from_slice(b".csv\"\r\nContent-Type: text/csv\r\n\r\n");
    body.extend_from_slice(header);
    body.extend_from_slice(records);
    body.extend_from_slice(b"\r\n--");
    body.extend_from_slice(IMP_BOUNDARY.as_bytes());
    body.extend_from_slice(b"--\r\n");
    body
}

/// `(rowsImported, rowsRejected)` from an `/imp?fmt=json` answer. `Err` when
/// the status is not `OK` or a count is missing.
///
/// # Errors
/// An answer that does not prove the rows landed.
pub fn parse_imp_response(json: &str) -> Result<(u64, u64), String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("/imp answer is not JSON: {e}"))?;
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or_default();
    if status != "OK" {
        return Err(format!("/imp status {status:?}"));
    }
    let imported = v
        .get("rowsImported")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| String::from("/imp answer has no rowsImported"))?;
    let rejected = v
        .get("rowsRejected")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| String::from("/imp answer has no rowsRejected"))?;
    Ok((imported, rejected))
}

/// Rewrites `CREATE TABLE <from> (` to `CREATE TABLE <to> (` in a
/// `SHOW CREATE TABLE` answer (quoted or bare name). `None` when the DDL does
/// not start with that exact table. Used only for the restore rehearsal's
/// scratch table.
#[must_use]
pub fn rename_ddl_table(ddl: &str, from: &str, to: &str) -> Option<String> {
    let trimmed = ddl.trim_start();
    let rest = trimmed
        .get(..13)
        .filter(|p| p.eq_ignore_ascii_case("CREATE TABLE "))
        .map(|_| &trimmed[13..])?;
    for quoted in [format!("'{from}'"), format!("\"{from}\""), from.to_string()] {
        if let Some(after) = rest.strip_prefix(&quoted) {
            if after.starts_with([' ', '(']) {
                return Some(format!("CREATE TABLE '{to}'{after}"));
            }
        }
    }
    None
}

/// QuestDB over HTTP, for the box-side rebuild steps.
pub struct QuestDbHttp {
    client: reqwest::Client,
    import_client: reqwest::Client,
    exec_url: String,
    imp_url: String,
}

impl QuestDbHttp {
    /// Builds the clients for `host:port`.
    ///
    /// # Errors
    /// HTTP-client build failure.
    // TEST-EXEMPT: builds HTTP clients
    pub fn new(host: &str, http_port: u16) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(QUESTDB_STATEMENT_TIMEOUT_SECS))
                .build()
                .context("QuestDB HTTP client")?,
            import_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(QUESTDB_IMPORT_TIMEOUT_SECS))
                .build()
                .context("QuestDB import client")?,
            exec_url: format!("http://{host}:{http_port}/exec"),
            imp_url: format!("http://{host}:{http_port}/imp"),
        })
    }

    /// Runs one statement; the JSON answer on 2xx.
    // TEST-EXEMPT: one QuestDB round trip
    pub async fn exec(&self, sql: &str) -> Result<serde_json::Value, String> {
        let response = self
            .client
            .get(&self.exec_url)
            .query(&[("query", sql)])
            .send()
            .await
            .map_err(|e| format!("QuestDB request failed: {e}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("QuestDB body read failed: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "QuestDB returned {status}: {}",
                tickvault_common::sanitize::capture_rest_error_body(&body)
            ));
        }
        serde_json::from_str(&body).map_err(|e| format!("QuestDB answer is not JSON: {e}"))
    }

    /// The first cell of the first row as a string.
    // TEST-EXEMPT: one QuestDB round trip
    async fn first_cell(&self, sql: &str) -> Result<serde_json::Value, String> {
        let v = self.exec(sql).await?;
        v.get("dataset")
            .and_then(|d| d.as_array())
            .and_then(|d| d.first())
            .and_then(|r| r.as_array())
            .and_then(|r| r.first())
            .cloned()
            .ok_or_else(|| format!("no row for {sql}"))
    }

    /// `SHOW CREATE TABLE <table>`.
    // TEST-EXEMPT: one QuestDB round trip
    pub async fn show_create_table(&self, table: &str) -> Result<String, String> {
        if !crate::partition_archive::is_rebuild_safe_table_name(table) {
            return Err(format!("unsafe table name {table:?}"));
        }
        self.first_cell(&format!("SHOW CREATE TABLE {table}"))
            .await?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("SHOW CREATE TABLE {table} returned no text"))
    }

    /// `count()` of `table`.
    // TEST-EXEMPT: one QuestDB round trip
    pub async fn count(&self, table: &str) -> Result<u64, String> {
        if !crate::partition_archive::is_rebuild_safe_table_name(table) {
            return Err(format!("unsafe table name {table:?}"));
        }
        self.first_cell(&format!("SELECT count() FROM {table}"))
            .await?
            .as_u64()
            .ok_or_else(|| format!("count of {table} is not a number"))
    }

    /// Every table name QuestDB lists.
    // TEST-EXEMPT: one QuestDB round trip
    pub async fn table_names(&self) -> Result<Vec<String>, String> {
        let v = self.exec("SELECT table_name FROM tables()").await?;
        let rows = v
            .get("dataset")
            .and_then(|d| d.as_array())
            .ok_or_else(|| String::from("tables() returned no dataset"))?;
        Ok(rows
            .iter()
            .filter_map(|r| r.as_array()?.first()?.as_str().map(str::to_string))
            .collect())
    }

    /// Drops a rehearsal scratch table. Refuses any name that does not start
    /// with [`REBUILD_PROBE_TABLE_PREFIX`].
    // TEST-EXEMPT: one QuestDB round trip; the refusal is a prefix check
    pub async fn drop_probe_table(&self, table: &str) -> Result<(), String> {
        if !table.starts_with(REBUILD_PROBE_TABLE_PREFIX)
            || !crate::partition_archive::is_rebuild_safe_table_name(table)
        {
            return Err(format!(
                "refusing to drop {table:?}: not a rebuild probe table"
            ));
        }
        self.exec(&format!("DROP TABLE IF EXISTS {table}"))
            .await
            .map(|_| ())
    }

    /// Imports a gzipped `/exp` CSV into the EXISTING `table`, header first,
    /// in chunks of at most [`IMPORT_CHUNK_BYTES`] cut at record boundaries.
    /// Returns the rows QuestDB reports imported; any rejected row is an
    /// error.
    // TEST-EXEMPT: QuestDB round trips; last_record_end, imp_multipart_body and parse_imp_response are tested below
    pub async fn import_csv_gz(&self, table: &str, gz_path: &Path) -> Result<u64, String> {
        if !crate::partition_archive::is_rebuild_safe_table_name(table) {
            return Err(format!("unsafe table name {table:?}"));
        }
        let file =
            std::fs::File::open(gz_path).map_err(|e| format!("open {}: {e}", gz_path.display()))?;
        let mut decoder = flate2::read::GzDecoder::new(file);
        let mut block = vec![0_u8; READ_BLOCK_BYTES];
        let mut pending: Vec<u8> = Vec::with_capacity(IMPORT_CHUNK_BYTES + READ_BLOCK_BYTES);
        let mut header: Option<Vec<u8>> = None;
        let mut imported: u64 = 0;
        let mut eof = false;
        while !eof {
            let n = decoder
                .read(&mut block)
                .map_err(|e| format!("decompress {}: {e}", gz_path.display()))?;
            if n == 0 {
                eof = true;
            } else {
                pending.extend_from_slice(&block[..n]);
            }
            if header.is_none() {
                let Some(end) = first_record_end(&pending) else {
                    if eof {
                        return Err(String::from("export has no header line"));
                    }
                    continue;
                };
                header = Some(pending.drain(..end).collect());
            }
            let ready = if eof {
                (!pending.is_empty()).then_some(pending.len())
            } else if pending.len() >= IMPORT_CHUNK_BYTES {
                last_record_end(&pending)
            } else {
                None
            };
            if let (Some(cut), Some(header)) = (ready, header.as_ref()) {
                let chunk: Vec<u8> = pending.drain(..cut).collect();
                imported = imported.saturating_add(self.post_import(table, header, &chunk).await?);
            }
        }
        Ok(imported)
    }

    /// One `/imp` request.
    // TEST-EXEMPT: one QuestDB round trip
    async fn post_import(&self, table: &str, header: &[u8], records: &[u8]) -> Result<u64, String> {
        let boundary = IMP_BOUNDARY.as_bytes();
        if records.windows(boundary.len()).any(|w| w == boundary) {
            return Err(String::from("CSV body contains the multipart boundary"));
        }
        let expected = count_records(records);
        let response = self
            .import_client
            .post(&self.imp_url)
            .query(&[
                ("name", table),
                ("overwrite", "false"),
                ("atomicity", "abort"),
                ("forceHeader", "true"),
                ("fmt", "json"),
            ])
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={IMP_BOUNDARY}"),
            )
            .body(imp_multipart_body(table, header, records))
            .send()
            .await
            .map_err(|e| format!("/imp request failed: {e}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("/imp body read failed: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "/imp returned {status}: {}",
                tickvault_common::sanitize::capture_rest_error_body(&body)
            ));
        }
        let (imported, rejected) = parse_imp_response(&body)?;
        if rejected != 0 || imported != expected {
            return Err(format!(
                "/imp into {table}: {imported} imported, {rejected} rejected, {expected} sent"
            ));
        }
        Ok(imported)
    }
}

/// Byte offset just past the FIRST complete record (the header), quote-aware.
fn first_record_end(buf: &[u8]) -> Option<usize> {
    let mut in_quotes = false;
    for (i, &b) in buf.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => return Some(i + 1),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition_archive::{
        RebuildPartitionInfo, build_rebuild_partition_list_sql, is_rebuild_safe_table_name,
        parse_rebuild_partitions, parse_single_count, rebuild_whole_table_key,
    };
    use crate::partition_manager::rebuild_keep_tables;

    #[test]
    fn test_is_rebuild_safe_table_name_refuses_anything_injectable() {
        assert!(is_rebuild_safe_table_name("ticks"));
        assert!(is_rebuild_safe_table_name("candles_1m"));
        for bad in [
            "",
            "1ticks",
            "Ticks",
            "ticks;",
            "ticks'",
            "a b",
            "sys.telemetry",
            "t-x",
        ] {
            assert!(!is_rebuild_safe_table_name(bad), "{bad:?}");
        }
        assert!(!is_rebuild_safe_table_name(&"a".repeat(128)));
    }

    #[test]
    fn test_build_rebuild_partition_list_sql_projects_the_inventory_columns() {
        assert_eq!(
            build_rebuild_partition_list_sql("ticks"),
            "SELECT name, active, numRows, diskSize FROM table_partitions('ticks')"
        );
    }

    #[test]
    fn test_parse_rebuild_partitions_by_column_name_and_reports_bad_rows() {
        let json = r#"{"columns":[{"name":"diskSize"},{"name":"name"},{"name":"numRows"},{"name":"active"}],
            "dataset":[[100,"2026-10-21T09",5,false],[7,"2026-10-22",3,true],[1,"x'y",1,false],[1,"2026-10-20",null,false]]}"#;
        let rows = parse_rebuild_partitions(json).expect("parses");
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[0],
            Ok(RebuildPartitionInfo {
                name: "2026-10-21T09".into(),
                active: false,
                num_rows: 5,
                disk_bytes: 100
            })
        );
        assert!(matches!(&rows[1], Ok(p) if p.active));
        assert_eq!(rows[2], Err("x'y".into()));
        assert_eq!(rows[3], Err("2026-10-20".into()));
        assert!(
            parse_rebuild_partitions(r#"{"columns":[{"name":"name"}],"dataset":[]}"#).is_none()
        );
        assert!(parse_rebuild_partitions("nope").is_none());
    }

    #[test]
    fn test_parse_single_count_never_reads_missing_as_zero() {
        assert_eq!(parse_single_count(r#"{"dataset":[[42]]}"#), Some(42));
        assert_eq!(parse_single_count(r#"{"dataset":[]}"#), None);
        assert_eq!(parse_single_count(r#"{"dataset":[["42"]]}"#), None);
        assert_eq!(parse_single_count("x"), None);
    }

    #[test]
    fn test_rebuild_whole_table_key_is_content_addressed() {
        assert_eq!(
            rebuild_whole_table_key("rebuild-2026-10-22/tables/", "order_audit", "ab"),
            "rebuild-2026-10-22/tables/order_audit/ab.csv.gz"
        );
    }

    #[test]
    fn test_rebuild_keep_tables_is_the_sebi_union_and_excludes_market_data() {
        let keep = rebuild_keep_tables();
        for t in [
            "instrument_lifecycle",
            "instrument_lifecycle_audit",
            "index_constituency",
            "order_audit",
            "partition_archive_audit",
            "schema_reset_log",
        ] {
            assert!(keep.contains(&t), "{t} must be restored");
        }
        for t in [
            "ticks",
            "market_depth",
            "market_depth_book",
            "top_volume_1s",
            "feed_aux_packets",
        ] {
            assert!(
                !keep.contains(&t),
                "{t} is market data and must start empty"
            );
        }
        assert!(!keep.iter().any(|t| t.starts_with("candles_")));
        let mut sorted = keep.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, keep, "sorted and unique");
    }

    #[test]
    fn test_is_safe_rel_path_accepts_plain_paths_only() {
        assert!(is_safe_rel_path("state/daily/x.marker"));
        assert!(is_safe_rel_path("spill/ticks/ticks-2026-10-22.ilp"));
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a/./b",
            "a//b",
            "a b",
            "a/\u{e9}",
        ] {
            assert!(!is_safe_rel_path(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn test_rebuild_file_key_embeds_the_digest() {
        assert_eq!(
            rebuild_file_key("state/a.marker", "ab12"),
            "rebuild-2026-10-22/files/state/a.marker.sha-ab12"
        );
    }

    #[test]
    fn test_file_sha256_known_vector() {
        let dir = std::env::temp_dir().join(format!("tv-rebuild-sha-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("abc");
        std::fs::write(&p, b"abc").expect("write");
        let (digest, len) = file_sha256(&p).expect("hash");
        assert_eq!(len, 3);
        assert_eq!(
            hex_encode(&digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(file_sha256(&dir.join("missing")).is_err());
    }

    #[test]
    fn test_last_record_end_is_quote_aware() {
        assert_eq!(last_record_end(b"a,b\nc,d\n"), Some(8));
        assert_eq!(last_record_end(b"a,b\nc,d"), Some(4));
        assert_eq!(last_record_end(b"a,\"x\ny\"\nc"), Some(8));
        assert_eq!(last_record_end(b"a,\"x\ny"), None);
        assert_eq!(last_record_end(b""), None);
    }

    #[test]
    fn test_count_records_counts_an_unterminated_last_record() {
        assert_eq!(count_records(b""), 0);
        assert_eq!(count_records(b"a\n"), 1);
        assert_eq!(count_records(b"a\nb"), 2);
        assert_eq!(count_records(b"\"x\ny\"\n"), 1);
    }

    #[test]
    fn test_first_record_end_finds_the_header() {
        assert_eq!(first_record_end(b"ts,v\n1,2\n"), Some(5));
        assert_eq!(first_record_end(b"\"t\ns\",v\n"), Some(8));
        assert_eq!(first_record_end(b"ts,v"), None);
    }

    #[test]
    fn test_imp_multipart_body_frames_one_data_part() {
        let body = imp_multipart_body("t", b"ts,v\n", b"1,2\n");
        let text = String::from_utf8(body).expect("ascii");
        assert!(text.starts_with(&format!("--{IMP_BOUNDARY}\r\n")));
        assert!(text.contains("name=\"data\"; filename=\"t.csv\""));
        assert!(text.contains("\r\n\r\nts,v\n1,2\n\r\n"));
        assert!(text.ends_with(&format!("--{IMP_BOUNDARY}--\r\n")));
    }

    #[test]
    fn test_parse_imp_response_requires_ok_and_both_counts() {
        assert_eq!(
            parse_imp_response(r#"{"status":"OK","rowsImported":5,"rowsRejected":0}"#),
            Ok((5, 0))
        );
        assert!(parse_imp_response(r#"{"status":"ERROR","rowsImported":5}"#).is_err());
        assert!(parse_imp_response(r#"{"status":"OK","rowsRejected":0}"#).is_err());
        assert!(parse_imp_response("not json").is_err());
    }

    #[test]
    fn test_rename_ddl_table_handles_quoted_and_bare_names() {
        let ddl = "CREATE TABLE 'instrument_lifecycle' ( ts TIMESTAMP ) timestamp(ts);";
        assert_eq!(
            rename_ddl_table(
                ddl,
                "instrument_lifecycle",
                "rebuild_probe_instrument_lifecycle"
            )
            .as_deref(),
            Some(
                "CREATE TABLE 'rebuild_probe_instrument_lifecycle' ( ts TIMESTAMP ) timestamp(ts);"
            )
        );
        assert!(rename_ddl_table("CREATE TABLE x (a INT)", "x", "y").is_some());
        assert!(rename_ddl_table("CREATE TABLE xy (a INT)", "x", "y").is_none());
        assert!(rename_ddl_table("DROP TABLE x", "x", "y").is_none());
    }
}
