# BLOCK 02 — AUTHENTICATION & TOKEN MANAGEMENT

## Status: PLANNED
## Depends On: Block 01 (COMPLETE)
## Blocks: Block 03 (WebSocket Connection Manager)

---

## OBJECTIVE

Implement the complete authentication pipeline: fetch Dhan credentials from AWS SSM Parameter Store, generate TOTP codes, acquire JWT access tokens via Dhan REST API, store tokens with O(1) atomic reads via arc-swap, and manage the 24-hour renewal lifecycle. All fully automated — zero manual intervention.

This block is second in the boot chain: `Config → Auth → WebSocket → Parse → Route`. Without a valid token, nothing downstream works — no WebSocket connections, no REST API calls, no order submission.

---

## BOOT CHAIN POSITION

```
Config (base.toml) → ★ AUTH (Block 02) ★ → WebSocket (Block 03) → Parse (Block 04) → ...
                     │
                     ├─ Read SSM secrets (client-id, client-secret, totp-secret)
                     ├─ Generate TOTP code
                     ├─ POST /v2/generateAccessToken → JWT (24h)
                     ├─ Store in ArcSwap<TokenState> → O(1) reads
                     └─ Spawn renewal task (refresh at 23h)
```

---

## AUTHENTICATION FLOW

### Step 1: Fetch Credentials from AWS SSM

```
Source:     AWS SSM Parameter Store (SecureString type)
Dev:        LocalStack at dlt-localstack:4566
Prod:       Real AWS SSM in ap-south-1

Paths:
  /dlt/<env>/dhan/client-id         → Dhan Client ID
  /dlt/<env>/dhan/client-secret     → Dhan password/secret
  /dlt/<env>/dhan/totp-secret       → TOTP Base32 secret

Environment detection:
  AWS_ENDPOINT_URL env var present  → LocalStack (dev)
  AWS_ENDPOINT_URL absent           → Real AWS (prod)
  Same SDK, same code path.
```

### Step 2: Generate TOTP Code

```
Library:    totp-rs 5.7.0 (from Bible)
Algorithm:  SHA-1
Digits:     6
Period:     30 seconds
Secret:     Base32-encoded (from SSM)
```

### Step 3: Acquire JWT Access Token

```
POST {config.dhan.rest_api_base_url}/v2/generateAccessToken
Content-Type: application/json

Request:
{
    "client_id": "<from SSM>",
    "client_secret": "<from SSM>",
    "totp": "<generated TOTP>"
}

Response (200 OK):
{
    "status": "success",
    "data": {
        "access_token": "<JWT>",
        "token_type": "Bearer",
        "expires_in": 86400
    }
}
```

### Step 4: Store Token via arc-swap

```
ArcSwap<Option<TokenState>>
  - None = no token (initial state)
  - Some(TokenState) = active token

TokenState:
  - access_token: Secret<String>   (secrecy-wrapped)
  - expires_at: DateTime<FixedOffset>
  - issued_at: DateTime<FixedOffset>

Read:  arc_swap.load() → O(1) atomic pointer read
Write: arc_swap.store() → O(1) atomic pointer swap
Drop:  old token zeroized via zeroize crate
```

### Step 5: Schedule Renewal

```
Token valid:       24 hours (config.token.token_validity_hours)
Refresh at:        23 hours (24 - config.token.refresh_before_expiry_hours)
Renewal endpoint:  POST /v2/renewToken (lighter than full auth)
Fallback:          If renewToken fails → generateAccessToken (full auth)
Retry:             backon exponential backoff (100ms → 30s, 5 attempts)
Circuit breaker:   failsafe (trip after 3 consecutive failures)
```

---

## TOKEN STATE MACHINE

```
             ┌──────────────────┐
             │    Uninitialized  │
             └────────┬─────────┘
                      │ generateAccessToken()
                      ▼
             ┌──────────────────┐
      ┌──────│      Active      │──────────────┐
      │      └────────┬─────────┘              │
      │               │ refresh window         │ token invalid/expired
      │               │ (23h elapsed)          │
      │               ▼                        ▼
      │      ┌──────────────────┐     ┌──────────────────┐
      │      │    Renewing      │     │     Expired      │
      │      └────────┬─────────┘     └────────┬─────────┘
      │               │                        │
      │       success │              generateAccessToken()
      │               │                        │
      └───────────────┘────────────────────────┘
```

**Transitions:**
- `Uninitialized → Active`: First generateAccessToken succeeds
- `Active → Renewing`: Refresh timer fires (23h after issue)
- `Renewing → Active`: renewToken or generateAccessToken succeeds
- `Active → Expired`: Token is past expires_at
- `Expired → Active`: Re-authenticate via generateAccessToken

