//! Runs the real public-function hook in isolated synthetic repositories.
//! Pins legacy verdicts and the once-per-crate inventory optimization.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

const HOOK: &str = include_str!("../../../.claude/hooks/pub-fn-test-guard.sh");

const COMMAND_DEADLINE: Duration = Duration::from_secs(10);
const CAPTURE_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
struct CaptureFailure {
    cause: io::Error,
    reaped: Option<ExitStatus>,
    cleanup_errors: Vec<io::Error>,
}

impl From<io::Error> for CaptureFailure {
    fn from(cause: io::Error) -> Self {
        Self {
            cause,
            reaped: None,
            cleanup_errors: Vec::new(),
        }
    }
}

async fn capture_stream(mut pipe: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = pipe.read(&mut chunk).await?;
        if count == 0 {
            return Ok(output);
        }
        if count > CAPTURE_LIMIT.saturating_sub(output.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fixture command output exceeds capture limit",
            ));
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

// Bounds waiting and both pipe reads, without detached readers or an external
// timeout executable. Cancellation drops both pipe futures before cleanup.
// Reaping the direct child still depends on OS scheduling; arbitrary descendants
// are not terminated as a process group. These fixtures own finite child work.
fn captured_output(command: Command, deadline: Duration) -> Result<Output, CaptureFailure> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut child = tokio::process::Command::from(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().expect("captured stdout");
        let stderr = child.stderr.take().expect("captured stderr");
        let outcome = {
            let wait = child.wait();
            let capture = async move {
                let (stdout, stderr, status) =
                    tokio::try_join!(capture_stream(stdout), capture_stream(stderr), wait)?;
                Ok::<_, io::Error>(Output {
                    status,
                    stdout,
                    stderr,
                })
            };
            tokio::time::timeout(deadline, capture).await
        };
        let cause = match outcome {
            Ok(Ok(output)) => return Ok(output),
            Ok(Err(error)) => error,
            Err(_) => io::Error::new(io::ErrorKind::TimedOut, "fixture command deadline exceeded"),
        };
        // Preserve the primary deadline/read error even if cleanup also fails.
        // Always attempt wait after start_kill, including an already-exited child.
        let mut cleanup_errors = Vec::new();
        if let Err(error) = child.start_kill() {
            cleanup_errors.push(error);
        }
        let reaped = match child.wait().await {
            Ok(status) => Some(status),
            Err(error) => {
                cleanup_errors.push(error);
                None
            }
        };
        Err(CaptureFailure {
            cause,
            reaped,
            cleanup_errors,
        })
    })
}

static NEXT: AtomicU64 = AtomicU64::new(0);

struct CaseDir(PathBuf);

