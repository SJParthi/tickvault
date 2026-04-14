# Static IP Technical Design — Complete Architecture Reference

> **Purpose:** Single source of truth for ALL static IP verification
> technical decisions, architecture, and test strategy.
>
> **Last updated:** 2026-03-10

---

## 1. SYSTEM ARCHITECTURE

### 1.1 Verification Pipeline

```
                    BOOT SEQUENCE
                    =============

AWS SSM (/tickvault/<env>/network/static-ip)
    │
    ▼
fetch_expected_ip_from_ssm()
    │ → SecretString → expose_secret() → trim()
    ▼
validate_ipv4_format(expected_ip)
    │ → Ipv4Addr::parse()
    ▼
detect_public_ip()
    │
    ├── Primary: HTTPS GET checkip.amazonaws.com
    │   ├── Attempt 1 (timeout 10s)
    │   ├── Attempt 2 (backoff 1s)
    │   └── Attempt 3 (backoff 2s)
    │
    └── Fallback: HTTPS GET api.ipify.org
        ├── Attempt 1 (timeout 10s)
        ├── Attempt 2 (backoff 1s)
        └── Attempt 3 (backoff 2s)
    │
    ▼
validate_ipv4_format(actual_ip)
    │
    ▼
Compare: expected_ip == actual_ip
    │
    ├── Match → IpVerificationResult { verified_ip }
    └── Mismatch → ApplicationError::IpVerificationFailed
```

### 1.2 Boot Sequence Position

```
Config → Observability → Logging → Notification
→ ★ IP Verification (verify_public_ip) ★
→ Auth → QuestDB → Universe → WebSocket → ...
```

Runs parallel with notification init in fast boot path.

### 1.3 Key Files

| File | Purpose |
|------|---------|
| `core/network/ip_verifier.rs` | All verification logic (449 lines) |
| `core/network/mod.rs` | Module declaration |
| `common/constants.rs` | IP check URLs, timeouts, retry counts |
| `common/error.rs` | `IpVerificationFailed` variant |
| `core/notification/events.rs` | IP success/failure notification events |
| `app/main.rs` | Boot integration (lines 313-333, 610-628) |

---

## 2. DATA STRUCTURES

### 2.1 IpVerificationResult

```rust
#[derive(Debug, Clone)]
pub struct IpVerificationResult {
    pub verified_ip: String,
}
```

**Known issue:** `verified_ip` is a plain String with `Debug` derive.
If logged via `{:?}`, the full IP leaks. Should use masking.

### 2.2 Error Type

```rust
// In common/error.rs:
IpVerificationFailed { reason: String }
```

Reason contains masked IPs and diagnostic information.

---

## 3. IP DETECTION LOGIC

### 3.1 Primary + Fallback Pattern

```rust
async fn detect_public_ip() -> Result<String, String> {
    // Primary: 3 retries with exponential backoff
    for attempt in 1..=3 {
        match fetch_ip_from_url(PRIMARY_URL, timeout).await {
            Ok(ip) => return Ok(ip),
            Err(_) => sleep(2^(attempt-1) seconds)
        }
    }
    // Fallback: 3 retries with exponential backoff
    for attempt in 1..=3 {
        match fetch_ip_from_url(FALLBACK_URL, timeout).await {
            Ok(ip) => return Ok(ip),
            Err(_) => sleep(2^(attempt-1) seconds)
        }
    }
    Err("all attempts exhausted")
}
```

### 3.2 Single URL Fetch

```rust
async fn fetch_ip_from_url(url: &str, timeout: Duration) -> Result<String, String> {
    let client = reqwest::Client::builder().timeout(timeout).build()?;
    let response = client.get(url).send().await?;
    let body = response.text().await?;
    let ip = body.trim().to_string();
    validate_ipv4_format(&ip)?;
    Ok(ip)
}
```

**Known issue:** Creates a new `reqwest::Client` per attempt (6 builds max).
Should reuse a single client.

### 3.3 IPv4 Validation

```rust
fn validate_ipv4_format(ip: &str) -> Result<(), String> {
    ip.parse::<Ipv4Addr>().map(|_| ()).map_err(|err| ...)
}
```

Rejects: IPv6, hostnames, ports, invalid octets, whitespace.

### 3.4 IP Masking

```rust
fn mask_ip(ip: &str) -> String {
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() == 4 {
        format!("{}.{}.XXX.XX", parts[0], parts[1])
    } else {
        "XXX.XXX.XXX.XXX".to_string()
    }
}
```

---

## 4. CONSTANTS

```rust
SSM_NETWORK_SERVICE = "network"
STATIC_IP_SECRET = "static-ip"
PUBLIC_IP_CHECK_PRIMARY_URL = "https://checkip.amazonaws.com"
PUBLIC_IP_CHECK_FALLBACK_URL = "https://api.ipify.org"
PUBLIC_IP_CHECK_TIMEOUT_SECS = 10
PUBLIC_IP_CHECK_MAX_RETRIES = 3
```

SSM path: `/tickvault/{env}/network/static-ip`

---

## 5. NOTIFICATION EVENTS

| Event | Severity | When |
|-------|----------|------|
| `IpVerificationSuccess { verified_ip }` | Low | IP matches SSM |
| `IpVerificationFailed { reason }` | Critical | Any failure |

