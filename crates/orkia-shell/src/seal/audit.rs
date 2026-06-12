// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

//! Renderer for the `audit` builtin (Shell Audit Log).
//!
//! Walks the scoped chains under `<data_dir>/agents/.../jobs/...`
//! and `<data_dir>/projects/...`, producing `BlockContent` rows
//! the shell renderer turns into the user-visible output.
//!
//! Log surface (`orkia audit verify [project]`). SEAL v1 documents
//! live under `orkia rfc seal <slug>`; `audit redact` is REPL-routed
//! separately (it mutates state).
//!
//! Lives in `orkia-shell` (not `orkia-builtin`) because chain
//! verification needs SHA-256 and the chain types — orkia-builtin
//! deliberately stays free of crypto/io plumbing.

use std::path::Path;

use orkia_shell_types::BlockContent;

use crate::seal::{SealChain, SealManager};

/// Render the read-only `audit [args]` views. Grammar:
///
///   - `audit`                      → cross-scope summary
///   - `audit verify [project]`     → verify a project chain
///   - `audit verify --job <id>`    → verify a job chain
///   - `audit verify`               → verify every chain found
///   - `audit --job <id>` / `--project <p>` / `--rfc <id>` [--last N]
///   - `audit --deep` / `audit --export`
///
/// `audit redact …` never reaches here — it is REPL-routed in
/// `dispatch_named` because it mutates state.
pub fn render(data_dir: &Path, args: &[String]) -> Vec<BlockContent> {
    let opts = parse_args(args);
    match (opts.job, opts.project.as_deref(), opts.rfc.as_deref()) {
        (Some(job_id), _, _) => render_job_scope(data_dir, job_id, &opts),
        (None, _, Some(rfc_id)) => render_rfc_scope(data_dir, rfc_id, &opts),
        (None, Some(project), None) => render_project_scope(data_dir, project, &opts),
        (None, None, None) if opts.rich => crate::seal::audit_verify::render(data_dir),
        (None, None, None) if opts.verify => render_verify_all(data_dir),
        (None, None, None) => render_summary(data_dir, &opts),
    }
}

/// `audit verify` with no scope — verify every job and project chain
/// under `data_dir` and report a per-chain line plus an overall verdict.
fn render_verify_all(data_dir: &Path) -> Vec<BlockContent> {
    let mut blocks = vec![BlockContent::SystemInfo(
        "⛓ audit verify · all chains".into(),
    )];
    let mut all_ok = true;
    let mut any = false;
    for j in list_job_chains(data_dir) {
        let path = data_dir
            .join("agents")
            .join(&j.agent)
            .join("jobs")
            .join(j.job_id.to_string())
            .join("seal.jsonl");
        if let Ok(chain) = SealChain::load(path) {
            any = true;
            let (ok, broken) = chain.verify();
            all_ok &= ok;
            blocks.push(verify_summary(
                ok,
                broken,
                &format!("job {} ({})", j.job_id, j.agent),
            ));
        }
    }
    for p in list_project_chains(data_dir) {
        let path = data_dir.join("projects").join(&p.name).join("seal.jsonl");
        if let Ok(chain) = SealChain::load(path) {
            any = true;
            let (ok, broken) = chain.verify();
            all_ok &= ok;
            blocks.push(verify_summary(ok, broken, &format!("project {}", p.name)));
        }
    }
    blocks.push(if !any {
        BlockContent::SystemInfo(" no SEAL chains yet".into())
    } else if all_ok {
        BlockContent::SystemInfo("⛓ all chains verified ✓".into())
    } else {
        BlockContent::Error("⛓ verification failed".into())
    });
    blocks
}

