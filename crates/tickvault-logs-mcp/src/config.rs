//! Endpoint / path resolution — parity port of the configuration section
//! of server.py (placeholder-aware env resolution + the committed
//! `config/claude-mcp-endpoints.toml` fallback).
//!
//! Every accessor is parameterized over an [`Env`] source so unit tests
//! exercise the full precedence chain without mutating process-global env
//! vars (the Rust twin of `test_placeholder_fallback.py` — every Python
//! assert has a twin in this module's tests).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Env-var lookup abstraction (process env in production, a map in tests).
pub trait Env {
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
pub struct RealEnv;

impl Env for RealEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Map-backed env for unit tests.
#[derive(Default)]
pub struct MapEnv(pub BTreeMap<String, String>);

impl Env for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// Parsed `config/claude-mcp-endpoints.toml` — mirrors the Python
/// `_load_endpoints_config` result shape:
/// `{"active": <profile>, "profiles": {<name>: {<kind>: <value>}}}`.
#[derive(Debug, Clone)]
pub struct EndpointsConfig {
    pub active: String,
    pub profiles: BTreeMap<String, BTreeMap<String, toml::Value>>,
}

impl Default for EndpointsConfig {
    fn default() -> Self {
        Self {
            active: "local".to_string(),
            profiles: BTreeMap::new(),
        }
    }
}

/// Python `_is_resolved`: set AND not a literal `${...}` placeholder.
pub fn is_resolved(value: Option<&str>) -> bool {
    let Some(value) = value else { return false };
    if value.is_empty() {
        return false;
    }
    let stripped = value.trim();
    !(stripped.starts_with("${") && stripped.ends_with('}'))
}

/// Python `_endpoints_config_path`: env override wins iff set and not a
/// placeholder; else `<repo_root>/config/claude-mcp-endpoints.toml`.
pub fn endpoints_config_path(env: &dyn Env, repo_root: &Path) -> PathBuf {
    if let Some(override_path) = env.get("TICKVAULT_MCP_ENDPOINTS_CONFIG") {
        let stripped = override_path.trim();
        let placeholder = stripped.starts_with("${") && stripped.ends_with('}');
        if !override_path.is_empty() && !placeholder {
            return PathBuf::from(override_path);
        }
    }
    repo_root.join("config").join("claude-mcp-endpoints.toml")
}

/// Python `_load_endpoints_config`: best-effort parse; missing or
/// malformed file falls back to `{"active": "local", "profiles": {}}` —
/// the MCP server must never crash because the config is absent.
pub fn load_endpoints_config(path: &Path) -> EndpointsConfig {
    let mut result = EndpointsConfig::default();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return result;
    };
    // toml 1.x: documents parse as `toml::Table` (`toml::Value::from_str`
    // parses a single VALUE expression, not a document).
    let Ok(parsed) = raw.parse::<toml::Table>() else {
        return result;
    };
    if let Some(active) = parsed.get("active").and_then(|v| v.as_str()) {
        result.active = active.to_string();
    }
    if let Some(profiles) = parsed.get("profiles").and_then(|v| v.as_table()) {
        for (name, cfg) in profiles {
            if let Some(table) = cfg.as_table() {
                let entries: BTreeMap<String, toml::Value> =
                    table.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                result.profiles.insert(name.clone(), entries);
            }
        }
    }
    result
}

/// Python `_active_profile`.
pub fn active_profile(env: &dyn Env, cfg: &EndpointsConfig) -> String {
    let override_val = env.get("TICKVAULT_MCP_PROFILE");
    if is_resolved(override_val.as_deref()) {
        return override_val.unwrap_or_default();
    }
    cfg.active.clone()
}

/// Python `_endpoint_url` — the 4-tier precedence order:
/// explicit > resolved env var > active-profile config > hardcoded default.
pub fn endpoint_url(
    env: &dyn Env,
    cfg: &EndpointsConfig,
    kind: &str,
    env_var: &str,
    default: &str,
    explicit: Option<&str>,
) -> String {
    if let Some(explicit) = explicit {
        // Python truthiness: an empty explicit string falls through.
        if !explicit.is_empty() {
            return explicit.to_string();
        }
    }
    let from_env = env.get(env_var);
    if is_resolved(from_env.as_deref()) {
        return from_env.unwrap_or_default();
    }
    let profile = active_profile(env, cfg);
    if let Some(profile_cfg) = cfg.profiles.get(&profile)
        && let Some(from_config) = profile_cfg.get(kind).and_then(|v| v.as_str())
        && !from_config.is_empty()
    {
        return from_config.to_string();
    }
    default.to_string()
}