impl CaseDir {
    fn new() -> Self {
        for _ in 0..100 {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "tickvault pubfn fixture {} {id}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("exclusive fixture creation: {error}"),
            }
        }
        panic!("fixture collision retry exhausted");
    }

    fn write(&self, path: &str, text: &str) {
        let target = self.0.join(path);
        fs::create_dir_all(target.parent().expect("fixture parent")).expect("fixture directories");
        fs::write(target, text).expect("fixture write");
    }

    fn git(&self, args: &[&str]) {
        self.git_with_environment(args, &[]);
    }

    fn git_with_environment(&self, args: &[&str], environment: &[(&str, OsString)]) {
        let mut command = Command::new("git");
        command
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "tag.gpgSign=false",
            ])
            .args(args)
            .current_dir(&self.0)
            .envs(environment.iter().map(|(key, value)| (*key, value)));
        self.isolate_git(&mut command);
        let out = captured_output(command, COMMAND_DEADLINE).expect("fixture git");
        assert!(out.status.success(), "fixture git failed: {out:?}");
    }

    fn isolate_git(&self, command: &mut Command) {
        // Explicit overrides and inherited selectors can both redirect Git.
        // Remove only this child's Git environment; never mutate the process.
        let explicit: Vec<_> = command.get_envs().map(|(key, _)| key.to_owned()).collect();
        for key in std::env::vars_os().map(|(key, _)| key).chain(explicit) {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        let template = self.0.join("empty-git-template");
        fs::create_dir_all(&template).expect("owned empty Git template");
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TEMPLATE_DIR", template)
            .env("GIT_TERMINAL_PROMPT", "0");
    }

    fn init_git(&self) {
        self.git(&["init", "-q"]);
    }

    fn run(&self, mode: &str, baseline: Option<u64>, ci: bool) -> Outcome {
        self.write(".claude/hooks/pub-fn-test-guard.sh", HOOK);
        let baseline_path = self.0.join(".claude/hooks/.untested-pubfn-baseline");
        if let Some(value) = baseline {
            fs::write(&baseline_path, format!("{value}\n")).expect("fixture baseline");
        } else if baseline_path.exists() {
            fs::remove_file(&baseline_path).expect("remove fixture baseline");
        }
        let mut command = self.command(mode);
        if ci {
            command.env("CI", "true");
        } else {
            command.env_remove("CI");
        }
        let output = captured_output(command, COMMAND_DEADLINE).expect("run real hook");
        Outcome {
            code: output.status.code().expect("hook exited normally"),
            stdout: output.stdout,
            stderr: output.stderr,
            baseline: fs::read(baseline_path).ok(),
        }
    }

    fn command(&self, mode: &str) -> Command {
        self.command_with_environment(mode, &[])
    }

    fn command_with_environment(&self, mode: &str, environment: &[(&str, OsString)]) -> Command {
        let mut command = Command::new("bash");
        command
            .arg(self.0.join(".claude/hooks/pub-fn-test-guard.sh"))
            .arg(&self.0)
            .arg(mode)
            .current_dir(&self.0)
            .envs(environment.iter().map(|(key, value)| (*key, value)))
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .env_remove("CDPATH")
            .env_remove("GREP_OPTIONS");
        self.isolate_git(&mut command);
        command
    }

    fn assert_outcome(
        &self,
        mode: &str,
        baseline: Option<u64>,
        ci: bool,
        code: i32,
        message: &str,
    ) {
        let new = self.run(mode, baseline, ci);
        assert!(new.stdout.is_empty(), "hook verdict belongs on stderr");
        if let Some(value) = baseline {
            assert_eq!(
                new.baseline,
                Some(format!("{value}\n").into_bytes()),
                "existing baseline must never be rewritten"
            );
        } else if ci {
            assert!(
                new.baseline.is_none(),
                "CI must never manufacture a baseline"
            );
        } else {
            assert_eq!(
                new.baseline,
                Some(b"1\n".to_vec()),
                "local fixture bootstrap must preserve the real count"
            );
        }
        assert_eq!(new.code, code, "unexpected real-hook verdict: {new:?}");
        assert!(
            String::from_utf8_lossy(&new.stderr).contains(message),
            "missing expected verdict: {new:?}"
        );
    }
}

impl Drop for CaseDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove exclusively owned fixture");
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    baseline: Option<Vec<u8>>,
}

#[test]
fn all_mode_duplicates_count_individually_and_async_crate_forms_are_kept() {
    let case = CaseDir::new();
    case.write("crates/a/src/lib.rs", "pub fn same() {}\npub async fn async_one() {}\npub(crate) fn crate_one() {}\npub(crate) async fn crate_async() {}\npub fn same() {}\n");
    case.write("crates/a/tests/check.rs", "#[test]\nfn test_async_one() {}\nfn crate_one_suffix() {}\n// test crate_async is deliberately a legacy comment match\n");
    case.assert_outcome("all", Some(2), true, 0, "stable (2)");
    case.assert_outcome("all", Some(1), true, 2, "INCREASED by 1");
}

#[test]
fn test_presence_is_file_wide_but_crate_local_and_tokio_only_does_not_qualify() {
    let case = CaseDir::new();
    case.write(
        "crates/a/src/lib.rs",
        "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n",
    );
    case.write(
        "crates/a/tests/one.rs",
        "#[test]\nfn unrelated() {}\n// test alpha is still a legacy name match\n",
    );
    case.write(
        "crates/a/tests/tokio.rs",
        "#[tokio::test]\nasync fn test_beta() {}\n",
    );
    case.write("crates/b/tests/other.rs", "#[test]\nfn test_gamma() {}\n");
    case.assert_outcome("all", Some(2), true, 0, "stable (2)");
}

