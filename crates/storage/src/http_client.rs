//! HTTP-CLIENT-01 — panic-free reqwest client construction (C2 fix, 2026-07-03).
//!
//! Root cause this module removes: 8 sites in `crates/storage/src/` used the
//! fallback `Client::builder().build().unwrap_or_else(|_| Client::new())`.
//! `reqwest::Client::new()` PANICS when the builder fails (TLS backend init,
//! DNS resolver init, fd exhaustion) — the exact conditions under which the
//! builder's `Err` arm is reachable in the first place. A panic inside a
//! tokio task is a SILENT task death (no `error!`, no respawn signal) — the
//! suspected mechanism behind the 2026-07-03 10:35 IST SLO-publisher death
//! during a 1.13M-frame storm (`SLO-03` incident): `wait_for_questdb_ready`
//! builds a client per invocation on the every-10s SLO tick and the every-5s
//! pool-watchdog tick.
//!
//! This module provides:
//! - [`client_from_build_result`] — the pure, testable core mapping a builder
//!   `Err` to a typed [`HttpClientBuildError`] instead of a panic.
//! - [`build_probe_client`] — build a timeout-bounded client, no panic path.
//! - [`shared_probe_client`] — a process-wide `OnceLock` client for the
//!   repeating QuestDB readiness probes: built ONCE, reused forever
//!   (O(1) after first call — a single atomic load).
//!
//! Failure emit sites log `error!` with
//! `code = ErrorCode::HttpClient01BuildFailed.code_str()` and increment
//! `tv_http_client_build_failed_total{site}` (STATIC labels only). Runbook:
//! `.claude/rules/project/http-client-error-codes.md`.

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client;

/// Request timeout for the shared QuestDB readiness-probe client.
///
/// Mirrors `boot_probe::PROBE_REQUEST_TIMEOUT_SECS` (2s) — a single probe
/// must never stretch the 5s pool-watchdog / 10s SLO scheduler ticks.
pub const PROBE_REQUEST_TIMEOUT_SECS: u64 = 2;

/// Typed, panic-free error for a failed `reqwest::ClientBuilder::build()`.
///
/// The message is the builder error's `Display` string with URL userinfo
/// redacted ([`redact_userinfo`] strips `scheme://user:pass@` credentials —
/// reqwest builder errors can embed proxy URLs from `HTTPS_PROXY` that carry
/// Basic-Auth userinfo); it never carries request bodies or our own tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpClientBuildError {
    message: String,
}

impl HttpClientBuildError {
    /// The captured builder-failure description.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for HttpClientBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HTTP-CLIENT-01 reqwest client build failed: {}",
            self.message
        )
    }
}

impl std::error::Error for HttpClientBuildError {}

