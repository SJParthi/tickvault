//! AWS SDK client construction helpers (thin, uncovered-by-design).
//!
//! These build real AWS clients from the Lambda execution-role environment —
//! exercised only in a live Lambda invoke (UNPROVEN until deploy; the pure
//! logic they feed is what the unit tests cover).

use aws_config::{BehaviorVersion, Region, SdkConfig};

/// Load the default SDK config (region/credentials from the Lambda env).
pub async fn sdk_config() -> SdkConfig {
    aws_config::defaults(BehaviorVersion::latest()).load().await
}

/// AWS Budgets is served from us-east-1 only (`budgets.amazonaws.com`).
/// 2026-10-22 (plan item 63, Quote 37): replaces the Cost Explorer client
/// (also us-east-1 only), so the month-to-date spend lookups are free.
pub async fn budgets_us_east_1() -> aws_sdk_budgets::Client {
    let base = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .load()
        .await;
    aws_sdk_budgets::Client::new(&base)
}

pub fn sns(config: &SdkConfig) -> aws_sdk_sns::Client {
    aws_sdk_sns::Client::new(config)
}

pub fn ec2(config: &SdkConfig) -> aws_sdk_ec2::Client {
    aws_sdk_ec2::Client::new(config)
}

pub fn ssm(config: &SdkConfig) -> aws_sdk_ssm::Client {
    aws_sdk_ssm::Client::new(config)
}

pub fn cloudwatch(config: &SdkConfig) -> aws_sdk_cloudwatch::Client {
    aws_sdk_cloudwatch::Client::new(config)
}

/// Lambda invoke client — the console FRONT's only AWS call surface
/// (legacy parity: `boto3.client("lambda")` invoking the back relay).
pub fn lambda(config: &SdkConfig) -> aws_sdk_lambda::Client {
    aws_sdk_lambda::Client::new(config)
}

pub fn eventbridge(config: &SdkConfig) -> aws_sdk_eventbridge::Client {
    aws_sdk_eventbridge::Client::new(config)
}
