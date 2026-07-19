//! Thin Lambda bootstrap — ALL logic lives in
//! `tickvault_aws_lambdas::operator_control` (thin-bin coverage rule).
//!
//! Serves BOTH the Lambda Function URL and the API-GW v2 (payload 2.0)
//! routes — the raw `serde_json::Value` event covers both shapes exactly
//! like the Python oracle's dict handling did.

// House restriction-lint blanket (match the lib.rs / binary-root shape):
// no unwrap/expect/print/dbg, no dropped Results, in production code.
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tickvault_aws_lambdas::logging::init_lambda_tracing();
    lambda_runtime::run(service_fn(handler)).await
}

async fn handler(event: LambdaEvent<Value>) -> Result<Value, Error> {
    tickvault_aws_lambdas::operator_control::handle(event.payload).await
}