**Failure handling:**
- Renewing fails 1st time: retry with backoff, log WARN
- Renewing fails 2nd time: Telegram alert, continue retrying
- Renewing fails 3rd time: circuit breaker trips, try generateAccessToken
- All retries exhausted: HALT system, SNS alert, human intervention required

---

## TOKEN RENEWAL ENDPOINT

```
POST {config.dhan.rest_api_base_url}/v2/renewToken
Authorization: <current_access_token>
Content-Type: application/json

Request:
{
    "client_id": "<from SSM>",
    "totp": "<fresh TOTP>"
}

Response (same format as generateAccessToken):
{
    "status": "success",
    "data": {
        "access_token": "<new JWT>",
        "token_type": "Bearer",
        "expires_in": 86400
    }
}
```

**Why renewToken first:** Faster, doesn't require client_secret. Only needs current valid token + TOTP. Falls back to generateAccessToken if renewToken fails (e.g., token already expired).

---

## SECURITY REQUIREMENTS

| Requirement | Implementation |
|------------|----------------|
| Credential wrapping | All SSM values in `Secret<String>` (secrecy 0.10.3) |
| Memory zeroing | `#[derive(Zeroize, ZeroizeOnDrop)]` on token structs (zeroize 1.8.2) |
| Debug safety | `Secret<T>` prints `[REDACTED]` — never leaks in logs |
| No disk storage | Tokens exist only in memory (ArcSwap) — never persisted |
| No cache storage | Never store tokens in Valkey or QuestDB |
| Header-only transport | Authorization header only — never in query params or body |
| Log safety | Log only: token_age_hours, expires_at, is_valid — never the token value |
| Rotation on leak | If token appears in logs → immediate rotation + incident report |

---

## FILES TO CREATE

| # | File | Purpose |
|---|------|---------|
| 1 | `crates/core/src/auth/mod.rs` | Module root — re-exports public API |
| 2 | `crates/core/src/auth/secret_manager.rs` | Fetch credentials from AWS SSM |
| 3 | `crates/core/src/auth/totp_generator.rs` | Generate TOTP codes from base32 secret |
| 4 | `crates/core/src/auth/token_manager.rs` | JWT lifecycle: acquire, store, renew, refresh |
| 5 | `crates/core/src/auth/types.rs` | Auth-specific types (TokenState, DhanAuthResponse) |

## FILES TO MODIFY

| # | File | Changes |
|---|------|---------|
| 1 | `crates/core/src/lib.rs` | Add `pub mod auth;` |
| 2 | `crates/core/Cargo.toml` | Add missing deps: `totp-rs`, `jsonwebtoken`, `secrecy`, `zeroize`, `aws-config`, `aws-sdk-ssm`, `failsafe`, `governor`, `serde_json` |
| 3 | `crates/common/src/error.rs` | Add auth error variants |
| 4 | `crates/common/src/constants.rs` | Add TOTP constants + auth endpoint paths |

---

## CONSTANTS TO ADD (`constants.rs`)

```rust
// ---------------------------------------------------------------------------
// Authentication — TOTP Configuration
// ---------------------------------------------------------------------------

/// TOTP digit count (Dhan uses 6-digit codes).
pub const TOTP_DIGITS: usize = 6;

/// TOTP time period in seconds (standard 30-second window).
pub const TOTP_PERIOD_SECS: u64 = 30;

/// TOTP algorithm name for logging (actual algorithm set in code).
pub const TOTP_ALGORITHM: &str = "SHA1";

// ---------------------------------------------------------------------------
// Authentication — Dhan REST API Endpoint Paths
// ---------------------------------------------------------------------------

/// Path for initial token generation (appended to rest_api_base_url).
pub const DHAN_GENERATE_TOKEN_PATH: &str = "/generateAccessToken";

/// Path for token renewal (appended to rest_api_base_url).
pub const DHAN_RENEW_TOKEN_PATH: &str = "/renewToken";

// ---------------------------------------------------------------------------
// Authentication — SSM Environment Path Segment
// ---------------------------------------------------------------------------

/// Default environment name for SSM path construction.
/// Overridden by ENVIRONMENT env var if present.
pub const DEFAULT_SSM_ENVIRONMENT: &str = "dev";

/// SSM path template: /dlt/<env>/dhan/<key>
/// Constructed at runtime using SSM_SECRET_BASE_PATH + env + service + key.
pub const SSM_DHAN_SERVICE: &str = "dhan";

// ---------------------------------------------------------------------------
// Authentication — Circuit Breaker
// ---------------------------------------------------------------------------

/// Maximum consecutive token renewal failures before circuit breaker trips.
pub const TOKEN_RENEWAL_CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

/// Circuit breaker reset timeout in seconds (try again after this).
pub const TOKEN_RENEWAL_CIRCUIT_BREAKER_RESET_SECS: u64 = 60;
```

