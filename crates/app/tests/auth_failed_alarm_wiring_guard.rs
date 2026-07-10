//! Source-scan ratchet — GAP-SEC-01 401-burst paging wired end-to-end
//! (2026-07-10, wave-2 #2 of the endpoint-hardening sequence).
//!
//! A credential brute-force sweep against the bearer-gated API surface on
//! the Tailscale funnel previously produced only warn!-level log lines —
//! no counter, no alarm, no page. This guard pins the full chain in
//! lockstep so a rename / deletion on ANY side (Rust emit sites, main.rs
//! pre-registration, the terraform file) fails the build:
//!
//! 1. `crates/api/src/lib.rs::build_router_with_auth` pre-registers
//!    `tv_api_auth_failed_total` at 0 at router construction — the single
//!    choke point BOTH boot paths call, provably before the first servable
//!    401 (no router => no request) and AFTER the boot Step-3 recorder
//!    install (the feed-stall round-5 first-sample baseline lesson — a
//!    pre-install registration resolves to the no-op recorder; a MISSING
//!    registration eats part of the session's first 401 burst as the delta
//!    baseline, silently raising the alarm's effective threshold). The
//!    install-before-router ORDER is pinned by a READ-ONLY source-order
//!    scan of main.rs (no main.rs edit required — the 2026-07-10 relocation
//!    out of main.rs; house pattern:
//!    `ratchet_tick_processor_spawns_before_reinject_await`).
//! 2. `crates/api/src/middleware.rs::require_bearer_auth` increments the
//!    counter in ALL THREE rejection arms (invalid token / malformed
//!    header / missing header) in its PRODUCTION region.
//! 3. `deploy/aws/terraform/auth-failed-alarm.tf` extracts the real metric
//!    field into the same identity (full extraction, feed-stall precedent)
//!    and the alarm reads it (Sum / 300s / threshold 25 / notBreaching).
//!
//! House precedents: `seal_drop_paging_wiring_guard.rs` (main.rs
//! source-order scan + terraform<->Rust lockstep + string-aware HCL
//! comment stripper), `groww_sidecar_supervisor.rs::
//! test_stall_restart_counter_is_preregistered_after_recorder_install`.
//! Runbook: `.claude/rules/project/gap-enforcement.md` (GAP-SEC-01).

use std::fs;
use std::path::PathBuf;

fn read_repo_file(rel_from_app: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_app);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())) // APPROVED: test
}

/// Strip `//`-line-comments (treating `://` URL scheme separators as code,
/// the `http_client_fallback_guard.rs` precedent) so a comment mentioning a
/// needle can never satisfy the scan.
fn strip_line_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Everything strictly before the first top-level `#[cfg(test)]` — the
/// production region (robust to code after `mod tests`). MUST be applied to
/// COMMENT-STRIPPED source: middleware.rs carries a doc comment that
/// mentions the `#[cfg(test)]` literal (the "cannot be cfg(test)" note),
/// which would otherwise truncate the region before the auth arms.
fn production_region(body: &str) -> &str {
    body.split("#[cfg(test)]").next().unwrap_or(body)
}

