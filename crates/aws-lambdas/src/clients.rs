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

/// Cost Explorer is served from us-east-1 only (`ce.us-east-1.amazonaws.com`).
/// 2026-11-11 (noise lock §2.18): used by the daily AWS waste check for its
/// ONE month-to-date `GetCostAndUsage` grouped by service; nothing else
/// calls Cost Explorer.
// TEST-EXEMPT: live config load; the client it returns is built by cost_explorer_from, which is tested.
pub async fn cost_explorer_us_east_1() -> aws_sdk_costexplorer::Client {
    let base = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .load()
        .await;
    cost_explorer_from(&base)
}

/// A Cost Explorer client with SDK retries switched OFF (one attempt).
/// Every `GetCostAndUsage` request is billed ($0.01), and noise lock §2.18
/// allows ONE per run: the SDK's default standard retry would send up to
/// three requests on a throttle or a 5xx.
pub fn cost_explorer_from(base: &SdkConfig) -> aws_sdk_costexplorer::Client {
    let conf = aws_sdk_costexplorer::config::Builder::from(base)
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .build();
    aws_sdk_costexplorer::Client::from_conf(conf)
}

pub fn sns(config: &SdkConfig) -> aws_sdk_sns::Client {
    aws_sdk_sns::Client::new(config)
}

/// An SNS client with SDK retries switched OFF (one attempt), for the daily
/// AWS waste check (noise lock §2.18: at most one message a day, never a
/// repeat). `sns:Publish` on a standard topic is not idempotent: with the
/// default standard retry, a publish SNS accepted but whose reply was lost
/// is sent again and the owner gets the same message twice.
pub fn sns_single_attempt(base: &SdkConfig) -> aws_sdk_sns::Client {
    let conf = aws_sdk_sns::config::Builder::from(base)
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .build();
    aws_sdk_sns::Client::from_conf(conf)
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

/// S3 client — the operator-control Lambda's read of the published movers
/// universe (plan PR MV, one GetObject on one key).
pub fn s3(config: &SdkConfig) -> aws_sdk_s3::Client {
    aws_sdk_s3::Client::new(config)
}

/// An S3 client with SDK retries switched OFF (one attempt), for the waste
/// check's create-only day claims. With retries on, a claim that S3 created
/// but whose reply was lost is retried, the retry gets 412, and the run would
/// read its own claim as "another run already claimed today".
pub fn s3_single_attempt(base: &SdkConfig) -> aws_sdk_s3::Client {
    let conf = aws_sdk_s3::config::Builder::from(base)
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_s3_client_builds_from_a_static_config_without_network() {
        let config = SdkConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("ap-south-1"))
            .build();
        let client = s3(&config);
        assert_eq!(
            client.config().region().map(|r| r.as_ref()),
            Some("ap-south-1")
        );
    }

    #[test]
    fn test_cost_explorer_from_makes_exactly_one_attempt() {
        // A base config that retries (standard mode, 3 attempts): the waste
        // check may still send only one billed request per run.
        let config = SdkConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(3))
            .build();
        let client = cost_explorer_from(&config);
        assert_eq!(
            client.config().retry_config().map(|r| r.max_attempts()),
            Some(1)
        );
        assert_eq!(
            client.config().region().map(|r| r.as_ref()),
            Some("us-east-1")
        );
    }

    fn retrying_base() -> SdkConfig {
        SdkConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("ap-south-1"))
            .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(3))
            .build()
    }

    #[test]
    fn test_sns_single_attempt_makes_exactly_one_attempt() {
        let base = retrying_base();
        // The plain client keeps the base's 3 attempts: that is the repeat
        // the waste check must not risk.
        assert_eq!(
            sns(&base).config().retry_config().map(|r| r.max_attempts()),
            Some(3)
        );
        let client = sns_single_attempt(&base);
        assert_eq!(
            client.config().retry_config().map(|r| r.max_attempts()),
            Some(1)
        );
        assert_eq!(
            client.config().region().map(|r| r.as_ref()),
            Some("ap-south-1")
        );
    }

    #[test]
    fn test_s3_single_attempt_makes_exactly_one_attempt() {
        let base = retrying_base();
        assert_eq!(
            s3(&base).config().retry_config().map(|r| r.max_attempts()),
            Some(3)
        );
        let client = s3_single_attempt(&base);
        assert_eq!(
            client.config().retry_config().map(|r| r.max_attempts()),
            Some(1)
        );
        assert_eq!(
            client.config().region().map(|r| r.as_ref()),
            Some("ap-south-1")
        );
    }
}