#[test]
fn empty_test_inventory_is_cached_and_does_not_satisfy_a_function() {
    let case = CaseDir::new();
    case.write("crates/a/src/lib.rs", "pub fn alpha() {}\npub fn beta() {}\n// test_alpha is not in a file containing the test attribute\n");
    case.assert_outcome("all", Some(2), true, 0, "stable (2)");
    case.assert_outcome("all", Some(5), true, 0, "decreased (5 -> 2)");
}

#[test]
fn exclusions_remain_exact_and_exemptions_only_inspect_the_preceding_line() {
    let case = CaseDir::new();
    case.write("crates/a/src/lib.rs", "pub fn first_line() {}\n// TEST-EXEMPT: unchanged escape hatch\npub fn exempted() {}\n// TEST-EXEMPT: not immediately adjacent\n\npub fn not_exempted() {}\npub fn new() {}\npub fn clone() {}\npub fn default() {}\n");
    case.write(
        "crates/a/src/ignored_test.rs",
        "pub fn ignored_test_path() {}\n",
    );
    case.write(
        "crates/a/tests/ignored.rs",
        "pub fn ignored_tests_path() {}\n",
    );
    case.write(
        "crates/a/benches/ignored.rs",
        "pub fn ignored_bench_path() {}\n",
    );
    case.write(
        "crates/a/src/plural_tests.rs",
        "pub fn plural_is_not_excluded() {}\n",
    );
    case.assert_outcome("all", Some(3), true, 0, "stable (3)");
}

#[test]
fn extraction_preserves_greedy_last_name_and_substring_comment_matches() {
    let case = CaseDir::new();
    case.write("crates/a/src/lib.rs", "// pub fn earlier() {} fn later() {}\npub fn prefix() {}\npub fn Uppercase() {}\npub fn underscore_123() {}\n");
    case.write(
        "crates/a/tests/check.rs",
        "#[test]\nfn test_later() {}\nfn test_prefix_extra() {}\nfn test_underscore_123() {}\n",
    );
    case.assert_outcome("all", Some(0), true, 0, "stable (0)");
}

#[test]
fn legacy_xargs_filename_spaces_quotes_and_backslashes_do_not_gain_coverage() {
    for filename in ["space name.rs", "quote'name.rs", "back\\slash.rs"] {
        let case = CaseDir::new();
        case.write("crates/a/src/lib.rs", "pub fn hidden() {}\n");
        case.write(
            &format!("crates/a/tests/{filename}"),
            "#[test]\nfn test_hidden() {}\n",
        );
        case.assert_outcome("all", Some(1), true, 0, "stable (1)");
    }
}

#[test]
fn production_paths_and_crate_names_with_spaces_preserve_both_modes() {
    let case = CaseDir::new();
    case.init_git();
    case.write("crates/a/src/source name.rs", "pub fn covered() {}\n");
    case.write("crates/a/tests/check.rs", "#[test]\nfn test_covered() {}\n");
    case.write("crates/b space/src/lib.rs", "pub fn untested() {}\n");
    case.git(&["add", "crates"]);
    case.assert_outcome("all", Some(1), true, 0, "stable (1)");
    case.assert_outcome(
        "staged",
        Some(1),
        true,
        2,
        "BLOCKED: 1 new public function(s)",
    );
}

#[test]
fn missing_baseline_is_refused_in_ci_and_only_bootstrapped_outside_ci() {
    let case = CaseDir::new();
    case.write("crates/a/src/lib.rs", "pub fn absent() {}\n");
    case.assert_outcome("all", None, true, 1, "baseline missing in CI");
    case.assert_outcome("all", None, false, 0, "baseline established: 1");
}