fn render_rfc_scope(data_dir: &Path, rfc_id: &str, opts: &Opts) -> Vec<BlockContent> {
    // Resolve project: explicit --project wins, otherwise scan the
    // workspace for the project containing this rfc.
    let project = match opts
        .project
        .clone()
        .or_else(|| resolve_project_for_rfc(data_dir, rfc_id))
    {
        Some(p) => p,
        None => {
            return vec![BlockContent::Error(format!(
                "audit: rfc '{rfc_id}' not found in any project (or pass --project <name>)"
            ))];
        }
    };
    let path = data_dir.join("projects").join(&project).join("seal.jsonl");
    let chain = match SealChain::load(path) {
        Ok(c) => c,
        Err(e) => return vec![BlockContent::Error(format!("audit: load failed: {e}"))],
    };
    let filtered: Vec<&orkia_shell_types::SealRecord> = chain
        .records()
        .iter()
        .filter(|r| record_belongs_to_rfc(r, rfc_id))
        .collect();
    if filtered.is_empty() {
        return vec![BlockContent::SystemInfo(format!(
            "audit: no rfc.* records for '{rfc_id}' in project {project}"
        ))];
    }
    if opts.export {
        let lines: Vec<String> = filtered
            .iter()
            .filter_map(|r| serde_json::to_string(r).ok())
            .collect();
        return vec![BlockContent::Text(lines.join("\n"))];
    }
    if opts.verify {
        // Verify the full project chain, then report whether the rfc
        // slice is contiguous (no missing seqs). A broken chain at any
        // seq is a global failure — there is no way to "verify just the
        // rfc records" without re-hashing the chain, because the chain
        // is the authority on integrity.
        let (ok, broken) = chain.verify();
        let mut blocks = verify_block(
            &format!("rfc {rfc_id} (project {project})"),
            (ok, broken),
            filtered.len(),
        );
        blocks.insert(
            0,
            BlockContent::SystemInfo(format!(
                "⛓ rfc {rfc_id}: {} rfc.* record(s) of {} total in project chain",
                filtered.len(),
                chain.len(),
            )),
        );
        return blocks;
    }
    let last = opts.last.unwrap_or(filtered.len());
    let start = filtered.len().saturating_sub(last);
    let mut blocks = vec![BlockContent::SystemInfo(format!(
        "⛓ SEAL chain · rfc {rfc_id} (project {project}) · {} rfc record(s)",
        filtered.len(),
    ))];
    for record in &filtered[start..] {
        blocks.push(record_row(record));
    }
    blocks
}

/// Returns true if `record` is an `rfc.*` event whose detail's `rfc_id`
/// field matches `rfc_id`. Also matches legacy `rfc.create`/`rfc.update`
/// events that store the id under `slug` instead.
fn record_belongs_to_rfc(record: &orkia_shell_types::SealRecord, rfc_id: &str) -> bool {
    if !record.event_type.starts_with("rfc.") {
        return false;
    }
    let detail = &record.detail;
    if detail.get("rfc_id").and_then(|v| v.as_str()) == Some(rfc_id) {
        return true;
    }
    if detail.get("slug").and_then(|v| v.as_str()) == Some(rfc_id) {
        return true;
    }
    false
}

/// Scan `<data_dir>/projects/*/rfcs/<rfc_id>.md` to find the owning project.
fn resolve_project_for_rfc(data_dir: &Path, rfc_id: &str) -> Option<String> {
    let projects_dir = data_dir.join("projects");
    let entries = std::fs::read_dir(&projects_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().into_string().ok()?;
        let candidate = entry.path().join("rfcs").join(format!("{rfc_id}.md"));
        if candidate.exists() {
            return Some(name);
        }
    }
    None
}

#[derive(Default)]
struct Opts {
    job: Option<u32>,
    project: Option<String>,
    /// project chain to records whose `detail` carries `rfc_id == <id>` so
    /// `orkia audit verify --rfc auth-pkce` validates the RFC slice
    /// end-to-end without manual greps.
    rfc: Option<String>,
    verify: bool,
    /// Set only by the `--verify` *flag* (not the `verify` verb). With no
    /// scope this selects the rich verify view — the chain walk plus the
    /// allow/deny verdict tree (`audit_verify::render`). The verb form
    /// `audit verify` keeps the terse all-chains summary.
    rich: bool,
    deep: bool,
    export: bool,
    last: Option<usize>,
}