/// Lexical dot-normalization matching Python `pathlib`'s parse-time
/// behavior: `.` components are dropped (`Path("a/./b")` == `Path("a/b")`,
/// `Path("")` == `Path(".")`), while `..` is KEPT verbatim — pathlib
/// pure-path construction/joins never resolve parent components. Purely
/// lexical: no filesystem access, no symlink/canonicalization. (Accepted
/// unreachable edge: POSIX's special `//`-root, which pathlib preserves,
/// collapses to `/` here — no config/env value can legitimately carry it.)
pub(crate) fn pathlib_lexical(path: &Path) -> PathBuf {
    let out: PathBuf = path
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Python `_logs_dir`. Every branch mirrors CPython's `Path(...)`
/// construction, which drops `.` components at parse time — the shipped
/// default `logs_dir_local = "./data/logs"` must join to
/// `<root>/data/logs`, never `<root>/./data/logs` (the joined string
/// echoes into tool outputs: `dir` / `path` / `log_dir` fields).
pub fn logs_dir(env: &dyn Env, cfg: &EndpointsConfig, repo_root: &Path) -> PathBuf {
    let override_val = env.get("TICKVAULT_LOGS_DIR");
    if is_resolved(override_val.as_deref()) {
        return pathlib_lexical(Path::new(&override_val.unwrap_or_default()));
    }
    let profile = active_profile(env, cfg);
    if let Some(profile_cfg) = cfg.profiles.get(&profile)
        && let Some(from_config) = profile_cfg.get("logs_dir_local").and_then(|v| v.as_str())
        && !from_config.is_empty()
    {
        let cfg_path = PathBuf::from(from_config);
        if cfg_path.is_absolute() {
            return pathlib_lexical(&cfg_path);
        }
        return pathlib_lexical(&repo_root.join(cfg_path));
    }
    pathlib_lexical(&repo_root.join("data").join("logs"))
}

/// Python `_logs_source`: "http" or "local".
pub fn logs_source(env: &dyn Env, cfg: &EndpointsConfig) -> String {
    let override_val = env.get("TICKVAULT_LOGS_SOURCE");
    if is_resolved(override_val.as_deref()) {
        let v = override_val.unwrap_or_default();
        if v == "http" || v == "local" {
            return v;
        }
    }
    let profile = active_profile(env, cfg);
    if let Some(profile_cfg) = cfg.profiles.get(&profile)
        && let Some(source) = profile_cfg.get("logs_source").and_then(|v| v.as_str())
        && (source == "http" || source == "local")
    {
        return source.to_string();
    }
    "local".to_string()
}

/// Python `_machine_logs_dir` — `data/logs/machine/` with the 2026-07-05
/// grace-window fallback to the legacy top-level dir.
pub fn machine_logs_dir(logs_dir: &Path) -> PathBuf {
    let machine = logs_dir.join("machine");
    if machine.is_dir() {
        return machine;
    }
    logs_dir.to_path_buf()
}

/// Python `_state_dir`.
pub fn state_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".claude").join("state")
}