#[test]
fn missing_workspace_all_mode_refuses_and_empty_staged_mode_is_clean() {
    let case = CaseDir::new();
    case.init_git();
    case.assert_outcome("all", Some(0), true, 2, "no crates/*.rs found");
    case.assert_outcome("staged", Some(0), true, 0, "");
}

#[test]
fn staged_new_functions_are_checked_without_honoring_all_mode_exemptions() {
    let case = CaseDir::new();
    case.init_git();
    case.write("crates/a/src/lib.rs", "// TEST-EXEMPT: staged mode deliberately ignores this\npub fn untested() {}\npub async fn tested() {}\n");
    case.write("crates/a/tests/check.rs", "#[test]\nfn test_tested() {}\n");
    case.git(&["add", "crates"]);
    case.assert_outcome(
        "staged",
        Some(999),
        true,
        2,
        "BLOCKED: 1 new public function(s)",
    );
}

#[test]
fn staged_deletions_are_not_counted_and_unchanged_existing_functions_are_ignored() {
    let case = CaseDir::new();
    case.init_git();
    case.write("crates/a/src/lib.rs", "pub fn old_untested() {}\n");
    case.write("crates/a/src/deleted.rs", "pub fn deleted_untested() {}\n");
    case.git(&["add", "crates"]);
    case.git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "test: fixture baseline",
    ]);
    case.write(
        "crates/a/src/lib.rs",
        "pub fn old_untested() {}\n// changed without a new declaration\n",
    );
    fs::remove_file(case.0.join("crates/a/src/deleted.rs")).expect("fixture deletion");
    case.git(&["add", "-A", "crates"]);
    case.assert_outcome(
        "staged",
        Some(0),
        true,
        0,
        "All new public functions have matching tests",
    );
}

#[test]
fn inventory_is_evaluated_once_per_crate_including_empty_inventories() {
    let case = CaseDir::new();
    case.write(
        "crates/a/src/lib.rs",
        "pub fn one() {}\npub fn two() {}\npub fn three() {}\n",
    );
    case.write(
        "crates/b/src/lib.rs",
        "pub fn four() {}\npub fn five() {}\n",
    );
    case.write(
        "crates/a/tests/check.rs",
        "#[test]\nfn test_one_two_three() {}\n",
    );
    case.write(".claude/hooks/pub-fn-test-guard.sh", HOOK);
    case.write(".claude/hooks/.untested-pubfn-baseline", "2\n");
    // Instrument only command count, delegating every argument to real grep.
    // No guard verdict or matching content is synthesized by this wrapper.
    case.write("grep-count.env", "grep() {\n  if [ \"${1:-}\" = -r ]; then printf 'scan\\n' >> \"$PUBFN_INVENTORY_LOG\"; fi\n  command grep \"$@\"\n}\n");
    let log = case.0.join("inventory-count.txt");
    let mut command = case.command("all");
    command
        .env("CI", "true")
        .env("BASH_ENV", case.0.join("grep-count.env"))
        .env("PUBFN_INVENTORY_LOG", &log);
    let output = captured_output(command, COMMAND_DEADLINE).expect("instrumented real hook");
    assert!(output.status.success(), "instrumented verdict: {output:?}");
    assert_eq!(
        fs::read_to_string(log)
            .expect("real recursive grep count")
            .lines()
            .count(),
        2,
        "each crate, including the no-test crate, must be enumerated once"
    );
}

fn sentinel_repository() -> CaseDir {
    let sentinel = CaseDir::new();
    sentinel.init_git();
    sentinel.write("sentinel.txt", "committed sentinel\n");
    sentinel.git(&["add", "-A"]);
    sentinel.git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "test: sentinel baseline",
    ]);
    sentinel.write("sentinel.txt", "changed but deliberately unstaged\n");
    sentinel
}

