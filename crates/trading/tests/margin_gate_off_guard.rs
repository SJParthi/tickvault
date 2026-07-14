//! Ratchet guard: the 🔷 DHAN margin gate ships DEFAULT-OFF behind BOTH
//! halves of its OFF-switch lattice — the `[dhan_margin_gate]` config gate
//! (serde default false, base.toml explicit false) AND the code-change
//! master lock `DHAN_MARGIN_GATE_REST_ALLOWED` (constants.rs, false until a
//! fresh dated operator quote lands in `.claude/rules/dhan/funds-margin.md`).
//!
//! Guard-file honesty: every scan targets SOURCE files (constants.rs,
//! margin_gate.rs, config/base.toml) — never this guard's own file — so no
//! assertion can vacuously match its own needle literals. The section
//! slicer carries its own self-test against a known-present key.

use std::fs;
use std::path::PathBuf;

use tickvault_common::config::DhanMarginGateConfig;

/// Repo root, resolved robustly from this crate's manifest dir.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn read_repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("guard cannot read {}: {err}", path.display()))
}

/// Slices the body of one TOML section: from its `[header]` line to the
/// next line that starts a new `[` table (or EOF). Parsing the SECTION
/// SLICE — not whole-file substring matching — because many sections carry
/// an `enabled = false` key.
fn toml_section<'a>(content: &'a str, header: &str) -> &'a str {
    let start = content
        .find(header)
        .unwrap_or_else(|| panic!("section {header} not found"));
    let after_header = start + header.len();
    let rest = &content[after_header..];
    let end = rest
        .lines()
        .scan(0_usize, |offset, line| {
            let line_start = *offset;
            *offset += line.len() + 1;
            Some((line_start, line))
        })
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map_or(rest.len(), |(line_start, _)| line_start);
    &rest[..end]
}

#[test]
fn test_margin_gate_rest_master_lock_const_is_false() {
    // The compiled truth: the master lock holds.
    assert!(
        !tickvault_common::constants::DHAN_MARGIN_GATE_REST_ALLOWED,
        "the margin-gate REST master lock must stay false until the operator grant \
         is recorded in .claude/rules/dhan/funds-margin.md"
    );
    // The source truth: the exact declaration literal is pinned so a
    // sneaky re-typing (e.g. cfg-gated true) fails the build.
    let constants_src = read_repo_file("crates/common/src/constants.rs");
    assert!(
        constants_src.contains("pub const DHAN_MARGIN_GATE_REST_ALLOWED: bool = false;"),
        "constants.rs must declare the margin-gate REST master lock as literally false"
    );
}

#[test]
fn test_margin_gate_base_toml_section_disabled() {
    let base_toml = read_repo_file("config/base.toml");
    let section = toml_section(&base_toml, "[dhan_margin_gate]");
    assert!(
        section.contains("enabled = false"),
        "[dhan_margin_gate] in config/base.toml must ship enabled = false; section was:\n{section}"
    );
}

#[test]
fn test_margin_gate_guard_section_slicer_finds_known_key() {
    // Self-test: the slicer must find a key KNOWN to exist inside a
    // different section, proving it is not vacuously empty.
    let base_toml = read_repo_file("config/base.toml");
    let section = toml_section(&base_toml, "[tf_consistency]");
    assert!(
        section.contains("enabled"),
        "slicer self-test: [tf_consistency] must contain an enabled key"
    );
    // And it must STOP at the next section header (no '[' table line
    // inside the slice).
    assert!(
        !section.lines().any(|l| l.trim_start().starts_with('[')),
        "slicer self-test: the slice must not spill into the next section"
    );
}

#[test]
fn test_margin_gate_config_serde_default_is_disabled() {
    // An EMPTY TOML document (absent section shape) must deserialize to
    // the disabled defaults — never an error, never enabled.
    let cfg: DhanMarginGateConfig =
        toml::from_str("").unwrap_or_else(|err| panic!("empty TOML must deserialize: {err}"));
    assert!(!cfg.enabled, "serde default must be DISABLED");
    assert_eq!(cfg.tenant_budget_percent, 50);
    assert_eq!(cfg.rest_self_cap_per_sec, 10);
    // Default::default() must agree with the serde defaults.
    let d = DhanMarginGateConfig::default();
    assert!(!d.enabled);
    assert_eq!(d.tenant_budget_percent, 50);
    assert_eq!(d.rest_self_cap_per_sec, 10);
}

