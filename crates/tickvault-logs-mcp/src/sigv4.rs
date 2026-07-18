//! AWS SigV4 request signing for CloudWatch Logs `FilterLogEvents` —
//! faithful port of server.py:897-991 (`_sigv4_signing_key` +
//! `build_cloudwatch_sigv4_request`) plus the pure CLI/portal parsers.
//!
//! HMAC-SHA256 is hand-rolled on top of the workspace-pinned `sha2`
//! (the only `hmac` crate in the lock is the digest-0.10-generation
//! 0.12.1, incompatible with sha2 0.11; a 15-line RFC 2104 implementation
//! avoids a new pin). Pinned by RFC 4231 vectors + Python-derived goldens.

use sha2::{Digest, Sha256};

/// RFC 2104 HMAC-SHA256.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        key_block[..32].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Python `_sigv4_signing_key` — the AWS SigV4 HMAC derivation chain.
pub fn sigv4_signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Signing timestamp — passed in so the builder stays pure/testable
/// (mirror of the Python `amz_now: datetime` parameter).
#[derive(Debug, Clone)]
pub struct AmzNow {
    /// `%Y%m%dT%H%M%SZ`
    pub amz_date: String,
    /// `%Y%m%d`
    pub date_stamp: String,
}

impl AmzNow {
    pub fn from_utc(now: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            amz_date: now.format("%Y%m%dT%H%M%SZ").to_string(),
            date_stamp: now.format("%Y%m%d").to_string(),
        }
    }
}

/// Pure builder for a SigV4-signed CloudWatch Logs `FilterLogEvents` POST.
/// Returns `(url, body_bytes, headers)` — byte-identical to the Python
/// builder for identical inputs (the request BODY is constructed manually
/// in Python dict-insertion order with compact separators, because the
/// body bytes feed the payload hash and therefore the signature).
#[allow(clippy::too_many_arguments)]
pub fn build_cloudwatch_sigv4_request(
    region: &str,
    log_group: &str,
    start_ms: i64,
    limit: i64,
    filter_pattern: Option<&str>,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    amz_now: &AmzNow,
) -> (String, Vec<u8>, Vec<(String, String)>) {
    let service = "logs";
    let host = format!("logs.{region}.amazonaws.com");
    let url = format!("https://{host}/");
    let amz_date = &amz_now.amz_date;
    let date_stamp = &amz_now.date_stamp;
    let target = "Logs_20140328.FilterLogEvents";
    let content_type = "application/x-amz-json-1.1";

    // Python: json.dumps({logGroupName, startTime, limit[, filterPattern]},
    // separators=(",", ":")) — insertion order, compact.
    let clamped_limit = limit.clamp(1, 10_000);
    let group_json = serde_json::Value::String(log_group.to_string()).to_string();
    let mut body_str = format!(
        "{{\"logGroupName\":{group_json},\"startTime\":{start_ms},\"limit\":{clamped_limit}"
    );
    if let Some(fp) = filter_pattern
        && !fp.is_empty()
    {
        let fp_json = serde_json::Value::String(fp.to_string()).to_string();
        body_str.push_str(&format!(",\"filterPattern\":{fp_json}"));
    }
    body_str.push('}');
    let body = body_str.into_bytes();
    let payload_hash = sha256_hex(&body);

    // Canonical headers sorted by lowercase name (x-amz-security-token
    // slots BEFORE x-amz-target when present).
    let mut header_pairs: Vec<(String, String)> = vec![
        ("content-type".into(), content_type.into()),
        ("host".into(), host.clone()),
        ("x-amz-content-sha256".into(), payload_hash.clone()),
        ("x-amz-date".into(), amz_date.clone()),
        ("x-amz-target".into(), target.into()),
    ];
    if let Some(token) = session_token {
        header_pairs.push(("x-amz-security-token".into(), token.to_string()));
    }
    header_pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let canon_headers: String = header_pairs
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let signed_headers: String = header_pairs
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!("POST\n/\n\n{canon_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signing_key = sigv4_signing_key(secret_key, date_stamp, region, service);
    let signature = hex(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".into(), content_type.into()),
        ("X-Amz-Date".into(), amz_date.clone()),
        ("X-Amz-Content-Sha256".into(), payload_hash),
        ("X-Amz-Target".into(), target.into()),
        ("Authorization".into(), authorization),
    ];
    if let Some(token) = session_token {
        headers.push(("X-Amz-Security-Token".into(), token.to_string()));
    }
    (url, body, headers)
}

/// Python `build_cloudwatch_filter_args` — the aws CLI argv builder.
pub fn build_cloudwatch_filter_args(
    log_group: &str,
    region: &str,
    start_ms: i64,
    limit: i64,
    filter_pattern: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "aws".into(),
        "logs".into(),
        "filter-log-events".into(),
        "--region".into(),
        region.into(),
        "--log-group-name".into(),
        log_group.into(),
        "--start-time".into(),
        start_ms.to_string(),
        "--limit".into(),
        limit.clamp(1, 10_000).to_string(),
        "--output".into(),
        "json".into(),
    ];
    if let Some(fp) = filter_pattern
        && !fp.is_empty()
    {
        args.push("--filter-pattern".into());
        args.push(fp.into());
    }
    args
}

