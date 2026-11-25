//! QuestDB HTTP pools in `deploy/docker/docker-compose.yml` (test gap T3,
//! PR DP-1, 2026-11-06).
//!
//! Measured live 2026-11-06 08:31–09:13 IST: QuestDB ran with
//! `http.worker.count=0` and `shared.network.worker.count=1`, so the disk-pressure
//! archiver's `/exp` export of one `market_depth` hour held the ONE thread every
//! ILP `/write` and `/exec` needed for 4–5 minutes at a time (14 BOOT-01, 2.9M
//! tick rows rescued to spill). On 2026-11-03 the one-slot `/exp` connection
//! pool had already starved the operator portal. This guard pins both repairs:
//!
//! 1. `QDB_HTTP_EXPORT_CONNECTION_LIMIT` is present and at least 2.
//! 2. `QDB_HTTP_WORKER_COUNT` is present, interpolated, and defaults to at
//!    least 2, so HTTP has a pool of its own and an export cannot hold the
//!    thread a `/write` needs.
//! 3. `QDB_SHARED_NETWORK_WORKER_COUNT` is present and defaults to at least 2.
//! 4. The HTTP worker count exceeds the export connection limit (PR DP
//!    review M6), so exports can never hold every HTTP worker.

use std::path::PathBuf;

fn compose() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/docker/docker-compose.yml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The `QDB_*` value lines of the `tv-questdb` service's `environment:` block.
fn questdb_env(compose: &str) -> Vec<(String, String)> {
    let mut in_service = false;
    let mut in_env = false;
    let mut out = Vec::new();
    for line in compose.lines() {
        let indent = line.len() - line.trim_start().len();
        let body = line.trim();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        if indent == 2 {
            in_service = body == "tv-questdb:";
            in_env = false;
            continue;
        }
        if !in_service {
            continue;
        }
        if indent == 4 {
            in_env = body == "environment:";
            continue;
        }
        if in_env
            && indent == 6
            && let Some((key, raw)) = body.split_once(':')
            && key.starts_with("QDB_")
        {
            out.push((key.to_string(), raw.trim().trim_matches('"').to_string()));
        }
    }
    out
}

/// The number a compose value resolves to with no `.env`: the literal, or
/// the default of `${VAR:-default}`.
fn default_number(raw: &str) -> u64 {
    let value = match raw.strip_prefix("${") {
        Some(inner) => inner
            .split('}')
            .next()
            .and_then(|i| i.split_once(":-"))
            .map(|(_, d)| d)
            .unwrap_or_else(|| panic!("{raw} has no :- default")),
        None => raw,
    };
    value
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{raw} does not default to a whole number"))
}

fn value_of<'a>(env: &'a [(String, String)], key: &str) -> &'a str {
    env.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("tv-questdb environment must carry {key}"))
}

#[test]
fn http_export_connection_limit_is_at_least_two() {
    let env = questdb_env(&compose());
    let limit = default_number(value_of(&env, "QDB_HTTP_EXPORT_CONNECTION_LIMIT"));
    assert!(
        limit >= 2,
        "QDB_HTTP_EXPORT_CONNECTION_LIMIT is {limit}: one slot lets the archiver's \
         export starve every other /exp reader (2026-11-03)"
    );
}

#[test]
fn http_has_its_own_worker_pool_of_at_least_two() {
    let env = questdb_env(&compose());
    let raw = value_of(&env, "QDB_HTTP_WORKER_COUNT");
    assert!(
        raw.starts_with("${") && raw.contains(":-"),
        "QDB_HTTP_WORKER_COUNT must be `${{VAR:-default}}` so rollback is config only: {raw}"
    );
    let n = default_number(raw);
    assert!(
        n >= 2,
        "QDB_HTTP_WORKER_COUNT defaults to {n}: with 0 HTTP rides the shared network \
         pool and with 1 an /exp export holds the thread every ILP /write needs \
         (2026-11-06, 4-5 minutes per market_depth hour)"
    );
}

// Regression: PR DP review M6 — 2 HTTP workers under an export limit of 4
// let two /exp streams hold every worker and starve /write and /exec again.
#[test]
fn http_workers_exceed_the_export_connection_limit() {
    let env = questdb_env(&compose());
    let workers = default_number(value_of(&env, "QDB_HTTP_WORKER_COUNT"));
    let exports = default_number(value_of(&env, "QDB_HTTP_EXPORT_CONNECTION_LIMIT"));
    assert!(
        workers > exports,
        "QDB_HTTP_WORKER_COUNT ({workers}) must exceed QDB_HTTP_EXPORT_CONNECTION_LIMIT \
         ({exports}) so exports can never hold every HTTP worker"
    );
}

#[test]
fn shared_network_pool_is_not_left_at_one() {
    let env = questdb_env(&compose());
    let n = default_number(value_of(&env, "QDB_SHARED_NETWORK_WORKER_COUNT"));
    assert!(
        n >= 2,
        "QDB_SHARED_NETWORK_WORKER_COUNT defaults to {n}; host tuning's core isolation \
         must not be able to pull it to one thread"
    );
}

#[test]
fn the_parser_reads_the_questdb_block_and_nothing_else() {
    let env = questdb_env(&compose());
    assert!(env.len() >= 10, "too few QDB_* keys read: {env:?}");
    assert!(env.iter().all(|(k, _)| k.starts_with("QDB_")));
    assert_eq!(default_number("${X:-3}"), 3);
    assert_eq!(default_number("4"), 4);
}