fn poisoned_git_environment(sentinel: &CaseDir) -> Vec<(&'static str, OsString)> {
    let hooks = sentinel.0.join("poison-hooks");
    sentinel.write(
        "poison-hooks/pre-commit",
        "#!/bin/sh\nprintf invoked > fixture-hook-ran\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(hooks.join("pre-commit"), fs::Permissions::from_mode(0o700))
            .expect("owned synthetic hook permission");
    }
    let config = sentinel.0.join("poison-config");
    sentinel.write("poison-config", &format!("[core]\n hooksPath = {}\n[commit]\n gpgSign = true\n[gpg]\n program = fixture-signing-must-not-run\n", hooks.display()));
    let git = sentinel.0.join(".git");
    vec![
        ("GIT_DIR", git.clone().into_os_string()),
        ("GIT_WORK_TREE", sentinel.0.clone().into_os_string()),
        ("GIT_COMMON_DIR", git.clone().into_os_string()),
        ("GIT_INDEX_FILE", git.join("index").into_os_string()),
        ("GIT_OBJECT_DIRECTORY", git.join("objects").into_os_string()),
        (
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            git.join("objects").into_os_string(),
        ),
        ("GIT_CONFIG", config.clone().into_os_string()),
        ("GIT_CONFIG_GLOBAL", config.clone().into_os_string()),
        ("GIT_CONFIG_SYSTEM", config.into_os_string()),
        ("GIT_CONFIG_NOSYSTEM", "0".into()),
        ("GIT_CONFIG_COUNT", "2".into()),
        ("GIT_CONFIG_KEY_0", "core.hooksPath".into()),
        ("GIT_CONFIG_VALUE_0", hooks.clone().into_os_string()),
        ("GIT_CONFIG_KEY_1", "commit.gpgSign".into()),
        ("GIT_CONFIG_VALUE_1", "true".into()),
        ("GIT_CONFIG_PARAMETERS", "'core.abbrev'='7'".into()),
        ("GIT_TEMPLATE_DIR", hooks.into_os_string()),
        (
            "GIT_TRACE",
            sentinel.0.join("poison-trace").into_os_string(),
        ),
    ]
}

