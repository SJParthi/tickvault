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

/// Cost Explorer is a us-east-1-only API — Python parity:
/// `boto3.client('ce', region_name='us-east-1')`.
pub async fn cost_explorer_us_east_1() -> aws_sdk_costexplorer::Client {
    let base = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .load()
        .await;
    aws_sdk_costexplorer::Client::new(&base)
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

pub fn eventbridge(config: &SdkConfig) -> aws_sdk_eventbridge::Client {
    aws_sdk_eventbridge::Client::new(config)
}
