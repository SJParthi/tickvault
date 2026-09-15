//! Offline operator entry point for the existing WAL sequence migration engine.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use tickvault_storage::ws_frame_spill::{
    SequenceMigrationEvidence, audit_retained_wal_namespace, inspect_sequence_namespace,
    lock_wal_dir, migrate_sequence_authority, verify_sequence_migration_receipt,
};

const MAX_EVIDENCE_BYTES: usize = 16 * 1024;
const MAX_PROVENANCE_BYTES: usize = 4096;
const HELP: &str = "\
Usage: tv-wal-sequence-migrate --wal-dir PATH --evidence FILE [--apply]
       tv-wal-sequence-migrate --inspect --wal-dir PATH
       tv-wal-sequence-migrate --audit-retained-wal --wal-dir PATH
       tv-wal-sequence-migrate --verify-receipt --wal-dir PATH --receipt-copy FILE
       tv-wal-sequence-migrate --help

The WAL directory must already exist. --inspect is strictly read-only, takes
no writer lock, and can run while the old writer is active. It emits JSON and
returns nonzero for migration-required, incomplete/corrupt/unsafe or unverified
observations. A locally empty directory or structurally valid authority passes
only this local preflight; neither authorizes deployment or proves the complete
database namespace. Detected concurrent changes are unverified. No WAL payloads
are read. Use an external timeout to bound blocked filesystem operations.

--audit-retained-wal reads every retained WAL record in root, replaying,
archive and quarantine using the production framing/CRC scanner. It returns
exact local sequence bounds and counts, never external namespace completeness.
Unknown scope, corrupt/torn records and legacy records without original IDs
refuse certification. Cost is O(bytes + entries log entries), with at most
4096 inventory entries; use an external timeout for the full offline scan.

--verify-receipt parses exact typed integers from a bounded private receipt
copy, byte-matches the actual namespace receipt, and validates the actual
directory-bound manifest/marker and sequence floor. It does not re-prove the
operator's external namespace attestation or scan WAL payloads.

Both offline read-only modes acquire the EXISTING owner lock exclusively;
an active writer or missing lock refuses them. They create no files, write no
authority/receipt, and allocate no sequence. Use an external timeout.
--inspect, --audit-retained-wal and --verify-receipt are mutually exclusive
and cannot use --evidence or --apply. --receipt-copy is only for --verify-receipt.
The evidence modes take the exclusive writer lock; an active writer refuses.

Without --apply: validate arguments, strict JSON evidence, and directory
ownership only. No retained-WAL scan or sequence migration is performed.
With --apply: the migration engine completely validates retained WAL, then
writes a synced migration receipt and sequence authority. It never deletes
or rewrites retained WAL/history and refuses an existing sequence authority.

FILE must be a regular JSON file, at most 16384 bytes, containing exactly:
  highest_capture_seq          unsigned integer within the signed i64 range
  complete_namespace_verified  true
  provenance                   nonempty string, at most 4096 UTF-8 bytes

Before setting the attestation true, verify a conservative capture bound
across the complete database/feed namespace: authoritative database history,
rescue/backup tiers, retained WAL, and any previous WAL roots. A MAX over only
current database rows is insufficient if older history has been pruned.
Provenance should identify the evidence scope and verifier; include no
credentials. This command does not query the database or infer a bound.

Example (validation only):
  tv-wal-sequence-migrate --wal-dir /srv/tickvault/ws-wal --evidence /secure/verified-bound.json
Add --apply only when that evidence is verified and the writer is stopped.
";

#[derive(Debug, PartialEq, Eq)]
struct Options {
    wal_dir: PathBuf,
    evidence_file: PathBuf,
    apply: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Inspect(PathBuf),
    AuditRetained(PathBuf),
    VerifyReceipt {
        wal_dir: PathBuf,
        receipt_copy: PathBuf,
    },
    Migrate(Options),
}