/// Redact URL userinfo (`scheme://user:pass@host` → `scheme://***@host`)
/// from an error message before it is logged / rendered.
///
/// reqwest builder errors can embed proxy URLs sourced from the
/// `HTTPS_PROXY` env var, which may carry Basic-Auth credentials in the
/// userinfo portion. Regex-free scan: for each `://` occurrence, if an `@`
/// appears before the next `/`, whitespace, or end-of-string, the
/// characters between `://` and the final `@` of that authority are
/// replaced with `***`.
fn redact_userinfo(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(idx) = rest.find("://") {
        let (head, tail) = rest.split_at(idx + "://".len());
        out.push_str(head);
        // Authority ends at the next '/', whitespace, or end of string.
        let boundary = tail
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..boundary];
        // rfind: a raw password may itself contain '@' — redact up to the
        // LAST '@' of the authority so no credential fragment survives.
        if let Some(at) = authority.rfind('@') {
            out.push_str("***");
            rest = &tail[at..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Pure core: map a `ClientBuilder::build()` result into a typed error.
///
/// NEVER panics — the `Err` arm captures the error's `Display` string
/// (userinfo-redacted via [`redact_userinfo`]) into
/// [`HttpClientBuildError`]. Extracted as a pure function so the
/// builder-failure path is deterministically unit-testable (a real
/// `reqwest` builder failure needs fd/TLS exhaustion, which tests cannot
/// safely induce).
///
/// Promoted `pub(crate)` → `pub` 2026-07-17 (post-#1562 audit): the app
/// crate's `oms_wiring::build_oms_http_client` reuses this SAME redaction +
/// typed-error core instead of duplicating it (one HTTP-CLIENT-01
/// implementation, one redaction path).
///
/// # Errors
///
/// Returns [`HttpClientBuildError`] carrying the redacted `Display` string
/// whenever `res` is `Err` — never panics, never falls back to the
/// panic-class `Client::new()`.
pub fn client_from_build_result<E: fmt::Display>(
    res: Result<Client, E>,
) -> Result<Client, HttpClientBuildError> {
    res.map_err(|e| HttpClientBuildError {
        message: redact_userinfo(&e.to_string()),
    })
}

/// Build a timeout-bounded reqwest client with NO panic path.
///
/// # Errors
///
/// Returns [`HttpClientBuildError`] when the underlying
/// `ClientBuilder::build()` fails (TLS backend init, resolver init, fd
/// exhaustion). Callers degrade gracefully — they never panic.
pub fn build_probe_client(timeout_secs: u64) -> Result<Client, HttpClientBuildError> {
    client_from_build_result(
        Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build(),
    )
}

/// Process-wide shared QuestDB readiness-probe client.
static PROBE_CLIENT: OnceLock<Client> = OnceLock::new();

/// Returns the process-wide shared probe client, building it on first call.
///
/// Built ONCE per process and reused across every probe invocation — the
/// boot probe, the every-5s pool-watchdog QuestDB probe, and the every-10s
/// SLO scheduler probe all share this instance, so TLS/resolver init happens
/// exactly once instead of ~8,640 times per trading session. O(1) after the
/// first call (a single atomic load).
///
/// The client is built BEFORE `get_or_init` so a build failure propagates as
/// a typed error instead of panicking inside the init closure. A racing
/// double-build is harmless — `OnceLock` keeps exactly one instance and the
/// loser's client is dropped.
///
/// # Errors
///
/// Returns [`HttpClientBuildError`] when the client cannot be built (and no
/// prior call succeeded). The NEXT call retries the build, so a transient
/// fd-exhaustion window does not poison the process forever.
pub fn shared_probe_client() -> Result<&'static Client, HttpClientBuildError> {
    if let Some(client) = PROBE_CLIENT.get() {
        return Ok(client);
    }
    let client = build_probe_client(PROBE_REQUEST_TIMEOUT_SECS)?;
    Ok(PROBE_CLIENT.get_or_init(move || client))
}

/// Verdict of a best-effort legacy-table rename.
///
/// Two of the four arms are NORMAL operation, which is most of the reason this
/// type exists — see [`try_rename_legacy_table`]. The third,
/// [`LegacyRenameOutcome::Split`], is the one that must never be silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyRenameOutcome {
    /// The rename applied: the legacy table's rows carried forward into the
    /// current name. Happens at most ONCE per table, on the first boot of the
    /// build that introduced the new name.
    Migrated,
    /// QuestDB refused the rename AND the legacy table is gone. This is the
    /// STEADY STATE, not a fault — the migration already ran on an earlier
    /// boot, or the source never existed on a fresh install.
    NotNeeded,
    /// QuestDB refused the rename and the legacy table IS STILL THERE.
    ///
    /// Both names now exist, so the history is SPLIT ACROSS TWO TABLES: new
    /// rows land under the current name while the legacy table holds
    /// everything written before. Reachable when a CREATE minted the new name
    /// before any rename ran — a rollback to a pre-rename build (whose ILP
    /// writer re-creates the legacy name) and then forward again, or a build
    /// that shipped the CREATE without its rename.
    ///
    /// This is the ONE arm callers must escalate. Before this variant existed
    /// the whole non-2xx case collapsed into `NotNeeded` at `debug!`, which
    /// meant moving the rename out of the error-logging statement loop
    /// destroyed the only signal that a split had happened. Loud here, not
    /// because a rename failed, but because two SEBI-retained tables now hold
    /// one table's history and only an operator can decide how to merge them.
    Split,
    /// The request never reached QuestDB, or the follow-up existence probe did
    /// not. Deliberately not escalated here: the CREATE/ALTER statements that
    /// follow hit the same dead server and own the loud, coded report, so
    /// raising it twice would double-count one outage across two codes.
    Unreachable,
}

/// Does this table exist? `None` means QuestDB could not be reached, which is
/// deliberately distinct from `Some(false)` — "absent" and "unknown" lead to
/// different verdicts in [`try_rename_legacy_table`], and conflating them is
/// how a split history would go unreported during an outage.
///
/// `count()` rather than a row read: QuestDB answers it from metadata, so the
/// cost does not scale with a table that may hold months of SEBI-retained rows.
async fn table_exists(client: &Client, exec_url: &str, table: &str) -> Option<bool> {
    let query = format!("SELECT count() FROM '{table}';");
    match client
        .get(exec_url)
        .query(&[("query", query.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => Some(true),
        // A non-2xx here is QuestDB saying the table is not there. Any other
        // SQL fault on a statement this simple would also mean we cannot read
        // it, and treating that as "absent" only ever suppresses a Split — the
        // conservative direction is the opposite, so this stays `false` and the
        // caller's CREATE reports anything genuinely broken under its own code.
        Ok(_) => Some(false),
        Err(_) => None,
    }
}

/// Run a one-shot `RENAME TABLE '<legacy>' TO '<current>'` whose failure is
/// EXPECTED and therefore must not be reported as an error.
///
/// ## The defect this exists to prevent (2026-08-14)
///
/// The REST tables gained a `rest_` name prefix, and each `ensure_*_table`
/// pushed the RENAME into the same statement vector as the CREATE and the
/// ALTERs, then ran the whole vector through one loop whose non-2xx arm fires
/// a coded `error!` and increments the table's `*_persist_errors_total`
/// counter.
///
/// But a RENAME that fails is the NORMAL case. It succeeds exactly once, ever.
/// From the second boot onward the target exists and QuestDB refuses it; on a
/// fresh install the source is absent and QuestDB refuses it. So every boot
/// but one logged a coded error and incremented a persist-error counter that
/// is EMF-published to CloudWatch and charted on the operator dashboard — an
/// error signal whose steady-state value is "one per boot, forever". That
/// trains the operator to discount the very counter that is supposed to mean
/// the REST tables are failing to persist.
///
/// The migration comment in those files already said the failure "is not an
/// error"; the code said it was. This function is the code agreeing with the
/// comment.
///
/// ## Why a refusal is not simply "fine" (the correction, same day)
///
/// Moving the RENAME out of the error-logging loop silenced a signal along
/// with the noise. QuestDB answers non-2xx for BOTH "target already exists"
/// and "source does not exist", so the first draft collapsed them into one
/// `NotNeeded` arm at `debug!`. Those two causes are not equivalent: if the
/// target exists AND the legacy table is still there, the history is split
/// across two SEBI-retained tables and nothing would ever have said so.
///
/// So a refusal now asks one more question — is the legacy table still
/// present? — and answers [`LegacyRenameOutcome::Split`] when it is. The
/// probe is a bounded `SELECT count() FROM '<legacy>'`, run ONLY on the
/// refusal path, so the steady state costs exactly one extra round trip per
/// table per boot and the happy path costs nothing.
///
/// Never a DROP, never a DELETE: these tables are SEBI-retained. Resolving a
/// split is an operator decision, and this function's only job is to make sure
/// they are told.
pub async fn try_rename_legacy_table(
    client: &Client,
    exec_url: &str,
    legacy: &str,
    current: &str,
) -> LegacyRenameOutcome {
    let ddl = format!("RENAME TABLE '{legacy}' TO '{current}';");
    match client
        .get(exec_url)
        .query(&[("query", ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            // Worth exactly one INFO line in the process's life: it is the
            // moment a populated table carried its history across a rename.
            tracing::info!(
                legacy_table = legacy,
                current_table = current,
                "legacy table renamed forward — existing rows carried into the new name"
            );
            LegacyRenameOutcome::Migrated
        }
        Ok(_) => match table_exists(client, exec_url, legacy).await {
            Some(true) => {
                // Deliberately NOT logged here. The caller owns the coded
                // `error!` because the error code is per-table (SPOT1M-02,
                // CHAIN-02, …) and a second log line from inside this helper
                // would double-report one condition.
                LegacyRenameOutcome::Split
            }
            Some(false) => {
                tracing::debug!(
                    legacy_table = legacy,
                    current_table = current,
                    "legacy-table rename not applicable — the legacy table does \
                     not exist (migration already ran, or fresh install). \
                     Expected, not a fault"
                );
                LegacyRenameOutcome::NotNeeded
            }
            None => {
                // The rename got an HTTP answer but the probe did not, so we
                // cannot tell NotNeeded from Split. Claiming either would be a
                // guess; `Unreachable` is the honest verdict and the CREATE
                // that follows reports the outage under its own code.
                tracing::debug!(
                    legacy_table = legacy,
                    current_table = current,
                    "legacy-table rename refused and the existence probe could \
                     not reach QuestDB — split state undetermined this boot"
                );
                LegacyRenameOutcome::Unreachable
            }
        },
        Err(err) => {
            tracing::debug!(
                legacy_table = legacy,
                current_table = current,
                ?err,
                "legacy-table rename could not reach QuestDB — the CREATE that \
                 follows reports the outage under its own code"
            );
            LegacyRenameOutcome::Unreachable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Byte counts are exact on purpose. An overstated Content-Length makes a
    // client that READS the body block until its timeout instead of returning
    // — and the split-detection probe added on 2026-08-14 does exactly that on
    // the refusal path, so a wrong length here would surface as a mystery
    // flake rather than a failed assertion. `{"error":"..."}` bodies below are
    // 24 and 27 bytes.
    const OK_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const REFUSED_400: &str =
        "HTTP/1.1 400 Bad Request\r\nContent-Length: 24\r\n\r\n{\"error\":\"table exists\"}";
    const ABSENT_400: &str =
        "HTTP/1.1 400 Bad Request\r\nContent-Length: 27\r\n\r\n{\"error\":\"table does not\"}";

    /// A QuestDB stand-in that answers the RENAME and the existence probe
    /// DIFFERENTLY, because the whole point of the split detection is that
    /// those two answers disagree.
    ///
    /// Routing is on the literal `RENAME` in the percent-encoded query string
    /// (`serde_urlencoded` leaves ASCII letters alone), so the mock cannot
    /// accidentally answer the probe with the rename's response.
    ///
    /// `probe_response: None` hangs up without replying — the transport
    /// failure that must classify `Unreachable`, never a guessed verdict.
    async fn spawn_questdb_mock(
        rename_response: &'static str,
        probe_response: Option<&'static str>,
    ) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                        let reply = if req.contains("RENAME") {
                            Some(rename_response)
                        } else {
                            probe_response
                        };
                        if let Some(body) = reply {
                            let _ = stream.write_all(body.as_bytes()).await;
                        }
                        // `None` => drop the stream unanswered.
                    });
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn test_try_rename_legacy_table_2xx_is_migrated() {
        let port = spawn_questdb_mock(OK_200, Some(ABSENT_400)).await;
        let client = build_probe_client(2).expect("probe client");
        let url = format!("http://127.0.0.1:{port}/exec");
        assert_eq!(
            try_rename_legacy_table(&client, &url, "old_t", "rest_old_t").await,
            LegacyRenameOutcome::Migrated
        );
    }

    #[tokio::test]
    async fn test_refused_rename_with_legacy_table_gone_is_not_needed_not_an_error() {
        // The steady state: QuestDB refuses because the migration already ran
        // (or never applied), and the legacy table is not there. Must NOT be
        // error-shaped, or the boot-time coded error and the false
        // persist-error increment come straight back every boot.
        let port = spawn_questdb_mock(REFUSED_400, Some(ABSENT_400)).await;
        let client = build_probe_client(2).expect("probe client");
        let url = format!("http://127.0.0.1:{port}/exec");
        assert_eq!(
            try_rename_legacy_table(&client, &url, "old_t", "rest_old_t").await,
            LegacyRenameOutcome::NotNeeded
        );
    }

    #[tokio::test]
    async fn test_refused_rename_with_legacy_table_still_present_is_split() {
        // THE regression this variant exists for. Same refusal as the test
        // above — the ONLY difference is that the legacy table still answers,
        // which means both names hold rows and the history is split.
        //
        // Collapsing this into NotNeeded is what the first draft did, and it
        // silently removed the only signal that a SEBI-retained table had been
        // orphaned. If this test ever asserts NotNeeded again, that regression
        // is back.
        let port = spawn_questdb_mock(REFUSED_400, Some(OK_200)).await;
        let client = build_probe_client(2).expect("probe client");
        let url = format!("http://127.0.0.1:{port}/exec");
        assert_eq!(
            try_rename_legacy_table(&client, &url, "old_t", "rest_old_t").await,
            LegacyRenameOutcome::Split
        );
    }

    #[tokio::test]
    async fn test_refused_rename_with_unanswerable_probe_is_unreachable_not_a_guess() {
        // The rename got an HTTP answer, the probe did not. We cannot tell
        // "absent" from "split", so the verdict must be Unreachable. Returning
        // NotNeeded here would quietly assert the safe case during an outage —
        // exactly the false-OK this whole change is about.
        let port = spawn_questdb_mock(REFUSED_400, None).await;
        let client = build_probe_client(2).expect("probe client");
        let url = format!("http://127.0.0.1:{port}/exec");
        assert_eq!(
            try_rename_legacy_table(&client, &url, "old_t", "rest_old_t").await,
            LegacyRenameOutcome::Unreachable
        );
    }

    #[tokio::test]
    async fn test_try_rename_legacy_table_unreachable_questdb_is_typed_not_panic() {
        // Port 1 is reserved and never listening — a real transport failure
        // without touching any live service.
        let client = build_probe_client(2).expect("probe client");
        assert_eq!(
            try_rename_legacy_table(&client, "http://127.0.0.1:1/exec", "old_t", "rest_old_t")
                .await,
            LegacyRenameOutcome::Unreachable
        );
    }

    #[test]
    fn test_legacy_rename_outcome_arms_are_distinct_and_only_split_escalates() {
        use LegacyRenameOutcome::{Migrated, NotNeeded, Split, Unreachable};
        let all = [Migrated, NotNeeded, Split, Unreachable];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "outcome arms must stay distinguishable");
            }
        }
        // Exhaustive match: adding a fifth arm stops this compiling until the
        // author decides whether the new state is one a caller must escalate.
        // Escalation policy lives here, next to the type, rather than being
        // re-derived at each of the three call sites.
        for outcome in all {
            let escalates = match outcome {
                Split => true,
                Migrated | NotNeeded | Unreachable => false,
            };
            assert_eq!(
                escalates,
                outcome == Split,
                "only Split is the operator-actionable arm"
            );
        }
    }

    #[test]
    fn test_client_from_build_result_maps_error_without_panic() {
        let res: Result<Client, &str> = Err("boom");
        let err = client_from_build_result(res).expect_err("Err must map to typed error");
        assert!(
            err.message().contains("boom"),
            "captured message must carry the source Display: {}",
            err.message()
        );
    }

    #[test]
    fn test_client_from_build_result_ok_passthrough() {
        let client = build_probe_client(1).expect("test client builds");
        let res: Result<Client, &str> = Ok(client);
        assert!(client_from_build_result(res).is_ok());
    }

    #[test]
    fn test_shared_probe_client_reuses_same_instance() {
        let a = shared_probe_client().expect("first call builds");
        let b = shared_probe_client().expect("second call reuses");
        assert!(
            std::ptr::eq(a, b),
            "shared_probe_client must return the SAME static instance"
        );
    }

    #[test]
    fn test_build_probe_client_succeeds_with_timeout() {
        let client = build_probe_client(PROBE_REQUEST_TIMEOUT_SECS);
        assert!(client.is_ok(), "normal-env build must succeed: {client:?}");
    }

    #[test]
    fn test_error_display_carries_code_and_message_no_secret_echo() {
        let res: Result<Client, &str> = Err("resolver init failed");
        let err = client_from_build_result(res).expect_err("typed error");
        let rendered = format!("{err}");
        assert!(rendered.contains("HTTP-CLIENT-01"), "{rendered}");
        assert!(rendered.contains("resolver init failed"), "{rendered}");
        // The error type has no fields beyond the builder message — assert
        // the rendered form is exactly the code + captured message (no URL,
        // no token, nothing else to leak).
        assert_eq!(
            rendered,
            "HTTP-CLIENT-01 reqwest client build failed: resolver init failed"
        );
        // std::error::Error trait object path.
        let as_err: &dyn std::error::Error = &err;
        assert_eq!(as_err.to_string(), rendered);
    }

    #[test]
    fn test_redact_userinfo_strips_basic_auth() {
        let input = "error trying to connect via proxy \
                     https://user:s3cret@proxy.example:8080 (tunnel failed)";
        let out = redact_userinfo(input);
        assert!(out.contains("***@proxy.example"), "{out}");
        assert!(!out.contains("s3cret"), "credential leaked: {out}");
        assert!(!out.contains("user:"), "username leaked: {out}");
        // Non-authority content is preserved verbatim.
        assert!(out.contains("(tunnel failed)"), "{out}");
        assert!(
            out.starts_with("error trying to connect via proxy "),
            "{out}"
        );
    }

    #[test]
    fn test_redact_userinfo_passthrough_no_userinfo() {
        for input in [
            "resolver init failed",
            "connect error for http://questdb:9000/exec timed out",
            "plain text with no url at all",
            "",
        ] {
            assert_eq!(
                redact_userinfo(input),
                input,
                "no-userinfo input must pass through"
            );
        }
    }

    #[test]
    fn test_redact_userinfo_password_containing_at_sign() {
        // A raw password containing '@' — rfind redacts up to the LAST '@'.
        let input = "proxy http://user:p@ss@proxy.host:3128/ refused";
        let out = redact_userinfo(input);
        assert!(out.contains("***@proxy.host"), "{out}");
        assert!(!out.contains("p@ss"), "credential leaked: {out}");
    }

    #[test]
    fn test_error_message_is_redacted() {
        let res: Result<Client, String> = Err(
            "builder error: proxy scheme https://admin:hunter2@corp-proxy.internal:8080 rejected"
                .to_string(),
        );
        let err = client_from_build_result(res).expect_err("typed error");
        let rendered = format!("{err}");
        assert!(
            !rendered.contains("hunter2"),
            "secret leaked in Display: {rendered}"
        );
        assert!(
            !rendered.contains("admin:"),
            "username leaked in Display: {rendered}"
        );
        assert!(rendered.contains("***@corp-proxy.internal"), "{rendered}");
        assert!(rendered.contains("HTTP-CLIENT-01"), "{rendered}");
        // message() accessor is redacted too (same underlying field).
        assert!(!err.message().contains("hunter2"), "{}", err.message());
    }
}
