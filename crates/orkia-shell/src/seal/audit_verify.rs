// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! The rich `audit --verify` view.
//!
//! `audit verify` (verb) gives a terse per-chain pass/fail line. The
//! `--verify` flag (no scope) selects this view instead: for each job
//! chain it walks the chain, prints the integrity summary, and renders
//! the allow/deny verdict tree — the cage's decisions in plain sight,
//! each paired with the exit code its command actually produced.
//!
//! Every value here is read straight from the on-disk `seal.jsonl`
//! (the same records `audit --job N --export` emits as JSON); this is a
//! presentation layer, not a second source of truth.

use orkia_shell_types::{BlockContent, SealRecord};

use crate::seal::SealChain;
use crate::seal::audit::{JobChainSummary, list_job_chains};

/// A single policy-relevant verdict, paired with the exit code of the
/// command outcome that immediately followed it in the chain.
struct VerdictRow {
    verdict: String,
    capability: Option<String>,
    command: String,
    exit_code: Option<i32>,
}

/// Render `audit --verify` (no scope): one rich block per job chain.
pub fn render(data_dir: &std::path::Path) -> Vec<BlockContent> {
    let jobs = list_job_chains(data_dir);
    if jobs.is_empty() {
        return vec![BlockContent::SystemInfo(
            " no SEAL chains yet — run an agent to start one".into(),
        )];
    }
    let mut blocks = Vec::new();
    let mut all_ok = true;
    for j in &jobs {
        match load_chain(data_dir, j) {
            Some(chain) => {
                all_ok &= chain.verify().0;
                blocks.extend(render_chain(&chain, j));
            }
            None => all_ok = false,
        }
    }
    if jobs.len() > 1 {
        blocks.push(if all_ok {
            BlockContent::SystemInfo("⛓ all chains verified ✓".into())
        } else {
            BlockContent::Error("⛓ verification failed".into())
        });
    }
    blocks
}

fn load_chain(data_dir: &std::path::Path, j: &JobChainSummary) -> Option<SealChain> {
    let path = data_dir
        .join("agents")
        .join(&j.agent)
        .join("jobs")
        .join(j.job_id.to_string())
        .join("seal.jsonl");
    SealChain::load(path).ok()
}

fn render_chain(chain: &SealChain, j: &JobChainSummary) -> Vec<BlockContent> {
    let verdicts = collect_verdicts(chain.records());
    let mut blocks = vec![
        BlockContent::SystemInfo(format!("⛓ audit verify · job {} ({})", j.job_id, j.agent)),
        BlockContent::Text(format!(
            "  \x1b[90mwalking chain · agents/{}/jobs/{}/seal.jsonl\x1b[0m",
            j.agent, j.job_id
        )),
        BlockContent::Text(format!("  \x1b[90m{}\x1b[0m", spine(&verdicts))),
    ];

    let (ok, broken) = chain.verify();
    blocks.push(if ok {
        BlockContent::Text(format!(
            "  \x1b[32m✓ {} records · SHA-256 chain intact · verify ok\x1b[0m",
            chain.len()
        ))
    } else {
        BlockContent::Error(format!("SEAL chain BROKEN at seq {}", broken.unwrap_or(0)))
    });

    for row in &verdicts {
        blocks.push(verdict_row(row));
    }
    blocks
}

/// Walk the chain once, keeping the policy-relevant verdicts (every
/// deny, plus allows a capability matched) and attaching the exit code
/// of the `command.outcome` that followed each one.
fn collect_verdicts(records: &[SealRecord]) -> Vec<VerdictRow> {
    let mut out: Vec<VerdictRow> = Vec::new();
    for r in records {
        match r.event_type.as_str() {
            "cage.verdict" => {
                let verdict = str_field(r, "verdict").unwrap_or("?").to_string();
                let capability = str_field(r, "capability").map(str::to_string);
                if verdict == "deny" || capability.is_some() {
                    out.push(VerdictRow {
                        verdict,
                        capability,
                        command: str_field(r, "command").unwrap_or("").to_string(),
                        exit_code: None,
                    });
                }
            }
            "command.outcome" => {
                if let Some(last) = out.last_mut()
                    && last.exit_code.is_none()
                {
                    last.exit_code = r
                        .detail
                        .get("exit_code")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                }
            }
            _ => {}
        }
    }
    out
}

