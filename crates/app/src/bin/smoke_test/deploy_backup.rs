//! Private pre-overwrite deployment snapshot. Cold path, O(total file bytes),
//! bounded to 128 entries and 256 MiB. No provider or application activation.
//! Copies remain root-only; the private manifest retains original ownership,
//! modes and timestamps. This is preservation, not automatic/atomic rollback.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_COMMAND_BYTES: u64 = 64 * 1024;
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;
const FIXED: &[&str] = &[
    "opt/tickvault/bin/tickvault",
    "opt/tickvault/bin/tv-movers",
    "opt/tickvault/bin/smoke_test",
    "opt/tickvault/bin/tickvault-host",
    "opt/tickvault/config/base.toml",
    "opt/tickvault/config/production.toml",
    "opt/tickvault/config/local.toml",
    "opt/tickvault/repo/deploy/docker/.env",
    "etc/systemd/system/tickvault.service",
    "etc/systemd/system/tickvault-host-tuning.service",
    "etc/systemd/system/tickvault-holiday-gate.service",
];
const DIRECTORIES: &[(&str, &str)] = &[
    ("opt/tickvault/config", "toml"),
    ("etc/systemd/system/tickvault.service.d", "conf"),
    ("etc/systemd/system/tickvault-host-tuning.service.d", "conf"),
    (
        "etc/systemd/system/tickvault-holiday-gate.service.d",
        "conf",
    ),
];
type Result<T> = std::result::Result<T, String>;

fn failure(reason: &str) -> String {
    // Callers never attach paths, file contents or command stderr.
    format!("DEPLOY_BACKUP: {reason}; deployment must stop")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl From<&fs::Metadata> for Stamp {
    fn from(m: &fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}

fn checked_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(failure("unsafe path"));
        }
        prefix.push(component);
        match fs::symlink_metadata(&prefix) {
            Ok(m) if m.file_type().is_symlink() => return Err(failure("symlink refused")),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(failure("source metadata unavailable")),
        }
    }
    fs::symlink_metadata(path)
        .map(Some)
        .map_err(|_| failure("source metadata unavailable"))
}

fn private_directory(path: &Path, owner: u32) -> Result<()> {
    match checked_metadata(path)? {
        Some(m) if m.is_dir() && m.uid() == owner && m.mode() & 0o777 == 0o700 => Ok(()),
        Some(_) => Err(failure("unsafe backup directory")),
        None => fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| failure("cannot create private backup directory")),
    }
}

fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| failure("cannot create backup record"))?;
    file.write_all(body)
        .and_then(|()| file.sync_all())
        .map_err(|_| failure("cannot persist backup record"))
}

/// All external tools are local and bounded; stderr is deliberately withheld.
fn output(command: Command) -> Result<Vec<u8>> {
    output_capped(command, MAX_COMMAND_BYTES)
}

fn output_capped(mut command: Command, limit: u64) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| failure("required local tool unavailable"))?;
    let Some(stdout) = child.stdout.take() else {
        // Every caller runs through timeout. Keep that supervisor alive so
        // its child cannot outlive the deadline when stdout is unavailable.
        let _ = child.wait();
        return Err(failure("local tool output unavailable"));
    };
    let mut bytes = Vec::new();
    if stdout.take(limit + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > limit {
        // Dropping the capped reader closes the pipe. Await the existing
        // bounded supervisor rather than killing it and orphaning its child.
        let _ = child.wait();
        return Err(failure("local tool output refused"));
    }
    if !child
        .wait()
        .map_err(|_| failure("local tool wait failed"))?
        .success()
    {
        return Err(failure("local tool failed"));
    }
    Ok(bytes)
}

fn git_command(repo: &Path, user: Option<&str>, args: &[&str]) -> Command {
    let mut command = Command::new("timeout");
    command.args(["--kill-after=5", "20"]);
    if let Some(user) = user {
        command.args(["sudo", "-u", user]);
    }
    command.arg("git").arg("-C").arg(repo).args(args);
    // A native Git hook may export these selectors. -C alone does not isolate
    // the target. Preserve configuration/auth/proxy/CA variables unchanged.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
    command
}

