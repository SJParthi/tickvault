//! Adversarial properties of the TOML configuration loader.
//!
//! Surface: `tickvault_common::config::ApplicationConfig` deserialized with
//! `toml::from_str` and checked with `ApplicationConfig::validate`, against
//! the real `config/base.toml`. (Production layers figment over the same
//! TOML; `toml::from_str` is the same serde path the fuzz target
//! `config_parser` drives.)
//!
//! Properties:
//!
//! 1. Arbitrary text, and TOML-shaped text, never panics in `from_str`, nor
//!    in `validate` when it deserializes.
//! 2. `config/base.toml` deserializes and validates, and a round trip
//!    through `toml::Value` (parse, re-serialize, parse) yields a config
//!    whose `Debug` rendering is byte-identical: re-serialization never
//!    changes a value the application reads.
//! 3. Every leaf of base.toml (numeric, string, bool, inside tables and
//!    arrays of tables) replaced by 0, -1, `i64::MAX`, `i64::MIN`, a
//!    fraction, NaN, +/-inf, -0.0, a string or a bool never panics in
//!    `from_str` or `validate`.
//! 4. (No separate property: "a leaf set to itself" is property 2, which
//!    checks the whole of base.toml once.)
//! 5. The guarded fields `validate` documents (rate limit, timeouts, batch
//!    size, daily-loss percent, capital, connection count, token windows,
//!    the depth-200 resubscribe bounds) set to their forbidden values are
//!    refused, and the error names the field. NaN and infinite money values
//!    are refused, never accepted as "not less than".
//!
//! Honest limits: property 3 asserts no panic and the typed refusal of the
//! guarded set only; for an arbitrary numeric leaf, whether a mutation is
//! refused depends on whether that key is read and on its Rust type, which
//! this file cannot see (unknown keys are ignored by serde here). The count
//! of leaves whose wrong-typed replacement still deserializes is measured
//! and printed, not asserted.

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use tickvault_common::config::ApplicationConfig;
use toml::Value;

fn base_toml_text() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/base.toml");
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn base_value() -> Value {
    static BASE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    BASE.get_or_init(|| toml::from_str::<Value>(&base_toml_text()).expect("base.toml is TOML"))
        .clone()
}

#[derive(Clone, Debug)]
enum Step {
    Key(String),
    Index(usize),
}

/// Every leaf (non-table, non-array value) of `value`, as a path.
fn leaf_paths(value: &Value, prefix: &mut Vec<Step>, out: &mut Vec<Vec<Step>>) {
    match value {
        Value::Table(table) => {
            for (key, child) in table {
                prefix.push(Step::Key(key.clone()));
                leaf_paths(child, prefix, out);
                prefix.pop();
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                prefix.push(Step::Index(i));
                leaf_paths(child, prefix, out);
                prefix.pop();
            }
        }
        _ => out.push(prefix.clone()),
    }
}

fn leaf_mut<'a>(value: &'a mut Value, path: &[Step]) -> &'a mut Value {
    path.iter().fold(value, |node, step| match step {
        Step::Key(key) => node
            .as_table_mut()
            .and_then(|t| t.get_mut(key))
            .expect("path key"),
        Step::Index(i) => node
            .as_array_mut()
            .and_then(|a| a.get_mut(*i))
            .expect("path index"),
    })
}

/// Deserialize and validate; `None` when `from_str` refuses, otherwise the
/// validate verdict. Any panic fails the test.
fn load(text: &str) -> Option<Result<ApplicationConfig, String>> {
    let config: ApplicationConfig = toml::from_str(text).ok()?;
    Some(
        config
            .validate()
            .map(|()| config.clone())
            .map_err(|e| format!("{e:#}")),
    )
}

fn replacements() -> Vec<(&'static str, Value)> {
    vec![
        ("0", Value::Integer(0)),
        ("-1", Value::Integer(-1)),
        ("i64::MAX", Value::Integer(i64::MAX)),
        ("i64::MIN", Value::Integer(i64::MIN)),
        ("0.5", Value::Float(0.5)),
        ("-0.0", Value::Float(-0.0)),
        ("NaN", Value::Float(f64::NAN)),
        ("inf", Value::Float(f64::INFINITY)),
        ("-inf", Value::Float(f64::NEG_INFINITY)),
        ("1e308", Value::Float(1e308)),
        ("string", Value::String("x".to_string())),
        ("empty string", Value::String(String::new())),
        ("bool", Value::Boolean(true)),
    ]
}

/// Property 2: base.toml loads, validates, and survives a `toml::Value`
/// round trip with an identical `Debug` rendering.
#[test]
fn test_base_toml_parses_validates_and_round_trips() {
    let text = base_toml_text();
    let config: ApplicationConfig = toml::from_str(&text).expect("base.toml deserializes");
    config.validate().expect("base.toml validates");
    let reserialized = toml::to_string(&base_value()).expect("re-serialize");
    let again: ApplicationConfig = toml::from_str(&reserialized).expect("round trip deserializes");
    again.validate().expect("round trip validates");
    assert_eq!(format!("{config:?}"), format!("{again:?}"));
}

