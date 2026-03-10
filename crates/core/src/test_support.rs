//! Shared test utilities for the `core` crate.
//!
//! Gated behind `#[cfg(test)]` — never compiled into production binaries.

/// Detects whether real AWS credentials are available on this machine.
///
/// Checks two credential sources used by the AWS SDK default chain:
/// 1. `AWS_ACCESS_KEY_ID` environment variable (explicit env-based creds)
/// 2. `~/.aws/credentials` file (profile-based creds from `aws configure`)
///
/// Returns `true` if either source exists. Tests that assert "must fail without
/// real SSM" should adapt their expectations when this returns `true`, since
/// SSM calls will succeed on machines with valid AWS credentials (e.g., dev Mac).
pub fn has_aws_credentials() -> bool {
    // Source 1: environment variable credentials
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() {
        return true;
    }

    // Source 2: AWS shared credentials file (~/.aws/credentials)
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let credentials_path = std::path::Path::new(&home).join(".aws").join("credentials");
        if credentials_path.exists() {
            return true;
        }
    }

    false
}
