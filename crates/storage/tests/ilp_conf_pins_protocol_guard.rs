//! Every ILP-over-HTTP sender configuration in `crates/storage/src` pins
//! `protocol_version=1`.
//!
//! Without the pin, `Sender::from_conf` reads QuestDB's `/settings` while the
//! writer is built. If QuestDB is slow at boot that read fails, the writer
//! falls back to "no sender" and, in the writers that never rebuild one, stays
//! that way for the whole process: every row it buffers is lost. Seen live on
//! 2026-11-03 for `ws_event_audit` and `feed_gap_audit`. With the pin the
//! sender is built without any network call, and a slow QuestDB surfaces as
//! a flush error that is retried.

use std::fs;
use std::path::Path;

#[test]
fn every_ilp_http_conf_in_storage_pins_the_protocol_version() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0usize;
    let mut offenders = Vec::new();
    for entry in fs::read_dir(&dir).expect("storage src dir is readable") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("source file is readable");
        for (n, line) in text.lines().enumerate() {
            if line.contains("\"http::addr={}:{};") {
                checked += 1;
                if !line.contains("protocol_version=") {
                    offenders.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
    }
    assert!(
        checked >= 7,
        "guard found only {checked} ILP confs; the scan pattern is stale"
    );
    assert!(
        offenders.is_empty(),
        "ILP confs without protocol_version (they probe QuestDB at build and can stay broken): {offenders:?}"
    );
}

#[test]
fn a_pinned_conf_builds_a_sender_without_reaching_questdb() {
    // Port 1 on loopback: nothing listens there. A pinned conf must still
    // build, because building makes no network call.
    let conf = "http::addr=127.0.0.1:1;protocol_version=1;";
    assert!(questdb::ingress::Sender::from_conf(conf).is_ok());
}