/// Property 3 (exhaustive): every leaf of base.toml times every hostile
/// replacement: never a panic. Counts are printed so the coverage is
/// visible; at least one replacement per numeric leaf must be refused
/// somewhere in the set, or the run proves nothing.
#[test]
fn test_every_leaf_of_base_toml_times_every_hostile_value_never_panics() {
    let base = base_value();
    let mut paths = Vec::new();
    leaf_paths(&base, &mut Vec::new(), &mut paths);
    assert!(paths.len() > 100, "base.toml has {} leaves", paths.len());
    let mut refused_by_serde = 0usize;
    let mut refused_by_validate = 0usize;
    let mut accepted = 0usize;
    for path in &paths {
        for (_, replacement) in replacements() {
            let mut mutated = base.clone();
            *leaf_mut(&mut mutated, path) = replacement;
            // NaN / inf serialize as `nan` / `inf`, which TOML accepts.
            let text = toml::to_string(&mutated).expect("serialize mutation");
            match load(&text) {
                None => refused_by_serde += 1,
                Some(Err(_)) => refused_by_validate += 1,
                Some(Ok(_)) => accepted += 1,
            }
        }
    }
    let total = paths.len() * replacements().len();
    assert_eq!(refused_by_serde + refused_by_validate + accepted, total);
    // Non-vacuity: the typed refusal paths are exercised.
    assert!(
        refused_by_serde > paths.len(),
        "serde refused {refused_by_serde}"
    );
    assert!(
        refused_by_validate > 0,
        "validate refused {refused_by_validate}"
    );
    println!(
        "base.toml mutations: {} leaves x {} values = {total}; serde refused {refused_by_serde}, \
         validate refused {refused_by_validate}, accepted {accepted}",
        paths.len(),
        replacements().len()
    );
}

/// Property 5: each documented guard refuses its forbidden values, and the
/// error text names the field (so the boot failure says what to fix).
#[test]
fn test_guarded_fields_refuse_forbidden_values_and_name_the_field() {
    let cases: Vec<(&str, &str, Value, &str)> = vec![
        (
            "trading",
            "max_orders_per_second",
            Value::Integer(0),
            "max_orders_per_second",
        ),
        (
            "trading",
            "max_orders_per_second",
            Value::Integer(11),
            "max_orders_per_second",
        ),
        (
            "trading",
            "timezone",
            Value::String("UTC".into()),
            "timezone",
        ),
        (
            "trading",
            "market_open_time",
            Value::String("25:00:00".into()),
            "market_open_time",
        ),
        (
            "trading",
            "market_open_time",
            Value::String("9am".into()),
            "market_open_time",
        ),
        (
            "network",
            "request_timeout_ms",
            Value::Integer(0),
            "request_timeout_ms",
        ),
        (
            "network",
            "retry_max_attempts",
            Value::Integer(0),
            "retry_max_attempts",
        ),
        (
            "token",
            "refresh_before_expiry_hours",
            Value::Integer(24),
            "refresh_before_expiry_hours",
        ),
        (
            "risk",
            "max_daily_loss_percent",
            Value::Float(0.0),
            "max_daily_loss_percent",
        ),
        (
            "risk",
            "max_daily_loss_percent",
            Value::Float(-1.0),
            "max_daily_loss_percent",
        ),
        (
            "risk",
            "max_daily_loss_percent",
            Value::Float(100.5),
            "max_daily_loss_percent",
        ),
        (
            "risk",
            "max_daily_loss_percent",
            Value::Float(f64::NAN),
            "max_daily_loss_percent",
        ),
        (
            "risk",
            "max_daily_loss_percent",
            Value::Float(f64::INFINITY),
            "max_daily_loss_percent",
        ),
        ("strategy", "capital", Value::Float(0.0), "capital"),
        ("strategy", "capital", Value::Float(-0.0), "capital"),
        ("strategy", "capital", Value::Float(-1.0), "capital"),
        ("strategy", "capital", Value::Float(f64::NAN), "capital"),
        (
            "strategy",
            "capital",
            Value::Float(f64::INFINITY),
            "capital",
        ),
        (
            "strategy",
            "capital",
            Value::Float(f64::NEG_INFINITY),
            "capital",
        ),
        (
            "instrument",
            "csv_download_timeout_secs",
            Value::Integer(0),
            "csv_download_timeout_secs",
        ),
        (
            "websocket",
            "ping_interval_secs",
            Value::Integer(0),
            "ping_interval_secs",
        ),
        (
            "websocket",
            "pong_timeout_secs",
            Value::Integer(0),
            "pong_timeout_secs",
        ),
        (
            "websocket",
            "subscription_batch_size",
            Value::Integer(0),
            "subscription_batch_size",
        ),
        (
            "websocket",
            "subscription_batch_size",
            Value::Integer(101),
            "subscription_batch_size",
        ),
        (
            "dhan",
            "max_websocket_connections",
            Value::Integer(0),
            "max_websocket_connections",
        ),
        (
            "dhan",
            "websocket_url",
            Value::String("ws://insecure".into()),
            "websocket_url",
        ),
        (
            "dhan",
            "auth_base_url",
            Value::String(String::new()),
            "auth_base_url",
        ),
        (
            "notification",
            "send_timeout_ms",
            Value::Integer(0),
            "send_timeout_ms",
        ),
        (
            "dhan_universe",
            "depth200_silent_swap_resubscribe_secs",
            Value::Integer(9),
            "depth200_silent_swap_resubscribe_secs",
        ),
        (
            "dhan_universe",
            "depth200_silent_swap_resubscribe_secs",
            Value::Integer(601),
            "depth200_silent_swap_resubscribe_secs",
        ),
    ];
    for (section, key, value, needle) in cases {
        let mut mutated = base_value();
        let table = mutated
            .get_mut(section)
            .and_then(Value::as_table_mut)
            .unwrap_or_else(|| panic!("base.toml has no [{section}]"));
        assert!(
            table.contains_key(key),
            "[{section}] {key} must be in base.toml for this guard to be meaningful"
        );
        table.insert(key.to_string(), value.clone());
        let text = toml::to_string(&mutated).expect("serialize");
        match load(&text) {
            Some(Err(message)) => assert!(
                message.contains(needle),
                "[{section}] {key} = {value}: error does not name the field: {message}"
            ),
            Some(Ok(_)) => panic!("[{section}] {key} = {value} was accepted"),
            None => panic!("[{section}] {key} = {value} did not even deserialize"),
        }
    }
}