fn parse_args(args: &[String]) -> Opts {
    let mut out = Opts::default();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--verify" => {
                out.verify = true;
                out.rich = true;
            }
            "--deep" => out.deep = true,
            "--export" => out.export = true,
            "--job" => {
                if let Some(n) = iter.next().and_then(|v| v.parse().ok()) {
                    out.job = Some(n);
                }
            }
            s if s.starts_with("--job=") => {
                if let Ok(n) = s["--job=".len()..].parse() {
                    out.job = Some(n);
                }
            }
            "--project" => {
                if let Some(v) = iter.next() {
                    out.project = Some(v.clone());
                }
            }
            s if s.starts_with("--project=") => {
                out.project = Some(s["--project=".len()..].to_string());
            }
            "--rfc" => {
                if let Some(v) = iter.next() {
                    out.rfc = Some(v.clone());
                }
            }
            s if s.starts_with("--rfc=") => {
                out.rfc = Some(s["--rfc=".len()..].to_string());
            }
            "--last" => {
                if let Some(n) = iter.next().and_then(|v| v.parse().ok()) {
                    out.last = Some(n);
                }
            }
            s if s.starts_with("--last=") => {
                if let Ok(n) = s["--last=".len()..].parse() {
                    out.last = Some(n);
                }
            }
            // `audit verify [project]` verb form. `verify` sets the
            // flag; the next bare token (if any) is the project scope.
            "verify" => out.verify = true,
            s if !s.starts_with("--") && out.project.is_none() => {
                out.project = Some(s.to_string());
            }
            _ => {}
        }
    }
    out
}

// ─── Summary (no scope) ────────────────────────────────────────────

fn render_summary(data_dir: &Path, _opts: &Opts) -> Vec<BlockContent> {
    let mut blocks = Vec::new();

    let jobs = list_job_chains(data_dir);
    if !jobs.is_empty() {
        blocks.push(BlockContent::SystemInfo(" JOB CHAINS".into()));
        for j in &jobs {
            blocks.push(BlockContent::Text(format!(
                "  job {:<4} ({}) · {} records{}",
                j.job_id,
                j.agent,
                j.record_count,
                if j.closed { " · closed" } else { "" },
            )));
        }
    }

    let projects = list_project_chains(data_dir);
    if !projects.is_empty() {
        blocks.push(BlockContent::SystemInfo(" PROJECT CHAINS".into()));
        for p in &projects {
            blocks.push(BlockContent::Text(format!(
                "  {:<24} · {} records · {} job ref{}",
                p.name,
                p.record_count,
                p.job_refs,
                if p.job_refs == 1 { "" } else { "s" },
            )));
        }
    }

    if jobs.is_empty() && projects.is_empty() {
        blocks.push(BlockContent::SystemInfo(
            " no SEAL chains yet — run an agent or `rfc create` to start one".into(),
        ));
    } else {
        let total: usize = jobs.iter().map(|j| j.record_count).sum::<usize>()
            + projects.iter().map(|p| p.record_count).sum::<usize>();
        blocks.push(BlockContent::Text(format!(
            "  total: {total} records across {} chain{}",
            jobs.len() + projects.len(),
            if jobs.len() + projects.len() == 1 {
                ""
            } else {
                "s"
            },
        )));
    }

    blocks
}

// ─── Job scope ─────────────────────────────────────────────────────

