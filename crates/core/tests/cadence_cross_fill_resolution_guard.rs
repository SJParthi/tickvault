//! Ratchet: PR #1693 item 7 — the cross_fill_audit resolution seam.
//!
//! Pins that BOTH `CrossFillAuditEvent` emit constructions in the cadence
//! runner stamp the closed-contract `resolution` token and the lane's
//! `retry_attempts` counter: the groww fallback LAUNCH event (stage
//! `groww_fallback`) carries `resolution: "native_late_retry"`, and the
//! cross-fill arm event (stage `cross_fill`) carries
//! `resolution: "cross_fill"`. Exact-count assertions so a dropped stamp
//! fails the build (source scan — the house
//! `cadence_history_repull_isolation_guard` style; no event-capture
//! harness exists for the OnceLock audit sink).

const RUNNER_SRC: &str = include_str!("../src/cadence/runner.rs");
const AUDIT_SRC: &str = include_str!("../src/cadence/audit.rs");

fn count(hay: &str, needle: &str) -> usize {
    hay.match_indices(needle).count()
}

#[test]
fn test_resolution_emitted_on_both_events() {
    // Exactly one construction per token — a dropped or duplicated stamp
    // fails the build.
    assert_eq!(
        count(RUNNER_SRC, "resolution: \"native_late_retry\","),
        1,
        "the fallback-launch emit must stamp resolution native_late_retry exactly once"
    );
    assert_eq!(
        count(RUNNER_SRC, "resolution: \"cross_fill\","),
        1,
        "the cross-fill arm emit must stamp resolution cross_fill exactly once"
    );
    assert_eq!(
        count(
            RUNNER_SRC,
            "retry_attempts: cycle.groww.late_retry_attempts,"
        ),
        1,
        "the fallback-launch emit must carry the groww lane late-retry counter"
    );
    assert_eq!(
        count(RUNNER_SRC, "retry_attempts: lane.late_retry_attempts,"),
        1,
        "the cross-fill arm emit must carry the lane late-retry counter"
    );

    // Every emit construction (exactly two) carries BOTH new fields, and
    // the stage→resolution pairing holds inside each construction window.
    let marker = "emit_cross_fill_audit(CrossFillAuditEvent {";
    let sites: Vec<usize> = RUNNER_SRC
        .match_indices(marker)
        .map(|(idx, _)| idx)
        .collect();
    assert_eq!(sites.len(), 2, "exactly two runner emit constructions");
    for start in sites {
        let tail = &RUNNER_SRC[start..];
        let end = tail.find("});").expect("emit construction closes");
        let window = &tail[..end];
        assert!(
            window.contains("resolution: \""),
            "emit construction missing a resolution stamp: {window}"
        );
        assert!(
            window.contains("retry_attempts: "),
            "emit construction missing a retry_attempts stamp: {window}"
        );
        if window.contains("stage: \"groww_fallback\",") {
            assert!(
                window.contains("resolution: \"native_late_retry\","),
                "groww_fallback event must resolve native_late_retry"
            );
        } else {
            assert!(
                window.contains("stage: \"cross_fill\","),
                "second emit construction must be the cross-fill arm"
            );
            assert!(
                window.contains("resolution: \"cross_fill\","),
                "cross_fill event must resolve cross_fill"
            );
        }
    }
}

#[test]
fn test_audit_event_and_sample_carry_resolution_fields() {
    assert!(
        AUDIT_SRC.contains("pub resolution: &'static str,"),
        "CrossFillAuditEvent must declare the resolution field"
    );
    assert!(
        AUDIT_SRC.contains("pub retry_attempts: u32,"),
        "CrossFillAuditEvent must declare the retry_attempts field"
    );
    let sample_start = AUDIT_SRC
        .find("fn sample_event()")
        .expect("sample_event present in audit.rs tests");
    let sample = &AUDIT_SRC[sample_start..];
    let end = sample.find("}\n    }").unwrap_or(sample.len());
    let window = &sample[..end];
    assert!(
        window.contains("resolution: \"cross_fill\","),
        "sample_event must carry the resolution field"
    );
    assert!(
        window.contains("retry_attempts: 1,"),
        "sample_event must carry the retry_attempts field"
    );
}