/// Python `parse_cloudwatch_events` — `aws logs filter-log-events` JSON →
/// compact `{ts_ms, stream, message}` list (stable-sorted by ts_ms,
/// trimmed to the newest `limit`).
pub fn parse_cloudwatch_events(stdout: &str, limit: i64) -> Vec<serde_json::Value> {
    let payload: serde_json::Value = if stdout.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(stdout) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        }
    };
    let events = payload
        .as_object()
        .and_then(|o| o.get("events"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<serde_json::Value> = Vec::new();
    for ev in events {
        let Some(obj) = ev.as_object() else { continue };
        let message = obj
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_end_matches('\n')
            .to_string();
        out.push(serde_json::json!({
            "ts_ms": obj.get("timestamp").cloned().unwrap_or(serde_json::Value::Null),
            "stream": obj.get("logStreamName").cloned().unwrap_or(serde_json::Value::Null),
            "message": message,
        }));
    }
    out.sort_by(|a, b| {
        let ka = a
            .get("ts_ms")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let kb = b
            .get("ts_ms")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
    });
    tail_limit(out, limit)
}

/// Python `out[-int(limit):] if limit > 0 else out`.
pub fn tail_limit(v: Vec<serde_json::Value>, limit: i64) -> Vec<serde_json::Value> {
    if limit > 0 {
        let len = v.len();
        let keep = (limit as usize).min(len);
        v.into_iter().skip(len - keep).collect()
    } else {
        v
    }
}

/// Python `parse_portal_logs_raw` — ERR_BEGIN/ERR_END + APP_BEGIN/APP_END
/// marker-delimited journalctl tail → `[{section, message}]`.
pub fn parse_portal_logs_raw(raw: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut section: Option<&str> = None;
    for line in crate::pycompat::py_splitlines(raw) {
        let stripped = line.trim();
        match stripped {
            "ERR_BEGIN" => {
                section = Some("err");
                continue;
            }
            "APP_BEGIN" => {
                section = Some("app");
                continue;
            }
            "ERR_END" | "APP_END" => {
                section = None;
                continue;
            }
            _ => {}
        }
        let Some(sec) = section else { continue };
        out.push(serde_json::json!({
            "section": sec,
            "message": line.trim_end_matches('\n'),
        }));
    }
    out
}

/// Python `filter_and_trim_portal_events` — client-side substring filter +
/// newest-`limit` trim.
pub fn filter_and_trim_portal_events(
    events: Vec<serde_json::Value>,
    filter_pattern: Option<&str>,
    limit: i64,
) -> Vec<serde_json::Value> {
    let filtered = match filter_pattern {
        Some(fp) if !fp.is_empty() => {
            let needle = fp.trim().to_string();
            events
                .into_iter()
                .filter(|e| {
                    e.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .contains(&needle)
                })
                .collect()
        }
        _ => events,
    };
    tail_limit(filtered, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test case 2 (key "Jefe").
    #[test]
    fn hmac_sha256_rfc4231_case2() {
        let out = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&out),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// RFC 4231 test case 6 (key longer than the block size).
    #[test]
    fn hmac_sha256_rfc4231_case6_long_key() {
        let key = vec![0xaau8; 131];
        let out = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&out),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// Golden derived from the LIVE server.py `_sigv4_signing_key`
    /// (2026-07-18).
    #[test]
    fn signing_key_matches_python_golden() {
        let key = sigv4_signing_key(
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&key),
            "2c94c0cf5378ada6887f09bb697df8fc0affdb34ba1cdd5bda32b664bd55b73c"
        );
    }

    /// Goldens generated from the LIVE server.py
    /// `build_cloudwatch_sigv4_request` (2026-07-18) — url, body bytes and
    /// every header must be byte-identical, signature included.
    #[test]
    fn sigv4_request_matches_python_golden_no_token() {
        let amz = AmzNow {
            amz_date: "20260718T053000Z".into(),
            date_stamp: "20260718".into(),
        };
        let (url, body, headers) = build_cloudwatch_sigv4_request(
            "ap-south-1",
            "/tickvault/prod/app",
            1_752_800_000_000,
            100,
            Some("ERROR"),
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            &amz,
        );
        assert_eq!(url, "https://logs.ap-south-1.amazonaws.com/");
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "{\"logGroupName\":\"/tickvault/prod/app\",\"startTime\":1752800000000,\"limit\":100,\"filterPattern\":\"ERROR\"}"
        );
        let h: std::collections::BTreeMap<_, _> = headers.into_iter().collect();
        assert_eq!(h["X-Amz-Date"], "20260718T053000Z");
        assert_eq!(
            h["X-Amz-Content-Sha256"],
            "8d9dde8eb62fe6a9a325413ccfdb714530e95b9526d2483a0a1bb6e6997a367b"
        );
        assert_eq!(h["Content-Type"], "application/x-amz-json-1.1");
        assert_eq!(h["X-Amz-Target"], "Logs_20140328.FilterLogEvents");
        assert_eq!(
            h["Authorization"],
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260718/ap-south-1/logs/aws4_request, \
             SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-target, \
             Signature=336601fbfddd34ad24fc7a84f5b24c6a53f0ade26a3db9123840f2b2b060ae4c"
        );
        assert!(!h.contains_key("X-Amz-Security-Token"));
    }

    #[test]
    fn sigv4_request_matches_python_golden_with_session_token() {
        let amz = AmzNow {
            amz_date: "20300102T030405Z".into(),
            date_stamp: "20300102".into(),
        };
        let (url, body, headers) = build_cloudwatch_sigv4_request(
            "us-east-1",
            "/g",
            5,
            20_000, // clamps to 10000
            None,
            "AK",
            "SK",
            Some("TOKEN123"),
            &amz,
        );
        assert_eq!(url, "https://logs.us-east-1.amazonaws.com/");
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "{\"logGroupName\":\"/g\",\"startTime\":5,\"limit\":10000}"
        );
        let h: std::collections::BTreeMap<_, _> = headers.into_iter().collect();
        assert_eq!(h["X-Amz-Security-Token"], "TOKEN123");
        assert_eq!(
            h["Authorization"],
            "AWS4-HMAC-SHA256 Credential=AK/20300102/us-east-1/logs/aws4_request, \
             SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token;x-amz-target, \
             Signature=a525b3da1a9fdd6e19c5e7698be138596a1601297687faa21058bdd473c544a2"
        );
    }

    #[test]
    fn filter_args_shape() {
        let args = build_cloudwatch_filter_args("/g", "ap-south-1", 123, 20_000, Some("ERROR"));
        assert_eq!(
            args,
            vec![
                "aws",
                "logs",
                "filter-log-events",
                "--region",
                "ap-south-1",
                "--log-group-name",
                "/g",
                "--start-time",
                "123",
                "--limit",
                "10000",
                "--output",
                "json",
                "--filter-pattern",
                "ERROR",
            ]
        );
        let no_filter = build_cloudwatch_filter_args("/g", "r", 1, 0, None);
        assert!(!no_filter.contains(&"--filter-pattern".to_string()));
        assert!(no_filter.contains(&"1".to_string())); // limit clamps up to 1
    }

    #[test]
    fn parse_events_sorts_and_trims() {
        let stdout = r#"{"events":[
            {"timestamp": 300, "logStreamName": "s1", "message": "c\n"},
            {"timestamp": 100, "logStreamName": "s2", "message": "a"},
            {"timestamp": 200, "logStreamName": "s3", "message": "b"},
            "not-a-dict"
        ]}"#;
        let out = parse_cloudwatch_events(stdout, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["message"], "b");
        assert_eq!(out[1]["message"], "c"); // trailing \n stripped
        assert!(parse_cloudwatch_events("not json", 5).is_empty());
        assert!(parse_cloudwatch_events("", 5).is_empty());
    }

    #[test]
    fn portal_raw_parser_sections() {
        let raw = "junk\nERR_BEGIN\ne1\ne2\nERR_END\nbetween\nAPP_BEGIN\na1\nAPP_END\n";
        let out = parse_portal_logs_raw(raw);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["section"], "err");
        assert_eq!(out[0]["message"], "e1");
        assert_eq!(out[2]["section"], "app");
        assert_eq!(out[2]["message"], "a1");
    }

    #[test]
    fn portal_filter_and_trim() {
        let events =
            parse_portal_logs_raw("ERR_BEGIN\nfoo ERROR one\nplain\nfoo ERROR two\nERR_END");
        let out = filter_and_trim_portal_events(events, Some("ERROR"), 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["message"], "foo ERROR two");
    }
}
