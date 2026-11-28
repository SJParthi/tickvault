use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    env,
    error::Error,
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, TableLike, Value};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Change {
    name: String,
    pinned: String,
    latest: String,
}
impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: pinned={} latest={}",
            self.name, self.pinned, self.latest
        )
    }
}

#[derive(Default)]
struct Report {
    fresh: usize,
    patch: Vec<Change>,
    minor: Vec<Change>,
    major: Vec<Change>,
    unknown: Vec<String>,
}
impl Report {
    fn print(&self, json: bool) -> Result<()> {
        let strings =
            |changes: &[Change]| changes.iter().map(ToString::to_string).collect::<Vec<_>>();
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"fresh_count":self.fresh,"patch_drift":strings(&self.patch),"minor_drift":strings(&self.minor),"major_drift":strings(&self.major),"unknown":self.unknown})
                )?
            );
        } else {
            println!(
                "S7 Dep Freshness Scanner\nFresh (pinned == latest):  {} deps",
                self.fresh
            );
            for (title, items) in [
                ("PATCH drift", strings(&self.patch)),
                ("MINOR drift", strings(&self.minor)),
                ("MAJOR drift", strings(&self.major)),
                ("UNKNOWN", self.unknown.clone()),
            ] {
                if !items.is_empty() {
                    println!("\n{title} ({}):", items.len());
                    for item in items {
                        println!("  {item}");
                    }
                }
            }
            if self.patch.is_empty()
                && self.minor.is_empty()
                && self.major.is_empty()
                && self.unknown.is_empty()
            {
                println!("ALL CLEAN: every workspace dep is at the latest stable version.");
            }
        }
        Ok(())
    }
}

fn dependencies(doc: &DocumentMut) -> Result<&dyn TableLike> {
    doc.get("workspace")
        .and_then(|v| v.get("dependencies"))
        .and_then(Item::as_table_like)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "Missing or empty [workspace.dependencies]".into())
}
struct Pin<'a> {
    package: &'a str,
    requirement: &'a str,
    version: Version,
}
fn pin<'a>(alias: &'a str, item: &'a Item) -> Result<Pin<'a>> {
    if ["git", "path", "registry", "registry-index"]
        .iter()
        .any(|key| item.get(key).is_some())
    {
        return Err("dependency source is outside the crates.io index".into());
    }
    let package = match item.get("package") {
        Some(name) => name.as_str().ok_or("invalid package name")?,
        None => alias,
    };
    if package.is_empty()
        || !package
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return Err("invalid crate name".into());
    }
    let requirement = item
        .as_str()
        .or_else(|| item.get("version").and_then(Item::as_str))
        .ok_or("missing string version")?;
    let version = Version::parse(requirement.strip_prefix('=').unwrap_or(requirement))?;
    if !version.pre.is_empty() {
        return Err("pre-release pin requires review".into());
    }
    Ok(Pin {
        package,
        requirement,
        version,
    })
}

#[derive(Deserialize)]
struct IndexVersion {
    name: String,
    vers: String,
    yanked: bool,
}

fn latest(package: &str, response: &str) -> Result<Version> {
    let mut best: Option<Version> = None;
    for line in response.lines().filter(|line| !line.trim().is_empty()) {
        let row: IndexVersion = serde_json::from_str(line)?;
        if !row.name.eq_ignore_ascii_case(package) {
            return Err("index returned a different package".into());
        }
        let v = Version::parse(&row.vers)?;
        if !row.yanked
            && v.pre.is_empty()
            && best
                .as_ref()
                .is_none_or(|old| v.cmp_precedence(old).is_gt())
        {
            best = Some(v);
        }
    }
    best.ok_or_else(|| "registry supplied no stable, non-yanked release".into())
}

