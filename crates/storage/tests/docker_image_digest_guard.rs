//! Ratchet: every container image in docker-compose.yml is SHA256-digest-pinned.
//!
//! Why this guard exists (2026-08-08). CLAUDE.md's DOCKER SERVICES section has
//! asserted *"All images pinned with SHA256 digest"* since 2026-04-25. On
//! 2026-08-08 that assertion was FALSE: `questdb/questdb:9.3.5` carried a bare
//! tag while loki and alloy carried `@sha256:` digests. A document asserting a
//! supply-chain control that is not actually in place is worse than an openly
//! unpinned tag — a reader (human or agent) stops checking.
//!
//! The stale comment beside the QuestDB image justified the omission by
//! claiming a digest pin would need "both arm64 + amd64 SHAs and conditional
//! logic, which is fragile". That premise was wrong. Docker Hub publishes a
//! MULTI-ARCH OCI IMAGE INDEX whose single digest resolves per-platform with no
//! conditional logic, verified live against registry-1.docker.io on 2026-08-08
//! (mediaType `application/vnd.oci.image.index.v1+json`, carrying
//! `linux/amd64` AND `linux/arm64`, round-tripping by digest with HTTP 200).
//! One digest therefore serves both Mac dev and the ARM Graviton `t4g` prod box.
//!
//! This guard makes the CLAUDE.md sentence mechanically TRUE from now on, and
//! keeps the doc coupled to the mechanism so neither can drift alone.
//!
//! HONEST SCOPE — this guard checks that a digest is PRESENT, not that it is
//! multi-arch. A future single-arch pin would still pass here. That residual is
//! recorded rather than hidden: `tv-loki` and `tv-alloy` are currently pinned to
//! single-arch manifests (their own comments say "Linux/amd64 digest"), which is
//! latent-but-harmless today because both sit behind `profiles: [logs]` and are
//! NOT started by a default `docker compose up`. Only `tv-questdb` starts by
//! default. Tightening this to assert multi-arch indexes would require network
//! access from the test, which CI test jobs do not have.

#![cfg(test)]

use std::path::PathBuf;

fn compose_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/docker/docker-compose.yml")
}

fn claude_md_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../CLAUDE.md")
}

/// Extract every effective `image:` value from the compose file, skipping
/// comment lines (a `#`-prefixed line is documentation, not a directive).
fn declared_images(compose: &str) -> Vec<String> {
    let mut images = Vec::new();
    for line in compose.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("image:") else {
            continue;
        };
        // Strip any trailing inline comment, then whitespace.
        let value = rest.split('#').next().unwrap_or(rest).trim();
        if !value.is_empty() {
            images.push(value.to_string());
        }
    }
    images
}

#[test]
fn every_compose_image_is_digest_pinned() {
    let compose = std::fs::read_to_string(compose_path())
        .unwrap_or_else(|e| panic!("cannot read docker-compose.yml: {e}"));

    let images = declared_images(&compose);

    // Non-vacuity: the compose file declares questdb + loki + alloy. If this
    // drops, the parser broke and the guard would pass while checking nothing.
    assert!(
        images.len() >= 3,
        "found only {} image declarations in docker-compose.yml — expected >= 3 \
         (questdb, loki, alloy). The parser or the compose file changed shape; \
         a guard that checks nothing is a false-OK (audit-findings Rule 11).\n\
         Parsed: {:?}",
        images.len(),
        images
    );

    let unpinned: Vec<&String> = images.iter().filter(|i| !i.contains("@sha256:")).collect();

    assert!(
        unpinned.is_empty(),
        "{} container image(s) in docker-compose.yml are NOT SHA256-digest-pinned: {:?}\n\n\
         CLAUDE.md asserts \"All images pinned with SHA256 digest\". Either pin the \
         image or change that sentence — the doc and the file must agree.\n\n\
         To resolve a multi-arch index digest (works for BOTH amd64 dev and the \
         ARM Graviton prod box, no conditional logic needed):\n  \
         TOKEN=$(curl -s \"https://auth.docker.io/token?service=registry.docker.io&scope=repository:<repo>:pull\" | jq -r .token)\n  \
         curl -sI -H \"Authorization: Bearer $TOKEN\" \\\n    \
           -H 'Accept: application/vnd.oci.image.index.v1+json' \\\n    \
           \"https://registry-1.docker.io/v2/<repo>/manifests/<tag>\" | grep -i docker-content-digest",
        unpinned.len(),
        unpinned
    );
}

#[test]
fn claude_md_still_carries_the_pinning_claim() {
    // Couples the doc to the mechanism in BOTH directions: if someone deletes
    // the CLAUDE.md sentence, this test tells them the guard above is what now
    // backs it, so the claim should be restored rather than quietly dropped.
    let claude_md = std::fs::read_to_string(claude_md_path())
        .unwrap_or_else(|e| panic!("cannot read CLAUDE.md: {e}"));
    assert!(
        claude_md.contains("All images pinned with SHA256 digest"),
        "CLAUDE.md no longer carries the \"All images pinned with SHA256 digest\" \
         claim. That claim is now mechanically enforced by \
         every_compose_image_is_digest_pinned in this file — restore the sentence, \
         or delete this test in the same PR with a dated note explaining why the \
         guarantee was dropped."
    );
}

#[test]
fn parser_ignores_commented_image_lines() {
    // Bite-proof: the compose file carries a comment line reading
    // "# Bible #16: questdb/questdb:9.3.5" directly above the real directive.
    // If the parser counted that, an unpinned image would hide inside a comment.
    let sample = "\
  # Bible #16: questdb/questdb:9.3.5\n\
    image: questdb/questdb:9.3.5@sha256:deadbeef\n\
  # image: some/unpinned:latest\n";
    let images = declared_images(sample);
    assert_eq!(
        images,
        vec!["questdb/questdb:9.3.5@sha256:deadbeef".to_string()],
        "parser must read only real `image:` directives, never commented ones"
    );
}

#[test]
fn parser_flags_a_planted_unpinned_image() {
    let sample = "    image: questdb/questdb:9.3.5\n";
    let images = declared_images(sample);
    assert_eq!(images.len(), 1);
    assert!(
        !images[0].contains("@sha256:"),
        "planted bare-tag image should be detected as unpinned"
    );
}