fn path_argument(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<PathBuf> {
    let value = args
        .next()
        .with_context(|| format!("{flag} requires a path"))?;
    ensure!(
        !value.is_empty() && value.as_encoded_bytes().first() != Some(&b'-'),
        "{flag} requires a path; prefix a filename beginning with '-' with './'"
    );
    Ok(PathBuf::from(value))
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let args: Vec<_> = args.into_iter().collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        return Ok(Command::Help);
    }
    let mut args = args.into_iter();
    let mut wal_dir = None;
    let mut evidence_file = None;
    let mut apply = false;
    let mut inspect = false;
    let mut audit = false;
    let mut verify = false;
    let mut receipt_copy = None;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--wal-dir") => {
                ensure!(wal_dir.is_none(), "--wal-dir must appear exactly once");
                wal_dir = Some(path_argument(&mut args, "--wal-dir")?);
            }
            Some("--evidence") => {
                ensure!(
                    evidence_file.is_none(),
                    "--evidence must appear exactly once"
                );
                evidence_file = Some(path_argument(&mut args, "--evidence")?);
            }
            Some("--apply") => {
                ensure!(!apply, "--apply must not be repeated");
                apply = true;
            }
            Some("--inspect") => {
                ensure!(!inspect, "--inspect must not be repeated");
                inspect = true;
            }
            Some("--audit-retained-wal") => {
                ensure!(!audit, "--audit-retained-wal must not be repeated");
                audit = true;
            }
            Some("--verify-receipt") => {
                ensure!(!verify, "--verify-receipt must not be repeated");
                verify = true;
            }
            Some("--receipt-copy") => {
                ensure!(
                    receipt_copy.is_none(),
                    "--receipt-copy must appear exactly once"
                );
                receipt_copy = Some(path_argument(&mut args, "--receipt-copy")?);
            }
            _ => bail!("unknown or extra argument; use --help for the exact syntax"),
        }
    }
    let wal_dir = wal_dir.context("missing required --wal-dir")?;
    ensure!(
        usize::from(inspect) + usize::from(audit) + usize::from(verify) <= 1,
        "read-only modes are mutually exclusive"
    );
    ensure!(
        verify || receipt_copy.is_none(),
        "--receipt-copy requires --verify-receipt"
    );
    if inspect || audit || verify {
        ensure!(
            !apply && evidence_file.is_none(),
            "read-only modes cannot be combined with --apply or --evidence"
        );
        return Ok(if inspect {
            Command::Inspect(wal_dir)
        } else if audit {
            Command::AuditRetained(wal_dir)
        } else {
            Command::VerifyReceipt {
                wal_dir,
                receipt_copy: receipt_copy.context("--verify-receipt requires --receipt-copy")?,
            }
        });
    }
    Ok(Command::Migrate(Options {
        wal_dir,
        evidence_file: evidence_file.context("missing required --evidence")?,
        apply,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceFile {
    highest_capture_seq: u64,
    complete_namespace_verified: bool,
    provenance: String,
}

fn parse_evidence(bytes: &[u8]) -> Result<SequenceMigrationEvidence> {
    ensure!(
        bytes.len() <= MAX_EVIDENCE_BYTES,
        "evidence file exceeds 16384 bytes"
    );
    let evidence: EvidenceFile =
        serde_json::from_slice(bytes).context("invalid migration evidence JSON")?;
    ensure!(
        evidence.complete_namespace_verified,
        "complete_namespace_verified must be explicitly true"
    );
    ensure!(
        evidence.highest_capture_seq <= i64::MAX as u64,
        "capture bound exceeds the signed database range"
    );
    ensure!(
        !evidence.provenance.trim().is_empty() && evidence.provenance.len() <= MAX_PROVENANCE_BYTES,
        "provenance must be nonempty and at most 4096 UTF-8 bytes"
    );
    Ok(SequenceMigrationEvidence {
        highest_capture_seq: evidence.highest_capture_seq,
        complete_namespace_verified: evidence.complete_namespace_verified,
        provenance: evidence.provenance,
    })
}

fn read_evidence(path: &Path) -> Result<SequenceMigrationEvidence> {
    let file = File::open(path).context("cannot open migration evidence file")?;
    let metadata = file
        .metadata()
        .context("cannot inspect migration evidence file")?;
    ensure!(metadata.is_file(), "evidence must be a regular JSON file");
    ensure!(
        metadata.len() <= MAX_EVIDENCE_BYTES as u64,
        "evidence file exceeds 16384 bytes"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_EVIDENCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    parse_evidence(&bytes)
}

fn run() -> Result<()> {
    let options = match parse_args(std::env::args_os().skip(1))? {
        Command::Help => {
            std::io::stdout().lock().write_all(HELP.as_bytes())?;
            return Ok(());
        }
        Command::Inspect(wal_dir) => {
            let report = inspect_sequence_namespace(&wal_dir);
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut stdout, &report)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
            ensure!(
                report.local_preflight_passed(),
                "local namespace preflight refused: {:?}; {}",
                report.namespace_state,
                report.reason
            );
            return Ok(());
        }
        Command::AuditRetained(wal_dir) => {
            let report = audit_retained_wal_namespace(&wal_dir);
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut stdout, &report)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
            ensure!(
                report.local_audit_passed(),
                "retained WAL audit refused: {}",
                report.reason
            );
            return Ok(());
        }
        Command::VerifyReceipt {
            wal_dir,
            receipt_copy,
        } => {
            let report = verify_sequence_migration_receipt(&wal_dir, &receipt_copy);
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut stdout, &report)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
            ensure!(
                report.receipt_verified,
                "receipt verification refused: {}",
                report.reason
            );
            return Ok(());
        }
        Command::Migrate(options) => options,
    };
    // Parse evidence before acquiring a lock or touching the WAL directory.
    let evidence = read_evidence(&options.evidence_file)?;
    let wal_dir = std::fs::canonicalize(&options.wal_dir)
        .context("WAL directory must already exist; refusing to create a fresh namespace")?;
    ensure!(
        wal_dir.is_dir(),
        "--wal-dir must name an existing directory"
    );
    let guard =
        lock_wal_dir(&wal_dir)?.context("migration requires the exclusive WAL directory guard")?;
    // Decimal strings preserve exact capture bounds in JSON clients whose
    // numeric representation cannot retain every integer beyond 2^53.
    let summary = if options.apply {
        let receipt = migrate_sequence_authority(&wal_dir, &guard, evidence)?;
        serde_json::json!({
            "mode": "apply",
            "migration_applied": true,
            "wal_directory": wal_dir.display().to_string(),
            "verified_capture_bound": receipt.verified_capture_bound.to_string(),
            "retained_wal_bound": receipt.retained_wal_bound.to_string(),
            "reserved_through": receipt.reserved_through.to_string(),
            "receipt_path": receipt.receipt_path.display().to_string(),
        })
    } else {
        serde_json::json!({
            "mode": "validate-inputs-only",
            "migration_applied": false,
            "wal_directory": wal_dir.display().to_string(),
            "caller_attested_capture_bound": evidence.highest_capture_seq.to_string(),
            "caller_attested_complete_namespace": evidence.complete_namespace_verified,
            "exclusive_directory_lock_acquired": true,
            "retained_wal_validation": "not_run",
            "sequence_authority_written": false,
        })
    };
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &summary)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("tv-wal-sequence-migrate: {error:#}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Result<Command> {
        parse_args(values.iter().map(|value| OsString::from(*value)))
    }

    #[test]
    fn migration_requires_an_explicit_apply_flag() {
        let base = [
            "--wal-dir",
            "/existing/wal",
            "--evidence",
            "/verified/evidence.json",
        ];
        assert!(matches!(
            args(&base).unwrap(),
            Command::Migrate(Options { apply: false, .. })
        ));
        let mut apply = base.to_vec();
        apply.push("--apply");
        assert!(matches!(
            args(&apply).unwrap(),
            Command::Migrate(Options { apply: true, .. })
        ));
        assert_eq!(args(&["--help"]).unwrap(), Command::Help);
    }

    #[test]
    fn read_only_inspection_needs_no_evidence_and_cannot_apply() {
        assert_eq!(
            args(&["--inspect", "--wal-dir", "/existing/wal"]).unwrap(),
            Command::Inspect(PathBuf::from("/existing/wal"))
        );
        for invalid in [
            vec!["--inspect"],
            vec!["--inspect", "--wal-dir", "/wal", "--apply"],
            vec!["--inspect", "--wal-dir", "/wal", "--evidence", "/proof"],
            vec!["--inspect", "--wal-dir", "/wal", "--inspect"],
        ] {
            assert!(args(&invalid).is_err());
        }
    }

    #[test]
    fn offline_read_only_modes_are_explicit_and_mutually_exclusive() {
        assert_eq!(
            args(&["--audit-retained-wal", "--wal-dir", "/wal"]).unwrap(),
            Command::AuditRetained(PathBuf::from("/wal"))
        );
        assert_eq!(
            args(&[
                "--verify-receipt",
                "--wal-dir",
                "/wal",
                "--receipt-copy",
                "/private/proof"
            ])
            .unwrap(),
            Command::VerifyReceipt {
                wal_dir: PathBuf::from("/wal"),
                receipt_copy: PathBuf::from("/private/proof")
            }
        );
        for invalid in [
            vec!["--audit-retained-wal", "--wal-dir", "/wal", "--apply"],
            vec![
                "--audit-retained-wal",
                "--wal-dir",
                "/wal",
                "--evidence",
                "/proof",
            ],
            vec!["--audit-retained-wal", "--wal-dir", "/wal", "--inspect"],
            vec![
                "--audit-retained-wal",
                "--wal-dir",
                "/wal",
                "--audit-retained-wal",
            ],
            vec![
                "--audit-retained-wal",
                "--wal-dir",
                "/wal",
                "--verify-receipt",
            ],
            vec![
                "--audit-retained-wal",
                "--wal-dir",
                "/wal",
                "--receipt-copy",
                "/proof",
            ],
            vec!["--verify-receipt", "--wal-dir", "/wal"],
            vec!["--verify-receipt", "--wal-dir", "/wal", "--receipt-copy"],
            vec![
                "--verify-receipt",
                "--wal-dir",
                "/wal",
                "--receipt-copy",
                "--apply",
            ],
            vec![
                "--verify-receipt",
                "--wal-dir",
                "/wal",
                "--receipt-copy",
                "/proof",
                "--apply",
            ],
            vec![
                "--verify-receipt",
                "--wal-dir",
                "/wal",
                "--receipt-copy",
                "/proof",
                "--evidence",
                "/evidence",
            ],
            vec![
                "--verify-receipt",
                "--wal-dir",
                "/wal",
                "--receipt-copy",
                "/proof",
                "--inspect",
            ],
            vec![
                "--verify-receipt",
                "--wal-dir",
                "/wal",
                "--receipt-copy",
                "/proof",
                "--verify-receipt",
            ],
            vec![
                "--verify-receipt",
                "--wal-dir",
                "/wal",
                "--receipt-copy",
                "/proof",
                "--receipt-copy",
                "/other",
            ],
            vec![
                "--wal-dir",
                "/wal",
                "--evidence",
                "/proof",
                "--receipt-copy",
                "/copy",
            ],
        ] {
            assert!(args(&invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn missing_duplicate_unknown_and_extra_arguments_are_refused() {
        for invalid in [
            vec![],
            vec!["--wal-dir"],
            vec!["--wal-dir", "--apply"],
            vec!["--wal-dir", "/wal", "--evidence", "/proof", "--unknown"],
            vec!["--wal-dir", "/wal", "--evidence", "/proof", "unexpected"],
            vec![
                "--wal-dir",
                "/wal",
                "--evidence",
                "/proof",
                "--apply",
                "--apply",
            ],
            vec![
                "--wal-dir",
                "/wal",
                "--evidence",
                "/proof",
                "--wal-dir",
                "/other",
            ],
            vec![
                "--wal-dir",
                "/wal",
                "--evidence",
                "/proof",
                "--evidence",
                "/other",
            ],
            vec!["--help", "--apply"],
        ] {
            assert!(
                args(&invalid).is_err(),
                "accepted invalid arguments: {invalid:?}"
            );
        }
    }

    #[test]
    fn evidence_requires_exact_fields_and_an_explicit_complete_attestation() {
        let valid = br#"{
            "highest_capture_seq": 123,
            "complete_namespace_verified": true,
            "provenance": "verified fixture scope"
        }"#;
        let evidence = parse_evidence(valid).unwrap();
        assert_eq!(evidence.highest_capture_seq, 123);
        for invalid in [
            r#"{"highest_capture_seq":123,"provenance":"scope"}"#,
            r#"{
                "highest_capture_seq": 123,
                "complete_namespace_verified": false,
                "provenance": "scope"
            }"#,
            r#"{
                "highest_capture_seq": 123,
                "complete_namespace_verified": true,
                "provenance": "scope",
                "extra": 1
            }"#,
            r#"{
                "highest_capture_seq": 123,
                "highest_capture_seq": 456,
                "complete_namespace_verified": true,
                "provenance": "scope"
            }"#,
            r#"{"highest_capture_seq":-1,"complete_namespace_verified":true,"provenance":"scope"}"#,
            r#"{
                "highest_capture_seq": 9223372036854775808,
                "complete_namespace_verified": true,
                "provenance": "scope"
            }"#,
            r#"{"highest_capture_seq":123,"complete_namespace_verified":true,"provenance":"  "}"#,
        ] {
            assert!(parse_evidence(invalid.as_bytes()).is_err());
        }
        let oversized_provenance = serde_json::json!({
            "highest_capture_seq": 123, "complete_namespace_verified": true,
            "provenance": "x".repeat(MAX_PROVENANCE_BYTES + 1),
        });
        assert!(parse_evidence(&serde_json::to_vec(&oversized_provenance).unwrap()).is_err());
        assert!(parse_evidence(&vec![b' '; MAX_EVIDENCE_BYTES + 1]).is_err());
    }
}