fn git(repo: &Path, user: Option<&str>, args: &[&str]) -> Result<Vec<u8>> {
    output(git_command(repo, user, args))
}

fn clean_head(repo: &Path, user: Option<&str>) -> Result<Option<String>> {
    if checked_metadata(repo)?.is_none() {
        return Ok(None);
    }
    if !checked_metadata(&repo.join(".git"))?.is_some_and(|m| m.is_dir()) {
        return Err(failure("repository identity unavailable"));
    }
    // Porcelain status alone can hide edits behind assume-unchanged or
    // skip-worktree. Refuse those flags instead of resetting hidden work.
    let entries = output_capped(
        git_command(repo, user, &["ls-files", "-v", "-z"]),
        MAX_INDEX_BYTES,
    )?;
    if (!entries.is_empty() && entries.last() != Some(&0))
        || entries
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .any(|entry| !entry.starts_with(b"H ") || entry.len() < 3)
    {
        return Err(failure(
            "hidden or unsupported index flags require preservation",
        ));
    }
    if !git(
        repo,
        user,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?
    .is_empty()
    {
        return Err(failure(
            "unpublished working-tree changes require preservation",
        ));
    }
    let bytes = git(repo, user, &["rev-parse", "--verify", "HEAD"])?;
    let sha = std::str::from_utf8(&bytes)
        .map_err(|_| failure("repository identity invalid"))?
        .trim();
    if sha.len() != 40
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(failure("repository identity invalid"));
    }
    Ok(Some(sha.to_string()))
}

fn inventory(root: &Path) -> Result<Vec<(String, Option<Stamp>)>> {
    let mut names: std::collections::BTreeSet<String> =
        FIXED.iter().map(|name| (*name).to_string()).collect();
    let mut inspected = 0;
    for (directory, extension) in DIRECTORIES {
        let path = root.join(directory);
        let Some(metadata) = checked_metadata(&path)? else {
            continue;
        };
        if !metadata.is_dir() {
            return Err(failure("source directory invalid"));
        }
        for entry in fs::read_dir(&path).map_err(|_| failure("source directory unreadable"))? {
            inspected += 1;
            if inspected > MAX_ENTRIES {
                return Err(failure("directory entry limit exceeded"));
            }
            let entry = entry.map_err(|_| failure("source entry unreadable"))?;
            if entry
                .path()
                .extension()
                .is_some_and(|value| value == *extension)
            {
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| failure("source filename invalid"))?;
                if !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                {
                    return Err(failure("source filename invalid"));
                }
                names.insert(format!("{directory}/{name}"));
            }
        }
    }
    if names.len() > MAX_ENTRIES {
        return Err(failure("file count limit exceeded"));
    }
    let mut total = 0_u64;
    names
        .into_iter()
        .map(|name| {
            let stamp = match checked_metadata(&root.join(&name))? {
                Some(m) if m.is_file() => {
                    total = total
                        .checked_add(m.len())
                        .ok_or_else(|| failure("byte limit exceeded"))?;
                    if total > MAX_BYTES {
                        return Err(failure("byte limit exceeded"));
                    }
                    Some(Stamp::from(&m))
                }
                Some(_) => return Err(failure("nonregular source refused")),
                None => None,
            };
            Ok((name, stamp))
        })
        .collect()
}

fn copy_one(source: &Path, destination: &Path, expected: &Stamp) -> Result<()> {
    let Some(metadata) = checked_metadata(source)? else {
        return Err(failure("source disappeared"));
    };
    if Stamp::from(&metadata) != *expected || !metadata.is_file() {
        return Err(failure("source changed before capture"));
    }
    let mut input = File::open(source).map_err(|_| failure("source unreadable"))?;
    if Stamp::from(
        &input
            .metadata()
            .map_err(|_| failure("source metadata unavailable"))?,
    ) != *expected
    {
        return Err(failure("source replaced before capture"));
    }
    if let Some(parent) = destination.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| failure("cannot create snapshot layout"))?;
    }
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .map_err(|_| failure("cannot create snapshot file"))?;
    let written = std::io::copy(
        &mut Read::by_ref(&mut input).take(expected.len + 1),
        &mut target,
    )
    .map_err(|_| failure("snapshot copy failed"))?;
    if written != expected.len
        || Stamp::from(
            &input
                .metadata()
                .map_err(|_| failure("source metadata unavailable"))?,
        ) != *expected
    {
        return Err(failure("source changed during capture"));
    }
    target
        .sync_all()
        .map_err(|_| failure("snapshot sync failed"))
}

