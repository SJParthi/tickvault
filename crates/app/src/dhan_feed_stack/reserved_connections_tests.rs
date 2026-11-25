//! Audit PR M / M4 (2026-10-26): `tv_dhan_feed_stack_connections` follows the
//! pool's reservations, including the main-feed widen after the contract
//! attach. It read 1 all session on 2026-10-26 (2,156 samples) while five
//! main-feed sockets were up, because it was written once from the boot plan.

use super::*;

fn rendered_main_feed(handle: &metrics_exporter_prometheus::PrometheusHandle) -> Option<String> {
    handle
        .render()
        .lines()
        .find(|l| l.starts_with("tv_dhan_feed_stack_connections{endpoint=\"main_feed\"}"))
        .map(|l| l.rsplit(' ').next().unwrap_or_default().to_owned())
}

// Regression: 2026-10-26 — the reserved-connections gauge stayed at the boot
// plan's 1 main-feed socket after the session widened to 5.
#[test]
fn test_regression_reserved_connections_gauge_follows_the_widen() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let mut pool = PoolSupervisor::new();
    metrics::with_local_recorder(&recorder, || {
        pool.admit(DhanEndpointType::MainFeed, Instant::now())
            .expect("first main-feed slot");
        publish_reserved_connections(&pool);
    });
    assert_eq!(rendered_main_feed(&handle).as_deref(), Some("1"));
    metrics::with_local_recorder(&recorder, || {
        for _ in 0..4 {
            pool.admit(DhanEndpointType::MainFeed, Instant::now())
                .expect("widen slot");
        }
        publish_reserved_connections(&pool);
    });
    assert_eq!(rendered_main_feed(&handle).as_deref(), Some("5"));
}

/// Every dial batch republishes the gauge, so the widen (which dials through
/// `dial_planned_connections` long after bring-up) moves it. Bite-checked by
/// removing the call: this test fails.
#[test]
fn test_regression_every_dial_batch_republishes_the_reserved_gauge() {
    let src = include_str!("../dhan_feed_stack.rs");
    let from = src
        .find("fn dial_planned_connections(plan: FeedStackPlan")
        .expect("dial_planned_connections");
    let body = &src[from..];
    let end = body.find("\n}\n").expect("end of dial_planned_connections");
    assert!(
        body[..end].contains("publish_reserved_connections(pool);"),
        "dial_planned_connections must republish the reserved-connections gauge"
    );
}
