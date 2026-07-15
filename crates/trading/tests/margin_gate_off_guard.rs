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
///
/// The header match is a NON-COMMENT LINE match (the trimmed line must
/// start with the header and must not start with `#`) — a commented-out
/// `# [dhan_margin_gate]` banner line can never satisfy the check.
fn toml_section<'a>(content: &'a str, header: &str) -> &'a str {
    let mut offset = 0_usize;
    let mut header_end = None;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') && trimmed.starts_with(header) {
            // Slice starts right after the WHOLE header line (any trailing
            // text on the header line is not section body).
            header_end = Some(offset + line.len());
            break;
        }
        offset += line.len() + 1;
    }
    let after_header =
        header_end.unwrap_or_else(|| panic!("section {header} not found as a non-comment line"));
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
    // A COMMENTED-OUT header must not satisfy the match — the slicer must
    // skip past it to the real (non-comment) header line.
    let synthetic = "# [demo]\n# enabled = true\n[demo]\nenabled = false\n[next]\nx = 1\n";
    let demo = toml_section(synthetic, "[demo]");
    assert!(
        demo.contains("enabled = false") && !demo.contains("enabled = true"),
        "slicer self-test: a commented-out section header must never satisfy the \
         match; sliced:\n{demo}"
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
    // 5, not 10: the funds/margin rate bucket is NOT named by Dhan's docs
    // (Assumed Non-Trading 20/sec) — 5/sec stays <= 50% even under the
    // more conservative 10/sec reading.
    assert_eq!(cfg.rest_self_cap_per_sec, 5);
    // Default::default() must agree with the serde defaults.
    let d = DhanMarginGateConfig::default();
    assert!(!d.enabled);
    assert_eq!(d.tenant_budget_percent, 50);
    assert_eq!(d.rest_self_cap_per_sec, 5);
}

#[test]
fn test_margin_gate_test_bypass_is_cfg_test_gated() {
    // ATTACHMENT semantics, not a proximity window: walking UP from the fn
    // line, skipping comment lines and NON-cfg attribute lines, the FIRST
    // remaining line must carry `#[cfg(test)]` — a nearby-but-unattached
    // cfg line (e.g. gating a neighbouring item) can never satisfy this.
    let src = read_repo_file("crates/trading/src/oms/margin_gate.rs");
    let lines: Vec<&str> = src.lines().collect();
    let bypass_line = lines
        .iter()
        .position(|l| l.contains("fn allow_rest_for_test"))
        .unwrap_or_else(|| panic!("allow_rest_for_test must exist in margin_gate.rs"));
    let mut attached_cfg_test = false;
    for line in lines[..bypass_line].iter().rev() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue; // comment lines sit between attributes and the fn
        }
        if trimmed.starts_with("#[") && !trimmed.contains("cfg(test)") {
            continue; // other attributes (e.g. #[allow]) may stack above
        }
        attached_cfg_test = trimmed.contains("#[cfg(test)]");
        break;
    }
    assert!(
        attached_cfg_test,
        "allow_rest_for_test must have an ATTACHED #[cfg(test)] attribute — the \
         master-lock bypass may never exist in a production build"
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
    // REST self-cap out of range (over the 10/sec hard ceiling, or below
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

/// The ONLY two files whose in-file test modules legitimately call the
/// gate/wrapper needles — they alone keep the production-region truncation
/// (the split excises their tests modules). Every other crate has no
/// legitimate needle caller ANYWHERE in src, so every other file is scanned
/// WHOLE — closing the items-after-tests-module blind region (e.g. main.rs
/// boot helpers declared below its column-0 `#[cfg(test)]` module). A
/// future legitimate cfg(test) caller elsewhere updates this allowlist
/// DELIBERATELY, never as a side effect.
const TRUNCATED_SCAN_ALLOWLIST: [&str; 2] = ["oms/margin_gate.rs", "oms/api_client.rs"];

/// The exact region the no-production-caller scan reads for one file:
/// whole-file everywhere, production-region-only for the two allowlisted
/// oms files above.
fn scanned_region<'a>(path: &std::path::Path, content: &'a str) -> std::borrow::Cow<'a, str> {
    if TRUNCATED_SCAN_ALLOWLIST
        .iter()
        .any(|suffix| path.ends_with(suffix))
    {
        std::borrow::Cow::Owned(production_region(content))
    } else {
        std::borrow::Cow::Borrowed(content)
    }
}

/// TRIPWIRE, not an adversarial barrier: this ratchet exists to catch
/// INNOCENT early wiring of the funds/margin surface before the OMS-wiring
/// PR deliberately opens it — it is NOT proof against a determined evasion.
/// The method-syntax needles do not catch UFCS calls
/// (`MarginGate::check_entry(&gate, ..)`) or aliased imports; that residual
/// is an accepted class per the house source-scan conventions.
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
        // Whole-file everywhere; production-region-only for the two
        // allowlisted oms files (their tests modules call the needles).
        let region = scanned_region(path, &content);
        // The gate's own module legitimately calls two wrappers on
        // self.api inside check_entry — the SOLE wrapper-needle allowance,
        // scoped to its PRODUCTION region only.
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
    // Whole-file self-test: the historical blind-spot file (main.rs, whose
    // boot helpers sit BELOW its column-0 #[cfg(test)] module) must be
    // scanned in FULL — the scanned region's length equals the file's.
    let main_rs_path = repo_root().join("crates/app/src/main.rs");
    let main_rs = fs::read_to_string(&main_rs_path)
        .unwrap_or_else(|err| panic!("guard cannot read {}: {err}", main_rs_path.display()));
    let main_rs_region = scanned_region(&main_rs_path, &main_rs);
    assert_eq!(
        main_rs_region.len(),
        main_rs.len(),
        "scanner self-test: crates/app/src/main.rs must be scanned WHOLE-FILE — a \
         truncated scan re-opens the items-after-tests-module blind region"
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
    // Fallback terminator: if no such item follows, slice to the first
    // column-0 `#[cfg(test)]` line instead — keeps the needle scan honest
    // (never a whole-tail slice into the tests module) if check_exit ever
    // becomes the last method in the impl.
    let fn_end = after[1..]
        .find("\n    pub ")
        .or_else(|| after[1..].find("\n#[cfg(test)]"))
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
