//! Thin Lambda bootstrap — ALL logic lives in
//! `tickvault_aws_lambdas::budget_killswitch` (thin-bin coverage rule).

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tickvault_aws_lambdas::logging::init_lambda_tracing();
    lambda_runtime::run(service_fn(handler)).await
}

async fn handler(event: LambdaEvent<Value>) -> Result<Value, Error> {
    tickvault_aws_lambdas::budget_killswitch::handle(event.payload).await
}