fn render_job_scope(data_dir: &Path, job_id: u32, opts: &Opts) -> Vec<BlockContent> {
    let agent = match find_agent_for_job(data_dir, job_id) {
        Some(a) => a,
        None => {
            return vec![BlockContent::Error(format!(
                "audit: no job chain found for job {job_id}"
            ))];
        }
    };

    let chain_path = data_dir
        .join("agents")
        .join(&agent)
        .join("jobs")
        .join(job_id.to_string())
        .join("seal.jsonl");

    let chain = match SealChain::load(chain_path) {
        Ok(c) => c,
        Err(e) => return vec![BlockContent::Error(format!("audit: load failed: {e}"))],
    };

    if opts.export {
        return export_chain(&chain);
    }

    if opts.verify {
        return verify_block(
            &format!("job {job_id} ({agent})"),
            chain.verify(),
            chain.len(),
        );
    }

    let last = opts.last.unwrap_or_else(|| chain.len());
    let start = chain.len().saturating_sub(last);
    let mut blocks = vec![BlockContent::SystemInfo(format!(
        "⛓ SEAL chain · job {job_id} ({agent}) · {} records{}",
        chain.len(),
        if chain.is_closed() { " · closed" } else { "" },
    ))];
    for record in &chain.records()[start..] {
        blocks.push(record_row(record));
    }
    let (ok, broken) = chain.verify();
    blocks.push(verify_summary(ok, broken, "chain"));
    blocks
}

// ─── Project scope ─────────────────────────────────────────────────

fn render_project_scope(data_dir: &Path, project: &str, opts: &Opts) -> Vec<BlockContent> {
    let path = data_dir.join("projects").join(project).join("seal.jsonl");
    let chain = match SealChain::load(path) {
        Ok(c) => c,
        Err(e) => return vec![BlockContent::Error(format!("audit: load failed: {e}"))],
    };

    if chain.is_empty() {
        return vec![BlockContent::Error(format!(
            "audit: project '{project}' has no SEAL chain yet"
        ))];
    }

    if opts.export {
        return export_chain(&chain);
    }

    if opts.deep {
        return render_deep(data_dir, project);
    }

    if opts.verify {
        return verify_block(&format!("project {project}"), chain.verify(), chain.len());
    }

    let last = opts.last.unwrap_or_else(|| chain.len());
    let start = chain.len().saturating_sub(last);
    let mut blocks = vec![BlockContent::SystemInfo(format!(
        "⛓ SEAL chain · project {project} · {} records",
        chain.len(),
    ))];
    for record in &chain.records()[start..] {
        blocks.push(record_row(record));
    }
    let (ok, broken) = chain.verify();
    blocks.push(verify_summary(ok, broken, "chain"));
    blocks
}

fn render_deep(data_dir: &Path, project: &str) -> Vec<BlockContent> {
    let manager = SealManager::new(data_dir.to_path_buf());
    let result = manager.verify_project_deep(project);

    let mut blocks = vec![BlockContent::SystemInfo(format!(
        "⛓ DEEP VERIFY · project {project}"
    ))];
    blocks.push(BlockContent::Text(format!(
        "  project chain: {} records {}",
        result.project_records,
        if result.project_ok { "✓" } else { "✗" },
    )));
    if result.job_results.is_empty() {
        blocks.push(BlockContent::Text("  (no job references)".into()));
    } else {
        blocks.push(BlockContent::Text("  referenced jobs:".into()));
        for jr in &result.job_results {
            let chain_mark = if jr.chain_ok { "✓" } else { "✗" };
            let tip_mark = if jr.tip_matches { "✓" } else { "✗" };
            blocks.push(BlockContent::Text(format!(
                "    job {:<4} ({}) · {} records · chain {} · tip {}",
                jr.job_id, jr.agent, jr.record_count, chain_mark, tip_mark,
            )));
        }
    }
    let all_ok = result.project_ok
        && result
            .job_results
            .iter()
            .all(|jr| jr.chain_ok && jr.tip_matches);
    blocks.push(if all_ok {
        BlockContent::SystemInfo("⛓ all chains verified ✓".into())
    } else {
        BlockContent::Error("⛓ verification failed".into())
    });
    blocks
}

// ─── Helpers ───────────────────────────────────────────────────────

pub(crate) struct JobChainSummary {
    pub(crate) job_id: u32,
    pub(crate) agent: String,
    pub(crate) record_count: usize,
    pub(crate) closed: bool,
}

struct ProjectChainSummary {
    name: String,
    record_count: usize,
    job_refs: usize,
}

