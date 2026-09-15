# Rust language boundary and remaining migration

The intended target is Rust for all owned non-frontend executable logic.
The current implementation has Rust business logic plus operational shell,
declarative infrastructure, and third-party runtime dependencies. These existing
dependencies are disclosed here; this document does not authorize new exceptions
or declare the literal Rust-only target complete.

| Surface | Current implementation | Remaining work |
| --- | --- | --- |
| Market ingestion, storage clients, API, trading, Lambda handlers | Rust workspace crates | Maintain language guards and validate the deployed binary against the reviewed source revision. A Rust client does not change its database's implementation language. |
| Browser UI | HTML and embedded browser scripts, including strings served by Rust | Frontend exception; keep the browser-surface inventory accurate. A zero standalone script-file count does not mean zero browser scripting. |
| Production boot and database recovery | `deploy/systemd/tickvault.service` invokes `scripts/ensure-questdb.sh`; `crates/aws-lambdas/src/operator_control.rs` issues the same operational command | Port the owned recovery decisions and credential handling to a Rust tool with equivalent failure behavior before removing the shell path. |
| Operator health checks | `crates/tickvault-logs-mcp/src/tools.rs::tool_run_doctor` invokes `scripts/doctor.sh` | Inventory its checks, preserve the JSON/report contract, and test failure/timeout handling before replacing it with an existing or extended Rust doctor. |
| Other operations, developer hooks and CI | Shell scripts and third-party actions | Inventory by actual invocation site. Migrate owned imperative logic separately from declarative CI configuration and vendor action runtimes. |
| Database and optional logging services | QuestDB; optional Loki/Alloy in Docker Compose | Third-party services are outside a claim about owned Rust business logic. A dependency-inclusive Rust-only requirement would require an explicitly planned replacement, compatibility validation and data migration. |
| Infrastructure and schemas | Terraform, YAML, TOML, SQL and service-unit configuration | These are not Rust source and are not browser frontend. Keep them visible when interpreting an entire-workspace requirement; declarative configuration is distinct from an alternative application backend. |

The spawn allowlist deliberately still contains `bash` and `sh`. Its frozen-set
test prevents silent additions; it does not prove that shell is Rust or that every
external command is harmless. The source-extension inventory likewise cannot
certify embedded code, dynamically selected executables or dependencies.

Before claiming a migration complete, verify the actual production invocation,
not only deletion of a filename. Boot and recovery parity must cover missing
executables, absent versus stopped containers, restricted service-user PATH,
credential retrieval failures, command timeouts, retries and observable errors.
Inspect commands without printing secret values, and test destructive recovery
cases in an isolated environment rather than on the live database.

This language inventory does not certify live service health, performance or
algorithmic complexity. Rust code can have linear or sorting costs, and emitting
a variable number of result rows requires work proportional to that output.

## Reading the automated source report

`make guarantees` keeps its command name for compatibility, but its output is
a source evidence report, not a production certification. Its labels distinguish
what this invocation actually evaluates:

| Label | Meaning |
| --- | --- |
| STATIC CHECK | A source/configuration predicate evaluated successfully. The row's scope matters; inspecting a test's source does not run that test. |
| REFERENCE | An inventory or authored description without a pass/fail predicate in this command. Named guards and tests must be run separately. |
| BOUNDED | A stated constraint or design tradeoff; the command does not verify that production stays within it. |
| IMPOSSIBLE | A literal requirement conflicts with arithmetic. |
| BROKEN | An evaluated static predicate failed. |

The existing failure predicates remain gates: any BROKEN row exits with status 1.
An unreadable/empty git index also exits with status 1. Status 0 only means no
evaluated predicate failed. The command does not execute the cited unit tests,
property tests, fuzzers or benchmarks, or query AWS and QuestDB. Historical
benchmark figures and authored complexity explanations remain references until
independently rerun or reviewed. Text and HTML use the same row data and labels.