fn preserve(root: &Path, owner: u32, git_user: Option<&str>, nonce: &str) -> Result<PathBuf> {
    if nonce.is_empty()
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(failure("snapshot identity invalid"));
    }
    let repo = root.join("opt/tickvault/repo");
    let old_head = clean_head(&repo, git_user)?;
    let before = inventory(root)?;
    let parent = root.join("var/lib/tickvault-deploy-backups");
    private_directory(&parent, owner)?;
    let destination = parent.join(nonce);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&destination)
        .map_err(|_| failure("snapshot destination already exists or cannot be created"))?;
    let mut records = Vec::new();
    let mut hashes = Command::new("timeout");
    hashes
        .args(["--kill-after=5", "30", "sha256sum", "--"])
        .current_dir(&destination);
    for (name, stamp) in &before {
        if let Some(stamp) = stamp {
            let relative = format!("files/{name}");
            copy_one(&root.join(name), &destination.join(&relative), stamp)?;
            hashes.arg(relative);
            records.push(serde_json::json!({
                "path":name,"present":true,"bytes":stamp.len,
                "mode":stamp.mode,"uid":stamp.uid,"gid":stamp.gid,
                "mtime_secs":stamp.mtime.0,"mtime_nanos":stamp.mtime.1,
                "ctime_secs":stamp.ctime.0,"ctime_nanos":stamp.ctime.1,
                "device":stamp.dev,"inode":stamp.ino
            }));
        } else {
            records.push(serde_json::json!({"path":name,"present":false}));
        }
    }
    if before != inventory(root)? || old_head != clean_head(&repo, git_user)? {
        return Err(failure("source set changed during capture"));
    }
    let reference = format!("refs/tickvault-deploy-backups/{nonce}");
    if let Some(sha) = &old_head {
        git(
            &repo,
            git_user,
            &[
                "update-ref",
                &reference,
                sha,
                "0000000000000000000000000000000000000000",
            ],
        )?;
        let retained = git(&repo, git_user, &["rev-parse", "--verify", &reference])?;
        if retained != format!("{sha}\n").as_bytes() {
            return Err(failure("prior source revision was not retained"));
        }
    }
    let manifest = serde_json::json!({
        "version":1,"source_head":old_head,"source_ref":old_head.as_ref().map(|_|&reference),
        "limits":{"max_entries":MAX_ENTRIES,"max_total_bytes":MAX_BYTES},
        "files":records,
        "scope":"fixed deployment files and unit drop-ins; ignored untracked source outside this inventory is not archived; no atomic or power-loss rollback guarantee"
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|_| failure("manifest serialization failed"))?;
    write_private(&destination.join("manifest.json"), &manifest_bytes)?;
    hashes.arg("manifest.json");
    let checksums = output(hashes)?;
    write_private(&destination.join("SHA256SUMS"), &checksums)?;
    write_private(
        &destination.join("COMPLETE"),
        b"private deployment snapshot complete\n",
    )?;
    if File::open(&destination)
        .and_then(|file| file.sync_all())
        .is_err()
    {
        let _ = fs::remove_file(destination.join("COMPLETE"));
        return Err(failure("snapshot directory sync failed"));
    }
    Ok(destination)
}

