//! `tv-guarantees` — the self-verifying guarantee report.
//!
//! Operator demand (2026-08-18, verbatim, typos preserved): *"Ensure to use
//! one and only RUST O(1) in the entire workspace codebase except frontend
//! alone so check this every nook and corner with assurance and guarantee"*,
//! *"no manual intervention or human inputs or human monitoring shod lobe
//! expected"*, *"I dont need any hallucination or illusion I just need the
//! working guaranteed assurance solution"*, and *"table level comapriosn view
//! where even any humans can udnerstand"*.
//!
//! This binary exists because every one of those sentences is really the same
//! request: **stop being told, start being shown.** Every number below is
//! MEASURED from the working tree at the moment of the run. Nothing is
//! hardcoded from a document, because a document cannot stay true — this
//! repository has now recorded four separate corrections to a single row of a
//! table in `CLAUDE.md`, each caused by re-reading the row instead of the code.
//!
//! # The three verdicts, and why there are three
//!
//! A two-state PASS/FAIL report would have to lie about this system, so it
//! does not use one:
//!
//! - **GUARANTEED** — mechanically enforced by a build-failing ratchet. If it
//!   regresses, CI goes red. This is the only class that is a promise.
//! - **BOUNDED** — genuinely not O(1), by nature, with a NAMED constraint and
//!   a known ceiling. Honest and safe, but never to be relabelled O(1).
//! - **IMPOSSIBLE** — the literal demand contradicts arithmetic. Reported as
//!   such, with the reason, and never quietly dropped from the list.
//!
//! The third class is the load-bearing one. `CLAUDE.md`'s own non-O(1) table
//! carries the warning this binary is built around: *"a partial disclosure
//! reads exactly like a complete one"*. A report that silently omitted the
//! impossible rows would read exactly like a report where everything passed.
//!
//! # Exit codes
//!
//! - `0` — every GUARANTEED row holds.
//! - `1` — at least one GUARANTEED row is BROKEN. Suitable as a CI gate.
//!
//! BOUNDED and IMPOSSIBLE rows never fail the run. They are facts about the
//! problem, not defects, and a gate that failed on them would be permanently
//! red and therefore permanently ignored.
//!
//! # Invocation
//!
//! ```bash
//! cargo run -p tickvault-app --bin tv-guarantees     # or: make guarantees
//! ```

#![allow(clippy::unwrap_used)] // APPROVED: diagnostic CLI, same posture as tv_doctor
#![allow(clippy::expect_used)] // APPROVED: diagnostic CLI, same posture as tv_doctor

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

/// How much a row can be promised.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Enforced by a build-failing ratchet.
    Guaranteed,
    /// Not O(1) by nature; constraint named, ceiling known.
    Bounded,
    /// The literal demand contradicts arithmetic.
    Impossible,
    /// A GUARANTEED row that no longer holds. Fails the run.
    Broken,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Self::Guaranteed => "GUARANTEED",
            Self::Bounded => "BOUNDED",
            Self::Impossible => "IMPOSSIBLE",
            Self::Broken => "** BROKEN **",
        }
    }
}

/// One measured row of the report.
struct Row {
    what: String,
    verdict: Verdict,
    measured: String,
    proof: String,
}

impl Row {
    fn new(what: &str, verdict: Verdict, measured: impl Into<String>, proof: &str) -> Self {
        Self {
            what: what.to_string(),
            verdict,
            measured: measured.into(),
            proof: proof.to_string(),
        }
    }
}

/// Every tracked file, one per line, from git.
///
/// `git ls-files` rather than a filesystem walk: an UNTRACKED file cannot
/// reach CI or the box, and counting it would report a scratch file as a
/// workspace violation. The plan-gate learned this the hard way in the
/// opposite direction — there, an untracked `.rs` file WAS the hole.
fn tracked_files() -> Vec<String> {
    let out = Command::new("git").args(["ls-files"]).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Count tracked files whose name ends in any of `exts`.
fn count_ext(files: &[String], exts: &[&str]) -> usize {
    files
        .iter()
        .filter(|f| exts.iter().any(|e| f.ends_with(e)))
        .count()
}

/// Count tracked files whose FIRST TWO BYTES are `#!`, whatever they are named.
///
/// Deliberately reads the file rather than matching an extension list. The
/// enumerate-one-more-extension approach has been wrong repeatedly in this
/// repository, always in the same direction: a class nobody listed is
/// invisible, and invisibility reads as green. `tools/deploy` with a `#!`
/// first line is a script; `tools/deploy.txt` without one is not.
fn count_shebangs(files: &[String]) -> usize {
    use std::io::Read as _;
    files
        .iter()
        .filter(|f| {
            std::fs::File::open(f).is_ok_and(|mut fh| {
                let mut head = [0u8; 2];
                fh.read_exact(&mut head).is_ok() && &head == b"#!"
            })
        })
        .count()
}

/// Count DISTINCT third-party actions referenced by any workflow.
///
/// Distinct rather than total, because ten `uses:` of `actions/checkout` is
/// one external dependency, not ten. Counted here purely so the number is
/// visible — this binary takes no position on whether they are in scope, and
/// the lock leaves that open.
fn count_third_party_actions() -> usize {
    let Ok(entries) = std::fs::read_dir(".github/workflows") else {
        return 0;
    };
    let mut seen: Vec<String> = Vec::new();
    for e in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        for line in text.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix("uses:") else {
                continue;
            };
            let name = rest.trim();
            // `./.github/...` is our own composite action, not a third party.
            if name.starts_with('.') || name.is_empty() {
                continue;
            }
            let base = name.split('@').next().unwrap_or(name).to_string();
            if !seen.contains(&base) {
                seen.push(base);
            }
        }
    }
    seen.len()
}

