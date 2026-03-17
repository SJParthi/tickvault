# Authentication Technical Design — Complete Architecture Reference

> **Purpose:** Single source of truth for ALL authentication technical
> decisions, architecture, and enforcement mechanisms.
>
> **Last updated:** 2026-03-10

---

## 1. SYSTEM ARCHITECTURE

### 1.1 Data Flow

```
                    BOOT SEQUENCE
                    =============

AWS SSM ──► fetch_dhan_credentials() ──► DhanCredentials
                                              │
                                              ├──► generate_totp_code()
                                              │         │
                                              │         ▼
                                              ├──► POST /app/generateAccessToken
                                              │         │
                                              │         ▼
                                              │    DhanGenerateTokenResponse
                                              │         │
                                              │         ▼
                                              │    TokenState::from_generate_response()
                                              │         │
                                              │         ▼
                                              └──► ArcSwap.store(Some(token))
                                                        │
                                                        ▼
                                              spawn_renewal_task()
                                                        │
                                                        ▼
                                              renewal_loop() (background)

                    HOT PATH (per request)
                    ======================

token_handle.load() ──► Arc<Option<TokenState>> ──► expose_secret()
       │                                                    │
       │ O(1) atomic pointer read                           │
       │ zero allocation                                    ▼
       │                                           HTTP header: access-token
       │                                           WebSocket URL: ?access_token=
       └──────────────────────────────────────────────────────
```

### 1.2 Boot Sequence Position

```
CryptoProvider → Config → Observability → Logging → Notification
→ IP Verification → ★ Auth (TokenManager::initialize) ★
→ QuestDB → Universe → WebSocket → TickProcessor → OrderUpdateWS
→ API → TokenRenewal → Shutdown
```

### 1.3 Crate Ownership

| Crate | What it owns |
|-------|-------------|
| `common` | Config types (`TokenConfig`, `DhanConfig`, `NetworkConfig`), error variants, constants |
| `core` | All auth logic: `secret_manager`, `totp_generator`, `token_manager`, `token_cache`, `types` |
| `trading` | OMS API client (consumes token via header) |
| `app` | Boot orchestration, two-phase boot logic |

### 1.4 Key Files

| File | Purpose | Lines |
|------|---------|-------|
| `core/auth/mod.rs` | Module root, re-exports | 45 |
| `core/auth/types.rs` | TokenState, credentials, API types | 1025 |
| `core/auth/token_manager.rs` | Full lifecycle: acquire, renew, retry, circuit breaker | 1400+ |
| `core/auth/token_cache.rs` | Fast crash-recovery disk cache | 633 |
| `core/auth/secret_manager.rs` | SSM credential retrieval | 628 |
| `core/auth/totp_generator.rs` | TOTP code generation | 223 |
| `common/config.rs` | TokenConfig, DhanConfig, NetworkConfig | — |
| `common/constants.rs` | Auth constants (40+) | — |
| `common/error.rs` | Auth error variants | — |

---

## 2. TOKEN HANDLE (HOT PATH)

### 2.1 Type Definition

```rust
pub type TokenHandle = Arc<ArcSwap<Option<TokenState>>>;
```

### 2.2 Read Path (O(1), Zero Allocation)

```rust
// Every WebSocket frame, every REST call:
let guard = token_handle.load();  // atomic pointer read
let token = guard.as_ref().as_ref()?;
let header = token.access_token().expose_secret();
```

### 2.3 Write Path (O(1), Atomic Swap)

```rust
// On renewal (background task, every 23h):
let new_token = TokenState::from_generate_response(&response);
token_handle.store(Arc::new(Some(new_token)));
// Old token dropped → SecretString → zeroize
```

### 2.4 Concurrent Safety

- 10+ reader threads + 1 writer thread tested
- No locks, no contention
- Last-write-wins semantics (correct for token renewal)

---

## 3. RETRY & CIRCUIT BREAKER

### 3.1 Auth Retry (Initial Acquisition)

```
TOTP failure → wait 30s → retry (up to 2 times)
HTTP failure → exponential backoff 100ms → 300s
Permanent error → fail fast, alert, halt
```

### 3.2 Renewal Retry

```
renewToken fails → try generateAccessToken (fallback)
All retries fail → circuit breaker
```

### 3.3 Circuit Breaker

| Parameter | Value | Constant |
|-----------|-------|----------|
| Trip threshold | 3 consecutive failures | `TOKEN_RENEWAL_CIRCUIT_BREAKER_THRESHOLD` |
| Reset timeout | 60 seconds | `TOKEN_RENEWAL_CIRCUIT_BREAKER_RESET_SECS` |
| Max cycles | 5 | `TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES` |
| Action on max | HALT system | Critical alert |

---

## 4. ERROR CLASSIFICATION

### 4.1 Permanent vs Transient

```rust
fn is_permanent_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("invalid pin")
        || lower.contains("invalid client")
        || lower.contains("blocked")
        || lower.contains("suspended")
        || lower.contains("disabled")
}
```

### 4.2 Error Types