/// Repo-root resolution (DELIBERATE DEVIATION from server.py, documented
/// in lib.rs): `TICKVAULT_MCP_REPO_ROOT` env override (if resolved), else
/// walk up from the current dir looking for `.mcp.json` or
/// `config/claude-mcp-endpoints.toml`, else the current dir. The
/// `.mcp.json` launcher runs from the repo root, so this resolves to the
/// same root `__file__` gives the Python server.
pub fn resolve_repo_root(env: &dyn Env) -> PathBuf {
    let override_val = env.get("TICKVAULT_MCP_REPO_ROOT");
    if is_resolved(override_val.as_deref()) {
        return PathBuf::from(override_val.unwrap_or_default());
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = cwd.clone();
    loop {
        if dir.join(".mcp.json").is_file()
            || dir
                .join("config")
                .join("claude-mcp-endpoints.toml")
                .is_file()
        {
            return dir;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return cwd,
        }
    }
}

/// Everything the tools need, resolved once at startup (the Python server
/// caches the parsed config per process the same way).
pub struct Ctx {
    pub repo_root: PathBuf,
    pub cfg: EndpointsConfig,
}

impl Ctx {
    pub fn from_process_env() -> Self {
        let env = RealEnv;
        let repo_root = resolve_repo_root(&env);
        let cfg_path = endpoints_config_path(&env, &repo_root);
        let cfg = load_endpoints_config(&cfg_path);
        Self { repo_root, cfg }
    }

    pub fn env(&self) -> RealEnv {
        RealEnv
    }

    pub fn logs_dir(&self) -> PathBuf {
        logs_dir(&RealEnv, &self.cfg, &self.repo_root)
    }

    pub fn machine_logs_dir(&self) -> PathBuf {
        machine_logs_dir(&self.logs_dir())
    }
}

// ---------------------------------------------------------------------------
// Tests — Rust twins of scripts/mcp-servers/tickvault-logs/
// test_placeholder_fallback.py (every Python assert has a twin here) plus
// precedence-chain coverage.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn cfg_with_profile(active: &str, name: &str, pairs: &[(&str, &str)]) -> EndpointsConfig {
        let mut cfg = EndpointsConfig {
            active: active.to_string(),
            profiles: BTreeMap::new(),
        };
        let table: BTreeMap<String, toml::Value> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), toml::Value::String(v.to_string())))
            .collect();
        cfg.profiles.insert(name.to_string(), table);
        cfg
    }

    // --- test_placeholder_fallback.py twin: _is_resolved cases -----------
    #[test]
    fn is_resolved_rejects_none() {
        assert!(!is_resolved(None));
    }

    #[test]
    fn is_resolved_rejects_empty() {
        assert!(!is_resolved(Some("")));
    }

    #[test]
    fn is_resolved_rejects_placeholder() {
        assert!(!is_resolved(Some("${FOO}")));
    }

    #[test]
    fn is_resolved_rejects_padded_placeholder() {
        assert!(!is_resolved(Some("  ${FOO}  ")));
    }

    #[test]
    fn is_resolved_accepts_url() {
        assert!(is_resolved(Some("http://127.0.0.1:9000")));
    }

    #[test]
    fn is_resolved_accepts_path() {
        assert!(is_resolved(Some("/data/logs")));
    }

    // --- twin: placeholder env falls through to the TOML config ----------
    #[test]
    fn placeholder_env_falls_through_to_config_for_endpoint_url() {
        let env = env(&[("TICKVAULT_QUESTDB_URL", "${TICKVAULT_QUESTDB_URL}")]);
        let cfg = cfg_with_profile("local", "local", &[("questdb_url", "http://cfg:9000")]);
        let url = endpoint_url(
            &env,
            &cfg,
            "questdb_url",
            "TICKVAULT_QUESTDB_URL",
            "http://127.0.0.1:9000",
            None,
        );
        assert_eq!(url, "http://cfg:9000");
    }

    #[test]
    fn placeholder_profile_env_falls_through_to_config_active() {
        let env = env(&[("TICKVAULT_MCP_PROFILE", "${TICKVAULT_MCP_PROFILE}")]);
        let cfg = cfg_with_profile("aws-prod", "aws-prod", &[]);
        assert_eq!(active_profile(&env, &cfg), "aws-prod");
    }

    #[test]
    fn placeholder_logs_dir_falls_through_to_config() {
        let env = env(&[("TICKVAULT_LOGS_DIR", "${TICKVAULT_LOGS_DIR}")]);
        let cfg = cfg_with_profile("local", "local", &[("logs_dir_local", "data/logs")]);
        let root = Path::new("/repo");
        assert_eq!(logs_dir(&env, &cfg, root), PathBuf::from("/repo/data/logs"));
    }

    #[test]
    fn placeholder_logs_source_falls_through_to_config() {
        let env = env(&[("TICKVAULT_LOGS_SOURCE", "${TICKVAULT_LOGS_SOURCE}")]);
        let cfg = cfg_with_profile("local", "local", &[("logs_source", "http")]);
        assert_eq!(logs_source(&env, &cfg), "http");
    }

    // --- twin: real env values WIN over the config ------------------------
    #[test]
    fn real_env_wins_over_config_for_endpoint_url() {
        let env = env(&[("TICKVAULT_QUESTDB_URL", "http://env:9000")]);
        let cfg = cfg_with_profile("local", "local", &[("questdb_url", "http://cfg:9000")]);
        let url = endpoint_url(
            &env,
            &cfg,
            "questdb_url",
            "TICKVAULT_QUESTDB_URL",
            "http://127.0.0.1:9000",
            None,
        );
        assert_eq!(url, "http://env:9000");
    }

    #[test]
    fn real_env_wins_for_profile_logs_dir_and_source() {
        let env = env(&[
            ("TICKVAULT_MCP_PROFILE", "mac-dev"),
            ("TICKVAULT_LOGS_DIR", "/abs/override"),
            ("TICKVAULT_LOGS_SOURCE", "http"),
        ]);
        let cfg = cfg_with_profile("local", "local", &[("logs_source", "local")]);
        assert_eq!(active_profile(&env, &cfg), "mac-dev");
        assert_eq!(
            logs_dir(&env, &cfg, Path::new("/repo")),
            PathBuf::from("/abs/override")
        );
        assert_eq!(logs_source(&env, &cfg), "http");
    }

    // --- precedence completion -------------------------------------------
    #[test]
    fn explicit_beats_env_and_config() {
        let env = env(&[("TICKVAULT_QUESTDB_URL", "http://env:9000")]);
        let cfg = cfg_with_profile("local", "local", &[("questdb_url", "http://cfg:9000")]);
        let url = endpoint_url(
            &env,
            &cfg,
            "questdb_url",
            "TICKVAULT_QUESTDB_URL",
            "http://d",
            Some("http://explicit:1"),
        );
        assert_eq!(url, "http://explicit:1");
    }

    #[test]
    fn default_used_when_nothing_else_resolves() {
        let env = MapEnv::default();
        let cfg = EndpointsConfig::default();
        let url = endpoint_url(
            &env,
            &cfg,
            "questdb_url",
            "TICKVAULT_QUESTDB_URL",
            "http://127.0.0.1:9000",
            None,
        );
        assert_eq!(url, "http://127.0.0.1:9000");
        assert_eq!(logs_source(&env, &cfg), "local");
        assert_eq!(
            logs_dir(&env, &cfg, Path::new("/r")),
            PathBuf::from("/r/data/logs")
        );
    }

    #[test]
    fn invalid_logs_source_env_value_falls_through() {
        // Python: `_is_resolved(override) and override in {"http", "local"}`
        let env = env(&[("TICKVAULT_LOGS_SOURCE", "carrier-pigeon")]);
        let cfg = EndpointsConfig::default();
        assert_eq!(logs_source(&env, &cfg), "local");
    }

    #[test]
    fn absolute_config_logs_dir_is_not_rejoined() {
        let env = MapEnv::default();
        let cfg = cfg_with_profile("local", "local", &[("logs_dir_local", "/abs/logs")]);
        assert_eq!(
            logs_dir(&env, &cfg, Path::new("/repo")),
            PathBuf::from("/abs/logs")
        );
    }

    // --- pathlib dot-normalization parity (hostile-review r1 finding 1) ---
    #[test]
    fn logs_dir_shipped_dotted_relative_value_joins_like_pathlib() {
        // The LITERAL shipped default in config/claude-mcp-endpoints.toml is
        // "./data/logs". Python: Path("/repo") / "./data/logs" drops the
        // leading `.` at construction => "/repo/data/logs". The Rust join
        // must produce the identical string (it echoes into tool outputs).
        let env = MapEnv::default();
        let cfg = cfg_with_profile("local", "local", &[("logs_dir_local", "./data/logs")]);
        assert_eq!(
            logs_dir(&env, &cfg, Path::new("/repo")),
            PathBuf::from("/repo/data/logs")
        );
    }

    #[test]
    fn logs_dir_dot_normalization_keeps_parent_components() {
        // pathlib drops ONLY `.`; `..` is kept verbatim in pure-path joins
        // (no lexical parent resolution, no symlink resolution).
        let env = MapEnv::default();
        let cfg = cfg_with_profile(
            "local",
            "local",
            &[("logs_dir_local", "./sub/../data/./logs")],
        );
        assert_eq!(
            logs_dir(&env, &cfg, Path::new("/repo")),
            PathBuf::from("/repo/sub/../data/logs")
        );
    }

    #[test]
    fn logs_dir_absolute_dotted_config_value_is_dot_normalized() {
        // Python Path("/abs/./data/logs") == Path("/abs/data/logs").
        let env = MapEnv::default();
        let cfg = cfg_with_profile("local", "local", &[("logs_dir_local", "/abs/./data/logs")]);
        assert_eq!(
            logs_dir(&env, &cfg, Path::new("/repo")),
            PathBuf::from("/abs/data/logs")
        );
    }

    #[test]
    fn logs_dir_env_override_is_dot_normalized() {
        // Python Path(override) also drops `.` at construction.
        let env = env(&[("TICKVAULT_LOGS_DIR", "/x/./y")]);
        let cfg = EndpointsConfig {
            active: "local".to_string(),
            profiles: BTreeMap::new(),
        };
        assert_eq!(
            logs_dir(&env, &cfg, Path::new("/repo")),
            PathBuf::from("/x/y")
        );
    }

    #[test]
    fn pathlib_lexical_empty_and_lone_dot_match_python() {
        // Python Path("") == Path(".") == PosixPath("."): both normalize
        // to "." here so the echoed string can never be empty.
        assert_eq!(pathlib_lexical(Path::new("")), PathBuf::from("."));
        assert_eq!(pathlib_lexical(Path::new(".")), PathBuf::from("."));
        assert_eq!(pathlib_lexical(Path::new("./")), PathBuf::from("."));
    }

    #[test]
    fn pathlib_lexical_drops_leading_curdir_on_relative_path() {
        // The MUTATION-KILLING case for the CurDir filter (review round-2):
        // Rust's `components()` already drops NON-leading `.` segments, so
        // only a LEADING `./` on a relative path ever reaches the filter —
        // and Path equality is component-wise, so without the filter this
        // returns `./data/logs` != `data/logs` and the assert fails.
        assert_eq!(
            pathlib_lexical(Path::new("./data/logs")),
            PathBuf::from("data/logs")
        );
    }

    // --- config loading fallbacks -----------------------------------------
    #[test]
    fn missing_config_file_falls_back_to_local_default() {
        let cfg = load_endpoints_config(Path::new("/definitely/not/here.toml"));
        assert_eq!(cfg.active, "local");
        assert!(cfg.profiles.is_empty());
    }

    #[test]
    fn malformed_config_file_falls_back_to_local_default() {
        let dir = std::env::temp_dir().join("tv-logs-mcp-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        std::fs::write(&path, "this is [not toml").unwrap();
        let cfg = load_endpoints_config(&path);
        assert_eq!(cfg.active, "local");
        assert!(cfg.profiles.is_empty());
    }

    #[test]
    fn parses_active_and_profiles() {
        let dir = std::env::temp_dir().join("tv-logs-mcp-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("good.toml");
        std::fs::write(
            &path,
            "active = \"mac-dev\"\n[profiles.mac-dev]\nquestdb_url = \"http://m:9000\"\n",
        )
        .unwrap();
        let cfg = load_endpoints_config(&path);
        assert_eq!(cfg.active, "mac-dev");
        assert_eq!(
            cfg.profiles["mac-dev"]["questdb_url"].as_str(),
            Some("http://m:9000")
        );
    }

    #[test]
    fn endpoints_config_path_placeholder_override_ignored() {
        let e = env(&[("TICKVAULT_MCP_ENDPOINTS_CONFIG", "${X}")]);
        let p = endpoints_config_path(&e, Path::new("/repo"));
        assert_eq!(p, PathBuf::from("/repo/config/claude-mcp-endpoints.toml"));
        let e2 = env(&[("TICKVAULT_MCP_ENDPOINTS_CONFIG", "/custom.toml")]);
        assert_eq!(
            endpoints_config_path(&e2, Path::new("/repo")),
            PathBuf::from("/custom.toml")
        );
    }

    #[test]
    fn machine_dir_grace_window_falls_back_to_top_level() {
        let base = std::env::temp_dir().join("tv-logs-mcp-machine-test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // No machine/ subdir yet -> legacy top-level.
        assert_eq!(machine_logs_dir(&base), base);
        std::fs::create_dir_all(base.join("machine")).unwrap();
        assert_eq!(machine_logs_dir(&base), base.join("machine"));
    }
}
