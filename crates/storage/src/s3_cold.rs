//! The cold S3 bucket (`tv-<env>-cold`): client, bucket resolution, the
//! never-overwrite PutObject, the checksum-reading HeadObject, and the content
//! identity helpers (SHA-256 as base64 for the wire and hex for audit rows).
//!
//! Extracted 2026-10-02 (plan item 45e-1) from `partition_archive`, which was
//! its only user, so the raw WAL segment uploader (`raw_frame_upload`) uses the
//! same code instead of a second copy that could drift. The move changes no
//! behaviour: `partition_archive` calls these exactly as it called its private
//! versions.
//!
//! Cold path only — every call here is a network round trip or a pure helper
//! on a fixed-size digest. Nothing here runs on the frame drain or the WAL
//! writer thread.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

// ---------------------------------------------------------------------------
// Content identity (pure — review round 2)
// ---------------------------------------------------------------------------

/// Standard base64 (RFC 4648, with padding) — exactly what S3's
/// `x-amz-checksum-sha256` header carries. Hand-rolled for a fixed 32-byte
/// digest rather than adding a new supply-chain root; pinned by a
/// known-vector unit test below.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Lowercase hex of a digest (audit-row provenance column).
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// `Write` adapter folding every byte into a SHA-256 digest on its way to
/// the inner writer — the gzip stream is hashed AS IT IS WRITTEN, so the
/// content-identity digest covers exactly the bytes on disk (and therefore
/// exactly the bytes S3 receives) with no second file read.
pub(crate) struct Sha256Writer<W: Write> {
    inner: W,
    hasher: sha2::Sha256,
}

impl<W: Write> Sha256Writer<W> {
    pub(crate) fn new(inner: W) -> Self {
        use sha2::Digest as _;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
        }
    }

    /// Consumes the adapter: `(inner writer, 32-byte digest)`.
    pub(crate) fn finish(self) -> (W, [u8; 32]) {
        use sha2::Digest as _;
        (self.inner, self.hasher.finalize().into())
    }
}

impl<W: Write> Write for Sha256Writer<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// Bucket resolution (pure + thin env shim)
// ---------------------------------------------------------------------------

/// Resolves the archive bucket (review round 1, F1b — FAIL-CLOSED): an
/// explicit non-empty config value ALWAYS wins; an empty config derives the
/// house cold bucket `tv-<env>-cold` (terraform `tv-${environment}-cold`)
/// ONLY when the environment was EXPLICITLY provided by an env var. No
/// explicit environment → `None` → the caller SKIPS archival entirely for
/// the run. There is deliberately NO `"prod"` default here: a dev box /
/// worktree / CI shell without env vars must never write (or overwrite)
/// objects in the prod cold bucket.
pub(crate) fn resolve_archive_bucket(
    configured: &str,
    environment: Option<&str>,
) -> Option<String> {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    environment.map(|env| format!("tv-{env}-cold"))
}

/// Environment-name resolution from raw env-var reads (pure — testable
/// without process-global env mutation). Precedence: `TV_ENVIRONMENT` →
/// `ENVIRONMENT`. `None` when neither is set (F1b fail-closed — the
/// SSM-path `"prod"` fallback in `secret_manager::resolve_environment` is
/// DELIBERATELY not mirrored here). A value with characters outside
/// `[a-zA-Z0-9-]` also resolves `None` (bucket-name safety, fail-closed).
pub(crate) fn resolve_environment_from(
    tv_environment: Option<&str>,
    environment: Option<&str>,
) -> Option<String> {
    let candidate = tv_environment
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| environment.map(str::trim).filter(|s| !s.is_empty()))?;
    if candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// Reads the explicitly-set runtime environment name (`TV_ENVIRONMENT` →
/// `ENVIRONMENT`); `None` when neither env var is set — archival is then
/// skipped for the run (F1b fail-closed).
// TEST-EXEMPT: two env reads; the decision is resolve_environment_from, tested directly
pub(crate) fn runtime_environment() -> Option<String> {
    resolve_environment_from(
        std::env::var("TV_ENVIRONMENT").ok().as_deref(),
        std::env::var("ENVIRONMENT").ok().as_deref(),
    )
}

/// S3 `HeadObject` metadata relevant to the reuse/conflict decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3ObjectMeta {
    /// ContentLength.
    pub len: u64,
    /// `x-amz-checksum-sha256` attribute (base64), when the object carries
    /// one. Our uploads ALWAYS set it; a foreign/legacy object may not.
    pub checksum_sha256_b64: Option<String>,
}

/// What already sits at an S3 key, compared with the bytes about to go there.
///
/// Content identity is length AND the stored SHA-256 checksum attribute. An
/// object with no checksum attribute (foreign or legacy) is `Different`:
/// nothing proves it holds these bytes, and the never-overwrite rule means it
/// is never written over either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingObject {
    /// Nothing at the key.
    Absent,
    /// Same length and same SHA-256: the upload already happened (a crash
    /// between the upload and its local record, or a re-run).
    Identical,
    /// Something else is there. Never overwritten.
    Different,
}