| Error | Severity | Retry? |
|-------|----------|--------|
| `SecretRetrieval` | Critical | 3 SSM retries |
| `TotpGenerationFailed` | Critical | No |
| `AuthenticationFailed` (permanent) | Critical | No — halt |
| `AuthenticationFailed` (transient) | Warning | Yes — backoff |
| `TokenRenewalFailed` | Warning→Critical | Yes — circuit breaker |

---

## 5. SECURITY ARCHITECTURE

### 5.1 Secret Protection Chain

```
SSM (encrypted at rest, KMS)
  └─► fetch_secret() → SecretString
        └─► TOTP generation (exposed briefly for code gen)
        └─► HTTP request body (exposed briefly, zeroized after)
        └─► TokenState.access_token (SecretString)
              └─► ArcSwap (in-memory only)
              └─► Token cache (0600 permissions, container-isolated)
              └─► On drop: zeroize → write_volatile + memory fences
```

### 5.2 Log Safety

| What's Logged | What's NOT Logged |
|---------------|-------------------|
| token_age_hours | access_token value |
| expires_at timestamp | client_secret |
| is_valid boolean | totp_secret |
| SSM paths | SSM values |
| masked IPs (203.0.XXX.XX) | full IPs |
| error reasons | raw HTTP responses containing tokens |

---

## 6. TEST STRATEGY

### 6.1 Current Tests (147)

| Module | Count | Coverage |
|--------|-------|----------|
| types.rs | 39 | Token state, parsing, serialization, debug redaction |
| token_manager.rs | 48 | ArcSwap operations, concurrent access, URL validation |
| secret_manager.rs | 33 | SSM paths, environment validation, error paths |
| totp_generator.rs | 14 | Code generation, invalid secrets, edge cases |
| token_cache.rs | 13 | Roundtrip, corruption, expiry, client ID validation |

### 6.2 Gap Tests to Add (16 gaps)

**CRITICAL (C1-C5):**

| ID | Test | What It Validates |
|----|------|-------------------|
| C1 | `test_full_auth_flow_with_mock_http` | TOTP → HTTP POST → parse → ArcSwap store (E2E) |
| C2 | `test_renewal_cycle_with_mock_http` | 23h timer → renewToken → fallback → store |
| C3 | `test_concurrent_token_swap_safety` | Multiple readers during atomic swap |
| C4 | `test_dhan_error_responses_handled` | 429/500/malformed JSON from Dhan |
| C5 | `test_token_cache_file_permissions` | Cache file has 0600 permissions |

**HIGH (H1-H5):**

| ID | Test | What It Validates |
|----|------|-------------------|
| H1 | `test_renewal_fallback_path` | renewToken fails → generateAccessToken succeeds |
| H2 | `test_totp_window_boundary` | Code gen at second 29, validation at second 31 |
| H3 | `test_both_response_formats` | Flat (generateAccessToken) vs wrapped (renewToken) |
| H4 | `test_circuit_breaker_trip_and_recovery` | 3 failures → trip → reset → retry |
| H5 | `test_notification_on_auth_failure` | Token failure emits NotificationEvent |

**MEDIUM (M1-M6):**

| ID | Test | What It Validates |
|----|------|-------------------|
| M1 | `test_token_cache_race_save_load` | Concurrent save/load on same file |
| M2 | `test_epoch_boundary_10_billion` | Exactly 10_000_000_000 (millis vs seconds edge) |
| M3 | `test_ssm_fetch_timeout_handling` | SSM taking >10s → timeout error |
| M4 | `test_expired_token_on_hot_path` | load() returns expired token between renewals |
| M5 | `test_token_cache_future_format_compat` | Extra JSON fields don't break deserialization |
| M6 | `test_ssm_credential_rotation_mid_session` | SSM value changes → next renewal uses new value |

---

## 7. CONSTANTS

```rust
// TOTP
TOTP_DIGITS = 6
TOTP_PERIOD_SECS = 64   // Note: code uses 30s in TOTP::new, constant is for retry timing
TOTP_SKEW = 1
TOTP_MAX_RETRIES = 2

// Endpoints
DHAN_GENERATE_TOKEN_PATH = "/app/generateAccessToken"
DHAN_RENEW_TOKEN_PATH = "/RenewToken"

// Retry
DHAN_TOKEN_GENERATION_COOLDOWN_SECS = 125
AUTH_RETRY_MAX_BACKOFF_SECS = 300

// Circuit Breaker
TOKEN_RENEWAL_CIRCUIT_BREAKER_THRESHOLD = 3
TOKEN_RENEWAL_CIRCUIT_BREAKER_RESET_SECS = 60
TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES = 5

// Cache
TOKEN_CACHE_FILE_PATH = "/tmp/dlt-token-cache"
TOKEN_CACHE_MIN_REMAINING_HOURS = 1

// Init
TOKEN_INIT_TIMEOUT_SECS = 300
```

---

## 8. CONFIGURATION

```toml
[dhan]
rest_api_base_url = "https://api.dhan.co/v2"
auth_base_url = "https://auth.dhan.co"
market_feed_url = "wss://api-feed.dhan.co"
order_update_websocket_url = "wss://api-order-update.dhan.co"

[token]
refresh_before_expiry_hours = 1
token_validity_hours = 24

[network]
request_timeout_ms = 10000
retry_initial_delay_ms = 100
retry_max_delay_ms = 30000
retry_max_attempts = 5
```