pub(crate) fn list_job_chains(data_dir: &Path) -> Vec<JobChainSummary> {
    let mut out = Vec::new();
    let agents_dir = data_dir.join("agents");
    let agents = match std::fs::read_dir(&agents_dir) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for agent_entry in agents.flatten() {
        let agent_name = match agent_entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let jobs_dir = agent_entry.path().join("jobs");
        let job_entries = match std::fs::read_dir(&jobs_dir) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for job_entry in job_entries.flatten() {
            let job_id: u32 = match job_entry.file_name().to_str().and_then(|s| s.parse().ok()) {
                Some(n) => n,
                None => continue,
            };
            let chain_path = job_entry.path().join("seal.jsonl");
            if !chain_path.exists() {
                continue;
            }
            if let Ok(chain) = SealChain::load(chain_path) {
                out.push(JobChainSummary {
                    job_id,
                    agent: agent_name.clone(),
                    record_count: chain.len(),
                    closed: chain.is_closed(),
                });
            }
        }
    }
    out.sort_by_key(|s| (s.agent.clone(), s.job_id));
    out
}

fn list_project_chains(data_dir: &Path) -> Vec<ProjectChainSummary> {
    let mut out = Vec::new();
    let dir = data_dir.join("projects");
    let entries = match std::fs::read_dir(&dir) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let chain_path = entry.path().join("seal.jsonl");
        if !chain_path.exists() {
            continue;
        }
        if let Ok(chain) = SealChain::load(chain_path) {
            let job_refs = chain
                .records()
                .iter()
                .filter(|r| r.event_type == "job.reference")
                .count();
            out.push(ProjectChainSummary {
                name,
                record_count: chain.len(),
                job_refs,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Scan all agents to find which one owns the given job id. Returns
/// the agent name on hit. We don't enforce uniqueness across agents
/// (job ids are monotonic per orkia install, so collisions only
/// happen if the state file is wiped); first hit wins.
fn find_agent_for_job(data_dir: &Path, job_id: u32) -> Option<String> {
    let agents_dir = data_dir.join("agents");
    let agents = std::fs::read_dir(&agents_dir).ok()?;
    for entry in agents.flatten() {
        let agent_name = entry.file_name().into_string().ok()?;
        let candidate = entry
            .path()
            .join("jobs")
            .join(job_id.to_string())
            .join("seal.jsonl");
        if candidate.exists() {
            return Some(agent_name);
        }
    }
    None
}

fn record_row(record: &orkia_shell_types::SealRecord) -> BlockContent {
    let hash_short = if record.hash.len() >= 12 {
        record.hash[..12].to_string()
    } else {
        record.hash.clone()
    };
    BlockContent::SealRecord {
        seq: record.seq,
        agent: "system".into(),
        event: record.event_type.clone(),
        hash_short,
    }
}

fn verify_summary(ok: bool, broken: Option<u64>, label: &str) -> BlockContent {
    if ok {
        BlockContent::SystemInfo(format!("⛓ {label} verified ✓"))
    } else {
        BlockContent::Error(format!("⛓ {label} BROKEN at seq {}", broken.unwrap_or(0)))
    }
}

fn verify_block(
    label: &str,
    verify_result: (bool, Option<u64>),
    record_count: usize,
) -> Vec<BlockContent> {
    let (ok, broken) = verify_result;
    if ok {
        vec![BlockContent::SystemInfo(format!(
            "⛓ {label}: SEAL chain verified ✓ ({record_count} records)"
        ))]
    } else {
        vec![BlockContent::Error(format!(
            "⛓ {label}: SEAL chain BROKEN at seq {}",
            broken.unwrap_or(0)
        ))]
    }
}

fn export_chain(chain: &SealChain) -> Vec<BlockContent> {
    let lines: Vec<String> = chain
        .records()
        .iter()
        .filter_map(|r| serde_json::to_string(r).ok())
        .collect();
    vec![BlockContent::Text(lines.join("\n"))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use orkia_shell_types::job::JobId;
    use tempfile::tempdir;

    fn make_one_job_chain(data_dir: &Path, agent: &str, job: u32) {
        let mut mgr = SealManager::new(data_dir.to_path_buf());
        let chain = mgr.create_job_chain(JobId(job), agent);
        chain
            .append("agent.spawn", serde_json::json!({"tools_count": 3}))
            .expect("append");
        chain
            .append("hook.PreToolUse", serde_json::json!({"tool": "Read"}))
            .expect("append");
        chain
            .append("agent.complete", serde_json::json!({"exit_code": 0}))
            .expect("append");
    }

    #[test]
    fn summary_lists_active_jobs() {
        let dir = tempdir().unwrap();
        make_one_job_chain(dir.path(), "faye", 1);
        let out = render(dir.path(), &[]);
        let joined = blocks_to_string(&out);
        assert!(joined.contains("JOB CHAINS"), "missing header: {joined}");
        assert!(joined.contains("job 1"));
        assert!(joined.contains("faye"));
    }

    #[test]
    fn job_view_renders_records_and_verifies() {
        let dir = tempdir().unwrap();
        make_one_job_chain(dir.path(), "faye", 2);
        let out = render(dir.path(), &["--job".into(), "2".into()]);
        let joined = blocks_to_string(&out);
        assert!(joined.contains("job 2 (faye)"));
        assert!(joined.contains("verified ✓"));
    }

    #[test]
    fn verify_only_skips_record_list() {
        let dir = tempdir().unwrap();
        make_one_job_chain(dir.path(), "faye", 3);
        let out = render(dir.path(), &["--job".into(), "3".into(), "--verify".into()]);
        // verify-only emits just the verify summary (no per-record rows,
        // no banner). So a single block carrying the verdict.
        assert_eq!(out.len(), 1);
        match &out[0] {
            BlockContent::SystemInfo(s) => assert!(s.contains("verified ✓")),
            other => panic!("expected SystemInfo, got {other:?}"),
        }
    }

    #[test]
    fn verify_verb_with_project_positional() {
        // `audit verify <project>` verb form maps to a project verify.
        let dir = tempdir().unwrap();
        let mut mgr = SealManager::new(dir.path().to_path_buf());
        mgr.seal_project("demo", "rfc.created", serde_json::json!({}))
            .expect("seal");
        let out = render(dir.path(), &["verify".into(), "demo".into()]);
        let joined = blocks_to_string(&out);
        assert!(joined.contains("project demo"), "missing header: {joined}");
        assert!(joined.contains("verified ✓"));
    }

    #[test]
    fn verify_verb_no_scope_checks_every_chain() {
        // `audit verify` with no scope walks all job + project chains.
        let dir = tempdir().unwrap();
        make_one_job_chain(dir.path(), "faye", 7);
        let out = render(dir.path(), &["verify".into()]);
        let joined = blocks_to_string(&out);
        assert!(joined.contains("audit verify · all chains"));
        assert!(joined.contains("job 7 (faye)"));
        assert!(joined.contains("all chains verified ✓"));
    }

    #[test]
    fn deep_verify_walks_project_and_jobs() {
        let dir = tempdir().unwrap();
        let mut mgr = SealManager::new(dir.path().to_path_buf());
        // Build a complete delegation cycle.
        mgr.create_job_chain(JobId(10), "faye");
        mgr.seal_job(JobId(10), "agent.spawn", serde_json::json!({}))
            .expect("seal");
        mgr.seal_job(
            JobId(10),
            "agent.complete",
            serde_json::json!({"exit_code": 0}),
        )
        .expect("seal");
        let tip = mgr.close_job_chain(JobId(10)).unwrap();
        mgr.seal_project("orkia-shell", "rfc.create", serde_json::json!({}))
            .expect("seal");
        mgr.seal_job_reference("orkia-shell", JobId(10), "faye", &tip)
            .expect("ref");

        let out = render(
            dir.path(),
            &["--project".into(), "orkia-shell".into(), "--deep".into()],
        );
        let joined = blocks_to_string(&out);
        assert!(joined.contains("DEEP VERIFY"));
        // job id and agent name appear in the same row, separated
        // by the padded job-id field; assert their substrings
        // independently so the format is free to change padding.
        assert!(joined.contains("job 10"));
        assert!(joined.contains("(faye)"));
        assert!(joined.contains("chain ✓"));
        assert!(joined.contains("tip ✓"));
        assert!(joined.contains("all chains verified ✓"));
    }

    fn seed_rfc_chain(data_dir: &Path, project: &str, rfc_id: &str) {
        let mut mgr = SealManager::new(data_dir.to_path_buf());
        mgr.seal_project(
            project,
            "rfc.created",
            serde_json::json!({ "rfc_id": rfc_id, "by": "human" }),
        )
        .expect("seal");
        mgr.seal_project(
            project,
            "rfc.state_changed",
            serde_json::json!({
                "rfc_id": rfc_id, "from": "draft-empty", "to": "draft-active"
            }),
        )
        .expect("seal");
        mgr.seal_project(
            project,
            "rfc.promoted",
            serde_json::json!({ "rfc_id": rfc_id, "version": 1 }),
        )
        .expect("seal");
        // Unrelated record on the same project chain — must be filtered out.
        mgr.seal_project(
            project,
            "rfc.created",
            serde_json::json!({ "rfc_id": "other-rfc" }),
        )
        .expect("seal");
        // Seed the on-disk rfc file so resolve_project_for_rfc can find it.
        let rfc_dir = data_dir.join("projects").join(project).join("rfcs");
        std::fs::create_dir_all(&rfc_dir).unwrap();
        std::fs::write(rfc_dir.join(format!("{rfc_id}.md")), "+++\n+++\n").unwrap();
    }

    #[test]
    fn rfc_scope_filters_by_id_and_auto_resolves_project() {
        let dir = tempdir().unwrap();
        seed_rfc_chain(dir.path(), "ws", "auth-pkce");
        let out = render(dir.path(), &["--rfc".into(), "auth-pkce".into()]);
        let joined = blocks_to_string(&out);
        assert!(joined.contains("rfc auth-pkce"), "missing header: {joined}");
        assert!(joined.contains("rfc.created"));
        assert!(joined.contains("rfc.state_changed"));
        assert!(joined.contains("rfc.promoted"));
        // The unrelated record is excluded.
        assert!(!joined.contains("other-rfc"));
    }

    #[test]
    fn rfc_verify_validates_the_underlying_project_chain() {
        let dir = tempdir().unwrap();
        seed_rfc_chain(dir.path(), "ws", "auth-pkce");
        let out = render(
            dir.path(),
            &["--rfc".into(), "auth-pkce".into(), "--verify".into()],
        );
        let joined = blocks_to_string(&out);
        assert!(joined.contains("3 rfc.* record"));
        assert!(joined.contains("verified ✓"));
    }

    #[test]
    fn rfc_scope_errors_when_unknown_id_and_no_project() {
        let dir = tempdir().unwrap();
        let out = render(dir.path(), &["--rfc".into(), "ghost".into()]);
        let joined = blocks_to_string(&out);
        assert!(joined.contains("not found in any project"));
    }

    #[test]
    fn unknown_job_returns_error() {
        let dir = tempdir().unwrap();
        let out = render(dir.path(), &["--job".into(), "999".into()]);
        let joined = blocks_to_string(&out);
        assert!(joined.contains("no job chain found"));
    }

    fn blocks_to_string(blocks: &[BlockContent]) -> String {
        let mut s = String::new();
        for b in blocks {
            match b {
                BlockContent::SystemInfo(t) | BlockContent::Text(t) | BlockContent::Error(t) => {
                    s.push_str(t);
                    s.push('\n');
                }
                BlockContent::SealRecord { event, .. } => {
                    s.push_str(event);
                    s.push('\n');
                }
                _ => {}
            }
        }
        s
    }
}