/// Classifies `meta` (the HeadObject result for a key) against the local
/// bytes' length and base64 SHA-256. Pure, O(1).
#[must_use]
pub(crate) fn classify_existing(
    meta: Option<&S3ObjectMeta>,
    len: u64,
    sha256_b64: &str,
) -> ExistingObject {
    match meta {
        None => ExistingObject::Absent,
        Some(m) if m.len == len && m.checksum_sha256_b64.as_deref() == Some(sha256_b64) => {
            ExistingObject::Identical
        }
        Some(_) => ExistingObject::Different,
    }
}

/// An S3 client bound to the cold bucket.
pub struct S3Cold {
    s3: aws_sdk_s3::Client,
    bucket: String,
}

impl S3Cold {
    /// Wraps an existing client (the partition archiver's stub tests inject
    /// one pointed at a local server).
    #[must_use]
    // TEST-EXEMPT: struct constructor around an AWS SDK client
    pub(crate) fn from_client(s3: aws_sdk_s3::Client, bucket: String) -> Self {
        Self { s3, bucket }
    }

    /// Builds the client from the default AWS credential chain (the
    /// instance role on the box) for an already-resolved bucket.
    pub(crate) async fn connect(bucket: String) -> Self {
        let aws_conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        Self::from_client(aws_sdk_s3::Client::new(&aws_conf), bucket)
    }

    /// Resolves the cold bucket ([`resolve_archive_bucket`]: an explicit
    /// config value, else `tv-<env>-cold` only when `TV_ENVIRONMENT` /
    /// `ENVIRONMENT` is set) and connects. `None` when no bucket resolves —
    /// fail closed, never a guessed prod bucket.
    // TEST-EXEMPT: loads the live AWS config chain; the resolution it
    // delegates to is unit-tested below, the client calls by the stub-server
    // tests in `partition_archive`.
    pub async fn load(configured_bucket: &str) -> Option<Self> {
        let bucket = resolve_archive_bucket(configured_bucket, runtime_environment().as_deref())?;
        Some(Self::connect(bucket).await)
    }

    /// The bucket this client writes to.
    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Conditional create from a file (`If-None-Match: "*"`), with the
    /// precomputed SHA-256 as `x-amz-checksum-sha256`: S3 rejects a body that
    /// does not match it, and stores it as the object's content identity. A
    /// lost race against another writer is a loud 412, never an overwrite.
    // TEST-EXEMPT: one AWS SDK call; the upload decisions around it are tested through the ColdObjectStore fake
    pub(crate) async fn put_if_absent_path(
        &self,
        path: &Path,
        key: &str,
        sha256_b64: &str,
    ) -> Result<()> {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(path)
            .await
            .context("open temp file for S3 upload")?;
        self.s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_none_match("*")
            .checksum_sha256(sha256_b64)
            .body(body)
            .send()
            .await
            .context("S3 conditional PutObject (If-None-Match + checksum_sha256) failed")?;
        Ok(())
    }

    /// [`Self::put_if_absent_path`] for an in-memory body, with user metadata.
    // TEST-EXEMPT: one AWS SDK call; the upload decisions around it are tested through the ColdObjectStore fake
    pub(crate) async fn put_if_absent_bytes(
        &self,
        key: &str,
        body: Vec<u8>,
        sha256_b64: &str,
        metadata: &[(&'static str, String)],
    ) -> Result<()> {
        let mut req = self
            .s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_none_match("*")
            .checksum_sha256(sha256_b64)
            .body(aws_sdk_s3::primitives::ByteStream::from(body));
        for (k, v) in metadata {
            req = req.metadata(*k, v.as_str());
        }
        req.send()
            .await
            .map_err(|e| anyhow::anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))
            .context("S3 conditional PutObject (If-None-Match + checksum_sha256) failed")?;
        Ok(())
    }

    /// HeadObject with `ChecksumMode::Enabled`: the object's length and its
    /// stored SHA-256 (base64). `Ok(None)` only when S3 says the key does not
    /// exist; any other failure is `Err`, so a caller can tell "absent" from
    /// "could not ask".
    pub(crate) async fn head(&self, key: &str) -> Result<Option<S3ObjectMeta>> {
        let head = match self
            .s3
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await
        {
            Ok(head) => head,
            Err(err) => {
                if err.as_service_error().is_some_and(|e| e.is_not_found()) {
                    return Ok(None);
                }
                anyhow::bail!(
                    "S3 HeadObject failed: {}",
                    aws_sdk_s3::error::DisplayErrorContext(&err)
                );
            }
        };
        let len = head
            .content_length()
            .and_then(|n| u64::try_from(n).ok())
            .context("S3 HeadObject returned no content length")?;
        Ok(Some(S3ObjectMeta {
            len,
            checksum_sha256_b64: head
                .checksum_sha256()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string),
        }))
    }

