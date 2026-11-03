//! The kernel TCP receive-pressure counters and receive-buffer ceilings stay on the local
//! `/metrics` page only (plan item 52j phase A, 2026-10-30).
//!
//! Shipping a name to CloudWatch costs money every month and needs a dated
//! row in the noise lock first. This guard fails the build if any of the new
//! names is selected by the current CloudWatch agent configuration or appears
//! literally in Terraform. Selectors must use an anchored alternation of exact
//! names; unsupported regex syntax fails closed. Dynamic Terraform construction
//! still needs review; the literal scan is not a full Terraform interpreter.

use std::path::{Path, PathBuf};

use tickvault_app::kernel_rx_queue_sampler::{
    KERNEL_RMEM_MAX_GAUGE, KERNEL_TCP_EXT_COUNTER, KERNEL_TCP_RMEM_MAX_GAUGE,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves") // APPROVED: test
}

const NEW_NAMES: [&str; 3] = [
    KERNEL_TCP_EXT_COUNTER,
    KERNEL_RMEM_MAX_GAUGE,
    KERNEL_TCP_RMEM_MAX_GAUGE,
];

/// The checked agent file uses exact names, not general regular expressions.
/// Refusing unsupported syntax prevents a broad selector from evading a literal
/// substring check and silently shipping these local-only series.
fn selector_is_explicit_and_local_free(selector: &str) -> bool {
    let Some(inner) = selector
        .strip_prefix('^')
        .and_then(|text| text.strip_suffix('$'))
    else {
        return false;
    };
    let names = match inner
        .strip_prefix('(')
        .and_then(|text| text.strip_suffix(')'))
    {
        Some(names) => names,
        None if !inner.contains('|') => inner,
        None => return false,
    };
    names.split('|').all(|name| {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b':')
            && !NEW_NAMES.contains(&name)
    })
}

#[test]
fn kernel_tcp_ext_names_are_not_in_the_cloudwatch_agent_config() {
    let path = repo_root().join("deploy/aws/cloudwatch-agent.json");
    let text = std::fs::read_to_string(&path).expect("cloudwatch-agent.json is readable"); // APPROVED: test
    let config: serde_json::Value = serde_json::from_str(&text).expect("agent JSON is valid");
    let declarations = config
        .pointer("/logs/metrics_collected/prometheus/emf_processor/metric_declaration")
        .and_then(serde_json::Value::as_array)
        .expect("metric declarations are an array");
    assert!(!declarations.is_empty(), "no declarations were checked");
    for declaration in declarations {
        let selectors = declaration
            .get("metric_selectors")
            .and_then(serde_json::Value::as_array)
            .expect("every declaration has selector strings");
        assert!(!selectors.is_empty(), "no selectors were checked");
        for selector in selectors {
            assert!(
                selector
                    .as_str()
                    .is_some_and(selector_is_explicit_and_local_free),
                "unsupported or local-kernel selector in {}",
                path.display()
            );
        }
    }
}

#[test]
fn test_selector_is_explicit_and_local_free_rejects_broad_and_literal_matches() {
    assert!(selector_is_explicit_and_local_free(
        "^(tv_allowed|tv_other_total)$"
    ));
    assert!(selector_is_explicit_and_local_free("^tv_allowed$"));
    for selector in [
        "^tv_.*$",
        "^tv_allowed|total$",
        "^.*$",
        ".*",
        "^(tv_allowed|)$",
        "^()$",
        "^tv_(allowed|kernel_.*)$",
        "^tv_[a-z]+$",
        "^tv_allowed$|^tv_kernel.*$",
    ] {
        assert!(
            !selector_is_explicit_and_local_free(selector),
            "accepted {selector}"
        );
    }
    for name in NEW_NAMES {
        assert!(!selector_is_explicit_and_local_free(&format!("^{name}$")));
        assert!(!selector_is_explicit_and_local_free(&format!(
            "^(tv_allowed|{name})$"
        )));
    }
}

#[test]
fn kernel_tcp_ext_names_are_not_in_terraform() {
    let dir = repo_root().join("deploy/aws/terraform");
    let entries = std::fs::read_dir(&dir).expect("terraform dir is readable"); // APPROVED: test
    let mut scanned = 0_usize;
    for entry in entries {
        let entry = entry.expect("terraform entry is readable");
        let path = entry.path();
        let is_tf = path
            .extension()
            .is_some_and(|ext| ext == "tf" || ext == "tftpl");
        if !is_tf {
            continue;
        }
        scanned += 1;
        let text = std::fs::read_to_string(&path).expect("terraform source is readable");
        for name in NEW_NAMES {
            assert!(
                !text.contains(name),
                "{name} is in {}: no alarm or filter may watch it without a dated noise-lock row",
                path.display()
            );
        }
    }
    assert!(scanned > 0, "the terraform scan must not pass vacuously");
}
