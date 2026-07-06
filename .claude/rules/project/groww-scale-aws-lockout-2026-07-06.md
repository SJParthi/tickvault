# Groww Scale AWS Lockout — Operator Lock 2026-07-06

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > `groww-second-feed-scope-2026-06-19.md` §34 > this file > defaults.
> **Scope:** PERMANENT. Every PR, every branch, every future Claude/Cowork session.
> **Operator-locked:** 2026-07-06 (verbatim quotes below).
> **Mechanical enforcement:** `crates/storage/tests/groww_scale_aws_lockout_guard.rs` (build-failing; `All Green` is a GitHub-required check on `main`, so a red guard physically blocks the merge button — see `merge-gate-lock-2026-07-04.md`).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demands (preserve exactly, do not paraphrase)

**Quote 1 (2026-07-06):**
> "wipe off this entire dynamic scaling connections of 86 connections and 86k instruments... stick to our one active dhan and one active groww connections and its subscriptions alone"

**Quote 2 (2026-07-06):**
> "NOWHERE these dynamic scalable changes should ever go to AWS"

## §1. The rule (one line)

**The Groww dynamic-scaling experiment (multi-connection fleet, scale lab) must NEVER reach AWS — the runtime is exactly ONE active Dhan connection + ONE active Groww connection, and any PR that routes the scale flag toward the AWS deploy path fails CI on `main`.**

## §2. The measured evidence (why the experiment is dead)

Groww enforces an account-level ADAPTIVE connection cap: 33 connections ACKed on a fresh account state but only 7 after churn, with a ~35-40 minute penalty window — and the account is shared with bruteX. A "86 connections / 86k instruments" fleet is not achievable on this account; the experiment is wiped per Quote 1.

## §3. What stays dormant on main (honest state)

- The fleet/ladder code (`GrowwScaleConfig`, `[feeds.groww.scale]`) remains on `main` but is DORMANT: the serde + `Default` `enabled` default is `false`, and `config/base.toml` (the only config carrying the section) sets `enabled = false`. No config file, nothing under `deploy/`, and no deploy workflow sets it true — verified 2026-07-06 and now ratcheted.
- The scale-lab artifacts (`scripts/local-autopilot.sh`, `deploy/local/probe-once.date`, SCALE_WINDOW_START/END) live ONLY on the `local-runtime` branch — they must never land on `main`.

## §4. The guard tests (`crates/storage/tests/groww_scale_aws_lockout_guard.rs`)

| Test | Pins |
|---|---|
| `no_config_file_enables_groww_scale` | no `config/*.toml` sets `enabled = true` under `[feeds.groww.scale]` (the deploy workflow ships `config/` to the box) |
| `groww_scale_config_default_is_off` | `GrowwScaleConfig.enabled` keeps the bare `#[serde(default)]` (false) + `Default` impl `enabled: false` |
| `probe_once_marker_absent_from_main` | `deploy/local/probe-once.date` never lands on `main` |
| `scale_lab_scripts_absent_from_main` | `scripts/local-autopilot.sh` + `deploy/local/` never land on `main` |
| `deploy_path_never_sets_scale_flag` | nothing under `deploy/` or the AWS deploy workflows sets the scale flag (env var or TOML) |
| `lockout_rule_file_exists_and_pins_the_operator_quote` | this file keeps the dated quotes |
| `toml_scanner_detects_flipped_flag` | the guard's own scanner cannot regress to a vacuous pass |

## §5. What a violating PR looks like (REJECT)

- Flips `feeds.groww.scale.enabled` to `true` in ANY tracked config file, env var, or deploy script.
- Re-adds `deploy/local/probe-once.date`, `scripts/local-autopilot.sh`, or any scale-lab harness to `main`.
- Changes the `GrowwScaleConfig.enabled` default away from `false`.
- Deletes or weakens `groww_scale_aws_lockout_guard.rs` (or this rule file) without a fresh dated operator quote added HERE first.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote, only then can the PR land.

## §6. Auto-driver / Insta-reel explanation

> Sir, the experiment of hiring 86 shop boys is CANCELLED — the market gate only ever lets a handful in, and the gate punishes crowding for half an hour. The shop runs with exactly TWO boys forever: one on the Dhan price board, one on the Groww price board. And there is now a lock on the delivery van: if anyone ever tries to sneak the 86-boy plan into the van going to the AWS shop, the van's engine refuses to start.

## §7. Trigger / auto-load

Always loaded. Reinforced on any session editing `config/*.toml` `[feeds.groww.scale]`, `crates/common/src/config.rs` (`GrowwScaleConfig`), anything under `deploy/`, `.github/workflows/deploy-aws*.yml`, or `crates/storage/tests/groww_scale_aws_lockout_guard.rs`.
