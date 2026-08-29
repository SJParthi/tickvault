//! The drain's loss series must be SEEDED, or an absent one reads as healthy.
//!
//! # The sweep that proved this, and why a built handle is not enough
//!
//! On 2026-08-29 a `cloudwatch list-metrics` sweep of `Tickvault/Prod` was
//! compared against the EMF selector. The selector names 104 metrics; the
//! account held 86. Thirty-four selected names had **never published a single
//! datapoint**, and `tv_dhan_feed_abandoned_bytes_total` was one of them —
//! even though `counters()` has built a `metrics::Counter` handle for it on
//! every drain since the counter was introduced.
//!
//! That is the whole lesson. Building a handle registers a key with the
//! recorder; it does not emit a sample. The CloudWatch agent computes a
//! counter as the DELTA between consecutive samples and drops the first
//! sample of a series it has never seen, so a counter that is never
//! incremented is never published — and **an absent series is
//! indistinguishable from a healthy zero one**.
//!
//! The consequence is not a missing chart. An alarm over a metric that never
//! publishes sits in `OK` forever and cannot fire: a permanently-green dead
//! monitor, the class this repository has already retired twice. Seeding
//! turns one unreadable silence into two distinguishable answers — "the drain
//! ran and abandoned nothing" versus "the drain never ran".
//!
//! # Why the seed lives in `run_frame_drain` and not at boot
//!
//! A central boot-time seeder would publish a confident zero for every
//! subsystem whether or not it is running, which is a NEW false-OK and a
//! worse one: it reads as positive evidence of health for work nothing is
//! doing. These series must appear exactly when the drain that owns them is
//! alive, so the seed is called from the drain itself.

const STACK: &str = include_str!("../src/dhan_feed_stack.rs");

/// The body of a named free function, bounded so the window cannot widen to
/// the whole file and let this test pass vacuously.
fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("{signature} must exist in dhan_feed_stack.rs"));
    let rest = &source[start + signature.len()..];
    // The next item at column 0 ends the body. Both `fn ` and `async fn ` are
    // matched so a neighbouring async item cannot silently extend the window.
    let end = rest
        .find("\nfn ")
        .into_iter()
        .chain(rest.find("\nasync fn "))
        .chain(rest.find("\npub fn "))
        .min()
        .map_or(rest.len(), |offset| offset);
    &rest[..end]
}

#[test]
fn the_drain_seeds_its_loss_series_before_the_first_frame() {
    let body = function_body(STACK, "fn seed_drain_loss_baselines()");

    assert!(
        body.contains("abandoned_bytes.increment(0)"),
        "tv_dhan_feed_abandoned_bytes_total is not seeded. Its handle is built in \
         counters(), which is NOT enough — it had a handle for months and still \
         published nothing. Only an explicit increment(0) puts the series on the wire."
    );

    for writer in ["\"tick\"", "\"depth\""] {
        assert!(
            body.contains(&format!(
                "OFFLOAD_SHUTDOWN_INCOMPLETE_COUNTER, \"writer\" => {writer}"
            )) && body.contains("increment(0)"),
            "the offload shutdown-abandonment series is not seeded for writer={writer}. \
             An abandoned writer queue is a permanent loss of everything it held; a \
             series that never publishes cannot report it and cannot be alarmed on."
        );
    }
}

#[test]
fn the_seed_is_actually_called_from_the_running_drain() {
    // The seeding function existing is worth nothing if nothing invokes it —
    // that is precisely the shape of the defect being fixed here, one level up.
    let drain = function_body(STACK, "async fn run_frame_drain(");
    assert!(
        drain.contains("seed_drain_loss_baselines()"),
        "run_frame_drain does not call seed_drain_loss_baselines(). An unseeded \
         drain republishes the 2026-08-29 state: the loss counters exist in code, \
         are EMF-selected, are paid for, and are invisible in CloudWatch."
    );
}

#[test]
fn the_seed_is_not_hoisted_into_a_boot_wide_seeder() {
    // Guards the DESIGN decision, not just the presence. Seeding every
    // subsystem at boot would publish a confident zero for a drain that never
    // ran — positive evidence of health for work nothing is doing, which is
    // strictly worse than the silence it replaces.
    let body = function_body(STACK, "fn seed_drain_loss_baselines()");
    let seeds = body.matches("increment(0)").count();
    assert!(
        (1..=8).contains(&seeds),
        "seed_drain_loss_baselines() seeds {seeds} series. It is scoped to counters \
         the DRAIN owns; a large count means unrelated subsystems were folded in, \
         which publishes health for components that may not be running."
    );
}

#[test]
fn the_body_extractor_cannot_pass_vacuously() {
    // Bite-proof for the harness itself: a window that silently widened to the
    // whole file would make every assertion above pass regardless of the code.
    let body = function_body(STACK, "fn seed_drain_loss_baselines()");
    assert!(
        body.len() < STACK.len() / 4,
        "the extracted body is {} bytes of a {} byte file — the window widened \
         and these tests would pass vacuously",
        body.len(),
        STACK.len()
    );
    assert!(
        !function_body(STACK, "async fn run_frame_drain(").contains("fn seed_drain_loss_baselines"),
        "run_frame_drain's window swallowed the seeding function's definition, so \
         'the drain calls the seed' would pass on the definition alone"
    );
}
