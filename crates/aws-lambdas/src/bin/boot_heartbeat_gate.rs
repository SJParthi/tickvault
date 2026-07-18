//! Thin Lambda bootstrap — ALL logic lives in
//! `tickvault_aws_lambdas::alarm_gate` (thin-bin coverage rule).

// House restriction-lint blanket (hostile-review r1 F5 — match the lib.rs /
// crates/app/src/main.rs binary-root shape): no unwrap/expect/print/dbg,
// no dropped Results, in production code.
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
    tickvault_aws_lambdas::alarm_gate::handle(event.payload).await
}