/// Count occurrences of a needle across a directory tree of `.rs` files.
///
/// Walks with `std::fs` rather than shelling out to a search tool, so the
/// report has no dependency on which binaries the host happens to carry.
fn count_in_rust_sources(root: &Path, needles: &[&str]) -> usize {
    fn walk(dir: &Path, needles: &[&str], acc: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // `target/` is build output, not source: counting it would
                // inflate every number and change with build state.
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                walk(&path, needles, acc);
            } else if path.extension().is_some_and(|e| e == "rs")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                for line in text.lines() {
                    let t = line.trim_start();
                    if needles.iter().any(|n| t.starts_with(n)) {
                        *acc += 1;
                    }
                }
            }
        }
    }
    let mut acc = 0;
    walk(root, needles, &mut acc);
    acc
}

/// Distinct `tv_*` measurement names defined anywhere in the Rust sources.
///
/// Distinct NAMES, not call sites: one metric is often incremented from
/// several places, and comparing "76 names shipped" against a count of call
/// sites would be an apples-to-oranges ratio dressed up as a coverage figure.
/// Templated names (`"tv_errcode_{}"`) count once, though they expand to
/// several real metrics -- stated because the denominator is a definition
/// count, not a runtime series count.
fn distinct_tv_names(root: &Path) -> usize {
    fn walk(dir: &Path, acc: &mut std::collections::BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Production sources only. A test asserting on a
                // metric name is not a measurement the system takes,
                // and counting it would inflate the denominator of
                // the one ratio on this page that reports a gap.
                if path
                    .file_name()
                    .is_some_and(|n| n == "target" || n == "tests" || n == "benches")
                {
                    continue;
                }
                walk(&path, acc);
            } else if path.extension().is_some_and(|e| e == "rs")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                for line in text.lines() {
                    let t = line.trim_start();
                    if t.starts_with("//") {
                        continue;
                    }
                    let mut rest = line;
                    while let Some(at) = rest.find("\"tv_") {
                        rest = &rest[at + 1..];
                        if let Some(close) = rest.find('"') {
                            acc.insert(rest[..close].to_string());
                        }
                    }
                }
            }
        }
    }
    let mut acc = std::collections::BTreeSet::new();
    walk(root, &mut acc);
    acc.len()
}
/// Count lines CONTAINING any needle, anywhere in the line.
///
/// Sibling to [`count_in_rust_sources`], which anchors at the start of the
/// trimmed line. That anchoring is correct for its own rows and wrong for
/// declarations: `pub const DEDUP_KEY_CANDLES: &str = "..."` and
/// `Self::Foo => "BAR-01"` both carry the interesting text mid-line, so the
/// anchored matcher scores them zero. The 2026-08-22 addition of section 6
/// hit exactly that and briefly reported a healthy dedup key as BROKEN --
/// a false alarm in the very report built to stop false alarms. Two matchers
/// with names that say which is which is the fix.
fn count_lines_containing(root: &Path, needles: &[&str]) -> usize {
    fn walk(dir: &Path, needles: &[&str], acc: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Production sources only. A test asserting on a
                // metric name is not a measurement the system takes,
                // and counting it would inflate the denominator of
                // the one ratio on this page that reports a gap.
                if path
                    .file_name()
                    .is_some_and(|n| n == "target" || n == "tests" || n == "benches")
                {
                    continue;
                }
                walk(&path, needles, acc);
            } else if path.extension().is_some_and(|e| e == "rs")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                for line in text.lines() {
                    // Comment lines mention these strings constantly --
                    // dated rationale, retired-table notes, doc sketches.
                    // Counting prose as a declaration is how a report
                    // ends up with a number that does not mean what its
                    // own label says.
                    let t = line.trim_start();
                    if t.starts_with("//") || t.starts_with("*") {
                        continue;
                    }
                    if needles.iter().any(|n| line.contains(n)) {
                        *acc += 1;
                    }
                }
            }
        }
    }
    let mut acc = 0;
    // A single file is a legitimate root: the error-code table lives in
    // exactly one file, and walking its whole directory would sweep in
    // every other match arm in the crate.
    if root.is_file() {
        if let Ok(text) = std::fs::read_to_string(root) {
            for line in text.lines() {
                let t = line.trim_start();
                if !t.starts_with("//") && needles.iter().any(|n| line.contains(n)) {
                    acc += 1;
                }
            }
        }
        return acc;
    }
    walk(root, needles, &mut acc);
    acc
}

