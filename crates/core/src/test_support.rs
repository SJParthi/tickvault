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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_aws_credentials_returns_bool() {
        // Just verify the function runs without panic and returns a bool.
        let result = has_aws_credentials();
        let _ = result; // suppress unused warning
    }

    #[test]
    fn test_has_aws_credentials_with_env_var_set() {
        // Temporarily set the env var and verify it returns true.
        let original = std::env::var("AWS_ACCESS_KEY_ID").ok();
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "test-key-for-coverage");
        }
        assert!(
            has_aws_credentials(),
            "should return true when AWS_ACCESS_KEY_ID is set"
        );
        // Restore original
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            match original {
                Some(val) => std::env::set_var("AWS_ACCESS_KEY_ID", val),
                None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
            }
        }
    }

    #[test]
    fn test_has_aws_credentials_without_env_var_checks_file() {
        // Remove the env var and verify it checks the file path.
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
        }

        // The result depends on whether ~/.aws/credentials exists
        let result = has_aws_credentials();

        // Verify the function checks the file-based credential path
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let credentials_path = std::path::Path::new(&home).join(".aws").join("credentials");
            if credentials_path.exists() {
                assert!(result, "should return true when credentials file exists");
            }
            // If file doesn't exist, result should be false
            // (unless another source exists)
        }

        // Restore
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            if let Some(val) = original_key {
                std::env::set_var("AWS_ACCESS_KEY_ID", val);
            }
        }
    }

    #[test]
    fn test_has_aws_credentials_no_home_env_returns_false() {
        // If both HOME and USERPROFILE are unset and AWS_ACCESS_KEY_ID is unset,
        // the function should return false.
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();
        let original_profile = std::env::var("USERPROFILE").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
        }

        let result = has_aws_credentials();
        assert!(
            !result,
            "should return false when no env vars and no home dir"
        );

        // Restore
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            if let Some(val) = original_key {
                std::env::set_var("AWS_ACCESS_KEY_ID", val);
            }
            if let Some(val) = original_home {
                std::env::set_var("HOME", val);
            }
            if let Some(val) = original_profile {
                std::env::set_var("USERPROFILE", val);
            }
        }
    }

    #[test]
    fn test_has_aws_credentials_userprofile_fallback() {
        // Test the USERPROFILE fallback path (Windows compat)
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();
        let original_profile = std::env::var("USERPROFILE").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("HOME");
        }

        // Set USERPROFILE to a temp directory that has no .aws/credentials
        let tmp_dir = format!("/tmp/dlt-test-userprofile-{}", std::process::id());
        let _ = std::fs::create_dir_all(&tmp_dir);
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::set_var("USERPROFILE", &tmp_dir);
        }

        let result = has_aws_credentials();
        assert!(
            !result,
            "should return false when USERPROFILE dir has no .aws/credentials"
        );

        // Restore
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            if let Some(val) = original_key {
                std::env::set_var("AWS_ACCESS_KEY_ID", val);
            }
            if let Some(val) = original_home {
                std::env::set_var("HOME", val);
            }
            match original_profile {
                Some(val) => std::env::set_var("USERPROFILE", val),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_has_aws_credentials_userprofile_with_credentials_file() {
        // Test the USERPROFILE path when .aws/credentials exists
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();
        let original_profile = std::env::var("USERPROFILE").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("HOME");
        }

        let tmp_dir = format!("/tmp/dlt-test-userprofile-creds-{}", std::process::id());
        let aws_dir = format!("{tmp_dir}/.aws");
        let _ = std::fs::create_dir_all(&aws_dir);
        let creds_path = format!("{aws_dir}/credentials");
        let _ = std::fs::write(&creds_path, "[default]\naws_access_key_id = test\n");
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::set_var("USERPROFILE", &tmp_dir);
        }

        let result = has_aws_credentials();
        assert!(
            result,
            "should return true when USERPROFILE has .aws/credentials"
        );

        // Restore
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            if let Some(val) = original_key {
                std::env::set_var("AWS_ACCESS_KEY_ID", val);
            }
            if let Some(val) = original_home {
                std::env::set_var("HOME", val);
            }
            match original_profile {
                Some(val) => std::env::set_var("USERPROFILE", val),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // HOME with .aws/credentials file
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_aws_credentials_home_with_credentials_file() {
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();
        let original_profile = std::env::var("USERPROFILE").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("USERPROFILE");
        }

        let tmp_dir = format!("/tmp/dlt-test-home-creds-{}", std::process::id());
        let aws_dir = format!("{tmp_dir}/.aws");
        let _ = std::fs::create_dir_all(&aws_dir);
        let creds_path = format!("{aws_dir}/credentials");
        let _ = std::fs::write(&creds_path, "[default]\naws_access_key_id = test\n");
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::set_var("HOME", &tmp_dir);
        }

        let result = has_aws_credentials();
        assert!(result, "should return true when HOME has .aws/credentials");

        // Restore
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            if let Some(val) = original_key {
                std::env::set_var("AWS_ACCESS_KEY_ID", val);
            }
            if let Some(val) = original_home {
                std::env::set_var("HOME", val);
            }
            if let Some(val) = original_profile {
                std::env::set_var("USERPROFILE", val);
            }
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // HOME without .aws/credentials file
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_aws_credentials_home_without_credentials_file() {
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();
        let original_profile = std::env::var("USERPROFILE").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("USERPROFILE");
        }

        let tmp_dir = format!("/tmp/dlt-test-home-no-creds-{}", std::process::id());
        let _ = std::fs::create_dir_all(&tmp_dir);
        // No .aws directory at all
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::set_var("HOME", &tmp_dir);
        }

        let result = has_aws_credentials();
        assert!(
            !result,
            "should return false when HOME has no .aws/credentials"
        );

        // Restore
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            if let Some(val) = original_key {
                std::env::set_var("AWS_ACCESS_KEY_ID", val);
            }
            if let Some(val) = original_home {
                std::env::set_var("HOME", val);
            }
            if let Some(val) = original_profile {
                std::env::set_var("USERPROFILE", val);
            }
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // AWS_ACCESS_KEY_ID takes priority over file check
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_aws_credentials_env_var_takes_priority() {
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            // Set env var — should return true immediately without checking file
            std::env::set_var("AWS_ACCESS_KEY_ID", "test-priority-key");
            // Set HOME to a nonexistent path so file check would fail
            std::env::set_var("HOME", "/tmp/nonexistent-dlt-test-dir-999");
        }

        let result = has_aws_credentials();
        assert!(result, "env var should take priority over file check");

        // Restore
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            match original_key {
                Some(val) => std::env::set_var("AWS_ACCESS_KEY_ID", val),
                None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
            }
            if let Some(val) = original_home {
                std::env::set_var("HOME", val);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Empty AWS_ACCESS_KEY_ID env var still counts as set
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_aws_credentials_empty_env_var_is_ok() {
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();

        // std::env::var returns Ok("") for empty env var
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "");
        }
        let result = has_aws_credentials();
        assert!(
            result,
            "empty AWS_ACCESS_KEY_ID env var still returns Ok from var()"
        );

        // Restore
        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            match original_key {
                Some(val) => std::env::set_var("AWS_ACCESS_KEY_ID", val),
                None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
            }
        }
    }
}