#[test]
fn test_margin_gate_test_bypass_is_cfg_test_gated() {
    let src = read_repo_file("crates/trading/src/oms/margin_gate.rs");
    let lines: Vec<&str> = src.lines().collect();
    let bypass_line = lines
        .iter()
        .position(|l| l.contains("fn allow_rest_for_test"))
        .unwrap_or_else(|| panic!("allow_rest_for_test must exist in margin_gate.rs"));
    let window_start = bypass_line.saturating_sub(4);
    let preceded_by_cfg_test = lines[window_start..bypass_line]
        .iter()
        .any(|l| l.contains("#[cfg(test)]"));
    assert!(
        preceded_by_cfg_test,
        "allow_rest_for_test must be #[cfg(test)]-gated — the master-lock bypass may \
         never exist in a production build"
    );
}

#[test]
fn test_margin_gate_and_composition_call_site_pinned() {
    // The judge-mandated AND-composition: the gate is enabled only when
    // BOTH the config gate AND the const master lock agree.
    let src = read_repo_file("crates/trading/src/oms/margin_gate.rs");
    assert!(
        src.contains("self.cfg.enabled && self.rest_allowed"),
        "is_enabled must stay the exact AND-composition of the config gate and the \
         REST master lock"
    );
}

#[test]
fn test_margin_gate_validate_bounds() {
    // Over-budget percent (shared account — never above 50).
    let over = DhanMarginGateConfig {
        enabled: false,
        tenant_budget_percent: 51,
        rest_self_cap_per_sec: 10,
    };
    assert!(over.validate().is_err(), "51% must be rejected");
    let zero = DhanMarginGateConfig {
        tenant_budget_percent: 0,
        ..DhanMarginGateConfig::default()
    };
    assert!(zero.validate().is_err(), "0% must be rejected");
    // REST self-cap out of range (over half the 20/sec budget, or below
    // the 2-call entry burst).
    let cap_high = DhanMarginGateConfig {
        rest_self_cap_per_sec: 11,
        ..DhanMarginGateConfig::default()
    };
    assert!(cap_high.validate().is_err(), "cap 11 must be rejected");
    let cap_low = DhanMarginGateConfig {
        rest_self_cap_per_sec: 1,
        ..DhanMarginGateConfig::default()
    };
    assert!(cap_low.validate().is_err(), "cap 1 must be rejected");
    // The legal boundaries pass.
    let max_ok = DhanMarginGateConfig {
        enabled: false,
        tenant_budget_percent: 50,
        rest_self_cap_per_sec: 10,
    };
    assert!(max_ok.validate().is_ok(), "(50, 10) must pass");
    let min_ok = DhanMarginGateConfig {
        enabled: false,
        tenant_budget_percent: 1,
        rest_self_cap_per_sec: 2,
    };
    assert!(min_ok.validate().is_ok(), "(1, 2) must pass");
}

/// Recursively collects every `.rs` file under `dir`.
fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The PRODUCTION region of a source file: everything up to the first
/// COLUMN-0 `#[cfg(test)]` line (the house split trick — the tests-module
/// attribute is unindented). Column-0 matters here: margin_gate.rs carries
/// INDENTED `#[cfg(test)]` method attributes mid-impl that must NOT
/// truncate the region, or the wrapper-call self-test below would go
/// vacuous.
fn production_region(content: &str) -> String {
    let mut region = String::new();
    for line in content.lines() {
        if line.starts_with("#[cfg(test)]") {
            break;
        }
        region.push_str(line);
        region.push('\n');
    }
    region
}