pub(super) fn preserve_deployment(
    root: &Path,
    owner: u32,
    git_user: Option<&str>,
) -> Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| failure("clock unavailable"))?;
    let nonce = format!("deploy-{}-{}", now.as_nanos(), std::process::id());
    preserve(root, owner, git_user, &nonce)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let name = format!(
                "tickvault-backup-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(name);
            fs::create_dir(&path).unwrap();
            for directory in ["var/lib", "opt/tickvault/config", "opt/tickvault/bin"] {
                fs::create_dir_all(path.join(directory)).unwrap();
            }
            Self(path)
        }
        fn owner(&self) -> u32 {
            fs::metadata(&self.0).unwrap().uid()
        }
        fn file(&self, name: &str, data: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, data).unwrap();
            path
        }
        fn backup(&self) -> Result<PathBuf> {
            preserve(&self.0, self.owner(), None, "fixture")
        }
        fn repository(&self) -> (PathBuf, String) {
            let repo = self.0.join("opt/tickvault/repo");
            fs::create_dir_all(&repo).unwrap();
            git(&repo, None, &["init", "--quiet"]).unwrap();
            fs::write(repo.join("tracked"), b"original").unwrap();
            git(&repo, None, &["add", "tracked"]).unwrap();
            let tree = String::from_utf8(git(&repo, None, &["write-tree"]).unwrap()).unwrap();
            let mut commit = git_command(
                &repo,
                None,
                &["commit-tree", tree.trim(), "-m", "owned fixture"],
            );
            commit
                .env("GIT_AUTHOR_NAME", "fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
            let sha = String::from_utf8(output(commit).unwrap())
                .unwrap()
                .trim()
                .to_string();
            git(&repo, None, &["update-ref", "refs/heads/fixture", &sha]).unwrap();
            git(&repo, None, &["symbolic-ref", "HEAD", "refs/heads/fixture"]).unwrap();
            (repo, sha)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn test_preserve_deployment_generates_private_snapshot_with_complete_manifest() {
        let fixture = Fixture::new();
        fixture.file("opt/tickvault/config/base.toml", b"old-config");
        let first = preserve_deployment(&fixture.0, fixture.owner(), None).unwrap();
        let second = preserve_deployment(&fixture.0, fixture.owner(), None).unwrap();
        assert_ne!(first, second);
        assert!(first.join("COMPLETE").exists());
        assert!(second.join("manifest.json").exists());
        assert_eq!(
            fs::read(first.join("files/opt/tickvault/config/base.toml")).unwrap(),
            b"old-config"
        );
    }
    #[test]
    fn test_regression_snapshot_preserves_bytes_metadata_and_absence_privately() {
        let fixture = Fixture::new();
        let source = fixture.file(
            "opt/tickvault/config/production.toml",
            b"SECRET_FIXTURE_DO_NOT_PRINT",
        );
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        let destination = fixture.backup().unwrap();
        assert_eq!(
            fs::read(destination.join("files/opt/tickvault/config/production.toml")).unwrap(),
            fs::read(&source).unwrap()
        );
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(destination.join("manifest.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(destination.join("manifest.json")).unwrap()).unwrap();
        let rows = manifest["files"].as_array().unwrap();
        assert!(
            rows.iter()
                .any(|row| row["path"] == "opt/tickvault/config/production.toml"
                    && row["mode"].as_u64().unwrap() & 0o777 == 0o640)
        );
        assert!(
            rows.iter()
                .any(|row| row["path"] == "opt/tickvault/bin/tv-movers" && row["present"] == false)
        );
        let mut verify = Command::new("timeout");
        verify
            .args(["10", "sha256sum", "--check", "SHA256SUMS"])
            .current_dir(&destination);
        assert!(!output(verify).unwrap().is_empty());
        assert!(destination.join("COMPLETE").is_file());
    }
    #[test]
    fn test_regression_symlink_source_or_parent_is_refused_without_completion() {
        for parent_link in [false, true] {
            let fixture = Fixture::new();
            let outside = fixture.file("outside", b"private");
            let source = fixture.0.join("opt/tickvault/config/local.toml");
            if parent_link {
                fs::remove_dir(fixture.0.join("opt/tickvault/config")).unwrap();
                std::os::unix::fs::symlink(
                    fixture.0.join("opt/tickvault/bin"),
                    fixture.0.join("opt/tickvault/config"),
                )
                .unwrap();
            } else {
                std::os::unix::fs::symlink(&outside, &source).unwrap();
            }
            assert!(fixture.backup().unwrap_err().contains("symlink"));
            assert_eq!(fs::read(outside).unwrap(), b"private");
            assert!(
                !fixture
                    .0
                    .join("var/lib/tickvault-deploy-backups/fixture/COMPLETE")
                    .exists()
            );
        }
    }
    #[test]
    fn test_regression_caps_and_private_destination_fail_before_overwrite() {
        let fixture = Fixture::new();
        let large = fixture.file("opt/tickvault/bin/tickvault", b"");
        File::options()
            .write(true)
            .open(&large)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(fixture.backup().unwrap_err().contains("byte limit"));
        fs::remove_file(large).unwrap();
        for n in 0..=MAX_ENTRIES {
            fixture.file(&format!("opt/tickvault/config/{n}.toml"), b"x");
        }
        assert!(fixture.backup().unwrap_err().contains("entry limit"));
        let fixture = Fixture::new();
        let parent = fixture.0.join("var/lib/tickvault-deploy-backups");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(fixture.backup().unwrap_err().contains("unsafe backup"));
    }
    #[test]
    fn test_regression_snapshot_collision_preserves_previous_complete_copy() {
        let fixture = Fixture::new();
        fixture.file("opt/tickvault/config/base.toml", b"old");
        let first = fixture.backup().unwrap();
        fixture.file("opt/tickvault/config/base.toml", b"new");
        assert!(fixture.backup().is_err());
        assert_eq!(
            fs::read(first.join("files/opt/tickvault/config/base.toml")).unwrap(),
            b"old"
        );
        assert!(first.join("COMPLETE").exists());
    }
    #[test]
    fn test_regression_replaced_source_and_nonregular_input_fail_closed() {
        let fixture = Fixture::new();
        let source = fixture.file("opt/tickvault/config/base.toml", b"old");
        let stamp = Stamp::from(&fs::metadata(&source).unwrap());
        fs::remove_file(&source).unwrap();
        fixture.file("opt/tickvault/config/base.toml", b"newer");
        assert!(copy_one(&source, &fixture.0.join("copy"), &stamp).is_err());
        assert!(!fixture.0.join("copy").exists());
        fs::remove_file(source).unwrap();
        fs::create_dir(fixture.0.join("opt/tickvault/config/base.toml")).unwrap();
        assert!(fixture.backup().unwrap_err().contains("nonregular"));
    }

    #[test]
    fn test_regression_unpublished_changes_refuse_before_capture() {
        for state in ["untracked", "unstaged", "staged"] {
            let fixture = Fixture::new();
            let (repo, original) = fixture.repository();
            if state == "untracked" {
                fs::write(repo.join("private-untracked"), b"UNPUBLISHED_SECRET").unwrap();
            } else {
                fs::write(repo.join("tracked"), b"UNPUBLISHED_SECRET").unwrap();
                if state == "staged" {
                    git(&repo, None, &["add", "tracked"]).unwrap();
                }
            }
            let error = fixture.backup().unwrap_err();
            assert!(
                error.contains("unpublished working-tree"),
                "{state}: {error}"
            );
            assert!(!error.contains("UNPUBLISHED_SECRET"));
            assert!(
                !fixture
                    .0
                    .join("var/lib/tickvault-deploy-backups/fixture")
                    .exists()
            );
            assert_eq!(
                git(&repo, None, &["rev-parse", "HEAD"]).unwrap(),
                format!("{original}\n").as_bytes()
            );
        }
    }

    #[test]
    fn test_regression_prior_commit_is_retained_and_reference_collision_refuses() {
        let fixture = Fixture::new();
        let (repo, original) = fixture.repository();
        let snapshot = fixture.backup().unwrap();
        assert_eq!(
            git(
                &repo,
                None,
                &["rev-parse", "refs/tickvault-deploy-backups/fixture"]
            )
            .unwrap(),
            format!("{original}\n").as_bytes()
        );
        assert!(snapshot.join("COMPLETE").exists());
        let fixture = Fixture::new();
        let (repo, original) = fixture.repository();
        git(
            &repo,
            None,
            &[
                "update-ref",
                "refs/tickvault-deploy-backups/fixture",
                &original,
            ],
        )
        .unwrap();
        assert!(fixture.backup().is_err());
        assert!(
            !fixture
                .0
                .join("var/lib/tickvault-deploy-backups/fixture/COMPLETE")
                .exists()
        );
        assert_eq!(
            git(
                &repo,
                None,
                &["rev-parse", "refs/tickvault-deploy-backups/fixture"]
            )
            .unwrap(),
            format!("{original}\n").as_bytes()
        );
    }

    #[test]
    fn test_regression_hidden_index_changes_are_refused_and_left_intact() {
        for flag in ["--assume-unchanged", "--skip-worktree"] {
            let fixture = Fixture::new();
            let (repo, original) = fixture.repository();
            git(&repo, None, &["update-index", flag, "tracked"]).unwrap();
            fs::write(repo.join("tracked"), b"hidden unpublished work").unwrap();
            assert!(
                git(&repo, None, &["status", "--porcelain=v1"])
                    .unwrap()
                    .is_empty()
            );
            let error = fixture.backup().unwrap_err();
            assert!(error.contains("index flags"));
            assert_eq!(
                fs::read(repo.join("tracked")).unwrap(),
                b"hidden unpublished work"
            );
            assert_eq!(
                git(&repo, None, &["rev-parse", "HEAD"]).unwrap(),
                format!("{original}\n").as_bytes()
            );
            assert!(!fixture.0.join("var/lib/tickvault-deploy-backups").exists());
        }
    }

    #[test]
    fn test_regression_inherited_git_selectors_cannot_redirect_backup_fixtures() {
        let outside = Fixture::new();
        let (repo, original) = outside.repository();
        let before_index = fs::read(repo.join(".git/index")).unwrap();
        let mut command = Command::new("timeout");
        command.args(["--kill-after=2", "15"])
            .arg(std::env::current_exe().unwrap())
            .args(["deploy_backup::tests::test_regression_prior_commit_is_retained_and_reference_collision_refuses", "--exact", "--test-threads=1"])
            .env("GIT_DIR", repo.join(".git"))
            .env("GIT_WORK_TREE", &repo)
            .env("GIT_INDEX_FILE", repo.join(".git/index"))
            .env("GIT_COMMON_DIR", repo.join(".git"))
            .env("GIT_OBJECT_DIRECTORY", repo.join(".git/objects"))
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", repo.join(".git/objects"));
        let result = output(command).unwrap();
        assert!(
            String::from_utf8(result)
                .unwrap()
                .contains("1 passed; 0 failed")
        );
        assert_eq!(fs::read(repo.join("tracked")).unwrap(), b"original");
        assert_eq!(fs::read(repo.join(".git/index")).unwrap(), before_index);
        assert_eq!(
            git(&repo, None, &["rev-parse", "HEAD"]).unwrap(),
            format!("{original}\n").as_bytes()
        );
        assert!(
            git(
                &repo,
                None,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/tickvault-deploy-backups/"
                ]
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn test_regression_local_tool_failures_and_output_caps_do_not_leak_content() {
        let mut success = Command::new("timeout");
        success.args(["--kill-after=1", "2", "printf", "owned-output"]);
        assert_eq!(output(success).unwrap(), b"owned-output");
        let mut failed = Command::new("timeout");
        failed.args(["--kill-after=1", "2", "false"]);
        assert_eq!(output(failed).unwrap_err(), failure("local tool failed"));
        let mut oversized = Command::new("timeout");
        oversized.args(["--kill-after=1", "2", "head", "-c", "65537", "/dev/zero"]);
        assert_eq!(
            output(oversized).unwrap_err(),
            failure("local tool output refused")
        );
    }

    #[test]
    fn test_regression_backup_parent_symlink_and_unreadable_repo_fail_closed() {
        let fixture = Fixture::new();
        let outside = fixture.0.join("outside-private");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.0.join("var/lib/tickvault-deploy-backups"))
            .unwrap();
        assert!(fixture.backup().unwrap_err().contains("symlink"));
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("opt/tickvault/repo/.git")).unwrap();
        assert!(fixture.backup().is_err());
        assert!(
            !fixture
                .0
                .join("var/lib/tickvault-deploy-backups/fixture/COMPLETE")
                .exists()
        );
    }
}