/// Strip `#`-comments from an HCL (terraform) body, STRING-AWARE: a `#`
/// inside a double-quoted string is kept as code. Comments can never be
/// terraform configuration, so they are removed before matching —
/// otherwise the tf file's own header comments (which cite the shape
/// needles) would satisfy the scan vacuously (the seal-drop 2026-07-09
/// hostile-review lesson).
fn strip_hcl_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut in_str = false;
        let mut esc = false;
        let mut cut = line.len();
        for (i, &b) in bytes.iter().enumerate() {
            if esc {
                esc = false;
                continue;
            }
            match b {
                b'\\' if in_str => esc = true,
                b'"' => in_str = !in_str,
                b'#' if !in_str => {
                    cut = i;
                    break;
                }
                _ => {}
            }
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

#[test]
fn test_strip_hcl_comments_keeps_strings_drops_comments() {
    // Anti-vacuity self-test: the stripper must drop comment text (incl. a
    // comment mentioning the threshold needle) while keeping a `#` inside a
    // quoted string intact.
    let src = "# header threshold = 25 comment\n\
               alarm_description = \"probe #1 - kept\"\n\
               real_attr = 1 # trailing comment threshold = 25\n";
    let stripped = strip_hcl_comments(src);
    assert!(
        !compact(&stripped).contains("threshold=25"),
        "comment text survived HCL stripping: {stripped}"
    );
    assert!(
        stripped.contains("probe #1 - kept") && stripped.contains("real_attr = 1"),
        "string or code text was wrongly removed: {stripped}"
    );
}

/// Whitespace-compacted form so rustfmt / terraform-fmt re-wrapping and
/// alignment can never hide a real site from a contiguous needle.
fn compact(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

const AUTH_FAILED_COUNTER: &str = "tv_api_auth_failed_total";

#[test]
fn test_auth_failed_counter_is_preregistered_after_recorder_install() {
    // Part A — the registration site lives in the api crate's router
    // constructor (`build_router_with_auth`), the single choke point BOTH
    // boot paths (fast crash-recovery arm + slow start_dhan_lane) call.
    // Registering there is provably before the first possible 401: the
    // server cannot serve a request before its router exists. Production
    // region only, comments stripped — a comment or #[cfg(test)] block
    // mentioning the counter must never satisfy this scan.
    let lib_full = read_repo_file("../api/src/lib.rs");
    let lib_stripped = strip_line_comments(&lib_full);
    let lib_prod = production_region(&lib_stripped);
    let router_fn = lib_prod
        .find("pub fn build_router_with_auth")
        .expect("build_router_with_auth must exist in crates/api/src/lib.rs"); // APPROVED: test
    let reg_needle = format!("counter!(\"{AUTH_FAILED_COUNTER}\").increment(0)");
    let reg = compact(&lib_prod[router_fn..]).find(&compact(&reg_needle));
    assert!(
        reg.is_some(),
        "build_router_with_auth must pre-register EXACTLY the unlabeled \
         {AUTH_FAILED_COUNTER} series at 0 (the single series the \
         tv-<env>-api-auth-failed pager reads: the CW agent's delta pipeline \
         drops the series' first sample as its baseline, so a lazily-born 401 \
         counter eats part of the session's FIRST burst and silently raises \
         the effective alarm threshold)"
    );
    // Part B — READ-ONLY boot-order scan of main.rs: the recorder install
    // (observability::init_metrics) must precede EVERY build_router_with_auth
    // call site in source order, so the Part-A registration always resolves
    // to the real recorder, never the no-op one (the feed-stall round-4 VOID
    // class). Source order is the house-accepted approximation of execution
    // order here (ratchet_tick_processor_spawns_before_reinject_await
    // precedent); both call sites live in post-install boot code today.
    let main_full = read_repo_file("src/main.rs");
    let main_stripped = strip_line_comments(&main_full);
    let main_src = production_region(&main_stripped);
    let install = main_src
        .find("observability::init_metrics(")
        .expect("the metrics recorder install site must exist in main.rs"); // APPROVED: test
    let call_needle = "build_router_with_auth(";
    let mut call_sites = 0usize;
    let mut search_from = 0usize;
    while let Some(pos) = main_src[search_from..].find(call_needle) {
        let abs = search_from + pos;
        assert!(
            abs > install,
            "every build_router_with_auth call site in main.rs must come AFTER \
             observability::init_metrics in source order (offset {abs} < install \
             {install}) — otherwise the api-crate counter registration could \
             resolve to the no-op recorder"
        );
        call_sites += 1;
        search_from = abs + call_needle.len();
    }
    assert!(
        call_sites >= 1,
        "main.rs must call build_router_with_auth at least once — the API \
         server (and thus the auth-failed counter registration) is otherwise \
         never constructed"
    );
}

#[test]
fn test_auth_failed_counter_emit_sites_present_in_all_three_rejection_arms() {
    // The bearer-auth middleware has exactly 3 rejection arms (invalid
    // token / malformed Authorization header / missing header — the BUG-3
    // warn! sites pin the arm structure). Each must increment the counter.
    // If a future refactor funnels the arms through a single choke point,
    // update this count deliberately in the same PR — never delete the scan.
    let src = read_repo_file("../api/src/middleware.rs");
    // Comments stripped FIRST: a middleware doc comment mentions the
    // `#[cfg(test)]` literal, which would truncate production_region early.
    let stripped = strip_line_comments(&src);
    let prod = compact(production_region(&stripped));
    let needle = format!("counter!(\"{AUTH_FAILED_COUNTER}\").increment(1)");
    let count = prod.matches(&needle).count();
    assert_eq!(
        count, 3,
        "require_bearer_auth must increment {AUTH_FAILED_COUNTER} in ALL THREE \
         rejection arms (found {count} emit sites) — the tv-<env>-api-auth-failed \
         alarm depends on every 401 being counted"
    );
    // Log-amplification defence: the arms must NOT gain a per-401 error!
    // (the 3 pre-existing warn!s are the ratcheted BUG-3 forensic surface;
    // error!-level per-attacker-request lines are banned — same reasoning
    // as the #1458 429 handling).
    let auth_fn_start = prod
        .find("pubasyncfnrequire_bearer_auth")
        .expect("require_bearer_auth must exist in middleware.rs"); // APPROVED: test
    let auth_fn_region = &prod[auth_fn_start..];
    // The boundary fn is REQUIRED (2026-07-10 hostile-review MEDIUM fix: an
    // earlier draft used a non-existent fn name, silently widening the scan
    // to end-of-file via unwrap_or — a dead needle). If request_tracing is
    // ever renamed, this fails LOUDLY; update the needle in the same PR.
    let fn_end = auth_fn_region
        .find("pubasyncfnrequest_tracing")
        .expect("request_tracing must follow require_bearer_auth in middleware.rs"); // APPROVED: test
    assert!(
        !auth_fn_region[..fn_end].contains("error!("),
        "require_bearer_auth must not log error!-level per-401 lines \
         (attacker-triggerable log amplification)"
    );
}

#[test]
fn test_auth_failed_alarm_filter_matches_emitted_series_shape() {
    // HCL comments stripped: the tf header comments cite these needles —
    // without stripping, a re-pointed real attribute would keep the guard
    // green vacuously (the seal-drop hostile-review class).
    let tf = strip_hcl_comments(&read_repo_file(
        "../../deploy/aws/terraform/auth-failed-alarm.tf",
    ));
    // Filter pattern built from the REAL metric literal so a Rust-side
    // rename breaks THIS test instead of silently blinding the filter.
    let pattern_needle = format!("{{ $.{AUTH_FAILED_COUNTER} = * }}");
    assert!(
        tf.contains(&pattern_needle),
        "auth-failed-alarm.tf filter must match the real /metrics event shape \
         ({{ $.{AUTH_FAILED_COUNTER} = * }} — the unlabeled full extraction); \
         expected pattern body: {pattern_needle}"
    );
    let compact_tf = compact(&tf);
    assert!(
        compact_tf.contains(&format!("value=\"$.{AUTH_FAILED_COUNTER}\"")),
        "the metric transformation must extract the per-scrape delta value from \
         the real counter field $.{AUTH_FAILED_COUNTER}"
    );
    // Full extraction: the transformation and the alarm both use the raw
    // counter identity (the feed-stall precedent — no label slice here).
    assert!(
        compact_tf.contains(&format!("name=\"{AUTH_FAILED_COUNTER}\""))
            && compact_tf.contains(&format!("metric_name=\"{AUTH_FAILED_COUNTER}\"")),
        "both the metric_transformation name and the alarm metric_name must use \
         the {AUTH_FAILED_COUNTER} identity"
    );
    for needle in [
        // Filter side (2026-07-10 hostile-review LOW fixes: pin the log
        // group, the host dimension extraction, and the hardcoded filter
        // namespace so a drift can never leave the alarm permanently
        // missing-data -> notBreaching with this ratchet still green).
        "log_group_name=\"/tickvault/${var.environment}/metrics\"",
        "host=\"$.host\"",
        "namespace=\"Tickvault/Prod\"",
        // Alarm side. namespace/dimensions use the shared locals — the
        // locals resolve to \"Tickvault/Prod\" / { host = \"tickvault-prod\" }
        // (app-alarms.tf), pairing with the filter's hardcoded namespace +
        // $.host dimension above; both sides pinned so they cannot split.
        "statistic=\"Sum\"",
        "period=300",
        "threshold=25",
        "evaluation_periods=1",
        "treat_missing_data=\"notBreaching\"",
        "namespace=local.app_namespace",
        "dimensions=local.app_dimensions",
        "alarm_actions=local.app_alarm_actions",
        "ok_actions=local.app_alarm_ok",
    ] {
        assert!(
            compact_tf.contains(needle),
            "auth-failed-alarm.tf filter/alarm shape drifted — missing `{needle}` \
             (Sum of per-scrape deltas, aligned 300s window, 25 rejections page, \
             filter and alarm bound to the same namespace + host dimension, \
             box-off missing data never breaches, SNS routing via the shared \
             app alarm actions)"
        );
    }
}