---

## ERROR VARIANTS TO ADD (`error.rs`)

```rust
/// TOTP code generation failed.
#[error("TOTP generation failed: {reason}")]
TotpGenerationFailed { reason: String },

/// Dhan authentication API call failed.
#[error("Dhan authentication failed: {reason}")]
AuthenticationFailed { reason: String },

/// Token renewal failed after all retries.
#[error("token renewal failed after {attempts} attempts: {reason}")]
TokenRenewalFailed { attempts: u32, reason: String },

/// Authentication circuit breaker tripped — too many consecutive failures.
#[error("auth circuit breaker tripped after {failures} consecutive failures")]
AuthCircuitBreakerTripped { failures: u32 },
```

---

## IMPLEMENTATION DESIGN

### `secret_manager.rs` — SSM Credential Retrieval

```rust
/// Fetches all Dhan authentication credentials from AWS SSM Parameter Store.
///
/// Returns credentials wrapped in Secret<String> for safety.
/// Uses aws-sdk-ssm with automatic endpoint detection
/// (LocalStack in dev, real AWS in prod).
pub async fn fetch_dhan_credentials(
    environment: &str,
) -> Result<DhanCredentials, ApplicationError>
```

**DhanCredentials struct:**
```rust
pub struct DhanCredentials {
    pub client_id: Secret<String>,
    pub client_secret: Secret<String>,
    pub totp_secret: Secret<String>,
}
```

**SSM path construction:**
```
format!("{}/{}/{}/{}",
    SSM_SECRET_BASE_PATH,   // "/dlt"
    environment,             // "dev" or "prod"
    SSM_DHAN_SERVICE,       // "dhan"
    secret_name             // "client-id", "client-secret", "totp-secret"
)
```

**AWS SDK initialization:**
```rust
let mut config_builder = aws_config::from_env().region(Region::new("ap-south-1"));

// If AWS_ENDPOINT_URL is set, use LocalStack
if let Ok(endpoint) = std::env::var("AWS_ENDPOINT_URL") {
    config_builder = config_builder.endpoint_url(&endpoint);
}

let aws_config = config_builder.load().await;
let ssm_client = aws_sdk_ssm::Client::new(&aws_config);
```

### `totp_generator.rs` — TOTP Code Generation

```rust
/// Generates a TOTP code from the base32 secret.
///
/// Uses SHA-1 algorithm with 6-digit codes and 30-second periods
/// as required by Dhan's authentication system.
pub fn generate_totp_code(
    totp_secret: &Secret<String>,
) -> Result<String, ApplicationError>
```

**Implementation uses totp-rs:**
```rust
use totp_rs::{Algorithm, TOTP, Secret as TotpSecret};

let totp = TOTP::new(
    Algorithm::SHA1,
    TOTP_DIGITS,
    1,  // step (standard)
    TOTP_PERIOD_SECS,
    TotpSecret::Encoded(secret_value.to_string())
        .to_bytes()
        .map_err(|e| ...)?,
)?;

let code = totp.generate_current()
    .map_err(|e| ...)?;
```

### `token_manager.rs` — JWT Lifecycle Management

```rust
/// Manages the Dhan JWT token lifecycle.
///
/// Provides O(1) atomic token reads for all downstream consumers
/// (WebSocket connections, REST API calls) via arc-swap.
/// Handles initial authentication, scheduled renewal, and failure recovery.
pub struct TokenManager {
    /// Atomic pointer to current token state. O(1) reads.
    token: Arc<ArcSwap<Option<TokenState>>>,

    /// Cached credentials (fetched once from SSM at startup).
    credentials: DhanCredentials,

    /// Dhan REST API base URL (from config).
    rest_api_base_url: String,

    /// HTTP client for Dhan API calls.
    http_client: reqwest::Client,

    /// Token refresh configuration.
    token_config: TokenConfig,

    /// Network retry configuration.
    network_config: NetworkConfig,
}
```