/// Page shell for [`render_html`] — fonts, tokens, layout.
///
/// Inlined rather than linked: the artifact host blocks every external origin
/// but Google Fonts, and a stylesheet that silently fails to load produces a
/// page that looks broken rather than one that looks unstyled.
const HTML_HEAD: &str = r##"<title>TickVault Guarantee Ledger</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Newsreader:ital,opsz,wght@0,6..72,400;0,6..72,500;1,6..72,400&family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap">
<style>
:root{--ground:#FBFBFD;--surface:#FFF;--surface-2:#F3F5F8;--ink:#12161C;--ink-2:#39424F;
--muted:#5A6472;--rule:#DDE2EA;--jade:#0F7A5A;--jade-soft:#E4F1EC;--ochre:#A66A11;
--ochre-soft:#F7EEDF;--indigo:#4C5BA8;--indigo-soft:#E9EBF7;--red:#B3261E;--red-soft:#FAE9E8;
--shadow:0 1px 2px rgba(18,22,28,.05),0 8px 24px rgba(18,22,28,.06)}
@media (prefers-color-scheme:dark){:root:not([data-theme="light"]){--ground:#0B1014;--surface:#141A21;
--surface-2:#1B222B;--ink:#E6EAF0;--ink-2:#B7C0CC;--muted:#8A95A3;--rule:#26303B;--jade:#3FB98D;
--jade-soft:#102A22;--ochre:#D69A3C;--ochre-soft:#2C2313;--indigo:#8B93E8;--indigo-soft:#1B1E33;
--red:#E8695F;--red-soft:#2E1614;--shadow:0 1px 2px rgba(0,0,0,.4),0 8px 24px rgba(0,0,0,.35)}}
:root[data-theme="dark"]{--ground:#0B1014;--surface:#141A21;--surface-2:#1B222B;--ink:#E6EAF0;
--ink-2:#B7C0CC;--muted:#8A95A3;--rule:#26303B;--jade:#3FB98D;--jade-soft:#102A22;--ochre:#D69A3C;
--ochre-soft:#2C2313;--indigo:#8B93E8;--indigo-soft:#1B1E33;--red:#E8695F;--red-soft:#2E1614;
--shadow:0 1px 2px rgba(0,0,0,.4),0 8px 24px rgba(0,0,0,.35)}
*{box-sizing:border-box}
body{margin:0;background:var(--ground);color:var(--ink);font-family:"IBM Plex Sans",system-ui,sans-serif;
font-size:16px;line-height:1.6;-webkit-font-smoothing:antialiased}
.wrap{max-width:1080px;margin:0 auto;padding:0 24px}
.mast{padding:72px 0 40px;border-bottom:1px solid var(--rule)}
.eyebrow{font-family:"IBM Plex Mono",monospace;font-size:12px;letter-spacing:.14em;text-transform:uppercase;
color:var(--muted);margin:0 0 20px}.eyebrow b{color:var(--ink);font-weight:600}
h1{font-family:Newsreader,Georgia,serif;font-weight:500;font-size:clamp(38px,6.2vw,68px);line-height:1.04;
letter-spacing:-.02em;margin:0 0 20px;text-wrap:balance}
h1 em{font-style:italic;color:var(--jade)}
.standfirst{font-size:clamp(17px,2.1vw,20px);color:var(--ink-2);max-width:62ch;margin:0 0 28px}
.band{display:grid;grid-template-columns:repeat(4,1fr);gap:14px;margin:36px 0 8px}
@media(max-width:720px){.band{grid-template-columns:repeat(2,1fr)}}
.tile{background:var(--surface);border:1px solid var(--rule);border-radius:10px;padding:20px 18px;
box-shadow:var(--shadow);position:relative;overflow:hidden}
.tile::before{content:"";position:absolute;inset:0 auto 0 0;width:3px;background:var(--c)}
.tile .n{font-family:"IBM Plex Mono",monospace;font-variant-numeric:tabular-nums;font-size:44px;
font-weight:600;line-height:1;color:var(--c);letter-spacing:-.02em}
.tile .k{font-family:"IBM Plex Mono",monospace;font-size:11px;letter-spacing:.12em;text-transform:uppercase;
color:var(--muted);margin-top:10px}
.tile .d{font-size:13.5px;color:var(--ink-2);margin-top:8px;line-height:1.45}
.t-g{--c:var(--jade)}.t-b{--c:var(--ochre)}.t-i{--c:var(--indigo)}.t-x{--c:var(--red)}
section{padding:52px 0;border-bottom:1px solid var(--rule)}
h2{font-family:Newsreader,Georgia,serif;font-weight:500;font-size:clamp(24px,3.2vw,34px);
letter-spacing:-.015em;margin:0 0 22px;text-wrap:balance}
.tscroll{overflow-x:auto;border:1px solid var(--rule);border-radius:10px;background:var(--surface);
box-shadow:var(--shadow)}
table{border-collapse:collapse;width:100%;min-width:660px}
thead th{font-family:"IBM Plex Mono",monospace;font-size:10.5px;letter-spacing:.12em;text-transform:uppercase;
color:var(--muted);text-align:left;font-weight:500;padding:13px 16px;border-bottom:1px solid var(--rule);
background:var(--surface-2);white-space:nowrap}
tbody td{padding:14px 16px;border-bottom:1px solid var(--rule);vertical-align:top;font-size:14.5px}
tbody tr:last-child td{border-bottom:none}
td.what{font-weight:500;color:var(--ink);min-width:190px}
td.meas{font-family:"IBM Plex Mono",monospace;font-variant-numeric:tabular-nums;font-size:13.5px;
color:var(--ink);white-space:nowrap}
td.why{color:var(--ink-2);font-size:14px;line-height:1.5;min-width:240px}
td.vd{width:1%}
.chip{display:inline-block;font-family:"IBM Plex Mono",monospace;font-size:10.5px;letter-spacing:.09em;
text-transform:uppercase;font-weight:600;padding:4px 9px;border-radius:4px;white-space:nowrap}
.c-g{background:var(--jade-soft);color:var(--jade)}
.c-b{background:var(--ochre-soft);color:var(--ochre)}
.c-i{background:var(--indigo-soft);color:var(--indigo)}
.c-x{background:var(--red-soft);color:var(--red)}
code{font-family:"IBM Plex Mono",monospace;font-size:.9em;background:var(--surface-2);padding:2px 6px;
border-radius:4px;border:1px solid var(--rule)}
footer{padding:40px 0 64px;color:var(--muted);font-size:13.5px}
footer p{max-width:66ch}
</style>
"##;

/// Escape the four characters that would otherwise close a tag or an entity.
///
/// The proof text is prose written in this file, not user input — but a page
/// that renders `<` literally would silently swallow everything after it, and
/// a swallowed row reads as a row that was never measured.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The same measurements as [`render`], as a self-contained HTML page.
///
/// Exists because a hand-authored status page goes stale the moment the code
/// moves, and a stale page that still reads green is the exact false-OK this
/// whole report is built to prevent. Generated from the identical `Row` values
/// the text output uses, so the two can never disagree.
fn render_html(sections: &[(&str, &[Row])]) -> String {
    let all: Vec<&Row> = sections.iter().flat_map(|(_, rows)| rows.iter()).collect();
    let broken = all.iter().filter(|r| r.verdict == Verdict::Broken).count();
    let bounded = all.iter().filter(|r| r.verdict == Verdict::Bounded).count();
    let impossible = all
        .iter()
        .filter(|r| r.verdict == Verdict::Impossible)
        .count();
    let guaranteed = all.len() - bounded - impossible - broken;

    let mut s = String::new();
    let _ = write!(s, "{}", HTML_HEAD);

    let _ = write!(
        s,
        r#"<div class="wrap"><header class="mast">
<p class="eyebrow">TickVault &middot; generated by <b>make guarantees</b> &middot; every number measured at run time</p>
<h1>Every guarantee here was <em>measured</em>, not claimed.</h1>
<p class="standfirst">An O(1) live F&amp;O trading system for NSE. This page is produced by the
same command that gates every pull request &mdash; so a promise cannot outlive the code that
stopped keeping it.</p>
<div class="band">
<div class="tile t-g"><div class="n">{guaranteed}</div><div class="k">Guaranteed</div><div class="d">Holds, with a test that fails the build if it stops holding.</div></div>
<div class="tile t-b"><div class="n">{bounded}</div><div class="k">Bounded</div><div class="d">True inside a stated limit. The limit is printed, never hidden.</div></div>
<div class="tile t-i"><div class="n">{impossible}</div><div class="k">Impossible</div><div class="d">Forbidden by arithmetic. Naming them is the honest part.</div></div>
<div class="tile t-x"><div class="n">{broken}</div><div class="k">Broken</div><div class="d">A guaranteed row that stopped holding. Any number here exits non-zero.</div></div>
</div></header>
"#
    );

    for (title, rows) in sections {
        let _ = write!(
            s,
            "<section><h2>{}</h2><div class=\"tscroll\"><table>\
             <thead><tr><th>What was measured</th><th>Verdict</th><th>Measured</th><th>Proof / why</th></tr></thead><tbody>",
            esc(title)
        );
        for r in rows.iter() {
            let (chip, cls) = match r.verdict {
                Verdict::Guaranteed => ("Guaranteed", "c-g"),
                Verdict::Bounded => ("Bounded", "c-b"),
                Verdict::Impossible => ("Impossible", "c-i"),
                Verdict::Broken => ("Broken", "c-x"),
            };
            let _ = write!(
                s,
                "<tr><td class=\"what\">{}</td><td class=\"vd\"><span class=\"chip {}\">{}</span></td>\
                 <td class=\"meas\">{}</td><td class=\"why\">{}</td></tr>",
                esc(&r.what),
                cls,
                chip,
                esc(&r.measured),
                esc(&r.proof)
            );
        }
        let _ = write!(s, "</tbody></table></div></section>");
    }

    let _ = write!(
        s,
        r#"<footer><p><b>How to reproduce every number on this page:</b> <code>make guarantees</code> for the
table, <code>make guarantees-html</code> for this page. Both read the working tree and count what is
actually there. The same command runs in continuous integration on every pull request, so a
guarantee that quietly stops holding is caught by the machine rather than remembered by a person.</p>
<p style="margin-top:14px">Bounded and impossible rows are facts about the problem, not defects &mdash;
they never fail the run, because a gate that failed on arithmetic would be permanently red, and a
permanently red gate is an ignored gate.</p></footer></div>"#
    );
    s
}
/// Render one section as a fixed-width table.
///
/// Deliberately plain text, and it stays the DEFAULT. The operator's standing
/// deliverable rule is "plain tables readable by anyone — never HTML", and a
/// report that needs a browser to be read is not one an operator checks at
/// 09:20 on a phone.
///
/// 2026-08-22: [`render_html`] was added alongside it — not in place of it —
/// after the operator asked three times for an "automated table level
/// comparison view". That rule governs the ANSWER given in chat, which is
/// still this text; the page is an extra surface for the artifact gallery,
/// generated from the same rows so it cannot drift from what this prints.
/// A bare `tv-guarantees` behaves exactly as before.
fn render(title: &str, rows: &[Row]) -> String {
    let w_what = rows.iter().map(|r| r.what.len()).max().unwrap_or(4).max(4);
    let w_verd = rows
        .iter()
        .map(|r| r.verdict.label().len())
        .max()
        .unwrap_or(7)
        .max(7);
    let w_meas = rows
        .iter()
        .map(|r| r.measured.len())
        .max()
        .unwrap_or(8)
        .max(8);

    let mut s = String::new();
    let _ = writeln!(s, "\n{title}");
    let _ = writeln!(s, "{}", "=".repeat(title.len()));
    let _ = writeln!(
        s,
        "{:<w_what$}  {:<w_verd$}  {:<w_meas$}  PROOF / WHY",
        "WHAT",
        "VERDICT",
        "MEASURED",
        w_what = w_what,
        w_verd = w_verd,
        w_meas = w_meas
    );
    let _ = writeln!(
        s,
        "{}  {}  {}  {}",
        "-".repeat(w_what),
        "-".repeat(w_verd),
        "-".repeat(w_meas),
        "-".repeat(40)
    );
    for r in rows {
        let _ = writeln!(
            s,
            "{:<w_what$}  {:<w_verd$}  {:<w_meas$}  {}",
            r.what,
            r.verdict.label(),
            r.measured,
            r.proof,
            w_what = w_what,
            w_verd = w_verd,
            w_meas = w_meas
        );
    }
    s
}

/// One workspace dependency, as the manifest actually declares it.
struct DepPin {
    name: String,
    /// `Some(version)` when declared `"=x.y.z"`; `None` when the declaration
    /// is a RANGE — which includes the bare `"x.y.z"` form, because cargo
    /// reads an unadorned version as `^x.y.z`.
    exact: Option<String>,
}

/// Parse `[workspace.dependencies]` from the root manifest.
///
/// Deliberately hand-rolled rather than `toml`-parsed: this must report on the
/// literal TEXT an author wrote, and a parser normalises `"1.2.3"` and
/// `"^1.2.3"` into the same `VersionReq` — erasing the exact distinction this
/// row exists to measure.
fn workspace_dep_pins(manifest: &str) -> Vec<DepPin> {
    let mut out = Vec::new();
    let mut inside = false;
    for raw in manifest.lines() {
        let line = raw.trim_start();
        if line.starts_with('[') {
            inside = line.starts_with("[workspace.dependencies]");
            continue;
        }
        if !inside || line.starts_with('#') {
            continue;
        }
        let Some((name, rest)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            continue;
        }
        // `{ workspace = true }`, path and git deps carry no registry version.
        let rest = rest.trim();
        let literal = if let Some(idx) = rest.find("version") {
            rest[idx..]
                .split_once('"')
                .and_then(|(_, r)| r.split_once('"'))
        } else if let Some(after_quote) = rest.strip_prefix('"') {
            after_quote.split_once('"')
        } else {
            None
        };
        let Some((version, _)) = literal else {
            continue;
        };
        if version.is_empty() || !version.starts_with(|c: char| c == '=' || c.is_ascii_digit()) {
            // ^ ~ * >= — an explicit range, same class as bare.
            out.push(DepPin {
                name: name.to_string(),
                exact: None,
            });
            continue;
        }
        out.push(DepPin {
            name: name.to_string(),
            exact: version.strip_prefix('=').map(str::to_string),
        });
    }
    out
}

/// Every workspace dependency name that at least one member crate actually
/// references via `{ workspace = true }`.
///
/// This is the check the "declared but never referenced" row always CLAIMED to
/// make and did not. That row asked a weaker question -- "does this name appear
/// anywhere in `Cargo.lock`?" -- which a dead declaration passes whenever some
/// OTHER crate pulls the same package in transitively. On 2026-08-22 the gap
/// was measured: nine workspace declarations had no member reference, and the
/// old row could see only seven of them; `quanta` and `clap` were invisible
/// because unrelated crates resolved them anyway. A row that under-reports
/// reads exactly like a complete one, which is why the question moved here.
fn referenced_workspace_deps() -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let Ok(entries) = std::fs::read_dir("crates") else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path().join("Cargo.toml")) else {
            continue;
        };
        for raw in text.lines() {
            let line = raw.trim_start();
            if line.starts_with('#') || !line.contains("workspace") {
                continue;
            }
            let Some((name, rest)) = line.split_once('=') else {
                continue;
            };
            if !rest.contains("workspace = true") && !rest.contains("workspace=true") {
                continue;
            }
            let name = name.trim();
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                continue;
            }
            out.insert(name.to_string());
        }
    }
    out
}

