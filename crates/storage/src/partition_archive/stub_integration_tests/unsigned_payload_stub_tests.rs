//! Unsigned-payload PutObject (2026-11-16) against the stub S3: the request
//! carries `x-amz-content-sha256: UNSIGNED-PAYLOAD` (the SDK did not hash the
//! body inside `send()`, on a runtime worker) AND the precomputed
//! `x-amz-checksum-sha256` S3 checks the body against, plus the
//! never-overwrite `If-None-Match: *`, and is still SigV4-signed.

use super::*;
use crate::s3_cold::S3Cold;

/// A client pointed at the stub with the PRODUCTION checksum setting (the
/// SDK default, not the `WhenRequired` the archiver stub tests use), so the
/// headers asserted here are the ones the box sends.
fn stub_cold(s3_url: &str) -> S3Cold {
    let creds = aws_sdk_s3::config::Credentials::new("test", "test", None, None, "stub");
    let conf = aws_sdk_s3::config::Builder::new()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new("ap-south-1"))
        .endpoint_url(s3_url)
        .credentials_provider(creds)
        .force_path_style(true)
        .build();
    S3Cold::from_client(
        aws_sdk_s3::Client::from_conf(conf),
        "tv-test-cold".to_string(),
    )
}

fn sha256_b64(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest: [u8; 32] = sha2::Sha256::digest(bytes).into();
    base64_encode(&digest)
}

/// The one PUT the stub saw.
fn only_put(log: &Mutex<Vec<SeenRequest>>) -> SeenRequest {
    let puts: Vec<SeenRequest> = log
        .lock()
        .expect("s3 log")
        .iter()
        .filter(|r| r.method == "PUT")
        .cloned()
        .collect();
    assert_eq!(puts.len(), 1, "exactly one PutObject: {puts:?}");
    puts.into_iter().next().expect("one put")
}

/// The headers every cold PutObject must carry.
fn assert_unsigned_with_checksum(put: &SeenRequest, sha_b64: &str, len: usize) {
    assert_eq!(
        put.header("x-amz-content-sha256"),
        Some("UNSIGNED-PAYLOAD"),
        "the body must not be hashed for signing: {put:?}"
    );
    assert_eq!(
        put.header("x-amz-checksum-sha256"),
        Some(sha_b64),
        "S3 must still get the precomputed checksum: {put:?}"
    );
    assert_eq!(put.header("if-none-match"), Some("*"));
    assert_eq!(
        put.header("content-length"),
        Some(len.to_string().as_str()),
        "a plain body (no aws-chunked trailer): {put:?}"
    );
    assert!(
        put.header("authorization")
            .is_some_and(|a| a.starts_with("AWS4-HMAC-SHA256 ")),
        "the request itself is still SigV4-signed: {put:?}"
    );
    assert!(
        put.header("x-amz-trailer").is_none(),
        "no trailer checksum: {put:?}"
    );
}

#[tokio::test]
async fn test_put_if_absent_bytes_sends_unsigned_payload_with_sha256_checksum() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (s3_url, log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;
    let cold = stub_cold(&s3_url);
    // Bigger than one 64 KiB read, so the stub reads a multi-chunk body.
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let sha = sha256_b64(&body);
    cold.put_if_absent_bytes(
        "raw-frames/2026-11-16/seg.wal.gz",
        body.clone(),
        &sha,
        &[("tv-note", "x".to_string())],
    )
    .await
    .expect("put");
    let put = only_put(&log);
    assert_unsigned_with_checksum(&put, &sha, body.len());
    assert_eq!(put.header("x-amz-meta-tv-note"), Some("x"));
    // The read-back verification the callers do still sees the checksum.
    let meta = cold
        .head("raw-frames/2026-11-16/seg.wal.gz")
        .await
        .expect("head")
        .expect("present");
    assert_eq!(meta.len, body.len() as u64);
    assert_eq!(meta.checksum_sha256_b64.as_deref(), Some(sha.as_str()));
}

#[tokio::test]
async fn test_put_if_absent_path_sends_unsigned_payload_with_sha256_checksum() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (s3_url, log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;
    let cold = stub_cold(&s3_url);
    let dir = unique_temp_dir();
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("part.csv.gz");
    let body = b"ts,security_id\n2026-11-16T09:15:00Z,13\n".to_vec();
    std::fs::write(&path, &body).expect("write");
    let sha = sha256_b64(&body);
    cold.put_if_absent_path(&path, "questdb-partitions/ticks/x.csv.gz", &sha)
        .await
        .expect("put");
    let put = only_put(&log);
    assert_unsigned_with_checksum(&put, &sha, body.len());
    let _ = std::fs::remove_dir_all(&dir);
}