#[test]
fn test_margin_gate_and_wrappers_have_no_production_callers() {
    // Pins the funds-margin.md claim "the funds/margin REST surface has
    // ZERO production callers" until the OMS-wiring PR INTENTIONALLY
    // updates this ratchet's allowlist. Leading-dot needles avoid matching
    // the fn DEFINITIONS (`pub async fn calculate_margin(` carries no
    // leading dot; `MarginGate::new(` never matches its own `pub fn new(`
    // declaration).
    const GATE_NEEDLES: [&str; 3] = ["MarginGate::new(", ".check_entry(", ".check_exit("];
    const WRAPPER_NEEDLES: [&str; 3] = [
        ".calculate_margin(",
        ".calculate_multi_margin(",
        ".get_fund_limit(",
    ];

    // Walk EVERY crates/*/src/**/*.rs file in the workspace (this guard
    // file lives under tests/, so it is never scanned — its own needle
    // literals cannot vacuously match).
    let crates_dir = repo_root().join("crates");
    let mut src_files = Vec::new();
    for entry in fs::read_dir(&crates_dir)
        .unwrap_or_else(|err| panic!("guard cannot read crates/: {err}"))
        .flatten()
    {
        let src = entry.path().join("src");
        if src.is_dir() {
            collect_rs_files(&src, &mut src_files);
        }
    }
    assert!(
        src_files.len() > 50,
        "walker sanity: expected the workspace src walk to find many files, got {}",
        src_files.len()
    );

    for path in &src_files {
        let content = fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("guard cannot read {}: {err}", path.display()));
        let region = production_region(&content);
        // The gate's own module legitimately calls two wrappers on
        // self.api inside check_entry — the SOLE allowlisted file.
        let is_margin_gate_module = path.ends_with("oms/margin_gate.rs");
        for needle in GATE_NEEDLES {
            assert!(
                !region.contains(needle),
                "production caller of the margin gate found ({needle:?} in {}) — the \
                 OMS-wiring PR must update this ratchet's allowlist DELIBERATELY, \
                 never as a side effect",
                path.display()
            );
        }
        if !is_margin_gate_module {
            for needle in WRAPPER_NEEDLES {
                assert!(
                    !region.contains(needle),
                    "production caller of a funds/margin REST wrapper found \
                     ({needle:?} in {}) — the OMS-wiring PR must update this \
                     ratchet's allowlist DELIBERATELY, never as a side effect",
                    path.display()
                );
            }
        }
    }

    // Non-vacuity self-test: the scanner must SEE real content — the
    // allowlisted margin_gate.rs production region genuinely contains the
    // wrapper calls (check_entry's two REST legs on self.api)...
    let margin_gate_src = read_repo_file("crates/trading/src/oms/margin_gate.rs");
    let margin_gate_region = production_region(&margin_gate_src);
    assert!(
        margin_gate_region.contains(".calculate_margin("),
        "scanner self-test: margin_gate.rs's production region must contain the \
         .calculate_margin( call — an over-eager region truncation would make \
         every assertion above vacuous"
    );
    assert!(
        margin_gate_region.contains(".get_fund_limit("),
        "scanner self-test: margin_gate.rs's production region must contain the \
         .get_fund_limit( call"
    );
    // ...and the region split genuinely TRUNCATES: the tests module (which
    // constructs the gate) is excluded while the full file contains it.
    assert!(
        margin_gate_src.contains("MarginGate::new("),
        "scanner self-test: the full margin_gate.rs file must contain a \
         MarginGate::new( construction (in its tests module)"
    );
    assert!(
        !margin_gate_region.contains("MarginGate::new("),
        "scanner self-test: the production region must EXCLUDE the tests \
         module's MarginGate::new( construction"
    );
}

#[test]
fn test_margin_gate_exit_arm_has_no_rest_reachability() {
    // Exits are NEVER margin-gated: the check_exit body must be
    // structurally REST-free — no await points, no REST wrappers, no
    // limiter touch.
    let src = read_repo_file("crates/trading/src/oms/margin_gate.rs");
    let fn_start = src
        .find("pub fn check_exit")
        .unwrap_or_else(|| panic!("check_exit must exist in margin_gate.rs"));
    let after = &src[fn_start..];
    // The fn body ends where the next `pub` item begins at method
    // indentation (check_entry's declaration or its doc comment block).
    let fn_end = after[1..]
        .find("\n    pub ")
        .map_or(after.len(), |offset| offset + 1);
    let exit_body = &after[..fn_end];
    for needle in [".await", "calculate_margin", "get_fund_limit", "self_cap"] {
        assert!(
            !exit_body.contains(needle),
            "check_exit must be structurally REST-free — found {needle:?} inside its \
             body slice:\n{exit_body}"
        );
    }
}