**Public API:**
```rust
impl TokenManager {
    /// Creates a new TokenManager and performs initial authentication.
    ///
    /// Fetches credentials from SSM, generates TOTP, acquires JWT,
    /// and spawns the background renewal task.
    pub async fn initialize(
        config: &ApplicationConfig,
    ) -> Result<Self, ApplicationError>

    /// Returns an Arc handle for O(1) token reads by downstream consumers.
    ///
    /// WebSocket connections and REST clients call this once at startup,
    /// then load() on every use. The pointer is swapped atomically on renewal.
    pub fn token_handle(&self) -> Arc<ArcSwap<Option<TokenState>>>

    /// Spawns the background token renewal task.
    ///
    /// Runs on a tokio interval timer. Renews at
    /// (token_validity_hours - refresh_before_expiry_hours) after issuance.
    pub fn spawn_renewal_task(self: Arc<Self>) -> tokio::task::JoinHandle<()>
}
```

**Token acquisition (private):**
```rust
/// Performs full authentication: TOTP → generateAccessToken → store.
async fn acquire_token(&self) -> Result<(), ApplicationError> {
    let totp_code = generate_totp_code(&self.credentials.totp_secret)?;

    let response = self.http_client
        .post(format!("{}{}", self.rest_api_base_url, DHAN_GENERATE_TOKEN_PATH))
        .json(&GenerateTokenRequest {
            client_id: self.credentials.client_id.expose_secret().clone(),
            client_secret: self.credentials.client_secret.expose_secret().clone(),
            totp: totp_code,
        })
        .timeout(Duration::from_millis(self.network_config.request_timeout_ms))
        .send()
        .await?;

    let auth_response: DhanAuthResponse = response.json().await?;
    let token_state = TokenState::from_response(auth_response);

    self.token.store(Arc::new(Some(token_state)));
    Ok(())
}
```

**Token renewal (private):**
```rust
/// Attempts token renewal. Falls back to full auth on failure.
async fn renew_token(&self) -> Result<(), ApplicationError> {
    // Try renewToken first (lighter)
    match self.try_renew_token().await {
        Ok(()) => Ok(()),
        Err(renew_err) => {
            warn!(?renew_err, "renewToken failed — falling back to generateAccessToken");
            self.acquire_token().await
        }
    }
}
```

**Renewal task with retry + circuit breaker:**
```rust
async fn renewal_loop(self: Arc<Self>) {
    loop {
        // Sleep until refresh window
        let sleep_duration = self.time_until_refresh();
        tokio::time::sleep(sleep_duration).await;

        // Retry with exponential backoff (backon)
        let result = (|| async { self.renew_token().await })
            .retry(ExponentialBuilder::default()
                .with_min_delay(Duration::from_millis(
                    self.network_config.retry_initial_delay_ms
                ))
                .with_max_delay(Duration::from_millis(
                    self.network_config.retry_max_delay_ms
                ))
                .with_max_times(self.network_config.retry_max_attempts as usize))
            .await;

        match result {
            Ok(()) => {
                info!(
                    expires_at = %self.current_expiry(),
                    "token renewed successfully"
                );
            }
            Err(err) => {
                error!(?err, "ALL token renewal attempts failed — system must halt");
                // Trigger Telegram + SNS alert (Block 13)
                // For now: log CRITICAL and continue — alert system built later
            }
        }
    }
}
```

---

## DOWNSTREAM CONSUMER PATTERN

How WebSocket and REST clients will use the token (Block 03+):

```rust
// At startup, each consumer gets a handle
let token_handle = token_manager.token_handle();

// On every WebSocket connection or REST call
let token_guard = token_handle.load();
let token_state = token_guard.as_ref()
    .ok_or(ApplicationError::AuthenticationFailed {
        reason: "no active token".to_string(),
    })?;

// Use in HTTP header
let header_value = format!("Bearer {}",
    token_state.access_token.expose_secret()
);
```

**Key property:** `token_handle.load()` is O(1) — a single atomic pointer read with no locking. This is on the hot path (every WebSocket reconnection, every REST call).

---

## DEPENDENCIES TO ADD TO `crates/core/Cargo.toml`

```toml
# Authentication (Block 02)
totp-rs = { workspace = true }
jsonwebtoken = { workspace = true }
secrecy = { workspace = true }
zeroize = { workspace = true }
aws-config = { workspace = true }
aws-sdk-ssm = { workspace = true }
failsafe = { workspace = true }
serde_json = { workspace = true }
```

Already present in core/Cargo.toml:
- `arc-swap`, `reqwest`, `backon`, `tokio`, `chrono`, `tracing`, `anyhow`, `thiserror`, `serde`

---

## TEST PLAN

### Unit Tests