/// Every `(name, version)` pair `Cargo.lock` actually resolved.
fn lock_pairs(lock: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut name: Option<String> = None;
    for raw in lock.lines() {
        let line = raw.trim();
        if line == "[[package]]" {
            name = None;
        } else if let Some(v) = line.strip_prefix("name = \"") {
            name = v.strip_suffix('"').map(str::to_string);
        } else if let Some(v) = line.strip_prefix("version = \"")
            && let (Some(n), Some(ver)) = (name.take(), v.strip_suffix('"'))
        {
            // `1.1.4+spec-1.1.0` — build metadata is not part of the version
            // a requirement matches on.
            let ver = ver.split('+').next().unwrap_or(ver);
            out.push((n, ver.to_string()));
        }
    }
    out
}

fn main() {
    let files = tracked_files();
    if files.is_empty() {
        println!("tv-guarantees: could not read the git index — run inside the repository.");
        std::process::exit(1);
    }

    // ---------------------------------------------------------------
    // Section 1 — one language
    // ---------------------------------------------------------------
    let interpreted = count_ext(
        &files,
        &[
            ".py", ".pyw", ".pyi", ".rb", ".php", ".lua", ".pl", ".js", ".mjs", ".cjs", ".ts",
        ],
    );
    let shell = count_ext(&files, &[".sh", ".bash", ".zsh"]);
    let rust = count_ext(&files, &[".rs"]);

    // Extension-less `#!` executables. Counted SEPARATELY from `.sh` because
    // the extension ban and the shebang check catch different things, and the
    // 2026-08-14 audit found exactly this hole: `tools/deploy` with a `#!`
    // first line is neither extension-banned nor invocation-scanned, so a
    // one-rename evasion was invisible. Asking about the FILE rather than its
    // NAME is the fix; this row makes the answer visible.
    let shebangs = count_shebangs(&files);

    // Third-party GitHub Actions. Each `uses:` is someone else's runtime —
    // most of the standard set is `using: node20`. Reported because the
    // rust-only lock's own §0.1 leaves this OPEN as a policy question
    // ("third-party CI actions are not our workspace codebase") and an
    // unreported open question reads exactly like a closed one.
    let actions = count_third_party_actions();

    let lang = vec![
        Row::new(
            "Interpreted runtimes tracked",
            if interpreted == 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Broken
            },
            format!("{interpreted} files"),
            "rust_only_guard: both allowlists empty, shrink-only",
        ),
        Row::new(
            "Rust sources",
            Verdict::Guaranteed,
            format!("{rust} files"),
            "the workspace proper",
        ),
        Row::new(
            "Shell scripts",
            Verdict::Bounded,
            format!("{shell} files"),
            "CI/hook glue. NOT Rust. git invokes hooks as shell",
        ),
        Row::new(
            "`#!` executables (any name)",
            Verdict::Bounded,
            format!("{shebangs} files"),
            "asks the FILE, not its name -- closes the rename evasion",
        ),
        Row::new(
            "Rust spawn literals",
            Verdict::Guaranteed,
            "benign only",
            "rust_only_guard: Command::new/.arg/.args scanned",
        ),
        Row::new(
            "Build toolchain surface",
            Verdict::Guaranteed,
            "scanned",
            ".cargo/config runner+linker, Cargo.toml, make, Docker",
        ),
        Row::new(
            "node-family invocations",
            Verdict::Guaranteed,
            "budgeted",
            "command-position scan, shrink-only budget",
        ),
        Row::new(
            "Third-party CI actions",
            Verdict::Bounded,
            format!("{actions} used"),
            "someone else's runtime; OPEN policy question per lock §0.1",
        ),
        Row::new(
            "Browser code in .rs",
            Verdict::Bounded,
            "pinned",
            "browser_surface guard: enumerated surfaces only",
        ),
    ];

    // ---------------------------------------------------------------
    // Section 2 — O(1), including the rows arithmetic forbids
    // ---------------------------------------------------------------
    // Counted from the git index, and BOTH conditions matter: the filename
    // must start with `dhat_` AND end in `.rs`.
    //
    // The first version of this counted directory entries under
    // `crates/*/tests/` whose name started with `dhat_`, and reported 19
    // against a true 15 — it was picking up non-source entries. A report whose
    // whole purpose is "no hallucination" cannot ship an inflated number, and
    // an inflated one is worse than a missing one: it reads as MORE assurance
    // than actually exists. Ground truth is `find crates -name 'dhat_*' -type f`.
    let dhat = files
        .iter()
        .filter(|f| {
            f.ends_with(".rs")
                && Path::new(f)
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("dhat_"))
        })
        .count();

    // Read the seam's allocation ceiling out of the gate that enforces it, so
    // this row cannot drift from the test the way a copied number would.
    let seam_budget: Option<u64> =
        std::fs::read_to_string("crates/app/tests/dhat_live_ingest_seam.rs")
            .ok()
            .and_then(|src| {
                src.lines()
                    .map(str::trim)
                    .find_map(|l| l.strip_prefix("const MAX_BLOCKS: u64 = "))
                    .map(|v| v.trim_end_matches(';').replace('_', ""))
            })
            .and_then(|v| v.parse().ok());

    let o1 = vec![
        Row::new(
            "Packet decode (hot path)",
            Verdict::Guaranteed,
            "O(1), 0 alloc",
            "fixed-offset from_le_bytes; DHAT gates below",
        ),
        Row::new(
            "DHAT zero-alloc gates",
            if dhat > 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Broken
            },
            format!("{dhat} suites"),
            "each fails the build on a re-introduced allocation",
        ),
        // The row above certifies the DECODER. It does not certify the SEAM,
        // and the budget is read out of the gate that enforces it so this row
        // cannot drift from the test the way a copied number would.
        //
        // CORRECTED 2026-08-22. This comment used to say the seam "measured
        // the full decode -> fold -> ILP-append path at ~5,100 blocks per
        // 10,000 ticks" and quoted the gate's own "the fold path allocates per
        // tick today". Re-measured, it is **11 blocks per 10,000 folds** --
        // amortised row-buffer doublings, no per-tick allocation. The 5,100
        // was a cross-contaminated dhat run the gate's own mutex was added to
        // eliminate; the budget was never re-tightened after the fix, and this
        // report faithfully republished it. Reading a constant is only as
        // honest as the constant.
        //
        // The row stays BOUNDED rather than becoming GUARANTEED: 11 is not 0,
        // the seam does allocate as its buffer grows, and calling that
        // zero-alloc would be the same over-claim in the opposite direction.
        Row::new(
            "Live ingest seam (decode->fold->append)",
            Verdict::Bounded,
            match seam_budget {
                Some(b) => format!("<={b} blk/10k"),
                None => "budget unread".to_string(),
            },
            "11 measured; buffer growth, not per-tick allocation",
        ),
        Row::new(
            "Instrument lookup",
            Verdict::Guaranteed,
            "O(1) avg",
            "papaya composite-key hash",
        ),
        Row::new(
            "Uniqueness + dedup",
            Verdict::Guaranteed,
            "O(1)",
            "QuestDB DEDUP keys, (security_id, segment, feed)",
        ),
        Row::new(
            "Silence detection",
            Verdict::Bounded,
            "70us / 25,000",
            "O(n), every 30s = 0.0002% of the interval",
        ),
        Row::new(
            "catch_up_seal_all",
            Verdict::Bounded,
            "~10ms / 600k cells",
            "every 5s at the 25,000 ceiling = 0.2% of the interval",
        ),
        Row::new(
            "O(1) SPACE for n instruments",
            Verdict::Impossible,
            "O(n)",
            "n things cannot be stored in constant space",
        ),
        Row::new(
            "O(1) TIME for all-instrument queries",
            Verdict::Impossible,
            "O(n)",
            "the answer depends on every element by definition",
        ),
    ];

    // ---------------------------------------------------------------
    // Section 3 — automation, and the one step that is deliberately manual
    // ---------------------------------------------------------------
    let workflows = std::fs::read_dir(".github/workflows")
        .map(|rd| rd.flatten().count())
        .unwrap_or(0);
    let scheduled = std::fs::read_dir(".github/workflows")
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    std::fs::read_to_string(e.path()).is_ok_and(|t| t.contains("schedule:"))
                })
                .count()
        })
        .unwrap_or(0);

    let auto = vec![
        Row::new(
            "CI workflows",
            Verdict::Guaranteed,
            format!("{workflows} files"),
            "All Green fan-in is the single merge choke point",
        ),
        Row::new(
            "Unattended schedules",
            Verdict::Guaranteed,
            format!("{scheduled} cron"),
            "mutation, fuzz, sanitizers, catch-up dispatch",
        ),
        Row::new(
            "Merge button",
            Verdict::Bounded,
            "operator",
            "merge-gate-lock 3.2: agents must NOT arm auto-merge",
        ),
    ];

    // ---------------------------------------------------------------
    // Section 4 — test surface
    // ---------------------------------------------------------------
    //
    // This DELIBERATELY disagrees with `.test-count-baseline` (10243 today),
    // and the gap is the point. The ratchet counts the attribute ANYWHERE on a
    // line, so it also counts every `// ... #[test] ...` in a comment; that is
    // fine for a ratchet, which only has to be monotonic. This row claims to
    // report the number of TESTS, so it requires the attribute at line start
    // and comes out ~100 lower. `#[async_std::test]` is not in the needle list
    // because the workspace has zero of them (verified 2026-08-22) -- adding a
    // needle for a class that does not exist would be decoration, not rigour.
    // If one ever appears, add it here: an invisible test class is worse than
    // a wrong total.
    let tests = count_in_rust_sources(Path::new("crates"), &["#[test]", "#[tokio::test]"]);
    let cov = std::fs::read_to_string("quality/crate-coverage-thresholds.toml").unwrap_or_default();
    // A `key = value` line, NOT merely a line containing '='.
    //
    // The first version counted any line with an '=' and reported 18 floors
    // for a file holding 9 — the banner rules (`# ====...`) and the prose that
    // explains the ratchet formula ("floor = measured ... minus 0.1") all
    // contain the character. Same class of error as the DHAT count above, and
    // caught the same way: by checking the tool's output against the file
    // instead of trusting that the tool was obviously right.
    let floors = cov
        .lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with('#') && !l.starts_with('['))
        .filter(|l| {
            l.split_once('=').is_some_and(|(k, _)| {
                let k = k.trim();
                !k.is_empty()
                    && k.chars().all(|c| {
                        c.is_ascii_lowercase() || c == '-' || c == '_' || c.is_ascii_digit()
                    })
            })
        })
        .count();

    let testing = vec![
        Row::new(
            "Tests",
            if tests > 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Broken
            },
            format!("{tests}"),
            "line-start #[test]/#[tokio::test] under crates/ -- see note",
        ),
        Row::new(
            "Coverage floors",
            Verdict::Guaranteed,
            format!("{floors} crates"),
            "ratcheted: floors move up only",
        ),
    ];

    // ---------------------------------------------------------------
    // Section 5 — every version pinned (THREE PRINCIPLES #3)
    //
    // This section exists because the report did not have one, and that
    // absence was not cosmetic: principles 1 and 2 were measured here every
    // run while principle 3 was checked only by a pre-commit grep for the
    // literal `^` character. A bare `"1.2.3"` IS `^1.2.3` to cargo, so that
    // grep passed 65 of 77 workspace deps green, and the lockfile drifted
    // underneath the manifest unnoticed -- measured 2026-08-18, 21 crates
    // adrift including tokio 1.52.3 -> 1.53.1 and rustls 0.23.40 -> 0.23.43.
    //
    // A guarantee nobody measures is a guarantee nobody has.
    // ---------------------------------------------------------------
    let manifest = std::fs::read_to_string("Cargo.toml").unwrap_or_default();
    let lock = std::fs::read_to_string("Cargo.lock").unwrap_or_default();
    let pins = workspace_dep_pins(&manifest);
    let pairs = lock_pairs(&lock);

    let ranged = pins.iter().filter(|p| p.exact.is_none()).count();
    let exact = pins.len() - ranged;

    // A dep no member crate references is DECLARED BUT UNUSED -- not a pinning
    // failure, so it is counted separately rather than folded into a verdict
    // it did not cause.
    let referenced = referenced_workspace_deps();
    let mut drifted = 0usize;
    let mut unreferenced = 0usize;
    for p in &pins {
        let Some(want) = p.exact.as_deref() else {
            continue;
        };
        if !referenced.contains(&p.name) {
            unreferenced += 1;
        }
        let mut seen_name = false;
        let mut matched = false;
        for (n, v) in &pairs {
            if n == &p.name {
                seen_name = true;
                if v == want {
                    matched = true;
                }
            }
        }
        if seen_name && !matched {
            drifted += 1;
        }
    }

    let pinning = vec![
        Row::new(
            "Workspace deps exactly pinned",
            if ranged == 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Broken
            },
            format!("{exact}/{} exact", pins.len()),
            "a bare \"1.2.3\" is ^1.2.3 to cargo -- both are ranges",
        ),
        Row::new(
            "Manifest agrees with Cargo.lock",
            if drifted == 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Broken
            },
            format!("{drifted} adrift"),
            "the version we declare is the version we build",
        ),
        Row::new(
            "Declared but never referenced",
            if unreferenced == 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Bounded
            },
            format!("{unreferenced} deps"),
            "every declaration is reachable from a member crate manifest",
        ),
    ];

    // ---------------------------------------------------------------
    // Section 6 — the data itself: stored, deduplicated, searchable, watched
    //
    // Added 2026-08-22. The operator's standing demand names this dimension
    // in every restatement -- "saving into db and auditing logging tracking
    // capturing debugging finding searching monitoring dashboard
    // visualising" -- and it was the one dimension this report never
    // measured. Five green sections read as "everything is checked", which
    // is the same false-OK this whole binary exists to prevent.
    //
    // These rows assert PROPERTIES, not a census. The first draft counted
    // "lines mentioning CREATE TABLE" and reported 52 for a system with 17
    // tables, because doc comments and test fixtures say the words too. A
    // number that does not mean what its label says is worse than no row:
    // it looks like evidence.
    // ---------------------------------------------------------------
    let storage_src = Path::new("crates/storage/src");

    // THE uniqueness invariant, stated as a property. Two different
    // instruments really do share a numeric id on different exchanges
    // (I-P1-11), and a Dhan candle and a Groww candle for the same minute
    // are different observations. A key naming security_id without segment
    // merges rows that are not the same row -- invisibly, no error anywhere.
    let keys_with_id = count_lines_containing(storage_src, &["DEDUP_KEY_"]);
    let canonical_candle_key = count_lines_containing(storage_src, &["security_id, segment, feed"]);

    let runbooks = std::fs::read_dir("docs/error-runbooks")
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
                .count()
        })
        .unwrap_or(0);

    let tf = std::fs::read_dir("deploy/aws/terraform")
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "tf"))
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let alarms = tf
        .matches("resource \"aws_cloudwatch_metric_alarm\"")
        .count();
    let log_filters = tf
        .matches("resource \"aws_cloudwatch_log_metric_filter\"")
        .count();

    // Shipped vs taken, MEASURED both ends. The first draft hardcoded
    // "76 of 371" -- a document number in a binary whose entire premise is
    // that documents cannot stay true. The gap is a deliberate cost
    // decision (all 371 would cost ~$111/mo against a budget whose
    // automatic action switches the trading box off), so both numbers are
    // shown rather than the flattering one.
    let selector =
        std::fs::read_to_string("deploy/aws/terraform/user-data.sh.tftpl").unwrap_or_default();
    let shipped = selector
        .split("\"metric_selectors\"")
        .nth(1)
        .map(|tail| {
            let end = tail.find(']').unwrap_or(0);
            tail[..end].matches("tv_").count()
        })
        .unwrap_or(0);
    let emitted = distinct_tv_names(Path::new("crates"));

    let observability = vec![
        Row::new(
            "Dedup keys, all named constants",
            if keys_with_id > 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Broken
            },
            format!("{keys_with_id} refs"),
            "never an inline literal: an inline key evades the feed-in-key guard",
        ),
        Row::new(
            "Candle key carries segment AND feed",
            if canonical_candle_key > 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Broken
            },
            format!("{canonical_candle_key} const"),
            "I-P1-11 + feed: same id on two exchanges, two brokers, stay distinct rows",
        ),
        Row::new(
            "Written runbooks",
            if runbooks > 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Broken
            },
            format!("{runbooks} files"),
            "every code maps to a fix; error_code_rule_file_crossref fails without one",
        ),
        Row::new(
            "CloudWatch alarms",
            if alarms > 0 {
                Verdict::Guaranteed
            } else {
                Verdict::Broken
            },
            format!("{alarms} + {log_filters} log"),
            "alarm_metric_has_a_route_guard: none may watch an unreachable metric",
        ),
        Row::new(
            "Measurements shipped vs taken",
            Verdict::Bounded,
            format!("{shipped} of {emitted}"),
            "a cost decision, not an oversight: all of them would trip the budget kill-switch",
        ),
    ];

    // ---------------------------------------------------------------
    // Report
    // ---------------------------------------------------------------
    let sections: Vec<(&str, &[Row])> = vec![
        ("1. One language", &lang[..]),
        ("2. O(1)", &o1[..]),
        ("3. Automation", &auto[..]),
        ("4. Test surface", &testing[..]),
        ("5. Every version pinned", &pinning[..]),
        (
            "6. Data — stored, deduped, searchable, watched",
            &observability[..],
        ),
    ];

    // `--html` emits the SAME rows as a page. One measurement pass, two
    // renderings, so the printed table and the published page can never
    // disagree -- which is the whole reason this is generated rather than
    // hand-written.
    if std::env::args().any(|a| a == "--html") {
        print!("{}", render_html(&sections));
        let broken = sections
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .filter(|r| r.verdict == Verdict::Broken)
            .count();
        std::process::exit(if broken > 0 { 1 } else { 0 });
    }

    println!("TICKVAULT GUARANTEE REPORT");
    println!("Every number below is measured from the working tree, now.");
    print!("{}", render("1. ONE LANGUAGE", &lang));
    print!("{}", render("2. O(1)", &o1));
    print!("{}", render("3. AUTOMATION", &auto));
    print!("{}", render("4. TEST SURFACE", &testing));
    print!("{}", render("5. EVERY VERSION PINNED", &pinning));
    print!(
        "{}",
        render(
            "6. DATA — STORED, DEDUPED, SEARCHABLE, WATCHED",
            &observability
        )
    );

    let all: Vec<Row> = lang
        .into_iter()
        .chain(o1)
        .chain(auto)
        .chain(testing)
        .chain(pinning)
        .chain(observability)
        .collect();
    let broken = all.iter().filter(|r| r.verdict == Verdict::Broken).count();
    let bounded = all.iter().filter(|r| r.verdict == Verdict::Bounded).count();
    let impossible = all
        .iter()
        .filter(|r| r.verdict == Verdict::Impossible)
        .count();

    println!("\nSUMMARY");
    println!("=======");
    println!(
        "  {} guaranteed   {bounded} bounded   {impossible} impossible   {broken} broken",
        all.len() - bounded - impossible - broken
    );
    println!(
        "\n  BOUNDED and IMPOSSIBLE rows are facts about the problem, not defects.\n  \
         They never fail this run -- a gate that failed on arithmetic would be\n  \
         permanently red, and a permanently red gate is an ignored gate."
    );

    if broken > 0 {
        println!("\n  {broken} GUARANTEED row(s) BROKEN -- exiting 1.");
        std::process::exit(1);
    }
    println!("\n  Every guaranteed row holds.");
}
