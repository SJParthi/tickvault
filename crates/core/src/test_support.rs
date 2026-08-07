//! Shared test utilities for the `core` crate.
//!
//! Gated behind `#[cfg(test)]` — never compiled into production binaries.

/// Detects whether tests that assert against a REAL, REACHABLE SSM should run.
///
/// Requires BOTH:
/// 1. an explicit `TICKVAULT_TEST_REAL_SSM=1` opt-in, and
/// 2. credentials on the machine ([`has_aws_credentials`]).
///
/// # Why this exists (2026-08-07)
///
/// Six tests previously branched on [`has_aws_credentials`] alone and, when it
/// returned `true`, asserted `"with real AWS credentials, fetch should
/// succeed"`. That conflates two different questions:
///
/// * *"are credentials present?"* — what `has_aws_credentials` answers, and
/// * *"is SSM actually reachable, with our parameters in it?"* — what those
///   assertions actually require.
///
/// Any environment holding the first without the second fails them: a CI
/// sandbox where the tooling exports a stray `AWS_ACCESS_KEY_ID` with no route
/// to SSM, a developer laptop carrying `~/.aws/credentials` for an unrelated
/// account, an offline machine, a locked-down VPC. Observed live: all six
/// panicked in a sandbox purely because the proxy had exported a key.
///
/// Requiring an explicit opt-in makes running against live infrastructure a
/// deliberate act rather than an accident of the ambient environment. Without
/// the opt-in the tests assert the genuine unit contract — returns `Err`,
/// never panics — which holds everywhere.
///
/// [`has_aws_credentials`]: fn.has_aws_credentials.html
pub fn real_ssm_tests_enabled() -> bool {
    matches!(
        std::env::var("TICKVAULT_TEST_REAL_SSM").as_deref(),
        Ok("1") | Ok("true")
    ) && has_aws_credentials()
}

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
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    // -----------------------------------------------------------------------
    // Env-var serialization guard
    //
    // Regression: 2026-07-05 — the tests below mutate the PROCESS-GLOBAL
    // env vars AWS_ACCESS_KEY_ID / HOME / USERPROFILE (set_var/remove_var)
    // while the default test harness runs them on parallel threads. Under
    // llvm-cov instrumentation the race manifested live in CI:
    // `test_has_aws_credentials_userprofile_with_credentials_file` panicked
    // ("should return true when USERPROFILE has .aws/credentials") because a
    // sibling test's remove_var("USERPROFILE") landed between this test's
    // set_var and its has_aws_credentials() call. Same flake class as the
    // PR #1411 tv_api_token_prod_guard incident (see
    // `.claude/rules/project/merge-gate-lock-2026-07-04.md` §3.2).
    //
    // Fix: every env-mutating test takes CREDS_ENV_LOCK first via
    // CredsEnvGuard, so the mutate → observe window is exclusive; the guard
    // also restores all three vars' prior values on Drop (panic-safe).
    // Poisoning-safe: a failing assertion in one test must not cascade
    // poison-panics, so the lock recovers via
    // `unwrap_or_else(|e| e.into_inner())`.
    // -----------------------------------------------------------------------

    static CREDS_ENV_LOCK: Mutex<()> = Mutex::new(());

    // 2026-08-07: TICKVAULT_TEST_REAL_SSM joins the guarded set. The
    // real_ssm_tests_enabled() tests mutate it, and it is process-global
    // exactly like the other three — leaving it unguarded would re-open the
    // same cross-thread race the CredsEnvGuard exists to close.
    const GUARDED_VARS: [&str; 4] = [
        "AWS_ACCESS_KEY_ID",
        "HOME",
        "USERPROFILE",
        "TICKVAULT_TEST_REAL_SSM",
    ];

    /// Scoped guard: holds the serialization lock for the test's duration
    /// and restores the pre-test values of all four credential-related env
    /// vars on Drop (even on panic), so no test leaks env state into the
    /// next lock holder.
    struct CredsEnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: [(&'static str, Option<std::ffi::OsString>); 4],
    }

    impl CredsEnvGuard {
        fn acquire() -> Self {
            let lock = CREDS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            Self {
                _lock: lock,
                saved: GUARDED_VARS.map(|name| (name, std::env::var_os(name))),
            }
        }
    }

    impl Drop for CredsEnvGuard {
        fn drop(&mut self) {
            // SAFETY: unsafe block required by Rust 2024 edition for
            // std::env::set_var/remove_var; serialized by the held mutex.
            unsafe {
                for (name, val) in &self.saved {
                    match val {
                        Some(v) => std::env::set_var(name, v),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[test]
    fn test_has_aws_credentials_returns_bool() {
        // Just verify the function runs without panic and returns a bool.
        let result = has_aws_credentials();
        let _ = result; // suppress unused warning
    }

    #[test]
    fn test_has_aws_credentials_with_env_var_set() {
        let _guard = CredsEnvGuard::acquire();
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
        let _guard = CredsEnvGuard::acquire();
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
        let _guard = CredsEnvGuard::acquire();
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
        let _guard = CredsEnvGuard::acquire();
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
        let tmp_dir = format!("/tmp/tv-test-userprofile-{}", std::process::id());
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
        let _guard = CredsEnvGuard::acquire();
        // Test the USERPROFILE path when .aws/credentials exists
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();
        let original_profile = std::env::var("USERPROFILE").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("HOME");
        }

        let tmp_dir = format!("/tmp/tv-test-userprofile-creds-{}", std::process::id());
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
        let _guard = CredsEnvGuard::acquire();
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();
        let original_profile = std::env::var("USERPROFILE").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("USERPROFILE");
        }

        let tmp_dir = format!("/tmp/tv-test-home-creds-{}", std::process::id());
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
        let _guard = CredsEnvGuard::acquire();
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();
        let original_profile = std::env::var("USERPROFILE").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("USERPROFILE");
        }

        let tmp_dir = format!("/tmp/tv-test-home-no-creds-{}", std::process::id());
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
        let _guard = CredsEnvGuard::acquire();
        let original_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let original_home = std::env::var("HOME").ok();

        // SAFETY: test is single-threaded for this env var manipulation
        unsafe {
            // Set env var — should return true immediately without checking file
            std::env::set_var("AWS_ACCESS_KEY_ID", "test-priority-key");
            // Set HOME to a nonexistent path so file check would fail
            std::env::set_var("HOME", "/tmp/nonexistent-tv-test-dir-999");
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
        let _guard = CredsEnvGuard::acquire();
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

    // -----------------------------------------------------------------------
    // real_ssm_tests_enabled() — all four env permutations
    //
    // Regression: 2026-08-07 — six tests in secret_manager.rs / ip_verifier.rs
    // / notification/service.rs branched on has_aws_credentials() alone and
    // then asserted "with real AWS credentials, fetch should succeed". A
    // sandbox whose tooling exported a bare AWS_ACCESS_KEY_ID (with no route
    // to SSM) satisfied the credential check and failed all six. The gate now
    // demands an explicit opt-in as well, so running against live AWS is a
    // deliberate act rather than an accident of the ambient environment.
    // -----------------------------------------------------------------------

    /// Sets the opt-in var and a fake credential, so only the opt-in is under
    /// test. Caller must already hold `CredsEnvGuard`.
    ///
    /// # Safety
    /// Serialized by the held `CREDS_ENV_LOCK`; the guard restores all four
    /// vars on Drop.
    unsafe fn set_opt_in(value: Option<&str>) {
        unsafe {
            match value {
                Some(v) => std::env::set_var("TICKVAULT_TEST_REAL_SSM", v),
                None => std::env::remove_var("TICKVAULT_TEST_REAL_SSM"),
            }
        }
    }

    #[test]
    fn test_real_ssm_disabled_when_opt_in_absent_even_with_credentials() {
        let _guard = CredsEnvGuard::acquire();
        // SAFETY: serialized by CredsEnvGuard; restored on Drop.
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "test-key");
            set_opt_in(None);
        }
        assert!(
            has_aws_credentials(),
            "precondition: the fake credential must satisfy has_aws_credentials"
        );
        assert!(
            !real_ssm_tests_enabled(),
            "credentials alone must NOT enable real-SSM tests — this is the exact \
             2026-08-07 sandbox failure (a stray AWS_ACCESS_KEY_ID with no SSM route)"
        );
    }

    #[test]
    fn test_real_ssm_disabled_when_opt_in_set_but_no_credentials() {
        let _guard = CredsEnvGuard::acquire();
        // SAFETY: serialized by CredsEnvGuard; restored on Drop.
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
            set_opt_in(Some("1"));
        }
        assert!(
            !real_ssm_tests_enabled(),
            "opt-in without credentials must NOT enable real-SSM tests"
        );
    }

    #[test]
    fn test_real_ssm_enabled_when_opt_in_and_credentials_both_present() {
        let _guard = CredsEnvGuard::acquire();
        // SAFETY: serialized by CredsEnvGuard; restored on Drop.
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "test-key");
            set_opt_in(Some("1"));
        }
        assert!(
            real_ssm_tests_enabled(),
            "both opt-in and credentials present must enable real-SSM tests"
        );
    }

    #[test]
    fn test_real_ssm_accepts_true_and_rejects_other_values() {
        let _guard = CredsEnvGuard::acquire();
        // SAFETY: serialized by CredsEnvGuard; restored on Drop.
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "test-key");
            set_opt_in(Some("true"));
        }
        assert!(real_ssm_tests_enabled(), "\"true\" must be accepted");

        for rejected in ["0", "false", "yes", "", "TRUE", "1 "] {
            // SAFETY: serialized by CredsEnvGuard; restored on Drop.
            unsafe { set_opt_in(Some(rejected)) };
            assert!(
                !real_ssm_tests_enabled(),
                "{rejected:?} must NOT enable real-SSM tests — only \"1\" and \"true\" do"
            );
        }
    }

    #[test]
    fn test_real_ssm_disabled_when_neither_present() {
        let _guard = CredsEnvGuard::acquire();
        // SAFETY: serialized by CredsEnvGuard; restored on Drop.
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
            set_opt_in(None);
        }
        assert!(
            !real_ssm_tests_enabled(),
            "neither opt-in nor credentials must NOT enable real-SSM tests"
        );
    }
}