| Test | What It Validates |
|------|------------------|
| `test_totp_generates_six_digit_code` | TOTP output is exactly 6 digits |
| `test_totp_code_changes_every_30_seconds` | Two codes 31s apart are different |
| `test_totp_invalid_base32_returns_error` | Bad secret → TotpGenerationFailed |
| `test_token_state_from_valid_response` | DhanAuthResponse → TokenState mapping |
| `test_token_state_expiry_calculation` | expires_at = issued_at + expires_in |
| `test_token_state_is_valid_before_expiry` | is_valid() returns true before expiry |
| `test_token_state_is_invalid_after_expiry` | is_valid() returns false after expiry |
| `test_token_state_needs_refresh_in_window` | needs_refresh() true at 23h+ |
| `test_token_state_no_refresh_before_window` | needs_refresh() false before 23h |
| `test_ssm_path_construction` | Path format matches /dlt/dev/dhan/client-id |
| `test_ssm_path_prod_environment` | Path uses "prod" when configured |
| `test_credentials_debug_redacted` | Debug print shows [REDACTED], not secrets |
| `test_token_debug_redacted` | Debug print shows [REDACTED] for access_token |
| `test_arc_swap_load_returns_none_initially` | Fresh ArcSwap has None |
| `test_arc_swap_store_makes_token_available` | Store → load returns Some |
| `test_arc_swap_store_replaces_previous` | Second store replaces first |

### Integration Tests (require LocalStack or mock)

| Test | What It Validates |
|------|------------------|
| `test_fetch_credentials_from_localstack` | SSM → DhanCredentials with all 3 secrets |
| `test_fetch_credentials_missing_secret_returns_error` | Missing SSM key → SecretRetrieval |
| `test_full_auth_flow_with_mock_dhan_api` | SSM → TOTP → API → TokenState |
| `test_token_renewal_with_mock_api` | renewToken flow works end-to-end |
| `test_renewal_fallback_to_generate_on_failure` | renewToken fails → generateAccessToken |
| `test_retry_with_exponential_backoff` | backon retries with increasing delays |

### Security Tests

| Test | What It Validates |
|------|------------------|
| `test_secret_not_in_debug_output` | Secret<String> Debug = "[REDACTED]" |
| `test_token_not_in_display_output` | TokenState Display has no token value |
| `test_zeroize_clears_memory` | After drop, memory contains zeros |

---

## BUILD ORDER (8 Steps)

1. **Constants** — Add TOTP, auth endpoint, circuit breaker constants to `constants.rs`. `cargo check`.

2. **Error variants** — Add auth error variants to `error.rs`. `cargo check`.

3. **Core Cargo.toml** — Add missing workspace deps (totp-rs, jsonwebtoken, secrecy, zeroize, aws-config, aws-sdk-ssm, failsafe, serde_json). `cargo check`.

4. **Auth types** — Create `crates/core/src/auth/types.rs` with TokenState, DhanCredentials, DhanAuthResponse, GenerateTokenRequest, RenewTokenRequest. `cargo check`.

5. **Secret manager** — Create `crates/core/src/auth/secret_manager.rs` — SSM retrieval. `cargo check`.

6. **TOTP generator** — Create `crates/core/src/auth/totp_generator.rs` — code generation. `cargo check`.

7. **Token manager** — Create `crates/core/src/auth/token_manager.rs` — full lifecycle with arc-swap, renewal task, retry, circuit breaker. `cargo check`.

8. **Module wiring + tests** — Create `crates/core/src/auth/mod.rs`, update `lib.rs`, write all unit tests. `cargo test`.

### Quality Gates After Each Step

```
cargo check          → zero errors
cargo fmt            → clean formatting
cargo clippy -- -D warnings → zero warnings
cargo test           → 100% pass (existing 96 + new auth tests)
```

---

## VERIFICATION

1. `cargo check` — zero errors after each step
2. `cargo test` — all tests pass (96 existing + new auth tests)
3. `cargo clippy -- -D warnings` — zero warnings
4. `cargo fmt --check` — clean
5. Integration test: run against LocalStack container, verify SSM retrieval
6. Mock test: mock Dhan API responses, verify full auth + renewal flow
7. Security test: verify Secret<String> never leaks in Debug/Display
8. Commit: `feat(auth): [Phase 1] implement Block 02 — authentication & token management`

---

## WHAT THIS BLOCK DOES NOT INCLUDE

- **Telegram alerting** — Alert integration built in Block 13 (Observability). For now, auth failures log ERROR only.
- **WebSocket auth injection** — That's Block 03's job. Block 02 provides the token_handle; Block 03 reads it.
- **Order API auth** — Block 10's job. Block 02 provides the token; Block 10 attaches it to REST calls.
- **Token persistence** — Tokens are NEVER persisted. Rebuilt from SSM credentials on every boot.