    /// DeleteObject. Used only by the partition archiver for an object the
    /// same run uploaded (see `partition_archive`). The box role has no
    /// `s3:DeleteObject` grant, so on prod this fails and the object stays.
    pub(crate) async fn delete(&self, key: &str) -> Result<()> {
        self.s3
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .context("S3 DeleteObject for a stale post-mismatch archive object")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_encode_known_vectors() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
    }

    #[test]
    fn classify_existing_needs_length_and_checksum_to_match() {
        let meta = |len, sum: Option<&str>| S3ObjectMeta {
            len,
            checksum_sha256_b64: sum.map(ToString::to_string),
        };
        assert_eq!(classify_existing(None, 10, "abc"), ExistingObject::Absent);
        assert_eq!(
            classify_existing(Some(&meta(10, Some("abc"))), 10, "abc"),
            ExistingObject::Identical
        );
        assert_eq!(
            classify_existing(Some(&meta(11, Some("abc"))), 10, "abc"),
            ExistingObject::Different,
            "a length mismatch is a different object"
        );
        assert_eq!(
            classify_existing(Some(&meta(10, Some("abd"))), 10, "abc"),
            ExistingObject::Different,
            "a checksum mismatch is a different object"
        );
        assert_eq!(
            classify_existing(Some(&meta(10, None)), 10, "abc"),
            ExistingObject::Different,
            "an object without a checksum proves nothing and is never reused"
        );
    }

    // ---- content identity: base64 / hex / Sha256Writer (review round 2) ----

    #[test]
    fn test_base64_encode_known_vectors() {
        // RFC 4648 vectors + the well-known sha256("abc") digest in the
        // exact wire format S3's x-amz-checksum-sha256 carries.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        use sha2::Digest as _;
        let digest: [u8; 32] = sha2::Sha256::digest(b"abc").into();
        assert_eq!(
            base64_encode(&digest),
            "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=",
            "sha256(\"abc\") base64 known vector"
        );
        assert_eq!(
            hex_encode(&digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256(\"abc\") hex known vector"
        );
    }

    #[test]
    fn test_sha256_writer_digest_matches_oneshot_and_passes_bytes_through() {
        use sha2::Digest as _;
        let payload = b"the compressed archive bytes, split across writes";
        let mut w = Sha256Writer::new(Vec::new());
        w.write_all(&payload[..7]).expect("write 1");
        w.write_all(&payload[7..]).expect("write 2");
        w.flush().expect("flush");
        let (inner, digest) = w.finish();
        assert_eq!(
            inner, payload,
            "every byte must pass through to the inner writer"
        );
        let oneshot: [u8; 32] = sha2::Sha256::digest(payload).into();
        assert_eq!(digest, oneshot, "streamed digest == one-shot digest");
    }

    // ---- bucket / environment resolution ------------------------------------

    #[test]
    fn test_resolve_archive_bucket_explicit_wins() {
        assert_eq!(
            resolve_archive_bucket("my-bucket", Some("prod")).as_deref(),
            Some("my-bucket")
        );
        assert_eq!(
            resolve_archive_bucket("  my-bucket  ", None).as_deref(),
            Some("my-bucket"),
            "explicit config needs no env var"
        );
    }

    #[test]
    fn test_resolve_archive_bucket_derives_cold_bucket_only_with_explicit_env() {
        assert_eq!(
            resolve_archive_bucket("", Some("prod")).as_deref(),
            Some("tv-prod-cold")
        );
        assert_eq!(
            resolve_archive_bucket("   ", Some("staging")).as_deref(),
            Some("tv-staging-cold")
        );
        // F1b fail-closed: no explicit environment → NO bucket → archival
        // skipped. A dev box can never default onto the prod cold bucket.
        assert_eq!(resolve_archive_bucket("", None), None);
        assert_eq!(resolve_archive_bucket("   ", None), None);
    }

    #[test]
    fn test_resolve_environment_from_precedence_and_fail_closed() {
        assert_eq!(
            resolve_environment_from(Some("prod"), None).as_deref(),
            Some("prod")
        );
        assert_eq!(
            resolve_environment_from(Some("staging"), Some("prod")).as_deref(),
            Some("staging")
        );
        assert_eq!(
            resolve_environment_from(None, Some("dev-1")).as_deref(),
            Some("dev-1")
        );
        // F1b: NO default — unset env vars resolve to None (skip archival).
        assert_eq!(resolve_environment_from(None, None), None);
        assert_eq!(resolve_environment_from(Some(""), Some("  ")), None);
        // Bucket-hostile values also resolve None (fail-closed).
        assert_eq!(resolve_environment_from(Some("../etc"), None), None);
        assert_eq!(resolve_environment_from(Some("a b"), None), None);
    }
}