fn str_field<'a>(r: &'a SealRecord, key: &str) -> Option<&'a str> {
    r.detail.get(key).and_then(|v| v.as_str())
}

fn spine(verdicts: &[VerdictRow]) -> String {
    if verdicts.is_empty() {
        return "genesis → (no policy verdicts)".into();
    }
    let markers: Vec<String> = verdicts
        .iter()
        .map(|v| format!("verdict({})", v.verdict))
        .collect();
    format!("genesis → … → {}", markers.join(" → "))
}

fn verdict_row(row: &VerdictRow) -> BlockContent {
    let cap = row.capability.as_deref().unwrap_or("default");
    let exit = row
        .exit_code
        .map(|e| format!("  \x1b[90m(exit {e})\x1b[0m"))
        .unwrap_or_default();
    let mark = if row.verdict == "deny" {
        "\x1b[31mdeny \x1b[0m"
    } else {
        "\x1b[32mallow\x1b[0m"
    };
    BlockContent::Text(format!(
        "     └─ {mark} {cap:<11} {}{exit}",
        truncate(&row.command, 48)
    ))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::job::JobId;
    use tempfile::tempdir;

    fn seed(data_dir: &std::path::Path) {
        let mut mgr = crate::seal::SealManager::new(data_dir.to_path_buf());
        mgr.create_job_chain(JobId(1), "faye");
        mgr.seal_job(JobId(1), "agent.spawn", serde_json::json!({}))
            .expect("spawn");
        // A default-allow (no capability) — must stay out of the tree.
        mgr.seal_job(
            JobId(1),
            "cage.verdict",
            serde_json::json!({
                "command": "git config --get user.email",
                "verdict": "allow", "capability": null, "rule": null
            }),
        )
        .expect("v1");
        mgr.seal_job(
            JobId(1),
            "command.outcome",
            serde_json::json!({"exit_code": 0}),
        )
        .expect("o1");
        // A capability allow — shown.
        mgr.seal_job(
            JobId(1),
            "cage.verdict",
            serde_json::json!({
                "command": "git commit -m \"extract auth middleware\"",
                "verdict": "allow", "capability": "git.commit", "rule": "git commit*"
            }),
        )
        .expect("v2");
        mgr.seal_job(
            JobId(1),
            "command.outcome",
            serde_json::json!({"exit_code": 0}),
        )
        .expect("o2");
        // A deny — shown, with its real exit code.
        mgr.seal_job(
            JobId(1),
            "cage.verdict",
            serde_json::json!({
                "command": "git push origin main",
                "verdict": "deny", "capability": "git.push", "rule": "git push*"
            }),
        )
        .expect("v3");
        mgr.seal_job(
            JobId(1),
            "command.outcome",
            serde_json::json!({"exit_code": 126}),
        )
        .expect("o3");
    }

    fn joined(blocks: &[BlockContent]) -> String {
        let mut s = String::new();
        for b in blocks {
            match b {
                BlockContent::SystemInfo(t) | BlockContent::Text(t) | BlockContent::Error(t) => {
                    s.push_str(t);
                    s.push('\n');
                }
                _ => {}
            }
        }
        s
    }

    #[test]
    fn rich_verify_shows_walk_summary_and_verdict_tree() {
        let dir = tempdir().unwrap();
        seed(dir.path());
        let out = render(dir.path());
        let s = joined(&out);
        assert!(
            s.contains("walking chain · agents/faye/jobs/1/seal.jsonl"),
            "{s}"
        );
        assert!(s.contains("SHA-256 chain intact · verify ok"), "{s}");
        assert!(s.contains("git.commit"), "{s}");
        assert!(s.contains("git.push"), "{s}");
        // The deny carries its real exit code, paired from command.outcome.
        assert!(s.contains("exit 126"), "{s}");
    }

    #[test]
    fn default_allow_is_excluded_from_tree() {
        let dir = tempdir().unwrap();
        seed(dir.path());
        let s = joined(&render(dir.path()));
        // The git-config default-allow must not appear as a verdict row.
        assert!(!s.contains("git config --get"), "{s}");
    }

    #[test]
    fn no_chains_reports_empty() {
        let dir = tempdir().unwrap();
        let s = joined(&render(dir.path()));
        assert!(s.contains("no SEAL chains yet"), "{s}");
    }
}