fn snapshot_tree(root: &std::path::Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(
        root: &std::path::Path,
        directory: &std::path::Path,
        files: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) {
        for entry in fs::read_dir(directory).expect("owned sentinel directory") {
            let entry = entry.expect("owned sentinel entry");
            let kind = entry.file_type().expect("owned sentinel type");
            if kind.is_dir() {
                walk(root, &entry.path(), files);
            } else {
                assert!(
                    kind.is_file(),
                    "synthetic sentinel must contain only regular files"
                );
                files.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .expect("sentinel-relative path")
                        .to_path_buf(),
                    fs::read(entry.path()).expect("owned sentinel bytes"),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

#[test]
fn fixture_git_children_cannot_mutate_a_poison_selected_external_repository() {
    let sentinel = sentinel_repository();
    let poisoned = poisoned_git_environment(&sentinel);
    let before = snapshot_tree(&sentinel.0);
    let index_before = fs::read(sentinel.0.join(".git/index")).expect("sentinel index");
    let case = CaseDir::new();
    case.write("crates/a/src/lib.rs", "pub fn fixture_only() {}\n");
    case.git_with_environment(&["init", "-q"], &poisoned);
    case.git_with_environment(&["add", "-A"], &poisoned);
    assert!(
        fs::read(sentinel.0.join(".git/index")).expect("sentinel index after add") == index_before,
        "fixture add must not modify the external sentinel Git index"
    );
    case.git_with_environment(
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "test: isolated fixture commit",
        ],
        &poisoned,
    );
    assert!(
        snapshot_tree(&sentinel.0) == before,
        "external sentinel repository must remain byte-identical"
    );
    assert!(
        !case.0.join("fixture-hook-ran").exists(),
        "fixture commit must not run inherited hooks"
    );
}

#[test]
fn hook_child_cannot_false_pass_through_a_poison_selected_clean_index() {
    let sentinel = sentinel_repository();
    let poisoned = poisoned_git_environment(&sentinel);
    let before = snapshot_tree(&sentinel.0);
    let case = CaseDir::new();
    case.init_git();
    case.write(
        "crates/a/src/lib.rs",
        "pub fn untested_fixture_addition() {}\n",
    );
    case.git(&["add", "crates"]);
    case.write(".claude/hooks/pub-fn-test-guard.sh", HOOK);
    case.write(".claude/hooks/.untested-pubfn-baseline", "0\n");
    let mut command = case.command_with_environment("staged", &poisoned);
    command.env("CI", "true");
    let output =
        captured_output(command, COMMAND_DEADLINE).expect("real hook with poisoned selectors");
    assert_eq!(
        output.status.code(),
        Some(2),
        "hook must inspect the fixture's untested staged addition"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("BLOCKED: 1 new public function(s)"));
    assert!(
        snapshot_tree(&sentinel.0) == before,
        "hook child must leave the external sentinel repository unchanged"
    );
}

fn builtin_child(script: &str) -> Command {
    let mut command = Command::new("bash");
    command
        .args(["--noprofile", "--norc", "-c", script])
        .env_remove("BASH_ENV")
        .env_remove("ENV");
    command
}

#[test]
fn command_deadline_terminates_and_reaps_a_non_exiting_direct_child() {
    let failure = captured_output(
        builtin_child("while :; do :; done"),
        Duration::from_millis(250),
    )
    .expect_err("non-exiting child must hit the Rust deadline");
    assert_eq!(failure.cause.kind(), io::ErrorKind::TimedOut);
    assert!(
        failure.reaped.is_some_and(|status| !status.success()),
        "direct child was not reaped after termination: {failure:?}"
    );
    assert!(
        failure.cleanup_errors.is_empty(),
        "direct-child cleanup failed: {failure:?}"
    );
}

#[test]
fn command_deadline_refuses_a_finite_slow_child() {
    // Finite even if the deadline is removed by an ordinary mutation control.
    let failure = captured_output(
        builtin_child("SECONDS=0; while (( SECONDS < 3 )); do :; done"),
        Duration::from_millis(250),
    )
    .expect_err("finite slow child must hit the Rust deadline");
    assert_eq!(failure.cause.kind(), io::ErrorKind::TimedOut);
    assert!(
        failure.reaped.is_some(),
        "direct child must be waited: {failure:?}"
    );
    assert!(
        failure.cleanup_errors.is_empty(),
        "direct-child cleanup failed: {failure:?}"
    );
}

#[test]
fn simultaneous_pipe_output_above_pipe_capacity_is_captured_exactly() {
    let output = captured_output(
        builtin_child("printf -v block '%4096s' ''; block=${block// /x}; for ((i=0;i<64;i++)); do printf '%s' \"$block\"; printf '%s' \"$block\" >&2; done"),
        COMMAND_DEADLINE,
    ).expect("both pipe readers must make progress");
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 256 * 1024);
    assert_eq!(output.stderr.len(), 256 * 1024);
    assert!(output.stdout.iter().all(|&byte| byte == b'x'));
    assert!(output.stderr.iter().all(|&byte| byte == b'x'));
}

#[test]
fn output_capture_limit_refuses_each_stream_without_truncating_a_verdict() {
    for stream in ["stdout", "stderr"] {
        let mut command = builtin_child(
            "if [ \"$1\" = stderr ]; then exec 1>&2; fi; for ((i=0;i<257;i++)); do printf '%4096s' ''; done",
        );
        command.args(["fixture", stream]);
        let failure = captured_output(command, COMMAND_DEADLINE)
            .expect_err("oversized output must fail explicitly");
        assert_eq!(failure.cause.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            failure.cause.to_string(),
            "fixture command output exceeds capture limit"
        );
        assert!(
            failure.reaped.is_some(),
            "overflowing direct child must be waited: {failure:?}"
        );
        assert!(
            failure.cleanup_errors.is_empty(),
            "direct-child cleanup failed: {failure:?}"
        );
    }
}

#[test]
fn capture_preserves_nonzero_exit_and_both_output_streams() {
    let output = captured_output(
        builtin_child("printf standard; printf diagnostic >&2; exit 7"),
        COMMAND_DEADLINE,
    )
    .expect("normal nonzero exit must remain a captured verdict");
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"standard");
    assert_eq!(output.stderr, b"diagnostic");
}