Critical events → Telegram alert immediately.

---

## 6. TEST STRATEGY

### 6.1 Current Tests (19)

| Category | Count | Coverage |
|----------|-------|----------|
| IPv4 validation | 4 | Valid, invalid, whitespace, newline |
| IP masking | 5 | Standard, private, invalid, empty, partial |
| SSM paths | 2 | Dev and prod path construction |
| URL validation | 2 | HTTPS enforcement for both URLs |
| Constant validation | 2 | Timeout and retry range checks |
| Result type | 2 | Debug and Clone |
| Integration (real AWS) | 2 | SSM fetch + full verification |

### 6.2 Gap Tests to Add (16 gaps)

**CRITICAL (C1-C5):**

| ID | Test | What It Validates |
|----|------|-------------------|
| C1 | `test_detect_public_ip_with_mock_server` | Primary + fallback HTTP flow offline |
| C2 | `test_primary_to_fallback_cascade` | Primary fails 3x → fallback succeeds |
| C3 | `test_ip_mismatch_detection` | expected ≠ actual → correct error |
| C4 | `test_empty_ssm_response_rejected` | SSM returns "" → IpVerificationFailed |
| C5 | `test_ip_verification_result_redaction` | verified_ip not leaked in logs |

**HIGH (H1-H5):**

| ID | Test | What It Validates |
|----|------|-------------------|
| H1 | `test_retry_backoff_attempt_count` | Exactly 3 attempts per URL |
| H2 | `test_ipv6_address_rejected_with_clear_error` | "::1" → helpful error message |
| H3 | `test_http_client_reuse` | Single client across retries |
| H4 | `test_non_ip_html_response_rejected` | HTML/captive portal → format check fails |
| H5 | `test_fetch_timeout_fires` | 10s timeout actually triggers |

**MEDIUM (M1-M6):**

| ID | Test | What It Validates |
|----|------|-------------------|
| M1 | `test_ssm_ip_with_whitespace_trimmed` | " 1.2.3.4 " → "1.2.3.4" |
| M2 | `test_mask_ip_with_ipv6_format` | "::1" → "XXX.XXX.XXX.XXX" |
| M3 | `test_primary_and_fallback_both_fail` | Clear error message listing both URLs |
| M4 | `test_no_runtime_re_verification` | Document: IP only checked at boot |
| M5 | `test_concurrent_verify_calls` | Two simultaneous verify_public_ip() calls |
| M6 | `test_mask_ip_preserves_network_info` | First 2 octets visible for debugging |

---

## 7. PERFORMANCE

| Operation | Time | Notes |
|-----------|------|-------|
| SSM fetch | 200-500ms | Network to ap-south-1 |
| IP detection (success) | 200-500ms | Single HTTPS request |
| IP detection (primary fail) | 7s | 3 retries with backoff |
| IP detection (all fail) | 14s | Primary + fallback exhausted |
| IPv4 validation | <1µs | String parse |
| Total (happy path) | ~500ms-1s | SSM + 1 HTTP |
| Total (worst case) | ~15s | SSM + all retries |

---

## 8. CONFIGURATION

No application-level config for IP verification. All parameters are constants:

```rust
// constants.rs
PUBLIC_IP_CHECK_PRIMARY_URL = "https://checkip.amazonaws.com"
PUBLIC_IP_CHECK_FALLBACK_URL = "https://api.ipify.org"
PUBLIC_IP_CHECK_TIMEOUT_SECS = 10
PUBLIC_IP_CHECK_MAX_RETRIES = 3
```

SSM path constructed from environment:
```
/tickvault/{ENVIRONMENT}/network/static-ip
```

---

## 9. TEST COVERAGE AUDIT (2026-03-10)

### 9.1 Validated Edge Cases

All edge cases in section 4 have matching `#[test]` functions:

| Edge Case | Test Function | Status |
|-----------|---------------|--------|
| HTTP non-200 status | `test_fetch_ip_url_non_200_status` | Verified |
| HTTP timeout (server hangs) | `test_fetch_ip_url_timeout` | Verified |
| Exponential backoff delays | `test_exponential_backoff_delay_calculation` | Verified (1s, 2s, 4s) |
| SSM constants match design | `test_ssm_constants_for_static_ip` | Pinned |
| Whitespace in IP response | `test_fetch_ip_url_trims_whitespace` | Verified |
| IP verification is ApplicationError | `test_ip_verification_error_is_application_error` | Verified |
| mask_ip preserves network octets | `test_mask_preserves_network_octets` | Verified |
| Captive portal HTML response | `test_captive_portal_html_rejected` | Verified |
| IPv6 address rejected | `test_ipv6_address_fails_ipv4_validation` | Verified |
| Partial IP response | `test_partial_ip_response_rejected` | Verified |

### 9.2 Known Limitations

- **`IpVerificationResult` Debug trait:** Derives `Debug` which prints unmasked IP when formatted as `{:?}`. Low risk (IP is whitelisted, not a secret), but inconsistent with auth module's `Secret<T>` pattern. `mask_ip()` is used in all intentional log statements.

### 9.3 Mechanical Enforcement

- **Test count ratchet:** Cannot decrease
- **Pub fn guard:** New pub fns must have matching tests