fn registry(package: &str) -> Result<String> {
    let name = package.to_ascii_lowercase(); // pin() validated the ASCII name.
    let key = match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    };
    let url = format!("https://index.crates.io/{key}");
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "2",
            "--max-time",
            "30",
            &url,
        ])
        .output()?;
    if !output.status.success() {
        return Err("registry request failed".into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn scan(doc: &DocumentMut, mut fetch: impl FnMut(&str) -> Result<String>) -> Result<Report> {
    let mut report = Report::default();
    for (alias, item) in dependencies(doc)?.iter() {
        let result = (|| -> Result<()> {
            let p = pin(alias, item)?;
            let latest = latest(p.package, &fetch(p.package)?)?;
            match latest.cmp_precedence(&p.version) {
                std::cmp::Ordering::Less => {
                    return Err("newest available stable version is older than the pin".into());
                }
                std::cmp::Ordering::Equal => report.fresh += 1,
                std::cmp::Ordering::Greater => {
                    let bucket = if p.version.major != latest.major {
                        &mut report.major
                    } else if p.version.minor != latest.minor {
                        &mut report.minor
                    } else {
                        &mut report.patch
                    };
                    bucket.push(Change {
                        name: alias.to_owned(),
                        pinned: p.version.to_string(),
                        latest: latest.to_string(),
                    });
                }
            }
            Ok(())
        })();
        if let Err(reason) = result {
            report.unknown.push(format!("{alias}: {reason}"));
        }
    }
    Ok(report)
}

fn apply(doc: &mut DocumentMut, changes: &[Change]) -> Result<()> {
    if changes.is_empty() {
        return Err("empty patch plan".into());
    }
    let mut seen = BTreeSet::new();
    // Validate every proposed edit against the original workspace, before publishing.
    for c in changes {
        if !seen.insert(&c.name) {
            return Err("duplicate patch proposal".into());
        }
        let item = dependencies(doc)?
            .get(&c.name)
            .ok_or("missing proposed dependency")?;
        let p = pin(&c.name, item)?;
        let next = Version::parse(&c.latest)?;
        if p.version.to_string() != c.pinned
            || !next.pre.is_empty()
            || next.major != p.version.major
            || next.minor != p.version.minor
            || next.patch <= p.version.patch
        {
            return Err(format!("stale or non-patch proposal for {}", c.name).into());
        }
        let requirement = format!(
            "{}{}",
            if p.requirement.starts_with('=') {
                "="
            } else {
                ""
            },
            next
        );
        let entry = &mut doc["workspace"]["dependencies"][&c.name];
        let version = if entry.is_str() {
            entry
        } else {
            entry.get_mut("version").ok_or("missing version field")?
        };
        let old = version.as_value_mut().ok_or("expected a version string")?;
        let decor = old.decor().clone();
        *old = Value::from(requirement);
        *old.decor_mut() = decor;
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = NamedTempFile::new_in(dir)?;
    if let Ok(metadata) = fs::metadata(path) {
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

fn run() -> Result<u8> {
    let mut json = false;
    let mut fail_minor = false;
    let mut plan_out = None;
    let mut plan_in = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--fail-on-minor" => fail_minor = true,
            "--write-plan" => {
                plan_out = Some(PathBuf::from(
                    args.next().ok_or("missing output plan path")?,
                ))
            }
            "--apply-plan" => {
                plan_in = Some(PathBuf::from(args.next().ok_or("missing input plan path")?))
            }
            _ => return Err(format!("unknown option {arg}").into()),
        }
    }
    let path = Path::new("Cargo.toml");
    let mut doc = fs::read_to_string(path)?.parse::<DocumentMut>()?;
    if let Some(input) = plan_in {
        if json || fail_minor || plan_out.is_some() {
            return Err("apply-plan cannot also scan".into());
        }
        let changes: Vec<Change> = serde_json::from_slice(&fs::read(input)?)?;
        apply(&mut doc, &changes)?;
        write_atomic(path, doc.to_string().as_bytes())?;
        println!("Applied {} patch proposals", changes.len());
        return Ok(0);
    }
    let report = scan(&doc, registry)?;
    report.print(json)?;
    if !report.unknown.is_empty() {
        return Ok(2);
    }
    if let Some(output) = plan_out {
        write_atomic(&output, &serde_json::to_vec(&report.patch)?)?;
    }
    Ok(u8::from(
        fail_minor && (!report.minor.is_empty() || !report.major.is_empty()),
    ))
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(reason) => {
            eprintln!("Dependency scan failed: {reason}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(package: &str, versions: &[(&str, bool)]) -> String {
        versions
            .iter()
            .map(|(vers, yanked)| {
                serde_json::json!({"name":package,"vers":vers,"yanked":yanked}).to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn complete_index_classifies_exact_and_plain_requirements_and_keeps_table_format() {
        let source = "# root\n[workspace.dependencies]\nplain = \"=1.2.3\" # keep me\nrenamed = { features = [\"x\"], package = \"lib\", version = \"=2.1.0\" }\nold = '1.0.0'\nminor = \"=1.8.9\"\nmajor = \"=2.0.0\"\nsame = \"=1.2.0\"\n";
        let mut doc = source.parse().unwrap();
        let mut called = Vec::new();
        let report = scan(&doc, |p| {
            called.push(p.to_owned());
            let candidates: &[(&str, bool)] = match p {
                "plain" => &[
                    ("1.2.3", false),
                    ("2.0.0-alpha.1", false),
                    ("1.2.5", true),
                    ("1.2.4", false),
                ],
                "lib" => &[("2.1.1+spec-1.1.0", false)],
                "old" => &[("1.0.2", false)],
                "minor" => &[("1.9.0", false)],
                "major" => &[("3.0.0", false)],
                "same" => &[("1.2.0+vendor", false)],
                _ => unreachable!(),
            };
            Ok(rows(p, candidates))
        })
        .unwrap();
        assert_eq!(called.len(), 6);
        assert_eq!(report.fresh, 1);
        assert_eq!(report.patch.len(), 3);
        assert_eq!(report.minor.len(), 1);
        assert_eq!(report.major.len(), 1);
        assert!(report.unknown.is_empty());
        apply(&mut doc, &report.patch).unwrap();
        let changed = doc.to_string();
        assert!(changed.contains("plain = \"=1.2.4\" # keep me"));
        assert!(
            changed
                .contains("features = [\"x\"], package = \"lib\", version = \"=2.1.1+spec-1.1.0\"")
        );
        assert!(!changed.contains("=1.0.2\""));
        assert!(changed.contains("1.0.2"));
    }
    #[test]
    fn incomplete_inputs_and_unknowns_never_become_clean() {
        for doc in ["[workspace]", "[workspace.dependencies]"] {
            assert!(scan(&doc.parse().unwrap(), |_| unreachable!()).is_err());
        }
        for (decl, raw) in [
            (
                "\"=2.0.0\"",
                rows("a", &[("3.0.0-beta", false), ("1.9.9", false)]),
            ),
            ("\"=2.0.0\"", rows("a", &[("2.0.1", true)])),
            ("\"=2.0.0\"", String::new()),
            ("\"=2.0.0\"", rows("other", &[("2.0.0", false)])),
            ("\"=2.0.0\"", "invalid json".into()),
            ("\"=2.0.0\"", rows("a", &[("broken", false)])),
            ("{path=\"abc\"}", String::new()),
            ("\"^2.0\"", String::new()),
            ("\"=2.0.0-alpha.1\"", String::new()),
        ] {
            let doc = format!("[workspace.dependencies]\na={decl}")
                .parse()
                .unwrap();
            let r = scan(&doc, |_| Ok(raw.clone())).unwrap();
            assert_eq!(r.unknown.len(), 1, "{decl}, {raw}");
            assert_eq!(r.fresh, 0);
            assert!(r.patch.is_empty());
        }
        let doc = "[workspace.dependencies]\na='=1.0.0'".parse().unwrap();
        let r = scan(&doc, |_| Err("registry unreachable".into())).unwrap();
        assert_eq!(r.unknown.len(), 1);
    }
    #[test]
    fn plans_reject_missing_stale_duplicate_minor_major_prerelease_and_downgrade() {
        let original = "[workspace.dependencies]\na='=2.1.0'";
        let change = |name: &str, pinned: &str, latest: &str| Change {
            name: name.into(),
            pinned: pinned.into(),
            latest: latest.into(),
        };
        for proposed in [
            vec![],
            vec![change("b", "2.1.0", "2.1.1")],
            vec![change("a", "2.0.0", "2.1.1")],
            vec![change("a", "2.1.0", "2.2.0")],
            vec![change("a", "2.1.0", "3.0.0")],
            vec![change("a", "2.1.0", "2.0.9")],
            vec![change("a", "2.1.0", "2.1.0")],
            vec![change("a", "2.1.0", "2.1.1-beta.1")],
            vec![change("a", "2.1.0", "2.1.1"), change("a", "2.1.0", "2.1.1")],
        ] {
            assert!(apply(&mut original.parse().unwrap(), &proposed).is_err());
        }
    }
}