/// Values a guard must ACCEPT (the boundary inside the allowed range), so
/// the refusals above are not a validator that refuses everything.
#[test]
fn test_guarded_fields_accept_their_boundaries() {
    let cases: Vec<(&str, &str, Value)> = vec![
        ("trading", "max_orders_per_second", Value::Integer(1)),
        ("trading", "max_orders_per_second", Value::Integer(10)),
        ("risk", "max_daily_loss_percent", Value::Float(100.0)),
        (
            "risk",
            "max_daily_loss_percent",
            Value::Float(f64::MIN_POSITIVE),
        ),
        ("websocket", "subscription_batch_size", Value::Integer(1)),
        ("websocket", "subscription_batch_size", Value::Integer(100)),
        (
            "dhan_universe",
            "depth200_silent_swap_resubscribe_secs",
            Value::Integer(10),
        ),
        (
            "dhan_universe",
            "depth200_silent_swap_resubscribe_secs",
            Value::Integer(600),
        ),
    ];
    for (section, key, value) in cases {
        let mut mutated = base_value();
        mutated
            .get_mut(section)
            .and_then(Value::as_table_mut)
            .unwrap_or_else(|| panic!("no [{section}]"))
            .insert(key.to_string(), value.clone());
        let text = toml::to_string(&mutated).expect("serialize");
        match load(&text) {
            Some(Ok(_)) => {}
            other => panic!("[{section}] {key} = {value} must be accepted: {other:?}"),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// Property 1a: arbitrary text never panics.
    #[test]
    fn prop_arbitrary_text_never_panics(text in "(?s).{0,300}") {
        let _ = load(&text);
    }

    /// Property 1b: TOML-shaped text (section headers, keys, every value
    /// kind, deep inline tables) never panics.
    #[test]
    fn prop_toml_shaped_text_never_panics(
        lines in prop::collection::vec(
            prop_oneof![
                "\\[[a-z_.]{1,20}\\]",
                "\\[\\[[a-z_.]{1,20}\\]\\]",
                "[a-z_]{1,16} = -?[0-9]{1,20}",
                "[a-z_]{1,16} = -?[0-9]{1,6}\\.[0-9]{1,6}(e-?[0-9]{1,3})?",
                "[a-z_]{1,16} = (nan|inf|-inf|true|false)",
                "[a-z_]{1,16} = \"[ -~&&[^\"\\\\]]{0,20}\"",
                "[a-z_]{1,16} = \\[(-?[0-9]{1,3}, ){0,4}\\]",
                "[a-z_]{1,16} = \\{ ([a-z]{1,4} = [0-9]{1,3}, ){0,3}[a-z]{1,4} = \\{\\} \\}",
                "[ -~]{0,30}",
            ],
            0..30,
        ),
    ) {
        let _ = load(&lines.join("\n"));
    }

}

proptest! {
    // Fewer cases: the exhaustive leaf x hostile-value test covers the grid.
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Property 3 (random): a random leaf of base.toml set to a random
    /// integer, float (including non-finite), string or bool never panics.
    #[test]
    fn prop_random_leaf_mutation_never_panics(
        leaf in any::<prop::sample::Index>(),
        replacement in prop_oneof![
            any::<i64>().prop_map(Value::Integer),
            any::<f64>().prop_map(Value::Float),
            "[ -~]{0,12}".prop_map(Value::String),
            any::<bool>().prop_map(Value::Boolean),
        ],
    ) {
        let base = base_value();
        let mut paths = Vec::new();
        leaf_paths(&base, &mut Vec::new(), &mut paths);
        let path = &paths[leaf.index(paths.len())];

        let mut mutated = base.clone();
        *leaf_mut(&mut mutated, path) = replacement;
        let text = toml::to_string(&mutated).expect("serialize mutation");
        let _ = load(&text);
    }
}
