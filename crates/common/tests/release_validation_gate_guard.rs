//! Exercise the actual merge/release shell gates with synthetic API responses.
//! No GitHub, AWS, service, database, or migration call leaves these fixtures.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";
const OTHER_SHA: &str = "abcdef1234567890abcdef1234567890abcdef12";
const TREE: &str = "0123456789abcdef0123456789abcdef01234567";
const RELEASE_CHECK: &str = "Release Validation / Release Validation Complete";

#[cfg(target_os = "macos")]
const FIXTURE_TOOL_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
#[cfg(not(target_os = "macos"))]
const FIXTURE_TOOL_PATH: &str = "/usr/bin:/bin";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/common parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn workflow(name: &str) -> String {
    std::fs::read_to_string(repo_root().join(".github/workflows").join(name))
        .expect("read actual workflow")
}

fn job(body: &str, name: &str) -> String {
    let marker = format!("  {name}:\n");
    body.split_once(&marker)
        .expect("workflow job")
        .1
        .lines()
        .take_while(|line| {
            !line.starts_with("  ")
                || line.starts_with("    ")
                || line.trim_start().starts_with('#')
                || line.trim().is_empty()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn script(body: &str, name: &str) -> String {
    let marker = format!("      - name: {name}\n");
    let step = body
        .split_once(&marker)
        .expect("named workflow step")
        .1
        .split("\n      - name:")
        .next()
        .expect("step body");
    step.split_once("        run: |\n")
        .expect("literal shell script")
        .1
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tickvault-release-gate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create isolated shell fixture");
        Self(path)
    }

    fn run(&self, shell: &str, env: &[(&str, &str)]) -> Output {
        Command::new("/bin/bash")
            .arg("-c")
            .arg(shell)
            .env_clear()
            .env("PATH", FIXTURE_TOOL_PATH)
            .env("TV_FIXTURE", &self.0)
            .env("RUNNER_TEMP", &self.0)
            .env("GITHUB_OUTPUT", self.0.join("outputs"))
            .env("GITHUB_STEP_SUMMARY", self.0.join("summary"))
            .envs(env.iter().copied())
            .output()
            .expect("execute actual gate with synthetic dependencies")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open_pr() -> Value {
    json!({
        "state": "open", "draft": false,
        "head": {"sha": SHA, "repo": {"full_name": "owner/project"}},
        "base": {"ref": "main", "repo": {"full_name": "owner/project"}}
    })
}

fn check(id: u64, name: &str, status: &str, conclusion: Value) -> Value {
    json!({
        "id": id, "name": name, "head_sha": SHA, "status": status,
        "conclusion": conclusion, "app": {"slug": "github-actions"}
    })
}

fn passing_checks() -> Vec<Value> {
    vec![
        check(10, "All Green", "completed", json!("success")),
        check(20, RELEASE_CHECK, "completed", json!("success")),
    ]
}

const GH_STUB: &str = r#"
gh() {
  if [ "$#" = 2 ] && [ "$1" = api ] && [ "$2" = repos/owner/project/pulls/42 ]; then
    if [ -e "$TV_FIXTURE/pr-read" ]; then
      printf '%s' "$TV_PR_RECHECK"
    else
      : > "$TV_FIXTURE/pr-read"
      printf '%s' "$TV_PR"
    fi
  elif [ "$#" = 4 ] && [ "$1" = api ] && [ "$2" = --paginate ] && [ "$3" = --slurp ] && [ "$4" = "repos/owner/project/commits/$TV_SHA/check-runs?filter=all&per_page=100" ]; then
    printf '%s' "$TV_CHECKS"
  elif [ "$#" = 7 ] && [ "$1" = pr ] && [ "$2" = merge ] && [ "$3" = --auto ] && [ "$4" = --squash ] && [ "$5" = --match-head-commit ] && [ "$6" = "$TV_SHA" ] && [ "$7" = https://github.com/owner/project/pull/42 ]; then
    printf '%s\n' "$@" > "$TV_FIXTURE/merge"
  else
    echo "unexpected gh arguments" >&2
    return 91
  fi
}
"#;

fn run_merge_case(
    case: &str,
    pr: &Value,
    recheck: &Value,
    checks: &[Value],
    expected_head: &str,
    accept: bool,
) {
    let fixture = Fixture::new();
    let run = script(
        &job(&workflow("auto-merge.yml"), "auto-merge"),
        "Verify same-repo origin + non-draft, then arm auto-merge (squash)",
    );
    assert!(!run.contains("${{"), "API values must enter through env");
    // Deliberately split the two checks across pages. The actual --paginate
    // --slurp API response must be consumed as pages, not only the final page.
    let pages = json!([
        {"check_runs": checks.iter().take(1).collect::<Vec<_>>()},
        {"check_runs": checks.iter().skip(1).collect::<Vec<_>>()}
    ]);
    let output = fixture.run(
        &format!("{GH_STUB}\n{run}"),
        &[
            ("PR_NUMBER", "42"),
            ("EXPECTED_HEAD_SHA", expected_head),
            ("REPO", "owner/project"),
            ("TV_SHA", SHA),
            ("TV_PR", &pr.to_string()),
            ("TV_PR_RECHECK", &recheck.to_string()),
            ("TV_CHECKS", &pages.to_string()),
        ],
    );
    assert_eq!(
        output.status.success(),
        accept,
        "{case}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.0.join("merge").exists(), accept, "{case}");
}

#[test]
fn auto_merge_requires_both_latest_completed_checks_on_the_current_head() {
    let pr = open_pr();
    run_merge_case("automatic success", &pr, &pr, &passing_checks(), SHA, true);
    run_merge_case("manual success", &pr, &pr, &passing_checks(), "", true);
    for target in ["All Green", RELEASE_CHECK] {
        for (status, conclusion) in [
            ("completed", json!("failure")),
            ("completed", json!("cancelled")),
            ("completed", json!("timed_out")),
            ("completed", json!("skipped")),
            ("completed", json!("neutral")),
            ("completed", Value::Null),
            ("queued", Value::Null),
            ("in_progress", Value::Null),
        ] {
            let mut checks = passing_checks();
            // A newer unsuccessful attempt must win over the old success.
            checks.push(check(30, target, status, conclusion));
            run_merge_case(target, &pr, &pr, &checks, SHA, false);
        }
        let mut missing = passing_checks();
        missing.retain(|entry| entry["name"] != target);
        run_merge_case("missing check", &pr, &pr, &missing, SHA, false);
        let mut stale = passing_checks();
        for entry in &mut stale {
            if entry["name"] == target {
                entry["head_sha"] = json!(OTHER_SHA);
            }
        }
        run_merge_case("stale check", &pr, &pr, &stale, SHA, false);
        let mut foreign = passing_checks();
        for entry in &mut foreign {
            if entry["name"] == target {
                entry["app"]["slug"] = json!("untrusted-check-app");
            }
        }
        run_merge_case("foreign app", &pr, &pr, &foreign, SHA, false);
    }
    let mut unrelated = passing_checks();
    unrelated.push(check(40, "Unrelated job", "completed", json!("failure")));
    run_merge_case("unrelated check", &pr, &pr, &unrelated, SHA, true);
}

#[test]
fn auto_merge_rechecks_origin_draft_state_and_head_before_the_merge_request() {
    let pr = open_pr();
    let mut variants = Vec::new();
    let mut fork = pr.clone();
    fork["head"]["repo"]["full_name"] = json!("fork/project");
    variants.push(fork);
    let mut draft = pr.clone();
    draft["draft"] = json!(true);
    variants.push(draft);
    let mut closed = pr.clone();
    closed["state"] = json!("closed");
    variants.push(closed);
    let mut retargeted = pr.clone();
    retargeted["base"]["ref"] = json!("other-branch");
    variants.push(retargeted);
    for changed in variants {
        run_merge_case(
            "initial eligibility",
            &changed,
            &changed,
            &passing_checks(),
            SHA,
            false,
        );
        run_merge_case(
            "eligibility changed",
            &pr,
            &changed,
            &passing_checks(),
            SHA,
            false,
        );
    }
    let mut pushed = pr.clone();
    pushed["head"]["sha"] = json!(OTHER_SHA);
    run_merge_case("head changed", &pr, &pushed, &passing_checks(), SHA, false);
    run_merge_case(
        "caller head stale",
        &pr,
        &pr,
        &passing_checks(),
        OTHER_SHA,
        false,
    );
}

#[test]
fn release_fan_in_requires_success_and_identical_commit_and_tree() {
    let run = script(
        &job(&workflow("release-validation.yml"), "release-validation"),
        "Require both successful jobs on the same exact candidate tree",
    );
    let cases = [
        ("success", "success", SHA, SHA, TREE, TREE, true),
        ("failure", "success", SHA, SHA, TREE, TREE, false),
        ("success", "failure", SHA, SHA, TREE, TREE, false),
        ("cancelled", "success", SHA, SHA, TREE, TREE, false),
        ("success", "skipped", SHA, SHA, TREE, TREE, false),
        ("success", "success", OTHER_SHA, SHA, TREE, TREE, false),
        ("success", "success", SHA, OTHER_SHA, TREE, TREE, false),
        ("success", "success", SHA, SHA, TREE, OTHER_SHA, false),
        ("success", "success", SHA, SHA, "", "", false),
        ("success", "success", "", "", TREE, TREE, false),
    ];
    for (arm, qdb, arm_sha, qdb_sha, arm_tree, qdb_tree, accept) in cases {
        let fixture = Fixture::new();
        let output = fixture.run(
            &run,
            &[
                ("SOURCE_SHA", SHA),
                ("ARM_RESULT", arm),
                ("QDB_RESULT", qdb),
                ("ARM_SHA", arm_sha),
                ("QDB_SHA", qdb_sha),
                ("ARM_TREE", arm_tree),
                ("QDB_TREE", qdb_tree),
            ],
        );
        assert_eq!(
            output.status.success(),
            accept,
            "{arm}/{qdb} source={SHA} arm_sha={arm_sha:?} qdb_sha={qdb_sha:?} arm_tree={arm_tree:?} qdb_tree={qdb_tree:?}: {output:?}"
        );
        assert_eq!(fixture.0.join("outputs").exists(), accept);
        if accept {
            assert_eq!(
                std::fs::read_to_string(fixture.0.join("outputs")).expect("validated outputs"),
                format!("sha={SHA}\ntree={TREE}\n")
            );
        }
    }
}

#[test]
fn release_validation_is_bound_to_source_and_required_by_every_merge_and_deploy_entry() {
    let release = workflow("release-validation.yml");
    assert!(release.contains("  workflow_call:\n    inputs:\n      source_sha:"));
    assert!(release.contains("required: true\n        type: string"));
    assert!(release.contains(
        "SOURCE_SHA: ${{ inputs.source_sha || github.event.pull_request.head.sha || github.sha }}"
    ));
    assert_eq!(release.matches("ref: ${{ env.SOURCE_SHA }}").count(), 2);
    assert_eq!(
        release
            .matches("test \"$(git rev-parse HEAD)\" = \"$SOURCE_SHA\"")
            .count(),
        2
    );
    assert!(release.contains("TICKVAULT_BUILD_GIT_SHA: ${{ env.SOURCE_SHA }}"));
    assert!(release.contains("name: release-arm64-evidence-${{ env.SOURCE_SHA }}"));
    assert!(release.contains("name: release-questdb-evidence-${{ env.SOURCE_SHA }}"));
    assert!(release.contains("format('called-{0}', github.run_id)"));
    assert!(release.contains("format('standalone-{0}', github.ref)"));
    assert!(
        release.contains(r#"cancel-in-progress: ${{ !contains(toJSON(inputs), '"source_sha"') }}"#)
    );
    assert!(!release.contains("continue-on-error:"));

    let deploy = workflow("deploy-aws.yml");
    let validation = job(&deploy, "release_validation");
    assert!(validation.contains("uses: ./.github/workflows/release-validation.yml"));
    assert!(validation.contains("source_sha: ${{ github.sha }}"));
    let deployment = job(&deploy, "deploy");
    assert!(
        deployment.contains("needs: [preflight, build, guard_market_hours, release_validation]")
    );
    assert!(deployment.contains("needs.release_validation.result == 'success'"));
    assert!(deployment.contains("needs.preflight.outputs.bootstrap_ready == 'true'"));
    assert!(deployment.contains("needs.guard_market_hours.outputs.deploy_allowed == 'true'"));
    let provenance = deployment
        .find("Require release validation for this deployment source")
        .expect("source gate");
    let credentials = deployment
        .find("Configure AWS credentials")
        .expect("AWS step");
    assert!(
        provenance < credentials,
        "refuse unvalidated source before AWS credentials/publication"
    );

    let ci = workflow("ci.yml");
    let ci_release = job(&ci, "release-validation");
    assert!(ci_release.contains("name: Release Validation"));
    assert!(ci_release.contains("uses: ./.github/workflows/release-validation.yml"));
    assert!(
        ci_release.contains("source_sha: ${{ github.event.pull_request.head.sha || github.sha }}")
    );
    let all_green = job(&ci, "all-green");
    for required in [
        "local-runtime-block",
        "commit-lint",
        "design-first-wall",
        "deploy-lint",
        "build-and-verify",
        "test",
        "security-and-audit",
        "coverage-and-perf",
        "repo-guards",
        "dhat-zero-alloc",
        "loom-concurrency",
        "release-validation",
    ] {
        assert!(
            all_green.contains(&format!("      - {required}\n")),
            "required CI job {required}"
        );
    }
    let merge = job(&ci, "enable-auto-merge");
    assert!(merge.contains("needs: all-green"));
    assert!(merge.contains("needs.all-green.result == 'success'"));
    assert!(merge.contains("github.event.pull_request.draft != true"));
    assert!(merge.contains("github.event.pull_request.head.repo.full_name == github.repository"));
    for actor in [
        "dependabot[bot]",
        "renovate[bot]",
        "SJParthi",
        "startsWith(github.head_ref, 'claude/')",
    ] {
        assert!(
            merge.contains(actor),
            "preserve caller eligibility: {actor}"
        );
    }
    assert!(merge.contains("uses: ./.github/workflows/auto-merge.yml"));
    assert!(merge.contains("source_sha: ${{ github.event.pull_request.head.sha }}"));
    assert!(merge.contains("checks: read"));
    assert!(
        !merge.contains("gh pr merge"),
        "both entry points must run the shared gate"
    );
}

#[test]
fn reusable_validation_rejects_empty_or_non_commit_inputs_before_recording_evidence() {
    let stub = r#"
git() {
  case "$*" in
    'rev-parse HEAD') printf '%s' "$TV_CHECKOUT_SHA" ;;
    'rev-parse HEAD^{tree}') printf '%s' "$TV_CHECKOUT_TREE" ;;
    *) return 91 ;;
  esac
}
"#;
    for name in ["arm64", "questdb"] {
        let run = script(
            &job(&workflow("release-validation.yml"), name),
            "Record and verify source identity",
        );
        for (present, requested, checkout, accept) in [
            ("true", SHA, SHA, true),
            ("false", "", SHA, true),
            ("true", "", SHA, false),
            ("true", "main", SHA, false),
            ("true", OTHER_SHA, SHA, false),
            ("true", SHA, OTHER_SHA, false),
        ] {
            let fixture = Fixture::new();
            let output = fixture.run(
                &format!("{stub}\n{run}"),
                &[
                    ("SOURCE_SHA", SHA),
                    ("SOURCE_INPUT_PRESENT", present),
                    ("REQUESTED_SOURCE_SHA", requested),
                    ("TV_CHECKOUT_SHA", checkout),
                    ("TV_CHECKOUT_TREE", TREE),
                ],
            );
            assert_eq!(output.status.success(), accept, "{name}: {output:?}");
            assert_eq!(fixture.0.join("outputs").exists(), accept);
        }
    }
}

#[test]
fn deployment_rejects_source_or_tree_mismatch_before_credentials() {
    let run = script(
        &job(&workflow("deploy-aws.yml"), "deploy"),
        "Require release validation for this deployment source",
    );
    let stub = r#"
git() {
  case "$*" in
    'rev-parse HEAD') printf '%s' "$TV_CHECKOUT_SHA" ;;
    'rev-parse HEAD^{tree}') printf '%s' "$TV_CHECKOUT_TREE" ;;
    *) return 91 ;;
  esac
}
"#;
    for (validated, validated_tree, checkout, checkout_tree, accept) in [
        (SHA, TREE, SHA, TREE, true),
        (OTHER_SHA, TREE, SHA, TREE, false),
        (SHA, OTHER_SHA, SHA, TREE, false),
        (SHA, TREE, OTHER_SHA, TREE, false),
        (SHA, "", SHA, TREE, false),
        ("", TREE, SHA, TREE, false),
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(
            &format!("{stub}\n{run}"),
            &[
                ("GITHUB_SHA", SHA),
                ("VALIDATED_SHA", validated),
                ("VALIDATED_TREE", validated_tree),
                ("TV_CHECKOUT_SHA", checkout),
                ("TV_CHECKOUT_TREE", checkout_tree),
            ],
        );
        assert_eq!(output.status.success(), accept, "{output:?}");
    }
}
