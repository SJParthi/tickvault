//! Benchmark: TOML config parsing latency.
//!
//! Measures `ApplicationConfig` deserialization from TOML string.
//! Budget from quality/benchmark-budgets.toml: < 10ms per parse.
//! This is cold-path (boot only) but budgeted to prevent regressions.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use dhan_live_trader_common::config::ApplicationConfig;

/// Embedded copy of a realistic base.toml for deterministic benchmarking.
/// Avoids file I/O in the benchmark loop.
const BASE_TOML: &str = r#"
[trading]
market_open_time = "09:00:00"
market_close_time = "15:30:00"
order_cutoff_time = "15:29:00"
data_collection_start = "09:00:00"
data_collection_end = "15:30:00"
timezone = "Asia/Kolkata"
max_orders_per_second = 10

[[trading.nse_holidays]]
date = "2026-01-26"
name = "Republic Day"

[[trading.nse_holidays]]
date = "2026-03-03"
name = "Holi"

[[trading.nse_holidays]]
date = "2026-03-26"
name = "Shri Ram Navami"

[[trading.nse_holidays]]
date = "2026-03-31"
name = "Shri Mahavir Jayanti"

[[trading.nse_holidays]]
date = "2026-04-03"
name = "Good Friday"

[[trading.nse_holidays]]
date = "2026-04-14"
name = "Dr. Ambedkar Jayanti"

[[trading.nse_holidays]]
date = "2026-05-01"
name = "Maharashtra Day"

[[trading.nse_holidays]]
date = "2026-05-28"
name = "Bakri Eid"

[[trading.nse_holidays]]
date = "2026-06-26"
name = "Muharram"

[[trading.nse_holidays]]
date = "2026-09-14"
name = "Ganesh Chaturthi"

[[trading.nse_holidays]]
date = "2026-10-02"
name = "Gandhi Jayanti"

[[trading.nse_holidays]]
date = "2026-10-20"
name = "Dussehra"

[[trading.nse_holidays]]
date = "2026-11-10"
name = "Diwali Balipratipada"

[[trading.nse_holidays]]
date = "2026-11-24"
name = "Guru Nanak Jayanti"

[[trading.nse_holidays]]
date = "2026-12-25"
name = "Christmas"

[[trading.muhurat_trading_dates]]
date = "2026-11-08"
name = "Diwali 2026"

[dhan]
websocket_url = "wss://api-feed.dhan.co"
order_update_websocket_url = "wss://api-order-update.dhan.co"
rest_api_base_url = "https://api.dhan.co/v2"
auth_base_url = "https://auth.dhan.co"
instrument_csv_url = "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
instrument_csv_fallback_url = "https://images.dhan.co/api-data/api-scrip-master.csv"
max_instruments_per_connection = 5000
max_websocket_connections = 5

[questdb]
host = "dlt-questdb"
http_port = 9000
pg_port = 8812
ilp_port = 9009

[valkey]
host = "dlt-valkey"
port = 6379
max_connections = 16

[prometheus]
host = "dlt-prometheus"
port = 9090

[websocket]
ping_interval_secs = 10
pong_timeout_secs = 10
max_consecutive_pong_failures = 2
reconnect_initial_delay_ms = 500
reconnect_max_delay_ms = 30000
reconnect_max_attempts = 10
subscription_batch_size = 100
connection_stagger_ms = 10000

[network]
request_timeout_ms = 10000
websocket_connect_timeout_ms = 10000
retry_initial_delay_ms = 100
retry_max_delay_ms = 30000
retry_max_attempts = 5

[token]
refresh_before_expiry_hours = 1
token_validity_hours = 24

[risk]
max_daily_loss_percent = 2.0
max_position_size_lots = 50

[strategy]
config_path = "config/strategies.toml"
capital = 1000000.0
dry_run = true

[logging]
level = "info"
format = "json"

[instrument]
daily_download_time = "08:00:00"
csv_cache_directory = "/app/data/instrument-cache"
csv_cache_filename = "api-scrip-master-detailed.csv"
csv_download_timeout_secs = 120
build_window_start = "09:00:00"
build_window_end = "15:30:00"

[api]
host = "0.0.0.0"
port = 3001

[subscription]
feed_mode = "Full"
subscribe_index_derivatives = true
subscribe_stock_derivatives = true
subscribe_display_indices = true
subscribe_stock_equities = true
stock_atm_strikes_above = 10
stock_atm_strikes_below = 10
stock_default_atm_fallback_enabled = true

[notification]
telegram_api_base_url = "https://api.telegram.org"
send_timeout_ms = 10000
sns_enabled = false

[observability]
metrics_port = 9091
otlp_endpoint = "http://dlt-jaeger:4317"
metrics_enabled = true
tracing_enabled = true

[historical]
enabled = true
lookback_days = 90
request_timeout_secs = 30
max_retries = 3
request_delay_ms = 500

[index_constituency]
enabled = true
download_timeout_secs = 30
max_concurrent_downloads = 5
inter_batch_delay_ms = 0
"#;

fn bench_toml_parse(c: &mut Criterion) {
    // Verify the TOML is valid before benchmarking.
    let _: ApplicationConfig = toml::from_str(BASE_TOML).unwrap();

    c.bench_function("config/toml_load", |b| {
        b.iter(|| {
            let config: ApplicationConfig = toml::from_str(black_box(BASE_TOML)).unwrap();
            black_box(&config);
        });
    });
}

fn bench_toml_parse_and_validate(c: &mut Criterion) {
    c.bench_function("config/toml_load_and_validate", |b| {
        b.iter(|| {
            let config: ApplicationConfig = toml::from_str(black_box(BASE_TOML)).unwrap();
            config.validate().unwrap();
            black_box(&config);
        });
    });
}

criterion_group!(benches, bench_toml_parse, bench_toml_parse_and_validate);
criterion_main!(benches);
